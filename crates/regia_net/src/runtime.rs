// SPDX-License-Identifier: MPL-2.0
//! The per-process net runtime (spec 70 §§ 6.3-6.6, 8.1): one [`NetNode`] resource per world that
//! turns sends into frames, frames into applied state and delivered messages, at fixed points of
//! each fixed tick.
//!
//! The node is sans-IO: the transport (the UDP endpoints of `crate::server` / `crate::client`, or
//! the in-memory hub of `crate::testing`) moves [`Datagram`]s between nodes. Frames:
//!
//! | Frame | Direction | Channel | Content |
//! |---|---|---|---|
//! | `TICK` | server to client | reliable ordered | one per send tick: ownership bits, spawns, deltas, the tick's reliable messages, despawns |
//! | `UMSG` | either | unreliable | the tick's unreliable messages, stamped with the tick and a sequence |
//! | `CMSG` | client to server | reliable ordered | the client tick's reliable messages, resync requests |
//! | `ACK` | client to server | unreliable | the newest applied server tick; protocol 2 adds the state acknowledgment |
//! | `STATE` | server to client | unreliable | protocol 2: spawns and deltas against acknowledged baselines, at most `replication.mtu_bytes` each |
//!
//! On the state channel a `TICK` frame carries only what must arrive (ownership bits, despawns,
//! reliable messages) and goes out only when it has some.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use regia_ecs::prelude::*;

use crate::bits::{BitReader, BitWriter, DecodeError};
use crate::codec::{CodecContext, EntityMap, LayoutHasher};
use crate::flood::{Admit, FloodGuard, FloodOutcome, FloodState};
use crate::handshake::DisconnectReason;
use crate::message::{
    Channel, Direction, MAX_BROADCASTS_PER_TICK, MAX_RELIABLE_QUEUE, MAX_RELIABLE_QUEUE_BYTES,
    MAX_UNRELIABLE_PER_TARGET, NetMessageRegistry, NetSendError, ValidateContext, Verdict,
};
use crate::model::{LocalPeer, NetId, NetMode, NetTick, OwnerCache, PeerId};
use crate::replication::{
    self, ClientReplication, ConnWire, NetPredicted, ReplicationRegistry, ServerReplication, StateAck,
};
use crate::server::RejectKind;
use crate::settings::{NetSettings, StateChannel};
use regia_app::FixedTime;

pub(crate) const TAG_TICK: u8 = 1;
pub(crate) const TAG_UMSG: u8 = 2;
const TAG_CMSG: u8 = 3;
pub(crate) const TAG_ACK: u8 = 4;
/// A state frame (spec 72 § 4, `crate::replication` has the layout).
pub(crate) const TAG_STATE: u8 = 7;
/// A frame of `ReliableUnordered` messages (wire 6, spec 120 § 5.1, ADR-10052 (3)): tag, tick,
/// message list without sequence numbers. It travels on the reliable-unordered class (the bridge's
/// stream 7), never inside a `TICK` or `CMSG` frame, so a stalled replication stream does not
/// hold it back. Either side sends it.
pub(crate) const TAG_RUMSG: u8 = 9;
/// Payload bytes of the `ReliableUnordered` messages one `RUMSG` frame carries at most: the frame
/// must stay one bridge part ([`crate::bridge::FRAME_PART_BYTES`]), because an unordered stream
/// does not keep a frame's parts in order. 512 bytes are left for the tag, tick, count and each
/// message's type, target and length. A message larger than this rides its tick's ordered frame.
pub const MAX_RUMSG_PAYLOAD: usize = crate::bridge::FRAME_PART_BYTES - 512;

/// State frames a client keeps waiting for `NetSet::Apply`. They drain every tick, so only a
/// client that stopped applying, or a server flooding it, reaches this.
pub const MAX_PENDING_STATE_FRAMES: usize = 1024;

/// A peer's inbound queue holds this many times `peer_cap_per_tick` messages. Frames that do not
/// fit are shed before they are decoded, so a flood costs no allocation (ADR-1892).
pub const INBOUND_QUEUE_TICKS: u32 = 8;
/// Resync ids one client frame may name. More is a malformed frame.
pub const MAX_RESYNC_IDS_PER_FRAME: usize = 256;
/// Message payload bytes one client frame carries; a heavier tick goes out as several frames, so a
/// correct client stays under `bridge::MAX_CLIENT_FRAME_BYTES`.
pub const MAX_CLIENT_FRAME_PAYLOAD: usize = 512 * 1024;
/// Resync ids one peer may send between two ticks; the rest are ignored and count as a breach.
const MAX_RESYNC_IDS_PER_TICK: usize = 4 * MAX_RESYNC_IDS_PER_FRAME;
/// Largest step between two tick frames. A correct server moves a few ticks at a time; a jump this
/// far (18 minutes at 60 Hz) would strand the client on a tick no later frame can reach.
pub const MAX_TICK_JUMP: u32 = 1 << 16;
/// Frames a client holds between two applies. They are reliable, so overflow ends the connection.
pub const MAX_PENDING_FRAMES: usize = 4096;
/// Frame bytes a client holds between two applies.
pub const MAX_PENDING_FRAME_BYTES: usize = 64 * 1024 * 1024;
/// Messages a client holds waiting for their tick or target; past it the newest are dropped.
pub const MAX_PENDING_MESSAGES: usize = 8192;
/// Message bytes a client holds waiting for their tick or target.
pub const MAX_PENDING_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// Bytes crossing the transport between two nodes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Datagram {
    /// On the server: the client peer it comes from or goes to. On a client: `PeerId::SERVER`.
    pub peer: PeerId,
    /// The delivery class (the transport keeps reliable classes reliable and ordered).
    pub channel: Channel,
    /// The frame.
    pub bytes: Vec<u8>,
}

/// `NetId` ↔ `Entity` on this process. Kept by the replication layer. Cloning is cheap (shared,
/// copy on write), so an encode or decode can hold a snapshot while the world changes.
#[derive(Resource, Debug, Default, Clone)]
pub struct NetEntities {
    maps: Arc<EntityMaps>,
}

#[derive(Debug, Default, Clone)]
struct EntityMaps {
    by_id: BTreeMap<NetId, Entity>,
    by_entity: HashMap<Entity, NetId>,
}

impl NetEntities {
    /// Records a mapping.
    pub fn insert(&mut self, id: NetId, e: Entity) {
        let m = Arc::make_mut(&mut self.maps);
        if let Some(old) = m.by_id.insert(id, e) {
            m.by_entity.remove(&old);
        }
        m.by_entity.insert(e, id);
    }

    /// Forgets a mapping.
    pub fn remove(&mut self, id: NetId) {
        if !self.maps.by_id.contains_key(&id) {
            return;
        }
        let m = Arc::make_mut(&mut self.maps);
        if let Some(e) = m.by_id.remove(&id) {
            m.by_entity.remove(&e);
        }
    }

    /// The entity of an id.
    pub fn entity(&self, id: NetId) -> Option<Entity> {
        self.maps.by_id.get(&id).copied()
    }

    /// The id of an entity.
    pub fn net_id(&self, e: Entity) -> Option<NetId> {
        self.maps.by_entity.get(&e).copied()
    }

    /// Every mapping in id order.
    pub fn iter(&self) -> impl Iterator<Item = (NetId, Entity)> + '_ {
        self.maps.by_id.iter().map(|(i, e)| (*i, *e))
    }

    /// How many replicated entities this process knows.
    pub fn len(&self) -> usize {
        self.maps.by_id.len()
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.maps.by_id.is_empty()
    }
}

impl EntityMap for NetEntities {
    fn to_net(&self, entity: Entity) -> Option<NetId> {
        self.net_id(entity)
    }
    fn to_entity(&self, id: NetId) -> Option<Entity> {
        self.entity(id)
    }
}

/// Where an outgoing message goes (resolved at `NetSet::Send`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Recipient {
    /// The server.
    Server,
    /// The owner of the target.
    OwnerOf(NetId),
    /// One named peer.
    Peer(PeerId),
    /// Every client the target is relevant to, and this process.
    All,
}

/// A message queued by `NetSender` this tick.
#[derive(Debug, Clone)]
pub struct OutMessage {
    /// Registry index.
    pub type_index: u16,
    /// Delivery class.
    pub channel: Channel,
    /// Direction.
    pub direction: Direction,
    /// Target (`None` = session message).
    pub target: Option<NetId>,
    /// Recipient.
    pub to: Recipient,
    /// Encoded message.
    pub payload: Vec<u8>,
}

/// A message on the wire or in a local queue, before delivery.
#[derive(Debug, Clone)]
struct WireMessage {
    type_index: u16,
    target: Option<NetId>,
    seq: u32,
    payload: Vec<u8>,
    /// Sent or received as `ReliableUnordered` in a `RUMSG` frame (not on the wire: the frame
    /// says it). A received one waits only for its target, not for its tick.
    unordered: bool,
}

#[derive(Debug, Clone)]
struct PendingMessage {
    msg: WireMessage,
    from: PeerId,
    sent_tick: NetTick,
    /// The receiver's local tick count when it arrived (waits are counted in local ticks, so a
    /// stalled state stream still times a message out).
    first_seen: u64,
    sequenced: bool,
}

/// A disconnect the node decided (the transport closes the connection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetDisconnect {
    /// The peer.
    pub peer: PeerId,
    /// Why.
    pub reason: DisconnectReason,
    /// The validation reason (for `InvalidMessage`) or a short detail.
    pub detail: &'static str,
}

#[derive(Debug)]
struct PeerLink {
    flood: FloodGuard,
    inbound: Vec<(WireMessage, NetTick, Channel)>,
    resync_ids: usize,
    reliable_out: Vec<WireMessage>,
    unreliable_out: Vec<WireMessage>,
    /// Queued unreliable messages per target entity, so the per-target cap is a counter, not a
    /// scan of the queue for every message (spec 120 § 8.4). Cleared with `unreliable_out`.
    unreliable_targets: HashMap<NetId, usize>,
    queued_bytes: usize,
    /// Reject log budget: `(second, lines logged in it)` (spec 70 § 6.4, ADR-5647).
    reject_log: (u32, u32),
    /// The malformed score of the runtime's own refusals (voice), the server's rule (ADR-8666):
    /// each adds its kind's `RejectKind::malformed_weight`, and the score leaks
    /// [`crate::inbound::MALFORMED_LEAK_PER_S`] points a second of ticks. At
    /// [`crate::server::MALFORMED_SCORE_LIMIT`] the peer is disconnected with `MalformedPacket`.
    score: RuntimeScore,
    /// The peer is dormant (spec 120 § 10.3): no frames are written for it, and its reliable game
    /// messages wait here with the tick they were queued at until it resumes.
    dormant: Option<HeldMessages>,
}

/// A peer's runtime malformed score (ADR-8666): the server transport's leaky bucket
/// (`inbound::PeerInbound`, ADR-8181/8606) in ticks instead of milliseconds. Each refusal adds its
/// kind's weight; the score loses [`crate::inbound::MALFORMED_LEAK_PER_S`] points per second of
/// ticks, exactly over time (the leak of ticks `a..b` is `L(b) - L(a)` with
/// `L(t) = leak x 1,000 x t / tps` in thousandths of a point), and no pass earns credit.
#[derive(Debug, Clone, Copy)]
struct RuntimeScore {
    /// Points times 1,000.
    milli: u64,
    /// The tick the leak was taken up to.
    clock: u32,
    /// Ticks per second the leak is spread over (at least 1).
    tps: u64,
}

impl RuntimeScore {
    /// Thousandths of a point per point.
    const SCALE: u64 = 1_000;

    fn new(tps: u32, now: NetTick) -> Self {
        Self {
            milli: 0,
            clock: now.0,
            tps: u64::from(tps.max(1)),
        }
    }

    fn set_ticks_per_second(&mut self, tps: u32) {
        self.tps = u64::from(tps.max(1));
    }

    /// Leaks the score up to tick `now`. A tick at or before the clock leaks nothing.
    fn leak_to(&mut self, now: NetTick) {
        if now.0 <= self.clock {
            return;
        }
        let total = |t: u32| crate::inbound::MALFORMED_LEAK_PER_S * Self::SCALE * u64::from(t) / self.tps;
        let leaked = total(now.0).saturating_sub(total(self.clock));
        self.milli = self.milli.saturating_sub(leaked);
        self.clock = now.0;
    }

    /// Leaks up to `now`, then adds `points`; true when the score reached the limit.
    fn charge(&mut self, now: NetTick, points: u32) -> bool {
        self.leak_to(now);
        self.milli = self
            .milli
            .saturating_add(u64::from(points).saturating_mul(Self::SCALE));
        self.milli >= u64::from(crate::server::MALFORMED_SCORE_LIMIT).saturating_mul(Self::SCALE)
    }

    /// Whole points.
    fn points(&self) -> u32 {
        u32::try_from(self.milli / Self::SCALE).unwrap_or(u32::MAX)
    }
}

/// A dormant peer's reliable messages (spec 120 § 10.3), oldest first, with the tick each was
/// queued at.
#[derive(Debug, Default)]
struct HeldMessages {
    since: NetTick,
    messages: VecDeque<(NetTick, WireMessage)>,
    bytes: usize,
    /// Messages dropped because the held queue passed its bound.
    dropped: u64,
}

impl HeldMessages {
    fn hold(&mut self, tick: NetTick, messages: Vec<WireMessage>) {
        for m in messages {
            self.bytes += m.payload.len();
            self.messages.push_back((tick, m));
        }
        // A dormant peer is never kicked for its queue: the oldest messages make room.
        while self.messages.len() > MAX_RELIABLE_QUEUE || self.bytes > MAX_RELIABLE_QUEUE_BYTES {
            let Some((_, m)) = self.messages.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(m.payload.len());
            self.dropped += 1;
        }
    }
}

/// Reject log lines per peer per second (spec 70 § 6.4: "a per-peer log cap of 10 lines per
/// second"). Every reject is still counted.
const REJECT_LOG_LINES_PER_SECOND: u32 = 10;

impl PeerLink {
    /// Spends one line of this peer's reject log budget for the second `tick` falls in (spec 70
    /// § 6.4, ADR-5647): false once [`REJECT_LOG_LINES_PER_SECOND`] lines were logged in it.
    fn log_budget(&mut self, tick: NetTick) -> bool {
        let second = tick.0 / self.flood.ticks_per_second().max(1);
        if self.reject_log.0 != second {
            self.reject_log = (second, 0);
        }
        let under = self.reject_log.1 < REJECT_LOG_LINES_PER_SECOND;
        if under {
            self.reject_log.1 += 1;
        }
        under
    }
}

