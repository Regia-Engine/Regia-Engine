// SPDX-License-Identifier: MPL-2.0
//! Replication for a simulation that lives outside the ECS (spec 120 § 8.6, ADR-8186).
//!
//! A game whose authoritative state is its own data (a lockstep-capable sim, a server written
//! against plain structs) still wants what the ECS replication path gives an ECS world: per-client
//! relevance with hysteresis, priority accumulators, distance rate tiers, a byte budget per client
//! and acknowledged-baseline deltas. [`ExternalReplication`] provides those over the standalone
//! [`SnapshotEncoder`]; [`ExternalInterpolator`] is the client side (decode, acknowledge,
//! interpolate between received states).
//!
//! The game supplies:
//!
//! - **entity keys** ([`ExternalKey`]: its own ids, mapped one to one onto [`NetId`]s on the wire),
//! - **positions** for relevance and a **class** per entity ([`ExternalClass`]: relevance shape,
//!   priority, rate tiers, refresh interval),
//! - a **priority function** and the **record codec** ([`ExternalSource`]: one value per field of
//!   the game's [`SnapshotSchema`]),
//! - per client: viewpoints, acknowledgements and (optionally) the rate it declared and the
//!   congestion controller's allowance.
//!
//! The engine provides the rest, with the same code the ECS path runs below its adapters: the
//! banded interest grid, the hysteresis rule and the rate tiers of [`crate::interest`], the
//! priority gain and the carry-over budget of [`crate::send`], and one [`SnapshotEncoder`] stream
//! (acknowledged baselines, budgeted packing by priority) per client. Nothing here needs the `ecs`
//! feature.
//!
//! One send tick, per client: grid candidates around each viewpoint plus the entities it already
//! holds plus world-relevant entities; the hysteresis test decides what it holds; each held entity's
//! accumulator grows by `class priority × game priority × distance factor`. Each held entity is
//! then either *offered* (it changed since the record the client last got and its tier interval
//! has passed, or that record was lost, or its refresh is due) or *deferred* (the client already
//! applied its current state, the record carrying it is still in flight, or its tier interval has
//! not passed). Offered entities are packed by accumulator into one packet of at most
//! `min(per-send budget, max_packet_bytes)` bytes ([`SnapshotEncoder::encode_selecting`]). Included
//! records reset their accumulator; budget-omitted ones keep growing, so nothing starves.
//!
//! Per-entity delivery, not the baseline chain, decides what the client shows: a deferred entity
//! stays at the acknowledged baseline's state in the codec's chain (so the decoder's world may
//! show an older state for it), while [`ExternalInterpolator`] keeps every entity at the newest
//! record it applied. The client's [`ExternalAck`] therefore names both its newest baseline (the
//! delta base) and the packets whose records it applied (a 64-packet bitfield, the same idea as
//! the transport's ack ranges); a record whose packet the client reports missing is sent again.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Debug;
use std::hash::Hash;
use std::marker::PhantomData;

use glam::Vec3;
// The `Resource` derive emits `bevy_ecs::` paths: the prelude re-exports the crate under that name.
#[cfg(feature = "ecs")]
use regia_ecs::prelude::{Resource, bevy_ecs};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::fast_map::FastMap;
use crate::interest::{GridAt, InterestGrid, RateTier, RateTiers, RelevanceShape, spatial_limit};
use crate::model::{NetId, NetTick, PeerId};
use crate::send::{MIN_CLIENT_RATE, priority_gain, tick_budget};
use crate::snapshot_delta::{
    DEFAULT_BASELINE_CAPACITY, EntitySnapshot, FieldValue, RecordSelection, Seq, SnapshotDecoder,
    SnapshotEncoder, SnapshotError, SnapshotSchema, WorldSnapshot,
};

/// A game's entity key. Keys map one to one onto the wire's [`NetId`]s, so the client can name the
/// game entity a record belongs to.
pub trait ExternalKey: Copy + Eq + Ord + Hash + Debug {
    /// The key's wire id.
    fn net_id(self) -> NetId;
    /// The key of a wire id, or `None` if no key maps to it.
    fn from_net_id(id: NetId) -> Option<Self>;
}

impl ExternalKey for u32 {
    fn net_id(self) -> NetId {
        NetId(self)
    }
    fn from_net_id(id: NetId) -> Option<Self> {
        Some(id.0)
    }
}

impl ExternalKey for u16 {
    fn net_id(self) -> NetId {
        NetId(u32::from(self))
    }
    fn from_net_id(id: NetId) -> Option<Self> {
        u16::try_from(id.0).ok()
    }
}

impl ExternalKey for NetId {
    fn net_id(self) -> NetId {
        self
    }
    fn from_net_id(id: NetId) -> Option<Self> {
        Some(id)
    }
}

/// What the game tells the engine about its entities on a send tick.
pub trait ExternalSource<K: ExternalKey> {
    /// The game's priority multiplier for `key` as seen by `client` (>= 0; DO27's combat-heat term
    /// is one such input). `distance` is the metres to the client's nearest viewpoint (`None`
    /// without a viewpoint or for a world-relevant entity without one). Default 1.
    fn priority(&self, key: K, client: PeerId, distance: Option<f32>) -> f32 {
        let _ = (key, client, distance);
        1.0
    }

    /// The record codec: writes the entity's state, one value per field of the schema, into `out`
    /// (cleared by the caller). Called at most once per entity per send tick.
    fn record(&self, key: K, out: &mut Vec<FieldValue>);
}

/// A relevance and rate class of entities (the external path's `NetProfile`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalClass {
    /// Where the entity is relevant: within a radius of a viewpoint (with hysteresis), or everywhere.
    pub relevance: RelevanceShape,
    /// Base priority (> 0): accumulator gain per send tick before the game's and the distance factor.
    pub priority: f32,
    /// Distance send-rate tiers (spec 120 § 8.3); `None`: every send.
    pub rate_tiers: Option<RateTiers>,
    /// Longest gap, in ticks, between two records of a held entity; at the limit the record is
    /// forced (ranked first, never deferred). `None`: no forced refresh.
    pub max_interval_ticks: Option<u32>,
}

impl Default for ExternalClass {
    fn default() -> Self {
        Self {
            relevance: RelevanceShape::Spatial {
                radius: 300.0,
                hysteresis: 0.1,
            },
            priority: 1.0,
            rate_tiers: None,
            max_interval_ticks: None,
        }
    }
}

