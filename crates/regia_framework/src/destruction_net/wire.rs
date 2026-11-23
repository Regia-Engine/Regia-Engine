// SPDX-License-Identifier: MPL-2.0
//! The bits of destruction over `regia_net` (ADR-1967): [`DestructionMessage`], server to one peer
//! on the reliable-ordered channel, and [`DestructionPoses`], unreliable-sequenced poses of moving
//! structures. Decoders read untrusted bytes: every length is capped before anything is reserved
//! and a non-finite number is refused, so nothing a peer sends reaches rapier as a NaN.

use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::{CodecContext, LayoutHasher, ReplicateCodec};
use regia_net::message::{Channel, Direction, NetMessage, ToOwnerMessage, ToServerMessage};
use regia_net::settings::RateLimit;
use regia_physics::destruction::{
    BreakEvent, BreakKind, CompactBreak, DestructibleId, DestructiblePose, DestructionSnapshot, DetachCause,
    InstanceSnapshot, MAX_DESTRUCTIBLE_CHUNKS,
};

/// Ops one stream message may carry (a full break is about 60 bytes: 64 KiB holds these).
pub const MAX_OPS_PER_MESSAGE: usize = 1024;

/// Broken bonds one snapshot instance may list.
pub const MAX_SNAPSHOT_BONDS: usize = 1 << 20;

/// Instances one snapshot part may hold.
pub const MAX_SNAPSHOT_INSTANCES: usize = 4096;

/// Poses one message may hold.
pub const MAX_POSES_PER_MESSAGE: usize = 512;

/// One step of the server's stream, applied in order.
#[derive(Clone, Debug, PartialEq)]
pub enum WireOp {
    /// An instance appeared.
    Define {
        /// Its id.
        id: DestructibleId,
        /// The key of its asset ([`super::asset_key`]).
        asset: u64,
        /// Its pose.
        pose: DestructiblePose,
        /// Its seed.
        seed: u64,
    },
    /// An instance left.
    Remove {
        /// Its id.
        id: DestructibleId,
    },
    /// The next event in the short form: the replica rebuilds the rest from asset, pose and seed.
    Compact {
        /// The instance.
        id: DestructibleId,
        /// Tick of the decision.
        tick: u64,
        /// What broke.
        kind: CompactBreak,
    },
    /// The next event in full (a structure that moved, where the short form would not rebuild).
    Full {
        /// The instance.
        id: DestructibleId,
        /// Tick of the decision.
        tick: u64,
        /// What broke.
        kind: BreakKind,
        /// The event seed.
        seed: u64,
    },
    /// The server's `structure_hash` at this point of the stream: a replica that differs lost step.
    Check {
        /// The hash.
        hash: u64,
    },
    /// An instance became relevant to this peer (ADR-2305): its whole state, as a snapshot holds
    /// it. A big one comes in pieces with the same id, each list continuing the last; `more` is
    /// set on every piece but the last.
    Instance {
        /// The instance, or a piece of it.
        instance: WireInstance,
        /// Another piece of the same instance follows.
        more: bool,
    },
    /// Breaks of instances this peer does not hold were left out (ADR-2305): the server's event
    /// sequence and digest after them.
    Skip {
        /// The next event's `seq`.
        next_seq: u64,
        /// The event digest after the skipped events.
        digest: u64,
    },
}

/// One tick's ops for one peer.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct StreamBatch {
    /// `seq` of the first break op; the others count up from it.
    pub first_seq: u64,
    /// Ticks of the break ops are written relative to this.
    pub base_tick: u64,
    /// The ops, in order.
    pub ops: Vec<WireOp>,
}

/// One instance in a snapshot.
#[derive(Clone, Debug, PartialEq)]
pub struct WireInstance {
    /// The key of its asset.
    pub asset: u64,
    /// Its state.
    pub state: InstanceSnapshot,
}