/// One refusal the replication runtime decided (server; spec 120 § 11.2, ADR-7984): who sent it,
/// what it was, and a short detail. Read with [`NetNode::take_rejections`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetRejection {
    /// The sender.
    pub peer: PeerId,
    /// What was refused.
    pub kind: RejectKind,
    /// A short detail (the message type's name for `server_only_message`).
    pub detail: &'static str,
    /// The server tick it was decided at.
    pub tick: NetTick,
}

/// Rejections [`NetNode`] keeps for a host that has not taken them; older ones are dropped and
/// counted (`NetNode::rejections_dropped`), so a flood cannot grow the log.
pub const MAX_KEPT_REJECTIONS: usize = 1024;

/// The per-process net runtime state. A resource.
#[derive(Resource, Debug)]
pub struct NetNode {
    /// Server: the tick being simulated. Client: the newest applied server tick.
    pub tick: NetTick,
    /// Received frames, filled by the transport.
    pub inbox: Vec<Datagram>,
    /// Frames to send, drained by the transport.
    pub outbox: Vec<Datagram>,
    /// Disconnects decided by the node, drained by the transport.
    pub disconnects: Vec<NetDisconnect>,
    /// Set by `NetPlugin`'s receive system when the previous frame's simulation step reached
    /// `stall_forgive_ms` (spec 70 § 6.6); the next fixed tick forgives the burst.
    pub stalled: bool,
    /// Client: the server closed the connection.
    pub closed: Option<DisconnectReason>,
    out_queue: Vec<OutMessage>,
    local: Vec<PendingMessage>,
    peers: BTreeMap<PeerId, PeerLink>,
    pending_frames: BTreeMap<u32, Vec<u8>>,
    pending_frame_bytes: usize,
    /// State frames waiting for `NetSet::Apply`: `(tick, seq, frame)`.
    pending_states: Vec<(NetTick, u32, Vec<u8>)>,
    pending_msgs: Vec<PendingMessage>,
    pending_msg_bytes: usize,
    newest_seq: HashMap<(u16, Option<NetId>), u32>,
    /// Server: the newest sequenced message per (sender, type, target).
    newest_seq_in: HashMap<(PeerId, u16, Option<NetId>), u32>,
    send_seq: u32,
    /// The message sequence of a recorder's connection, apart from the clients' (spec 103 § 3.4).
    virtual_seq: u32,
    broadcasts_this_tick: usize,
    /// The second (tick / tick rate) of the last broadcast-overflow warning (spec 70 § 6.5:
    /// warn once per second).
    broadcast_warned_second: Option<u32>,
    /// Lines logged by the two capped logs, for tests: `(reject lines, broadcast-overflow warnings)`.
    capped_logs: (u64, u64),
    send_errors_logged: HashSet<(u16, &'static str)>,
    /// Delivered messages in order `(tick, type index, from, target)`: the replay log of spec 70
    /// § 10 test 13 (kept only when `record` is on).
    pub delivered_log: Vec<(NetTick, u16, PeerId, Option<NetId>)>,
    /// Keep `delivered_log`.
    pub record: bool,
    /// Fixed ticks this node has applied (`NetSet::Apply` runs), the local clock of message waits.
    pub local_ticks: u64,
    seen: SeenSeqs,
    /// Refusals not yet taken by the host, oldest first (at most [`MAX_KEPT_REJECTIONS`]).
    rejections: VecDeque<NetRejection>,
    /// Every refusal decided, taken or not.
    rejected_total: u64,
    /// Refusals dropped from a full log before the host took them.
    rejections_dropped: u64,
    /// Every refusal counted per peer and code, kept after the host takes the log (KI-NT-025,
    /// ADR-10077; `net.rejections`).
    rejections_by_peer: crate::status::RejectionsByPeer,
    /// Disconnects decided outside a pass with world access (a charge from a system), applied by
    /// the next `net_receive` or `net_send`.
    pending_kicks: Vec<(PeerId, DisconnectReason, &'static str)>,
}

/// Recently received unreliable message sequence numbers per sender: the transport may duplicate
/// an unreliable datagram, a message still arrives at most once (spec 70 § 6.5 item 2). A sequence
/// that fell behind the window stays refused through `floor`, so an old datagram cannot be replayed.
#[derive(Debug, Default)]
struct SeenSeqs {
    by_peer: HashMap<PeerId, PeerSeqs>,
}

#[derive(Debug, Default)]
struct PeerSeqs {
    /// Accepted sequences still inside the window.
    set: BTreeSet<u32>,
    /// Highest accepted sequence, in serial order.
    newest: Option<u32>,
    /// The newest sequence that fell behind the window. The window drops the serial-oldest
    /// sequence that is still a safe distance behind `newest`, not the one inserted first and
    /// not a half-range stray: one reordered or far-ahead datagram must not become the floor
    /// for every later sequence at or below it.
    floor: Option<u32>,
}

/// Sequence numbers remembered per sender.
const SEEN_WINDOW: usize = 8192;

/// Furthest a remembered sequence may sit behind `newest` and still be dropped as the window's
/// oldest. The next two distances, `2^31 - 1` and `2^31`, are not behind the live counter in
/// signed serial order. Storing either as `floor` makes every later sequence fail
/// `seq_not_newer`.
const MAX_SAFE_BEHIND: u32 = (1u32 << 31) - 2;

/// Whether `seq` is strictly behind `newest` by at most [`MAX_SAFE_BEHIND`].
fn safe_window_behind(newest: u32, seq: u32) -> bool {
    let behind = newest.wrapping_sub(seq);
    (1..=MAX_SAFE_BEHIND).contains(&behind)
}

/// The member of `set` furthest behind `newest` in numeric serial order.
///
/// Values numerically above `newest` are the pre-wrap tail; the smallest of those is the
/// oldest. Otherwise every member is at or below `newest` and the numeric minimum is oldest.
/// A half-range stray can occupy that slot. Callers must not store it as the floor; see
/// `safe_window_behind`.
fn serial_oldest(set: &BTreeSet<u32>, newest: u32) -> Option<u32> {
    set.range(newest.wrapping_add(1)..)
        .next()
        .copied()
        .or_else(|| set.iter().next().copied())
}

impl SeenSeqs {
    /// Records `seq` from `peer`. False when it was already seen or has left the window.
    fn insert(&mut self, peer: PeerId, seq: u32) -> bool {
        let seen = self.by_peer.entry(peer).or_default();
        if seen.floor.is_some_and(|floor| seq_not_newer(seq, floor)) {
            return false;
        }
        // A half-range sequence is not a newer `send_seq` and is not a reorder the signed
        // window can represent. Recording it lets the numeric pre-wrap scan evict it as the
        // oldest and store it as `floor`, which then refuses every later honest sequence.
        if seen
            .newest
            .is_some_and(|newest| (seq.wrapping_sub(newest) as i32) <= 0 && !safe_window_behind(newest, seq))
        {
            return false;
        }
        if !seen.set.insert(seq) {
            return false;
        }
        if seen
            .newest
            .is_none_or(|newest| (seq.wrapping_sub(newest) as i32) > 0)
        {
            seen.newest = Some(seq);
        }
        while seen.set.len() > SEEN_WINDOW {
            let Some(newest) = seen.newest else {
                break;
            };
            let Some(old) = serial_oldest(&seen.set, newest) else {
                break;
            };
            if !seen.set.remove(&old) {
                break;
            }
            // A sequence that drifted onto the half-range boundary as `newest` advanced is
            // not a floor. Dropping it keeps the live counter admissible.
            if !safe_window_behind(newest, old) {
                continue;
            }
            if seen.floor.is_none_or(|floor| seq_not_newer(floor, old)) {
                seen.floor = Some(old);
            }
        }
        true
    }
}

impl Default for NetNode {
    fn default() -> Self {
        Self {
            tick: NetTick(0),
            inbox: Vec::new(),
            outbox: Vec::new(),
            disconnects: Vec::new(),
            stalled: false,
            closed: None,
            out_queue: Vec::new(),
            local: Vec::new(),
            peers: BTreeMap::new(),
            pending_frames: BTreeMap::new(),
            pending_frame_bytes: 0,
            pending_states: Vec::new(),
            pending_msgs: Vec::new(),
            pending_msg_bytes: 0,
            newest_seq: HashMap::new(),
            newest_seq_in: HashMap::new(),
            send_seq: 0,
            virtual_seq: 0,
            broadcasts_this_tick: 0,
            broadcast_warned_second: None,
            capped_logs: (0, 0),
            send_errors_logged: HashSet::new(),
            delivered_log: Vec::new(),
            record: false,
            local_ticks: 0,
            seen: SeenSeqs::default(),
            rejections: VecDeque::new(),
            rejected_total: 0,
            rejections_dropped: 0,
            rejections_by_peer: crate::status::RejectionsByPeer::default(),
            pending_kicks: Vec::new(),
        }
    }
}

impl NetNode {
    /// Charges one refusal to a connected client (server; spec 120 § 11.2, ADR-7984): records a
    /// [`NetRejection`] that names it, then either disconnects it (a fatal kind, with
    /// `ProtocolError`) or scores the kind's `RejectKind::malformed_weight` (16 points for
    /// `VoiceMalformed`) on a score that leaks 4 points a second of ticks, as the server's
    /// transport score does (ADR-8606, ADR-8666; at [`crate::server::MALFORMED_SCORE_LIMIT`]
    /// points, `MalformedPacket`). The disconnect is
    /// applied by the next `net_receive` or `net_send` of this node. Returns false (and records
    /// nothing) for a peer that is not connected, the server itself included.
    pub fn reject(&mut self, peer: PeerId, kind: RejectKind, detail: &'static str) -> bool {
        let tick = self.tick;
        let Some(link) = self.peers.get_mut(&peer) else {
            return false;
        };
        let kick = if kind.is_fatal() {
            Some((DisconnectReason::ProtocolError, kind.as_str()))
        } else {
            link.score
                .charge(tick, kind.malformed_weight())
                .then_some((DisconnectReason::MalformedPacket, kind.as_str()))
        };
        // The same per-peer budget as a validator's reject (spec 70 § 6.4): a flood of refusals
        // is counted in full but logs at most 10 lines a second (ADR-8438).
        if link.log_budget(tick) {
            self.capped_logs.0 += 1;
            tracing::debug!(target: "net", peer = peer.0, kind = kind.as_str(), detail, "net message rejected");
        }
        if self.rejections.len() >= MAX_KEPT_REJECTIONS {
            self.rejections.pop_front();
            self.rejections_dropped += 1;
        }
        self.rejections.push_back(NetRejection {
            peer,
            kind,
            detail,
            tick,
        });
        self.rejected_total += 1;
        self.rejections_by_peer.note(peer.0, kind.as_str());
        if let Some((reason, detail)) = kick
            && !self.pending_kicks.iter().any(|(p, _, _)| *p == peer)
        {
            self.pending_kicks.push((peer, reason, detail));
        }
        true
    }

    /// Takes the refusals recorded since the last call, oldest first (server).
    pub fn take_rejections(&mut self) -> Vec<NetRejection> {
        self.rejections.drain(..).collect()
    }

    /// The refusals not yet taken, oldest first, without taking them (for `net.status`).
    pub fn rejections(&self) -> impl DoubleEndedIterator<Item = &NetRejection> + ExactSizeIterator {
        self.rejections.iter()
    }

    /// Every refusal this node decided, and how many of them a full log dropped before the host
    /// took them: `(total, dropped)`.
    pub fn rejection_counts(&self) -> (u64, u64) {
        (self.rejected_total, self.rejections_dropped)
    }

    /// Every refusal this node decided, counted per peer and lowercase code, taken from the log
    /// or not; a peer's row stays after it leaves, up to [`crate::status::MAX_REJECTION_PEERS`]
    /// peers (KI-NT-025, ADR-10077). The agent reads it as `net.rejections`.
    pub fn rejections_by_peer(&self) -> &crate::status::RejectionsByPeer {
        &self.rejections_by_peer
    }

    /// A connected client's current malformed points (server), as of its last charge or pass:
    /// whole points, the leak since then not yet taken.
    pub fn peer_malformed_points(&self, peer: PeerId) -> Option<u32> {
        self.peers.get(&peer).map(|l| l.score.points())
    }

    /// State frames waiting for `NetSet::Apply` (client).
    pub fn pending_state_frames(&self) -> usize {
        self.pending_states.len()
    }

    /// Whether a client peer is connected (server). A recorder's virtual connection is not one (spec 103 § 3.1).
    pub fn has_peer(&self, peer: PeerId) -> bool {
        !crate::recorder::is_virtual_peer(peer) && self.peers.contains_key(&peer)
    }

    /// Whether `peer` has a link here, a recorder's virtual connection included.
    pub fn has_link(&self, peer: PeerId) -> bool {
        self.peers.contains_key(&peer)
    }

    /// Connected client peers (server), in id order. A recorder's virtual connection is not one
    /// (spec 103 § 3.1).
    pub fn peers(&self) -> Vec<PeerId> {
        self.peers
            .keys()
            .copied()
            .take_while(|p| !crate::recorder::is_virtual_peer(*p))
            .collect()
    }

    /// Everything the send path writes frames for: the clients and a recorder's connection.
    fn links(&self) -> Vec<PeerId> {
        self.peers.keys().copied().collect()
    }

    /// The connected client peer with the lowest id. The map is ordered by id, so this is the
    /// first key and it does not allocate.
    pub fn min_peer(&self) -> Option<PeerId> {
        self.peers.first_key_value().map(|(id, _)| *id)
    }

    /// A peer's flood state and last breach (server).
    pub fn peer_flood(&self, peer: PeerId) -> Option<(FloodState, u32, Option<NetTick>)> {
        self.peers.get(&peer).map(|p| {
            (
                p.flood.state(),
                p.flood.breaches_last_second(self.tick),
                p.flood.last_breach_tick(),
            )
        })
    }

    /// Client: whether a waiting message of `bytes` more would go past the pending bounds.
    fn pending_is_full(&self, bytes: usize) -> bool {
        self.pending_msgs.len() >= MAX_PENDING_MESSAGES
            || self.pending_msg_bytes.saturating_add(bytes) > MAX_PENDING_MESSAGE_BYTES
    }

    /// Client: drops what is pending from the previous server.
    pub(crate) fn forget_server(&mut self) {
        self.pending_frames.clear();
        self.pending_frame_bytes = 0;
        self.pending_states.clear();
        self.pending_msgs.clear();
        self.pending_msg_bytes = 0;
        self.newest_seq.clear();
        self.seen.by_peer.clear();
    }

    /// Client: frames and messages waiting to be applied.
    pub fn pending_counts(&self) -> (usize, usize) {
        (self.pending_frames.len(), self.pending_msgs.len())
    }

    /// Client: the bytes of the frames waiting to be applied.
    pub fn pending_frame_bytes(&self) -> usize {
        self.pending_frame_bytes
    }

    /// The tick rate a peer's flood guard counts with (server).
    pub fn peer_ticks_per_second(&self, peer: PeerId) -> Option<u32> {
        self.peers.get(&peer).map(|p| p.flood.ticks_per_second())
    }

    /// Inbound items shed for a peer before they were stored (server).
    pub fn peer_inbound_shed(&self, peer: PeerId) -> Option<u64> {
        self.peers.get(&peer).map(|p| p.flood.shed())
    }

    pub(crate) fn queue_out(&mut self, msg: OutMessage) {
        self.out_queue.push(msg);
    }

    pub(crate) fn log_send_error_once(&mut self, type_index: u16, err: NetSendError) {
        let key = match err {
            NetSendError::NotServer => "not_server",
            NetSendError::NotClient => "not_client",
            _ => "other",
        };
        if self.send_errors_logged.insert((type_index, key)) {
            tracing::warn!(target: "net", type_index, error = %err, "net message send refused");
        }
    }
}

/// Ticks per second of this world: the fixed rate its net sets run at. A world with no app runs
/// [`crate::send::DEFAULT_TICK_HZ`] unless it holds a `FixedTime`.
pub fn tick_hz(world: &World) -> f64 {
    world
        .get_resource::<FixedTime>()
        .map_or(crate::send::DEFAULT_TICK_HZ, |f| 1.0 / f.step().as_secs_f64())
}

/// Server: a client peer finished its handshake and is in game (spec 77 drives this).
pub fn server_add_peer(world: &mut World, peer: PeerId) {
    ensure_resources(world);
    let tps = tick_hz(world).round() as u32;
    let node_tick = world.resource::<NetNode>().tick;
    world.resource_mut::<NetNode>().peers.insert(
        peer,
        PeerLink {
            flood: FloodGuard::new(tps),
            inbound: Vec::new(),
            resync_ids: 0,
            reliable_out: Vec::new(),
            unreliable_out: Vec::new(),
            unreliable_targets: HashMap::new(),
            queued_bytes: 0,
            reject_log: (0, 0),
            score: RuntimeScore::new(tps, node_tick),
            dormant: None,
        },
    );
    let wire = match world.resource::<NetSettings>().replication.state_channel {
        StateChannel::Unreliable => ConnWire::Undecided,
        StateChannel::Reliable => ConnWire::Stream,
    };
    world.resource_mut::<ServerReplication>().add_conn_on(peer, wire);
}

/// Server: a virtual connection (spec 103 § 3.1). It has a link for messages to queue on and no transport.
pub(crate) fn server_add_virtual_peer(world: &mut World, peer: PeerId) {
    ensure_resources(world);
    let tps = tick_hz(world).round() as u32;
    let node_tick = world.resource::<NetNode>().tick;
    world.resource_mut::<NetNode>().peers.insert(
        peer,
        PeerLink {
            flood: FloodGuard::new(tps),
            inbound: Vec::new(),
            resync_ids: 0,
            reliable_out: Vec::new(),
            unreliable_out: Vec::new(),
            unreliable_targets: HashMap::new(),
            queued_bytes: 0,
            reject_log: (0, 0),
            score: RuntimeScore::new(tps, node_tick),
            dormant: None,
        },
    );
}

/// Server: `peer`'s link goes dormant (spec 120 § 10.3): frames stop, unreliable messages are
/// dropped, reliable ones are held. False when the node does not know the peer.
pub(crate) fn node_hold_peer(world: &mut World, peer: PeerId) -> bool {
    let Some(mut node) = world.get_resource_mut::<NetNode>() else {
        return false;
    };
    let tick = node.tick;
    let Some(link) = node.peers.get_mut(&peer) else {
        return false;
    };
    if link.dormant.is_none() {
        let mut held = HeldMessages {
            since: tick,
            ..HeldMessages::default()
        };
        held.hold(tick, std::mem::take(&mut link.reliable_out));
        link.dormant = Some(held);
    }
    link.unreliable_out.clear();
    link.queued_bytes = 0;
    link.inbound.clear();
    true
}

/// What [`node_resume_peer`] did with a dormant peer's held messages.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct HeldOutcome {
    /// The node held the peer (it was dormant).
    pub was_dormant: bool,
    /// The tick it went dormant at.
    pub since: NetTick,
    /// Reliable messages delivered again.
    pub kept: u64,
    /// Reliable messages older than the grace, dropped.
    pub expired: u64,
    /// Reliable messages dropped while held because the queue passed its bound.
    pub overflowed: u64,
}

/// Server: a dormant (or taken over) peer is back on a new connection (spec 120 § 10.3). Held
/// reliable messages younger than `grace_ticks` go out again, older ones are dropped; the inbound
/// sequence state is forgotten, because the client's new connection numbers its messages afresh.
pub(crate) fn node_resume_peer(world: &mut World, peer: PeerId, grace_ticks: u32) -> HeldOutcome {
    let Some(mut node) = world.get_resource_mut::<NetNode>() else {
        return HeldOutcome::default();
    };
    let tick = node.tick;
    node.seen.by_peer.remove(&peer);
    node.newest_seq_in.retain(|(p, _, _), _| *p != peer);
    let Some(link) = node.peers.get_mut(&peer) else {
        return HeldOutcome::default();
    };
    link.inbound.clear();
    let Some(held) = link.dormant.take() else {
        return HeldOutcome::default();
    };
    let mut out = HeldOutcome {
        was_dormant: true,
        since: held.since,
        overflowed: held.dropped,
        ..HeldOutcome::default()
    };
    let mut restored: Vec<WireMessage> = Vec::with_capacity(held.messages.len());
    for (queued, m) in held.messages {
        if tick.0.saturating_sub(queued.0) > grace_ticks {
            out.expired += 1;
        } else {
            out.kept += 1;
            link.queued_bytes += m.payload.len();
            restored.push(m);
        }
    }
    restored.append(&mut link.reliable_out);
    link.reliable_out = restored;
    out
}

/// Server: whether the node has a link for `peer` (live or dormant).
pub(crate) fn node_knows_peer(world: &World, peer: PeerId) -> bool {
    world
        .get_resource::<NetNode>()
        .is_some_and(|n| n.peers.contains_key(&peer))
}

/// Server: whether the node holds `peer` dormant (spec 120 § 10.3).
pub fn server_peer_is_dormant(world: &World, peer: PeerId) -> bool {
    world
        .get_resource::<NetNode>()
        .and_then(|n| n.peers.get(&peer))
        .is_some_and(|l| l.dormant.is_some())
}

/// Server: a client peer left or was removed. Its entities lose `NetInterrupted` here, before
/// the game keeps, despawns or hands them off (spec 120 § 10.2).
pub fn server_remove_peer(world: &mut World, peer: PeerId) {
    crate::model::mark_peer_interrupted(world, peer, false);
    if let Some(mut node) = world.get_resource_mut::<NetNode>() {
        node.peers.remove(&peer);
        node.seen.by_peer.remove(&peer);
        node.newest_seq_in.retain(|(p, _, _), _| *p != peer);
    }
    if let Some(mut rep) = world.get_resource_mut::<ServerReplication>() {
        rep.remove_conn(peer);
    }
    crate::predict::server_remove_peer(world, peer);
}

/// Server: a peer's transport reports it interrupted (`true`: silent past `interrupt_ms`) or
/// restored (`false`), for a game that drives its own transport (spec 120 § 10.2). The peer's
/// owned entities and player controllers get or lose `NetInterrupted`; an entity that changes
/// owner, or a peer that leaves, loses it at the next send. `NetLink` and `NetSession` call this
/// for you.
pub fn server_set_peer_interrupted(world: &mut World, peer: PeerId, interrupted: bool) {
    crate::model::mark_peer_interrupted(world, peer, interrupted);
}

/// Server: disconnects a peer with a reason (the transport closes the connection).
pub fn server_kick(world: &mut World, peer: PeerId, reason: DisconnectReason, detail: &'static str) {
    server_remove_peer(world, peer);
    if let Some(mut node) = world.get_resource_mut::<NetNode>() {
        tracing::info!(target: "net", peer = peer.0, reason = reason.as_str(), detail, "net peer disconnected");
        node.disconnects.push(NetDisconnect { peer, reason, detail });
    }
}

/// Applies the disconnects [`NetNode::reject`] decided (server).
fn apply_pending_kicks(world: &mut World) {
    let kicks = std::mem::take(&mut world.resource_mut::<NetNode>().pending_kicks);
    for (peer, reason, detail) in kicks {
        if world.resource::<NetNode>().has_peer(peer) {
            server_kick(world, peer, reason, detail);
        }
    }
}

/// Inserts every resource the runtime needs (idempotent).
pub fn ensure_resources(world: &mut World) {
    world.init_resource::<NetNode>();
    world.init_resource::<NetEntities>();
    world.init_resource::<NetMessageRegistry>();
    world.init_resource::<ReplicationRegistry>();
    world.init_resource::<ServerReplication>();
    world.init_resource::<ClientReplication>();
    world.init_resource::<replication::NetEvents>();
    world.init_resource::<OwnerCache>();
    world.init_resource::<NetSettings>();
    world.init_resource::<NetMode>();
    if world.get_resource::<LocalPeer>().is_none() {
        let mode = *world.resource::<NetMode>();
        world.insert_resource(LocalPeer(mode.is_server().then_some(PeerId::SERVER)));
    }
}

/// The protocol hash (spec 70 § 6.1, spec 71 § 11): every registered message type (path, direction,
/// channel, layout) and every replicated component (path, order, audience variant, codec). Client
/// and server must agree to join. The protocol version is compared on its own: builds that differ
/// only in it negotiate (ADR-1950). The hash starts with the session message layout revision
/// ([`SESSION_WIRE_REVISION`](crate::session::proto::SESSION_WIRE_REVISION); the constant 1 until
/// ADR-7259), so builds whose session messages differ refuse each other at the handshake. It ends
/// with the transform record's revision
/// ([`TRANSFORM_RECORD_REVISION`](crate::replication::TRANSFORM_RECORD_REVISION), spec 120 § 8.2,
/// ADR-8467), so a build with another transform record is refused instead of misreading it.
pub fn protocol_hash(world: &World) -> u64 {
    let mut h = LayoutHasher::new();
    h.write_u64(crate::session::proto::SESSION_WIRE_REVISION);
    if let Some(m) = world.get_resource::<NetMessageRegistry>() {
        m.hash_into(&mut h);
    }
    if let Some(r) = world.get_resource::<ReplicationRegistry>() {
        r.hash_into(&mut h);
    }
    if let Some(p) = world.get_resource::<crate::predict::PredictionRegistry>() {
        p.hash_into(&mut h);
    }
    h.write_str(crate::replication::TRANSFORM_RECORD_TYPE);
    h.write_u64(crate::replication::TRANSFORM_RECORD_REVISION);
    h.finish()
}

/// The registered types behind [`protocol_hash`], one `(type path, fingerprint)` each, sorted by
/// name, plus [`SESSION_WIRE_TYPE`] with the session message layout revision and
/// [`TRANSFORM_RECORD_TYPE`](crate::replication::TRANSFORM_RECORD_TYPE) with the transform record's
/// revision (ADR-8467). Two processes whose
/// hashes differ compare these to name the differing types (dev builds, spec 77 § 4 item 2).
pub fn protocol_types(world: &World) -> Vec<(String, u64)> {
    let mut out = vec![
        (
            SESSION_WIRE_TYPE.to_owned(),
            crate::session::proto::SESSION_WIRE_REVISION,
        ),
        (
            crate::replication::TRANSFORM_RECORD_TYPE.to_owned(),
            crate::replication::TRANSFORM_RECORD_REVISION,
        ),
    ];
    if let Some(m) = world.get_resource::<NetMessageRegistry>() {
        out.extend(m.type_fingerprints());
    }
    if let Some(r) = world.get_resource::<ReplicationRegistry>() {
        out.extend(r.type_fingerprints());
    }
    if let Some(p) = world.get_resource::<crate::predict::PredictionRegistry>() {
        out.extend(p.type_fingerprints());
    }
    out.sort();
    out
}

/// The row of [`protocol_types`] that names the session message layout: a dev client of another
/// revision sees `regia_net::session::wire (different)` (or `missing on server` against a build
/// from before ADR-7259) in its `protocol_mismatch`.
pub const SESSION_WIRE_TYPE: &str = "regia_net::session::wire";

fn mode(world: &World) -> NetMode {
    world.get_resource::<NetMode>().copied().unwrap_or_default()
}

// ---- frame codec -------------------------------------------------------------------------------

fn write_messages(w: &mut BitWriter, msgs: &[WireMessage], with_seq: bool) {
    w.write_varint(msgs.len() as u64);
    for m in msgs {
        w.write_varint(u64::from(m.type_index));
        w.write_bool(m.target.is_some());
        if let Some(t) = m.target {
            w.write_varint(u64::from(t.0));
        }
        if with_seq {
            w.write_varint(u64::from(m.seq));
        }
        w.write_varint(m.payload.len() as u64);
        w.write_bytes(&m.payload);
    }
}

fn read_messages(r: &mut BitReader, with_seq: bool) -> Result<Vec<WireMessage>, DecodeError> {
    let n = read_message_count(r)?;
    read_message_list(r, n, with_seq)
}

fn read_message_count(r: &mut BitReader) -> Result<usize, DecodeError> {
    // Every message costs at least 17 bits (type, target bit, length), so the input bounds the count
    // before any work.
    let most = (r.remaining_bits() / 17).min(1 << 16);
    r.read_len(most)
}

fn read_message_list(r: &mut BitReader, n: usize, with_seq: bool) -> Result<Vec<WireMessage>, DecodeError> {
    // Every message costs at least 26 bits (type, target bit, length): size the buffer by that.
    let mut out = Vec::with_capacity(n.min(r.remaining_bits() / 26));
    for _ in 0..n {
        let type_index = u16::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("type"))?;
        let target = if r.read_bool()? {
            Some(NetId(
                u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("id"))?,
            ))
        } else {
            None
        };
        let seq = if with_seq {
            u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("seq"))?
        } else {
            0
        };
        let len = r.read_len(crate::message::MAX_MESSAGE_BYTES)?;
        let payload = r.read_bytes(len)?;
        out.push(WireMessage {
            type_index,
            target,
            seq,
            payload,
            unordered: false,
        });
    }
    Ok(out)
}