/// Settings of an [`ExternalReplication`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExternalConfig {
    /// The simulation's fixed tick rate in Hz.
    pub tick_hz: f64,
    /// A packet goes to each client every this many ticks (`tick % send_every_ticks == 0`).
    pub send_every_ticks: u32,
    /// The project's per-client rate in bytes per second (the same default as
    /// `NetSettings::client_rate_bytes`).
    pub rate: u32,
    /// Largest packet in bytes (one datagram's payload).
    pub max_packet_bytes: usize,
    /// Interest grid cell size in metres.
    pub cell_m: f32,
    /// Baselines each client's encoder keeps: size it to the worst round trip in sends.
    pub baseline_capacity: usize,
    /// The classes; an entity names one by index. Empty means one default class.
    pub classes: Vec<ExternalClass>,
}

impl Default for ExternalConfig {
    fn default() -> Self {
        Self {
            tick_hz: 60.0,
            send_every_ticks: 1,
            rate: 49_152,
            max_packet_bytes: 1200,
            cell_m: 64.0,
            baseline_capacity: DEFAULT_BASELINE_CAPACITY,
            classes: Vec::new(),
        }
    }
}

impl ExternalConfig {
    /// Checks the settings.
    pub fn validate(&self) -> Result<(), ExternalError> {
        let bad = |field: &'static str, message: String| Err(ExternalError::InvalidConfig { field, message });
        if !(self.tick_hz > 0.0 && self.tick_hz.is_finite()) {
            return bad("tick_hz", format!("must be > 0, is {}", self.tick_hz));
        }
        if self.send_every_ticks == 0 {
            return bad("send_every_ticks", "must be >= 1".into());
        }
        if self.rate < MIN_CLIENT_RATE {
            return bad("rate", format!("must be >= {MIN_CLIENT_RATE}, is {}", self.rate));
        }
        if self.max_packet_bytes < 64 {
            return bad(
                "max_packet_bytes",
                format!("must be >= 64, is {}", self.max_packet_bytes),
            );
        }
        if !(self.cell_m > 0.0 && self.cell_m.is_finite()) {
            return bad("cell_m", format!("must be > 0, is {}", self.cell_m));
        }
        if self.classes.len() > usize::from(u16::MAX) {
            return bad("classes", format!("at most {} classes", u16::MAX));
        }
        for class in &self.classes {
            if !(class.priority > 0.0 && class.priority.is_finite()) {
                return bad("classes.priority", format!("must be > 0, is {}", class.priority));
            }
            if let RelevanceShape::Spatial { radius, hysteresis } = class.relevance
                && !(radius > 0.0 && radius.is_finite() && (0.0..=1.0).contains(&hysteresis))
            {
                return bad(
                    "classes.relevance",
                    format!("radius must be > 0 and hysteresis in 0..=1, are {radius} and {hysteresis}"),
                );
            }
            if let Some(t) = &class.rate_tiers {
                t.check()
                    .map_err(|(field, message)| ExternalError::InvalidConfig { field, message })?;
            }
        }
        Ok(())
    }
}

/// Why an external replication call failed. [`ExternalError::code`] is the stable code a host
/// reports to agents.
#[derive(Debug, Clone, PartialEq, Error)]
pub enum ExternalError {
    /// A setting is out of range.
    #[error("invalid external replication setting `{field}`: {message}")]
    InvalidConfig {
        /// The setting.
        field: &'static str,
        /// What is wrong.
        message: String,
    },
    /// No client with this id was added.
    #[error("unknown client {0:?}")]
    UnknownClient(PeerId),
    /// A client with this id already exists.
    #[error("client {0:?} already exists")]
    DuplicateClient(PeerId),
    /// An entity names a class the config does not have.
    #[error("unknown class {class} (the config has {count})")]
    UnknownClass {
        /// The class index.
        class: u16,
        /// How many classes the config has.
        count: usize,
    },
    /// An entity's position is not finite.
    #[error("entity position is not finite")]
    BadPosition,
    /// The codec refused a client's packet (usually a record that does not match the schema).
    #[error("encoding for client {peer:?} failed: {error}")]
    Encode {
        /// The client.
        peer: PeerId,
        /// The codec's error.
        error: SnapshotError,
    },
    /// The client could not decode a packet (it is dropped; the stream recovers on the next one).
    #[error("decoding failed: {0}")]
    Decode(SnapshotError),
}

impl ExternalError {
    /// The stable lowercase code of the error (agent replies, logs).
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidConfig { .. } => "invalid_config",
            Self::UnknownClient(_) => "unknown_client",
            Self::DuplicateClient(_) => "duplicate_client",
            Self::UnknownClass { .. } => "unknown_class",
            Self::BadPosition => "bad_position",
            Self::Encode { .. } => "encode_failed",
            Self::Decode(_) => "decode_failed",
        }
    }
}

/// A client's acknowledgement (spec 120 § 8.6), sent back on any channel. [`ExternalAck::to_bytes`]
/// is a fixed 16-byte little-endian form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExternalAck {
    /// The newest baseline the client holds: the server deltas against it.
    pub baseline: Seq,
    /// The newest packet whose records the client applied.
    pub applied: Seq,
    /// Bit `i` set: packet `applied - 1 - i` was applied too.
    pub applied_bits: u64,
}

impl ExternalAck {
    /// Bytes of [`ExternalAck::to_bytes`].
    pub const BYTES: usize = 16;

    /// The 16-byte form: `baseline u32le | applied u32le | applied_bits u64le`.
    pub fn to_bytes(&self) -> [u8; Self::BYTES] {
        let mut out = [0u8; Self::BYTES];
        out[..4].copy_from_slice(&self.baseline.0.to_le_bytes());
        out[4..8].copy_from_slice(&self.applied.0.to_le_bytes());
        out[8..].copy_from_slice(&self.applied_bits.to_le_bytes());
        out
    }

    /// Reads [`ExternalAck::to_bytes`]; `None` unless `bytes` is exactly 16 bytes.
    pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; Self::BYTES] = bytes.try_into().ok()?;
        let (b, rest) = bytes.split_first_chunk::<4>()?;
        let (a, bits) = rest.split_first_chunk::<4>()?;
        let bits: [u8; 8] = (*bits).try_into().ok()?;
        Some(Self {
            baseline: Seq(u32::from_le_bytes(*b)),
            applied: Seq(u32::from_le_bytes(*a)),
            applied_bits: u64::from_le_bytes(bits),
        })
    }
}