/// A piece of a snapshot; part 0 carries the header. A big world spans several messages.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct SnapshotPart {
    /// Counts up per peer: parts of different snapshots never mix.
    pub sync: u32,
    /// This part, from 0.
    pub index: u16,
    /// How many parts the snapshot has.
    pub total: u16,
    /// The world counters (the header of [`DestructionSnapshot`]; `instances` here stays empty).
    pub header: DestructionSnapshot,
    /// The instances of this part.
    pub instances: Vec<WireInstance>,
}

/// Everything that must arrive whole and in order, in one type: a handler sees one delivery order
/// per message type, and a snapshot and the stream after it must not swap.
#[derive(Clone, Debug, PartialEq)]
pub enum DestructionMessage {
    /// The next ops of the stream.
    Stream(StreamBatch),
    /// A piece of a snapshot.
    Snapshot(SnapshotPart),
}

impl NetMessage for DestructionMessage {
    const DIRECTION: Direction = Direction::ToOwner;
}
impl ToOwnerMessage for DestructionMessage {}

/// Client to server: "I lost step, send me a snapshot".
#[derive(Clone, Debug, PartialEq)]
pub struct DestructionResync {
    /// The events the client has applied.
    pub have_seq: u64,
}

impl NetMessage for DestructionResync {
    const DIRECTION: Direction = Direction::ToServer;
    const RATE_LIMIT: Option<RateLimit> = Some(RateLimit {
        count: 4,
        per_ticks: 60,
    });
}
impl ToServerMessage for DestructionResync {}

/// The poses of structures that move, per peer (only what that peer holds, ADR-2573); newest wins,
/// and the server repeats them while it can lose one.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct DestructionPoses {
    /// Instance and pose.
    pub poses: Vec<(DestructibleId, DestructiblePose)>,
}

impl NetMessage for DestructionPoses {
    const DIRECTION: Direction = Direction::ToOwner;
    const CHANNEL: Channel = Channel::UnreliableSequenced;
}
impl ToOwnerMessage for DestructionPoses {}

fn write_f3(w: &mut BitWriter, v: [f32; 3]) {
    for c in v {
        w.write_f32(c);
    }
}

fn read_finite(r: &mut BitReader) -> Result<f32, DecodeError> {
    let v = r.read_f32()?;
    if v.is_finite() {
        Ok(v)
    } else {
        Err(DecodeError::Invalid("non-finite destruction value"))
    }
}

fn read_f3(r: &mut BitReader) -> Result<[f32; 3], DecodeError> {
    Ok([read_finite(r)?, read_finite(r)?, read_finite(r)?])
}

fn read_u32(r: &mut BitReader) -> Result<u32, DecodeError> {
    u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("destruction index above u32"))
}

fn write_id(w: &mut BitWriter, id: DestructibleId) {
    w.write_varint(u64::from(id.0));
}

fn read_id(r: &mut BitReader) -> Result<DestructibleId, DecodeError> {
    read_u32(r).map(DestructibleId)
}

fn write_pose(w: &mut BitWriter, pose: &DestructiblePose) {
    write_f3(w, pose.translation);
    for c in pose.rotation {
        w.write_f32(c);
    }
}

fn read_pose(r: &mut BitReader) -> Result<DestructiblePose, DecodeError> {
    let translation = read_f3(r)?;
    let rotation = [read_finite(r)?, read_finite(r)?, read_finite(r)?, read_finite(r)?];
    Ok(DestructiblePose {
        translation,
        rotation,
    })
}

fn write_cause(w: &mut BitWriter, cause: DetachCause) {
    w.write_bool(cause == DetachCause::Collapse);
}

fn read_cause(r: &mut BitReader) -> Result<DetachCause, DecodeError> {
    Ok(if r.read_bool()? {
        DetachCause::Collapse
    } else {
        DetachCause::Damage
    })
}

const OP_DEFINE: u64 = 0;
const OP_REMOVE: u64 = 1;
const OP_BOND: u64 = 2;
const OP_DETACH: u64 = 3;
const OP_FULL_BOND: u64 = 4;
const OP_FULL_DETACH: u64 = 5;
const OP_CHECK: u64 = 6;
/// Tag 7 opens the extended ops: a varint names which.
const OP_EXTENDED: u64 = 7;
const EXT_INSTANCE: u64 = 0;
const EXT_SKIP: u64 = 1;