/// Whether a message of `channel` with `len` payload bytes goes in a `RUMSG` frame (ADR-10052 (3)):
/// `ReliableUnordered` that fits one frame part. A larger one keeps its tick's ordered frame.
fn unordered_class(channel: Channel, len: usize) -> bool {
    channel == Channel::ReliableUnordered && len <= MAX_RUMSG_PAYLOAD
}

/// `msgs` as `RUMSG` frames of `tick`, each within one bridge part: a new frame starts before the
/// payload bytes would pass [`MAX_RUMSG_PAYLOAD`] (every message fits one on its own).
fn rumsg_frames(tick: NetTick, msgs: &[WireMessage]) -> Vec<Vec<u8>> {
    let mut frames = Vec::new();
    let (mut start, mut bytes) = (0, 0);
    let mut emit = |batch: &[WireMessage]| {
        let mut w = BitWriter::new();
        w.write_bits(u64::from(TAG_RUMSG), 8);
        w.write_varint(u64::from(tick.0));
        write_messages(&mut w, batch, false);
        frames.push(w.finish());
    };
    for (i, m) in msgs.iter().enumerate() {
        // Each message's own framing (type, target, length) is under 16 bytes; 512 bytes of
        // headroom cover 32 messages, so the budget also counts them.
        let cost = m.payload.len() + 16;
        if i > start && (bytes + cost > MAX_RUMSG_PAYLOAD || i - start >= 32) {
            emit(&msgs[start..i]);
            (start, bytes) = (i, 0);
        }
        bytes += cost;
    }
    if start < msgs.len() {
        emit(&msgs[start..]);
    }
    frames
}