/// Serial order of sequence numbers (RFC 1982): `a` is newer than `b`.
fn seq_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Sequences behind the newest applied one that a lost packet is judged lost after (covers the
/// reorder the client may still see).
const LOST_AFTER: u32 = 3;

/// The packets a client applied, over the last 256 sequences.
#[derive(Debug, Clone, Default)]
struct Receipts {
    newest: Option<u32>,
    /// Bit `k` of the 256: sequence `newest - k` was applied.
    bits: [u64; 4],
}

/// What the server knows of one packet's delivery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Delivery {
    Applied,
    InFlight,
    Lost,
}

impl Receipts {
    fn set(&mut self, seq: u32) {
        let Some(newest) = self.newest else {
            self.newest = Some(seq);
            self.bits = [1, 0, 0, 0];
            return;
        };
        if seq_newer(seq, newest) {
            let shift = seq.wrapping_sub(newest);
            self.shift(shift);
            self.newest = Some(seq);
            self.bits[0] |= 1;
        } else {
            let k = newest.wrapping_sub(seq);
            if k < 256 {
                self.bits[(k / 64) as usize] |= 1 << (k % 64);
            }
        }
    }

    /// Moves every bit `by` positions toward older sequences.
    fn shift(&mut self, by: u32) {
        if by >= 256 {
            self.bits = [0; 4];
            return;
        }
        let (words, bits) = ((by / 64) as usize, by % 64);
        let old = self.bits;
        for (i, w) in self.bits.iter_mut().enumerate() {
            let lo = i
                .checked_sub(words)
                .and_then(|j| old.get(j))
                .copied()
                .unwrap_or(0);
            let carry = if bits == 0 {
                0
            } else {
                i.checked_sub(words + 1)
                    .and_then(|j| old.get(j))
                    .map_or(0, |v| v >> (64 - bits))
            };
            *w = (lo << bits) | carry;
        }
    }

    fn take(&mut self, ack: &ExternalAck) {
        self.set(ack.applied.0);
        for i in 0..64u32 {
            if (ack.applied_bits >> i) & 1 == 1 {
                self.set(ack.applied.0.wrapping_sub(1 + i));
            }
        }
    }

    fn delivery(&self, seq: Seq) -> Delivery {
        let Some(newest) = self.newest else {
            return Delivery::InFlight;
        };
        if seq_newer(seq.0, newest) {
            return Delivery::InFlight;
        }
        let k = newest.wrapping_sub(seq.0);
        if k >= 256 {
            return Delivery::Lost;
        }
        if (self.bits[(k / 64) as usize] >> (k % 64)) & 1 == 1 {
            Delivery::Applied
        } else if k >= LOST_AFTER {
            Delivery::Lost
        } else {
            Delivery::InFlight
        }
    }
}

/// A 64-bit fingerprint of a record's quantized wire state (what the client would show).
fn record_hash(schema: &SnapshotSchema, record: &[FieldValue]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h = (h ^ v).wrapping_mul(0x0000_0100_0000_01b3);
        h ^= h >> 29;
    };
    for (at, value) in record.iter().enumerate() {
        let q = schema.quantize_value(at, value).unwrap_or(*value);
        match q {
            FieldValue::Bool(b) => mix(u64::from(b)),
            FieldValue::U8(v) => mix(u64::from(v)),
            FieldValue::U16(v) => mix(u64::from(v)),
            FieldValue::U32(v) | FieldValue::Enum(v) => mix(u64::from(v)),
            FieldValue::I32(v) => mix(u64::from(v as u32)),
            FieldValue::F32(v) => mix(u64::from(v.to_bits())),
            FieldValue::Vec3(v) => {
                for c in v.to_array() {
                    mix(u64::from(c.to_bits()));
                }
            }
            FieldValue::Quat(v) => {
                for c in v.to_array() {
                    mix(u64::from(c.to_bits()));
                }
            }
        }
        mix(at as u64 + 1);
    }
    h
}

/// One encoded packet for one client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalPacket {
    /// The client.
    pub peer: PeerId,
    /// The packet's sequence number (the client acknowledges it).
    pub seq: Seq,
    /// The packet bytes ([`SnapshotEncoder`] format).
    pub bytes: Vec<u8>,
}

/// Counters of one client's stream (cumulative unless named `last_*` or `relevant`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct ExternalClientCounters {
    /// Packets encoded.
    pub packets: u64,
    /// Packet bytes encoded.
    pub bytes: u64,
    /// Full packets (no usable acknowledged baseline).
    pub full_packets: u64,
    /// Records packed (adds, updates and removes).
    pub records: u64,
    /// Records the byte budget left out (deferred to a later send, never lost).
    pub omitted: u64,
    /// Updates the distance rate tiers held back.
    pub tier_held: u64,
    /// Records sent by distance tier, `[near, mid, far]` (no tiers counts as near).
    pub tier_sent: [u64; 3],
    /// Send ticks skipped because the budget could not hold even a packet header.
    pub budget_skips: u64,
    /// Acknowledgements taken.
    pub acks: u64,
    /// Acknowledgements ignored (older than the newest, or of a baseline the encoder no longer has).
    pub stale_acks: u64,
    /// Entities the client holds (relevant) after the last send.
    pub relevant: u32,
    /// The last send's byte budget.
    pub last_budget: u32,
    /// The last packet's bytes.
    pub last_bytes: u32,
}

/// One client in [`ExternalStats`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExternalClientStats {
    /// The client.
    pub peer: u32,
    /// The rate the budget uses, bytes per second.
    pub rate: u32,
    /// The newest acknowledged sequence.
    pub ack: Option<u32>,
    /// The counters.
    pub counters: ExternalClientCounters,
}

/// A snapshot of an [`ExternalReplication`] for hosts and agents: a host returns it from its own
/// agent reply (book page `agents/net-external-replication.md`).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExternalStats {
    /// Entities indexed.
    pub entities: usize,
    /// The grid's band radii in metres.
    pub band_radii_m: Vec<f32>,
    /// Each client, by id.
    pub clients: Vec<ExternalClientStats>,
}

/// One published [`ExternalStats`] snapshot in [`NetExternalStats`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExternalStatsEntry {
    /// The game tick the host published at.
    pub tick: u32,
    /// How many times this label was published (1 on the first publish).
    pub publishes: u64,
    /// The snapshot, as [`ExternalReplication::stats`] returned it.
    pub stats: ExternalStats,
}