fn write_op(w: &mut BitWriter, op: &WireOp, base_tick: u64) {
    match op {
        WireOp::Define {
            id,
            asset,
            pose,
            seed,
        } => {
            w.write_bits(OP_DEFINE, 3);
            write_id(w, *id);
            w.write_bits(*asset, 64);
            write_pose(w, pose);
            w.write_bits(*seed, 64);
        }
        WireOp::Remove { id } => {
            w.write_bits(OP_REMOVE, 3);
            write_id(w, *id);
        }
        WireOp::Compact { id, tick, kind } => {
            let tag = match kind {
                CompactBreak::Bond { .. } => OP_BOND,
                CompactBreak::Detach { .. } => OP_DETACH,
            };
            w.write_bits(tag, 3);
            write_id(w, *id);
            w.write_varint(tick.saturating_sub(base_tick));
            match kind {
                CompactBreak::Bond { bond } => w.write_varint(u64::from(*bond)),
                CompactBreak::Detach { chunk, cause, linvel } => {
                    w.write_varint(u64::from(*chunk));
                    write_cause(w, *cause);
                    write_f3(w, *linvel);
                }
            }
        }
        WireOp::Full { id, tick, kind, seed } => {
            let tag = match kind {
                BreakKind::BondBroken { .. } => OP_FULL_BOND,
                BreakKind::ChunkDetached { .. } => OP_FULL_DETACH,
            };
            w.write_bits(tag, 3);
            write_id(w, *id);
            w.write_varint(tick.saturating_sub(base_tick));
            w.write_bits(*seed, 64);
            match kind {
                BreakKind::BondBroken { bond, a, b, position } => {
                    w.write_varint(u64::from(*bond));
                    w.write_varint(u64::from(*a));
                    w.write_varint(u64::from(*b));
                    write_f3(w, *position);
                }
                BreakKind::ChunkDetached {
                    chunk,
                    cause,
                    position,
                    linvel,
                    angvel,
                } => {
                    w.write_varint(u64::from(*chunk));
                    write_cause(w, *cause);
                    write_f3(w, *position);
                    write_f3(w, *linvel);
                    write_f3(w, *angvel);
                }
            }
        }
        WireOp::Check { hash } => {
            w.write_bits(OP_CHECK, 3);
            w.write_bits(*hash, 64);
        }
        WireOp::Instance { instance, more } => {
            w.write_bits(OP_EXTENDED, 3);
            w.write_varint(EXT_INSTANCE);
            w.write_bool(*more);
            write_instance(w, instance);
        }
        WireOp::Skip { next_seq, digest } => {
            w.write_bits(OP_EXTENDED, 3);
            w.write_varint(EXT_SKIP);
            w.write_varint(*next_seq);
            w.write_bits(*digest, 64);
        }
    }
}

fn read_tick(r: &mut BitReader, base_tick: u64) -> Result<u64, DecodeError> {
    base_tick
        .checked_add(r.read_varint()?)
        .ok_or(DecodeError::Invalid("destruction tick overflows"))
}