fn read_tick(r: &mut BitReader) -> Result<NetTick, DecodeError> {
    Ok(NetTick(
        u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("tick"))?,
    ))
}

// ---- NetSet::Receive -----------------------------------------------------------------------------

/// `NetSet::Receive`: reads the inbox into per-tick queues. A malformed frame from a client
/// disconnects it with `MalformedPacket`.
pub fn net_receive(world: &mut World) {
    let _span = tracing::trace_span!("net_messages_receive").entered();
    ensure_resources(world);
    // Handles the asset server loaded since last frame resolve from here on (ADR-2665 (3)).
    crate::assets::register_asset_server_handles(world);
    let inbox = std::mem::take(&mut world.resource_mut::<NetNode>().inbox);
    if mode(world).is_server() {
        apply_pending_kicks(world);
        let mut kicks = Vec::new();
        for d in inbox {
            if !world.resource::<NetNode>().has_peer(d.peer) {
                continue;
            }
            match server_read_frame(world, &d) {
                Ok(()) => {}
                Err(_) => kicks.push(d.peer),
            }
        }
        for p in kicks {
            server_kick(world, p, DisconnectReason::MalformedPacket, "frame");
        }
    } else {
        for d in inbox {
            if client_read_frame(world, &d).is_err() {
                tracing::warn!(target: "net", "net: malformed frame from the server");
                world.resource_mut::<NetNode>().closed = Some(DisconnectReason::ProtocolError);
            }
        }
    }
}

fn server_read_frame(world: &mut World, d: &Datagram) -> Result<(), DecodeError> {
    let mut r = BitReader::new(&d.bytes);
    let tag = r.read_bits(8)? as u8;
    match tag {
        TAG_CMSG => {
            let tick = read_tick(&mut r)?;
            let n = read_message_count(&mut r)?;
            if !inbound_room(world, d.peer, n) {
                return Ok(());
            }
            let msgs = read_message_list(&mut r, n, false)?;
            let n = r.read_len(MAX_RESYNC_IDS_PER_FRAME)?;
            let mut resync = Vec::with_capacity(n);
            for _ in 0..n {
                resync.push(NetId(
                    u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("id"))?,
                ));
            }
            let mut node = world.resource_mut::<NetNode>();
            let mut wanted = true;
            if let Some(link) = node.peers.get_mut(&d.peer) {
                for m in msgs {
                    link.inbound.push((m, tick, d.channel));
                }
                link.resync_ids += n;
                if link.resync_ids > MAX_RESYNC_IDS_PER_TICK {
                    link.flood.note_overflow(n);
                    wanted = false;
                }
            }
            if wanted {
                world
                    .resource_mut::<ServerReplication>()
                    .request_resync(d.peer, resync);
            }
        }
        TAG_RUMSG => {
            let tick = read_tick(&mut r)?;
            let n = read_message_count(&mut r)?;
            if !inbound_room(world, d.peer, n) {
                return Ok(());
            }
            let msgs = read_message_list(&mut r, n, false)?;
            let mut node = world.resource_mut::<NetNode>();
            if let Some(link) = node.peers.get_mut(&d.peer) {
                for m in msgs {
                    link.inbound.push((m, tick, Channel::ReliableUnordered));
                }
            }
        }
        TAG_UMSG => {
            let tick = read_tick(&mut r)?;
            let n = read_message_count(&mut r)?;
            if !inbound_room(world, d.peer, n) {
                return Ok(());
            }
            let msgs = read_message_list(&mut r, n, true)?;
            let mut node = world.resource_mut::<NetNode>();
            let fresh: Vec<WireMessage> = msgs
                .into_iter()
                .filter(|m| node.seen.insert(d.peer, m.seq))
                .collect();
            if let Some(link) = node.peers.get_mut(&d.peer) {
                for m in fresh {
                    link.inbound.push((m, tick, Channel::Unreliable));
                }
            }
        }
        TAG_ACK => {
            let tick = read_tick(&mut r)?;
            let state = StateAck::read(&mut r)?;
            // A client cannot have applied a tick the server has not sent: such an ack would put
            // the entities only it holds to sleep.
            let now = world.resource::<NetNode>().tick;
            if tick <= now {
                let entropy = configured_entropy(world);
                world.resource_mut::<ServerReplication>().ack_state_at(
                    d.peer,
                    tick,
                    state,
                    entropy,
                    Some(now),
                );
            }
        }
        crate::predict::TAG_INPUT => crate::predict::server_read_input(world, d.peer, &mut r)?,
        _ => return Err(DecodeError::Invalid("frame tag")),
    }
    Ok(())
}

/// The server's configured entropy stage for new connections (spec 72 § 18, ADR-3315):
/// `config/net.ron`'s `replication.entropy`, unless a dedicated host's `server.toml` names
/// `[net] entropy`, which overrides it (spec 72 § 8 precedence). Negotiated per connection when
/// the first `ACK` arrives.
fn configured_entropy(world: &World) -> crate::entropy::EntropyMode {
    let host = world
        .get_resource::<crate::session::NetSession>()
        .filter(|s| s.is_dedicated())
        .and_then(|s| s.server())
        .and_then(|s| s.options().entropy);
    host.unwrap_or_else(|| {
        world
            .get_resource::<NetSettings>()
            .map(|s| s.replication.entropy)
            .unwrap_or_default()
    })
}

/// Whether `n` more messages fit `peer`'s inbound queue. When they do not the frame is shed
/// undecoded and the peer is charged a breach.
fn inbound_room(world: &mut World, peer: PeerId, n: usize) -> bool {
    let per_tick = world.resource::<NetSettings>().messages.peer_cap_per_tick;
    let cap = per_tick.saturating_mul(INBOUND_QUEUE_TICKS) as usize;
    let mut node = world.resource_mut::<NetNode>();
    let Some(link) = node.peers.get_mut(&peer) else {
        return false;
    };
    if link.inbound.len().saturating_add(n) <= cap {
        return true;
    }
    link.flood.note_overflow(n);
    false
}

fn client_read_frame(world: &mut World, d: &Datagram) -> Result<(), DecodeError> {
    let mut r = BitReader::new(&d.bytes);
    let tag = r.read_bits(8)? as u8;
    match tag {
        TAG_TICK => {
            let tick = read_tick(&mut r)?;
            let (applied, stream) = {
                let client = world.resource::<ClientReplication>();
                (client.applied, client.applied_stream)
            };
            // The last tick number ends a session; nothing may run on it (`NetTick::next` pins).
            if tick.0 == u32::MAX {
                return Err(DecodeError::Invalid("tick out of range"));
            }
            let mut node = world.resource_mut::<NetNode>();
            let newest = node
                .pending_frames
                .last_key_value()
                .map(|(t, _)| *t)
                .max(applied.map(|a| a.0));
            if let Some(newest) = newest {
                // The stream is ordered, so a far jump either way is corrupt, and would otherwise
                // strand the client: every later frame would look already applied.
                if tick.0.abs_diff(newest) > MAX_TICK_JUMP {
                    return Err(DecodeError::Invalid("tick jump"));
                }
            }
            // A tick already applied is never applied twice (a state frame moves `applied` only).
            if stream.is_some_and(|a| tick <= a) {
                return Ok(());
            }
            if node.pending_frames.len() >= MAX_PENDING_FRAMES
                || node.pending_frame_bytes + d.bytes.len() > MAX_PENDING_FRAME_BYTES
            {
                return Err(DecodeError::Invalid("too many pending frames"));
            }
            node.pending_frame_bytes += d.bytes.len();
            if let Some(old) = node.pending_frames.insert(tick.0, d.bytes.clone()) {
                node.pending_frame_bytes = node.pending_frame_bytes.saturating_sub(old.len());
            }
        }
        TAG_STATE => {
            let (tick, seq) = replication::read_state_header(&mut r)?;
            let mut node = world.resource_mut::<NetNode>();
            // A dropped frame is a lost frame: it stays unacknowledged and the next delta covers it.
            if node.pending_states.len() < MAX_PENDING_STATE_FRAMES {
                node.pending_states.push((tick, seq, d.bytes.clone()));
            }
        }
        crate::entropy::TAG_STATE_ENTROPY => {
            // The entropy-negotiated state frame (spec 72 § 18, ADR-3315): same header and the
            // same loss semantics as TAG_STATE; the flags byte after the header is read when the
            // frame is applied.
            let (tick, seq) = replication::read_state_header(&mut r)?;
            let mut node = world.resource_mut::<NetNode>();
            if node.pending_states.len() < MAX_PENDING_STATE_FRAMES {
                node.pending_states.push((tick, seq, d.bytes.clone()));
            }
        }
        TAG_RUMSG => {
            let tick = read_tick(&mut r)?;
            let msgs = read_messages(&mut r, false)?;
            let mut arrived: Vec<(u16, u64)> = Vec::new();
            let mut shed = Vec::new();
            let mut node = world.resource_mut::<NetNode>();
            let now = node.local_ticks;
            for mut m in msgs {
                m.unordered = true;
                arrived.push((m.type_index, m.payload.len() as u64));
                if node.pending_is_full(m.payload.len()) {
                    shed.push(m.type_index);
                    continue;
                }
                node.pending_msg_bytes += m.payload.len();
                node.pending_msgs.push(PendingMessage {
                    msg: m,
                    from: PeerId::SERVER,
                    sent_tick: tick,
                    first_seen: now,
                    sequenced: false,
                });
            }
            for type_index in shed {
                count(world, type_index, |c| c.dropped_overflow += 1);
            }
            for (type_index, len) in arrived {
                count(world, type_index, |c| {
                    c.received += 1;
                    c.bytes_received += len;
                });
            }
        }
        TAG_UMSG => {
            let tick = read_tick(&mut r)?;
            let msgs = read_messages(&mut r, true)?;
            let mut arrived: Vec<(u16, u64)> = Vec::new();
            let mut node = world.resource_mut::<NetNode>();
            let now = node.local_ticks;
            let mut shed = Vec::new();
            for m in msgs {
                if !node.seen.insert(PeerId::SERVER, m.seq) {
                    continue;
                }
                arrived.push((m.type_index, m.payload.len() as u64));
                if node.pending_is_full(m.payload.len()) {
                    shed.push(m.type_index);
                    continue;
                }
                node.pending_msg_bytes += m.payload.len();
                node.pending_msgs.push(PendingMessage {
                    msg: m,
                    from: PeerId::SERVER,
                    sent_tick: tick,
                    first_seen: now,
                    sequenced: true,
                });
            }
            for type_index in shed {
                count(world, type_index, |c| c.dropped_overflow += 1);
            }
            for (type_index, len) in arrived {
                count(world, type_index, |c| {
                    c.received += 1;
                    c.bytes_received += len;
                });
            }
        }
        crate::predict::TAG_INPUT_ACK => crate::predict::client_read_ack(world, &mut r)?,
        _ => return Err(DecodeError::Invalid("frame tag")),
    }
    Ok(())
}

// ---- NetSet::Apply -------------------------------------------------------------------------------

/// `NetSet::Apply` (start of each fixed tick): clears last tick's messages and events, then on a
/// client applies queued server ticks in tick order (spawns, state, messages, despawns), and on a
/// server delivers this tick's to-server and local messages.
pub fn net_apply(world: &mut World) {
    let _span = tracing::trace_span!("net_apply_tick").entered();
    ensure_resources(world);
    let registry = world.resource::<NetMessageRegistry>().clone();
    for t in registry.types() {
        (t.clear)(world);
    }
    replication::clear_events(world);
    crate::predict::clear_events(world);
    world.resource_mut::<NetNode>().local_ticks += 1;
    if mode(world).is_server() {
        replication::deliver_server_notices(world);
        server_apply(world, &registry);
        crate::predict::server_consume(world);
    } else {
        client_apply(world, &registry);
        // Copies without interpolation whose records are steady move along them (ADR-8522).
        replication::advance_steady_copies(world);
    }
}

fn deliver(
    world: &mut World,
    registry: &NetMessageRegistry,
    m: &WireMessage,
    from: PeerId,
    sent_tick: NetTick,
    late: bool,
) -> Result<(), DecodeError> {
    let Some(t) = registry.get(m.type_index) else {
        return Err(DecodeError::Invalid("unknown message type"));
    };
    let target = match m.target {
        Some(id) => match world.resource::<NetEntities>().entity(id) {
            Some(e) => Some(e),
            None => {
                count(world, m.type_index, |c| c.unresolved += 1);
                return Ok(());
            }
        },
        None => None,
    };
    let map = world.resource::<NetEntities>().clone();
    let asset_cx = message_asset_context(world);
    let cx = CodecContext::new(&map).with_assets(asset_cx.as_deref());
    let mut r = BitReader::new(&m.payload);
    let decoded = (t.decode)(&mut r, &cx);
    record_message_asset_misses(world, &cx, t.name);
    let msg = decoded?;
    count(world, m.type_index, |c| {
        c.delivered += 1;
        if late {
            c.late += 1;
        }
    });
    let tick = world.resource::<NetNode>().tick;
    let mut node = world.resource_mut::<NetNode>();
    if node.record {
        node.delivered_log.push((tick, m.type_index, from, m.target));
    }
    (t.deliver)(world, msg, target, from, sent_tick, late);
    Ok(())
}