/// External replication stats a host publishes for agents (`net.external_stats`, KI-NT-012,
/// ADR-8892). The replication lives in the game's process, not in an ECS world, so the engine
/// cannot find it: the host calls [`ExternalReplication::publish_stats`] (or [`Self::publish`])
/// under a label, at whatever rate it likes (once a second is plenty: [`ExternalReplication::stats`]
/// allocates one row per client). An ECS resource with the `ecs` feature; a plain value without.
#[derive(Debug, Clone, Default)]
#[cfg_attr(feature = "ecs", derive(Resource))]
pub struct NetExternalStats {
    entries: BTreeMap<String, ExternalStatsEntry>,
}

impl NetExternalStats {
    /// The most labels kept; a new label past it is refused (an existing one still refreshes).
    pub const MAX_LABELS: usize = 16;

    /// Stores `stats` as the newest snapshot of `label`, taken at `tick`. Returns `false` (and
    /// stores nothing) for a new label when [`Self::MAX_LABELS`] labels are already held.
    pub fn publish(&mut self, label: &str, tick: NetTick, stats: ExternalStats) -> bool {
        if let Some(entry) = self.entries.get_mut(label) {
            entry.tick = tick.0;
            entry.publishes += 1;
            entry.stats = stats;
            return true;
        }
        if self.entries.len() >= Self::MAX_LABELS {
            return false;
        }
        self.entries.insert(
            label.to_owned(),
            ExternalStatsEntry {
                tick: tick.0,
                publishes: 1,
                stats,
            },
        );
        true
    }

    /// The newest snapshot of `label`.
    pub fn get(&self, label: &str) -> Option<&ExternalStatsEntry> {
        self.entries.get(label)
    }

    /// Every published snapshot, by label.
    pub fn entries(&self) -> &BTreeMap<String, ExternalStatsEntry> {
        &self.entries
    }

    /// Forgets `label`; `false` if it was not published.
    pub fn remove(&mut self, label: &str) -> bool {
        self.entries.remove(label).is_some()
    }

    /// The number of published labels.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// No label is published.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// One entity row.
#[derive(Debug, Clone)]
struct Row<K> {
    key: K,
    pos: Vec3,
    class: u16,
    /// Where the slot sits in the grid (`None`: world-relevant, or a free slot).
    cell: Option<GridAt>,
    alive: bool,
}

/// One client's view of one entity it holds.
#[derive(Debug, Clone, Copy)]
struct Held {
    acc: f32,
    /// The tick of the last record sent.
    last_sent: Option<u32>,
    /// The packet that carried it, and the fingerprint of the state it carried.
    sent: Option<(Seq, u64)>,
    tier: RateTier,
}

/// One client.
#[derive(Debug)]
struct Client {
    encoder: SnapshotEncoder,
    ack: Option<Seq>,
    viewpoints: Vec<Vec3>,
    declared_rate: Option<u32>,
    allowance: Option<u32>,
    carry: f64,
    held: FastMap<u32, Held>,
    receipts: Receipts,
    counters: ExternalClientCounters,
    /// Reused per send: the world this client is sent.
    world: WorldSnapshot,
    /// Reused per send: `(id, slot)` of the world's entities, sorted by id.
    ids: Vec<(NetId, u32)>,
    /// Reused per send: deferred ids.
    defer: Vec<NetId>,
    /// Reused per send: offered ids (always get a record).
    force: Vec<NetId>,
}

impl Client {
    fn rate(&self, project: u32) -> u32 {
        let mut rate = project;
        if let Some(d) = self.declared_rate {
            rate = rate.min(d.max(MIN_CLIENT_RATE));
        }
        if let Some(a) = self.allowance {
            rate = rate.min(a.max(MIN_CLIENT_RATE));
        }
        rate
    }
}

/// Server-side replication of a game-owned simulation to many clients (spec 120 § 8.6). See the
/// module docs.
#[derive(Debug)]
pub struct ExternalReplication<K: ExternalKey> {
    config: ExternalConfig,
    classes: Vec<ExternalClass>,
    schema: SnapshotSchema,
    grid: InterestGrid,
    rows: Vec<Row<K>>,
    slot_of: FastMap<K, u32>,
    free: Vec<u32>,
    world_slots: BTreeSet<u32>,
    clients: BTreeMap<PeerId, Client>,
    /// Per slot: the tick its record was last written into `records`, and the record.
    record_tick: Vec<u32>,
    records: Vec<Vec<FieldValue>>,
    record_hashes: Vec<u64>,
    /// Reused per send: candidate slots.
    candidates: Vec<u32>,
}

impl<K: ExternalKey> ExternalReplication<K> {
    /// A replication for `schema` (the game's record layout) with `config`.
    pub fn new(schema: SnapshotSchema, config: ExternalConfig) -> Result<Self, ExternalError> {
        config.validate()?;
        let classes = if config.classes.is_empty() {
            vec![ExternalClass::default()]
        } else {
            config.classes.clone()
        };
        let mut grid = InterestGrid::default();
        grid.reset(config.cell_m);
        Ok(Self {
            config,
            classes,
            schema,
            grid,
            rows: Vec::new(),
            slot_of: FastMap::default(),
            free: Vec::new(),
            world_slots: BTreeSet::new(),
            clients: BTreeMap::new(),
            record_tick: Vec::new(),
            records: Vec::new(),
            record_hashes: Vec::new(),
            candidates: Vec::new(),
        })
    }

    /// The settings.
    pub fn config(&self) -> &ExternalConfig {
        &self.config
    }

    /// The record schema.
    pub fn schema(&self) -> &SnapshotSchema {
        &self.schema
    }

    /// How many entities are indexed.
    pub fn entity_count(&self) -> usize {
        self.slot_of.len()
    }

    /// Adds an entity or moves it (and changes its class). Call it for every entity whose position
    /// or class changed since the last send.
    pub fn upsert(&mut self, key: K, position: Vec3, class: u16) -> Result<(), ExternalError> {
        if usize::from(class) >= self.classes.len() {
            return Err(ExternalError::UnknownClass {
                class,
                count: self.classes.len(),
            });
        }
        if !position.is_finite() {
            return Err(ExternalError::BadPosition);
        }
        let slot = match self.slot_of.get(&key) {
            Some(&slot) => slot,
            None => {
                let row = Row {
                    key,
                    pos: position,
                    class,
                    cell: None,
                    alive: true,
                };
                let slot = if let Some(slot) = self.free.pop() {
                    if let Some(r) = self.rows.get_mut(slot as usize) {
                        *r = row;
                    }
                    slot
                } else {
                    self.rows.push(row);
                    (self.rows.len() - 1) as u32
                };
                self.slot_of.insert(key, slot);
                self.index(slot);
                return Ok(());
            }
        };
        let Some(row) = self.rows.get_mut(slot as usize) else {
            return Ok(());
        };
        let class_changed = row.class != class;
        row.pos = position;
        row.class = class;
        if class_changed {
            self.unindex(slot);
            self.index(slot);
            return Ok(());
        }
        let Some(row) = self.rows.get(slot as usize) else {
            return Ok(());
        };
        if let Some((band, cell)) = row.cell {
            let now = self.grid.cell_of(position);
            if now != cell {
                self.grid.remove_at((band, cell), slot);
                self.grid.insert_at((band, now), slot);
                if let Some(row) = self.rows.get_mut(slot as usize) {
                    row.cell = Some((band, now));
                }
            }
        }
        Ok(())
    }