fn read_op(r: &mut BitReader, base_tick: u64) -> Result<WireOp, DecodeError> {
    Ok(match r.read_bits(3)? {
        OP_DEFINE => WireOp::Define {
            id: read_id(r)?,
            asset: r.read_bits(64)?,
            pose: read_pose(r)?,
            seed: r.read_bits(64)?,
        },
        OP_REMOVE => WireOp::Remove { id: read_id(r)? },
        OP_BOND => WireOp::Compact {
            id: read_id(r)?,
            tick: read_tick(r, base_tick)?,
            kind: CompactBreak::Bond { bond: read_u32(r)? },
        },
        OP_DETACH => WireOp::Compact {
            id: read_id(r)?,
            tick: read_tick(r, base_tick)?,
            kind: CompactBreak::Detach {
                chunk: read_u32(r)?,
                cause: read_cause(r)?,
                linvel: read_f3(r)?,
            },
        },
        OP_FULL_BOND => WireOp::Full {
            id: read_id(r)?,
            tick: read_tick(r, base_tick)?,
            seed: r.read_bits(64)?,
            kind: BreakKind::BondBroken {
                bond: read_u32(r)?,
                a: read_u32(r)?,
                b: read_u32(r)?,
                position: read_f3(r)?,
            },
        },
        OP_FULL_DETACH => WireOp::Full {
            id: read_id(r)?,
            tick: read_tick(r, base_tick)?,
            seed: r.read_bits(64)?,
            kind: BreakKind::ChunkDetached {
                chunk: read_u32(r)?,
                cause: read_cause(r)?,
                position: read_f3(r)?,
                linvel: read_f3(r)?,
                angvel: read_f3(r)?,
            },
        },
        OP_CHECK => WireOp::Check {
            hash: r.read_bits(64)?,
        },
        OP_EXTENDED => match r.read_varint()? {
            EXT_INSTANCE => {
                let more = r.read_bool()?;
                WireOp::Instance {
                    instance: read_instance(r)?,
                    more,
                }
            }
            EXT_SKIP => WireOp::Skip {
                next_seq: r.read_varint()?,
                digest: r.read_bits(64)?,
            },
            _ => return Err(DecodeError::Invalid("unknown destruction op")),
        },
        _ => return Err(DecodeError::Invalid("unknown destruction op")),
    })
}

/// Ascending indices as gaps: most lists are short and clustered.
fn write_list(w: &mut BitWriter, list: &[u32]) {
    w.write_varint(list.len() as u64);
    let mut last = 0;
    for &v in list {
        w.write_varint(u64::from(v.saturating_sub(last)));
        last = v;
    }
}

fn read_list(r: &mut BitReader, max: usize) -> Result<Vec<u32>, DecodeError> {
    let len = r.read_len(max)?;
    // Every entry costs at least a byte: refuse a length the input cannot hold.
    if len > r.remaining_bits() / 8 {
        return Err(DecodeError::Invalid(
            "destruction list is longer than its message",
        ));
    }
    let mut out = Vec::with_capacity(len);
    let mut last = 0u32;
    for _ in 0..len {
        let gap = read_u32(r)?;
        last = last
            .checked_add(gap)
            .ok_or(DecodeError::Invalid("destruction list overflows"))?;
        out.push(last);
    }
    Ok(out)
}

fn write_instance(w: &mut BitWriter, i: &WireInstance) {
    w.write_bits(i.asset, 64);
    write_id(w, i.state.id);
    write_pose(w, &i.state.pose);
    w.write_bits(i.state.seed, 64);
    write_list(w, &i.state.detached);
    write_list(w, &i.state.broken);
}

fn read_instance(r: &mut BitReader) -> Result<WireInstance, DecodeError> {
    Ok(WireInstance {
        asset: r.read_bits(64)?,
        state: InstanceSnapshot {
            id: read_id(r)?,
            pose: read_pose(r)?,
            seed: r.read_bits(64)?,
            detached: read_list(r, MAX_DESTRUCTIBLE_CHUNKS)?,
            broken: read_list(r, MAX_SNAPSHOT_BONDS)?,
        },
    })
}

/// The bytes one snapshot instance takes.
pub(crate) fn instance_bytes(i: &WireInstance) -> usize {
    let mut w = BitWriter::new();
    write_instance(&mut w, i);
    w.finish().len()
}

/// Bytes of `v` as a varint.
fn varint_bytes(v: u64) -> usize {
    (64 - v.leading_zeros() as usize).div_ceil(7).max(1)
}

/// Bytes of an instance's fixed part (asset, id, pose, seed and the two list lengths, at most).
fn instance_head_bytes(id: DestructibleId) -> usize {
    8 + varint_bytes(u64::from(id.0)) + 7 * 4 + 8 + 2 * varint_bytes(MAX_SNAPSHOT_BONDS as u64)
}