/// The world's asset-server context for message payloads: a `Handle<T>` field in a net message
/// resolves through it exactly as in a replicated component (spec 71 § 3, ADR-2665 (2)).
fn message_asset_context(world: &World) -> Option<std::sync::Arc<crate::codec::AssetContext>> {
    world
        .get_resource::<crate::assets::NetAssets>()
        .and_then(crate::assets::NetAssets::context)
}

/// Handle fields of a message that decoded unset are listed in `NetAssets` under the message's
/// name and logged once per key (`net.asset_missing`), as for components.
fn record_message_asset_misses(world: &mut World, cx: &CodecContext, message: &str) {
    let misses = cx.take_asset_misses();
    if misses.is_empty() {
        return;
    }
    let tick = world.resource::<NetNode>().tick;
    world
        .get_resource_or_insert_with(crate::assets::NetAssets::default)
        .record(misses, message, tick);
}

fn count(world: &mut World, type_index: u16, f: impl FnOnce(&mut crate::message::MessageCounters)) {
    if let Some(c) = world
        .resource_mut::<NetMessageRegistry>()
        .counters_mut(type_index)
    {
        f(c);
    }
}

fn server_apply(world: &mut World, registry: &NetMessageRegistry) {
    let settings = world.resource::<NetSettings>().messages.clone();
    let tick = world.resource::<NetNode>().tick;
    // Local messages queued last tick (offline, listen host, server-owned targets).
    let local = std::mem::take(&mut world.resource_mut::<NetNode>().local);
    for p in local {
        let _ = deliver(world, registry, &p.msg, p.from, p.sent_tick, false);
    }
    let rates = registry.rates(settings.default_rate);
    let forgive = std::mem::take(&mut world.resource_mut::<NetNode>().stalled);
    let peers = world.resource::<NetNode>().peers();
    let tps = tick_hz(world).round() as u32;
    let mut owners = world.remove_resource::<OwnerCache>().unwrap_or_default();
    // One walk per target entity in this pass, never one per message (spec 70 § 11, ADR-5647). An
    // earlier pass or an agent `net.set_owner` may have changed ownership since the cache was filled.
    owners.clear();
    for peer in peers {
        let inbound = {
            let mut node = world.resource_mut::<NetNode>();
            let Some(link) = node.peers.get_mut(&peer) else {
                continue;
            };
            link.resync_ids = 0;
            link.flood.set_ticks_per_second(tps);
            link.flood.begin_tick(forgive);
            // No pass earns credit (ADR-8666, the server's rule since ADR-8181): the score leaks
            // with ticks instead, so bad traffic mixed with good still adds up.
            link.score.set_ticks_per_second(tps);
            link.score.leak_to(tick);
            link.flood.refill(&rates);
            std::mem::take(&mut link.inbound)
        };
        let mut kick: Option<(DisconnectReason, &'static str)> = None;
        for (m, sent_tick, channel) in inbound {
            if kick.is_some() {
                break;
            }
            let Some(t) = registry.get(m.type_index) else {
                kick = Some((DisconnectReason::MalformedPacket, "message type"));
                break;
            };
            let len = m.payload.len() as u64;
            count(world, m.type_index, |c| {
                c.received += 1;
                c.bytes_received += len;
            });
            let admit = {
                let mut node = world.resource_mut::<NetNode>();
                match node.peers.get_mut(&peer) {
                    Some(link) => link.flood.admit(
                        usize::from(m.type_index),
                        t.rate.unwrap_or(settings.default_rate),
                        t.always_admit,
                        m.payload.len(),
                        &settings,
                    ),
                    None => Admit::RateLimited,
                }
            };
            if admit != Admit::Admitted {
                count(world, m.type_index, |c| c.rate_limited += 1);
                continue;
            }
            // Charged to the flood guard above, then refused: only to-server types run here. A
            // correct client never sends a server-only type, so it is a protocol error that names
            // the sender (spec 120 § 11.2, ADR-7984), never a silent count.
            if t.direction != Direction::ToServer {
                count(world, m.type_index, |c| c.rejected += 1);
                world
                    .resource_mut::<NetNode>()
                    .reject(peer, RejectKind::ServerOnlyMessage, t.name);
                kick = Some((DisconnectReason::ProtocolError, "server-only message type"));
                break;
            }
            // A client cannot claim a tick the server has not reached.
            let sent_tick = sent_tick.min(tick);
            // Target and ownership at the processing tick (spec 70 § 6.5 item 5).
            let target = match m.target {
                Some(id) => match world.resource::<NetEntities>().entity(id) {
                    Some(e) => {
                        if owners.owner(world, e) != peer {
                            count(world, m.type_index, |c| c.not_owner += 1);
                            continue;
                        }
                        Some(e)
                    }
                    None => {
                        count(world, m.type_index, |c| c.unresolved += 1);
                        continue;
                    }
                },
                None => None,
            };
            // Sequenced dedup after the target resolved and its owner passed: a key is only
            // stored for a target this peer owns, so wire-chosen ids cannot grow the map
            // (audit-v035-net M1).
            if t.channel == Channel::UnreliableSequenced && channel == Channel::Unreliable {
                let key = (peer, m.type_index, m.target);
                let mut node = world.resource_mut::<NetNode>();
                if node
                    .newest_seq_in
                    .get(&key)
                    .is_some_and(|n| seq_not_newer(m.seq, *n))
                {
                    continue;
                }
                node.newest_seq_in.insert(key, m.seq);
            }
            let map = world.resource::<NetEntities>().clone();
            let asset_cx = message_asset_context(world);
            // A client chose these bytes: it may name what the server holds, not make it load.
            let cx = CodecContext::new(&map)
                .with_assets(asset_cx.as_deref())
                .without_loading();
            let mut r = BitReader::new(&m.payload);
            let decoded = (t.decode)(&mut r, &cx);
            record_message_asset_misses(world, &cx, t.name);
            let Ok(msg) = decoded else {
                kick = Some((DisconnectReason::MalformedPacket, "message"));
                break;
            };
            let verdict = (t.validate)(
                msg.as_ref(),
                &ValidateContext {
                    from: peer,
                    target,
                    world,
                    tick,
                },
            );
            match verdict {
                Verdict::Accept => {
                    count(world, m.type_index, |c| c.delivered += 1);
                    let mut node = world.resource_mut::<NetNode>();
                    if node.record {
                        node.delivered_log.push((tick, m.type_index, peer, m.target));
                    }
                    (t.deliver)(world, msg, target, peer, sent_tick, false);
                }
                Verdict::Reject(reason) => {
                    count(world, m.type_index, |c| c.rejected += 1);
                    // At most 10 lines per peer per second (spec 70 § 6.4, ADR-5647).
                    let mut node = world.resource_mut::<NetNode>();
                    let log = node
                        .peers
                        .get_mut(&peer)
                        .is_some_and(|link| link.log_budget(tick));
                    if log {
                        node.capped_logs.0 += 1;
                        tracing::debug!(target: "net", peer = peer.0, reason, "net message rejected");
                    }
                }
                Verdict::Kick(reason) => {
                    count(world, m.type_index, |c| c.rejected += 1);
                    kick = Some((DisconnectReason::InvalidMessage, reason));
                }
            }
        }
        let outcome = {
            let mut node = world.resource_mut::<NetNode>();
            match node.peers.get_mut(&peer) {
                Some(link) => link.flood.end_tick(tick, &settings),
                None => FloodOutcome::Steady,
            }
        };
        match outcome {
            FloodOutcome::BecameSlowed => {
                tracing::info!(target: "net", peer = peer.0, "net peer flood state slowed");
            }
            FloodOutcome::BecameClear => {
                tracing::info!(target: "net", peer = peer.0, "net peer flood state clear");
            }
            FloodOutcome::Disconnect => {
                kick = kick.or(Some((DisconnectReason::Flood, "flood")));
            }
            FloodOutcome::Steady => {}
        }
        if let Some((reason, detail)) = kick {
            server_kick(world, peer, reason, detail);
            // A kick may hand the peer's entities back to the server: resolve again.
            owners.clear();
        }
    }
    owners.clear();
    world.insert_resource(owners);
}

fn client_apply(world: &mut World, registry: &NetMessageRegistry) {
    replication::client_check_lost_copies(world);
    replication::client_mark_unbound(world);
    let local = std::mem::take(&mut world.resource_mut::<NetNode>().local);
    for p in local {
        let _ = deliver(world, registry, &p.msg, p.from, p.sent_tick, false);
    }
    let frames = {
        let mut node = world.resource_mut::<NetNode>();
        node.pending_frame_bytes = 0;
        std::mem::take(&mut node.pending_frames)
    };
    let mut states = std::mem::take(&mut world.resource_mut::<NetNode>().pending_states);
    states.sort_by_key(|(tick, seq, _)| (*tick, *seq));
    let mut states = states.into_iter().peekable();
    let apply_state = |world: &mut World, tick: NetTick, bytes: &[u8]| -> bool {
        if replication::client_apply_state_frame(world, bytes).is_err() {
            tracing::warn!(target: "net", tick = tick.0, "net: malformed state frame");
            world.resource_mut::<NetNode>().closed = Some(DisconnectReason::ProtocolError);
            return false;
        }
        let mut node = world.resource_mut::<NetNode>();
        node.tick = node.tick.max(tick);
        true
    };
    // Prediction reconciles against state: on the state channel a tick frame carries none.
    let on_state_channel = world
        .get_resource::<ClientReplication>()
        .is_some_and(|c| c.state.active());
    let has_frames = states.peek().is_some() || (!on_state_channel && !frames.is_empty());
    crate::predict::client_before_apply(world, has_frames);
    for (tick, bytes) in frames {
        let tick = NetTick(tick);
        // A frame is checked whole before it touches the world: a corrupt one changes nothing.
        if validate_tick_frame(&bytes).is_err() {
            tracing::warn!(target: "net", tick = tick.0, "net: malformed tick frame");
            world.resource_mut::<NetNode>().closed = Some(DisconnectReason::ProtocolError);
            return;
        }
        // State of a tick goes before its messages and despawns (spec 70 § 6.5 item 3).
        while let Some((t, _, state)) = states.next_if(|(t, _, _)| *t <= tick) {
            if !apply_state(world, t, &state) {
                return;
            }
        }
        let mut r = BitReader::new(&bytes);
        let result = (|| -> Result<(), DecodeError> {
            let _tag = r.read_bits(8)?;
            let _tick = read_tick(&mut r)?;
            let gone = replication::client_apply_state(world, &mut r, tick)?;
            let mut node = world.resource_mut::<NetNode>();
            node.tick = node.tick.max(tick);
            // The tick's reliable messages, then its unreliable ones that already arrived.
            let msgs = read_messages(&mut r, false)?;
            for m in msgs {
                deliver_client(world, registry, m, tick, false);
            }
            run_pending(world, registry);
            replication::client_apply_despawns(world, &gone, tick);
            Ok(())
        })();
        if result.is_err() {
            tracing::warn!(target: "net", tick = tick.0, "net: malformed tick frame");
            world.resource_mut::<NetNode>().closed = Some(DisconnectReason::ProtocolError);
            return;
        }
    }
    for (tick, _, state) in states {
        if !apply_state(world, tick, &state) {
            return;
        }
    }
    crate::predict::client_after_apply(world, has_frames);
    run_pending(world, registry);
}

/// Walks a `TICK` frame without applying it.
fn validate_tick_frame(bytes: &[u8]) -> Result<(), DecodeError> {
    let mut r = BitReader::new(bytes);
    let _tag = r.read_bits(8)?;
    let _tick = read_tick(&mut r)?;
    replication::validate_state_section(&mut r)?;
    read_messages(&mut r, false)?;
    Ok(())
}

/// Walks a frame a server sends a stream client, without applying it (spec 103: a recorded frame is checked before the
/// client path sees it).
pub(crate) fn validate_server_frame(bytes: &[u8]) -> Result<(), DecodeError> {
    let mut r = BitReader::new(bytes);
    match r.read_bits(8)? as u8 {
        TAG_TICK => validate_tick_frame(bytes),
        TAG_UMSG => {
            read_tick(&mut r)?;
            read_messages(&mut r, true)?;
            Ok(())
        }
        TAG_RUMSG => {
            read_tick(&mut r)?;
            read_messages(&mut r, false)?;
            Ok(())
        }
        _ => Err(DecodeError::Invalid("frame tag")),
    }
}

/// The same `TICK` frame with an empty message list. `None` for a frame that is not a `TICK` frame.
pub(crate) fn state_only_tick_frame(bytes: &[u8]) -> Result<Option<Vec<u8>>, DecodeError> {
    let mut r = BitReader::new(bytes);
    if r.read_bits(8)? as u8 != TAG_TICK {
        return Ok(None);
    }
    read_tick(&mut r)?;
    replication::validate_state_section(&mut r)?;
    let section_end = r.bit_pos();
    let mut w = BitWriter::with_capacity(section_end.div_ceil(8) + 1);
    w.append_slice(bytes, section_end);
    w.write_varint(0);
    Ok(Some(w.finish()))
}

/// `state`'s replication section with `messages`' message list: the first recorded tick is a keyframe plus that
/// tick's messages (spec 103 § 8).
pub(crate) fn tick_frame_with_messages_of(state: &[u8], messages: &[u8]) -> Result<Vec<u8>, DecodeError> {
    let mut r = BitReader::new(state);
    r.read_bits(8)?;
    read_tick(&mut r)?;
    replication::validate_state_section(&mut r)?;
    let state_end = r.bit_pos();
    let mut r = BitReader::new(messages);
    r.read_bits(8)?;
    read_tick(&mut r)?;
    replication::validate_state_section(&mut r)?;
    let msgs = read_messages(&mut r, false)?;
    let mut w = BitWriter::with_capacity(state.len() + messages.len());
    w.append_slice(state, state_end);
    write_messages(&mut w, &msgs, false);
    Ok(w.finish())
}

/// Delivers a server message on a client, or keeps it pending while its target is unresolved.
fn deliver_client(
    world: &mut World,
    registry: &NetMessageRegistry,
    m: WireMessage,
    sent_tick: NetTick,
    sequenced: bool,
) {
    let len = m.payload.len() as u64;
    count(world, m.type_index, |c| {
        c.received += 1;
        c.bytes_received += len;
    });
    let now = world.resource::<NetNode>().local_ticks;
    let mut node = world.resource_mut::<NetNode>();
    if node.pending_is_full(m.payload.len()) {
        let type_index = m.type_index;
        count(world, type_index, |c| c.dropped_overflow += 1);
        return;
    }
    node.pending_msg_bytes += m.payload.len();
    node.pending_msgs.push(PendingMessage {
        msg: m,
        from: PeerId::SERVER,
        sent_tick,
        first_seen: now,
        sequenced,
    });
    let _ = registry;
}

/// Runs pending client messages whose tick is applied and whose target exists; drops the ones
/// that waited too long.
fn run_pending(world: &mut World, registry: &NetMessageRegistry) {
    let applied = world.resource::<NetNode>().tick;
    let local = world.resource::<NetNode>().local_ticks;
    let wait = u64::from(world.resource::<NetSettings>().messages.unresolved_wait_ticks);
    // An `ALIGN_TO_VIEW` hold lasts at most spec 74's `max_delay_ms` (spec 70 § 6.5 item 3,
    // ADR-5647), so a stalled interpolation tick cannot hold a message forever.
    let max_hold = (world
        .resource::<NetSettings>()
        .interpolation
        .max_delay_ms
        .max(0.0)
        / 1000.0
        * tick_hz(world))
    .ceil() as u64;
    let interp = world.get_resource::<NetInterpTick>().map(|t| t.0);
    let pending = std::mem::take(&mut world.resource_mut::<NetNode>().pending_msgs);
    let mut keep = Vec::new();
    let mut ready: Vec<PendingMessage> = Vec::new();
    for p in pending {
        let Some(t) = registry.get(p.msg.type_index) else {
            continue;
        };
        // Everything pending came from the server: a to-server type has no business here.
        if t.direction == Direction::ToServer {
            count(world, p.msg.type_index, |c| c.rejected += 1);
            continue;
        }
        let target_known = p
            .msg
            .target
            .is_none_or(|id| world.resource::<NetEntities>().entity(id).is_some());
        // A message stamped with a tick that never comes does not wait forever. Ticks are late
        // under loss (the state stream is reliable), so this bound is the longer one.
        let waited = local.saturating_sub(p.first_seen);
        // An unordered message waits for its target only, never for its tick (ADR-10052 (3)).
        let tick_wait = !p.msg.unordered && p.sent_tick > applied;
        if (!target_known && waited >= wait) || (tick_wait && waited >= wait.saturating_mul(8)) {
            count(world, p.msg.type_index, |c| c.unresolved += 1);
            continue;
        }
        if tick_wait || !target_known {
            keep.push(p);
            continue;
        }
        if t.align_to_view
            && let Some(view) = interp
            && view < p.sent_tick
            && waited < max_hold
        {
            keep.push(p);
            continue;
        }
        ready.push(p);
    }
    // Sent order: tick, then the order received.
    ready.sort_by_key(|p| p.sent_tick);
    for p in ready {
        if p.sequenced
            && let Some(t) = registry.get(p.msg.type_index)
            && t.channel == Channel::UnreliableSequenced
        {
            let key = (p.msg.type_index, p.msg.target);
            let mut node = world.resource_mut::<NetNode>();
            let newest = node.newest_seq.get(&key).copied();
            if newest.is_some_and(|n| seq_not_newer(p.msg.seq, n)) {
                continue;
            }
            node.newest_seq.insert(key, p.msg.seq);
        }
        let late = p.sent_tick < applied;
        if deliver(world, registry, &p.msg, p.from, p.sent_tick, late).is_err() {
            tracing::warn!(target: "net", "net: undecodable message from the server");
        }
    }
    let mut node = world.resource_mut::<NetNode>();
    node.pending_msgs.extend(keep);
    node.pending_msg_bytes = node.pending_msgs.iter().map(|p| p.msg.payload.len()).sum();
}

/// Whether `seq` is the same as or older than `newest`, with wrap-around (serial-number arithmetic).
fn seq_not_newer(seq: u32, newest: u32) -> bool {
    (seq.wrapping_sub(newest) as i32) <= 0
}

/// Client: the interpolation tick of spec 74 (proxies are shown at it). `ALIGN_TO_VIEW` messages
/// wait until it reaches their tick; without it they run with the state.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NetInterpTick(pub NetTick);

// ---- NetSet::Send --------------------------------------------------------------------------------

/// `NetSet::Assign` + `NetSet::Gather` + `NetSet::Send` (end of each fixed tick): routes this
/// tick's messages, gathers replication (server) and writes the frames into the outbox.
pub fn net_send(world: &mut World) {
    let _span = tracing::trace_span!("net_messages_send").entered();
    ensure_resources(world);
    // A handle loaded earlier in this frame travels by its guid in this tick's frame.
    crate::assets::register_asset_server_handles(world);
    if mode(world).is_server() {
        // The interruption marker follows the current owner: settled before the gather reads it.
        crate::model::reconcile_interrupted(world);
        let tick = world.resource::<NetNode>().tick;
        server_send(world);
        crate::recorder::after_send(world, tick);
    } else {
        client_send(world);
    }
}

fn server_send(world: &mut World) {
    apply_pending_kicks(world);
    let tick = world.resource::<NetNode>().tick;
    // Packets are assembled on send ticks (spec 72 § 3); messages queue for the next one.
    let every = world
        .resource::<NetSettings>()
        .replication
        .send_every_ticks
        .max(1);
    let send_tick = tick.0.is_multiple_of(every);
    let mut sections = if send_tick {
        crate::recorder::before_gather(world, tick);
        let mut sections = replication::server_gather(world, tick);
        crate::recorder::take_keyframe(world, tick, &mut sections);
        sections
    } else {
        replication::server_assign(world, tick);
        BTreeMap::new()
    };
    let out = std::mem::take(&mut world.resource_mut::<NetNode>().out_queue);
    let mut owners = world.remove_resource::<OwnerCache>().unwrap_or_default();
    // One walk per target entity in this pass, never one per message (spec 70 § 11, ADR-5647).
    owners.clear();
    // The second of this tick, for the once-per-second broadcast-overflow warning.
    let second = tick.0 / (tick_hz(world).round() as u32).max(1);
    let peers = world.resource::<NetNode>().links();
    world.resource_mut::<NetNode>().broadcasts_this_tick = 0;
    // One buffer for every message's recipients (spec 120 § 8.4: no per-message allocation).
    let mut targets: Vec<PeerId> = Vec::new();
    for m in out {
        let wire = |seq| WireMessage {
            type_index: m.type_index,
            target: m.target,
            seq,
            payload: m.payload.clone(),
            unordered: unordered_class(m.channel, m.payload.len()),
        };
        count(world, m.type_index, |c| c.sent += 1);
        targets.clear();
        let mut local = false;
        match m.to {
            Recipient::Server => local = true,
            Recipient::Peer(p) => {
                if p == PeerId::SERVER {
                    local = true;
                } else {
                    targets.push(p);
                }
            }
            Recipient::OwnerOf(id) => {
                let owner = world
                    .resource::<NetEntities>()
                    .entity(id)
                    .map(|e| owners.owner(world, e))
                    .unwrap_or(PeerId::SERVER);
                if owner == PeerId::SERVER {
                    local = true;
                } else {
                    targets.push(owner);
                }
            }
            Recipient::All => {
                let mut node = world.resource_mut::<NetNode>();
                node.broadcasts_this_tick += 1;
                if node.broadcasts_this_tick > MAX_BROADCASTS_PER_TICK {
                    // Warn once per second (spec 70 § 6.5 queue bounds, ADR-5647).
                    if node.broadcast_warned_second != Some(second) {
                        node.broadcast_warned_second = Some(second);
                        node.capped_logs.1 += 1;
                        tracing::warn!(
                            target: "net",
                            max = MAX_BROADCASTS_PER_TICK,
                            "net broadcasts over the per-tick cap were dropped"
                        );
                    }
                    count(world, m.type_index, |c| c.dropped_overflow += 1);
                    continue;
                }
                local = true;
                let skip_owner = world
                    .resource::<NetMessageRegistry>()
                    .get(m.type_index)
                    .is_some_and(|t| t.skip_predicting_owner);
                let entity = m.target.and_then(|id| world.resource::<NetEntities>().entity(id));
                // The target's slot, looked up once: each peer then checks its own copy, so a
                // targeted broadcast costs one lookup per peer, not a holder list per message
                // searched once per peer (spec 120 § 8.4).
                let holder_slot: Option<Option<u32>> = m
                    .target
                    .map(|id| world.resource::<ServerReplication>().slot_of(id));
                let predicting_owner = match (skip_owner, entity) {
                    (true, Some(e)) if world.get::<NetPredicted>(e).is_some() => Some(owners.owner(world, e)),
                    _ => None,
                };
                let rep = world.resource::<ServerReplication>();
                for p in &peers {
                    if holder_slot.is_some_and(|slot| !rep.holds_slot(*p, slot)) {
                        continue;
                    }
                    if predicting_owner == Some(*p) {
                        continue;
                    }
                    targets.push(*p);
                }
            }
        }
        if local {
            let mut node = world.resource_mut::<NetNode>();
            node.local.push(PendingMessage {
                msg: wire(0),
                from: PeerId::SERVER,
                sent_tick: tick,
                first_seen: 0,
                sequenced: false,
            });
        }
        let mut overflow: Vec<PeerId> = Vec::new();
        for p in targets.drain(..) {
            if !world.resource::<NetNode>().peers.contains_key(&p) {
                continue;
            }
            let len = m.payload.len() as u64;
            let on_recorder = crate::recorder::is_virtual_peer(p);
            if !on_recorder {
                count(world, m.type_index, |c| c.bytes_sent += len);
            }
            let mut node = world.resource_mut::<NetNode>();
            let seq = if on_recorder {
                node.virtual_seq = node.virtual_seq.wrapping_add(1);
                node.virtual_seq
            } else {
                node.send_seq = node.send_seq.wrapping_add(1);
                node.send_seq
            };
            let Some(link) = node.peers.get_mut(&p) else {
                continue;
            };
            if m.channel.is_reliable() {
                link.queued_bytes += m.payload.len();
                link.reliable_out.push(wire(seq));
                if !on_recorder
                    && link.dormant.is_none()
                    && (link.reliable_out.len() > MAX_RELIABLE_QUEUE
                        || link.queued_bytes > MAX_RELIABLE_QUEUE_BYTES)
                {
                    overflow.push(p);
                }
            } else {
                link.unreliable_out.push(wire(seq));
                let over = m.target.is_some_and(|id| {
                    let queued = link.unreliable_targets.entry(id).or_insert(0);
                    *queued += 1;
                    *queued > MAX_UNRELIABLE_PER_TARGET
                });
                if over {
                    // The oldest message to this target goes (the scan runs only on overflow).
                    if let Some(first) = link.unreliable_out.iter().position(|w| w.target == m.target) {
                        link.unreliable_out.remove(first);
                    }
                    if let Some(queued) = m.target.and_then(|id| link.unreliable_targets.get_mut(&id)) {
                        *queued -= 1;
                    }
                    count(world, m.type_index, |c| c.dropped_overflow += 1);
                }
            }
        }
        for p in overflow {
            server_kick(world, p, DisconnectReason::ReliableOverflow, "reliable queue");
            // A kick may hand the peer's entities back to the server: resolve again.
            owners.clear();
        }
    }
    owners.clear();
    world.insert_resource(owners);
    if !send_tick {
        world.resource_mut::<NetNode>().tick = tick.next();
        return;
    }

    // Frames.
    let peers = world.resource::<NetNode>().links();
    // Message bytes per peer, charged to the next tick's send budget (spec 72 § 7 item 3).
    let mut message_bytes: Vec<(PeerId, u32)> = Vec::new();
    let mut node = world.resource_mut::<NetNode>();
    for p in peers {
        let Some(link) = node.peers.get_mut(&p) else {
            continue;
        };
        if let Some(held) = link.dormant.as_mut() {
            // Spec 120 § 10.3: no frames for a dormant peer; its reliable messages wait.
            held.hold(tick, std::mem::take(&mut link.reliable_out));
            link.unreliable_out.clear();
            link.queued_bytes = 0;
            continue;
        }
        let (unordered, reliable): (Vec<WireMessage>, Vec<WireMessage>) =
            std::mem::take(&mut link.reliable_out)
                .into_iter()
                .partition(|m| m.unordered);
        let unreliable = std::mem::take(&mut link.unreliable_out);
        link.unreliable_targets.clear();
        link.queued_bytes = 0;
        let mut w = BitWriter::new();
        w.write_bits(u64::from(TAG_TICK), 8);
        w.write_varint(u64::from(tick.0));
        let gathered = sections.remove(&p);
        if let Some(g) = gathered.as_ref() {
            w.append(&g.section);
        } else {
            // An empty replication section: no ownership changes, spawns, updates, despawns or
            // removed level entities.
            for _ in 0..5 {
                w.write_varint(0);
            }
        }
        let before = w.bit_len();
        write_messages(&mut w, &reliable, false);
        let mut msg_bits = w.bit_len() - before;
        // State frames first: a tick's state goes before its messages.
        let on_state_channel = gathered.as_ref().is_some_and(|g| g.wire == ConnWire::Acked);
        let quiet =
            on_state_channel && reliable.is_empty() && gathered.as_ref().is_some_and(|g| g.section_empty);
        if let Some(g) = gathered {
            for f in g.packets {
                node.outbox.push(Datagram {
                    peer: p,
                    channel: f.channel,
                    bytes: f.bytes,
                });
            }
        }
        if !quiet {
            node.outbox.push(Datagram {
                peer: p,
                channel: Channel::ReliableOrdered,
                bytes: w.finish(),
            });
        }
        // ReliableUnordered messages ride their own frames on the unordered class (wire 6,
        // ADR-10052 (3)): a stalled replication stream never holds them back.
        for bytes in rumsg_frames(tick, &unordered) {
            msg_bits += bytes.len() * 8;
            node.outbox.push(Datagram {
                peer: p,
                channel: Channel::ReliableUnordered,
                bytes,
            });
        }
        if !unreliable.is_empty() {
            let mut w = BitWriter::new();
            w.write_bits(u64::from(TAG_UMSG), 8);
            w.write_varint(u64::from(tick.0));
            write_messages(&mut w, &unreliable, true);
            msg_bits += w.bit_len();
            node.outbox.push(Datagram {
                peer: p,
                channel: Channel::Unreliable,
                bytes: w.finish(),
            });
        }
        message_bytes.push((p, msg_bits.div_ceil(8) as u32));
    }
    node.tick = tick.next();
    if node.tick.0 == u32::MAX {
        // `NetTick` does not wrap: the session ends here rather than repeating one tick number.
        tracing::error!(target: "net", "net: the tick counter ran out, ending the session");
        let peers = node.peers();
        node.disconnects
            .extend(peers.into_iter().map(|peer| NetDisconnect {
                peer,
                reason: DisconnectReason::ServerShutdown,
                detail: "tick counter exhausted",
            }));
    }
    let hz = tick_hz(world);
    if let Some(mut interest) = world.get_resource_mut::<crate::relevance::NetInterest>() {
        let second = (f64::from(tick.0) / hz) as u64;
        for (p, bytes) in message_bytes {
            if let Some(pi) = interest.peers.get_mut(&p) {
                pi.send.message_debt = pi.send.message_debt.saturating_add(bytes);
                pi.send.bandwidth.total.add(second, u64::from(bytes));
            }
        }
    }
    // Input acknowledgments (spec 73 § 4), unreliable: the newest one wins.
    let acks = crate::predict::server_ack_frames(world, tick);
    let mut node = world.resource_mut::<NetNode>();
    for (peer, bytes) in acks {
        if node.peers.contains_key(&peer) {
            node.outbox.push(Datagram {
                peer,
                channel: Channel::Unreliable,
                bytes,
            });
        }
    }
}

/// Cuts a tick's messages into frames of at most [`MAX_CLIENT_FRAME_PAYLOAD`] (one frame when there
/// are none, so a resync list still goes out).
fn client_batches(msgs: &[WireMessage]) -> Vec<&[WireMessage]> {
    let mut batches = Vec::new();
    let (mut start, mut bytes) = (0, 0);
    for (i, m) in msgs.iter().enumerate() {
        if i > start && bytes + m.payload.len() > MAX_CLIENT_FRAME_PAYLOAD {
            batches.push(&msgs[start..i]);
            (start, bytes) = (i, 0);
        }
        bytes += m.payload.len();
    }
    batches.push(&msgs[start..]);
    batches
}

fn client_send(world: &mut World) {
    let out = std::mem::take(&mut world.resource_mut::<NetNode>().out_queue);
    let tick = world.resource::<NetNode>().tick;
    let mut unordered_out = Vec::new();
    let mut reliable = Vec::new();
    let mut unreliable = Vec::new();
    for m in out {
        let len = m.payload.len() as u64;
        count(world, m.type_index, |c| {
            c.sent += 1;
            c.bytes_sent += len;
        });
        let mut node = world.resource_mut::<NetNode>();
        node.send_seq = node.send_seq.wrapping_add(1);
        let unordered = unordered_class(m.channel, m.payload.len());
        let w = WireMessage {
            type_index: m.type_index,
            target: m.target,
            seq: node.send_seq,
            payload: m.payload,
            unordered,
        };
        match m.to {
            Recipient::Server => {
                if unordered {
                    unordered_out.push(w);
                } else if m.channel.is_reliable() {
                    reliable.push(w);
                } else {
                    unreliable.push(w);
                }
            }
            // A client broadcast is local and cosmetic: it runs here only.
            _ => node.local.push(PendingMessage {
                msg: w,
                from: PeerId::SERVER,
                sent_tick: tick,
                first_seen: 0,
                sequenced: false,
            }),
        }
    }
    // Longer lists go out over the next frames, at the rate the server takes them.
    let resync: Vec<NetId> = {
        let mut client = world.resource_mut::<ClientReplication>();
        let ids: Vec<NetId> = client
            .resync
            .iter()
            .copied()
            .take(replication::MAX_RESYNC_PER_TICK)
            .collect();
        for id in &ids {
            client.resync.remove(id);
            client.state.expect_full(*id);
        }
        ids
    };
    let mut node = world.resource_mut::<NetNode>();
    if !reliable.is_empty() || !resync.is_empty() {
        for (i, batch) in client_batches(&reliable).into_iter().enumerate() {
            let mut w = BitWriter::new();
            w.write_bits(u64::from(TAG_CMSG), 8);
            w.write_varint(u64::from(tick.0));
            write_messages(&mut w, batch, false);
            let ids: &[NetId] = if i == 0 { &resync } else { &[] };
            w.write_varint(ids.len() as u64);
            for id in ids {
                w.write_varint(u64::from(id.0));
            }
            node.outbox.push(Datagram {
                peer: PeerId::SERVER,
                channel: Channel::ReliableOrdered,
                bytes: w.finish(),
            });
        }
    }
    // ReliableUnordered messages: their own frames on the unordered class (ADR-10052 (3)).
    for bytes in rumsg_frames(tick, &unordered_out) {
        node.outbox.push(Datagram {
            peer: PeerId::SERVER,
            channel: Channel::ReliableUnordered,
            bytes,
        });
    }
    if !unreliable.is_empty() {
        for batch in client_batches(&unreliable) {
            let mut w = BitWriter::new();
            w.write_bits(u64::from(TAG_UMSG), 8);
            w.write_varint(u64::from(tick.0));
            write_messages(&mut w, batch, true);
            node.outbox.push(Datagram {
                peer: PeerId::SERVER,
                channel: Channel::Unreliable,
                bytes: w.finish(),
            });
        }
    }
    // The input stream (spec 73 § 4).
    if let Some(bytes) = crate::predict::client_input_frame(world) {
        world.resource_mut::<NetNode>().outbox.push(Datagram {
            peer: PeerId::SERVER,
            channel: Channel::Unreliable,
            bytes,
        });
    }
    // Acknowledge only ticks actually applied.
    if world.resource::<ClientReplication>().applied.is_some() {
        let mut w = BitWriter::new();
        w.write_bits(u64::from(TAG_ACK), 8);
        w.write_varint(u64::from(tick.0));
        // After the tick, where a protocol 1 server stops reading (spec 72 § 4, ADR-1950).
        world
            .resource::<ClientReplication>()
            .state
            .acks
            .snapshot()
            .write(&mut w);
        world.resource_mut::<NetNode>().outbox.push(Datagram {
            peer: PeerId::SERVER,
            channel: Channel::Unreliable,
            bytes: w.finish(),
        });
    }
}

/// Runs one whole fixed tick on a world with no app: `NetSet::Apply`, the given gameplay schedule,
/// then `NetSet::Send` (tests, tools and headless servers that drive the world themselves).
pub fn run_net_tick(world: &mut World, gameplay: &mut Schedule) {
    net_receive(world);
    net_apply(world);
    gameplay.run(world);
    crate::predict::net_predict(world);
    net_send(world);
}

#[cfg(test)]
mod replay_tests {
    use super::*;

    /// Spec 120 § 10.3: a dormant peer's held reliable messages come back on resume unless they
    /// are older than the grace, and the held queue drops its oldest instead of kicking.
    #[test]
    fn held_messages_keep_the_young_and_drop_the_old() {
        let msg = |seq| WireMessage {
            type_index: 0,
            target: None,
            seq,
            payload: vec![0; 4],
            unordered: false,
        };
        let mut world = World::new();
        ensure_resources(&mut world);
        let peer = PeerId(4);
        server_add_peer(&mut world, peer);
        world.resource_mut::<NetNode>().tick = NetTick(100);
        world
            .resource_mut::<NetNode>()
            .peers
            .get_mut(&peer)
            .expect("peer")
            .reliable_out
            .push(msg(1));
        assert!(node_hold_peer(&mut world, peer));
        assert!(server_peer_is_dormant(&world, peer));
        // Queued later while dormant, at tick 150.
        if let Some(held) = world
            .resource_mut::<NetNode>()
            .peers
            .get_mut(&peer)
            .and_then(|l| l.dormant.as_mut())
        {
            held.hold(NetTick(150), vec![msg(2)]);
        }
        // Back at tick 200 with a 60-tick grace: the tick-100 message is too old.
        world.resource_mut::<NetNode>().tick = NetTick(200);
        let out = node_resume_peer(&mut world, peer, 60);
        assert_eq!((out.was_dormant, out.kept, out.expired), (true, 1, 1));
        assert!(!server_peer_is_dormant(&world, peer));
        let link = &world.resource::<NetNode>().peers[&peer];
        assert_eq!(
            link.reliable_out.iter().map(|m| m.seq).collect::<Vec<_>>(),
            vec![2]
        );
        // The bound: past MAX_RELIABLE_QUEUE the oldest go, counted.
        let mut held = HeldMessages::default();
        held.hold(
            NetTick(1),
            (0..(MAX_RELIABLE_QUEUE as u32 + 5)).map(msg).collect(),
        );
        assert_eq!(held.messages.len(), MAX_RELIABLE_QUEUE);
        assert_eq!(held.dropped, 5);
        assert_eq!(held.messages.front().map(|(_, m)| m.seq), Some(5));
    }

    #[test]
    fn a_sequence_that_left_the_window_stays_refused() {
        let mut seen = SeenSeqs::default();
        let peer = PeerId(1);
        assert!(seen.insert(peer, 1));
        assert!(!seen.insert(peer, 1));
        for seq in 2..=(SEEN_WINDOW as u32 + 2) {
            assert!(seen.insert(peer, seq));
        }
        assert!(!seen.insert(peer, 1), "replayed after leaving the window");
        assert!(!seen.insert(peer, 2), "the newest one to leave");
        assert!(seen.insert(peer, SEEN_WINDOW as u32 + 3));
        // Another sender has its own floor.
        assert!(seen.insert(PeerId(2), 1));
    }

    /// UDP can deliver an honest sender's monotonic `send_seq` out of order. The floor is the
    /// newest sequence that fell out of the window behind the true newest, not whichever
    /// sequence happened to be inserted first. 8500 is 693 behind 9193, inside `SEEN_WINDOW`.
    #[test]
    fn audit_net2_reordered_sequence_inside_the_window_is_accepted() {
        let mut seen = SeenSeqs::default();
        let peer = PeerId(1);
        assert!(seen.insert(peer, 9000));
        for seq in 1000..=9192 {
            if seq != 8500 && seq != 9000 {
                assert!(seen.insert(peer, seq), "seq {seq}");
            }
        }
        assert!(seen.insert(peer, 9193));
        assert!(
            seen.insert(peer, 8500),
            "8500 was never seen and is inside the window of newest 9193"
        );
        assert!(
            !seen.insert(peer, 1000),
            "the serial-oldest sequence that left the window stays refused"
        );
    }

    /// One datagram whose sequence is far ahead of the sender's real counter must not become
    /// the floor. After it leaves the window, sequences at the real counter are still new.
    #[test]
    fn audit_net2_far_ahead_sequence_does_not_raise_the_floor() {
        let mut seen = SeenSeqs::default();
        let peer = PeerId(1);
        assert!(seen.insert(peer, 2_000_000_000));
        for seq in 0..=8192 {
            assert!(
                seen.insert(peer, seq),
                "seq {seq} is still inside the window after one far-ahead datagram"
            );
        }
    }

    /// A sequence exactly half the `u32` space ahead of the live counter is not a newer
    /// `send_seq` (`seq_not_newer`). The numeric pre-wrap scan used to evict it first and store
    /// it as `floor`, after which every later honest sequence failed `seq_not_newer` and the
    /// sender's unreliable channel stayed muted. The same mute happens one step past the half
    /// (`2^31 + 1`), which is still not a safe eviction.
    #[test]
    fn a_half_range_sequence_does_not_raise_the_floor() {
        for step in [0u32, 1] {
            let mut seen = SeenSeqs::default();
            let peer = PeerId(1);
            for seq in 0..SEEN_WINDOW as u32 {
                assert!(seen.insert(peer, seq), "{seq}");
            }
            let newest = SEEN_WINDOW as u32 - 1;
            let poison = newest.wrapping_add(1u32 << 31).wrapping_add(step);
            let _accepted = seen.insert(peer, poison);
            let live = SEEN_WINDOW as u32;
            assert!(
                seen.insert(peer, live),
                "half-range + {step} must not become the floor over the live counter"
            );
            assert!(
                !seen.insert(peer, live.wrapping_sub(10)),
                "a sequence still inside the window stays a duplicate"
            );
        }
    }

    /// The same poison, accepted while the window still has room, must not become the floor
    /// once the window fills and evicts it.
    #[test]
    fn an_early_half_range_sequence_does_not_raise_the_floor() {
        for step in [0u32, 1] {
            let mut seen = SeenSeqs::default();
            let peer = PeerId(1);
            for seq in 0..=100 {
                assert!(seen.insert(peer, seq), "{seq}");
            }
            let poison = 100u32.wrapping_add(1u32 << 31).wrapping_add(step);
            let _accepted = seen.insert(peer, poison);
            for seq in 101..=(SEEN_WINDOW as u32 + 100) {
                assert!(
                    seen.insert(peer, seq),
                    "seq {seq} after an early half-range + {step} datagram"
                );
            }
        }
    }

    #[test]
    fn an_ack_past_the_server_tick_is_ignored() {
        let peer = PeerId(3);
        let mut world = World::new();
        world.insert_resource(NetNode {
            tick: NetTick(5),
            ..NetNode::default()
        });
        let mut replication = ServerReplication::default();
        replication.add_conn(peer);
        world.insert_resource(replication);
        let frame = |tick: u32| {
            let mut w = BitWriter::new();
            w.write_bits(u64::from(TAG_ACK), 8);
            w.write_varint(u64::from(tick));
            Datagram {
                peer,
                channel: Channel::Unreliable,
                bytes: w.finish(),
            }
        };
        assert!(server_read_frame(&mut world, &frame(u32::MAX)).is_ok());
        assert_eq!(world.resource::<ServerReplication>().acked(peer), None);
        assert!(server_read_frame(&mut world, &frame(4)).is_ok());
        assert_eq!(
            world.resource::<ServerReplication>().acked(peer),
            Some(NetTick(4))
        );
    }
}

/// audit-v035-net M1: the server stored a sequenced-dedup key for every admitted
/// `UnreliableSequenced` message before resolving its target or checking its owner, so one client
/// sending to targets it does not own (any wire-chosen id) grew the map until it disconnected.
#[cfg(test)]
mod sequenced_dedup_tests {
    use regia_ecs::ecs::system::RunSystemOnce;
    use regia_ecs::prelude::*;
    use regia_macros::Replicate;

    use super::NetNode;
    use crate::message::{Channel, Direction, NetMessage, NetSender, ToServerMessage, register_net_message};
    use crate::model::{NetId, NetMode, NetOwner};
    use crate::replication::Replicated;
    use crate::testing::LoopbackHub;

    /// A targeted, unreliable-sequenced client-to-server stream.
    #[derive(Clone, Debug, PartialEq, Replicate)]
    struct Aim {
        yaw: u32,
    }
    impl NetMessage for Aim {
        const DIRECTION: Direction = Direction::ToServer;
        const CHANNEL: Channel = Channel::UnreliableSequenced;
    }
    impl ToServerMessage for Aim {}

    fn setup(world: &mut World) {
        register_net_message::<Aim>(world);
    }

    #[test]
    fn sequenced_keys_are_kept_only_for_owned_targets() {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 11, setup);
        let a = hub.add_client();
        let b = hub.add_client();
        let (pa, pb) = (hub.clients[a].peer, hub.clients[b].peer);
        let mine = hub.server.world.spawn((Replicated::default(), NetOwner(pa))).id();
        let theirs: Vec<Entity> = (0..3)
            .map(|_| hub.server.world.spawn((Replicated::default(), NetOwner(pb))).id())
            .collect();
        hub.run(4);
        let copy = |hub: &LoopbackHub, server: Entity| {
            let id = *hub.server.world.get::<NetId>(server).expect("net id");
            hub.clients[a]
                .world
                .resource::<super::NetEntities>()
                .entity(id)
                .expect("replicated to a")
        };
        let targets: Vec<Entity> = std::iter::once(mine)
            .chain(theirs.iter().copied())
            .map(|e| copy(&hub, e))
            .collect();
        hub.clients[a]
            .world
            .run_system_once(move |mut net: NetSender| {
                for (i, target) in targets.iter().enumerate() {
                    net.send_to_server(*target, Aim { yaw: i as u32 })
                        .expect("queued");
                }
            })
            .expect("system runs");
        hub.run(4);
        let keys = hub.server.world.resource::<NetNode>().newest_seq_in.len();
        assert_eq!(keys, 1, "only the owned target keeps a dedup key, not {keys}");
    }
}