    /// Removes an entity; each client that holds it gets a remove record. Returns whether it existed.
    pub fn remove(&mut self, key: K) -> bool {
        let Some(slot) = self.slot_of.remove(&key) else {
            return false;
        };
        self.unindex(slot);
        if let Some(row) = self.rows.get_mut(slot as usize) {
            row.alive = false;
        }
        // The slot will be reused: no client may keep this entity's accumulator or tier state.
        for client in self.clients.values_mut() {
            client.held.remove(&slot);
        }
        self.free.push(slot);
        true
    }

    fn index(&mut self, slot: u32) {
        let Some(row) = self.rows.get(slot as usize) else {
            return;
        };
        let class = self.classes.get(usize::from(row.class)).map(|c| c.relevance);
        match class {
            Some(RelevanceShape::Spatial { radius, .. }) => {
                let at = (self.grid.band_of(radius), self.grid.cell_of(row.pos));
                self.grid.insert_at(at, slot);
                if let Some(row) = self.rows.get_mut(slot as usize) {
                    row.cell = Some(at);
                }
            }
            Some(RelevanceShape::World) | None => {
                self.world_slots.insert(slot);
            }
        }
    }

    fn unindex(&mut self, slot: u32) {
        let Some(row) = self.rows.get_mut(slot as usize) else {
            return;
        };
        if let Some(at) = row.cell.take() {
            self.grid.remove_at(at, slot);
        }
        self.world_slots.remove(&slot);
    }

    /// Adds a client. Its first packet is a full snapshot of what is relevant to it.
    pub fn add_client(&mut self, peer: PeerId) -> Result<(), ExternalError> {
        if self.clients.contains_key(&peer) {
            return Err(ExternalError::DuplicateClient(peer));
        }
        self.clients.insert(
            peer,
            Client {
                encoder: SnapshotEncoder::with_capacity(self.schema.clone(), self.config.baseline_capacity),
                ack: None,
                viewpoints: Vec::new(),
                declared_rate: None,
                allowance: None,
                carry: 0.0,
                held: FastMap::default(),
                receipts: Receipts::default(),
                counters: ExternalClientCounters::default(),
                world: WorldSnapshot::default(),
                ids: Vec::new(),
                defer: Vec::new(),
                force: Vec::new(),
            },
        );
        Ok(())
    }

    /// Removes a client. Returns whether it existed.
    pub fn remove_client(&mut self, peer: PeerId) -> bool {
        self.clients.remove(&peer).is_some()
    }

    /// The clients, in id order.
    pub fn clients(&self) -> impl Iterator<Item = PeerId> + '_ {
        self.clients.keys().copied()
    }

    fn client_mut(&mut self, peer: PeerId) -> Result<&mut Client, ExternalError> {
        self.clients
            .get_mut(&peer)
            .ok_or(ExternalError::UnknownClient(peer))
    }

    /// Sets a client's viewpoints (its camera, its avatar, a spectated player). Without one, only
    /// world-relevant entities are relevant to it.
    pub fn set_viewpoints(&mut self, peer: PeerId, viewpoints: &[Vec3]) -> Result<(), ExternalError> {
        let client = self.client_mut(peer)?;
        client.viewpoints.clear();
        client
            .viewpoints
            .extend(viewpoints.iter().copied().filter(|v| v.is_finite()));
        Ok(())
    }

    /// The rate the client declared, bytes per second: it lowers the project's rate (not below
    /// [`MIN_CLIENT_RATE`]), never raises it. `None` clears it.
    pub fn set_declared_rate(&mut self, peer: PeerId, rate: Option<u32>) -> Result<(), ExternalError> {
        self.client_mut(peer)?.declared_rate = rate;
        Ok(())
    }

    /// The congestion controller's allowance for the client, bytes per second (spec 120 § 4.2 item
    /// 9): the budget is `min(project rate, declared rate, allowance)`, not below
    /// [`MIN_CLIENT_RATE`]. `None` (the default) leaves the static rate.
    pub fn set_allowance(&mut self, peer: PeerId, allowance: Option<u32>) -> Result<(), ExternalError> {
        self.client_mut(peer)?.allowance = allowance;
        Ok(())
    }

    /// Takes an acknowledgement (the client's [`ExternalInterpolator::ack`]). Its applied packets
    /// always count; its baseline is ignored (and counted) when older than the newest one taken or
    /// no longer in the encoder's ring.
    pub fn ack(&mut self, peer: PeerId, ack: ExternalAck) -> Result<(), ExternalError> {
        let client = self.client_mut(peer)?;
        client.receipts.take(&ack);
        let seq = ack.baseline;
        let newer = client.ack.is_none_or(|held| seq_newer(seq.0, held.0));
        if newer && client.encoder.has_baseline(seq) {
            client.ack = Some(seq);
            client.counters.acks += 1;
        } else {
            client.counters.stale_acks += 1;
        }
        Ok(())
    }

    /// A client's counters.
    pub fn counters(&self, peer: PeerId) -> Option<ExternalClientCounters> {
        self.clients.get(&peer).map(|c| c.counters)
    }

    /// The keys a client holds (relevant to it at the last send), sorted.
    pub fn relevant(&self, peer: PeerId) -> Vec<K> {
        let Some(client) = self.clients.get(&peer) else {
            return Vec::new();
        };
        let mut keys: Vec<K> = client
            .held
            .keys()
            .filter_map(|slot| self.rows.get(*slot as usize).filter(|r| r.alive).map(|r| r.key))
            .collect();
        keys.sort_unstable();
        keys
    }