/// An instance in pieces of at most about `max_bytes` each (ADR-2305): every piece has the
/// instance's asset, id, pose and seed, and the lists continue from piece to piece, in order. An
/// instance that fits is one piece. A receiver joins consecutive pieces with the same id
/// ([`join_piece`]).
pub(crate) fn split_instance(i: &WireInstance, max_bytes: usize) -> Vec<WireInstance> {
    if instance_bytes(i) <= max_bytes {
        return vec![i.clone()];
    }
    let room = max_bytes.saturating_sub(instance_head_bytes(i.state.id)).max(16);
    let empty = || WireInstance {
        asset: i.asset,
        state: InstanceSnapshot {
            detached: Vec::new(),
            broken: Vec::new(),
            ..i.state.clone()
        },
    };
    let mut pieces = vec![empty()];
    let mut used = 0usize;
    // Detached chunks first, then broken bonds; each list's gaps restart in every piece.
    for (list, is_detached) in [(&i.state.detached, true), (&i.state.broken, false)] {
        let mut last = 0u32;
        for &v in list {
            let mut cost = varint_bytes(u64::from(v.saturating_sub(last)));
            if used + cost > room {
                // A new piece's list starts from 0 again.
                pieces.push(empty());
                used = 0;
                cost = varint_bytes(u64::from(v));
            }
            if let Some(piece) = pieces.last_mut() {
                if is_detached {
                    piece.state.detached.push(v);
                } else {
                    piece.state.broken.push(v);
                }
            }
            used += cost;
            last = v;
        }
    }
    pieces
}

/// Joins `piece` onto `into` when it continues it: same asset, id, pose and seed, and each list
/// carries on in ascending order. `false` (and nothing joined) otherwise.
pub(crate) fn join_piece(into: &mut WireInstance, piece: &WireInstance) -> bool {
    let continues = |a: &[u32], b: &[u32]| match (a.last(), b.first()) {
        (Some(x), Some(y)) => y > x,
        _ => true,
    };
    let fits = into.asset == piece.asset
        && into.state.id == piece.state.id
        && into.state.pose == piece.state.pose
        && into.state.seed == piece.state.seed
        && continues(&into.state.detached, &piece.state.detached)
        && continues(&into.state.broken, &piece.state.broken)
        // Detached chunks come before broken bonds: none after a piece that had broken bonds.
        && (piece.state.detached.is_empty() || into.state.broken.is_empty());
    if fits {
        into.state.detached.extend_from_slice(&piece.state.detached);
        into.state.broken.extend_from_slice(&piece.state.broken);
    }
    fits
}

impl ReplicateCodec for DestructionMessage {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        match self {
            Self::Stream(batch) => {
                w.write_bool(false);
                w.write_varint(batch.first_seq);
                w.write_varint(batch.base_tick);
                w.write_varint(batch.ops.len() as u64);
                for op in &batch.ops {
                    write_op(w, op, batch.base_tick);
                }
            }
            Self::Snapshot(part) => {
                w.write_bool(true);
                w.write_varint(u64::from(part.sync));
                w.write_varint(u64::from(part.index));
                w.write_varint(u64::from(part.total));
                w.write_varint(u64::from(part.header.next_id));
                w.write_varint(part.header.next_seq);
                w.write_bits(part.header.event_digest, 64);
                w.write_varint(part.header.tick);
                w.write_varint(part.instances.len() as u64);
                for i in &part.instances {
                    write_instance(w, i);
                }
            }
        }
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        if !r.read_bool()? {
            let first_seq = r.read_varint()?;
            let base_tick = r.read_varint()?;
            let len = r.read_len(MAX_OPS_PER_MESSAGE)?;
            let mut ops = Vec::with_capacity(len.min(r.remaining_bits()));
            for _ in 0..len {
                ops.push(read_op(r, base_tick)?);
            }
            return Ok(Self::Stream(StreamBatch {
                first_seq,
                base_tick,
                ops,
            }));
        }
        let sync = read_u32(r)?;
        let index = u16::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("part index"))?;
        let total = u16::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("part count"))?;
        if index >= total {
            return Err(DecodeError::Invalid("part index past its count"));
        }
        let header = DestructionSnapshot {
            next_id: read_u32(r)?,
            next_seq: r.read_varint()?,
            event_digest: r.read_bits(64)?,
            tick: r.read_varint()?,
            instances: Vec::new(),
        };
        let len = r.read_len(MAX_SNAPSHOT_INSTANCES)?;
        let mut instances = Vec::with_capacity(len.min(r.remaining_bits()));
        for _ in 0..len {
            instances.push(read_instance(r)?);
        }
        Ok(Self::Snapshot(SnapshotPart {
            sync,
            index,
            total,
            header,
            instances,
        }))
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str("regia_framework::destruction_net::DestructionMessage/2");
    }
}