/// Spec 70 § 11 (ADR-5647): ownership is resolved once per entity per message pass, never once per
/// message. Before ADR-5647 both passes walked the owner chain for every message.
#[cfg(test)]
mod owner_cache_tests {
    use regia_ecs::ecs::system::RunSystemOnce;
    use regia_ecs::prelude::*;
    use regia_macros::Replicate;

    use super::NetEntities;
    use crate::message::{
        Direction, MessageCounters, NetMessage, NetMessageRegistry, NetSender, ToOwnerMessage,
        ToServerMessage, register_net_message,
    };
    use crate::model::{NetId, NetMode, NetOwner, owner_walks, reset_owner_walks};
    use crate::replication::Replicated;
    use crate::settings::RateLimit;
    use crate::testing::LoopbackHub;

    /// A reliable client-to-server message with room for a burst.
    #[derive(Clone, Debug, PartialEq, Replicate)]
    struct Shot {
        n: u32,
    }
    impl NetMessage for Shot {
        const DIRECTION: Direction = Direction::ToServer;
        const RATE_LIMIT: Option<RateLimit> = Some(RateLimit {
            count: 1000,
            per_ticks: 1,
        });
    }
    impl ToServerMessage for Shot {}

    /// A reliable server-to-owner message.
    #[derive(Clone, Debug, PartialEq, Replicate)]
    struct Hit {
        n: u32,
    }
    impl NetMessage for Hit {
        const DIRECTION: Direction = Direction::ToOwner;
    }
    impl ToOwnerMessage for Hit {}