    /// The tier the client's last send put an entity in (`None` if the client does not hold it).
    pub fn tier_of(&self, peer: PeerId, key: K) -> Option<RateTier> {
        let slot = self.slot_of.get(&key)?;
        self.clients.get(&peer)?.held.get(slot).map(|h| h.tier)
    }

    /// The stats snapshot for hosts and agents.
    pub fn stats(&self) -> ExternalStats {
        ExternalStats {
            entities: self.slot_of.len(),
            band_radii_m: self.grid.band_radii().collect(),
            clients: self
                .clients
                .iter()
                .map(|(peer, c)| ExternalClientStats {
                    peer: peer.0,
                    rate: c.rate(self.config.rate),
                    ack: c.ack.map(|s| s.0),
                    counters: c.counters,
                })
                .collect(),
        }
    }

    /// Publishes [`Self::stats`] as `label`'s newest snapshot in `into`, taken at `tick`
    /// (`net.external_stats`, ADR-8892); see [`NetExternalStats::publish`] for the label cap.
    pub fn publish_stats(&self, label: &str, tick: NetTick, into: &mut NetExternalStats) -> bool {
        into.publish(label, tick, self.stats())
    }

    /// Runs one tick: on a send tick (`tick % send_every_ticks == 0`) encodes one packet per client
    /// into `out` (appended); on other ticks does nothing. A client whose budget cannot hold a
    /// packet header this tick gets no packet (its budget carries).
    pub fn send_tick<S: ExternalSource<K>>(
        &mut self,
        tick: NetTick,
        source: &S,
        out: &mut Vec<ExternalPacket>,
    ) -> Result<(), ExternalError> {
        if !tick.0.is_multiple_of(self.config.send_every_ticks) {
            return Ok(());
        }
        let _span = tracing::trace_span!("regia_net::external_send", clients = self.clients.len()).entered();
        if self.record_tick.len() < self.rows.len() {
            self.record_tick.resize(self.rows.len(), u32::MAX);
            self.records.resize_with(self.rows.len(), Vec::new);
            self.record_hashes.resize(self.rows.len(), 0);
        }
        let send_hz = self.config.tick_hz / f64::from(self.config.send_every_ticks);
        let peers: Vec<PeerId> = self.clients.keys().copied().collect();
        for peer in peers {
            self.send_client(tick, peer, send_hz, source, out)?;
        }
        Ok(())
    }

    fn send_client<S: ExternalSource<K>>(
        &mut self,
        tick: NetTick,
        peer: PeerId,
        send_hz: f64,
        source: &S,
        out: &mut Vec<ExternalPacket>,
    ) -> Result<(), ExternalError> {
        let Some(mut client) = self.clients.remove(&peer) else {
            return Ok(());
        };
        let result = self.pack_client(tick, peer, send_hz, source, &mut client, out);
        self.clients.insert(peer, client);
        result
    }

    fn pack_client<S: ExternalSource<K>>(
        &mut self,
        tick: NetTick,
        peer: PeerId,
        send_hz: f64,
        source: &S,
        client: &mut Client,
        out: &mut Vec<ExternalPacket>,
    ) -> Result<(), ExternalError> {
        // Candidates: the grid around each viewpoint, what the client holds (to test it against
        // the widened radius), and the world-relevant entities.
        let mut cand = std::mem::take(&mut self.candidates);
        cand.clear();
        for vp in &client.viewpoints {
            self.grid.query_bands(*vp, |s| cand.push(s));
        }
        cand.extend(client.held.keys().copied());
        cand.extend(self.world_slots.iter().copied());
        cand.sort_unstable();
        cand.dedup();

        let mut used = 0usize;
        client.ids.clear();
        client.defer.clear();
        client.force.clear();
        for &slot in &cand {
            let Some(row) = self.rows.get(slot as usize).filter(|r| r.alive) else {
                client.held.remove(&slot);
                continue;
            };
            let Some(class) = self.classes.get(usize::from(row.class)) else {
                continue;
            };
            let distance = client
                .viewpoints
                .iter()
                .map(|vp| row.pos.distance(*vp))
                .min_by(f32::total_cmp);
            let was_in = client.held.contains_key(&slot);
            let radius = match class.relevance {
                RelevanceShape::Spatial { radius, hysteresis } => {
                    if !distance.is_some_and(|d| d <= spatial_limit(radius, hysteresis, was_in)) {
                        if was_in {
                            client.held.remove(&slot);
                        }
                        continue;
                    }
                    Some(radius)
                }
                RelevanceShape::World => None,
            };
            let game = source.priority(row.key, peer, distance);
            let game = if game.is_finite() { game.max(0.0) } else { 0.0 };
            let gain = game * priority_gain(class.priority, radius, distance, false, true, false, 1.0);
            let tier = match (&class.rate_tiers, distance) {
                (Some(t), Some(d)) => t.tier(d),
                _ => RateTier::Near,
            };
            // The record, written once per entity per send tick.
            let (Some(stamp), Some(rec), Some(hash)) = (
                self.record_tick.get_mut(slot as usize),
                self.records.get_mut(slot as usize),
                self.record_hashes.get_mut(slot as usize),
            ) else {
                continue;
            };
            if *stamp != tick.0 {
                rec.clear();
                source.record(row.key, rec);
                *hash = record_hash(&self.schema, rec);
                *stamp = tick.0;
            }
            let held = client.held.entry(slot).or_insert(Held {
                acc: 0.0,
                last_sent: None,
                sent: None,
                tier,
            });
            held.acc = (held.acc + gain).min(f32::MAX);
            held.tier = tier;
            let since = held.last_sent.map(|t| tick.0.wrapping_sub(t));
            let forced = class
                .max_interval_ticks
                .is_some_and(|m| since.is_none_or(|d| d >= m));
            let id = row.key.net_id();
            // Offer or defer (see the module docs).
            let offer = match held.sent {
                _ if forced => true,
                None => true,
                Some((seq, sent_hash)) if sent_hash == *hash => {
                    client.receipts.delivery(seq) == Delivery::Lost
                }
                Some(_) => {
                    let waits = match (&class.rate_tiers, since) {
                        (Some(tiers), Some(since)) => {
                            since
                                < tiers.interval_ticks(
                                    tier,
                                    self.config.send_every_ticks,
                                    self.config.tick_hz,
                                )
                        }
                        _ => false,
                    };
                    client.counters.tier_held += u64::from(waits);
                    !waits
                }
            };
            if offer {
                client.force.push(id);
            } else {
                client.defer.push(id);
            }
            // The codec packs by descending priority: the accumulator's bits order like the
            // accumulator for non-negative values; forced records rank first.
            let priority = if forced {
                u32::MAX
            } else {
                held.acc.max(0.0).to_bits().min(u32::MAX - 1)
            };
            if let Some(e) = client.world.entities.get_mut(used) {
                e.id = id;
                e.priority = priority;
                e.fields.clear();
                e.fields.extend_from_slice(rec);
            } else {
                client.world.entities.push(EntitySnapshot {
                    id,
                    priority,
                    fields: rec.clone(),
                });
            }
            used += 1;
            client.ids.push((id, slot));
        }
        client.world.entities.truncate(used);
        self.candidates = cand;
        client.ids.sort_unstable();
        client.defer.sort_unstable();
        client.force.sort_unstable();

        let rate = client.rate(self.config.rate);
        let budget = tick_budget(rate, send_hz, 1.0, client.carry, 0).bytes;
        let packet_budget = (budget.max(0.0).floor() as usize).min(self.config.max_packet_bytes);
        client.counters.last_budget = packet_budget as u32;
        client.counters.relevant = client.held.len() as u32;
        let encoded = client.encoder.encode_selecting(
            tick,
            &client.world,
            client.ack,
            packet_budget,
            RecordSelection {
                defer: &client.defer,
                force: &client.force,
            },
        );
        let (bytes, report) = match encoded {
            Ok(done) => done,
            Err(SnapshotError::BudgetTooSmall { .. }) => {
                client.carry = budget;
                client.counters.budget_skips += 1;
                client.counters.last_bytes = 0;
                return Ok(());
            }
            Err(error) => return Err(ExternalError::Encode { peer, error }),
        };
        client.carry = budget - bytes.len() as f64;
        let c = &mut client.counters;
        c.packets += 1;
        c.bytes += bytes.len() as u64;
        c.last_bytes = bytes.len() as u32;
        c.full_packets += u64::from(report.is_full);
        c.records += report.included.len() as u64;
        c.omitted += report.omitted.len() as u64;
        for id in &report.included {
            let Ok(at) = client.ids.binary_search_by_key(id, |(i, _)| *i) else {
                // A remove record.
                continue;
            };
            let Some(&(_, slot)) = client.ids.get(at) else {
                continue;
            };
            if let Some(held) = client.held.get_mut(&slot) {
                held.acc = 0.0;
                held.last_sent = Some(tick.0);
                held.sent = self.record_hashes.get(slot as usize).map(|h| (report.seq, *h));
                if let Some(n) = c.tier_sent.get_mut(held.tier as usize) {
                    *n += 1;
                }
            }
        }
        out.push(ExternalPacket {
            peer,
            seq: report.seq,
            bytes,
        });
        Ok(())
    }
}