impl ReplicateCodec for DestructionResync {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_varint(self.have_seq);
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        Ok(Self {
            have_seq: r.read_varint()?,
        })
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str("regia_framework::destruction_net::DestructionResync/1");
    }
}

impl ReplicateCodec for DestructionPoses {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        w.write_varint(self.poses.len() as u64);
        for (id, pose) in &self.poses {
            write_id(w, *id);
            write_pose(w, pose);
        }
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let len = r.read_len(MAX_POSES_PER_MESSAGE)?;
        let mut poses = Vec::with_capacity(len.min(r.remaining_bits()));
        for _ in 0..len {
            poses.push((read_id(r)?, read_pose(r)?));
        }
        Ok(Self { poses })
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str("regia_framework::destruction_net::DestructionPoses/1");
    }
}

/// A full [`BreakEvent`] from its op and its place in the stream.
pub(crate) fn full_event(
    seq: u64,
    tick: u64,
    destructible: DestructibleId,
    kind: &BreakKind,
    seed: u64,
) -> BreakEvent {
    BreakEvent {
        seq,
        tick,
        destructible,
        kind: kind.clone(),
        seed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(m: &DestructionMessage) -> Vec<u8> {
        let mut w = BitWriter::new();
        m.encode(&mut w, &CodecContext::detached());
        w.finish()
    }

    fn decoded(bytes: &[u8]) -> Result<DestructionMessage, DecodeError> {
        let mut r = BitReader::new(bytes);
        DestructionMessage::decode(&mut r, &CodecContext::detached())
    }

    fn pose() -> DestructiblePose {
        DestructiblePose {
            translation: [1.0, -2.5, 3.25],
            rotation: [0.0, 0.0, 0.0, 1.0],
        }
    }

    fn every_op() -> Vec<WireOp> {
        let id = DestructibleId(7);
        vec![
            WireOp::Define {
                id,
                asset: 0xABCD_EF01_2345_6789,
                pose: pose(),
                seed: u64::MAX,
            },
            WireOp::Compact {
                id,
                tick: 40,
                kind: CompactBreak::Bond { bond: 12 },
            },
            WireOp::Compact {
                id,
                tick: 41,
                kind: CompactBreak::Detach {
                    chunk: 4095,
                    cause: DetachCause::Collapse,
                    linvel: [0.5, -0.25, 1e-3],
                },
            },
            WireOp::Full {
                id,
                tick: 41,
                seed: 5,
                kind: BreakKind::BondBroken {
                    bond: 3,
                    a: 1,
                    b: 2,
                    position: [1.0, 2.0, 3.0],
                },
            },
            WireOp::Full {
                id,
                tick: 42,
                seed: 6,
                kind: BreakKind::ChunkDetached {
                    chunk: 9,
                    cause: DetachCause::Damage,
                    position: [1.0, 2.0, 3.0],
                    linvel: [4.0, 5.0, 6.0],
                    angvel: [7.0, 8.0, 9.0],
                },
            },
            WireOp::Check { hash: 99 },
            WireOp::Instance {
                instance: WireInstance {
                    asset: 3,
                    state: InstanceSnapshot {
                        id,
                        pose: pose(),
                        seed: 4,
                        detached: vec![1, 5],
                        broken: vec![0, 7, 300],
                    },
                },
                more: true,
            },
            WireOp::Skip {
                next_seq: 1 << 40,
                digest: u64::MAX - 3,
            },
            WireOp::Remove { id },
        ]
    }

    fn big_instance(detached: u32, broken: u32, stride: u32) -> WireInstance {
        WireInstance {
            asset: 0x55,
            state: InstanceSnapshot {
                id: DestructibleId(40_000),
                pose: pose(),
                seed: 9,
                detached: (0..detached).map(|c| c * 3).collect(),
                broken: (0..broken).map(|b| b * stride).collect(),
            },
        }
    }

    #[test]
    fn a_big_instance_splits_into_pieces_that_fit_and_join_back_whole() {
        // 256 KiB and more of lists: up to 4096 detached chunks and 140,000 to 300,000 bonds at 1-byte and
        // 2-byte gaps.
        for (detached, broken, stride) in [(4096, 270_000, 1), (4096, 140_000, 200), (10, 300_000, 1)] {
            let whole = big_instance(detached, broken, stride);
            let size = instance_bytes(&whole);
            assert!(size >= 256 * 1024, "{size}");
            let pieces = split_instance(&whole, 48 * 1024);
            assert!(pieces.len() >= 6, "{} pieces", pieces.len());
            for piece in &pieces {
                assert!(instance_bytes(piece) <= 48 * 1024, "{}", instance_bytes(piece));
                // Each piece in its own snapshot message stays under the transport's 64 KiB.
                let m = DestructionMessage::Snapshot(SnapshotPart {
                    total: 1,
                    instances: vec![piece.clone()],
                    ..SnapshotPart::default()
                });
                let bytes = encoded(&m);
                assert!(bytes.len() < regia_net::message::MAX_MESSAGE_BYTES);
                assert_eq!(decoded(&bytes), Ok(m));
            }
            let mut joined = pieces[0].clone();
            for piece in &pieces[1..] {
                assert!(join_piece(&mut joined, piece));
            }
            assert_eq!(joined, whole);
        }
        // A small one is one piece, unchanged.
        let small = big_instance(3, 3, 1);
        assert_eq!(split_instance(&small, 48 * 1024), vec![small]);
    }

    #[test]
    fn pieces_that_do_not_continue_are_not_joined() {
        let whole = big_instance(4096, 100_000, 1);
        let pieces = split_instance(&whole, 16 * 1024);
        let mut a = pieces[0].clone();
        // The same piece twice goes back in the lists.
        assert!(!join_piece(&mut a, &pieces[0]));
        assert_eq!(a, pieces[0]);
        let mut other = pieces[1].clone();
        other.state.id = DestructibleId(1);
        assert!(!join_piece(&mut a, &other));
        let mut moved = pieces[1].clone();
        moved.state.pose.translation[0] += 1.0;
        assert!(!join_piece(&mut a, &moved));
        // Detached chunks after broken bonds would not be in order.
        let mut tail = pieces[pieces.len() - 1].clone();
        let mut late = tail.clone();
        late.state.detached = vec![u32::MAX];
        late.state.broken.clear();
        tail.state.broken = vec![1];
        assert!(!join_piece(&mut tail, &late));
    }

    #[test]
    fn every_op_round_trips_bit_exact() {
        let m = DestructionMessage::Stream(StreamBatch {
            first_seq: 1000,
            base_tick: 40,
            ops: every_op(),
        });
        assert_eq!(decoded(&encoded(&m)), Ok(m));
    }

    #[test]
    fn a_snapshot_part_round_trips() {
        let m = DestructionMessage::Snapshot(SnapshotPart {
            sync: 3,
            index: 1,
            total: 2,
            header: DestructionSnapshot {
                next_id: 9,
                next_seq: 700,
                event_digest: 0x1234,
                tick: 12_000,
                instances: Vec::new(),
            },
            instances: vec![WireInstance {
                asset: 5,
                state: InstanceSnapshot {
                    id: DestructibleId(2),
                    pose: pose(),
                    seed: 8,
                    detached: vec![0, 1, 2, 500],
                    broken: vec![9],
                },
            }],
        });
        assert_eq!(decoded(&encoded(&m)), Ok(m));
    }

    #[test]
    fn a_part_past_its_own_count_is_refused() {
        let part = |index, total| {
            DestructionMessage::Snapshot(SnapshotPart {
                index,
                total,
                ..SnapshotPart::default()
            })
        };
        for (index, total) in [(0, 0), (2, 2), (3, 2), (u16::MAX, u16::MAX)] {
            assert!(
                decoded(&encoded(&part(index, total))).is_err(),
                "{index} of {total}"
            );
        }
        let last = part(u16::MAX - 1, u16::MAX);
        assert_eq!(decoded(&encoded(&last)), Ok(last));
    }

    #[test]
    fn a_compact_break_is_a_fraction_of_a_full_one() {
        let size = |op: WireOp| {
            encoded(&DestructionMessage::Stream(StreamBatch {
                first_seq: 0,
                base_tick: 40,
                ops: vec![op],
            }))
            .len()
        };
        let ops = every_op();
        let (bond, detach, full_bond, full_detach) = (
            size(ops[1].clone()),
            size(ops[2].clone()),
            size(ops[3].clone()),
            size(ops[4].clone()),
        );
        assert!(bond * 3 <= full_bond, "{bond} against {full_bond}");
        assert!(detach * 2 <= full_detach, "{detach} against {full_detach}");
    }

    #[test]
    fn nothing_decodes_into_a_non_finite_number_or_an_unknown_op() {
        let m = DestructionMessage::Stream(StreamBatch {
            first_seq: 0,
            base_tick: 0,
            ops: vec![WireOp::Full {
                id: DestructibleId(0),
                tick: 0,
                seed: 0,
                kind: BreakKind::BondBroken {
                    bond: 0,
                    a: 0,
                    b: 1,
                    position: [f32::NAN, 0.0, 0.0],
                },
            }],
        });
        assert!(decoded(&encoded(&m)).is_err());
        // Tag 7 opens the extended ops; extended op 2 and up are free.
        for ext in [2u64, 3, 1 << 40] {
            let mut w = BitWriter::new();
            w.write_bool(false);
            w.write_varint(0);
            w.write_varint(0);
            w.write_varint(1);
            w.write_bits(7, 3);
            w.write_varint(ext);
            w.write_bits(0, 64);
            assert!(decoded(&w.finish()).is_err(), "extended op {ext}");
        }
    }

    #[test]
    fn hostile_lengths_are_refused_before_they_reserve() {
        let mut w = BitWriter::new();
        w.write_bool(false);
        w.write_varint(0);
        w.write_varint(0);
        w.write_varint(u64::MAX);
        assert!(decoded(&w.finish()).is_err());
        let mut w = BitWriter::new();
        w.write_bits(0, 64);
        w.write_varint(0);
        for _ in 0..7 {
            w.write_f32(0.0);
        }
        w.write_bits(0, 64);
        w.write_varint(MAX_DESTRUCTIBLE_CHUNKS as u64); // a list that the input cannot hold
        let mut r = BitReader::new(&[]);
        let bytes = w.finish();
        assert!(read_instance(&mut r).is_err());
        let mut r = BitReader::new(&bytes);
        assert!(read_instance(&mut r).is_err());
    }

    #[test]
    fn random_bytes_never_panic() {
        // SplitMix64 bytes (seeded, the same every run).
        let mut state = 0x1234_5678_u64;
        let mut next = || {
            state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z ^ (z >> 27)
        };
        for len in 0..512usize {
            let bytes: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            let _ = decoded(&bytes);
            let mut r = BitReader::new(&bytes);
            let _ = DestructionPoses::decode(&mut r, &CodecContext::detached());
        }
        // Every truncation of a valid message is an error or a shorter message, never a panic.
        let bytes = encoded(&DestructionMessage::Stream(StreamBatch {
            first_seq: 5,
            base_tick: 40,
            ops: every_op(),
        }));
        for n in 0..bytes.len() {
            let _ = decoded(&bytes[..n]);
        }
    }
}