    fn setup(world: &mut World) {
        register_net_message::<Shot>(world);
        register_net_message::<Hit>(world);
    }

    fn counters<M: 'static>(world: &World) -> MessageCounters {
        let reg = world.resource::<NetMessageRegistry>();
        let i = reg.index_of::<M>().expect("registered");
        reg.counters[usize::from(i)]
    }

    #[test]
    fn a_burst_to_one_entity_walks_its_owner_chain_once_per_pass() {
        const BURST: u32 = 50;
        const TICKS: usize = 4;
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 5, setup);
        let a = hub.add_client();
        let pa = hub.clients[a].peer;
        let pawn = hub.server.world.spawn((Replicated::default(), NetOwner(pa))).id();
        hub.run(TICKS);
        let id = *hub.server.world.get::<NetId>(pawn).expect("net id");
        let copy = hub.clients[a]
            .world
            .resource::<NetEntities>()
            .entity(id)
            .expect("replicated to the owner");

        // Client to server: the apply pass checks the sender owns the target.
        hub.clients[a]
            .world
            .run_system_once(move |mut net: NetSender| {
                for n in 0..BURST {
                    net.send_to_server(copy, Shot { n }).expect("queued");
                }
            })
            .expect("system runs");
        reset_owner_walks();
        hub.run(TICKS);
        let walks = owner_walks();
        assert_eq!(
            counters::<Shot>(&hub.server.world).delivered,
            u64::from(BURST),
            "the owner's messages are delivered"
        );
        assert!(
            walks <= TICKS as u64,
            "{walks} owner walks for {BURST} to-server messages over {TICKS} ticks"
        );

        // Server to owner: the send pass finds the owner of the target.
        hub.server
            .world
            .run_system_once(move |mut net: NetSender| {
                for n in 0..BURST {
                    net.send_to_owner(pawn, Hit { n }).expect("queued");
                }
            })
            .expect("system runs");
        reset_owner_walks();
        hub.run(TICKS);
        let walks = owner_walks();
        assert_eq!(
            counters::<Hit>(&hub.clients[a].world).delivered,
            u64::from(BURST),
            "every to-owner message reaches the owner"
        );
        assert!(
            walks <= TICKS as u64,
            "{walks} owner walks for {BURST} to-owner messages over {TICKS} ticks"
        );
    }
}