/// Settings of an [`ExternalInterpolator`].
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct InterpolatorConfig {
    /// How far behind the newest received tick playback runs, in ticks (covers jitter and a lost
    /// packet or two; about two send intervals plus the jitter).
    pub delay_ticks: f64,
    /// Samples kept per entity.
    pub max_samples: usize,
    /// How far past the newest received tick playback may run before it holds, in ticks.
    pub max_ahead_ticks: f64,
    /// A playback lag beyond this many ticks jumps forward instead of catching up.
    pub snap_ticks: f64,
}

impl Default for InterpolatorConfig {
    fn default() -> Self {
        Self {
            delay_ticks: 6.0,
            max_samples: 8,
            max_ahead_ticks: 2.0,
            snap_ticks: 30.0,
        }
    }
}

/// Counters of an [`ExternalInterpolator`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct InterpolatorCounters {
    /// Packets decoded.
    pub packets: u64,
    /// Packets that failed to decode (dropped).
    pub decode_errors: u64,
    /// Packets older than the newest decoded one (kept as baselines, no samples).
    pub late: u64,
    /// Entity samples taken.
    pub samples: u64,
    /// Forward playback jumps (the lag passed `snap_ticks`).
    pub snaps: u64,
}

/// What one received packet changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExternalReceived {
    /// The packet's sequence.
    pub seq: Seq,
    /// The packet's tick.
    pub tick: NetTick,
    /// Entities added.
    pub added: usize,
    /// Entities updated.
    pub updated: usize,
    /// Entities removed.
    pub removed: usize,
    /// The packet was older than the newest decoded one.
    pub late: bool,
}

/// The received states of one entity, oldest first.
#[derive(Debug, Clone, Default)]
struct Track {
    samples: VecDeque<(u32, Vec<FieldValue>)>,
}

/// The client side of [`ExternalReplication`] (spec 120 § 8.6): decodes packets, names the
/// acknowledgement to send back, and interpolates each entity between its received states.
///
/// Only the records a packet carries become samples (an entity the server deferred or left out of
/// a packet keeps its last sample), so the shown state never steps back to an older baseline.
/// Playback runs `delay_ticks` behind the newest received tick and never moves backward: it
/// catches up 5 % faster or slower than real time and jumps forward only past `snap_ticks`.
#[derive(Debug)]
pub struct ExternalInterpolator<K: ExternalKey> {
    decoder: SnapshotDecoder,
    config: InterpolatorConfig,
    tracks: BTreeMap<NetId, Track>,
    newest: Option<u32>,
    playback: Option<f64>,
    /// The newest packet applied, and bit `i`: packet `applied - 1 - i` was applied too.
    applied: Option<(Seq, u64)>,
    counters: InterpolatorCounters,
    _key: PhantomData<K>,
}

impl<K: ExternalKey> ExternalInterpolator<K> {
    /// An interpolator for `schema` (the server's).
    pub fn new(schema: SnapshotSchema, config: InterpolatorConfig) -> Self {
        Self {
            decoder: SnapshotDecoder::new(schema),
            config,
            tracks: BTreeMap::new(),
            newest: None,
            playback: None,
            applied: None,
            counters: InterpolatorCounters::default(),
            _key: PhantomData,
        }
    }