/// Spec 70 § 6.4 and § 6.5 (ADR-5647): the reject log is capped at 10 lines per peer per second
/// and the broadcast-overflow warning is written at most once per second; the counters still count
/// every drop.
#[cfg(test)]
mod capped_log_tests {
    use regia_ecs::ecs::system::RunSystemOnce;
    use regia_ecs::prelude::*;
    use regia_macros::Replicate;

    use super::{NetNode, tick_hz};
    use crate::message::{
        BroadcastMessage, Direction, MessageCounters, NetMessage, NetMessageRegistry, NetSender,
        ToServerMessage, ValidateContext, Verdict, register_net_message,
    };
    use crate::model::NetMode;
    use crate::settings::RateLimit;
    use crate::testing::LoopbackHub;

    /// A session message the server always rejects, with room for a burst.
    #[derive(Clone, Debug, PartialEq, Replicate)]
    struct Probe {
        n: u32,
    }
    impl NetMessage for Probe {
        const DIRECTION: Direction = Direction::ToServer;
        const RATE_LIMIT: Option<RateLimit> = Some(RateLimit {
            count: 1000,
            per_ticks: 1,
        });
        fn validate(&self, _cx: &ValidateContext) -> Verdict {
            Verdict::Reject("probe")
        }
    }
    impl ToServerMessage for Probe {}

    /// A session broadcast.
    #[derive(Clone, Debug, PartialEq, Replicate)]
    struct Flash {
        n: u32,
    }
    impl NetMessage for Flash {
        const DIRECTION: Direction = Direction::Broadcast;
    }
    impl BroadcastMessage for Flash {}

    fn setup(world: &mut World) {
        register_net_message::<Probe>(world);
        register_net_message::<Flash>(world);
    }

    fn counters<M: 'static>(world: &World) -> MessageCounters {
        let reg = world.resource::<NetMessageRegistry>();
        let i = reg.index_of::<M>().expect("registered");
        reg.counters[usize::from(i)]
    }

    fn probes(hub: &mut LoopbackHub, client: usize, n: u32) {
        hub.clients[client]
            .world
            .run_system_once(move |mut net: NetSender| {
                for k in 0..n {
                    net.send_session_to_server(Probe { n: k }).expect("queued");
                }
            })
            .expect("system runs");
    }

    fn flashes(hub: &mut LoopbackHub, n: u32) {
        hub.server
            .world
            .run_system_once(move |mut net: NetSender| {
                for k in 0..n {
                    net.broadcast_session(Flash { n: k }).expect("queued");
                }
            })
            .expect("system runs");
    }

    #[test]
    fn reject_lines_are_capped_per_peer_per_second() {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 21, setup);
        let a = hub.add_client();
        let b = hub.add_client();
        hub.run(3);
        let tps = tick_hz(&hub.server.world).round() as usize;
        // Align to the start of a second so one burst cannot straddle two.
        while !(hub.server.world.resource::<NetNode>().tick.0 as usize).is_multiple_of(tps) {
            hub.step();
        }
        probes(&mut hub, a, 30);
        probes(&mut hub, b, 3);
        hub.run(4);
        let sv = &hub.server.world;
        assert_eq!(counters::<Probe>(sv).rejected, 33, "every reject is counted");
        assert_eq!(
            sv.resource::<NetNode>().capped_logs.0,
            10 + 3,
            "10 lines for the flooding peer, 3 for the other"
        );
        // A second later the budget is back.
        hub.run(tps);
        probes(&mut hub, a, 30);
        hub.run(4);
        let sv = &hub.server.world;
        assert_eq!(counters::<Probe>(sv).rejected, 63);
        assert_eq!(sv.resource::<NetNode>().capped_logs.0, 13 + 10);
    }

    #[test]
    fn broadcast_overflow_warns_once_per_second() {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 22, setup);
        hub.run(3);
        let tps = tick_hz(&hub.server.world).round() as usize;
        while !(hub.server.world.resource::<NetNode>().tick.0 as usize).is_multiple_of(tps) {
            hub.step();
        }
        flashes(&mut hub, 1030);
        hub.step();
        flashes(&mut hub, 1030);
        hub.step();
        let sv = &hub.server.world;
        assert_eq!(
            counters::<Flash>(sv).dropped_overflow,
            12,
            "6 per tick are dropped"
        );
        assert_eq!(
            sv.resource::<NetNode>().capped_logs.1,
            1,
            "one warning in one second"
        );
        hub.run(tps);
        flashes(&mut hub, 1030);
        hub.step();
        let sv = &hub.server.world;
        assert_eq!(counters::<Flash>(sv).dropped_overflow, 18);
        assert_eq!(
            sv.resource::<NetNode>().capped_logs.1,
            2,
            "a new second warns again"
        );
    }

    /// Review item 25 (ADR-8438): the runtime's own refusals (`NetNode::reject`, voice and
    /// server-only kinds) share the per-peer budget of 10 log lines a second (spec 70 § 6.4);
    /// every refusal is still recorded and scored.
    #[test]
    fn runtime_refusals_share_the_reject_log_budget() {
        use crate::model::{NetTick, PeerId};
        use crate::server::RejectKind;
        let mut world = World::new();
        super::ensure_resources(&mut world);
        let (flooder, other) = (PeerId(3), PeerId(4));
        super::server_add_peer(&mut world, flooder);
        super::server_add_peer(&mut world, other);
        let tps = tick_hz(&world).round() as u32;
        world.resource_mut::<NetNode>().tick = NetTick(tps * 5);
        let mut node = world.resource_mut::<NetNode>();
        for _ in 0..40 {
            assert!(node.reject(flooder, RejectKind::VoiceMalformed, "voice packet"));
        }
        for _ in 0..3 {
            assert!(node.reject(other, RejectKind::VoiceMalformed, "voice packet"));
        }
        assert_eq!(
            node.capped_logs.0,
            10 + 3,
            "10 lines for the flooder, 3 for the other"
        );
        assert_eq!(node.rejection_counts(), (43, 0), "every refusal is recorded");
        // `VoiceMalformed` weighs 16 (ADR-8666; it was one point, below the kind's own weight).
        assert_eq!(node.peer_malformed_points(flooder), Some(40 * 16));
        // The next second has a fresh budget, and the score leaked 4 points over it.
        node.tick = NetTick(tps * 6);
        for _ in 0..12 {
            node.reject(flooder, RejectKind::VoiceMalformed, "voice packet");
        }
        assert_eq!(node.capped_logs.0, 13 + 10);
        assert_eq!(node.peer_malformed_points(flooder), Some(40 * 16 - 4 + 12 * 16));
    }

    /// ADR-8666 (review finding 5, ADR-8606 residual (b)): the runtime's voice score uses
    /// `RejectKind::malformed_weight` (16) and the server's leak of 4 points a second, with no
    /// credit for clean passes, so a sustained malformed voice stream is removed in about 1 s at
    /// the voice rate (60 a second: 1,024 / (60 x 16 - 4) is about 1.07 s), a refusal every 4 s
    /// never accumulates, and a burst of 63 is survived.
    #[test]
    fn voice_refusals_score_the_server_weight_and_leak_in_time() {
        use crate::model::{NetTick, PeerId};
        use crate::server::{MALFORMED_SCORE_LIMIT, RejectKind};
        let mut world = World::new();
        super::ensure_resources(&mut world);
        let peer = PeerId(3);
        super::server_add_peer(&mut world, peer);
        let tps = tick_hz(&world).round() as u32;
        let mut node = world.resource_mut::<NetNode>();
        assert!(node.reject(peer, RejectKind::VoiceMalformed, "voice packet"));
        assert_eq!(
            node.peer_malformed_points(peer),
            Some(RejectKind::VoiceMalformed.malformed_weight())
        );
        // One second of ticks later: 16 - 4 + 16.
        node.tick = NetTick(tps);
        node.reject(peer, RejectKind::VoiceMalformed, "voice packet");
        assert_eq!(node.peer_malformed_points(peer), Some(28));
        // One every 4 s never accumulates past one refusal's weight.
        for k in 2..=40 {
            node.tick = NetTick(tps * 4 * k);
            node.reject(peer, RejectKind::VoiceMalformed, "voice packet");
            assert!(node.peer_malformed_points(peer).is_some_and(|p| p <= 28));
        }
        assert!(node.pending_kicks.is_empty(), "a slow drip is never removed");
        // A burst of 63 in one tick is survived; the next one (64 x 16 = 1,024) removes it.
        node.tick = NetTick(tps * 1_000);
        for _ in 0..63 {
            node.reject(peer, RejectKind::VoiceMalformed, "voice packet");
        }
        assert!(node.pending_kicks.is_empty(), "63 refusals are survived");
        assert!(
            node.peer_malformed_points(peer)
                .is_some_and(|p| p < MALFORMED_SCORE_LIMIT)
        );
        // A stream at the voice rate: one a tick at 60 ticks a second crosses within about 1.1 s.
        let start = tps * 2_000;
        let mut removed_at = None;
        for t in 0..tps * 2 {
            node.tick = NetTick(start + t);
            node.reject(peer, RejectKind::VoiceMalformed, "voice packet");
            if removed_at.is_none() && !node.pending_kicks.is_empty() {
                removed_at = Some(t);
            }
        }
        assert_eq!(
            node.pending_kicks.first().map(|k| k.1),
            Some(crate::handshake::DisconnectReason::MalformedPacket)
        );
        let removed_at = removed_at.expect("a sustained stream is removed");
        assert!(
            removed_at < tps + tps / 10,
            "removed after {removed_at} ticks at {tps} ticks a second"
        );
    }
}