    /// Decodes one packet. A packet that fails to decode is dropped and counted; the stream
    /// recovers on a later packet.
    pub fn receive(&mut self, packet: &[u8]) -> Result<ExternalReceived, ExternalError> {
        let _span = tracing::trace_span!("regia_net::external_receive").entered();
        let decoded = match self.decoder.decode(packet) {
            Ok(d) => d,
            Err(e) => {
                self.counters.decode_errors += 1;
                return Err(ExternalError::Decode(e));
            }
        };
        self.counters.packets += 1;
        let late = self.newest.is_some_and(|n| decoded.tick.0 < n)
            || (decoded.added.is_empty()
                && decoded.updated.is_empty()
                && decoded.removed.is_empty()
                && self.decoder.last_seq() != Some(decoded.seq));
        if late {
            self.counters.late += 1;
        }
        let entities = &decoded.snapshot.entities;
        for id in decoded.added.iter().chain(&decoded.updated) {
            let Ok(at) = entities.binary_search_by_key(id, |e| e.id) else {
                continue;
            };
            let Some(entity) = entities.get(at) else {
                continue;
            };
            let track = self.tracks.entry(*id).or_default();
            if track.samples.back().is_some_and(|(t, _)| *t >= decoded.tick.0) {
                continue;
            }
            track.samples.push_back((decoded.tick.0, entity.fields.clone()));
            while track.samples.len() > self.config.max_samples.max(2) {
                track.samples.pop_front();
            }
            self.counters.samples += 1;
        }
        for id in &decoded.removed {
            self.tracks.remove(id);
        }
        if !late {
            self.newest = Some(self.newest.map_or(decoded.tick.0, |n| n.max(decoded.tick.0)));
            let seq = decoded.seq;
            self.applied = Some(match self.applied {
                None => (seq, 0),
                Some((newest, bits)) if seq_newer(seq.0, newest.0) => {
                    let by = seq.0.wrapping_sub(newest.0);
                    let moved = if by > 64 {
                        0
                    } else {
                        ((bits << (by - 1)) << 1) | (1 << (by - 1))
                    };
                    (seq, moved)
                }
                Some(held) => held,
            });
        }
        Ok(ExternalReceived {
            seq: decoded.seq,
            tick: decoded.tick,
            added: decoded.added.len(),
            updated: decoded.updated.len(),
            removed: decoded.removed.len(),
            late,
        })
    }

    /// The acknowledgement to send to the server after each received packet (or each tick): the
    /// newest baseline held and the recent packets applied. `None` before the first packet.
    pub fn ack(&self) -> Option<ExternalAck> {
        let baseline = self.decoder.latest_baseline()?;
        let (applied, applied_bits) = self.applied?;
        Some(ExternalAck {
            baseline,
            applied,
            applied_bits,
        })
    }

    /// The codec's reconstructed world for the newest packet (quantized exactly as sent). An entity
    /// the server deferred shows its acknowledged-baseline state here, which may be older than what
    /// the client applied: read [`ExternalInterpolator::latest`] or
    /// [`ExternalInterpolator::sample`] for what to show.
    pub fn current(&self) -> Option<&WorldSnapshot> {
        self.decoder.current()
    }

    /// Writes `key`'s newest applied state into `out` (cleared first) and returns its tick, or
    /// `None` if the client does not hold `key`.
    pub fn latest(&self, key: K, out: &mut Vec<FieldValue>) -> Option<NetTick> {
        out.clear();
        let (tick, fields) = self.tracks.get(&key.net_id())?.samples.back()?;
        out.extend_from_slice(fields);
        Some(NetTick(*tick))
    }

    /// The newest received tick.
    pub fn newest_tick(&self) -> Option<NetTick> {
        self.newest.map(NetTick)
    }

    /// The playback tick (fractional), once a packet arrived.
    pub fn playback_tick(&self) -> Option<f64> {
        self.playback
    }

    /// The counters.
    pub fn counters(&self) -> InterpolatorCounters {
        self.counters
    }

    /// Advances playback by `ticks` of local time. Never moves it backward.
    pub fn advance(&mut self, ticks: f64) {
        let Some(newest) = self.newest else {
            return;
        };
        let newest = f64::from(newest);
        let target = newest - self.config.delay_ticks;
        let cap = newest + self.config.max_ahead_ticks;
        let step = if ticks.is_finite() { ticks.max(0.0) } else { 0.0 };
        let next = match self.playback {
            None => target,
            Some(p) => {
                let lag = target - p;
                if lag > self.config.snap_ticks {
                    self.counters.snaps += 1;
                    target
                } else {
                    let rate = if lag > 1.0 {
                        1.05
                    } else if lag < -1.0 {
                        0.95
                    } else {
                        1.0
                    };
                    (p + step * rate).min(cap).max(p)
                }
            }
        };
        self.playback = Some(next);
    }

    /// The keys of the entities the client holds, sorted.
    pub fn keys(&self) -> impl Iterator<Item = K> + '_ {
        self.tracks.keys().filter_map(|id| K::from_net_id(*id))
    }

    /// Writes `key`'s state at the playback tick into `out` (cleared first): floats, vectors and
    /// rotations blend between the two samples around the playback tick, other fields take the
    /// older sample's value. Before the first sample it shows the first, after the last it holds
    /// the last. Returns `false` (and leaves `out` empty) if the client does not hold `key`.
    pub fn sample(&self, key: K, out: &mut Vec<FieldValue>) -> bool {
        out.clear();
        let Some(track) = self.tracks.get(&key.net_id()) else {
            return false;
        };
        let t = self.playback.unwrap_or(f64::MAX);
        let after = track.samples.iter().position(|(tick, _)| f64::from(*tick) > t);
        match after {
            Some(0) => {
                if let Some((_, f)) = track.samples.front() {
                    out.extend_from_slice(f);
                }
            }
            None => {
                if let Some((_, f)) = track.samples.back() {
                    out.extend_from_slice(f);
                }
            }
            Some(i) => {
                let (Some((ta, a)), Some((tb, b))) = (track.samples.get(i - 1), track.samples.get(i)) else {
                    return false;
                };
                let span = f64::from(tb.wrapping_sub(*ta)).max(1.0);
                let s = ((t - f64::from(*ta)) / span).clamp(0.0, 1.0) as f32;
                out.extend(a.iter().zip(b).map(|(x, y)| blend(*x, *y, s)));
            }
        }
        true
    }
}

/// One field between two samples at `s` in `0..=1`.
fn blend(a: FieldValue, b: FieldValue, s: f32) -> FieldValue {
    match (a, b) {
        (FieldValue::F32(x), FieldValue::F32(y)) => FieldValue::F32(x + (y - x) * s),
        (FieldValue::Vec3(x), FieldValue::Vec3(y)) => FieldValue::Vec3(x.lerp(y, s)),
        (FieldValue::Quat(x), FieldValue::Quat(y)) => FieldValue::Quat(x.slerp(y, s)),
        (x, _) => x,
    }
}

#[cfg(test)]
mod tests;
