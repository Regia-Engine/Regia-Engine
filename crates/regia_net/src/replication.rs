// SPDX-License-Identifier: MPL-2.0
//! Entity replication (spec 71): the `Replicated` marker, registering components, spawn and
//! despawn, level-bound entities, hierarchy, entity references, audiences, change notices, sleep,
//! `NetVec` and the transform fast path with teleports.
//!
//! The server's world holds the truth. For every entity with [`Replicated`], each connection gets
//! a copy: it appears with a spawn record, follows field deltas of the registered components and
//! disappears with a despawn record. Two carriers (ADR-1950): `state_channel`, unreliable frames
//! with deltas against acknowledged baselines (protocol 2), and the reliable-ordered tick stream of
//! `crate::runtime` (protocol 1), where a delta is taken against the last value sent.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use glam::{Quat, Vec3};
use regia_ecs::ecs::change_detection::Tick;
use regia_ecs::ecs::component::ComponentId;
use regia_ecs::ecs::lifecycle::HookContext;
use regia_ecs::ecs::world::DeferredWorld;
use regia_ecs::prelude::*;
use regia_scene::components::EntityGuid;
use regia_scene::level::LevelMember;
use regia_scene::prefab::PrefabInstance;
use regia_scene::streaming::StreamedReplicated;
use regia_scene::transform::{GlobalTransform, Transform};

use crate::bits::{BitReader, BitWriter, DecodeError};
use crate::codec::{CodecContext, EntityMap, LayoutHasher, ReplicateCodec};
use crate::dense::{FastMap, NO_SLOT, SlotBits, SlotTable};
use crate::model::{NetId, NetOwnedLocally, NetRole, NetTick, OwnerCache, PeerId};
use crate::profile::{NetProfile, RelevanceShape};
use crate::recorder::RecorderView;
use crate::relevance::{InterestEntity, NetInterest, PeerInterest};
use crate::runtime::{NetEntities, NetNode};
use crate::send::{Packer, priority_gain, rank_key, tick_budget};

pub mod relevance_view;
mod state_channel;
mod transform_record;

pub use state_channel::MAX_CLIENT_ENTITIES;
pub use state_channel::STATE_BASE_REORDER_TICKS;
pub use state_channel::{
    ClientStateChannel, ClientStateCounters, ConnWire, IN_FLIGHT_TIMEOUT_TICKS, MIN_STATE_MTU_BYTES,
    STATE_PACKET_LOG, STATE_PACKET_RESERVE, STATE_VERSION_RING, StateAck, StateAcks, StateCounters,
    StateLink, StatePacket, TOMBSTONE_TICKS,
};
use state_channel::{KIND_FAR, KIND_REPEAT, KIND_SPAWN, KIND_UPDATE, OutRecord, SentRecord};
pub use state_channel::{MAX_IN_FLIGHT_TIMEOUT_TICKS, MAX_STATE_PACKET_LOG, OVERSIZE_TIMEOUT_FACTOR};
pub(crate) use state_channel::{client_apply_state_frame, read_state_header};
use transform_record::QTransform;
pub use transform_record::{
    DEFAULT_WIRE_STEP_M, FAR_RECORD_BITS, FAR_XZ_SHIFT, FAR_Y_SHIFT, POSITION_CELL_BITS, PREDICTION_TICK_HZ,
    TRANSFORM_RECORD_REVISION, TRANSFORM_RECORD_TYPE, VELOCITY_STEP_MPS,
};

// ---- components --------------------------------------------------------------------------------

/// How the transform fast path sends an entity's transform (spec 71 § 9).
#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransformReplication {
    /// No transform travels.
    None,
    /// Position only.
    PositionOnly,
    /// Position, rotation and velocity (when the entity has a `NetVelocity`).
    #[default]
    Movement,
    /// Movement plus scale.
    Full,
}

/// The sleep policy of a replicated entity (spec 71 § 8).
#[derive(Reflect, Default, Clone, Copy, Debug, PartialEq)]
pub enum NetSleep {
    /// Never sleeps.
    #[default]
    Never,
    /// Starts asleep once its spawn is acknowledged by every relevant connection; wakes on any
    /// replicated change (and sleeps again once that change is acknowledged).
    StartAsleep,
    /// Falls asleep after `quiet_ticks` ticks with no replicated change; wakes on any change.
    AfterQuiet {
        /// Quiet ticks before sleeping.
        quiet_ticks: u32,
    },
}

/// Opts an entity into replication. Reflected and saved in `.level.ron` / `.prefab.ron`.
#[derive(Component, Reflect, Default, Clone, Debug)]
#[reflect(Component, Default)]
#[component(on_remove = replicated_removed)]
pub struct Replicated {
    /// Relevance, priority and rate profile: the `.netprofile.ron` asset path (spec 72). `None`
    /// = the project default profile. A path string, not an `AssetRef`, because the component is
    /// reflected and `AssetRef` has no reflection yet (ADR-1643).
    pub profile: Option<String>,
    /// Transform fast path (§ 9).
    pub transform: TransformReplication,
    /// Sleep policy (§ 8).
    pub sleep: NetSleep,
}

/// Forces an entity asleep (server). Removing it, or any write to a replicated component, wakes it.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
pub struct NetAsleep;

/// Group names for `Audience::SharedGroup` (on replicated entities and on `Player` entities). A
/// connection's membership is the union over its peer's players.
#[derive(Component, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct NetGroups(pub Vec<String>);

/// Server: the owner of this entity predicts it (the spec 73 `Predicted` setting). The owning
/// client's copy gets `NetRole::Predicted` instead of `Proxy`.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
pub struct NetPredicted;

/// Server: the linear velocity the transform fast path sends with `Movement` and `Full` (written by
/// the game, or by the physics or character bridge). Clients receive it into the same component; a
/// record without one (a far record, a `PositionOnly` record, an entity that dropped it) removes it from a
/// proxy, while a copy the client owns or predicts keeps its own (ADR-10215).
#[derive(Component, Reflect, Default, Clone, Copy, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct NetVelocity(pub Vec3);

/// Server: multiplies the entity's priority gain (spec 72 § 6) on every connection, on top of its
/// profile's `priority`. The physics body bridge raises it with a body's speed (ADR-2902), so a
/// tumbling crate wins the budget over one that slides slowly. Values are clamped to
/// [`NET_PRIORITY_SCALE_MIN`, `NET_PRIORITY_SCALE_MAX`]; a non-finite value counts as 1. Not
/// replicated and not saved; writing it never wakes an asleep entity.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct NetPriorityScale(pub f32);

impl Default for NetPriorityScale {
    fn default() -> Self {
        Self(1.0)
    }
}

/// The smallest [`NetPriorityScale`] the gather uses.
pub const NET_PRIORITY_SCALE_MIN: f32 = 0.01;
/// The largest [`NetPriorityScale`] the gather uses.
pub const NET_PRIORITY_SCALE_MAX: f32 = 100.0;

impl NetPriorityScale {
    /// The factor the gather applies: clamped, and 1 for a non-finite value.
    pub fn factor(self) -> f32 {
        if self.0.is_finite() {
            self.0.clamp(NET_PRIORITY_SCALE_MIN, NET_PRIORITY_SCALE_MAX)
        } else {
            1.0
        }
    }
}

/// Combat heat, a game priority input (spec 120 § 8.3): recent damage an entity dealt or took.
/// Near a viewer it raises the entity's priority for that connection by
/// `1 + heat × max(0, 1 − distance / radius_m)`, so a fight close to a player wins the budget over
/// quiet entities at the same distance. The heat halves every [`NET_COMBAT_HEAT_HALF_LIFE_S`]
/// seconds from the tick it was last raised; [`net_note_combat`] raises it. Server only, not
/// replicated, not saved; it never wakes an asleep entity.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct NetCombatHeat {
    /// Heat at `at` (clamped to `0..=`[`NET_COMBAT_HEAT_MAX`] when read).
    pub heat: f32,
    /// The tick the heat was last raised.
    pub at: NetTick,
    /// The distance from a viewer at which the boost fades to nothing, in metres (> 0).
    pub radius_m: f32,
    /// Ticks for the heat to halve (>= 1); [`net_note_combat`] sets it from the tick rate.
    pub half_life_ticks: u32,
}

/// Seconds for combat heat to halve.
pub const NET_COMBAT_HEAT_HALF_LIFE_S: f64 = 2.0;
/// The largest combat heat the gather uses.
pub const NET_COMBAT_HEAT_MAX: f32 = 10.0;
/// The default reach of combat heat around a viewer, in metres.
pub const NET_COMBAT_HEAT_RADIUS_M: f32 = 50.0;

impl NetCombatHeat {
    /// The heat at `tick`: halved once per `half_life_ticks` since `at`, clamped to
    /// `0..=`[`NET_COMBAT_HEAT_MAX`]; 0 for a non-finite value.
    pub fn at_tick(&self, tick: NetTick) -> f32 {
        if !self.heat.is_finite() {
            return 0.0;
        }
        let age = tick.0.saturating_sub(self.at.0) as f32 / self.half_life_ticks.max(1) as f32;
        (self.heat.clamp(0.0, NET_COMBAT_HEAT_MAX) * 0.5f32.powf(age)).max(0.0)
    }

    /// The priority factor for a viewer at `distance` metres (`None`: no position, full boost).
    pub fn factor(heat: f32, radius_m: f32, distance: Option<f32>) -> f32 {
        if heat <= 0.0 {
            return 1.0;
        }
        let near = match distance {
            Some(d) if radius_m > 0.0 && radius_m.is_finite() => (1.0 - d / radius_m).max(0.0),
            Some(_) => 0.0,
            None => 1.0,
        };
        1.0 + heat * near
    }
}

/// Raises an entity's combat heat by `amount` (damage dealt or taken; 1.0 is one notable hit) at
/// the current server tick, after the old heat's decay, within [`NET_COMBAT_HEAT_RADIUS_M`] of a
/// viewer unless the entity already names its own radius. Returns false when the entity does not
/// exist or `amount` is not a positive finite number.
pub fn net_note_combat(world: &mut World, entity: Entity, amount: f32) -> bool {
    if !(amount.is_finite() && amount > 0.0) || world.get_entity(entity).is_err() {
        return false;
    }
    let tick = world
        .get_resource::<crate::runtime::NetNode>()
        .map_or(NetTick(0), |n| n.tick);
    let half_life_ticks =
        ((NET_COMBAT_HEAT_HALF_LIFE_S * crate::runtime::tick_hz(world)).round() as u32).max(1);
    let now = world.get::<NetCombatHeat>(entity).copied();
    let heat = NetCombatHeat {
        heat: (now.map_or(0.0, |h| h.at_tick(tick)) + amount).min(NET_COMBAT_HEAT_MAX),
        at: tick,
        radius_m: now.map_or(NET_COMBAT_HEAT_RADIUS_M, |h| h.radius_m),
        half_life_ticks,
    };
    world.entity_mut(entity).insert(heat);
    true
}

/// Client: a level-placed replicated copy whose bind has not arrived yet (shown with its level
/// state, role `Proxy`).
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
pub struct NetUnbound;

/// Client: a level-bound copy that is not relevant now; it keeps its last state.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
pub struct NetOutOfScope;

/// Client: the last transform update carried the teleport bit (proxies snap, spec 74).
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
pub struct NetTeleported;

// ---- events ------------------------------------------------------------------------------------

/// A replicated copy was created on this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetSpawned {
    /// The local copy.
    pub entity: Entity,
    /// Its id.
    pub net_id: NetId,
}

/// A replicated copy is about to be despawned on this client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetDespawning {
    /// The copy (still alive while handlers read this tick's events).
    pub entity: Entity,
}

/// A pending entity reference was patched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetRefResolved {
    /// The referring entity.
    pub entity: Entity,
    /// The component type's path.
    pub component: &'static str,
}

/// This tick's replication events on a client. Cleared at `NetSet::Apply`.
#[derive(Resource, Debug, Default)]
pub struct NetEvents {
    /// Copies created, in apply order.
    pub spawned: Vec<NetSpawned>,
    /// Copies despawned, in apply order (already gone from the world).
    pub despawning: Vec<NetDespawning>,
    /// References patched.
    pub refs_resolved: Vec<NetRefResolved>,
    /// Copies whose transform update carried the teleport bit.
    pub teleported: Vec<Entity>,
}

impl NetEvents {
    fn clear(&mut self) {
        self.spawned.clear();
        self.despawning.clear();
        self.refs_resolved.clear();
        self.teleported.clear();
    }
}

/// One change notice (spec 71 § 7).
#[derive(Debug, Clone)]
pub struct NetChanged<C> {
    /// The entity whose component changed.
    pub entity: Entity,
    /// The previous value (only with `keep_previous: true`).
    pub previous: Option<C>,
}

/// This tick's change notices of `C`. Cleared at `NetSet::Apply`.
#[derive(Resource, Debug)]
pub struct NetChangedEvents<C: Send + Sync + 'static> {
    items: Vec<NetChanged<C>>,
}

impl<C: Send + Sync + 'static> Default for NetChangedEvents<C> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

impl<C: Send + Sync + 'static> NetChangedEvents<C> {
    /// This tick's notices, in order.
    pub fn iter(&self) -> impl Iterator<Item = &NetChanged<C>> {
        self.items.iter()
    }
    /// How many.
    pub fn len(&self) -> usize {
        self.items.len()
    }
    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
}

// ---- registration ------------------------------------------------------------------------------

/// Which connections receive a component's values (spec 71 § 6).
#[derive(Clone, Copy)]
pub enum Audience {
    /// Every connection the entity is relevant to.
    All,
    /// Only the owning connection.
    OwnerOnly,
    /// Every relevant connection except the owner.
    Others,
    /// The spawn record only; later changes are never sent.
    SpawnOnly,
    /// Everyone in the spawn record, and only the owner afterwards.
    SpawnThenOwner,
    /// Connections whose copy is a `Proxy` (never a predicting owner).
    ProxyCopies,
    /// Connections where one of the peer's players shares a name in `NetGroups` with the entity.
    SharedGroup,
    /// Everyone relevant while the predicate returns true; evaluated once per entity that carries
    /// the component on every gather (not once per connection).
    Predicate(fn(&World, Entity) -> bool),
}

impl std::fmt::Debug for Audience {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Audience {
    /// Lowercase name (the protocol hash uses the variant only).
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::All => "all",
            Self::OwnerOnly => "owner_only",
            Self::Others => "others",
            Self::SpawnOnly => "spawn_only",
            Self::SpawnThenOwner => "spawn_then_owner",
            Self::ProxyCopies => "proxy_copies",
            Self::SharedGroup => "shared_group",
            Self::Predicate(_) => "predicate",
        }
    }
}

/// When a client emits `NetChanged<C>` (spec 71 § 7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NotifyWhen {
    /// When the value differs from the client's copy.
    #[default]
    Changed,
    /// On every received update, even if equal.
    EveryUpdate,
    /// Never.
    Never,
}

/// Per-component replication options.
#[derive(Debug, Clone, Copy)]
pub struct ReplicateOptions {
    /// Who receives the component (default `All`).
    pub audience: Audience,
    /// When clients get change notices (default `Changed`).
    pub notify: NotifyWhen,
    /// Keep the previous value in notices (default false).
    pub keep_previous: bool,
    /// Emit the same notices on the server for its own changes (default false).
    pub notify_on_server: bool,
}

impl Default for ReplicateOptions {
    fn default() -> Self {
        Self {
            audience: Audience::All,
            notify: NotifyWhen::Changed,
            keep_previous: false,
            notify_on_server: false,
        }
    }
}

/// A component value, type-erased.
/// Shared, so one snapshot per tick serves every connection's baseline.
pub type ErasedComp = Arc<dyn Any + Send + Sync>;

/// What a client apply of one component did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ApplyOutcome {
    unresolved: Vec<NetId>,
    referenced: Vec<NetId>,
}

/// Client apply of one component entry: `(world, copy, bits, context, full, spawn record)`.
type ApplyFn =
    fn(&mut World, Entity, &mut BitReader, &CodecContext, bool, bool) -> Result<ApplyOutcome, DecodeError>;

/// One registered component type, type-erased.
#[derive(Clone)]
pub struct RepComponent {
    /// The type's path.
    pub name: &'static str,
    /// Options it was registered with.
    pub options: ReplicateOptions,
    /// The codec layout hash.
    pub layout: u64,
    /// Values can hold entity references (from the codec's layout).
    pub has_refs: bool,
    id: Option<ComponentId>,
    snapshot: fn(&World, Entity) -> Option<ErasedComp>,
    changed: fn(&World, Entity, Tick, Tick) -> CompState,
    eq: fn(&ErasedComp, &ErasedComp, &CodecContext) -> bool,
    encode: fn(&ErasedComp, &mut BitWriter, &CodecContext),
    encode_delta: fn(&ErasedComp, &ErasedComp, &mut BitWriter, &CodecContext),
    apply: ApplyFn,
    decode_full: fn(&mut BitReader, &CodecContext) -> Result<ErasedComp, DecodeError>,
    decode_delta: fn(&ErasedComp, &mut BitReader, &CodecContext) -> Result<ErasedComp, DecodeError>,
    install: fn(&mut World, Entity, &ErasedComp, &CodecContext, bool),
    remove: fn(&mut World, Entity),
    has: fn(&World, Entity) -> bool,
    notify_server: fn(&mut World, Entity, Option<ErasedComp>),
    clear_events: fn(&mut World),
    to_debug: fn(&ErasedComp) -> String,
    received_debug: fn(&World, Entity) -> Option<String>,
}

impl std::fmt::Debug for RepComponent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RepComponent")
            .field("name", &self.name)
            .field("audience", &self.options.audience)
            .finish()
    }
}

fn snapshot_of<C: Component + ReplicateCodec>(world: &World, e: Entity) -> Option<ErasedComp> {
    world.get::<C>(e).map(|c| Arc::new(c.clone()) as ErasedComp)
}

/// A component's state at a gather.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompState {
    Absent,
    Unchanged,
    Changed,
}

fn state_of<C: Component>(world: &World, e: Entity, last: Tick, this: Tick) -> CompState {
    match world.get_entity(e).ok().and_then(|r| r.get_change_ticks::<C>()) {
        None => CompState::Absent,
        Some(t) if t.is_changed(last, this) || t.is_added(last, this) => CompState::Changed,
        Some(_) => CompState::Unchanged,
    }
}

/// Component ids the gather reads per entity, looked up once per gather.
struct GatherIds {
    child_of: Option<ComponentId>,
    replicated: Option<ComponentId>,
    transform: Option<ComponentId>,
    velocity: Option<ComponentId>,
    owner: Option<ComponentId>,
    owner_link: Option<ComponentId>,
    predicted: Option<ComponentId>,
    asleep: Option<ComponentId>,
    global: Option<ComponentId>,
    priority_scale: Option<ComponentId>,
    combat_heat: Option<ComponentId>,
    profile_ref: Option<ComponentId>,
}

/// An order-independent signature of every `NetOwner` and `NetOwnerLink` in the world.
pub(crate) fn owner_signature(world: &mut World) -> (u64, u64) {
    let mix = signature_mix;
    let mut owners = world.query::<(Entity, &crate::model::NetOwner)>();
    let a = owners.iter(world).fold(0u64, |acc, (e, o)| {
        acc.wrapping_add(mix(e.to_bits(), u64::from(o.0.0)))
    });
    let mut links = world.query::<(Entity, &crate::model::NetOwnerLink)>();
    let b = links.iter(world).fold(0u64, |acc, (e, l)| {
        acc.wrapping_add(mix(e.to_bits(), l.0.to_bits()))
    });
    (a, b)
}

/// `owner_signature` plus every `ChildOf` pair. Lag compensation invalidates its owner cache
/// from this. Gather still uses `owner_signature` alone, so a parent-only edit does not mark
/// every replicated owner dirty.
pub(crate) fn owner_record_signature(world: &mut World) -> (u64, u64) {
    let (a, b) = owner_signature(world);
    let mut parents = world.query::<(Entity, &ChildOf)>();
    // Each pair goes through the mixer before the sum. A raw `e ^ rotl(parent, 17)` sum is the
    // same after two children swap parents whenever their bits do not overlap (small entity
    // indices), so the owner cache kept the old owners (corr-net, ADR-5580).
    let c = parents.iter(world).fold(0u64, |acc, (e, p)| {
        acc.wrapping_add(signature_mix(e.to_bits(), p.0.to_bits() ^ 0x5bd1_e995_c0de_c41d))
    });
    (a ^ c.rotate_left(7), b.wrapping_add(c))
}

/// One `(entity, value)` pair of an order-independent signature: a SplitMix64 finalizer over
/// both, so a sum of pairs changes when two entities swap their values.
fn signature_mix(a: u64, b: u64) -> u64 {
    let mut z = a ^ b.rotate_left(29) ^ 0x9e37_79b9_7f4a_7c15;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z ^ (z >> 31)
}

fn has_parent(er: &EntityRef, child_of: Option<ComponentId>) -> bool {
    child_of.is_some_and(|id| er.contains_id(id))
}

impl GatherIds {
    fn new(world: &World) -> Self {
        Self {
            child_of: world.component_id::<ChildOf>(),
            replicated: world.component_id::<Replicated>(),
            transform: world.component_id::<Transform>(),
            velocity: world.component_id::<NetVelocity>(),
            owner: world.component_id::<crate::model::NetOwner>(),
            owner_link: world.component_id::<crate::model::NetOwnerLink>(),
            predicted: world.component_id::<NetPredicted>(),
            asleep: world.component_id::<NetAsleep>(),
            global: world.component_id::<GlobalTransform>(),
            priority_scale: world.component_id::<NetPriorityScale>(),
            combat_heat: world.component_id::<NetCombatHeat>(),
            profile_ref: world.component_id::<crate::profile::NetProfileRef>(),
        }
    }
}

fn ticks_state(er: &EntityRef, id: Option<ComponentId>, last: Tick, this: Tick) -> CompState {
    match id.and_then(|id| er.get_change_ticks_by_id(id)) {
        None => CompState::Absent,
        Some(t) if t.is_changed(last, this) || t.is_added(last, this) => CompState::Changed,
        Some(_) => CompState::Unchanged,
    }
}

fn eq_of<C: ReplicateCodec + 'static>(a: &ErasedComp, b: &ErasedComp, cx: &CodecContext) -> bool {
    match (a.downcast_ref::<C>(), b.downcast_ref::<C>()) {
        (Some(a), Some(b)) => a.wire_eq(b, cx),
        _ => false,
    }
}

fn encode_of<C: ReplicateCodec + 'static>(v: &ErasedComp, w: &mut BitWriter, cx: &CodecContext) {
    if let Some(v) = v.downcast_ref::<C>() {
        v.encode(w, cx);
    }
}

fn encode_delta_of<C: ReplicateCodec + 'static>(
    v: &ErasedComp,
    base: &ErasedComp,
    w: &mut BitWriter,
    cx: &CodecContext,
) {
    match (v.downcast_ref::<C>(), base.downcast_ref::<C>()) {
        (Some(v), Some(base)) => v.encode_delta(base, w, cx),
        (Some(v), None) => v.encode(w, cx),
        _ => {}
    }
}

fn debug_of<C: std::fmt::Debug + 'static>(v: &ErasedComp) -> String {
    v.downcast_ref::<C>()
        .map(|c| format!("{c:?}"))
        .unwrap_or_default()
}

fn received_debug_of<C: Component + std::fmt::Debug>(world: &World, e: Entity) -> Option<String> {
    world
        .get::<crate::interp::NetReceived<C>>(e)
        .map(|r| format!("{:?}", r.0))
}

/// Client apply: `full` decodes a whole value, otherwise a delta on top of the copy's value.
fn apply_of<C: Component + ReplicateCodec + std::fmt::Debug>(
    world: &mut World,
    e: Entity,
    r: &mut BitReader,
    cx: &CodecContext,
    full: bool,
    spawn: bool,
) -> Result<ApplyOutcome, DecodeError> {
    // An interpolated proxy (spec 74): deltas decode against the newest received value, which
    // may not be shown yet.
    let capture = world
        .get_resource::<ClientReplication>()
        .and_then(|c| c.step_capture)
        .filter(|(ce, _, _)| *ce == e);
    let received = match capture {
        Some(_) => world.get::<crate::interp::NetReceived<C>>(e).map(|r| r.0.clone()),
        None => None,
    };
    let current = received.or_else(|| world.get::<C>(e).cloned());
    let new = if full {
        C::decode(r, cx)?
    } else {
        match &current {
            Some(base) => C::decode_delta(base, r, cx)?,
            None => return Err(DecodeError::Invalid("delta for a component the copy lacks")),
        }
    };
    let stats = cx.take_stats();
    let outcome = ApplyOutcome {
        unresolved: stats.unresolved,
        referenced: stats.referenced,
    };
    arrive(world, e, new, cx, spawn, capture);
    Ok(outcome)
}

fn decode_full_of<C: Component + ReplicateCodec>(
    r: &mut BitReader,
    cx: &CodecContext,
) -> Result<ErasedComp, DecodeError> {
    Ok(Arc::new(C::decode(r, cx)?))
}

fn decode_delta_of<C: Component + ReplicateCodec>(
    base: &ErasedComp,
    r: &mut BitReader,
    cx: &CodecContext,
) -> Result<ErasedComp, DecodeError> {
    let base = base
        .downcast_ref::<C>()
        .ok_or(DecodeError::Invalid("baseline of another component"))?;
    Ok(Arc::new(C::decode_delta(base, r, cx)?))
}

/// Client: a decoded value of a state version reaches the copy (protocol 2).
fn install_of<C: Component + ReplicateCodec + std::fmt::Debug>(
    world: &mut World,
    e: Entity,
    value: &ErasedComp,
    cx: &CodecContext,
    spawn: bool,
) {
    let capture = world
        .get_resource::<ClientReplication>()
        .and_then(|c| c.step_capture)
        .filter(|(ce, _, _)| *ce == e);
    if let Some(v) = value.downcast_ref::<C>() {
        arrive(world, e, v.clone(), cx, spawn, capture);
    }
}

/// A received value reaches a copy: shown now, or through the interpolation buffers for an
/// interpolated proxy (`capture`).
fn arrive<C: Component + ReplicateCodec + std::fmt::Debug>(
    world: &mut World,
    e: Entity,
    new: C,
    cx: &CodecContext,
    spawn: bool,
    capture: Option<(Entity, u64, bool)>,
) {
    if let Some((_, tick, spawn_record)) = capture {
        if let Ok(mut ent) = world.get_entity_mut(e) {
            ent.insert(crate::interp::NetReceived(new.clone()));
        }
        let blended = crate::interp::is_interpolated_type::<C>(world);
        if blended {
            crate::interp::push_interp_value(world, e, tick, new.clone());
        }
        if spawn_record {
            install_value::<C>(world, e, new, spawn, cx);
        } else if !blended {
            // A step component: shown when the interpolation tick reaches its tick.
            crate::interp::queue_step(
                world,
                e,
                tick,
                Box::new(move |w: &mut World, e: Entity| {
                    if w.get::<crate::interp::NetReceived<C>>(e).is_some() {
                        install_value::<C>(w, e, new, false, &CodecContext::detached());
                    }
                }),
            );
        }
        return;
    }
    // No longer an interpolated proxy (owned now, or its profile changed): the value is shown on
    // arrival and the interpolation copy is stale.
    if world.get::<crate::interp::NetReceived<C>>(e).is_some()
        && let Ok(mut ent) = world.get_entity_mut(e)
    {
        ent.remove::<crate::interp::NetReceived<C>>();
    }
    install_value::<C>(world, e, new, spawn, cx);
}

/// Inserts a received value on a client copy and emits its `NetChanged` notice.
fn install_value<C: Component + ReplicateCodec + std::fmt::Debug>(
    world: &mut World,
    e: Entity,
    mut new: C,
    spawn: bool,
    cx: &CodecContext,
) {
    let current = world.get::<C>(e).cloned();
    // Fields that never travel keep the copy's own value (spec 71 § 3); a first spawn has none.
    if let Some(local) = &current {
        new.keep_local(local);
    }
    let opts = world
        .get_resource::<ReplicationRegistry>()
        .and_then(|reg| reg.get_by_type(TypeId::of::<C>()))
        .map(|c| c.options)
        .unwrap_or_default();
    let differs = match &current {
        Some(old) => !old.wire_eq(&new, &cx.scratch()),
        None => true,
    };
    let notify = spawn
        || match opts.notify {
            NotifyWhen::Changed => differs,
            NotifyWhen::EveryUpdate => true,
            NotifyWhen::Never => false,
        };
    let previous = if opts.keep_previous && !spawn {
        current
    } else {
        None
    };
    // `insert` replaces in place and also serves immutable components.
    if let Ok(mut ent) = world.get_entity_mut(e) {
        ent.insert(new);
    }
    if notify && opts.notify != NotifyWhen::Never {
        world
            .get_resource_or_insert_with(NetChangedEvents::<C>::default)
            .items
            .push(NetChanged { entity: e, previous });
    }
}

fn has_of<C: Component>(world: &World, e: Entity) -> bool {
    world.get::<C>(e).is_some()
}

fn remove_of<C: Component>(world: &mut World, e: Entity) {
    if let Ok(mut ent) = world.get_entity_mut(e) {
        // Also the interpolation copies (spec 74), so queued step values of it are dropped.
        ent.remove::<(
            C,
            crate::interp::NetReceived<C>,
            crate::interp::NetInterpValues<C>,
        )>();
    }
}

fn notify_server_of<C: Component + ReplicateCodec>(
    world: &mut World,
    e: Entity,
    previous: Option<ErasedComp>,
) {
    let previous = previous
        .and_then(|p| p.downcast::<C>().ok())
        .map(|a| (*a).clone());
    world
        .get_resource_or_insert_with(NetChangedEvents::<C>::default)
        .items
        .push(NetChanged { entity: e, previous });
}

fn clear_events_of<C: Component>(world: &mut World) {
    if let Some(mut ev) = world.get_resource_mut::<NetChangedEvents<C>>() {
        ev.items.clear();
    }
}

/// The registered component types, in registration order.
#[derive(Resource, Debug, Default, Clone)]
pub struct ReplicationRegistry {
    comps: Vec<RepComponent>,
    by_type: HashMap<TypeId, u16>,
}

impl ReplicationRegistry {
    /// Registers `C` (a second registration is ignored).
    pub fn register<C: Component + ReplicateCodec + std::fmt::Debug>(
        &mut self,
        options: ReplicateOptions,
    ) -> u16 {
        if let Some(i) = self.by_type.get(&TypeId::of::<C>()) {
            return *i;
        }
        let mut h = LayoutHasher::new();
        C::layout(&mut h);
        let index = self.comps.len() as u16;
        self.comps.push(RepComponent {
            name: std::any::type_name::<C>(),
            options,
            has_refs: h.saw_entity(),
            layout: h.finish(),
            id: None,
            snapshot: snapshot_of::<C>,
            changed: state_of::<C>,
            eq: eq_of::<C>,
            encode: encode_of::<C>,
            encode_delta: encode_delta_of::<C>,
            apply: apply_of::<C>,
            decode_full: decode_full_of::<C>,
            decode_delta: decode_delta_of::<C>,
            install: install_of::<C>,
            remove: remove_of::<C>,
            has: has_of::<C>,
            notify_server: notify_server_of::<C>,
            clear_events: clear_events_of::<C>,
            to_debug: debug_of::<C>,
            received_debug: received_debug_of::<C>,
        });
        self.by_type.insert(TypeId::of::<C>(), index);
        index
    }

    /// Every registered component, in order.
    pub fn components(&self) -> &[RepComponent] {
        &self.comps
    }

    /// A component by index.
    pub fn get(&self, index: u16) -> Option<&RepComponent> {
        self.comps.get(usize::from(index))
    }

    fn get_by_type(&self, t: TypeId) -> Option<&RepComponent> {
        self.by_type.get(&t).and_then(|i| self.get(*i))
    }

    /// The index of a component by its path or short name (agent protocol).
    pub fn find(&self, name: &str) -> Option<u16> {
        self.comps
            .iter()
            .position(|c| c.name == name || c.name.rsplit("::").next() == Some(name))
            .map(|i| i as u16)
    }

    /// Adds the component list, order, audience variants and codecs to the protocol hash.
    pub fn hash_into(&self, h: &mut LayoutHasher) {
        h.write_str("components");
        h.write_u64(self.comps.len() as u64);
        for c in &self.comps {
            h.write_str(c.name);
            h.write_str(c.options.audience.as_str());
            h.write_u64(c.layout);
        }
    }

    /// One `(type path, fingerprint)` per registered component: the fields [`Self::hash_into`]
    /// feeds for that type (dev builds name the differing types in `protocol_mismatch`, spec 77 § 4).
    pub fn type_fingerprints(&self) -> Vec<(String, u64)> {
        self.comps
            .iter()
            .map(|c| {
                let mut h = LayoutHasher::new();
                h.write_str(c.options.audience.as_str());
                h.write_u64(c.layout);
                (c.name.to_owned(), h.finish())
            })
            .collect()
    }

    /// The `Debug` text of a component value on an entity (agent `net.entity_state` on the server,
    /// and on a client copy that is showing the value now).
    pub fn debug_value(&self, world: &World, index: u16, e: Entity) -> Option<String> {
        let c = self.get(index)?;
        (c.snapshot)(world, e).map(|v| (c.to_debug)(&v))
    }

    /// The `Debug` text of the last value applied to a client copy.
    ///
    /// An interpolated or stepped proxy keeps that value in `NetReceived` until the interpolation
    /// clock shows it. A copy with no `NetReceived` reports the live component.
    pub fn debug_value_applied(&self, world: &World, index: u16, e: Entity) -> Option<String> {
        let c = self.get(index)?;
        (c.received_debug)(world, e).or_else(|| (c.snapshot)(world, e).map(|v| (c.to_debug)(&v)))
    }
}

/// Registers replicated components on an app.
pub trait ReplicateAppExt {
    /// Registers `C`. Call once per component type, on every process, in the same order (it feeds
    /// the protocol hash).
    fn replicate<C: Component + ReplicateCodec + std::fmt::Debug>(
        &mut self,
        opts: ReplicateOptions,
    ) -> &mut Self;
}

impl ReplicateAppExt for regia_app::App {
    fn replicate<C: Component + ReplicateCodec + std::fmt::Debug>(
        &mut self,
        opts: ReplicateOptions,
    ) -> &mut Self {
        register_replicated::<C>(self.world_mut(), opts);
        self
    }
}

/// Registers `C` on a world (what [`ReplicateAppExt::replicate`] does).
pub fn register_replicated<C: Component + ReplicateCodec + std::fmt::Debug>(
    world: &mut World,
    opts: ReplicateOptions,
) {
    let id = world.register_component::<C>();
    let mut registry = world.get_resource_or_insert_with(ReplicationRegistry::default);
    let index = registry.register::<C>(opts);
    if let Some(c) = registry.comps.get_mut(usize::from(index)) {
        c.id = Some(id);
    }
}

// ---- transform fast path -----------------------------------------------------------------------

/// The transform state the fast path sends.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetTransform {
    /// Position (world for roots, parent-relative for replicated children).
    pub translation: Vec3,
    /// Rotation.
    pub rotation: Quat,
    /// Scale (`Full` only).
    pub scale: Vec3,
    /// Linear velocity (`Movement` and `Full`, when the entity has `NetVelocity`).
    pub velocity: Option<Vec3>,
}

/// Reads the absolute transform record of `mode` (spec 120 § 8.2, ADR-8467).
fn decode_transform(mode: TransformReplication, r: &mut BitReader) -> Result<NetTransform, DecodeError> {
    QTransform::read_abs(mode, r).map(|q| q.to_net())
}

fn transform_mode_bits(mode: TransformReplication) -> u64 {
    match mode {
        TransformReplication::None => 0,
        TransformReplication::PositionOnly => 1,
        TransformReplication::Movement => 2,
        TransformReplication::Full => 3,
    }
}

fn transform_mode_from(bits: u64) -> TransformReplication {
    match bits {
        1 => TransformReplication::PositionOnly,
        2 => TransformReplication::Movement,
        3 => TransformReplication::Full,
        _ => TransformReplication::None,
    }
}

fn server_transform(
    world: &World,
    e: Entity,
    parent_replicated: bool,
    quant: Option<crate::profile::PositionQuantizer>,
) -> Option<NetTransform> {
    let ent = world.get_entity(e).ok()?;
    let local = ent.get::<Transform>()?;
    let (translation, rotation, scale) = if !parent_replicated && ent.get::<ChildOf>().is_some() {
        // Propagation has not run yet: the parent-relative offset is not a world position and
        // the client reads this as a root, so send nothing until the global transform exists.
        let g = ent.get::<GlobalTransform>()?;
        let (s, r, t) = g.0.to_scale_rotation_translation();
        (t, r, s)
    } else {
        (local.translation, local.rotation, local.scale)
    };
    // A root crosses the wire in the shared world frame (spec 78 § 14); a child stays relative.
    let translation = match crate::origin::wire_offset(world) {
        Some(offset) if !parent_replicated => crate::origin::local_to_wire(translation, offset),
        _ => translation,
    };
    // A root's wire position sits on its profile's `position_step_m` grid (ADR-2413 (4)); a child
    // stays relative and unsnapped. `quant` is the root's cached quantizer (`None` = raw).
    let translation = if parent_replicated {
        translation
    } else {
        quant.map_or(translation, |q| q.snap(translation))
    };
    Some(NetTransform {
        translation,
        rotation,
        scale,
        velocity: ent.get::<NetVelocity>().map(|v| v.0),
    })
}

// ---- NetVec -----------------------------------------------------------------------------------

/// Elements a [`NetVec`] holds at most. The wire bound is this constant.
/// `NetSettings.replication.netvec_max` is reserved and does not change it (spec 71 § 10).
pub const NETVEC_MAX: usize = 4096;

/// `NetVec::push` on a full list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("the NetVec holds the maximum of {0} elements")]
pub struct NetVecFull(pub usize);

/// A list whose elements have server-assigned stable ids (never reused within the list). It
/// replicates as adds, removes and per-element changes instead of whole lists (spec 71 § 10).
///
/// `Clone` shares element allocations. [`NetVec::get_mut`] and [`NetVec::remove`] copy an element
/// only when another list still holds it. The wire format is unchanged.
#[derive(Component, Debug, Clone, PartialEq)]
pub struct NetVec<T: ReplicateCodec + Send + Sync + 'static> {
    items: Vec<(u32, Arc<T>)>,
    next_id: u32,
}

impl<T: ReplicateCodec + Send + Sync + 'static> Default for NetVec<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            next_id: 1,
        }
    }
}

impl<T: ReplicateCodec + Send + Sync + 'static> NetVec<T> {
    /// An empty list.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a value; returns its stable id.
    pub fn push(&mut self, value: T) -> Result<u32, NetVecFull> {
        if self.items.len() >= NETVEC_MAX {
            return Err(NetVecFull(NETVEC_MAX));
        }
        // Ids are never reused: past `u32::MAX` (0 marks the exhausted counter) or onto a live id
        // the list refuses instead of handing out a colliding id.
        let id = self.next_id;
        if id == 0 || self.items.iter().any(|(i, _)| *i == id) {
            return Err(NetVecFull(NETVEC_MAX));
        }
        self.next_id = id.checked_add(1).unwrap_or(0);
        self.items.push((id, Arc::new(value)));
        Ok(id)
    }

    /// Removes an element by id.
    pub fn remove(&mut self, id: u32) -> Option<T> {
        let pos = self.items.iter().position(|(i, _)| *i == id)?;
        let (_, value) = self.items.remove(pos);
        Some(Arc::unwrap_or_clone(value))
    }

    /// An element by id.
    pub fn get(&self, id: u32) -> Option<&T> {
        self.items.iter().find(|(i, _)| *i == id).map(|(_, v)| v.as_ref())
    }

    /// An element by id, mutable. Copies the element when another list still shares it.
    pub fn get_mut(&mut self, id: u32) -> Option<&mut T> {
        self.items
            .iter_mut()
            .find(|(i, _)| *i == id)
            .map(|(_, v)| Arc::make_mut(v))
    }

    /// Moves an element to a new position (clamped to the end).
    pub fn move_to(&mut self, id: u32, index: usize) {
        if let Some(pos) = self.items.iter().position(|(i, _)| *i == id) {
            let item = self.items.remove(pos);
            let index = index.min(self.items.len());
            self.items.insert(index, item);
        }
    }

    /// `(id, value)` in order.
    pub fn iter(&self) -> impl Iterator<Item = (u32, &T)> {
        self.items.iter().map(|(i, v)| (*i, v.as_ref()))
    }

    /// Element count.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether the list is empty.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Element ids in order.
    pub fn ids(&self) -> Vec<u32> {
        self.items.iter().map(|(i, _)| *i).collect()
    }

    /// The element events from `old` to `self`: removes, adds, then changes, each in id order.
    pub fn diff(&self, old: &Self) -> Vec<(NetVecEventKind, u32)> {
        let cx = CodecContext::detached();
        let old_ids: BTreeMap<u32, &T> = old.items.iter().map(|(i, v)| (*i, v.as_ref())).collect();
        let new_ids: BTreeMap<u32, &T> = self.items.iter().map(|(i, v)| (*i, v.as_ref())).collect();
        let mut out = Vec::new();
        for id in old_ids.keys() {
            if !new_ids.contains_key(id) {
                out.push((NetVecEventKind::Removed, *id));
            }
        }
        for id in new_ids.keys() {
            if !old_ids.contains_key(id) {
                out.push((NetVecEventKind::Added, *id));
            }
        }
        for (id, v) in &new_ids {
            if let Some(o) = old_ids.get(id)
                && !v.wire_eq(o, &cx)
            {
                out.push((NetVecEventKind::Changed, *id));
            }
        }
        out
    }
}

/// A baseline's elements, searched in place when ids increase and by map otherwise.
struct NetVecBaseline<'a, T> {
    sorted: bool,
    items: &'a [(u32, Arc<T>)],
    map: HashMap<u32, &'a Arc<T>>,
}

impl<'a, T> NetVecBaseline<'a, T> {
    fn new(items: &'a [(u32, Arc<T>)]) -> Self {
        let sorted = items.windows(2).all(|w| w[0].0 < w[1].0);
        let map = if sorted {
            HashMap::new()
        } else {
            items.iter().map(|(id, value)| (*id, value)).collect()
        };
        Self { sorted, items, map }
    }

    fn get(&self, id: u32) -> Option<&Arc<T>> {
        if self.sorted {
            let at = self.items.binary_search_by_key(&id, |pair| pair.0).ok()?;
            Some(&self.items[at].1)
        } else {
            self.map.get(&id).copied()
        }
    }
}

impl<T: ReplicateCodec + Send + Sync + 'static> ReplicateCodec for NetVec<T> {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        w.write_varint(u64::from(self.next_id));
        w.write_varint(self.items.len() as u64);
        for (id, v) in &self.items {
            w.write_varint(u64::from(*id));
            v.encode(w, cx);
        }
    }

    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        let next_id = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("id"))?;
        let n = r.read_len(NETVEC_MAX)?;
        let mut items = Vec::with_capacity(n.min(r.remaining_bits()));
        for _ in 0..n {
            let id = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("id"))?;
            items.push((id, Arc::new(T::decode(r, cx)?)));
        }
        Ok(Self { items, next_id })
    }

    fn wire_eq(&self, other: &Self, cx: &CodecContext) -> bool {
        self.next_id == other.next_id
            && self.items.len() == other.items.len()
            && self
                .items
                .iter()
                .zip(&other.items)
                .all(|((a, x), (b, y))| a == b && x.wire_eq(y, cx))
    }

    /// Delta: the id order, then for each id one bit "new or changed" and its value (full for new
    /// ids, a delta for known ones). Removed ids are those missing from the order.
    ///
    /// A baseline whose ids are in order is searched in place. An unordered baseline (after
    /// [`NetVec::move_to`]) falls back to a map. An unchanged element is shared, not cloned.
    fn encode_delta(&self, base: &Self, w: &mut BitWriter, cx: &CodecContext) {
        let old = NetVecBaseline::new(&base.items);
        w.write_varint(u64::from(self.next_id));
        w.write_varint(self.items.len() as u64);
        for (id, v) in &self.items {
            w.write_varint(u64::from(*id));
            match old.get(*id).map(Arc::as_ref) {
                Some(o) if v.wire_eq(o, &cx.scratch()) => w.write_bits(0, 2),
                Some(o) => {
                    w.write_bits(1, 2);
                    v.encode_delta(o, w, cx);
                }
                None => {
                    w.write_bits(2, 2);
                    v.encode(w, cx);
                }
            }
        }
    }

    fn decode_delta(base: &Self, r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        let old = NetVecBaseline::new(&base.items);
        let next_id = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("id"))?;
        let n = r.read_len(NETVEC_MAX)?;
        let mut items = Vec::with_capacity(n.min(r.remaining_bits()));
        for _ in 0..n {
            let id = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("id"))?;
            let v = match r.read_bits(2)? {
                0 => Arc::clone(old.get(id).ok_or(DecodeError::Invalid("unknown NetVec id"))?),
                1 => {
                    let o = old.get(id).ok_or(DecodeError::Invalid("unknown NetVec id"))?;
                    Arc::new(T::decode_delta(o.as_ref(), r, cx)?)
                }
                2 => Arc::new(T::decode(r, cx)?),
                _ => return Err(DecodeError::Invalid("NetVec element tag")),
            };
            items.push((id, v));
        }
        Ok(Self { items, next_id })
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str("net_vec");
        T::layout(h);
    }
}

/// What happened to one `NetVec` element.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetVecEventKind {
    /// The id appeared.
    Added,
    /// The element's value changed.
    Changed,
    /// The id disappeared.
    Removed,
}

/// One element event, emitted on clients after the tick's state is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetVecEvent {
    /// The entity holding the list.
    pub entity: Entity,
    /// What happened.
    pub kind: NetVecEventKind,
    /// The element id.
    pub id: u32,
}

/// Collects `NetVecEvent`s for `NetVec<T>` from its change notices. Register the list with
/// [`register_netvec`], which keeps previous values so this can diff them.
pub fn netvec_events<T: ReplicateCodec + Send + Sync + std::fmt::Debug + 'static>(
    world: &World,
) -> Vec<NetVecEvent> {
    let Some(ev) = world.get_resource::<NetChangedEvents<NetVec<T>>>() else {
        return Vec::new();
    };
    let empty = NetVec::<T>::default();
    let mut out = Vec::new();
    for n in ev.iter() {
        let Some(now) = world.get::<NetVec<T>>(n.entity) else {
            continue;
        };
        let old = n.previous.as_ref().unwrap_or(&empty);
        for (kind, id) in now.diff(old) {
            out.push(NetVecEvent {
                entity: n.entity,
                kind,
                id,
            });
        }
    }
    out
}

/// Registers `NetVec<T>` with `keep_previous` so [`netvec_events`] can diff updates.
pub fn register_netvec<T: ReplicateCodec + Send + Sync + std::fmt::Debug + 'static>(
    world: &mut World,
    audience: Audience,
) {
    register_replicated::<NetVec<T>>(
        world,
        ReplicateOptions {
            audience,
            notify: NotifyWhen::Changed,
            keep_previous: true,
            notify_on_server: false,
        },
    );
}

// ---- server state ------------------------------------------------------------------------------

/// How a client creates its copy (spec 71 § 4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpawnKind {
    /// Spawn an empty entity and insert the replicated components.
    Bare,
    /// Instantiate the prefab locally, then apply replicated values on top.
    Prefab(u128),
    /// Bind to the client's own level entity with this guid.
    Level {
        /// The level (`LevelMember` id).
        level: u128,
        /// The entity's `EntityGuid`.
        guid: u128,
    },
}

/// Bytes of the largest encoded transform (`Full` with velocity and scale).
const TRANSFORM_BYTES: usize = 48;

/// The transform as the fast path encodes it. Held inline: a version is read for every record, and
/// a pointer more per version is a cache miss more per record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TransformBits {
    len: u16,
    bytes: [u8; TRANSFORM_BYTES],
    /// The integers the bytes encode: what a delta against this version is computed from.
    q: QTransform,
}

impl TransformBits {
    /// The record of `t` under `mode` on the position grid `quant` (`None`: raw), encoded once and
    /// shared by every connection (spec 120 § 8.2, ADR-8467).
    fn from_net(
        mode: TransformReplication,
        t: &NetTransform,
        quant: Option<crate::profile::PositionQuantizer>,
    ) -> Self {
        let q = QTransform::from_net(mode, t, quant);
        let mut w = BitWriter::new();
        q.write_abs(&mut w);
        let mut bytes = [0; TRANSFORM_BYTES];
        for (to, from) in bytes.iter_mut().zip(w.as_bytes()) {
            *to = *from;
        }
        Self {
            len: w.bit_len() as u16,
            bytes,
            q,
        }
    }
}

/// One entity's replicated state at one send tick (spec 72 § 4.1), shared by every connection.
/// What did not change is shared with the version before it. Connections hold the version they
/// are known to have by reference, so a version lives exactly as long as something can delta
/// against it.
#[derive(Debug)]
struct StateVersion {
    tick: NetTick,
    /// Counts this entity's versions (wraps): how far behind a baseline is.
    number: u32,
    parent: Option<NetId>,
    transform: Option<TransformBits>,
    /// `(component, value)` sorted by component.
    comps: Arc<Vec<(u16, ErasedComp)>>,
}

impl StateVersion {
    fn comp(&self, ci: u16) -> Option<&ErasedComp> {
        self.comps
            .binary_search_by_key(&ci, |(i, _)| *i)
            .ok()
            .and_then(|at| self.comps.get(at))
            .map(|(_, v)| v)
    }
}

/// The identity of a version (its address; 0 = none).
#[derive(Debug)]
struct ServerEntity {
    id: NetId,
    entity: Entity,
    slot: u32,
    kind: SpawnKind,
    depth: u32,
    transform_mode: TransformReplication,
    sleep: NetSleep,
    asleep: bool,
    by_marker: bool,
    started: bool,
    quiet_ticks: u32,
    teleport: bool,
    resend: bool,
    last_change: NetTick,
    server_values: Vec<Option<ErasedComp>>,
    /// Per registered component: changed since the last gather (refreshed every gather).
    changed: Vec<bool>,
    /// Any replicated component, the transform or the parent changed since the last gather.
    dirty: bool,
    /// Its `Replicated` component or parent changed at this gather.
    structure: bool,
    /// A shared-group or predicate audience changed since the last gather. Presence alone is
    /// not enough: an unchanged audience does not queue an update.
    volatile: bool,
    /// The transform as the fast path encodes it; refreshed on change.
    tcache: Option<TransformBits>,
    /// The newest state version.
    current: Option<Arc<StateVersion>>,
    /// `current` was made at this gather.
    fresh_version: bool,
    /// Which registered components the entity had at the last gather (a bitset).
    present: Vec<u64>,
    /// The resolved owner (recomputed when any entity's ownership inputs change).
    owner: PeerId,
    /// The ownership inputs of this entity: its `NetOwner`, `NetOwnerLink` target and parent.
    owner_inputs: (Option<PeerId>, Option<Entity>, Option<Entity>),
    /// The server entity has `NetPredicted`.
    predicted: bool,
    /// The `NetId` of its replicated parent.
    parent_net: Option<NetId>,
    /// `parent_net` changed at this gather.
    parent_moved: bool,
    /// Carries `NetAsleep` at this gather.
    forced: bool,
    /// Its `Transform` or `GlobalTransform` changed at this gather (relevance positions).
    moved: bool,
    /// `predicted` changed at this gather.
    predicted_flipped: bool,
    /// The resolved owner changed at this gather (relevance and the copies' ownership bits look
    /// at this entity again; spec 120 § 8.4: no full pass over every copy).
    owner_flipped: bool,
    /// The owner inputs were read once: a first read is a new entity's, which resolves its own
    /// owner alone (no other entity's chain changed with it).
    owner_known: bool,
    /// A new entity whose owner inputs are set: the gather resolves its owner alone this tick.
    owner_pending: bool,
    /// Its [`NetPriorityScale`] factor at this gather (1 without the component).
    priority_scale: f32,
    /// Its [`NetCombatHeat`] at this gather (0 without the component) and the heat's reach.
    heat: (f32, f32),
    /// Cached root position quantizer. `None` is a raw position (no step).
    quant: Option<crate::profile::PositionQuantizer>,
    /// `NetProfiles` generation this quantizer was resolved against.
    quant_gen: u64,
    /// `quant` has been resolved. A profile with no step is `quant: None` with this set.
    quant_set: bool,
    /// A `NetProfileRef` was present when `quant` was resolved.
    quant_had_ref: bool,
    /// The profile's extrapolation gate in metres (`None`: off), resolved with `quant` (spec 120
    /// § 8.2, ADR-8522).
    gate: Option<f32>,
}

/// One entity as relevance reads it.
fn interest_row(s: &ServerEntity) -> InterestEntity {
    InterestEntity {
        id: s.id,
        slot: s.slot,
        entity: s.entity,
        owner: s.owner,
        depth: s.depth,
        dirty: s.dirty || s.parent_moved || s.moved,
        structure: s.structure || s.parent_moved || s.owner_flipped,
        predicted: s.predicted,
        parent: s.parent_net,
    }
}

impl ServerEntity {
    /// Something relevance reads for this entity changed at this gather.
    fn interest_input_changed(&self) -> bool {
        self.dirty
            || self.parent_moved
            || self.moved
            || self.structure
            || self.predicted_flipped
            || self.owner_flipped
    }
}

/// One copy a connection holds.
#[derive(Debug)]
struct ConnEntity {
    id: NetId,
    slot: u32,
    /// The state version the connection is known to hold.
    base: Option<Arc<StateVersion>>,
    /// `base`'s number: the walk compares it for every held copy and must not read `base` for it.
    base_number: u32,
    /// The tick of the version the client has under that content. A new version equal to `base`
    /// for this connection moves `base` without a record, so the client never saw its tick.
    base_tick: NetTick,
    /// Components of `base` the connection does not follow (their audience left it).
    unmatched: Vec<u16>,
    owned: bool,
    predicted: bool,
    last_sent: NetTick,
    out_of_scope: bool,
    full_again: bool,
    /// The next update owes the teleport bit (`net_teleport` while it was held back).
    teleport_due: bool,
    /// The next update owes a full resend (`net_resend` while it was held back).
    resend_due: bool,
    /// When this copy was created for the connection (acknowledged state channel: older records
    /// belong to a copy before it).
    born: NetTick,
    /// The tick of the last `net_teleport`.
    teleport_tick: NetTick,
    /// The tick of the last `net_resend`.
    resend_tick: NetTick,
    /// The client acknowledged a spawn of this copy.
    spawn_acked: bool,
    /// The newest packet that carried this copy, and the number of the version it carried.
    sent_seq: u32,
    sent_version: u32,
    /// An entity with an extrapolation gate: the update records sent since the acknowledged
    /// baseline, `(packet, version)` (`NO_PACKET` until placed). The client may be showing any of
    /// them, so the gate holds a record back only when every one still extrapolates within it
    /// (ADR-8522). Empty for every other entity.
    flight: Vec<(u32, Arc<StateVersion>)>,
    /// The entity had an extrapolation gate when the gate last looked at this copy. Its baseline
    /// then expires by the records sent (`flight`), not by the versions made: a held-back version
    /// never reaches the client's ring, so the baseline stays there (ADR-8522).
    gated: bool,
    /// The client holds `base` in its far form (spec 120 § 8.3, ADR-10124): only far records of it
    /// were acknowledged, so the client's position is `base`'s coarse position (its cell is
    /// `base`'s) and its rotation and velocity are not `base`'s. Such a baseline carries component
    /// deltas and further far records, never a transform delta; a copy that holds one stays an
    /// update candidate until an exact record is acknowledged.
    base_far: bool,
}

/// Bits a far record must save over the exact update for a copy with an exact baseline to take
/// the far form (ADR-10124): leaving the far form costs an absolute record, so a copy enters it
/// only when the exact record is at least a byte longer.
const FAR_ENTRY_MARGIN_BITS: usize = 8;

/// Seconds a version stands before a far-tier copy takes it in the exact form (ADR-10199): a
/// far copy whose entity came to rest gets its exact transform once, still in the far tier, and
/// is then even with the server, so a far crowd at rest costs a gather nothing. Without it a copy
/// held in its far form stays an update candidate of every send tick for as long as it is far.
pub const FAR_SETTLE_SECONDS: f64 = 2.0;

/// `sent_seq` of a copy with nothing on the way.
const NO_PACKET: u32 = u32::MAX;

/// A version number no version has.
const NO_VERSION: u32 = u32::MAX;

impl ConnEntity {
    /// The number of the version the connection is known to hold.
    fn base_version(&self) -> u32 {
        if self.base.is_some() {
            self.base_number
        } else {
            NO_VERSION
        }
    }
}

/// The per-tick lists of one connection's gather, kept for their buffers (spec 120 § 8.4: a steady
/// tick allocates no per-connection list). Each is cleared before use.
#[derive(Debug, Default)]
struct ConnScratch {
    visit: Vec<u32>,
    keep: Vec<(u32, bool)>,
    updates: Vec<(NetId, u32)>,
    spawns: Vec<(u32, NetId, u32)>,
    scope_out: Vec<(NetId, u32)>,
    own_changes: Vec<(NetId, bool, bool)>,
}

/// What the server knows about one connection's copies.
#[derive(Debug, Default)]
pub(crate) struct ConnReplication {
    /// The copies, packed.
    held: Vec<ConnEntity>,
    /// Slot to position in `held` (`NO_SLOT` = no copy).
    index: Vec<u32>,
    /// The newest tick this connection acknowledged.
    acked: Option<NetTick>,
    joined: bool,
    /// The join window is closed (spec 77 § 6.1, ADR-7212): every join-critical spawn (the peer's
    /// own entities, `sent_to_all` game state) of the connection's join went out. While it is open,
    /// those spawns go outside the spawn budget. A travel opens it again.
    join_spawns_done: bool,
    resync: BTreeSet<NetId>,
    /// Some copy is out of scope or due a full resend (the steady-state shortcut does not apply).
    irregular: bool,
    /// Slots that are dirty, in flight, awaiting an ack, or otherwise not known to match.
    watch: SlotBits,
    /// `watch` was filled by a full acknowledged walk.
    watch_built: bool,
    /// A relevant entity had no copy after the last spawn search (a budget may have deferred it).
    spawn_owed: bool,
    /// The relevant slots still without a copy after the last tick's pack (the pipeline's spawn
    /// candidates besides this tick's gained slots).
    owed_slots: Vec<u32>,
    /// The per-tick lists of `gather_conn`, kept between gathers for their buffers.
    scratch: ConnScratch,
    /// The state channel of the connection.
    link: StateLink,
    /// Empty writers for the next records.
    writers: Vec<BitWriter>,
    /// This connection is a recorder's (`crate::recorder`).
    recorder: Option<Box<RecorderRole>>,
    /// Warned already that this connection's relevant set reached 90 % of `MAX_CLIENT_ENTITIES`
    /// (ADR-8850).
    warned_near_cap: bool,
    /// A resumed connection (spec 120 § 10.3): the copies its client held when it went dormant.
    /// The first written frame despawns the ones the server despawned meanwhile, which a fresh
    /// connection state would otherwise never mention.
    resume_owed: Vec<NetId>,
    /// Slots whose record for this connection was over [`crate::send::MAX_ENTITY_STATE_BYTES`],
    /// with the id and the state version measured (ADR-8419): until the entity changes it is
    /// refused without being encoded again.
    refused: HashMap<u32, (NetId, u32)>,
}

/// What makes a connection a recorder's (spec 103 § 3): its view, and what it heals next.
#[derive(Debug)]
struct RecorderRole {
    view: RecorderView,
    /// A `Player` view's set: that peer's held bits at this tick, frozen when it leaves.
    bits: Option<crate::dense::SlotBits>,
    /// The ids this connection gained in its last gather.
    spawned: Vec<NetId>,
    /// Its last gather gained or lost a copy: references may need another full record.
    heal: bool,
    /// A scratch writer for probing references.
    probe: BitWriter,
    /// A `Player` view's peer left: the set in `bits` stays, and no filter runs for that peer.
    frozen: bool,
}

impl ConnReplication {
    fn acked_at_least(&self, tick: NetTick) -> bool {
        self.acked.is_some_and(|a| a >= tick)
    }

    fn copy(&self, slot: u32) -> Option<&ConnEntity> {
        match self.index.get(slot as usize) {
            Some(i) if *i != NO_SLOT => self.held.get(*i as usize),
            _ => None,
        }
    }

    fn copy_mut(&mut self, slot: u32) -> Option<&mut ConnEntity> {
        match self.index.get(slot as usize) {
            Some(i) if *i != NO_SLOT => self.held.get_mut(*i as usize),
            _ => None,
        }
    }

    /// Adds a copy, or replaces the copy of its slot.
    fn insert(&mut self, ce: ConnEntity) {
        let slot = ce.slot;
        let at = slot as usize;
        self.watch.set(slot);
        if at >= self.index.len() {
            self.index.resize(at + 1, NO_SLOT);
        }
        match self.index.get(at).copied() {
            Some(i) if i != NO_SLOT => {
                if let Some(old) = self.held.get_mut(i as usize) {
                    *old = ce;
                }
            }
            _ => {
                if let Some(i) = self.index.get_mut(at) {
                    *i = self.held.len() as u32;
                }
                self.held.push(ce);
            }
        }
    }

    /// Removes the copy of a slot.
    fn remove(&mut self, slot: u32) -> Option<ConnEntity> {
        let at = *self.index.get(slot as usize)?;
        if at == NO_SLOT {
            return None;
        }
        if let Some(i) = self.index.get_mut(slot as usize) {
            *i = NO_SLOT;
        }
        let ce = self.held.swap_remove(at as usize);
        if let Some(moved) = self.held.get(at as usize)
            && let Some(i) = self.index.get_mut(moved.slot as usize)
        {
            *i = at;
        }
        self.watch.unset(slot);
        Some(ce)
    }
}

/// The protocol version that brought the acknowledged state channel.
pub const STATE_CHANNEL_PROTOCOL: u32 = 2;

/// Resync requests a connection may queue per tick (the rest are ignored; an honest client asks
/// again).
pub const MAX_RESYNC_PER_TICK: usize = 64;

/// Wall time of the last gather by phase, in milliseconds (spec 72 § 14; agent `net.state_channel`).
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct GatherTiming {
    /// Change detection, sleep and state versions.
    pub changes_ms: f64,
    /// Relevance (span `net_relevance`).
    pub relevance_ms: f64,
    /// Priority, packing and commit for every connection (spans `net_priority`, `net_pack`).
    pub send_ms: f64,
    /// The serial part of `relevance_ms`: what connections share, before they fan out.
    pub relevance_shared_ms: f64,
    /// The walks over held copies, summed over connections (CPU time, not wall time).
    pub walk_cpu_ms: f64,
    /// Priority gain and ranking, summed over connections.
    pub priority_cpu_ms: f64,
    /// Encoding and packing, summed over connections.
    pub pack_cpu_ms: f64,
    /// Commit and frame assembly, summed over connections.
    pub commit_cpu_ms: f64,
    /// Candidates the ranking put in order, summed over connections: what the budget could reach,
    /// not everything that waits.
    pub ordered_candidates: u64,
}

/// Counters of the replication layer (agent `net.replicated` and the tests).
///
/// The codec counters (`quantize_clamped`, `refs_not_replicated`) are counted per
/// (connection, component) replication gather: one clamped component on a 64-peer server
/// reports 64, on a 1-peer test 1. Thresholds over these totals must account for the
/// connection count (audit fixrev-15-k net3-determinism L6, ADR-4772).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReplicationCounters {
    /// Values clamped by quantizers, counted per (connection, component) replication gather.
    pub quantize_clamped: u64,
    /// Entity references to non-replicated entities, counted per (connection, component)
    /// replication gather like [`Self::quantize_clamped`].
    pub refs_not_replicated: u64,
    /// Prefab spawns that fell back to bare.
    pub prefab_missing: u64,
    /// Level binds whose guid was missing.
    pub level_entity_missing: u64,
    /// Level binds kept until the streamed cell of their entity spawns (spec 71 § 4.5).
    pub level_entity_buffered: u64,
    /// Kept level binds whose entity spawned and asked the server for its full state.
    pub level_entity_buffer_applied: u64,
    /// Level binds not kept because the buffer was full (`ClientReplication::level_bind_cap`).
    pub level_entity_buffer_dropped: u64,
    /// Kept level binds that ended before their entity spawned: despawn, relevance loss, a closed
    /// level or a new session.
    pub level_entity_buffer_cleared: u64,
    /// Pending references dropped over the cap.
    pub pending_refs_dropped: u64,
    /// Replicated copies despawned by client gameplay.
    pub client_despawned_proxy: u64,
    /// Replicated entities whose parent is not replicated (sent as roots).
    pub parent_not_replicated: u64,
    /// Spawn records over the 16 KiB entity state limit, not sent (spec 72 § 7).
    pub state_too_large: u64,
    /// Resync loops detected (spec 72 § 9).
    pub resync_loops: u64,
    /// Join-critical spawn records (a joining peer's own entities and `sent_to_all` game state)
    /// sent outside the spawn budget (spec 77 § 6.1, ADR-7212).
    pub join_critical_spawns: u64,
    /// Connections whose relevant set reached 90 % of `MAX_CLIENT_ENTITIES`, counted once per
    /// connection (warned once, ADR-8850): past the cap the client refuses the next spawn and
    /// the session ends (ADR-8760).
    pub relevant_near_cap: u64,
}

/// Server replication state: the replicated entities and every connection's view of them.
#[derive(Resource, Debug, Default)]
pub struct ServerReplication {
    next_net_id: u32,
    /// Bumps when a replicated entity is added or removed.
    entity_gen: u64,
    entities: SlotTable<ServerEntity>,
    conns: BTreeMap<PeerId, ConnReplication>,
    last_gather: Option<Tick>,
    /// The tick of the last gather (a send tick).
    last_send: Option<NetTick>,
    level_removed: Vec<(u128, u128)>,
    /// A travel despawned the old level (spec 77 § 6.2): the next despawn pass forgets the removed
    /// level entities instead of keeping them for joins into the new level — but only the ones
    /// the travel itself despawned (this set), not removals recorded in the same pass for other
    /// entities.
    level_reset_pending: bool,
    /// The `(level, guid)` pairs the in-flight travel despawned (empty when no travel is being
    /// collected). Consumed with `level_reset_pending`.
    travel_removed: std::collections::BTreeSet<(u128, u128)>,
    warned_parent: HashSet<Entity>,
    warned_refs: HashSet<u16>,
    exhausted_logged: bool,
    owners_stale: bool,
    /// Entities joined or left since the last gather.
    membership_stale: bool,
    /// Entities got ids since the last gather (an off tick's `server_assign` included).
    spawned_pending: bool,
    owner_sig: (u64, u64),
    warned_large: HashSet<NetId>,
    /// Far-tier updates keep the exact record (`set_far_records(false)`).
    exact_far_records: bool,
    /// Counters.
    pub counters: ReplicationCounters,
    /// Wall time of the last gather.
    pub timing: GatherTiming,
    /// Copies the last gather walked, summed over connections.
    copies_visited: u64,
    /// Entities the last gather checked for a despawn (ADR-7213).
    despawn_checks: u64,
    /// Packed interest slots the last gather's priority pass read.
    priority_slots: u64,
    /// Reused compact entity array for the connection walks.
    hot_scratch: Vec<TickHot>,
    pending_server_notices: Vec<(u16, Entity, Option<ErasedComp>)>,
    /// Update candidates the last gather queued, summed over connections.
    last_update_candidates: u64,
    /// Predicate results of the last gather, keyed by entity and component index.
    /// Rebuilt every gather, so a despawned id or a removed component drops out.
    predicate_results: HashMap<(NetId, u16), bool>,
    /// `NetGroups` last seen on entities that carry a shared-group component.
    entity_groups: HashMap<NetId, Vec<String>>,
    /// Group set of each peer at the last gather.
    peer_group_sets: HashMap<PeerId, HashSet<String>>,
    /// Dormant peers (spec 120 § 10.3): the copies each one's client held when its connection
    /// state was dropped.
    dormant_held: BTreeMap<PeerId, Vec<NetId>>,
}

/// The connection-relevance filter of spec 72 (relevance, interest grid). Until spec 72 lands,
/// every replicated entity is relevant to every connection unless a game installs a filter here.
#[derive(Resource, Default)]
pub struct NetRelevance {
    /// `None` = everything relevant to everyone.
    pub filter: Option<fn(&World, Entity, PeerId) -> bool>,
}

impl ServerReplication {
    /// Starts tracking a connection on the reliable tick stream (its first frame carries the join
    /// snapshot).
    pub fn add_conn(&mut self, peer: PeerId) {
        self.add_conn_on(peer, ConnWire::Stream);
    }

    /// Starts tracking a connection on a state wire.
    pub fn add_conn_on(&mut self, peer: PeerId, wire: ConnWire) {
        self.conns.entry(peer).or_default().link.wire = wire;
    }

    /// Update candidates the last gather queued, summed over connections.
    ///
    /// A steady tick, with no component write and no audience change, queues none.
    pub fn update_candidates_last_gather(&self) -> u64 {
        self.last_update_candidates
    }

    pub(crate) fn set_transport_frame_bytes(&mut self, peer: PeerId, bytes: usize) {
        self.conns.entry(peer).or_default().link.transport_frame_bytes = Some(bytes);
    }

    /// The transport's frame budget for `peer`, as last set (ADR-8300 reads it live).
    pub(crate) fn transport_frame_bytes(&self, peer: PeerId) -> Option<usize> {
        self.conns.get(&peer).and_then(|c| c.link.transport_frame_bytes)
    }

    /// The transport's send allowance for a connection, bytes per second (spec 120 § 4.2 item 9,
    /// ADR-8301): its tick budget becomes `min(project rate, allowance)`. `None` lifts it. A peer
    /// replication does not track yet is ignored.
    pub fn set_send_allowance(&mut self, peer: PeerId, bytes_per_second: Option<u32>) {
        if let Some(c) = self.conns.get_mut(&peer) {
            c.link.send_allowance = bytes_per_second;
        }
    }

    /// The state channel of a connection.
    pub fn state_link(&self, peer: PeerId) -> Option<&StateLink> {
        self.conns.get(&peer).map(|c| &c.link)
    }

    /// Warns once when one connection's relevant set reaches 90 % of [`MAX_CLIENT_ENTITIES`]
    /// (ADR-8850): past the cap the client refuses the next spawn and the session ends with a
    /// protocol error (ADR-8760), and the server's own copy count is the only early signal. The
    /// gather calls this once per connection and gather with the connection's held copies;
    /// [`ReplicationCounters::relevant_near_cap`] counts one per connection, never one per tick.
    fn note_relevant_set(&mut self, peer: PeerId, held: usize) {
        let Some(conn) = self.conns.get_mut(&peer) else {
            return;
        };
        if conn.warned_near_cap || held * 10 < MAX_CLIENT_ENTITIES * 9 {
            return;
        }
        conn.warned_near_cap = true;
        self.counters.relevant_near_cap += 1;
        tracing::warn!(
            target: "net",
            peer = peer.0,
            held,
            cap = MAX_CLIENT_ENTITIES,
            "net: one client's relevant set is near the entity cap"
        );
    }

    /// Copies the last [`server_gather`] walked, summed over connections.
    ///
    /// A quiet acknowledged connection walks the copies that are dirty, in flight, or still
    /// awaiting an ack, not every copy it holds.
    pub fn copies_visited(&self) -> u64 {
        self.copies_visited
    }

    /// Entities the last [`server_gather`] checked for a despawn: the ones whose [`Replicated`]
    /// left since the gather before (queued by its `on_remove` hook), not every tracked entity
    /// (spec 71 § 15, ADR-7213). The first gather of a world checks them all. Duplicate queue
    /// entries (a remove → re-add → remove in one tick) count once.
    pub fn despawn_checks(&self) -> u64 {
        self.despawn_checks
    }

    /// Packed interest slots the last gather's priority pass examined, summed over connections.
    pub fn priority_slots(&self) -> u64 {
        self.priority_slots
    }

    /// Whether a far-tier update may travel as a far record (spec 120 § 8.3, ADR-10124): on by
    /// default.
    pub fn far_records(&self) -> bool {
        !self.exact_far_records
    }

    /// Turns the far record on or off for every connection. Off, a far-tier copy keeps its exact
    /// record at the far rate (the behavior before transform record revision 4): for a game whose
    /// far copies need their rotation or sub-metre height, and for measuring what the far record
    /// saves. A copy already held in its far form gets its exact state with its next record.
    pub fn set_far_records(&mut self, on: bool) {
        self.exact_far_records = !on;
    }

    /// Sets how a connection receives state, skipping the negotiation (servers without clients:
    /// benches and tools).
    pub fn set_wire(&mut self, peer: PeerId, wire: ConnWire) {
        if let Some(c) = self.conns.get_mut(&peer) {
            c.link.wire = wire;
        }
    }

    /// Acknowledges every state frame sent to a connection so far, as a client with a perfect link
    /// would (benches).
    pub fn ack_all_state(&mut self, peer: PeerId) {
        if let Some(c) = self.conns.get_mut(&peer) {
            c.ack_everything();
        }
    }

    /// Forgets a connection.
    pub fn remove_conn(&mut self, peer: PeerId) {
        self.conns.remove(&peer);
        self.dormant_held.remove(&peer);
    }

    /// A peer goes dormant (spec 120 § 10.3): its connection state (copies, baselines, acks, state
    /// link) is dropped, and the ids its client held are kept for its resume.
    pub fn make_dormant(&mut self, peer: PeerId) {
        let held: Vec<NetId> = self
            .conns
            .get(&peer)
            .map(|c| c.held.iter().map(|ce| ce.id).collect())
            .unwrap_or_default();
        self.conns.remove(&peer);
        self.dormant_held.insert(peer, held);
    }

    /// A dormant (or taken over) peer is back (spec 120 § 10.3): a fresh connection state, so the
    /// next gather is a join gather that sends every relevant entity in full; the copies its
    /// client held that the server despawned meanwhile are despawned in that frame.
    pub fn resume_conn(&mut self, peer: PeerId, wire: ConnWire) {
        let owed = match self.dormant_held.remove(&peer) {
            Some(held) => held,
            None => self
                .conns
                .get(&peer)
                .map(|c| c.held.iter().map(|ce| ce.id).collect())
                .unwrap_or_default(),
        };
        self.conns.remove(&peer);
        self.add_conn_on(peer, wire);
        if let Some(c) = self.conns.get_mut(&peer) {
            c.resume_owed = owed;
        }
    }

    /// Whether the peer is dormant here (its connection state dropped, held ids kept).
    pub fn is_dormant(&self, peer: PeerId) -> bool {
        self.dormant_held.contains_key(&peer)
    }

    /// The `EntityGuid`s of level-placed replicated entities despawned since the level loaded
    /// (spec 77 § 6.1 item 1, spec 71 § 4.5), in despawn order.
    pub fn level_removed_guids(&self) -> Vec<u128> {
        self.level_removed.iter().map(|(_, guid)| *guid).collect()
    }

    /// Every replicated entity: its `NetId`, its server entity and the owner resolved at the last
    /// gather (`PeerId::SERVER` when nothing owns it). Read by the join snapshot (spec 77 § 6.1).
    pub fn entity_owners(&self) -> impl Iterator<Item = (NetId, Entity, PeerId)> + '_ {
        self.entities.iter().map(|(_, s)| (s.id, s.entity, s.owner))
    }

    /// Whether `peer`'s join window is still open: some of its join-critical spawns (its own
    /// entities, `sent_to_all` game state) have not gone out yet (spec 77 § 6.1, ADR-7212).
    pub fn join_window_open(&self, peer: PeerId) -> bool {
        self.conns.get(&peer).is_some_and(|c| !c.join_spawns_done)
    }

    /// A level travel (spec 77 § 6.2): every connection's join window opens again, so each peer's
    /// own and game-state entities of the new level go outside the spawn budget, and the removed
    /// level entities of the old level are forgotten once the travel's despawns are collected.
    pub fn begin_level(&mut self) {
        for conn in self.conns.values_mut() {
            conn.join_spawns_done = false;
        }
        self.level_removed.clear();
        self.level_reset_pending = true;
    }

    /// Records the level entities a travel despawned (`despawn_for_travel`, spec 77 § 6.2): the
    /// next despawn pass forgets exactly these removals, and no others. Unions with any earlier
    /// in-flight travel's set.
    pub(crate) fn set_travel_removed(&mut self, removed: std::collections::BTreeSet<(u128, u128)>) {
        self.travel_removed.extend(removed);
    }

    /// The owner signature of the last gather: what `entity_owners` reports, as a cheap dirty
    /// stamp (it changes exactly when a resolved `NetOwner` or `NetOwnerLink` changed).
    pub(crate) fn owners_signature(&self) -> (u64, u64) {
        self.owner_sig
    }

    /// The `NetId` the next replicated entity gets. Ids are never reused, so every entity that has
    /// one now has a lower id (a travel's keep list, spec 77 § 6.2, ADR-7259).
    pub fn next_net_id(&self) -> NetId {
        NetId(self.next_net_id)
    }

    /// A `Player` view whose peer left keeps the set it held, with or without the relevance pipeline (spec 103 § 3.3):
    /// no game's filter runs for a peer that is gone, and nothing new enters.
    pub(crate) fn freeze_recorder_view(&mut self) {
        let Some(conn) = self.conns.get_mut(&crate::recorder::RECORDER_PEER) else {
            return;
        };
        let slots: Vec<u32> = conn.held.iter().map(|ce| ce.slot).collect();
        let Some(role) = conn.recorder.as_mut() else {
            return;
        };
        role.frozen = true;
        if role.bits.is_none() {
            let mut bits = SlotBits::default();
            for slot in slots {
                bits.set(slot);
            }
            role.bits = Some(bits);
        }
        conn.irregular = true;
    }

    /// Starts tracking a recorder's connection: the tick stream, unbudgeted, with its own view (spec 103 § 3).
    pub(crate) fn add_recorder_conn(&mut self, peer: PeerId, view: RecorderView) {
        let (bits, frozen) = if matches!(view, RecorderView::Player(_)) {
            let running = self
                .conns
                .get(&crate::recorder::RECORDER_PEER)
                .and_then(|c| c.recorder.as_ref());
            (
                running.and_then(|r| r.bits.clone()),
                running.is_some_and(|r| r.frozen),
            )
        } else {
            (None, false)
        };
        let conn = self.conns.entry(peer).or_default();
        conn.link.wire = ConnWire::Stream;
        conn.recorder = Some(Box::new(RecorderRole {
            view,
            bits,
            frozen,
            spawned: Vec::new(),
            heal: false,
            probe: BitWriter::new(),
        }));
    }

    /// Records an acknowledgement.
    pub fn ack(&mut self, peer: PeerId, tick: NetTick) {
        self.ack_state(peer, tick, None, crate::entropy::EntropyMode::Off);
    }

    /// Records an `ACK` frame: the newest applied tick and, from a protocol 2 client, its state
    /// acknowledgment. The first one decides how an undecided connection receives state, and the
    /// entropy stage with it (spec 72 § 18, ADR-3315): `project` is the server's configured
    /// [`crate::entropy::EntropyMode`].
    pub fn ack_state(
        &mut self,
        peer: PeerId,
        tick: NetTick,
        state: Option<StateAck>,
        project: crate::entropy::EntropyMode,
    ) {
        // Without the server's own tick, the tick after the last gather stands for it.
        let now = self.last_send.map(|t| NetTick(t.0.wrapping_add(1)));
        self.ack_state_at(peer, tick, state, project, now);
    }

    /// [`Self::ack_state`] at the server's tick `now`: the acknowledgment also gives the
    /// connection's state round trip (spec 120 § 8.1, ADR-8419).
    pub(crate) fn ack_state_at(
        &mut self,
        peer: PeerId,
        tick: NetTick,
        state: Option<StateAck>,
        project: crate::entropy::EntropyMode,
        now: Option<NetTick>,
    ) {
        let Some(c) = self.conns.get_mut(&peer) else {
            return;
        };
        c.acked = Some(c.acked.map_or(tick, |a| a.max(tick)));
        if c.link.wire == ConnWire::Undecided {
            let speaks = state
                .as_ref()
                .is_some_and(|s| s.protocol >= STATE_CHANNEL_PROTOCOL);
            c.link.wire = if speaks { ConnWire::Acked } else { ConnWire::Stream };
            // The same acknowledgment names the client's protocol: entropy rides the decision.
            // A protocol 2 client never names 3, so its frames stay byte-identical.
            c.link.entropy = state.as_ref().map_or(crate::entropy::EntropyMode::Off, |s| {
                crate::entropy::negotiated(s.protocol, project)
            });
            tracing::debug!(target: "net", peer = peer.0, wire = c.link.wire.as_str(), entropy = c.link.entropy.as_str(), "net state wire chosen");
        }
        if let Some(s) = state
            && c.link.wire == ConnWire::Acked
        {
            c.apply_state_ack(&s, now);
        }
    }

    /// The tick a connection last acknowledged.
    #[cfg(test)]
    pub(crate) fn acked(&self, peer: PeerId) -> Option<NetTick> {
        self.conns.get(&peer).and_then(|c| c.acked)
    }

    /// A client asked for full state of these entities again.
    pub fn request_resync(&mut self, peer: PeerId, ids: impl IntoIterator<Item = NetId>) {
        let entities = &self.entities;
        if let Some(c) = self.conns.get_mut(&peer) {
            // Only copies this connection holds, and a bounded number per tick: a resync costs a
            // full spawn record, so a client cannot ask for the whole world every tick.
            for id in ids {
                if c.resync.len() >= MAX_RESYNC_PER_TICK {
                    break;
                }
                if entities.slot_of(id).is_some_and(|s| c.copy(s).is_some()) {
                    c.resync.insert(id);
                }
            }
        }
    }

    /// The `NetId` of a server entity.
    pub fn net_id_of(&self, e: Entity) -> Option<NetId> {
        self.entities
            .iter()
            .find(|(_, s)| s.entity == e)
            .map(|(id, _)| id)
    }

    /// Whether a replicated entity is asleep.
    pub fn is_asleep(&self, id: NetId) -> Option<bool> {
        self.entities.get(id).map(|s| s.asleep)
    }

    /// Connections that have not acknowledged an entity's latest state.
    pub fn pending_acks(&self, id: NetId) -> usize {
        let Some(slot) = self.entities.slot_of(id) else {
            return 0;
        };
        // A recorder has no acknowledgements to wait for: it would count forever (spec 103 § 3.1).
        self.conns
            .iter()
            .filter(|(p, _)| !crate::recorder::is_virtual_peer(**p))
            .filter(|(_, c)| c.copy(slot).is_some_and(|ce| !c.acked_at_least(ce.last_sent)))
            .count()
    }

    /// Connections that hold a copy of an entity.
    pub fn holders(&self, id: NetId) -> Vec<PeerId> {
        let mut holders = self.all_holders(id);
        holders.retain(|p| !crate::recorder::is_virtual_peer(*p));
        holders
    }

    /// [`Self::holders`] with a recorder's connection: who a message about the entity goes to.
    pub(crate) fn all_holders(&self, id: NetId) -> Vec<PeerId> {
        let Some(slot) = self.entities.slot_of(id) else {
            return Vec::new();
        };
        self.conns
            .iter()
            .filter(|(_, c)| c.copy(slot).is_some_and(|ce| !ce.out_of_scope))
            .map(|(p, _)| *p)
            .collect()
    }

    /// Whether a connection, a virtual one included, holds a copy of an entity.
    #[must_use]
    pub fn conn_holds(&self, peer: PeerId, id: NetId) -> bool {
        self.holds_slot(peer, self.entities.slot_of(id))
    }

    /// The slot of a replicated entity (`None`: not replicated).
    pub(crate) fn slot_of(&self, id: NetId) -> Option<u32> {
        self.entities.slot_of(id)
    }

    /// Whether a connection holds an in-scope copy of a slot (`None`: nobody does).
    pub(crate) fn holds_slot(&self, peer: PeerId, slot: Option<u32>) -> bool {
        slot.is_some_and(|slot| {
            self.conns
                .get(&peer)
                .and_then(|c| c.copy(slot))
                .is_some_and(|ce| !ce.out_of_scope)
        })
    }

    /// Replicated entities: `(net_id, entity, asleep, spawn kind)`.
    pub fn entities(&self) -> impl Iterator<Item = (NetId, Entity, bool, SpawnKind)> + '_ {
        self.entities
            .iter()
            .map(|(id, s)| (id, s.entity, s.asleep, s.kind))
    }

    /// Wakes an entity (`net_wake`).
    pub fn wake(&mut self, id: NetId) -> bool {
        match self.entities.get_mut(id) {
            Some(s) => {
                s.asleep = false;
                s.by_marker = false;
                s.started = true;
                s.quiet_ticks = 0;
                true
            }
            None => false,
        }
    }

    /// Marks the next transform update of an entity with the teleport bit (`net_teleport`).
    pub fn teleport(&mut self, id: NetId) -> bool {
        match self.entities.get_mut(id) {
            Some(s) => {
                s.teleport = true;
                true
            }
            None => false,
        }
    }

    /// Sends an entity's current state once to every relevant connection without waking it
    /// (`net_resend`). Returns the peers it is queued to (a recorder's connection gets it too, and is not a peer).
    pub fn resend(&mut self, id: NetId) -> Vec<PeerId> {
        let holders = self.holders(id);
        if let Some(s) = self.entities.get_mut(id) {
            s.resend = true;
        }
        holders
    }

    /// The tick an entity's replicated state last changed on the server.
    pub fn last_change(&self, id: NetId) -> Option<NetTick> {
        self.entities.get(id).map(|s| s.last_change)
    }

    /// A connection's copy of an entity: `(last sent tick, baseline tick)`. The baseline is the
    /// newest state the connection acknowledged for it (`None` until it acknowledged the spawn).
    pub fn copy_ticks(&self, peer: PeerId, id: NetId) -> Option<(NetTick, Option<NetTick>)> {
        let c = self.conns.get(&peer)?;
        let ce = c.copy(self.entities.slot_of(id)?)?;
        let base = if c.link.wire == ConnWire::Acked {
            ce.base.as_ref().map(|b| b.tick)
        } else {
            c.acked.map(|a| a.min(ce.last_sent))
        };
        Some((ce.last_sent, base))
    }

    /// Entities whose state is over [`crate::send::MAX_ENTITY_STATE_BYTES`] now, in id order: no
    /// connection receives them, and a copy a client held was taken away (spec 120 § 8.1,
    /// ADR-8419). Each was reported once (`net.state_too_large`) when it crossed the cap.
    pub fn too_large_entities(&self) -> Vec<NetId> {
        let mut ids: Vec<NetId> = self.warned_large.iter().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// Whether a connection is tracked.
    pub fn has_conn(&self, peer: PeerId) -> bool {
        self.conns.contains_key(&peer)
    }

    /// The entity of a `NetId`.
    pub fn entity_of(&self, id: NetId) -> Option<Entity> {
        self.entities.get(id).map(|s| s.entity)
    }

    /// Bumps when a replicated entity is added or removed. The relevance gizmo reuses its mark
    /// list while this, and the interest generation, stay put.
    pub fn entity_generation(&self) -> u64 {
        self.entity_gen
    }
}

/// `net_wake(entity)`: wakes an asleep entity (server).
pub fn net_wake(world: &mut World, entity: Entity) -> bool {
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.remove::<NetAsleep>();
    }
    let id = world.get::<NetId>(entity).copied();
    match (id, world.get_resource_mut::<ServerReplication>()) {
        (Some(id), Some(mut rep)) => rep.wake(id),
        _ => false,
    }
}

/// `net_teleport(entity)`: the next transform update carries the teleport bit (server).
pub fn net_teleport(world: &mut World, entity: Entity) -> bool {
    // Lag compensation never interpolates a rewind across the jump (spec 75 § 4).
    if let Some(mut history) = world.get_resource_mut::<crate::lagcomp::LagCompHistory>() {
        history.mark_cut(entity);
    }
    let id = world.get::<NetId>(entity).copied();
    match (id, world.get_resource_mut::<ServerReplication>()) {
        (Some(id), Some(mut rep)) => rep.teleport(id),
        _ => false,
    }
}

/// `net_resend(entity)`: sends the current state once without leaving sleep (server).
pub fn net_resend(world: &mut World, entity: Entity) -> Vec<PeerId> {
    let id = world.get::<NetId>(entity).copied();
    match (id, world.get_resource_mut::<ServerReplication>()) {
        (Some(id), Some(mut rep)) => rep.resend(id),
        _ => Vec::new(),
    }
}

// ---- server gather -----------------------------------------------------------------------------

const ENTRY_FULL: u64 = 0;
const ENTRY_DELTA: u64 = 1;
const ENTRY_REMOVED: u64 = 2;

/// How `e` was spawned, from the components on it (spec 71 § 4.4).
///
/// `LevelMember` together with `EntityGuid` is a level bind. Otherwise `PrefabInstance` is a
/// prefab spawn. Anything else, including a missing entity, is bare.
pub fn spawn_kind_of(world: &World, e: Entity) -> SpawnKind {
    let Ok(ent) = world.get_entity(e) else {
        return SpawnKind::Bare;
    };
    if let (Some(level), Some(guid)) = (ent.get::<LevelMember>(), ent.get::<EntityGuid>()) {
        return SpawnKind::Level {
            level: level.0,
            guid: guid.0.as_u128(),
        };
    }
    if let Some(p) = ent.get::<PrefabInstance>() {
        return SpawnKind::Prefab(p.prefab);
    }
    SpawnKind::Bare
}

fn replicated_parent(world: &World, e: Entity) -> Option<Entity> {
    let parent = world.get::<ChildOf>(e)?.0;
    world.get::<Replicated>(parent).map(|_| parent)
}

fn depth_of(world: &World, e: Entity) -> u32 {
    let mut d = 0;
    let mut cur = e;
    while let Some(p) = replicated_parent(world, cur) {
        d += 1;
        cur = p;
        if d > 64 {
            break;
        }
    }
    d
}

/// End of a send tick on the server (`NetSet::Assign` + `NetSet::Gather`): records despawns,
/// evaluates sleep, stores the new state versions and builds this tick's replication section for
/// every connection. The runtime wraps each section in the connection's tick frame and sends the
/// state frames beside it. New entities get their ids in [`server_assign`] on other ticks.
pub fn server_gather(world: &mut World, tick: NetTick) -> BTreeMap<PeerId, GatheredConn> {
    let _span = tracing::trace_span!("net_gather").entered();
    world.init_resource::<ServerReplication>();
    world.init_resource::<ReplicationRegistry>();
    world.init_resource::<NetEntities>();
    world.init_resource::<OwnerCache>();
    let registry = world.resource::<ReplicationRegistry>().clone();
    let settings = world
        .get_resource::<crate::settings::NetSettings>()
        .map(|s| s.replication.clone())
        .unwrap_or_default();
    let sleep_enabled = settings.sleep_enabled;
    let this_run = world.read_change_tick();
    let mut rep = world.remove_resource::<ServerReplication>().unwrap_or_default();
    let last_run = rep.last_gather.unwrap_or(Tick::new(0));
    let elapsed = rep.last_send.map_or(1, |t| tick.0.saturating_sub(t.0).max(1));
    rep.last_send = Some(tick);
    rep.copies_visited = 0;
    rep.priority_slots = 0;
    let entities_before = rep.entities.len();

    assign_new(world, &mut rep, tick);
    // Entities assigned on an off tick (`server_assign`) are this send tick's spawns too.
    let spawned_now = std::mem::take(&mut rep.spawned_pending) || rep.entities.len() != entities_before;
    let despawned = collect_despawns(world, &mut rep);

    // Change detection, sleep and state versions.
    let phase = std::time::Instant::now();
    let entities_now = world.resource::<NetEntities>().clone();
    // New entities and despawns change chains too, and so does any owner or link anywhere in the
    // world (a replicated pawn may link to a non-replicated controller).
    let mut owners_dirty = rep.owners_stale;
    let membership_changed = rep.membership_stale || rep.owners_stale;
    rep.owners_stale = false;
    rep.membership_stale = false;
    let sig = owner_signature(world);
    if sig != rep.owner_sig {
        rep.owner_sig = sig;
        owners_dirty = true;
    }
    let ids_of = GatherIds::new(world);
    // `u64::MAX` means the resource is absent. A real generation starts at 0, so inserting
    // or removing `NetProfiles` does not look like the same generation.
    let profile_gen = world
        .get_resource::<crate::profile::NetProfiles>()
        .map(|p| p.generation())
        .unwrap_or(u64::MAX);
    let comp_ids: Vec<Option<ComponentId>> = registry.components().iter().map(|c| c.id).collect();
    let mut unforce: Vec<Entity> = Vec::new();
    // What relevance must look at again, in slot order: a chunk has each entity in cache, and
    // reading them here keeps this out of the serial part of the update.
    let mut touched: Vec<InterestEntity> = Vec::new();
    {
        let wr: &World = world;
        let cx = ChangeInput {
            world: wr,
            registry: &registry,
            ids: &ids_of,
            comp_ids: &comp_ids,
            entities: &entities_now,
            last_run,
            this_run,
            tick,
            elapsed,
            profile_gen,
        };
        let slots = rep.entities.slots_mut();
        let chunk = if slots.len() < PARALLEL_ENTITIES {
            slots.len().max(1)
        } else {
            crate::dense::worker_chunk(slots.len())
        };
        let results = crate::dense::par_mut(slots, chunk, |part| {
            let mut dirty = false;
            let mut unforce = Vec::new();
            let mut touched = Vec::new();
            for s in part.iter_mut().flatten() {
                dirty |= detect_changes(&cx, s, &mut unforce);
                if s.interest_input_changed() {
                    touched.push(interest_row(s));
                }
            }
            (dirty, unforce, touched)
        });
        for (dirty, list, rows) in results {
            owners_dirty |= dirty;
            unforce.extend(list);
            touched.extend(rows);
        }
    }
    for e in unforce {
        if let Ok(mut ent) = world.get_entity_mut(e) {
            ent.remove::<NetAsleep>();
        }
    }
    // Owners: resolved again only when an ownership input changed somewhere (spec 70 § 11: one walk
    // per entity whose parent, link or owner changed; a changed input can move a whole subtree).
    let mut flipped: Vec<u32> = Vec::new();
    if owners_dirty {
        let _span = tracing::trace_span!("net_owner_resolve").entered();
        let mut cache = world.remove_resource::<OwnerCache>().unwrap_or_default();
        cache.clear();
        for s in rep.entities.values_mut() {
            let owner = cache.resolve(world, s.entity);
            s.owner_pending = false;
            if owner != s.owner {
                s.owner = owner;
                s.owner_flipped = true;
                flipped.push(s.slot);
            }
        }
        world.insert_resource(cache);
    } else {
        // New entities with owner inputs resolve their own chain (spec 120 § 8.4: a spawn is not
        // an ownership change anywhere else). They are in `touched` already (a new entity is).
        let pending: Vec<u32> = touched
            .iter()
            .filter(|row| rep.entities.at(row.slot).is_some_and(|s| s.owner_pending))
            .map(|row| row.slot)
            .collect();
        if !pending.is_empty() {
            let mut cache = world.remove_resource::<OwnerCache>().unwrap_or_default();
            for slot in pending {
                if let Some(s) = rep.entities.at_mut(slot) {
                    s.owner_pending = false;
                    let owner = cache.resolve(world, s.entity);
                    if owner != s.owner {
                        s.owner = owner;
                        s.owner_flipped = true;
                        flipped.push(slot);
                    }
                }
            }
            world.insert_resource(cache);
        }
    }
    if !flipped.is_empty() {
        // The rows read before the owners were resolved carry the old owner.
        for row in touched.iter_mut() {
            if let Some(s) = rep.entities.at(row.slot) {
                *row = interest_row(s);
            }
        }
        for slot in &flipped {
            if let Some(s) = rep.entities.at(*slot) {
                touched.push(interest_row(s));
            }
        }
        touched.sort_unstable_by_key(|row| row.slot);
        touched.dedup_by_key(|row| row.slot);
    }
    for (ci, c) in registry.components().iter().enumerate() {
        if !c.options.notify_on_server {
            continue;
        }
        // Slot order, not a copy of the id list: `slots()` ends before `at_mut`.
        let n = rep.entities.slots().len();
        for slot in 0..n as u32 {
            let Some(s) = rep.entities.at_mut(slot) else {
                continue;
            };
            if s.changed.get(ci) != Some(&true) {
                continue;
            }
            let now = (c.snapshot)(world, s.entity);
            if s.server_values.len() < registry.components().len() {
                s.server_values.resize_with(registry.components().len(), || None);
            }
            let prev = s.server_values.get_mut(ci).and_then(|v| v.take());
            let cx = CodecContext::detached();
            let differs = match (&prev, &now) {
                (Some(p), Some(n)) => !(c.eq)(p, n, &cx),
                (None, Some(_)) => true,
                _ => false,
            };
            if let Some(slot) = s.server_values.get_mut(ci) {
                *slot = now;
            }
            if differs {
                let prev = if c.options.keep_previous { prev } else { None };
                let e = s.entity;
                rep.pending_server_notices.push((ci as u16, e, prev));
            }
        }
    }

    rep.timing.changes_ms = phase.elapsed().as_secs_f64() * 1000.0;
    let phase = std::time::Instant::now();
    let relevance = world.get_resource::<NetRelevance>().and_then(|r| r.filter);
    let peers: Vec<PeerId> = rep.conns.keys().copied().collect();
    // Spec 72: relevance pipeline, priority and budgets (when installed).
    let mut interest = world.remove_resource::<NetInterest>();
    if let Some(i) = interest.as_mut() {
        // Profile and grid changes reach entities that did not change themselves. Ownership
        // changes reach exactly the entities whose owner flipped (in `touched`), and spawns and
        // despawns exactly themselves (spec 120 § 8.4).
        let every = i.needs_every_entity(world);
        if every {
            touched.clear();
            touched.extend(rep.entities.iter().map(|(_, s)| interest_row(s)));
        }
        let gone: Vec<u32> = despawned.iter().map(|(_, slot)| *slot).collect();
        // A recorder's connections are not in the pipeline: they have no budget, and their view reads a peer's set.
        let real = peers.partition_point(|p| !crate::recorder::is_virtual_peer(*p));
        i.update_from(
            world,
            &touched,
            every,
            &gone,
            peers.get(..real).unwrap_or(&peers),
            tick,
        );
        follow_recorder_views(&mut rep, i);
    }
    rep.timing.relevance_ms = phase.elapsed().as_secs_f64() * 1000.0;
    rep.timing.relevance_shared_ms =
        rep.timing.relevance_ms - interest.as_ref().map_or(0.0, |i| i.fan_out_ms);
    // A connection that gains an asleep entity gets what the world holds now, not the last version:
    // writes change detection never saw are why it could sleep.
    let joining = rep
        .conns
        .values()
        .any(|c| !c.joined && c.recorder.is_none() && c.link.wire != ConnWire::Undecided);
    let gained: Vec<u32> = if joining {
        rep.entities
            .iter()
            .filter(|(_, s)| s.asleep)
            .map(|(_, s)| s.slot)
            .collect()
    } else {
        interest.as_ref().map_or(Vec::new(), |i| i.gained.clone())
    };
    for slot in gained {
        if let Some(s) = rep.entities.at_mut(slot)
            && s.asleep
            && !s.fresh_version
        {
            refresh_version(world, &registry, s, tick);
        }
    }
    let phase = std::time::Instant::now();
    let send_cfg = SendConfig::from_world(&rep, interest.as_ref(), &settings, crate::runtime::tick_hz(world));
    let entities_map = world.resource::<NetEntities>().clone();
    // Handle fields travel as their asset keys (spec 71 § 3, ADR-2415).
    let asset_cx = world
        .get_resource::<crate::assets::NetAssets>()
        .and_then(|a| a.context());
    // Predicates run only for carriers. An audience marks the entity volatile only when its
    // result changed, so a steady tick queues no update for an unchanged value.
    let mut audience = track_audience(world, &registry, &mut rep);
    // A `Player` recorder sees through its player's groups (spec 103 § 3.3). These entries are
    // removed again before the group sets are kept for change detection.
    let recorder_peers: Vec<(PeerId, PeerId)> = rep
        .conns
        .iter()
        .filter_map(|(peer, conn)| match conn.recorder.as_deref() {
            Some(RecorderRole {
                view: RecorderView::Player(of),
                ..
            }) => Some((*peer, *of)),
            _ => None,
        })
        .collect();
    for (peer, of) in &recorder_peers {
        let groups = audience.peer_groups.get(of).cloned().unwrap_or_default();
        audience.peer_groups.insert(*peer, groups);
    }
    // The entities that may need an update this tick, for connections that already hold every
    // copy (the steady state): they visit only these instead of every replicated entity.
    let structural = owners_dirty || membership_changed || relevance.is_some() || interest.is_some();
    let tick_candidates: Vec<u32> = if structural {
        Vec::new()
    } else {
        rep.entities
            .iter()
            .filter(|(_, s)| {
                s.dirty || s.teleport || s.resend || s.volatile || s.parent_moved || s.fresh_version
            })
            .map(|(_, s)| s.slot)
            .collect()
    };
    let attention: Vec<u32> = rep
        .entities
        .iter()
        .filter(|(_, s)| {
            s.dirty
                || s.teleport
                || s.resend
                || s.volatile
                || s.parent_moved
                || s.fresh_version
                || s.predicted_flipped
                || s.owner_flipped
        })
        .map(|(_, s)| s.slot)
        .collect();
    let mut hot = std::mem::take(&mut rep.hot_scratch);
    tick_hot(rep.entities.slots(), interest.as_ref(), &mut hot);

    // Per connection, in parallel when the pipeline is on: each connection touches only its own
    // state and reads the rest, so its bytes equal a serial run (spec 72 § 11).
    let mut work: Vec<ConnWork> = Vec::with_capacity(peers.len());
    for peer in &peers {
        let Some(conn) = rep.conns.remove(peer) else {
            continue;
        };
        let pint = interest.as_mut().and_then(|i| i.peers.remove(peer));
        work.push(ConnWork {
            peer: *peer,
            conn,
            pint,
            done: None,
        });
    }
    {
        let wr: &World = world;
        let sh = GatherShared {
            mtu: settings.mtu_bytes,
            world: wr,
            rep: &rep,
            registry: &registry,
            predicates: &audience.predicates,
            groups: &audience.peer_groups,
            map: &entities_map,
            assets: asset_cx.as_deref(),
            hot: &hot,
            tick,
            send: &send_cfg,
            interest: interest.as_ref(),
            relevance,
            structural,
            tick_candidates: &tick_candidates,
            despawned: &despawned,
            owners_dirty,
            attention: &attention,
            spawned_now,
        };
        let chunk = if interest.is_none() {
            work.len().max(1)
        } else {
            crate::dense::worker_chunk(work.len())
        };
        let notes = crate::dense::par_mut(&mut work, chunk, |part| {
            let mut caches = TickCaches::default();
            let mut notes: Vec<(u16, u32, u32)> = Vec::new();
            for item in part.iter_mut() {
                gather_conn(&sh, item, &mut caches, &mut notes);
            }
            notes
        });
        let mut too_large: Vec<(NetId, usize)> = Vec::new();
        let mut fit_again: Vec<NetId> = Vec::new();
        let mut loops: Vec<(PeerId, NetId, Vec<&'static str>)> = Vec::new();
        let mut cpu = [0.0f64; 4];
        rep.timing.ordered_candidates = 0;
        let mut update_candidates = 0u64;
        for item in &mut work {
            if let Some(done) = item.done.as_mut() {
                for (sum, s) in cpu.iter_mut().zip(done.cpu) {
                    *sum += s * 1000.0;
                }
                rep.timing.ordered_candidates += done.ordered as u64;
                rep.copies_visited += done.visited;
                rep.priority_slots += done.priority_slots;
                update_candidates += done.updates as u64;
                rep.counters.join_critical_spawns += u64::from(done.critical_sent);
                too_large.append(&mut done.too_large);
                fit_again.append(&mut done.fit_again);
                for (id, comps) in done.loops.drain(..) {
                    loops.push((item.peer, id, comps));
                }
            }
        }
        [
            rep.timing.walk_cpu_ms,
            rep.timing.priority_cpu_ms,
            rep.timing.pack_cpu_ms,
            rep.timing.commit_cpu_ms,
        ] = cpu;
        rep.last_update_candidates = update_candidates;
        for (ci, clamped, refs) in notes.into_iter().flatten() {
            rep.counters.quantize_clamped += u64::from(clamped);
            if refs > 0 {
                rep.counters.refs_not_replicated += u64::from(refs);
                if rep.warned_refs.insert(ci) {
                    let name = registry.get(ci).map_or("?", |c| c.name);
                    tracing::warn!(target: "net", component = name, "net.ref_not_replicated");
                }
            }
        }
        // Logged and counted once per entity per crossing of the cap (ADR-8419): an entity whose
        // record fits again (and that no connection refused this gather) is warned about again if
        // it grows past the cap later.
        for id in fit_again {
            if !too_large.iter().any(|(t, _)| *t == id) {
                rep.warned_large.remove(&id);
            }
        }
        for (id, bytes) in too_large {
            if rep.warned_large.insert(id) {
                tracing::error!(target: "net", net_id = id.0, bytes, "net.state_too_large");
                rep.counters.state_too_large += 1;
            }
        }
        for (peer, id, comps) in loops {
            rep.counters.resync_loops += 1;
            tracing::warn!(target: "net", peer = peer.0, net_id = id.0, components = ?comps, "net.resync_loop");
        }
    }
    rep.hot_scratch = hot;
    rep.predicate_results = audience.predicates;
    for (peer, _) in &recorder_peers {
        audience.peer_groups.remove(peer);
    }
    rep.peer_group_sets = audience.peer_groups;
    let mut out = BTreeMap::new();
    for item in work {
        let ConnWork {
            peer,
            conn,
            pint,
            done,
        } = item;
        let held = conn.held.len();
        rep.conns.insert(peer, conn);
        rep.note_relevant_set(peer, held);
        if let (Some(pi), Some(i)) = (pint, interest.as_mut()) {
            i.peers.insert(peer, pi);
        }
        if let Some(done) = done {
            out.insert(
                peer,
                GatheredConn {
                    section: done.bits,
                    section_empty: done.section_empty,
                    wire: done.wire,
                    packets: done.packets,
                },
            );
        }
    }
    if let Some(i) = interest {
        world.insert_resource(i);
    }
    rep.timing.send_ms = phase.elapsed().as_secs_f64() * 1000.0;

    // Teleport and resend flags are one-shot; sleep entry once every connection acknowledged.
    let ServerReplication { entities, conns, .. } = &mut rep;
    let conns: &BTreeMap<PeerId, ConnReplication> = conns;
    let slots = entities.slots_mut();
    let chunk = if slots.len() < PARALLEL_ENTITIES {
        slots.len().max(1)
    } else {
        crate::dense::worker_chunk(slots.len())
    };
    crate::dense::par_mut(slots, chunk, |part| {
        for s in part.iter_mut().flatten() {
            enter_sleep(s, conns, sleep_enabled);
        }
    });

    rep.entities.reclaim();
    rep.last_gather = Some(this_run);
    world.insert_resource(rep);
    world.increment_change_tick();
    out
}

/// `NetSet::Assign` on a tick that sends nothing: new replicated entities get their ids, so
/// gameplay can name them in messages, whether or not this is a send tick (spec 72 § 3).
pub fn server_assign(world: &mut World, tick: NetTick) {
    world.init_resource::<ServerReplication>();
    world.init_resource::<NetEntities>();
    let mut rep = world.remove_resource::<ServerReplication>().unwrap_or_default();
    assign_new(world, &mut rep, tick);
    world.insert_resource(rep);
}

/// One connection's output of a send tick.
#[derive(Debug)]
pub struct GatheredConn {
    /// The replication section of its tick frame.
    pub section: BitWriter,
    /// The section says nothing (on the state channel the tick frame then needs no sending).
    pub section_empty: bool,
    /// How the connection receives state.
    pub wire: ConnWire,
    /// Its state frames.
    pub packets: Vec<StatePacket>,
}

/// Entities below this many are scanned on the calling thread.
const PARALLEL_ENTITIES: usize = 512;

/// What change detection reads.
struct ChangeInput<'a> {
    world: &'a World,
    registry: &'a ReplicationRegistry,
    ids: &'a GatherIds,
    comp_ids: &'a [Option<ComponentId>],
    entities: &'a NetEntities,
    last_run: Tick,
    this_run: Tick,
    tick: NetTick,
    elapsed: u32,
    /// `NetProfiles::generation` for this gather (`u64::MAX` when the resource is absent).
    profile_gen: u64,
}

/// Refreshes one entity's change flags, sleep and state version; returns whether an ownership
/// input changed.
fn detect_changes(cx: &ChangeInput<'_>, s: &mut ServerEntity, unforce: &mut Vec<Entity>) -> bool {
    let world = cx.world;
    let e = s.entity;
    s.owner_flipped = false;
    let Ok(er) = world.get_entity(e) else {
        return false;
    };
    let ids_of = cx.ids;
    let (last_run, this_run) = (cx.last_run, cx.this_run);
    let st = |id: Option<ComponentId>| ticks_state(&er, id, last_run, this_run);
    let comps = cx.registry.components();
    s.changed.clear();
    let mut any = false;
    let mut comp_any = false;
    // The presence bitset is rebuilt in place (no allocation per entity per tick).
    let words = comps.len().div_ceil(64);
    let mut removed = false;
    if s.present.len() != words {
        s.present.resize(words, 0);
        removed = true;
    }
    let mut word_now = 0u64;
    for (ci, c) in comps.iter().enumerate() {
        let state = match cx.comp_ids.get(ci).copied().flatten() {
            Some(id) => st(Some(id)),
            None => (c.changed)(world, e, last_run, this_run),
        };
        let ch = state == CompState::Changed;
        any |= ch;
        comp_any |= ch;
        s.changed.push(ch);
        if state != CompState::Absent {
            word_now |= 1 << (ci % 64);
        }
        if ci % 64 == 63 || ci + 1 == comps.len() {
            // A removed component shows only as a missing bit (removal has no change tick).
            if let Some(word) = s.present.get_mut(ci / 64) {
                removed |= *word != word_now;
                *word = word_now;
            }
            word_now = 0;
        }
    }
    any |= removed;
    comp_any |= removed;
    // A resend sends what the world holds now, including writes change detection never saw. It
    // needs a new version for that, but it is not a change: the entity stays asleep.
    if s.resend {
        comp_any = true;
        s.changed.fill(true);
    }
    // Re-parenting or a new `Replicated` setting refreshes depth and modes.
    let structure = st(ids_of.child_of) == CompState::Changed || st(ids_of.replicated) == CompState::Changed;
    s.structure = structure;
    if structure {
        s.depth = depth_of(world, e);
        if let Some(r) = er.get::<Replicated>() {
            s.transform_mode = r.transform;
            s.sleep = r.sleep;
        }
    }
    // The root snap grid is resolved once per profile, not once per encoded transform. A stable
    // row keeps `s.quant` and does not call `NetProfiles::get`.
    let has_ref = ids_of.profile_ref.is_some_and(|id| er.contains_id(id));
    let ref_changed = st(ids_of.profile_ref) == CompState::Changed;
    if !s.quant_set || structure || s.quant_gen != cx.profile_gen || has_ref != s.quant_had_ref || ref_changed
    {
        s.quant = crate::profile::entity_position_quantizer(world, e);
        s.gate = crate::profile::entity_extrapolation_gate(world, e);
        s.quant_gen = cx.profile_gen;
        s.quant_set = true;
        s.quant_had_ref = has_ref;
    }
    // A child of a non-replicated parent sends its world transform, which moves with the parent.
    let world_space = has_parent(&er, ids_of.child_of) && replicated_parent(world, e).is_none();
    let t_changed = st(ids_of.transform) == CompState::Changed
        || st(ids_of.velocity) == CompState::Changed
        || (world_space && st(ids_of.global) == CompState::Changed);
    let mut t_refreshed = false;
    if s.transform_mode == TransformReplication::None {
        t_refreshed = s.tcache.is_some();
        s.tcache = None;
    } else if t_changed || structure || s.resend || s.tcache.is_none() {
        let bits = read_transform(world, s);
        t_refreshed = bits != s.tcache;
        s.tcache = bits;
    }
    any |= t_changed && s.transform_mode != TransformReplication::None;
    // Relevance reads positions even when the transform is not replicated.
    s.moved = st(ids_of.transform) == CompState::Changed || st(ids_of.global) == CompState::Changed;
    s.dirty = any || structure;
    let has = |id: Option<ComponentId>| id.is_some_and(|id| er.contains_id(id));
    let owner_now = if has(ids_of.owner) {
        er.get::<crate::model::NetOwner>().map(|o| o.0)
    } else {
        None
    };
    let link_now = if has(ids_of.owner_link) {
        er.get::<crate::model::NetOwnerLink>().map(|l| l.0)
    } else {
        None
    };
    let parent_now = if has(ids_of.child_of) {
        er.get::<ChildOf>().map(|p| p.0)
    } else {
        None
    };
    let inputs = (owner_now, link_now, parent_now);
    let changed = inputs != s.owner_inputs;
    if changed {
        s.owner_inputs = inputs;
    }
    // A new entity's first read is not a change of the world's owner chains: the gather resolves
    // its owner alone.
    let owners_dirty = changed && s.owner_known;
    let first_read = !s.owner_known;
    s.owner_known = true;
    s.owner_pending = first_read && changed;
    let predicted_was = s.predicted;
    s.predicted = has(ids_of.predicted);
    let parent_net = match inputs.2 {
        Some(p) if world.get::<Replicated>(p).is_some() => cx.entities.net_id(p),
        _ => None,
    };
    s.parent_moved = parent_net != s.parent_net;
    s.parent_net = parent_net;
    // A root is sent in the shared frame on its profile's grid, a child as is (ADR-2413 (4)): a
    // parent that came, went or stopped replicating re-reads the transform. Removing `ChildOf`
    // has no change tick, so `structure` above does not see it.
    if s.parent_moved && s.transform_mode != TransformReplication::None {
        let bits = read_transform(world, s);
        t_refreshed |= bits != s.tcache;
        s.tcache = bits;
    }
    // Audience changes are applied after this pass, once predicates and groups are compared
    // with the previous gather. A component write still sets `dirty` above.
    s.volatile = false;
    let forced = has(ids_of.asleep);
    s.forced = forced;
    s.priority_scale = if has(ids_of.priority_scale) {
        er.get::<NetPriorityScale>().map_or(1.0, |p| p.factor())
    } else {
        1.0
    };
    s.heat = if has(ids_of.combat_heat) {
        er.get::<NetCombatHeat>()
            .map_or((0.0, 0.0), |h| (h.at_tick(cx.tick), h.radius_m))
    } else {
        (0.0, 0.0)
    };
    s.predicted_flipped = predicted_was != s.predicted;
    if any {
        s.last_change = cx.tick;
        s.quiet_ticks = 0;
    } else {
        s.quiet_ticks = s.quiet_ticks.saturating_add(cx.elapsed);
    }
    if s.asleep && (any || structure || (s.by_marker && !forced)) {
        s.asleep = false;
        s.started = true;
        if forced {
            unforce.push(e);
        }
    }
    // A new state version (spec 72 § 4.1), shared by every connection. What did not change keeps
    // the previous version's values.
    s.fresh_version = false;
    if s.current.is_none() || comp_any || t_refreshed || s.parent_moved {
        let previous = s.current.as_deref();
        let comps_now = match previous {
            Some(p) if !comp_any => p.comps.clone(),
            _ => Arc::new(snapshot_comps(world, cx.registry, s, previous)),
        };
        s.current = Some(Arc::new(StateVersion {
            tick: cx.tick,
            number: previous.map_or(0, |p| p.number.wrapping_add(1)),
            parent: s.parent_net,
            transform: s.tcache,
            comps: comps_now,
        }));
        s.fresh_version = true;
    }
    owners_dirty
}

/// The encoded transform of an entity as the world holds it now.
fn read_transform(world: &World, s: &ServerEntity) -> Option<TransformBits> {
    let parent_replicated = replicated_parent(world, s.entity).is_some();
    // A child stays unsnapped even when the cache holds a root quantizer.
    let quant = if parent_replicated { None } else { s.quant };
    server_transform(world, s.entity, parent_replicated, quant)
        .map(|t| TransformBits::from_net(s.transform_mode, &t, quant))
}

/// A new version with everything read from the world again.
fn refresh_version(world: &World, registry: &ReplicationRegistry, s: &mut ServerEntity, tick: NetTick) {
    s.changed.fill(true);
    if s.transform_mode != TransformReplication::None {
        s.tcache = read_transform(world, s);
    }
    let previous = s.current.as_deref();
    let comps = Arc::new(snapshot_comps(world, registry, s, previous));
    let number = previous.map_or(0, |p| p.number.wrapping_add(1));
    s.current = Some(Arc::new(StateVersion {
        tick,
        number,
        parent: s.parent_net,
        transform: s.tcache,
        comps,
    }));
    s.fresh_version = true;
}

/// The replicated component values of an entity for a new version: changed ones are read from the
/// world, the rest are shared with `previous`.
fn snapshot_comps(
    world: &World,
    registry: &ReplicationRegistry,
    s: &ServerEntity,
    previous: Option<&StateVersion>,
) -> Vec<(u16, ErasedComp)> {
    let mut out = Vec::new();
    for (ci, c) in registry.components().iter().enumerate() {
        if !s.present.get(ci / 64).is_some_and(|w| w & (1 << (ci % 64)) != 0) {
            continue;
        }
        let ci = ci as u16;
        let kept = previous
            .filter(|_| !s.changed.get(usize::from(ci)).copied().unwrap_or(true))
            .and_then(|p| p.comp(ci));
        let value = match kept {
            Some(v) => Some(v.clone()),
            None => (c.snapshot)(world, s.entity),
        };
        if let Some(v) = value {
            out.push((ci, v));
        }
    }
    out
}

const HOT_LIVE: u8 = 1;
const HOT_DIRTY: u8 = 1 << 1;
const HOT_TELEPORT: u8 = 1 << 2;
const HOT_RESEND: u8 = 1 << 3;
const HOT_VOLATILE: u8 = 1 << 4;
const HOT_ASLEEP: u8 = 1 << 5;
const HOT_PREDICTED: u8 = 1 << 6;

/// One entity as a connection's walk reads it this tick (a compact copy of `ServerEntity`).
#[derive(Debug, Clone, Copy)]
struct TickHot {
    id: NetId,
    owner: PeerId,
    /// The number of the current state version (`NO_VERSION` = none).
    number: u32,
    /// The entity's row in the interest profile table.
    profile: u32,
    flags: u8,
    /// Its [`NetPriorityScale`] factor.
    priority_scale: f32,
    /// Its [`NetCombatHeat`] and the heat's reach in metres.
    heat: (f32, f32),
}

impl Default for TickHot {
    fn default() -> Self {
        Self {
            id: NetId(0),
            owner: PeerId::SERVER,
            number: NO_VERSION,
            profile: u32::MAX,
            flags: 0,
            priority_scale: 1.0,
            heat: (0.0, 0.0),
        }
    }
}

impl TickHot {
    fn of(s: &ServerEntity, interest: Option<&NetInterest>) -> Self {
        let mut flags = HOT_LIVE;
        for (on, bit) in [
            (s.dirty, HOT_DIRTY),
            (s.teleport, HOT_TELEPORT),
            (s.resend, HOT_RESEND),
            (s.volatile, HOT_VOLATILE),
            (s.asleep, HOT_ASLEEP),
            (s.predicted, HOT_PREDICTED),
        ] {
            if on {
                flags |= bit;
            }
        }
        Self {
            id: s.id,
            owner: s.owner,
            number: s.current.as_ref().map_or(NO_VERSION, |v| v.number),
            profile: interest.map_or(u32::MAX, |i| i.profile_row(s.slot)),
            flags,
            priority_scale: s.priority_scale,
            heat: s.heat,
        }
    }

    fn has(self, bit: u8) -> bool {
        self.flags & bit != 0
    }
}

/// Ends an entity's one-shot flags for this gather and lets it fall asleep.
fn enter_sleep(s: &mut ServerEntity, conns: &BTreeMap<PeerId, ConnReplication>, enabled: bool) {
    s.teleport = false;
    s.resend = false;
    if !enabled {
        s.asleep = false;
        return;
    }
    if s.asleep {
        return;
    }
    let wants = s.forced
        || match s.sleep {
            NetSleep::Never => false,
            NetSleep::StartAsleep => s.quiet_ticks > 0,
            NetSleep::AfterQuiet { quiet_ticks } => s.quiet_ticks >= quiet_ticks,
        };
    if !wants {
        return;
    }
    let current = s.current.as_ref().map_or(NO_VERSION, |v| v.number);
    // A recorder has what was sent as it was sent, and must not move when an entity sleeps (spec 103 § 3.4).
    let all_acked = conns.values().filter(|c| c.recorder.is_none()).all(|c| {
        c.copy(s.slot).is_none_or(|ce| {
            ce.out_of_scope || (ce.base_version() == current && c.acked_at_least(ce.last_sent))
        })
    });
    if all_acked {
        s.asleep = true;
        s.by_marker = s.forced;
    }
}

/// The compact entity array of this tick, by slot. `hot` is reused across gathers.
fn tick_hot(slots: &[Option<ServerEntity>], interest: Option<&NetInterest>, hot: &mut Vec<TickHot>) {
    hot.clear();
    hot.resize(slots.len(), TickHot::default());
    let fill = |at: usize, part: &mut [TickHot]| {
        for (h, s) in part.iter_mut().zip(slots.get(at..).unwrap_or(&[])) {
            if let Some(s) = s {
                *h = TickHot::of(s, interest);
            }
        }
    };
    if slots.len() < PARALLEL_ENTITIES {
        fill(0, hot);
    } else {
        let chunk = crate::dense::worker_chunk(slots.len());
        regia_core::tasks::par_chunks_mut(hot, chunk, |i, part| fill(i * chunk, part));
    }
}

/// What every connection's gather reads.
struct GatherShared<'a> {
    mtu: u32,
    world: &'a World,
    rep: &'a ServerReplication,
    registry: &'a ReplicationRegistry,
    predicates: &'a HashMap<(NetId, u16), bool>,
    groups: &'a HashMap<PeerId, HashSet<String>>,
    map: &'a NetEntities,
    /// The asset-server context `Handle<T>` fields encode through (ADR-2415).
    assets: Option<&'a crate::codec::AssetContext>,
    hot: &'a [TickHot],
    tick: NetTick,
    send: &'a SendConfig,
    interest: Option<&'a NetInterest>,
    relevance: Option<fn(&World, Entity, PeerId) -> bool>,
    structural: bool,
    tick_candidates: &'a [u32],
    despawned: &'a [(NetId, u32)],
    /// An owner or owner-link input changed this gather.
    owners_dirty: bool,
    /// Slots whose state may need a record even on a quiet acknowledged connection.
    attention: &'a [u32],
    /// `assign_new` gave an entity an id during this gather.
    spawned_now: bool,
}

impl GatherShared<'_> {
    fn hot_at(&self, slot: u32) -> TickHot {
        self.hot.get(slot as usize).copied().unwrap_or_default()
    }

    fn audience(&self, peer: PeerId) -> AudienceInput<'_> {
        AudienceInput {
            world: self.world,
            peer,
            predicates: self.predicates,
            groups: self.groups,
        }
    }
}

/// One connection through the gather.
struct ConnWork {
    peer: PeerId,
    conn: ConnReplication,
    pint: Option<PeerInterest>,
    done: Option<ConnDone>,
}

/// What one connection's gather produced.
struct ConnDone {
    bits: BitWriter,
    section_empty: bool,
    wire: ConnWire,
    packets: Vec<StatePacket>,
    too_large: Vec<(NetId, usize)>,
    /// Ids warned as too large whose record fits again (ADR-8419).
    fit_again: Vec<NetId>,
    /// Resync loops to log.
    loops: Vec<(NetId, Vec<&'static str>)>,
    /// CPU seconds: walk, priority, pack, commit.
    cpu: [f64; 4],
    /// Ranking candidates put in order.
    ordered: usize,
    /// Held copies this connection's walk entered.
    visited: u64,
    /// Packed interest slots this connection's priority pass read.
    priority_slots: u64,
    /// Copies that may need an update record this tick.
    updates: usize,
    /// Join-critical spawns sent outside the spawn budget (spec 77 § 6.1).
    critical_sent: u32,
}

/// One candidate of a connection's priority ranking.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ranked {
    /// The send order and the id, as one integer ([`rank_key`]).
    key: u64,
    slot: u32,
    depth: u32,
    gain: f32,
    spawn: bool,
}

impl Ranked {
    fn id(&self) -> NetId {
        NetId(self.key as u32)
    }

    fn forced(&self) -> bool {
        self.key >> 32 == 0
    }
}

/// A delta record costs about this much at least: a budget can pick about `bytes / this` deltas,
/// so only that many candidates are sorted first. Smaller records reach the next window of the
/// ranking: this sets the speed, not the order.
const MIN_RECORD_BYTES: f64 = 8.0;

/// Candidates beyond what a budget can pick by size, ranked anyway (cheap headroom for records
/// that turn out to carry nothing).
const RANK_MARGIN: usize = 16;

/// Puts the `count` smallest of `ranked[from..]` in order at `from`, leaving the rest unsorted, and
/// returns where the ordered part ends.
fn order_window(ranked: &mut [Ranked], from: usize, count: usize) -> usize {
    let Some(tail) = ranked.get_mut(from..) else {
        return ranked.len();
    };
    let count = count.max(1);
    if tail.len() > count {
        tail.select_nth_unstable_by_key(count, |r| r.key);
    }
    let head = tail.len().min(count);
    if let Some(window) = tail.get_mut(..head) {
        window.sort_unstable_by_key(|r| r.key);
    }
    from + head
}

/// What one held copy contributed to the walk.
struct CopyStep {
    irregular: bool,
    expired: bool,
}

/// Per-copy inputs that are not already on the shared gather or the copy itself.
struct CopyAsk<'a> {
    peer: PeerId,
    /// Whose view a game's relevance filter judges (`None`: no filter, a recorder of everything).
    filter: Option<PeerId>,
    link: &'a StateLink,
    acked: bool,
    /// The interest bit (everything, when the pipeline is off).
    wanted: bool,
}

/// One held copy: scope, a repeated spawn, or an update.
fn step_copy(
    sh: &GatherShared<'_>,
    hot: TickHot,
    ce: &mut ConnEntity,
    ask: &CopyAsk<'_>,
    lists: &mut WalkLists<'_>,
) -> CopyStep {
    let entity = if sh.relevance.is_some() {
        sh.rep.entities.at(ce.slot).map(|s| s.entity)
    } else {
        None
    };
    let passes = sh.relevance.is_none_or(|f| {
        ask.filter
            .is_none_or(|p| entity.is_some_and(|e| f(sh.world, e, p)))
    });
    let relevant = ask.wanted && passes;
    if ask.acked && relevant && !ce.out_of_scope && !ce.full_again {
        return CopyStep {
            irregular: false,
            expired: visit_acked(hot, ce, ask.peer, ask.link, sh.tick, lists),
        };
    }
    CopyStep {
        irregular: classify(hot, ce, relevant, ask.peer, lists),
        expired: false,
    }
}

/// Whether a copy must be walked again: it is behind, in flight, or still owes a record.
fn copy_needs_watch(ce: &ConnEntity, link: &StateLink, tick: NetTick, hot: TickHot) -> bool {
    if !hot.has(HOT_LIVE) {
        return false;
    }
    if ce.out_of_scope
        || ce.full_again
        || ce.base.is_none()
        || !ce.spawn_acked
        || ce.teleport_due
        || ce.resend_due
        || ce.base_number != hot.number
        // Held in its far form: the exact state is owed when the copy leaves the far tier, which
        // no change of the entity announces (ADR-10124).
        || ce.base_far
    {
        return true;
    }
    ce.sent_seq != NO_PACKET && link.unresolved(ce.sent_seq, tick)
}

/// The copy of `slot`, from a split borrow of the connection.
fn held_at<'a>(held: &'a mut [ConnEntity], index: &[u32], slot: u32) -> Option<&'a mut ConnEntity> {
    match index.get(slot as usize) {
        Some(i) if *i != NO_SLOT => held.get_mut(*i as usize),
        _ => None,
    }
}

/// A slot the connection does not hold yet, when it is relevant.
fn consider_spawn(
    sh: &GatherShared<'_>,
    index: &[u32],
    filter: Option<PeerId>,
    id: NetId,
    slot: u32,
    in_set: bool,
    lists: &mut WalkLists<'_>,
) {
    if index.get(slot as usize).is_some_and(|i| *i != NO_SLOT) {
        return;
    }
    let Some(s) = sh.rep.entities.at(slot) else {
        return;
    };
    let entity = sh.relevance.is_some().then_some(s.entity);
    let passes = sh
        .relevance
        .is_none_or(|f| filter.is_none_or(|p| entity.is_some_and(|e| f(sh.world, e, p))));
    if in_set && passes {
        lists.spawns.push((s.depth, id, slot));
    }
}

/// A relevant entity still has no copy (a spawn the budget did not send).
/// Without the pipeline (with it, `owed_slots` says it).
fn owes_spawn(conn: &ConnReplication, rep: &ServerReplication) -> bool {
    rep.entities
        .ids()
        .iter()
        .any(|(_, slot)| conn.copy(*slot).is_none())
}

/// One connection's tick: resyncs, the walk over its copies, ownership bits, priority ranking,
/// encoding, packing and commit.
fn gather_conn(
    sh: &GatherShared<'_>,
    item: &mut ConnWork,
    caches: &mut TickCaches,
    notes: &mut Vec<(u16, u32, u32)>,
) {
    let peer = item.peer;
    let conn = &mut item.conn;
    let tick = sh.tick;
    let rep = sh.rep;
    let mut w = BitWriter::new();
    let wire = conn.link.wire;
    if wire == ConnWire::Undecided {
        // The client's first `ACK` decides; until then only empty tick frames go (they draw it).
        for _ in 0..5 {
            w.write_varint(0);
        }
        item.done = Some(ConnDone {
            bits: w,
            section_empty: true,
            wire,
            packets: Vec::new(),
            too_large: Vec::new(),
            fit_again: Vec::new(),
            loops: Vec::new(),
            cpu: [0.0; 4],
            ordered: 0,
            visited: 0,
            priority_slots: 0,
            updates: 0,
            critical_sent: 0,
        });
        return;
    }
    let acked = wire == ConnWire::Acked;
    let walk_start = std::time::Instant::now();
    // The connection's spare writers serve this turn of the worker's scratch.
    std::mem::swap(&mut caches.writers, &mut conn.writers);
    let mut loops: Vec<(NetId, Vec<&'static str>)> = Vec::new();
    let join = !conn.joined;
    conn.joined = true;
    // Resyncs keep the copy tracked (so a despawn or scope loss still reaches it) and send a
    // full spawn record again.
    let resync = std::mem::take(&mut conn.resync);
    let mut later: Vec<NetId> = Vec::new();
    for id in resync {
        if let Some(pi) = item.pint.as_mut() {
            // Spec 72 § 9: 10 per second per client; three of one entity within 5 s is a loop.
            let (admitted, looped) = pi.send.admit_resync(id, tick, sh.send.tick_hz);
            if looped {
                let comps: Vec<&str> = rep.entities.get(id).map_or(Vec::new(), |s| {
                    sh.registry
                        .components()
                        .iter()
                        .enumerate()
                        .filter(|(ci, _)| s.present.get(ci / 64).is_some_and(|w| w & (1 << (ci % 64)) != 0))
                        .map(|(_, c)| c.name)
                        .collect()
                });
                loops.push((id, comps));
            }
            if !admitted {
                // Over the rate: kept for a later tick (the client asks only once).
                later.push(id);
                continue;
            }
        }
        if let Some(slot) = rep.entities.slot_of(id)
            && let Some(ce) = conn.copy_mut(slot)
        {
            ce.full_again = true;
            conn.irregular = true;
            conn.watch.set(slot);
        }
    }
    conn.resync.extend(later);
    if conn.recorder.as_ref().is_some_and(|r| r.heal) {
        heal_recorder_references(sh, conn);
    }

    // Spawn records (parents first: depth, then NetId).
    // One pass: spawn candidates, out-of-scope copies, ownership bits that changed, and the
    // copies that may need an update (the rest provably match what the connection holds).
    let mut scratch = std::mem::take(&mut conn.scratch);
    let mut spawns = std::mem::take(&mut scratch.spawns);
    let mut scope_out = std::mem::take(&mut scratch.scope_out);
    let mut own_changes = std::mem::take(&mut scratch.own_changes);
    spawns.clear();
    scope_out.clear();
    own_changes.clear();
    let mut visited = 0u64;
    // Acknowledged connections do not take the steady shortcut: a copy can be behind without the
    // entity being dirty this tick. They walk the watch set instead, once it has been built.
    // An ownership change visits the copies whose owner flipped (`attention`), not every copy
    // (spec 120 § 8.4); a connection without the pipeline still walks them all.
    let force_full =
        !conn.watch_built || join || (sh.owners_dirty && item.pint.is_none()) || sh.relevance.is_some();
    let steady =
        !acked && !sh.structural && !join && conn.held.len() == rep.entities.len() && !conn.irregular;
    // The watch list is a sorted visit of scattered copies; once it names most of what the
    // connection holds (a budget-starved connection keeps most copies behind), the in-order walk
    // of every copy is cheaper and gives the same result.
    let partial = acked && !force_full && conn.watch.count() * 2 < conn.held.len();
    let mut visit_slots = std::mem::take(&mut scratch.visit);
    visit_slots.clear();
    if partial {
        conn.watch.for_each(|slot| {
            if conn.copy(slot).is_some() {
                visit_slots.push(slot);
            }
        });
        for slot in sh.attention {
            if conn.copy(*slot).is_some() {
                visit_slots.push(*slot);
            }
        }
        if let Some(pi) = item.pint.as_ref() {
            for slot in pi.gained_slots.iter().chain(pi.lost_slots.iter()) {
                if conn.copy(*slot).is_some() {
                    visit_slots.push(*slot);
                }
            }
        }
        visit_slots.sort_unstable();
        visit_slots.dedup();
    }
    let mut updates = std::mem::take(&mut scratch.updates);
    updates.clear();
    updates.reserve(if partial {
        visit_slots.len()
    } else {
        conn.held.len()
    });
    if steady {
        for slot in sh.tick_candidates {
            if let Some(ce) = conn.copy_mut(*slot) {
                visited += 1;
                visit_held(sh.hot_at(*slot), ce, peer, &mut updates, &mut own_changes);
            }
        }
    }
    let mut still_irregular = false;
    let mut refresh_owed = false;
    let mut lists = WalkLists {
        spawns: &mut spawns,
        scope_out: &mut scope_out,
        updates: &mut updates,
        own_changes: &mut own_changes,
    };
    if steady {
        // Every copy is held and nothing is deferred: only this tick's candidates.
        conn.spawn_owed = false;
    } else {
        // Only the connection's relevant (or lingering) entities and the copies it holds are
        // visited (spec 72); without the pipeline every entity is relevant. An acknowledged
        // connection that already holds its set visits the outstanding copies, not every copy.
        let pint = item.pint.as_ref();
        let ConnReplication {
            held,
            link,
            index,
            watch,
            watch_built,
            spawn_owed,
            owed_slots,
            recorder,
            ..
        } = &mut *conn;
        let mut expired = 0u64;
        let view_bits = recorder.as_ref().and_then(|r| r.bits.as_ref());
        let in_set =
            |slot: u32| pint.is_none_or(|pi| pi.held_bits.get(slot)) && view_bits.is_none_or(|b| b.get(slot));
        // Whose eyes a game's filter looks through: a recorder's view, or nobody's for `Everything`.
        let filter = match recorder.as_ref().map(|r| (r.view, r.frozen)) {
            None => Some(peer),
            Some((RecorderView::Player(of), false)) => Some(of),
            Some((RecorderView::Player(_), true) | (RecorderView::Everything, _)) => None,
        };
        if partial {
            let mut keep = std::mem::take(&mut scratch.keep);
            keep.clear();
            keep.reserve(visit_slots.len());
            for slot in &visit_slots {
                let Some(ce) = held_at(held, index, *slot) else {
                    keep.push((*slot, false));
                    continue;
                };
                let hot = sh.hot_at(*slot);
                if !hot.has(HOT_LIVE) {
                    keep.push((*slot, false));
                    continue;
                }
                visited += 1;
                let step = step_copy(
                    sh,
                    hot,
                    ce,
                    &CopyAsk {
                        peer,
                        filter,
                        link,
                        acked: true,
                        wanted: in_set(*slot),
                    },
                    &mut lists,
                );
                still_irregular |= step.irregular;
                expired += u64::from(step.expired);
                keep.push((*slot, copy_needs_watch(ce, link, tick, hot)));
            }
            for (slot, stays) in keep.drain(..) {
                if stays {
                    watch.set(slot);
                } else {
                    watch.unset(slot);
                }
            }
            scratch.keep = keep;
            match pint {
                // Spawn candidates are what became relevant this tick and what is still owed, not
                // the whole held set: a spawn elsewhere costs this connection nothing (spec 120
                // § 8.4). Relevance lists every new entity it holds in `gained_slots`.
                Some(pi) => {
                    if !pi.gained_slots.is_empty() || !owed_slots.is_empty() {
                        refresh_owed = true;
                        for slot in pi.gained_slots.iter().chain(owed_slots.iter()) {
                            let id = sh.hot_at(*slot).id;
                            consider_spawn(sh, index, filter, id, *slot, in_set(*slot), &mut lists);
                        }
                    }
                }
                None => {
                    if sh.spawned_now || *spawn_owed {
                        refresh_owed = true;
                        rep.entities.ids().iter().for_each(|(id, slot)| {
                            consider_spawn(sh, index, filter, *id, *slot, in_set(*slot), &mut lists);
                        });
                    }
                }
            }
        } else {
            // Copies of what the connection should hold: when that is everything, the search for
            // spawn candidates below has nothing to find.
            let mut covered = 0usize;
            // The visit list is not used on this path: its buffer holds the copies to watch.
            let mut watch_keep = std::mem::take(&mut visit_slots);
            watch_keep.clear();
            for ce in held.iter_mut() {
                let hot = sh.hot_at(ce.slot);
                if !hot.has(HOT_LIVE) {
                    continue;
                }
                visited += 1;
                let wanted = in_set(ce.slot);
                covered += usize::from(wanted);
                let step = step_copy(
                    sh,
                    hot,
                    ce,
                    &CopyAsk {
                        peer,
                        filter,
                        link,
                        acked,
                        wanted,
                    },
                    &mut lists,
                );
                still_irregular |= step.irregular;
                expired += u64::from(step.expired);
                if acked && copy_needs_watch(ce, link, tick, hot) {
                    watch_keep.push(ce.slot);
                }
            }
            if acked {
                // Cleared in place: the words keep their buffer.
                watch.reset(0);
                for slot in &watch_keep {
                    watch.set(*slot);
                }
                *watch_built = true;
            }
            visit_slots = watch_keep;
            refresh_owed = true;
            match pint {
                Some(pi) if covered == pi.held.len() => {}
                Some(pi) => pi.held.iter().for_each(|(id, slot)| {
                    consider_spawn(sh, index, filter, *id, *slot, in_set(*slot), &mut lists);
                }),
                None => rep.entities.ids().iter().for_each(|(id, slot)| {
                    consider_spawn(sh, index, filter, *id, *slot, in_set(*slot), &mut lists);
                }),
            }
        }
        link.counters.baselines_expired += expired;
    }
    if !steady {
        conn.irregular = still_irregular;
    }
    // The extrapolation gate (spec 120 § 8.2, ADR-8522): on the acknowledged state channel, a
    // gated entity whose only news is a transform the client's extrapolation still shows within
    // the gate is not offered. Its copy stays behind (and watched), so it is checked again next
    // send tick.
    if acked {
        let hz = sh.send.tick_hz;
        let ConnReplication {
            held, index, link, ..
        } = &mut *conn;
        let before = updates.len();
        updates.retain(|(_, slot)| {
            let (Some(s), Some(ce)) = (rep.entities.at(*slot), held_at(held, index, *slot)) else {
                return true;
            };
            !gate_holds_back(s, ce, link, tick, hz)
        });
        link.counters.gate_held += (before - updates.len()) as u64;
    }
    // The ranking orders a budgeted connection's updates; the rest go in id order.
    if item.pint.is_none() {
        updates.sort_unstable();
    }
    scope_out.sort_unstable();
    own_changes.sort_unstable_by_key(|(id, _, _)| *id);
    // Parents first: depth is computed fresh for the candidates (a parent may have become
    // replicated since the child's depth was cached).
    for (d, _, slot) in spawns.iter_mut() {
        if let Some(s) = rep.entities.at(*slot) {
            *d = depth_of(sh.world, s.entity);
        }
    }
    spawns.sort_unstable();
    // A slot both gained and owed is one candidate.
    spawns.dedup_by_key(|(_, _, slot)| *slot);
    w.write_varint(own_changes.len() as u64);
    for (id, owned, predicted) in &own_changes {
        w.write_varint(u64::from(id.0));
        w.write_bool(*owned);
        w.write_bool(*predicted);
    }
    let mut section_empty = own_changes.is_empty();

    let mut prepared_spawns: Vec<PreparedSpawn> = Vec::new();
    let mut prepared: Vec<PreparedUpdate> = Vec::new();
    let mut too_large: Vec<(NetId, usize)> = Vec::new();
    let mut fit_again: Vec<NetId> = Vec::new();
    let walk_s = walk_start.elapsed().as_secs_f64();
    let (mut priority_s, mut pack_s, mut ordered) = (0.0, 0.0, 0);
    let mut priority_slots = 0u64;
    let mut critical_sent = 0u32;
    match item.pint.as_mut() {
        None => {
            // No pipeline: everything goes (no spawn budget, so no join window either).
            conn.join_spawns_done = true;
            for (depth, id, slot) in &spawns {
                if let Some(sp) = enc_spawn(sh, peer, acked, caches, notes, *depth, *id, *slot) {
                    prepared_spawns.push(sp);
                }
            }
            for (id, slot) in &updates {
                if let Some(p) = enc_update(sh, peer, conn, caches, notes, *id, *slot, false) {
                    prepared.push(p);
                }
            }
        }
        Some(pi) => {
            let mut packed = pack_conn(
                sh,
                peer,
                conn,
                pi,
                PackInput {
                    spawns: &spawns,
                    updates: &updates,
                },
                caches,
                notes,
            );
            prepared_spawns = std::mem::take(&mut packed.spawns);
            prepared = std::mem::take(&mut packed.updates);
            too_large = std::mem::take(&mut packed.too_large);
            fit_again = std::mem::take(&mut packed.fit_again);
            for (slot, id, number) in packed.refused.drain(..) {
                conn.refused.insert(slot, (id, number));
            }
            // A withdrawn copy leaves the client the way an out-of-scope copy does, and the
            // entity is owed a spawn again (the partial walk only searches when one is owed).
            refresh_owed |= !packed.withdraw.is_empty();
            scope_out.append(&mut packed.withdraw);
            scope_out.sort_unstable();
            scope_out.dedup();
            priority_s = packed.priority_s;
            pack_s = packed.pack_s;
            ordered = packed.ordered;
            priority_slots = packed.priority_slots;
            critical_sent = packed.critical_sent;
            if packed.critical_pending == 0 {
                conn.join_spawns_done = true;
            }
        }
    }
    let commit_start = std::time::Instant::now();
    let second = (f64::from(tick.0) / sh.send.tick_hz) as u64;

    // Spawns.
    prepared_spawns.sort_by_key(|s| (s.depth, s.id));
    let mut gained_copies: Vec<NetId> = Vec::new();
    let mut records: Vec<OutRecord> = Vec::with_capacity(prepared_spawns.len() + prepared.len());
    if acked {
        w.write_varint(0);
    } else {
        w.write_varint(prepared_spawns.len() as u64);
    }
    for mut sp in prepared_spawns {
        conn.refused.remove(&sp.ce.slot);
        if let Some(pi) = item.pint.as_mut() {
            charge_bandwidth(
                pi,
                sh.registry,
                second,
                sp.id,
                sp.bits.bit_len().div_ceil(8),
                &sp.comp_bytes,
            );
        }
        if !acked {
            w.append(&sp.bits);
            if conn.recorder.is_some() && conn.copy(sp.ce.slot).is_none() {
                gained_copies.push(sp.id);
            }
            conn.insert(sp.ce);
            continue;
        }
        // The copy's baseline stays empty until the spawn is acknowledged (spec 72 § 4.2).
        let repeat = conn.copy(sp.ce.slot);
        let kind = if repeat.is_some() { KIND_REPEAT } else { KIND_SPAWN };
        if let Some(old) = repeat {
            sp.ce.born = old.born;
            sp.ce.spawn_acked = old.spawn_acked;
        }
        let Some(version) = sp.ce.base.take() else {
            continue;
        };
        sp.ce.sent_version = version.number;
        records.push(OutRecord {
            kind,
            version_ago: u64::from(tick.0.saturating_sub(version.tick.0)),
            base_ago: None,
            fields: sp.bits,
            sent: SentRecord {
                slot: sp.ce.slot,
                id: sp.id,
                version,
                unmatched: sp.ce.unmatched.clone(),
                teleport: false,
                resend: false,
                far: false,
            },
        });
        conn.insert(sp.ce);
    }

    // Updates.
    let mut update_bits: Vec<(NetId, BitWriter)> = Vec::new();
    let mut far_sent = 0u64;
    for p in prepared {
        let current = rep.entities.at(p.slot).and_then(|s| s.current.as_ref());
        let Some(ce) = conn.copy_mut(p.slot) else {
            continue;
        };
        if acked && p.any {
            let (Some(version), true) = (current, ce.base.is_some()) else {
                continue;
            };
            let base_tick = ce.base_tick;
            ce.last_sent = tick;
            ce.sent_version = version.number;
            if rep.entities.at(p.slot).is_some_and(|s| s.gate.is_some()) {
                // Placed below (the packet number); a record that is never placed stays
                // `NO_PACKET` and is pruned as resolved.
                ce.flight.push((NO_PACKET, Arc::clone(version)));
            }
            if let Some(pi) = item.pint.as_mut() {
                charge_bandwidth(
                    pi,
                    sh.registry,
                    second,
                    p.id,
                    p.bits.bit_len().div_ceil(8),
                    &p.comp_bytes,
                );
            }
            far_sent += u64::from(p.far);
            records.push(OutRecord {
                kind: if p.far { KIND_FAR } else { KIND_UPDATE },
                version_ago: u64::from(tick.0.saturating_sub(version.tick.0)),
                base_ago: Some(u64::from(version.tick.0.saturating_sub(base_tick.0))),
                fields: p.bits,
                sent: SentRecord {
                    slot: p.slot,
                    id: p.id,
                    version: Arc::clone(version),
                    unmatched: p.unmatched,
                    teleport: ce.teleport_due,
                    resend: ce.resend_due,
                    far: p.far,
                },
            });
            continue;
        }
        if acked {
            ce.sent_version = current.map_or(NO_VERSION, |v| v.number);
        }
        ce.base_number = current.map_or(0, |v| v.number);
        ce.base = current.cloned();
        ce.unmatched = p.unmatched;
        ce.teleport_due = false;
        ce.resend_due = false;
        if p.any {
            ce.last_sent = tick;
            if let Some(pi) = item.pint.as_mut() {
                charge_bandwidth(
                    pi,
                    sh.registry,
                    second,
                    p.id,
                    p.bits.bit_len().div_ceil(8),
                    &p.comp_bytes,
                );
            }
            update_bits.push((p.id, p.bits));
        }
    }
    w.write_varint(update_bits.len() as u64);
    for (id, u) in &update_bits {
        w.write_varint(u64::from(id.0));
        w.append(u);
    }
    let mut packets = Vec::new();
    conn.link.counters.far_records += far_sent;
    if acked {
        let (frames, placed) = conn.link.assemble(tick, records, sh.mtu, &mut caches.writers);
        packets = frames;
        for (slot, seq) in placed {
            if let Some(ce) = conn.copy_mut(slot) {
                ce.sent_seq = seq;
                if let Some(last) = ce.flight.last_mut()
                    && last.0 == NO_PACKET
                {
                    last.0 = seq;
                }
            }
        }
    }

    // Despawns, out-of-scope and (join) level_removed.
    let mut gone: Vec<(NetId, bool)> = Vec::new();
    for (id, slot) in sh.despawned {
        conn.refused.remove(slot);
        if conn.remove(*slot).is_some() {
            gone.push((*id, false));
            if let Some(pi) = item.pint.as_mut() {
                pi.send.bandwidth.by_entity.remove(id);
            }
        }
    }
    // A resumed connection: copies its client kept through the dormancy whose entity is gone.
    for id in std::mem::take(&mut conn.resume_owed) {
        if rep.entities.slot_of(id).is_none() && !gone.iter().any(|(g, _)| *g == id) {
            gone.push((id, false));
        }
    }
    for (id, slot) in &scope_out {
        let Some(s) = rep.entities.at(*slot) else {
            continue;
        };
        let level = matches!(s.kind, SpawnKind::Level { .. });
        if level {
            if let Some(ce) = conn.copy_mut(*slot) {
                ce.out_of_scope = true;
            }
            conn.irregular = true;
        } else {
            conn.remove(*slot);
            if let Some(pi) = item.pint.as_mut() {
                pi.send.bandwidth.by_entity.remove(id);
            }
        }
        gone.push((*id, level));
    }
    w.write_varint(gone.len() as u64);
    for (id, scope) in &gone {
        w.write_varint(u64::from(id.0));
        w.write_bool(*scope);
    }
    let removed: &[(u128, u128)] = if join { &rep.level_removed } else { &[] };
    w.write_varint(removed.len() as u64);
    for (level, guid) in removed {
        write_u128(&mut w, *level);
        write_u128(&mut w, *guid);
    }
    section_empty &= gone.is_empty() && removed.is_empty();
    if let Some(role) = conn.recorder.as_mut() {
        role.heal = !gained_copies.is_empty() || !gone.is_empty();
        // What left this view still maps on the server, so a referrer to it must be named here to be sent again.
        gained_copies.extend(gone.iter().map(|(id, _)| *id));
        role.spawned = gained_copies;
    }
    if let Some(pi) = item.pint.as_mut() {
        let state: usize = packets.iter().map(|f: &StatePacket| f.bytes.len()).sum();
        let bytes = (w.bit_len().div_ceil(8) + state) as u32;
        pi.send.bandwidth.total.add(second, u64::from(bytes));
        pi.send.bandwidth.recent.push_back((tick, bytes));
        while pi.send.bandwidth.recent.len() > 300 {
            pi.send.bandwidth.recent.pop_front();
        }
    }
    if refresh_owed {
        match item.pint.as_ref() {
            // What was a candidate and still has no copy: the next tick offers it again.
            Some(pi) => {
                let mut owed = std::mem::take(&mut conn.owed_slots);
                owed.clear();
                owed.extend(
                    spawns
                        .iter()
                        .map(|(_, _, slot)| *slot)
                        .filter(|slot| pi.held_bits.get(*slot) && conn.copy(*slot).is_none()),
                );
                conn.spawn_owed = !owed.is_empty();
                conn.owed_slots = owed;
            }
            None => conn.spawn_owed = owes_spawn(conn, rep),
        }
    }
    std::mem::swap(&mut caches.writers, &mut conn.writers);
    let update_count = updates.len();
    conn.scratch = ConnScratch {
        visit: visit_slots,
        updates,
        spawns,
        scope_out,
        own_changes,
        ..scratch
    };
    let cpu = [walk_s, priority_s, pack_s, commit_start.elapsed().as_secs_f64()];
    item.done = Some(ConnDone {
        bits: w,
        section_empty,
        wire,
        packets,
        too_large,
        fit_again,
        loops,
        cpu,
        ordered,
        visited,
        priority_slots,
        updates: update_count,
        critical_sent,
    });
}

/// A recording has no server to answer a resync, so after a gather that gained or lost a copy, each copy whose
/// references name a target that just spawned, or one that no longer maps, goes out again as a full record (spec 103
/// § 3.6). A copy naming only what the connection already held costs one encode and no bytes.
fn heal_recorder_references(sh: &GatherShared<'_>, conn: &mut ConnReplication) {
    struct Probe<'a> {
        map: &'a dyn EntityMap,
        spawned: &'a [NetId],
        hit: std::cell::Cell<bool>,
    }
    impl EntityMap for Probe<'_> {
        fn to_net(&self, entity: Entity) -> Option<NetId> {
            let id = self.map.to_net(entity);
            match id {
                Some(id) if self.spawned.contains(&id) => self.hit.set(true),
                None if entity != Entity::PLACEHOLDER => self.hit.set(true),
                _ => {}
            }
            id
        }
        fn to_entity(&self, id: NetId) -> Option<Entity> {
            self.map.to_entity(id)
        }
    }
    let Some(mut role) = conn.recorder.take() else {
        return;
    };
    role.heal = false;
    let mut again = false;
    for ce in &mut conn.held {
        if ce.out_of_scope {
            continue;
        }
        let Some(version) = sh.rep.entities.at(ce.slot).and_then(|s| s.current.as_ref()) else {
            continue;
        };
        let probe = Probe {
            map: sh.map,
            spawned: &role.spawned,
            hit: std::cell::Cell::new(false),
        };
        for (ci, value) in version.comps.iter() {
            let Some(c) = sh.registry.get(*ci).filter(|c| c.has_refs) else {
                continue;
            };
            role.probe.clear();
            let cx = CodecContext::new(&probe);
            (c.encode)(value, &mut role.probe, &cx);
        }
        if probe.hit.get() {
            ce.full_again = true;
            again = true;
        }
    }
    conn.irregular |= again;
    role.spawned.clear();
    conn.recorder = Some(role);
}

/// A `Player` view reads that peer's held set at this tick, so hysteresis and linger match. When the peer has gone the
/// last set stays (spec 103 § 3.3).
fn follow_recorder_views(rep: &mut ServerReplication, interest: &NetInterest) {
    for (_, conn) in rep.conns.range_mut(crate::recorder::FIRST_VIRTUAL_PEER..) {
        let Some(role) = conn.recorder.as_mut() else {
            continue;
        };
        let RecorderView::Player(of) = role.view else {
            continue;
        };
        if let Some(pi) = interest.peers.get(&of) {
            role.bits
                .get_or_insert_with(Default::default)
                .copy_from(&pi.held_bits);
        }
    }
}

/// Bytes a typical state record adds around its fields: id, length, kind and tick offsets.
const STATE_RECORD_FRAMING: usize = 6;

/// Room an update's writer starts with: a transform and a few fields, no regrowth for most records.
const UPDATE_BYTES: usize = 32;

fn charge_bandwidth(
    pi: &mut PeerInterest,
    registry: &ReplicationRegistry,
    second: u64,
    id: NetId,
    bytes: usize,
    comp_bytes: &[(u16, usize)],
) {
    let bw = &mut pi.send.bandwidth;
    bw.by_entity.entry(id).or_default().add(second, bytes as u64);
    for (ci, b) in comp_bytes {
        let name = registry.get(*ci).map_or("?", |c| c.name);
        bw.by_component.entry(name).or_default().add(second, *b as u64);
    }
}

/// The spawn and update candidates of one connection's tick.
struct PackInput<'a> {
    spawns: &'a [(u32, NetId, u32)],
    updates: &'a [(NetId, u32)],
}

/// What the packer picked for one connection.
#[derive(Default)]
struct Packed {
    /// Join-critical spawns sent (outside the spawn budget).
    critical_sent: u32,
    /// Join-critical spawns still waiting (a parent, an oversize record): the window stays open.
    critical_pending: u32,
    spawns: Vec<PreparedSpawn>,
    updates: Vec<PreparedUpdate>,
    too_large: Vec<(NetId, usize)>,
    /// Over-the-cap records of `(slot, id, version)`, for the connection's refusal cache.
    refused: Vec<(u32, NetId, u32)>,
    /// Copies the connection holds whose record is over the cap: they leave the client like an
    /// out-of-scope copy (spec 120 § 8.1, R5: never frozen).
    withdraw: Vec<(NetId, u32)>,
    /// Ids warned as too large whose record fits again.
    fit_again: Vec<NetId>,
    held_back: u32,
    /// Updates the distance rate tiers held back.
    tier_held: u32,
    priority_s: f64,
    pack_s: f64,
    /// Candidates put in order.
    ordered: usize,
    /// Packed slots this pass read.
    priority_slots: u64,
}

/// Spec 72 §§ 6-7: priorities first (cheap), then records are encoded in priority order only while
/// they can still be picked. Only the head of the ranking a budget can reach is sorted; the rest
/// is ordered only if the head leaves room.
fn pack_conn(
    sh: &GatherShared<'_>,
    peer: PeerId,
    conn: &ConnReplication,
    pi: &mut PeerInterest,
    input: PackInput<'_>,
    caches: &mut TickCaches,
    notes: &mut Vec<(u16, u32, u32)>,
) -> Packed {
    let tick = sh.tick;
    let rep = sh.rep;
    let priority_start = std::time::Instant::now();
    let acked = conn.link.wire == ConnWire::Acked;
    let mut oversize_free = conn.link.oversize_free(tick);
    // A little under a frame's payload: the record's own framing goes around these bytes.
    let oversize_above = conn.link.payload_bytes(sh.mtu).saturating_sub(16);
    // The bytes a state record adds around its fields (the tick stream's are in the fields).
    let framing = if acked { STATE_RECORD_FRAMING } else { 0 };
    // The connection's ranking and picked lists keep their buffers between ticks (spec 120 § 8.4).
    let mut ranked = std::mem::take(&mut pi.pack_ranked);
    ranked.clear();
    ranked.reserve(input.spawns.len() + input.updates.len());
    let mut gained = 0.0f32;
    let mut starved = 0u32;
    let mut out = Packed::default();
    let default_profile = NetProfile::default();
    let profile_of = |row: u32| -> &NetProfile {
        sh.interest
            .and_then(|i| i.profile_in_row(row))
            .unwrap_or(&default_profile)
    };
    let gain_of = |st: &crate::relevance::PeerSlot, profile: &NetProfile, owned: bool, hot: TickHot| -> f32 {
        hot.priority_scale
            * NetCombatHeat::factor(hot.heat.0, hot.heat.1, st.distance())
            * priority_gain(
                profile.priority,
                match profile.relevance {
                    RelevanceShape::Spatial { radius, .. } => Some(radius),
                    RelevanceShape::World => None,
                },
                st.distance(),
                profile.view_falloff,
                st.in_view(),
                owned,
                profile.owner_boost,
            )
    };
    // Spec 77 § 6.1 item 2 (ADR-7212): while the join window is open, the peer's own entities and
    // the `sent_to_all` game state rank first and go outside the spawn budget.
    let join_open = !conn.join_spawns_done;
    let mut critical_total = 0u32;
    {
        let _span = tracing::trace_span!("net_priority").entered();
        for (depth, id, slot) in input.spawns {
            let hot = sh.hot_at(*slot);
            let owned = hot.owner == peer;
            let profile = profile_of(hot.profile);
            let critical = join_open && (owned || profile.sent_to_all);
            let Some(st) = pi.slots.get_mut(*slot) else {
                continue;
            };
            let g = gain_of(st, profile, owned, hot);
            gained += g;
            st.acc += g;
            // A full state for a copy the client has (its baseline expired) is an update in all but
            // name, so its interval counts. A new copy goes as soon as it can.
            let since = st.last_sent().map(|t| tick.0.saturating_sub(t.0));
            // A join-critical candidate counts before the min_interval hold: held back, it is
            // still pending and must keep the join window open until it actually goes (spec 77
            // § 6.1; an over-limit record is released by the `too_large` adjustment below).
            critical_total += u32::from(critical);
            if since.is_some_and(|d| d < profile.min_interval_ticks)
                && conn.copy(*slot).is_some_and(|ce| ce.spawn_acked)
            {
                out.held_back += 1;
                continue;
            }
            ranked.push(Ranked {
                key: rank_key(critical, st.acc, *id),
                slot: *slot,
                depth: *depth,
                gain: g,
                spawn: true,
            });
        }
        // A short update list is cheaper by index. When most packed slots are candidates, one
        // array walk beats an index read per copy. The sort below makes the visit order irrelevant.
        let mut scanned = 0u64;
        // Unwanted packed slots are counted outside the closure: it already borrows `scanned`.
        let mut skipped = 0u64;
        {
            // The closure ends before `scanned` and `out` are used again.
            let mut push_update = |st: &mut crate::relevance::PeerSlot, slot: u32| {
                scanned += 1;
                let hot = sh.hot_at(slot);
                let id = hot.id;
                let owned = hot.owner == peer;
                let profile = profile_of(hot.profile);
                let g = gain_of(st, profile, owned, hot);
                gained += g;
                st.acc += g;
                if st.acc > 10.0 * profile.priority {
                    starved += 1;
                }
                let since = st.last_sent().map(|t| tick.0.saturating_sub(t.0));
                if since.is_some_and(|d| d < profile.min_interval_ticks) {
                    out.held_back += 1;
                    return;
                }
                let forced = profile
                    .max_interval_ticks
                    .is_some_and(|m| since.is_none_or(|d| d >= m));
                // Distance rate tiers (spec 120 § 8.3): a mid or far entity waits for its tier's
                // interval. Owned and forced records never wait; the accumulator keeps growing, so
                // the entity ranks high on the tick it may go.
                if !forced
                    && !owned
                    && let (Some(tiers), Some(d), Some(since)) =
                        (profile.rate_tiers.as_ref(), st.distance(), since)
                    && since < tiers.interval_ticks(tiers.tier(d), sh.send.send_every_ticks, sh.send.tick_hz)
                {
                    out.tier_held += 1;
                    return;
                }
                ranked.push(Ranked {
                    key: rank_key(forced, st.acc, id),
                    slot,
                    depth: 0,
                    gain: g,
                    spawn: false,
                });
            };
            if input.updates.len() < pi.slots.len() {
                for (_, slot) in input.updates {
                    if let Some(st) = pi.slots.get_mut(*slot) {
                        push_update(st, *slot);
                    }
                }
            } else {
                let mut wanted = std::mem::take(&mut caches.wanted);
                wanted.reset(sh.hot.len());
                for (_, slot) in input.updates {
                    wanted.set(*slot);
                }
                let (owners, values) = pi.slots.parts_mut();
                for (slot, st) in owners.iter().copied().zip(values) {
                    if !wanted.get(slot) {
                        skipped += 1;
                        continue;
                    }
                    push_update(st, slot);
                }
                caches.wanted = wanted;
            }
        }
        out.priority_slots = scanned + skipped;
    }
    let _span = tracing::trace_span!("net_pack").entered();
    // The project rate, never above what the transport's rate controller allows (spec 120 § 4.2
    // item 9, ADR-8301).
    let rate = pi
        .send
        .rate(sh.send.client_rate)
        .min(conn.link.send_allowance.unwrap_or(u32::MAX));
    let budget = tick_budget(
        rate,
        sh.send.send_hz(),
        sh.send.scale,
        pi.send.carry,
        sh.send.spawns_per_send,
    );
    let debt = f64::from(std::mem::take(&mut pi.send.message_debt));
    let mut packer = Packer::new(budget, debt);
    // Only what the budget can reach is ordered: forced and spawn records always, deltas by size.
    let forced = ranked.iter().filter(|r| r.forced() || r.spawn).count();
    let reach = forced + (budget.bytes.max(0.0) / MIN_RECORD_BYTES) as usize + RANK_MARGIN;
    let mut ordered = order_window(&mut ranked, 0, reach);
    out.priority_s = priority_start.elapsed().as_secs_f64();
    let pack_start = std::time::Instant::now();
    let mut picked = std::mem::take(&mut pi.pack_picked);
    picked.clear();
    let mut spawns_pending = 0u32;
    // A child's spawn waits while its parent's spawn is a candidate not yet picked.
    // A parent already known to be over the entity-state cap is not a candidate: it is offered
    // again every tick and never enters `spawned_now`, which would leave the child pending
    // forever. The client can spawn the child and wait for a parent that may arrive later.
    let mut spawn_set: HashSet<NetId> = input.spawns.iter().map(|(_, id, _)| *id).collect();
    spawn_set.retain(|id| !sh.rep.warned_large.contains(id));
    let mut spawned_now: HashSet<NetId> = HashSet::new();
    let mut at = 0usize;
    let mut spawns_unseen = ranked.iter().filter(|r| r.spawn).count();
    while at < ranked.len() {
        if at == ordered {
            if !packer.wants(false, false) && !(spawns_unseen > 0 && packer.wants(true, false)) {
                // Nothing below the ordered part can be picked: it all waits.
                for r in ranked.get(at..).unwrap_or(&[]) {
                    if r.spawn {
                        spawns_pending += 1;
                    } else {
                        out.held_back += 1;
                    }
                }
                break;
            }
            ordered = order_window(&mut ranked, at, reach);
        }
        let Some(r) = ranked.get(at).copied() else {
            break;
        };
        at += 1;
        spawns_unseen -= usize::from(r.spawn);
        let (id, is_forced) = (r.id(), r.forced());
        if !packer.wants(r.spawn, is_forced) {
            if r.spawn {
                spawns_pending += 1;
            } else {
                out.held_back += 1;
            }
            continue;
        }
        // An entity whose record is over the entity-state cap (spawns and updates share it,
        // spec 120 § 8.1, R5) cannot go: no priority builds up for it, it does not hold the join
        // window open, and a copy the client holds leaves it instead of freezing there.
        let number = sh.hot_at(r.slot).number;
        if r.spawn {
            let parent = rep.entities.at(r.slot).and_then(|s| s.parent_net);
            if parent.is_some_and(|p| spawn_set.contains(&p) && !spawned_now.contains(&p)) {
                spawns_pending += 1;
                continue;
            }
            if conn.refused.get(&r.slot) == Some(&(id, number)) {
                // Measured over the cap at this very version: not encoded again (ADR-8419).
                gained -= r.gain;
                critical_total = critical_total.saturating_sub(u32::from(is_forced));
                refuse_record(&mut out, pi, conn, (r.slot, id, number), None);
                spawn_set.remove(&id);
                continue;
            }
            let Some(sp) = enc_spawn(sh, peer, acked, caches, notes, r.depth, id, r.slot) else {
                continue;
            };
            let bytes = sp.bits.bit_len().div_ceil(8);
            if bytes > crate::send::MAX_ENTITY_STATE_BYTES {
                // Dropped from this tick's spawn set too, so a later child in the same pack is
                // not held for a parent that this tick already refused. `warned_large` covers the
                // next gather.
                gained -= r.gain;
                critical_total = critical_total.saturating_sub(u32::from(is_forced));
                refuse_record(&mut out, pi, conn, (r.slot, id, number), Some(bytes));
                spawn_set.remove(&id);
                continue;
            }
            if sh.rep.warned_large.contains(&id) {
                out.fit_again.push(id);
            }
            if acked && bytes > oversize_above {
                // Larger than a packet: reliable, and one at a time (spec 72 § 7).
                if !oversize_free {
                    spawns_pending += 1;
                    continue;
                }
                oversize_free = false;
            }
            // A join-critical spawn offers as forced: outside the spawn budget (bytes still count).
            if packer.offer(true, is_forced, bytes + framing) {
                picked.push(r.slot);
                spawned_now.insert(id);
                out.spawns.push(sp);
                out.critical_sent += u32::from(is_forced);
            } else {
                spawns_pending += 1;
            }
            continue;
        }
        // The far tier's reduced record (spec 120 § 8.3, ADR-10124): an owned or forced record is
        // never far.
        let far_tier = !is_forced && sh.hot_at(r.slot).owner != peer && {
            let tiers = profile_of(sh.hot_at(r.slot).profile).rate_tiers.as_ref();
            let distance = pi.slots.get(r.slot).and_then(|st| st.distance());
            tiers
                .zip(distance)
                .is_some_and(|(t, d)| t.tier(d) == crate::profile::RateTier::Far)
        };
        let Some(p) = enc_update(sh, peer, conn, caches, notes, id, r.slot, far_tier) else {
            continue;
        };
        if !p.any {
            // Nothing for this connection after all: no gain, commit the bookkeeping.
            gained -= r.gain;
            if let Some(st) = pi.slots.get_mut(r.slot) {
                st.acc -= r.gain;
                if p.far {
                    // A far copy whose coarse position did not move stays a candidate (its exact
                    // state is owed when it leaves the tier), but nothing is owed now.
                    st.acc = 0.0;
                }
            }
            out.updates.push(p);
            continue;
        }
        let bytes = p.bits.bit_len().div_ceil(8);
        if bytes > crate::send::MAX_ENTITY_STATE_BYTES {
            // An update over the cap: the same limit as a spawn (spec 120 § 8.1, R5).
            gained -= r.gain;
            refuse_record(&mut out, pi, conn, (r.slot, id, number), Some(bytes));
            continue;
        }
        if sh.rep.warned_large.contains(&id) {
            out.fit_again.push(id);
        }
        if acked && bytes > oversize_above {
            if !oversize_free {
                out.held_back += 1;
                continue;
            }
            oversize_free = false;
        }
        if packer.offer(false, is_forced, bytes + framing) {
            picked.push(r.slot);
            out.updates.push(p);
        } else {
            out.held_back += 1;
        }
    }
    out.ordered = ordered;
    out.critical_pending = critical_total.saturating_sub(out.critical_sent);
    let used = packer.used();
    // Debt (messages, one record larger than the tick's budget) is paid back in full from the
    // next ticks' budgets, so the bytes per second hold at the rate (spec 120 § 8.1, adversarial
    // row 4, ADR-8419; ADR-1662 (5) held it to one tick, which let every tick overspend).
    pi.send.carry = budget.bytes - used;
    // A sent record gives up the tick's average share of the gained priority (ADR-1662:
    // resetting to 0 loses the overshoot at tick granularity and skews the ratios).
    let share = send_share(gained, picked.len());
    let mut tier_sent = [0u32; 3];
    for slot in &picked {
        if let Some(st) = pi.slots.get_mut(*slot) {
            st.acc = accumulator_after_send(st.acc, share);
            st.set_last_sent(tick);
            let tier = profile_of(sh.hot_at(*slot).profile)
                .rate_tiers
                .as_ref()
                .zip(st.distance())
                .map_or(crate::profile::RateTier::Near, |(t, d)| t.tier(d));
            if let Some(n) = tier_sent.get_mut(tier as usize) {
                *n += 1;
            }
        }
    }
    pi.send.counters.tier_held = out.tier_held;
    pi.send.counters.tier_sent = tier_sent;
    pi.send.counters.budget = budget.bytes.max(0.0) as u32;
    pi.send.counters.budget_used = used as u32;
    pi.send.counters.starved = starved;
    pi.send.counters.spawns_pending = spawns_pending;
    pi.send.counters.deferred = out.held_back;
    pi.send.counters.relevant = pi.held.len() as u32;
    pi.pack_ranked = ranked;
    pi.pack_picked = picked;
    out.pack_s = pack_start.elapsed().as_secs_f64();
    out
}

/// One record over the entity-state cap (spec 120 § 8.1, R5, ADR-8419): logged when measured
/// (`bytes`), remembered at its version, no priority kept, and a copy the client holds withdrawn.
fn refuse_record(
    out: &mut Packed,
    pi: &mut PeerInterest,
    conn: &ConnReplication,
    (slot, id, number): (u32, NetId, u32),
    bytes: Option<usize>,
) {
    if let Some(bytes) = bytes {
        out.too_large.push((id, bytes));
        out.refused.push((slot, id, number));
    }
    if let Some(st) = pi.slots.get_mut(slot) {
        st.acc = 0.0;
    }
    if conn.copy(slot).is_some_and(|ce| !ce.out_of_scope) {
        out.withdraw.push((id, slot));
    }
}

/// Encodes a spawn record for one entity. Nothing changes until the record is picked.
#[allow(clippy::too_many_arguments)] // ADR-1950: the shared state, the record and the two worker scratch maps.
fn enc_spawn(
    sh: &GatherShared<'_>,
    peer: PeerId,
    acked: bool,
    caches: &mut TickCaches,
    notes: &mut Vec<(u16, u32, u32)>,
    depth: u32,
    id: NetId,
    slot: u32,
) -> Option<PreparedSpawn> {
    let s = sh.rep.entities.at(slot)?;
    let version = s.current.clone()?;
    let owned = s.owner == peer;
    let predicted = owned && s.predicted;
    let parent = version.parent;
    let mut ce = ConnEntity {
        id,
        slot,
        base: Some(version.clone()),
        base_number: version.number,
        base_tick: version.tick,
        unmatched: Vec::new(),
        owned,
        predicted,
        last_sent: sh.tick,
        out_of_scope: false,
        full_again: false,
        teleport_due: false,
        resend_due: false,
        born: sh.tick,
        spawn_acked: false,
        teleport_tick: NetTick(0),
        resend_tick: NetTick(0),
        sent_seq: NO_PACKET,
        sent_version: NO_VERSION,
        flight: Vec::new(),
        gated: false,
        base_far: false,
    };
    let mut sw = BitWriter::new();
    // A state frame's record names the entity in its own header.
    if !acked {
        sw.write_varint(u64::from(id.0));
    }
    match s.kind {
        SpawnKind::Bare => sw.write_bits(0, 2),
        SpawnKind::Prefab(p) => {
            sw.write_bits(1, 2);
            write_u128(&mut sw, p);
        }
        SpawnKind::Level { level, guid } => {
            sw.write_bits(2, 2);
            write_u128(&mut sw, level);
            write_u128(&mut sw, guid);
        }
    }
    sw.write_bool(parent.is_some());
    if let Some(p) = parent {
        sw.write_varint(u64::from(p.0));
    }
    sw.write_bool(owned);
    sw.write_bool(predicted);
    // Transform.
    sw.write_bits(transform_mode_bits(s.transform_mode), 2);
    if s.transform_mode != TransformReplication::None {
        sw.write_bool(version.transform.is_some());
        if let Some(t) = &version.transform {
            sw.write_bool(s.teleport);
            if acked {
                // The steady bit (ADR-8522), as on an update record.
                sw.write_bool(s.gate.is_some());
            }
            sw.append_slice(&t.bytes, usize::from(t.len));
        }
    }
    // Components that pass their audience in a spawn record.
    let mut entries: Vec<(u16, u64, BitWriter)> = Vec::new();
    for (ci, value) in version.comps.iter() {
        let c = sh.registry.get(*ci)?;
        let audience_ok =
            sh.audience(peer)
                .matches(c.options.audience, true, (owned, predicted), s.entity, id, *ci);
        if !audience_ok {
            ce.unmatched.push(*ci);
            continue;
        }
        let bits = caches.full(c, slot, *ci, value, (sh.map, sh.assets), notes);
        entries.push((*ci, ENTRY_FULL, bits));
    }
    let comp_bytes = entries
        .iter()
        .map(|(ci, _, b)| (*ci, b.bit_len().div_ceil(8)))
        .collect();
    write_entries(&mut sw, &entries);
    Some(PreparedSpawn {
        depth,
        id,
        bits: sw,
        ce,
        comp_bytes,
    })
}

/// Encodes an update for one held copy against what the connection holds. Nothing changes until
/// the record is picked.
///
/// `far_tier`: the copy is in its profile's far rate tier for this connection and the record is
/// neither owned nor forced, so it may take the far form (spec 120 § 8.3, ADR-10124).
#[allow(clippy::too_many_arguments)] // ADR-1950: the shared state, the record and the two worker scratch maps.
fn enc_update(
    sh: &GatherShared<'_>,
    peer: PeerId,
    conn: &ConnReplication,
    caches: &mut TickCaches,
    notes: &mut Vec<(u16, u32, u32)>,
    id: NetId,
    slot: u32,
    far_tier: bool,
) -> Option<PreparedUpdate> {
    let s = sh.rep.entities.at(slot)?;
    let ce = conn.copy(slot)?;
    let now = s.current.as_ref()?;
    let teleport = s.teleport || ce.teleport_due;
    let resend = s.resend || ce.resend_due;
    let base = ce.base.as_deref();
    let mut p = PreparedUpdate {
        id,
        slot,
        bits: caches
            .writers
            .pop()
            .unwrap_or_else(|| BitWriter::with_capacity(UPDATE_BYTES)),
        unmatched: ce.unmatched.clone(),
        comp_bytes: Vec::new(),
        any: false,
        far: false,
    };
    let u = &mut p.bits;
    // Parent change.
    let parent = now.parent;
    let parent_changed = base.is_none_or(|b| b.parent != parent);
    u.write_bool(parent_changed);
    if parent_changed {
        p.any = true;
        u.write_bool(parent.is_some());
        if let Some(pn) = parent {
            u.write_varint(u64::from(pn.0));
        }
    }
    // Transform.
    let acked = conn.link.wire == ConnWire::Acked;
    let base_t = base.and_then(|b| b.transform.as_ref());
    // A baseline the client holds in its far form (ADR-10124): its position there is the coarse
    // one and its rotation and velocity are older, so the exact record is always news and never a
    // delta.
    let base_far = ce.base_far && base_t.is_some();
    let t_changed = match (&now.transform, base_t) {
        (Some(now), Some(sent)) => base_far || now != sent,
        (Some(_), None) => true,
        _ => false,
    };
    let send_t = now.transform.is_some() && (t_changed || teleport || resend);
    u.write_bool(send_t);
    if send_t && let Some(t) = &now.transform {
        p.any = true;
        u.write_bool(teleport);
        // On the state channel the record is a delta against the acknowledged baseline the client
        // holds (spec 120 § 8.2, ADR-8467); the tick stream and a baseline of another mode get the
        // absolute record.
        if acked {
            // The steady bit (ADR-8522): the server keeps this copy within the profile's gate, so
            // the client extrapolates the record for as long as it holds it.
            u.write_bool(s.gate.is_some());
        }
        let delta_base = base_t.filter(|b| acked && !base_far && t.q.can_delta(&b.q));
        if acked {
            u.write_bool(delta_base.is_some());
        }
        match delta_base {
            // The predictor's time: the record's version tick less the baseline's, as the
            // client computes it from `base_ago`.
            Some(b) => {
                t.q.write_delta(&b.q, now.tick.0.saturating_sub(ce.base_tick.0), u)
            }
            None => {
                // The mode the bytes were encoded under (the copy's mode follows it).
                u.write_bits(transform_mode_bits(t.q.mode), 2);
                u.append_slice(&t.bytes, usize::from(t.len));
            }
        }
    }
    // The far form (spec 120 § 8.3, ADR-10124): a far-tier update whose transform fits the far
    // record carries the coarse position instead of the fields above when that is shorter. Against
    // a far baseline the record is due only when the coarse position moved.
    // A version that has stood for `FAR_SETTLE_SECONDS` is the entity at rest: it goes exact
    // (ADR-10199), so the copy stops being held in its far form and leaves the walk.
    let settle_ticks = ((FAR_SETTLE_SECONDS * sh.send.tick_hz).round() as u32).max(1);
    let settled = sh.tick.0.saturating_sub(now.tick.0) >= settle_ticks;
    let far_ok = far_tier
        && acked
        && sh.rep.far_records()
        && !parent_changed
        && !teleport
        && !resend
        && !settled
        && s.gate.is_none()
        && !ce.predicted;
    let far_form = now
        .transform
        .as_ref()
        .zip(base_t)
        .filter(|_| far_ok)
        .and_then(|(t, b)| t.q.far_against(&b.q).map(|far| (far, b)));
    if let Some((far, b)) = far_form {
        let mut f = caches
            .writers
            .pop()
            .unwrap_or_else(|| BitWriter::with_capacity(UPDATE_BYTES));
        QTransform::write_far(&far, &b.q, &mut f);
        // From an exact baseline the far form must save a byte: once a copy is held in its far
        // form its exact record is absolute, so a walker whose predicted delta is as short as the
        // far record stays exact.
        let margin = if base_far { 0 } else { FAR_ENTRY_MARGIN_BITS };
        if f.bit_len() + margin <= p.bits.bit_len() && f.bit_len() < p.bits.bit_len() {
            std::mem::swap(&mut p.bits, &mut f);
            p.far = true;
            // The client's position is the baseline's coarse one: the same coarse position is
            // no news (the baseline then moves on without a record).
            p.any = !base_far || b.q.far_pos() != Some(far);
        }
        f.clear();
        caches.writers.push(f);
    }
    // Components: the two versions' lists are sorted by index; walk them together.
    let mut entries: Vec<(u16, u64, BitWriter)> = Vec::new();
    let empty: Vec<(u16, ErasedComp)> = Vec::new();
    let held: &[(u16, ErasedComp)] = base.map_or(empty.as_slice(), |b| b.comps.as_slice());
    let current: &[(u16, ErasedComp)] = now.comps.as_slice();
    let (mut a, mut b) = (0usize, 0usize);
    loop {
        let next_now = current.get(a);
        let next_base = held.get(b);
        let (ci, value, base_value) = match (next_now, next_base) {
            (None, None) => break,
            (Some((n, v)), Some((m, w))) if n == m => {
                a += 1;
                b += 1;
                (*n, Some(v), Some(w))
            }
            (Some((n, v)), Some((m, _))) if n < m => {
                a += 1;
                (*n, Some(v), None)
            }
            (Some((n, v)), None) => {
                a += 1;
                (*n, Some(v), None)
            }
            (_, Some((m, w))) => {
                b += 1;
                (*m, None, Some(w))
            }
        };
        let c = sh.registry.get(ci)?;
        let matched = sh.audience(peer).matches(
            c.options.audience,
            false,
            (ce.owned, ce.predicted),
            s.entity,
            id,
            ci,
        );
        let was_matched = !ce.unmatched.contains(&ci);
        if !matched {
            if was_matched && !p.unmatched.contains(&ci) {
                p.unmatched.push(ci);
            }
            continue;
        }
        let start_matching = !was_matched;
        if start_matching {
            p.unmatched.retain(|u| *u != ci);
        }
        // The same value object means no change (versions share what did not change).
        if let (Some(v), Some(w)) = (value, base_value)
            && Arc::ptr_eq(v, w)
            && !start_matching
            && !resend
        {
            continue;
        }
        match (value, base_value) {
            (Some(now), Some(base)) => {
                if start_matching || resend {
                    let bits = caches.full(c, slot, ci, now, (sh.map, sh.assets), notes);
                    entries.push((ci, ENTRY_FULL, bits));
                } else if let Some(bits) = caches.delta((c, slot, ci), now, base, (sh.map, sh.assets), notes)
                {
                    entries.push((ci, ENTRY_DELTA, bits));
                }
            }
            (Some(now), None) => {
                let bits = caches.full(c, slot, ci, now, (sh.map, sh.assets), notes);
                entries.push((ci, ENTRY_FULL, bits));
            }
            (None, Some(_)) => entries.push((ci, ENTRY_REMOVED, BitWriter::new())),
            (None, None) => {}
        }
    }
    p.any |= !entries.is_empty();
    // A version equal to the baseline can still be news: the client may have applied a newer one
    // that was sent after the baseline, and an empty delta is what brings it back.
    let sent_past_base = ce.sent_version != ce.base_version();
    let on_its_way = ce.sent_version == now.number && conn.link.unresolved(ce.sent_seq, sh.tick);
    p.any |= conn.link.wire == ConnWire::Acked && sent_past_base && !on_its_way;
    p.comp_bytes = entries
        .iter()
        .map(|(ci, _, b)| (*ci, b.bit_len().div_ceil(8) + 1))
        .collect();
    write_entries(&mut p.bits, &entries);
    Some(p)
}

/// The per-connection lists the walk fills.
struct WalkLists<'a> {
    spawns: &'a mut Vec<(u32, NetId, u32)>,
    scope_out: &'a mut Vec<(NetId, u32)>,
    updates: &'a mut Vec<(NetId, u32)>,
    own_changes: &'a mut Vec<(NetId, bool, bool)>,
}

/// One held copy in the per-connection walk: a copy leaving scope, a spawn record again, or an
/// update candidate. Returns whether the copy keeps the connection irregular.
fn classify(
    hot: TickHot,
    ce: &mut ConnEntity,
    relevant: bool,
    peer: PeerId,
    lists: &mut WalkLists<'_>,
) -> bool {
    if ce.out_of_scope || ce.full_again {
        if relevant {
            lists.spawns.push((0, ce.id, ce.slot));
        } else if ce.full_again {
            ce.full_again = false;
            if !ce.out_of_scope {
                lists.scope_out.push((ce.id, ce.slot));
            }
        }
        return ce.out_of_scope && !relevant;
    }
    if !relevant {
        lists.scope_out.push((ce.id, ce.slot));
        return false;
    }
    visit_held(hot, ce, peer, lists.updates, lists.own_changes);
    false
}

/// An encoded spawn record waiting for the budget.
struct PreparedSpawn {
    depth: u32,
    id: NetId,
    bits: BitWriter,
    ce: ConnEntity,
    comp_bytes: Vec<(u16, usize)>,
}

/// An encoded update waiting for the budget, with what it changes in the connection's view.
struct PreparedUpdate {
    id: NetId,
    slot: u32,
    bits: BitWriter,
    /// The copy's unmatched components after this record.
    unmatched: Vec<u16>,
    comp_bytes: Vec<(u16, usize)>,
    /// Carries anything (else it is not sent, only committed).
    any: bool,
    /// The fields are a far update's (kind 3, ADR-10124): the far record and the entries.
    far: bool,
}

/// The send settings of one gather (spec 72 § 7).
struct SendConfig {
    tick_hz: f64,
    send_every_ticks: u32,
    client_rate: u32,
    spawns_per_send: usize,
    /// The total outbound cap's scale factor for every connection's budget.
    scale: f64,
}

impl SendConfig {
    fn from_world(
        rep: &ServerReplication,
        interest: Option<&NetInterest>,
        r: &crate::settings::ReplicationSettings,
        tick_hz: f64,
    ) -> Self {
        let mut scale = 1.0;
        if let Some(i) = interest
            && r.server_rate_bytes > 0
        {
            let total: f64 = rep
                .conns
                .keys()
                .filter(|p| !crate::recorder::is_virtual_peer(**p))
                .map(|p| {
                    f64::from(
                        i.peers
                            .get(p)
                            .map_or(r.client_rate_bytes, |pi| pi.send.rate(r.client_rate_bytes)),
                    )
                })
                .sum();
            if total > f64::from(r.server_rate_bytes) {
                scale = f64::from(r.server_rate_bytes) / total;
            }
        }
        Self {
            tick_hz,
            send_every_ticks: r.send_every_ticks.max(1),
            client_rate: r.client_rate_bytes,
            spawns_per_send: r.spawns_per_send.max(1) as usize,
            scale,
        }
    }

    /// Send ticks per second.
    fn send_hz(&self) -> f64 {
        self.tick_hz / f64::from(self.send_every_ticks)
    }
}

/// One held copy in the per-connection pass: its ownership bits, and whether it needs an update.
fn visit_held(
    hot: TickHot,
    ce: &mut ConnEntity,
    peer: PeerId,
    updates: &mut Vec<(NetId, u32)>,
    own_changes: &mut Vec<(NetId, bool, bool)>,
) {
    let owned = hot.owner == peer;
    let predicted = owned && hot.has(HOT_PREDICTED);
    let own_changed = owned != ce.owned || predicted != ce.predicted;
    if own_changed {
        ce.owned = owned;
        ce.predicted = predicted;
        own_changes.push((ce.id, owned, predicted));
    }
    let behind = ce.base_version() != hot.number || ce.teleport_due || ce.resend_due;
    let resend = hot.has(HOT_RESEND);
    // Asleep: still send what changed without a component write (ownership, parent, audience
    // inputs), so no change made while asleep is lost.
    if hot.has(HOT_ASLEEP) && !resend && !own_changed && !hot.has(HOT_VOLATILE) && !behind {
        return;
    }
    if behind || hot.has(HOT_DIRTY) || hot.has(HOT_TELEPORT) || resend || hot.has(HOT_VOLATILE) || own_changed
    {
        updates.push((ce.id, ce.slot));
        // One-shot flags of this tick travel with the update, even when the budget defers it.
        ce.teleport_due |= hot.has(HOT_TELEPORT);
        ce.resend_due |= resend;
    }
}

/// One held copy of a connection on the state channel. A record stays offered until it is
/// acknowledged, but not while a packet carrying the same version may still be on its way (spec 72
/// § 4.2, ADR-1950 (4)). Returns whether the copy's baseline expired.
fn visit_acked(
    hot: TickHot,
    ce: &mut ConnEntity,
    peer: PeerId,
    link: &StateLink,
    tick: NetTick,
    lists: &mut WalkLists<'_>,
) -> bool {
    let owned = hot.owner == peer;
    let predicted = owned && hot.has(HOT_PREDICTED);
    let own_changed = owned != ce.owned || predicted != ce.predicted;
    if own_changed {
        ce.owned = owned;
        ce.predicted = predicted;
        lists.own_changes.push((ce.id, owned, predicted));
    }
    if hot.has(HOT_TELEPORT) {
        ce.teleport_due = true;
        ce.teleport_tick = tick;
    }
    if hot.has(HOT_RESEND) {
        ce.resend_due = true;
        ce.resend_tick = tick;
    }
    // Spec 72 § 4.1: a baseline this far behind may have left the client's ring.
    // A gated copy's held-back versions never reach the ring; what was sent to it is bounded by
    // `MAX_GATE_FLIGHT` (ADR-8522).
    let expired =
        ce.base.is_some() && !ce.gated && hot.number.wrapping_sub(ce.base_number) >= STATE_VERSION_RING;
    if expired {
        ce.base = None;
        ce.sent_seq = NO_PACKET;
    }
    // A copy the client predicts is not damped: reconciliation compares every state frame with
    // what the client ran, so a frame must hold the entity's state until it is acknowledged.
    let on_way = |version_matters: bool| {
        !ce.predicted
            && ce.sent_seq != NO_PACKET
            && (!version_matters || ce.sent_version == hot.number)
            && link.unresolved(ce.sent_seq, tick)
    };
    let in_flight = on_way(true);
    if ce.base.is_none() {
        // Unacknowledged spawn (or expired baseline): a full spawn record again, unless one is on
        // its way. For a first spawn any version does the job: an entity that changes every tick
        // is not offered every tick until the acknowledgment arrives. A copy the client has is
        // behind, so only the same version waits.
        let spawn_on_way = !ce.spawn_acked && on_way(false);
        if !spawn_on_way && !in_flight {
            lists.spawns.push((0, ce.id, ce.slot));
        }
        return expired;
    }
    // A copy held in its far form is behind whatever the version says: its exact state is owed
    // as soon as it leaves the far tier (ADR-10124).
    let behind = ce.base_version() != hot.number || ce.teleport_due || ce.resend_due || ce.base_far;
    if hot.has(HOT_ASLEEP) && !own_changed && !hot.has(HOT_VOLATILE) && !behind {
        return expired;
    }
    if (behind && !in_flight) || hot.has(HOT_VOLATILE) || own_changed {
        lists.updates.push((ce.id, ce.slot));
    }
    expired
}

/// The bits one shared encode produced, with its codec stats (`clamped`, `unreplicated_refs`),
/// folded per connection that uses the bits.
type CachedEncode = (BitWriter, u32, u32);

/// Encodings shared by a worker's connections within one gather. Each entry keeps the codec
/// stats of its encode so every connection that uses the bits folds them: the totals then do not
/// depend on how many connections shared a worker's chunk (ADR-4772).
#[derive(Default)]
struct TickCaches {
    full: FastMap<(u32, u16), CachedEncode>,
    /// Keyed by the baseline value's address (versions keep it alive through the gather).
    /// `None` = no change against that baseline.
    deltas: FastMap<(u32, u16, usize), Option<CachedEncode>>,
    /// Empty writers for update records: the spare writers of the connection in turn.
    writers: Vec<BitWriter>,
    /// The bits of the update candidates of the connection in turn.
    wanted: SlotBits,
}

impl TickCaches {
    fn full(
        &mut self,
        c: &RepComponent,
        slot: u32,
        ci: u16,
        value: &ErasedComp,
        (map, assets): (&NetEntities, Option<&crate::codec::AssetContext>),
        notes: &mut Vec<(u16, u32, u32)>,
    ) -> BitWriter {
        let entry = self.full.entry((slot, ci)).or_insert_with(|| {
            let mut bits = BitWriter::new();
            let cx = CodecContext::new(map).with_assets(assets);
            (c.encode)(value, &mut bits, &cx);
            let stats = cx.take_stats();
            (bits, stats.clamped, stats.unreplicated_refs)
        });
        if entry.1 > 0 || entry.2 > 0 {
            notes.push((ci, entry.1, entry.2));
        }
        entry.0.clone()
    }

    /// `at` is `(component, slot, component index)`.
    fn delta(
        &mut self,
        at: (&RepComponent, u32, u16),
        now: &ErasedComp,
        base: &ErasedComp,
        (map, assets): (&NetEntities, Option<&crate::codec::AssetContext>),
        notes: &mut Vec<(u16, u32, u32)>,
    ) -> Option<BitWriter> {
        let (c, slot, ci) = at;
        let key = (slot, ci, Arc::as_ptr(base) as *const () as usize);
        let entry = self.deltas.entry(key).or_insert_with(|| {
            let cx = CodecContext::new(map).with_assets(assets);
            if (c.eq)(now, base, &cx.scratch()) {
                None
            } else {
                let mut bits = BitWriter::new();
                (c.encode_delta)(now, base, &mut bits, &cx);
                let stats = cx.take_stats();
                Some((bits, stats.clamped, stats.unreplicated_refs))
            }
        });
        let (bits, clamped, refs) = entry.as_ref()?;
        if *clamped > 0 || *refs > 0 {
            notes.push((ci, *clamped, *refs));
        }
        Some(bits.clone())
    }
}

fn write_u128(w: &mut BitWriter, v: u128) {
    w.write_bits(v as u64, 64);
    w.write_bits((v >> 64) as u64, 64);
}

fn read_u128(r: &mut BitReader) -> Result<u128, DecodeError> {
    let lo = u128::from(r.read_bits(64)?);
    let hi = u128::from(r.read_bits(64)?);
    Ok(lo | (hi << 64))
}

/// A record's component entry count (protocol 4, ADR-8467): one `0` bit for none (a transform-only
/// update), else a `1` bit and the count minus one as a varint.
fn write_entry_count(w: &mut BitWriter, n: usize) {
    w.write_bool(n != 0);
    if n != 0 {
        w.write_varint(n as u64 - 1);
    }
}

/// Reads [`write_entry_count`]'s code, refusing a count over `max`.
fn read_entry_count(r: &mut BitReader, max: usize) -> Result<usize, DecodeError> {
    if !r.read_bool()? {
        return Ok(0);
    }
    let n = r.read_varint()?.saturating_add(1);
    let n = usize::try_from(n).unwrap_or(usize::MAX);
    if n > max {
        return Err(DecodeError::TooLong { len: n, max });
    }
    Ok(n)
}

fn write_entries(w: &mut BitWriter, entries: &[(u16, u64, BitWriter)]) {
    write_entry_count(w, entries.len());
    for (ci, kind, bits) in entries {
        w.write_varint(u64::from(*ci));
        w.write_bits(*kind, 2);
        w.write_varint(bits.bit_len() as u64);
        w.append(bits);
    }
}

/// Whether the presence bitset recorded at change detection includes component `ci`.
fn present_has(bits: &[u64], ci: usize) -> bool {
    bits.get(ci / 64)
        .is_some_and(|word| word & (1u64 << (ci % 64)) != 0)
}

/// Predicate answers and peer group sets from one gather.
struct AudienceTrack {
    predicates: HashMap<(NetId, u16), bool>,
    peer_groups: HashMap<PeerId, HashSet<String>>,
}

/// Builds this gather's predicate cache and peer group sets, and marks entities whose audience
/// changed. A predicate runs only when the entity carries that component.
fn track_audience(
    world: &mut World,
    registry: &ReplicationRegistry,
    rep: &mut ServerReplication,
) -> AudienceTrack {
    let mut next_pred: HashMap<(NetId, u16), bool> = HashMap::new();
    let mut volatile_slots: Vec<u32> = Vec::new();
    for (ci, c) in registry.components().iter().enumerate() {
        let Audience::Predicate(f) = c.options.audience else {
            continue;
        };
        let ci16 = ci as u16;
        for (id, s) in rep.entities.iter() {
            if !present_has(&s.present, ci) {
                continue;
            }
            let now = f(world, s.entity);
            if rep.predicate_results.get(&(id, ci16)) != Some(&now) {
                volatile_slots.push(s.slot);
            }
            next_pred.insert((id, ci16), now);
        }
    }
    let groups_of_peer = peer_groups(world);
    let peers_changed = groups_of_peer != rep.peer_group_sets;
    let shared: Vec<usize> = registry
        .components()
        .iter()
        .enumerate()
        .filter(|(_, c)| matches!(c.options.audience, Audience::SharedGroup))
        .map(|(i, _)| i)
        .collect();
    let mut old_groups = std::mem::take(&mut rep.entity_groups);
    let mut next_groups: HashMap<NetId, Vec<String>> = HashMap::new();
    if !shared.is_empty() {
        for (id, s) in rep.entities.iter() {
            if !shared.iter().any(|ci| present_has(&s.present, *ci)) {
                continue;
            }
            if peers_changed {
                volatile_slots.push(s.slot);
            }
            let now = world
                .get::<NetGroups>(s.entity)
                .map(|g| g.0.as_slice())
                .unwrap_or(&[]);
            match old_groups.remove(&id) {
                Some(prev) if prev.as_slice() == now => {
                    next_groups.insert(id, prev);
                }
                None if now.is_empty() => {
                    next_groups.insert(id, Vec::new());
                }
                _ => {
                    volatile_slots.push(s.slot);
                    next_groups.insert(id, now.to_vec());
                }
            }
        }
    }
    rep.entity_groups = next_groups;
    volatile_slots.sort_unstable();
    volatile_slots.dedup();
    for slot in volatile_slots {
        if let Some(s) = rep.entities.at_mut(slot) {
            s.volatile = true;
        }
    }
    AudienceTrack {
        predicates: next_pred,
        peer_groups: groups_of_peer,
    }
}

fn peer_groups(world: &mut World) -> HashMap<PeerId, HashSet<String>> {
    let mut out: HashMap<PeerId, HashSet<String>> = HashMap::new();
    let mut q = world.query::<(&crate::model::NetPlayer, &NetGroups)>();
    for (p, g) in q.iter(world) {
        out.entry(p.id.peer).or_default().extend(g.0.iter().cloned());
    }
    out
}

/// What audience evaluation needs for one connection.
struct AudienceInput<'a> {
    world: &'a World,
    peer: PeerId,
    predicates: &'a HashMap<(NetId, u16), bool>,
    groups: &'a HashMap<PeerId, HashSet<String>>,
}

impl AudienceInput<'_> {
    /// `own` is `(owned by this connection, predicted by its owner)`.
    fn matches(
        &self,
        audience: Audience,
        spawn: bool,
        own: (bool, bool),
        entity: Entity,
        id: NetId,
        ci: u16,
    ) -> bool {
        let (owned, predicted) = own;
        match audience {
            Audience::All => true,
            Audience::OwnerOnly => owned,
            Audience::Others => !owned,
            Audience::SpawnOnly => spawn,
            Audience::SpawnThenOwner => spawn || owned,
            Audience::ProxyCopies => !(owned && predicted),
            Audience::SharedGroup => {
                let Some(mine) = self.groups.get(&self.peer) else {
                    // A recorder with the `Everything` view has no groups and sees them all.
                    return crate::recorder::is_virtual_peer(self.peer);
                };
                self.world
                    .get::<NetGroups>(entity)
                    .is_some_and(|g| g.0.iter().any(|n| mine.contains(n)))
            }
            Audience::Predicate(_) => self.predicates.get(&(id, ci)).copied().unwrap_or(false),
        }
    }
}

fn assign_new(world: &mut World, rep: &mut ServerReplication, tick: NetTick) {
    let _span = tracing::trace_span!("net_assign").entered();
    let mut q = world.query_filtered::<(Entity, &Replicated), Without<NetId>>();
    let mut new: Vec<(Entity, Replicated)> = q.iter(world).map(|(e, r)| (e, r.clone())).collect();
    new.sort_by_key(|(e, _)| e.to_bits());
    for (e, r) in new {
        if rep.next_net_id == u32::MAX {
            if !rep.exhausted_logged {
                rep.exhausted_logged = true;
                tracing::error!(target: "net", entity = ?e, "net.netid_exhausted");
            }
            continue;
        }
        rep.next_net_id = rep.next_net_id.max(1);
        let id = NetId(rep.next_net_id);
        rep.next_net_id += 1;
        let kind = spawn_kind_of(world, e);
        let depth = depth_of(world, e);
        if world.get::<ChildOf>(e).is_some()
            && replicated_parent(world, e).is_none()
            && rep.warned_parent.insert(e)
        {
            rep.counters.parent_not_replicated += 1;
            tracing::warn!(target: "net", entity = ?e, "net.parent_not_replicated");
        }
        if let Ok(mut ent) = world.get_entity_mut(e) {
            ent.insert((id, NetRole::Authority));
        }
        world.resource_mut::<NetEntities>().insert(id, e);
        // A new entity resolves its own owner (`owner_known`); no other entity's chain moved.
        rep.membership_stale = true;
        rep.spawned_pending = true;
        let slot = rep.entities.insert(
            id,
            ServerEntity {
                id,
                entity: e,
                slot: 0,
                kind,
                depth,
                transform_mode: r.transform,
                sleep: r.sleep,
                asleep: false,
                by_marker: false,
                started: false,
                quiet_ticks: 0,
                teleport: false,
                resend: false,
                last_change: tick,
                server_values: Vec::new(),
                changed: Vec::new(),
                dirty: true,
                structure: true,
                volatile: false,
                tcache: None,
                current: None,
                fresh_version: false,
                present: Vec::new(),
                owner: PeerId::SERVER,
                owner_inputs: (None, None, None),
                predicted: false,
                parent_net: None,
                parent_moved: false,
                forced: false,
                moved: true,
                predicted_flipped: false,
                owner_flipped: false,
                owner_known: false,
                owner_pending: false,
                priority_scale: 1.0,
                heat: (0.0, 0.0),
                quant: None,
                quant_gen: 0,
                quant_set: false,
                quant_had_ref: false,
                gate: None,
            },
        );
        if let Some(s) = rep.entities.at_mut(slot) {
            s.slot = slot;
        }
        rep.entity_gen = rep.entity_gen.wrapping_add(1);
    }
}

/// Most removals [`ReplicatedRemovals`] queues between two gathers; past it the next gather scans
/// every tracked entity instead.
const MAX_QUEUED_REMOVALS: usize = 65_536;

/// Server: the entities whose [`Replicated`] left since the last gather (removed, or the entity
/// despawned), queued by the component's `on_remove` hook (ADR-7213). The gather checks only these
/// for despawns instead of every tracked entity. Inserted by the first gather (which scans
/// everything); a world without it queues nothing.
#[derive(Resource, Debug, Default)]
struct ReplicatedRemovals {
    entities: Vec<Entity>,
    /// More than [`MAX_QUEUED_REMOVALS`] arrived: the next gather scans every tracked entity.
    overflowed: bool,
}

/// `Replicated`'s `on_remove` hook: queues the entity for the next gather's despawn check.
fn replicated_removed(mut world: DeferredWorld<'_>, context: HookContext) {
    if let Some(mut removals) = world.get_resource_mut::<ReplicatedRemovals>() {
        if removals.entities.len() < MAX_QUEUED_REMOVALS {
            removals.entities.push(context.entity);
        } else {
            removals.overflowed = true;
        }
    }
}

fn collect_despawns(world: &mut World, rep: &mut ServerReplication) -> Vec<(NetId, u32)> {
    let mut gone = Vec::new();
    let replicated = world.component_id::<Replicated>();
    let lost = |world: &World, e: Entity| {
        !world
            .get_entity(e)
            .is_ok_and(|e| replicated.is_some_and(|c| e.contains_id(c)))
    };
    // The hook's queue names what may have gone; the first gather (no queue yet) and an
    // overflowed queue check every tracked entity.
    let queued = match world.get_resource_mut::<ReplicatedRemovals>() {
        Some(mut removals) if !removals.overflowed => Some(std::mem::take(&mut removals.entities)),
        Some(mut removals) => {
            removals.overflowed = false;
            removals.entities.clear();
            None
        }
        None => {
            world.insert_resource(ReplicatedRemovals::default());
            None
        }
    };
    let mut missing: Vec<(NetId, Entity, SpawnKind)> = match queued {
        Some(queued) => {
            let entities = world.resource::<NetEntities>();
            let mut ids: Vec<NetId> = queued.iter().filter_map(|e| entities.net_id(*e)).collect();
            ids.sort_unstable();
            ids.dedup();
            // Counted after the dedup: a remove → re-add → remove in one tick queues the entity
            // twice, but the gather checks it once.
            rep.despawn_checks = ids.len() as u64;
            ids.into_iter()
                .filter_map(|id| rep.entities.get(id).map(|s| (id, s.entity, s.kind)))
                .filter(|(_, e, _)| lost(world, *e))
                .collect()
        }
        None => {
            rep.despawn_checks = rep.entities.len() as u64;
            rep.entities
                .iter()
                .filter(|(_, s)| lost(world, s.entity))
                .map(|(id, s)| (id, s.entity, s.kind))
                .collect()
        }
    };
    missing.sort_unstable_by_key(|(id, _, _)| *id);
    if missing.is_empty() {
        // A travel that despawned nothing replicated leaves nothing to forget.
        rep.level_reset_pending = false;
        rep.travel_removed.clear();
        return gone;
    }
    let mut ids = Vec::with_capacity(missing.len());
    let mut dead: Vec<Entity> = Vec::new();
    for (id, e, kind) in missing {
        if world.get_entity(e).is_err() {
            dead.push(e);
        }
        if world.get_entity(e).is_ok() {
            // `Replicated` removed: client copies go, the server entity stays local.
            if let Ok(mut ent) = world.get_entity_mut(e) {
                ent.remove::<(NetId, NetRole)>();
            }
        } else if let SpawnKind::Level { level, guid } = kind {
            rep.level_removed.push((level, guid));
        }
        world.resource_mut::<NetEntities>().remove(id);
        world.resource_mut::<OwnerCache>().forget(e);
        // The warn-once sets keep live entities only: a long session that churns warned entities
        // would otherwise grow them by every entity it ever warned about (sprint-deep-zai-replication
        // 1 and 2). A `NetId` is never handed out again, and an entity that comes back as a
        // replicated one is a new replicated life, warned about again.
        rep.warned_parent.remove(&e);
        rep.warned_large.remove(&id);
        ids.push(id);
    }
    if std::mem::take(&mut rep.level_reset_pending) {
        // The travel's own despawns took their levels with them; a removal pushed in the same
        // pass for anything the travel did NOT despawn (a persistent level entity the game
        // destroyed before this gather) is still news a joiner into the new level must hear.
        rep.level_removed
            .retain(|entry| !rep.travel_removed.contains(entry));
        rep.travel_removed.clear();
    }
    rep.membership_stale = true;
    // A live entity linked to one that is gone resolves to another owner; nothing else does (a
    // chain through a despawned parent left with the parent, and a child that stays lost its
    // `ChildOf`, which its own change detection sees).
    if !dead.is_empty() {
        dead.sort_unstable();
        let mut links = world.query::<&crate::model::NetOwnerLink>();
        if links.iter(world).any(|l| dead.binary_search(&l.0).is_ok()) {
            rep.owners_stale = true;
        }
    }
    for (id, slot, _) in rep.entities.remove_many(ids) {
        gone.push((id, slot));
        rep.entity_gen = rep.entity_gen.wrapping_add(1);
    }
    gone
}

/// Moves the server's own change notices (gathered at the end of the previous tick) into the
/// notice resources; called at `NetSet::Apply` after clearing.
pub(crate) fn deliver_server_notices(world: &mut World) {
    let pending = match world.get_resource_mut::<ServerReplication>() {
        Some(mut rep) => std::mem::take(&mut rep.pending_server_notices),
        None => return,
    };
    let registry = world.get_resource::<ReplicationRegistry>().cloned();
    let Some(registry) = registry else {
        return;
    };
    for (ci, e, prev) in pending {
        if let Some(c) = registry.get(ci) {
            (c.notify_server)(world, e, prev);
        }
    }
}

/// Clears every per-tick replication event resource (start of `NetSet::Apply`).
pub(crate) fn clear_events(world: &mut World) {
    if let Some(mut ev) = world.get_resource_mut::<NetEvents>() {
        ev.clear();
    }
    let registry = world.get_resource::<ReplicationRegistry>().cloned();
    if let Some(registry) = registry {
        for c in registry.components() {
            (c.clear_events)(world);
        }
    }
}

// ---- client apply ------------------------------------------------------------------------------

/// One pending entity reference on a client (spec 71 § 5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingRef {
    /// The referring entity.
    pub entity: Entity,
    /// The component index.
    pub component: u16,
    /// The id it waits for.
    pub waiting_for: NetId,
    /// When it started waiting.
    pub since: NetTick,
}

/// Client replication state.
#[derive(Resource, Debug, Default)]
pub struct ClientReplication {
    /// Pending references, oldest first.
    pub pending_refs: std::collections::VecDeque<PendingRef>,
    /// How many pending references each `(entity, component)` has.
    pending_keys: HashMap<(Entity, u16), usize>,
    /// Ids to ask the server to send in full again.
    pub resync: BTreeSet<NetId>,
    /// Counters.
    pub counters: ReplicationCounters,
    warned_prefab: HashSet<u128>,
    referrers: HashMap<NetId, Vec<(Entity, u16)>>,
    /// Copies waiting for their parent's spawn: `(child, parent id)`.
    pending_parents: Vec<(Entity, NetId)>,
    refs_of: HashMap<(Entity, u16), Vec<NetId>>,
    resolved_now: Vec<(Entity, u16)>,
    /// The newest applied server tick.
    pub applied: Option<NetTick>,
    /// The interpolated proxy whose entries are being applied: `(copy, tick, spawn record)`.
    step_capture: Option<(Entity, u64, bool)>,
    /// The newest tick frame applied (on the state channel `applied` also follows state frames).
    pub applied_stream: Option<NetTick>,
    /// The state channel (spec 72 § 4).
    pub state: ClientStateChannel,
    /// Level binds waiting for the streamed cell of their entity: `(level, guid) -> id`.
    level_binds: BTreeMap<(u128, u128), NetId>,
    /// The reverse of `level_binds`.
    level_bind_ids: HashMap<NetId, (u128, u128)>,
    /// Bound copies of streamed level entities: their cell's unload despawns them, which is not a
    /// loss, and their bind waits for the cell again.
    streamed_copies: HashMap<NetId, (u128, u128)>,
    /// Streamed level entities the server removed: despawned whenever their cell spawns them.
    level_removed_streamed: BTreeSet<(u128, u128)>,
    /// The `StreamedReplicated` revision the binds were last checked against.
    streamed_revision: Option<u64>,
    warned_bind_cap: bool,
    /// The most level binds kept at once (spec 71 § 4.5); `None` is [`MAX_BUFFERED_LEVEL_BINDS`].
    pub level_bind_cap: Option<usize>,
}

/// The default bound on level binds a client keeps for entities whose streamed cell has not spawned
/// (`ClientReplication::level_bind_cap`).
pub const MAX_BUFFERED_LEVEL_BINDS: usize = 16_384;

impl ClientReplication {
    /// Forgets every copy and reference, keeping the counters and the caps (a replay's reset, spec 103 § 8.1).
    pub(crate) fn forget_everything(&mut self) {
        let (counters, cap) = (self.counters, self.level_bind_cap);
        // ADR-8850: the state channel's `entity_cap` survives the reset like `level_bind_cap`,
        // so it is carried out before the rebuild (which would default it).
        let mut state = std::mem::take(&mut self.state);
        state.reset_for_new_session();
        *self = Self {
            counters,
            level_bind_cap: cap,
            state,
            ..Self::default()
        };
    }

    /// The newest server tick whose state this client has. On the state channel that is the newest
    /// state frame: tick frames carry no state there, so `applied` can run ahead of it.
    pub fn state_tick(&self) -> Option<NetTick> {
        if self.state.active() {
            self.state.newest_tick
        } else {
            self.applied
        }
    }

    /// A copy waits for its parent's spawn (the parent's copy went away first).
    pub(crate) fn push_pending_parent(&mut self, child: Entity, parent: NetId) {
        self.pending_parents.push((child, parent));
    }

    /// Level binds kept until the streamed cell of their entity spawns (spec 71 § 4.5), as
    /// `(level, guid, id)` in `(level, guid)` order.
    pub fn buffered_level_binds(&self) -> impl Iterator<Item = (u128, u128, NetId)> + '_ {
        self.level_binds
            .iter()
            .map(|(&(level, guid), &id)| (level, guid, id))
    }

    /// Whether `id` is a level bind waiting for its cell.
    pub fn is_level_bind_buffered(&self, id: NetId) -> bool {
        self.level_bind_ids.contains_key(&id)
    }

    /// Keeps a level bind until its entity spawns. False (counted) when the buffer is full.
    fn buffer_level_bind(&mut self, level: u128, guid: u128, id: NetId) -> bool {
        let key = (level, guid);
        if self.level_binds.get(&key) == Some(&id) {
            return true;
        }
        let cap = self.level_bind_cap.unwrap_or(MAX_BUFFERED_LEVEL_BINDS);
        if !self.level_binds.contains_key(&key) && self.level_binds.len() >= cap {
            self.counters.level_entity_buffer_dropped += 1;
            if !self.warned_bind_cap {
                self.warned_bind_cap = true;
                tracing::warn!(target: "net", cap, "net.level_bind_buffer_full");
            }
            return false;
        }
        if let Some(old) = self.level_binds.insert(key, id) {
            self.level_bind_ids.remove(&old);
        }
        if let Some(old) = self.level_bind_ids.insert(id, key)
            && old != key
        {
            self.level_binds.remove(&old);
        }
        self.counters.level_entity_buffered += 1;
        true
    }

    /// Ends a kept level bind; its key, when it had one.
    fn take_level_bind(&mut self, id: NetId) -> Option<(u128, u128)> {
        let key = self.level_bind_ids.remove(&id)?;
        self.level_binds.remove(&key);
        Some(key)
    }

    /// Ends every kept level bind and forgets the removed-entity list (a new session's ids and
    /// level state are its own).
    fn clear_level_binds(&mut self) {
        self.counters.level_entity_buffer_cleared += self.level_binds.len() as u64;
        self.level_binds.clear();
        self.level_bind_ids.clear();
        self.streamed_copies.clear();
        self.level_removed_streamed.clear();
    }
}

/// Host hook: instantiates a prefab on a client for a spawn record. Without one installed the
/// engine spawner runs ([`engine_prefab_spawn`]: the prefab from the client's `PrefabLibrary`); a
/// prefab neither can make spawns bare (and logs `net.prefab_missing` once per asset).
#[derive(Resource)]
pub struct NetPrefabSpawner(pub PrefabSpawnFn);

impl NetPrefabSpawner {
    /// The engine's spawner ([`engine_prefab_spawn`]) as a hook, for a game that wraps it or
    /// installs it explicitly.
    pub fn engine() -> Self {
        Self(Box::new(engine_prefab_spawn))
    }
}

/// Per-world counter behind [`engine_prefab_spawn`]'s root guids (ADR-7213). A process-wide
/// counter would make a replay that triggers prefab spawns differ from run to run; a fresh world
/// starts over and mints the same guid sequence for the same spawn order.
#[derive(Resource, Debug, Default)]
struct PrefabSpawnCounter(u64);

/// The engine prefab spawner (spec 71 § 4.4, ADR-7213): instantiates prefab `prefab` from the
/// world's `regia_scene::prefab::PrefabLibrary` under a fresh root. `None` when the world has no
/// prefab library or type registry (no scene support), the library does not hold the template
/// (or a template it nests), or the instantiation fails: the copy then spawns bare and
/// `net.prefab_missing` is logged once for the asset.
pub fn engine_prefab_spawn(world: &mut World, prefab: u128) -> Option<Entity> {
    use regia_scene::format::{PrefabInstanceData, SceneAssetRef};
    use regia_scene::prefab::{PrefabLibrary, instantiate_prefab};
    use regia_scene::spawn::SpawnOptions;
    let asset = regia_core::id::Guid::from_u128(prefab);
    let loaded = world
        .get_resource::<PrefabLibrary>()
        .is_some_and(|library| library.first_missing(asset).is_none());
    if !loaded || !world.contains_resource::<regia_ecs::reflect::AppTypeRegistry>() {
        return None;
    }
    // Member guids derive from the root guid: each copy gets its own, from the world's counter
    // mixed with the prefab id (never nil); a replay of the same spawns mints the same guids.
    let n = {
        let mut counter = world.get_resource_or_insert_with(PrefabSpawnCounter::default);
        counter.0 += 1;
        counter.0
    };
    let root_guid = regia_core::id::Guid::from_u128(
        (prefab.rotate_left(64) ^ (u128::from(n) << 1) ^ 0x6e65_745f_7072_6566_6162_5f63_6f70_7901).max(1),
    );
    let root = world.spawn(EntityGuid(root_guid)).id();
    let instance = PrefabInstanceData::plain(SceneAssetRef(asset));
    match instantiate_prefab(world, &instance, root, &SpawnOptions::default(), 0) {
        Ok(_) => Some(root),
        Err(err) => {
            tracing::warn!(target: "net", prefab = %format!("{prefab:032x}"), error = %err, "net.prefab_spawn_failed");
            if let Ok(e) = world.get_entity_mut(root) {
                e.despawn();
            }
            None
        }
    }
}

/// Instantiates prefab `u128` in the world and returns its root (`None` = cannot load it).
pub type PrefabSpawnFn = Box<dyn Fn(&mut World, u128) -> Option<Entity> + Send + Sync>;

/// Records one tick frame may hold per section. Spawns and despawns are not budgeted without the
/// interest pipeline, so a world can really send this many at once: the bound that matters is the
/// frame's remaining input.
pub const MAX_RECORDS_PER_FRAME: usize = 1 << 20;
const MIN_OWNERSHIP_BITS: usize = 10;
// The smallest records of protocol 4 (ADR-8467): a one-bit empty entry count.
const MIN_SPAWN_BITS: usize = 16;
const MIN_UPDATE_BITS: usize = 11;
const MIN_DESPAWN_BITS: usize = 9;
const MIN_LEVEL_BITS: usize = 256;

/// Reads a record count, bounded by what the rest of the frame could hold: a lie fails here, before
/// any record is read.
fn read_count(r: &mut BitReader, min_bits: usize) -> Result<usize, DecodeError> {
    r.read_len(MAX_RECORDS_PER_FRAME.min(r.remaining_bits() / min_bits))
}

/// A new connection came up on this world: the ticks and message sequence of whatever it was
/// connected to before mean nothing to this server, and frames pending from then must not run.
/// Copies stay: the server's spawn records adopt them.
pub fn client_session_started(world: &mut World) {
    world.init_resource::<ClientReplication>();
    {
        let mut client = world.resource_mut::<ClientReplication>();
        client.applied = None;
        client.applied_stream = None;
        // ADR-8850: the host-set `entity_cap` survives the new session, exactly like
        // `level_bind_cap` (the bridge calls this on every connect).
        client.state.reset_for_new_session();
        client.clear_level_binds();
    }
    if let Some(mut node) = world.get_resource_mut::<NetNode>() {
        node.forget_server();
    }
    // The previous server's tick numbers must not outlive it: a prediction watermark or a full
    // snapshot ring of high ticks would swallow every frame of the new server.
    if let Some(mut cp) = world.get_resource_mut::<crate::predict::ClientPrediction>() {
        cp.reset_for_new_session();
    }
    if let Some(mut n) = world.get_resource_mut::<crate::interp::NetInterpolation>() {
        n.clock.restart();
    }
    let mut buffers = world.query::<&mut crate::interp::NetSnapshotBuffer>();
    for mut b in buffers.iter_mut(world) {
        b.0.clear();
    }
}

/// Walks one tick's replication section without touching any world, so a frame that is corrupt
/// anywhere is refused before its first spawn. Mirrors [`client_apply_state`]; component payloads
/// are skipped by their lengths (their decode depends on the local registry).
pub fn validate_state_section(r: &mut BitReader) -> Result<(), DecodeError> {
    let _span = tracing::trace_span!("net_validate").entered();
    for _ in 0..read_count(r, MIN_OWNERSHIP_BITS)? {
        read_net_id(r)?;
        r.read_bits(2)?;
    }
    for _ in 0..read_count(r, MIN_SPAWN_BITS)? {
        read_net_id(r)?;
        match r.read_bits(2)? {
            0 => {}
            1 => {
                read_u128(r)?;
            }
            2 => {
                read_u128(r)?;
                read_u128(r)?;
            }
            _ => return Err(DecodeError::Invalid("spawn kind")),
        }
        if r.read_bool()? {
            read_net_id(r)?;
        }
        r.read_bits(2)?;
        let mode = transform_mode_from(r.read_bits(2)?);
        if mode != TransformReplication::None && r.read_bool()? {
            r.read_bool()?;
            decode_transform(mode, r)?;
        }
        skip_entries(r)?;
    }
    for _ in 0..read_count(r, MIN_UPDATE_BITS)? {
        read_net_id(r)?;
        if r.read_bool()? && r.read_bool()? {
            read_net_id(r)?;
        }
        if r.read_bool()? {
            r.read_bool()?;
            let mode = transform_mode_from(r.read_bits(2)?);
            decode_transform(mode, r)?;
        }
        skip_entries(r)?;
    }
    for _ in 0..read_count(r, MIN_DESPAWN_BITS)? {
        read_net_id(r)?;
        r.read_bool()?;
    }
    for _ in 0..read_count(r, MIN_LEVEL_BITS)? {
        read_u128(r)?;
        read_u128(r)?;
    }
    Ok(())
}

/// Applies one tick's replication section on a client (spawns, state; despawns are returned so
/// the runtime can run messages between state and despawns, spec 70 § 6.5 item 3).
///
/// The runtime has already walked the section with [`validate_state_section`], so a frame that
/// fails here does so for world-dependent reasons only. Spawn and update records of an id a
/// travel or despawn record already tombstoned are dropped and counted, like the state channel's
/// record loop: a retransmitted record of the old level must not bring a copy back (ADR-7295).
pub fn client_apply_state(
    world: &mut World,
    r: &mut BitReader,
    tick: NetTick,
) -> Result<Vec<(NetId, bool)>, DecodeError> {
    let _span = tracing::trace_span!("net_apply").entered();
    world.init_resource::<ClientReplication>();
    world.init_resource::<NetEvents>();
    world.init_resource::<NetEntities>();
    world.init_resource::<ReplicationRegistry>();
    let registry = world.resource::<ReplicationRegistry>().clone();
    let mut spawned_now: Vec<NetId> = Vec::new();
    let mut level_index: LevelIndex = None;
    // On the state channel the clock follows state frames: a retransmitted tick frame arrives late.
    if !world.resource::<ClientReplication>().state.active() {
        crate::interp::on_state_frame(world, tick);
    }

    // Ownership changes.
    let n = read_count(r, MIN_OWNERSHIP_BITS)?;
    for _ in 0..n {
        let id = read_net_id(r)?;
        let owned = r.read_bool()?;
        let predicted = r.read_bool()?;
        if let Some(e) = world.resource::<NetEntities>().entity(id) {
            set_owned(world, e, owned, predicted);
            if owned || predicted {
                // An owned or predicted copy is not interpolated: queued step values apply now,
                // before newer state or local prediction (ADR-1662 (9d)).
                crate::interp::flush_steps(world, e);
            }
        } else {
            // Its spawn record may still be on the unreliable channel.
            world
                .resource_mut::<ClientReplication>()
                .state
                .hold_ownership(id, owned, predicted, tick);
        }
    }

    // Spawns.
    let n = read_count(r, MIN_SPAWN_BITS)?;
    for _ in 0..n {
        let id = read_net_id(r)?;
        let kind = match r.read_bits(2)? {
            0 => SpawnKind::Bare,
            1 => SpawnKind::Prefab(read_u128(r)?),
            2 => SpawnKind::Level {
                level: read_u128(r)?,
                guid: read_u128(r)?,
            },
            _ => return Err(DecodeError::Invalid("spawn kind")),
        };
        let parent = if r.read_bool()? {
            Some(read_net_id(r)?)
        } else {
            None
        };
        let owned = r.read_bool()?;
        let predicted = r.read_bool()?;
        let mode = transform_mode_from(r.read_bits(2)?);
        let transform = if mode != TransformReplication::None && r.read_bool()? {
            let teleport = r.read_bool()?;
            Some((decode_transform(mode, r)?, teleport))
        } else {
            None
        };
        // A record of an id a travel or despawn record already dropped (ADR-7295): a travel
        // overtakes the replication stream, so a record of the old level still in flight must
        // not bring the copy back for the ticks until the server's despawn records arrive. The
        // despawn notice clears the tombstone when it arrives, as on the state channel.
        {
            let mut client = world.resource_mut::<ClientReplication>();
            if client.state.is_tombstoned(id, tick.0) {
                client.state.counters.tombstoned += 1;
                skip_entries(r)?;
                continue;
            }
        }
        let existing = world.resource::<NetEntities>().entity(id);
        let alive = existing.filter(|e| world.get_entity(*e).is_ok());
        if existing.is_none() {
            // ADR-8760: the tick stream keeps the same entity cap as the state channel.
            let held = world.resource::<NetEntities>().len();
            let mut client = world.resource_mut::<ClientReplication>();
            if held >= client.state.cap() {
                client.state.refuse_over_cap(id);
                client.state.take_cap_refusal();
                return Err(DecodeError::Invalid("client entity cap"));
            }
        }
        let e = match alive {
            Some(e) => e,
            None => match create_copy(world, kind, &mut level_index, id) {
                Some(e) => e,
                None => {
                    // Kept until its cell spawns, which asks for the full state again.
                    world.resource_mut::<NetEntities>().remove(id);
                    skip_entries(r)?;
                    continue;
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
        let transform = transform.map(|(t, teleport)| (to_local_frame(world, e, t), teleport));
        if let Ok(mut ent) = world.get_entity_mut(e) {
            ent.insert(NetTransformMode(mode));
        }
        crate::interp::adopt_named_profile(world, e);
        let interp = crate::interp::is_interpolated_proxy(world, e);
        if !interp {
            crate::interp::flush_steps(world, e);
        } else if existing.is_some() {
            // A spawn record is the full current state: older queued step values must not land
            // on top of it later.
            crate::interp::drop_steps(world, e);
        }
        if let Some((t, teleport)) = transform {
            if interp && existing.is_some() {
                // A resync or a copy back in scope: the shown pose moves through the buffer.
                note_transform(world, e, mode, teleport);
            } else {
                write_transform(world, e, mode, &t, teleport);
            }
            if interp {
                crate::interp::buffer_transform(world, e, proxy_state(tick, &t, teleport, false));
            }
        }
        if interp && existing.is_none() {
            crate::interp::hide_until_shown(world, e, tick);
        }
        let applied = apply_captured(world, r, e, &registry, true, tick, interp)?;
        if existing.is_some() {
            // A spawn record for a copy this client already has (a resync, or a level copy back in
            // scope) is the full current state: components it no longer carries go.
            for (ci, c) in registry.components().iter().enumerate() {
                if !applied.contains(&(ci as u16)) && (c.has)(world, e) {
                    (c.remove)(world, e);
                }
            }
        }
        attach_waiting_children(world, id, e);
        if existing.is_none() {
            world.resource_mut::<NetEvents>().spawned.push(NetSpawned {
                entity: e,
                net_id: id,
            });
        }
        spawned_now.push(id);
    }

    // Updates.
    let n = read_count(r, MIN_UPDATE_BITS)?;
    for _ in 0..n {
        let id = read_net_id(r)?;
        let e = world.resource::<NetEntities>().entity(id);
        let parent_changed = r.read_bool()?;
        let parent = if parent_changed {
            if r.read_bool()? {
                Some(Some(read_net_id(r)?))
            } else {
                Some(None)
            }
        } else {
            None
        };
        let has_t = r.read_bool()?;
        let t = if has_t {
            let teleport = r.read_bool()?;
            let mode = transform_mode_from(r.read_bits(2)?);
            Some((decode_transform(mode, r)?, teleport, mode))
        } else {
            None
        };
        let Some(e) = e.filter(|e| world.get_entity(*e).is_ok()) else {
            // A copy this client lost: skip its entries and ask for full state, unless a travel
            // or despawn record already tombstoned the id: the server despawned it, its records
            // are on the way, and asking for full state would resurrect it (ADR-7295).
            skip_entries(r)?;
            let mut client = world.resource_mut::<ClientReplication>();
            if client.state.is_tombstoned(id, tick.0) {
                client.state.counters.tombstoned += 1;
            } else if !client.is_level_bind_buffered(id) {
                client.resync.insert(id);
            }
            continue;
        };
        if let Some(parent) = parent {
            set_parent(world, e, parent);
        }
        let t = t.map(|(t, teleport, mode)| (to_local_frame(world, e, t), teleport, mode));
        let interp = crate::interp::is_interpolated_proxy(world, e);
        if !interp {
            crate::interp::flush_steps(world, e);
        }
        if let Some((t, teleport, mode)) = t {
            if interp {
                note_transform(world, e, mode, teleport);
                crate::interp::buffer_transform(world, e, proxy_state(tick, &t, teleport, false));
            } else {
                write_transform(world, e, mode, &t, teleport);
            }
        }
        apply_captured(world, r, e, &registry, false, tick, interp)?;
    }

    // Despawns (returned; applied after messages).
    let n = read_count(r, MIN_DESPAWN_BITS)?;
    let mut gone = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        let id = read_net_id(r)?;
        let scope = r.read_bool()?;
        gone.push((id, scope));
    }
    // Level entities removed before this client joined.
    let n = read_count(r, MIN_LEVEL_BITS)?;
    for _ in 0..n {
        let level = read_u128(r)?;
        let guid = read_u128(r)?;
        if let Some(e) = find_level_entity(world, &mut level_index, level, guid) {
            let _ = world.despawn(e);
        } else if is_streamed(world, level, guid) {
            // Its cell has not spawned it: it goes when the cell does.
            remember_removed_streamed(world, level, guid);
        }
    }

    after_spawns(world, &spawned_now);
    let mut client = world.resource_mut::<ClientReplication>();
    client.applied = Some(client.applied.map_or(tick, |a| a.max(tick)));
    client.applied_stream = Some(tick);
    Ok(gone)
}

/// Children whose parent arrived just now attach to it.
fn attach_waiting_children(world: &mut World, id: NetId, e: Entity) {
    let waiting: Vec<Entity> = {
        let mut client = world.resource_mut::<ClientReplication>();
        let mut out = Vec::new();
        client.pending_parents.retain(|(child, p)| {
            if *p == id {
                out.push(*child);
                false
            } else {
                true
            }
        });
        out
    };
    for child in waiting {
        if let Ok(mut ent) = world.get_entity_mut(child) {
            ent.insert(ChildOf(e));
        }
    }
}

/// Pending references whose target just spawned ask for the referrer's full state again; patched
/// references become events.
fn after_spawns(world: &mut World, spawned_now: &[NetId]) {
    if !spawned_now.is_empty() {
        let spawned: HashSet<NetId> = spawned_now.iter().copied().collect();
        let resolved_referrers: Vec<Entity> = world
            .resource::<ClientReplication>()
            .pending_refs
            .iter()
            .filter(|p| spawned.contains(&p.waiting_for))
            .map(|p| p.entity)
            .collect();
        let ids: Vec<NetId> = resolved_referrers
            .iter()
            .filter_map(|e| world.get::<NetId>(*e).copied())
            .collect();
        world.resource_mut::<ClientReplication>().resync.extend(ids);
    }
    let resolved = std::mem::take(&mut world.resource_mut::<ClientReplication>().resolved_now);
    let registry = world.resource::<ReplicationRegistry>().clone();
    for (entity, ci) in resolved {
        let component = registry.get(ci).map_or("?", |c| c.name);
        world
            .resource_mut::<NetEvents>()
            .refs_resolved
            .push(NetRefResolved { entity, component });
    }
}

/// Despawns copies after the tick's messages ran (spec 70 § 6.5 item 3).
pub fn client_apply_despawns(world: &mut World, gone: &[(NetId, bool)], tick: NetTick) {
    despawn_copies(world, gone, tick, false);
}

/// Client, at a travel's `TravelBegin` (spec 77 § 6.2 step 3, ADR-7259, ADR-7295): despawns these
/// copies of the old level now. On the state channel each id is tombstoned past every tick, so a
/// state frame of the old level still in flight cannot bring a copy back; on the reliable tick
/// stream the travel floor (`ClientStateChannel::begin_travel_floor`, installed by
/// `session::travel::drop_copies_for_travel`) covers the id the same way. Ids are never
/// reused; the server's own despawn notice clears the tombstone when it arrives. No interpolation
/// wait: the level is gone. Returns how many copies were despawned.
pub fn client_drop_for_travel(world: &mut World, gone: &[NetId]) -> u32 {
    if gone.is_empty()
        || world.get_resource::<ClientReplication>().is_none()
        || world.get_resource::<NetEntities>().is_none()
        || world.get_resource::<NetEvents>().is_none()
    {
        return 0;
    }
    let before = world
        .get_resource::<NetEntities>()
        .map_or(0, |ids| ids.iter().count());
    let list: Vec<(NetId, bool)> = gone.iter().map(|id| (*id, false)).collect();
    despawn_copies(world, &list, NetTick(u32::MAX), true);
    let after = world
        .get_resource::<NetEntities>()
        .map_or(0, |ids| ids.iter().count());
    u32::try_from(before.saturating_sub(after)).unwrap_or(u32::MAX)
}

/// The despawn of copies: `travel` skips the interpolation wait (a travel's drop, ADR-7259).
fn despawn_copies(world: &mut World, gone: &[(NetId, bool)], tick: NetTick, travel: bool) {
    let comp_count = world
        .get_resource::<ReplicationRegistry>()
        .map_or(0, |r| r.components().len() as u16);
    for (id, scope) in gone {
        // Over the state channel the notice can arrive after a newer spawn of the same id, and
        // older state frames can arrive after it. On the reliable tick stream a travel overtakes
        // the stream, and the notice clears the travel's tombstone for the id (ADR-7295).
        let kept_bind = {
            let mut client = world.resource_mut::<ClientReplication>();
            if client.state.active() {
                if !client.state.forget(*id, tick) {
                    continue;
                }
            } else if !travel && !*scope {
                client.state.clear_travel_tombstone(*id);
            }
            if !*scope {
                client.streamed_copies.remove(id);
            }
            let kept = client.take_level_bind(*id);
            if kept.is_some() {
                client.counters.level_entity_buffer_cleared += 1;
            }
            kept
        };
        if let Some((level, guid)) = kept_bind {
            if !*scope {
                // Despawned on the server before its cell spawned here: it goes when it does.
                remember_removed_streamed(world, level, guid);
            }
            continue;
        }
        let Some(e) = world.resource::<NetEntities>().entity(*id) else {
            continue;
        };
        if *scope {
            // Level-bound copy left relevance: keep it with its last state.
            if let Ok(mut ent) = world.get_entity_mut(e) {
                ent.insert(NetOutOfScope);
            }
            continue;
        }
        if world.get_entity(e).is_err() {
            world.resource_mut::<NetEntities>().remove(*id);
            continue;
        }
        world
            .resource_mut::<NetEvents>()
            .despawning
            .push(NetDespawning { entity: e });
        world.resource_mut::<NetEntities>().remove(*id);
        // Replicated children are not despawned with their parent's copy (they have their own
        // records): detach them and let them wait for the parent to come back. A copy whose
        // despawn waits for the interpolation tick keeps them until then (`net_interpolate`).
        let kids: Vec<Entity> = if !travel && crate::interp::would_defer_despawn(world, e, tick) {
            Vec::new()
        } else {
            world
                .get::<Children>(e)
                .map(|c| c.iter().filter(|k| world.get::<NetId>(*k).is_some()).collect())
                .unwrap_or_default()
        };
        for k in kids {
            if let Ok(mut ent) = world.get_entity_mut(k) {
                ent.remove::<ChildOf>();
            }
            world
                .resource_mut::<ClientReplication>()
                .pending_parents
                .push((k, *id));
        }
        // References to it become pending again: the referrers' full state is asked for, and
        // decodes with the target unknown (spec 71 § 5).
        let referrers = world
            .resource_mut::<ClientReplication>()
            .referrers
            .remove(id)
            .unwrap_or_default();
        let ids: Vec<NetId> = referrers
            .iter()
            .filter(|(r, _)| *r != e)
            .filter_map(|(r, _)| world.get::<NetId>(*r).copied())
            .collect();
        {
            let mut client = world.resource_mut::<ClientReplication>();
            let client = &mut *client;
            client.resync.extend(ids);
            if !client.pending_refs.is_empty() {
                client.pending_refs.retain(|p| p.entity != e);
                client.pending_keys.retain(|(k, _), _| *k != e);
            }
            client.pending_parents.retain(|(child, _)| *child != e);
            // The despawned copy's own references, through the reverse index.
            for ci in 0..comp_count {
                let key = (e, ci);
                for target in client.refs_of.remove(&key).unwrap_or_default() {
                    if let Some(list) = client.referrers.get_mut(&target) {
                        list.retain(|r| *r != key);
                        if list.is_empty() {
                            client.referrers.remove(&target);
                        }
                    }
                }
            }
        }
        // An interpolated proxy stays until its despawn tick is shown (spec 74 § 6).
        if world.get_entity(e).is_ok() && (travel || !crate::interp::defer_despawn(world, e, tick)) {
            let _ = world.despawn(e);
        }
    }
}

/// Applies a copy's component entries; for an interpolated proxy the values go through the
/// interpolation buffers (spec 74 § 3).
fn apply_captured(
    world: &mut World,
    r: &mut BitReader,
    e: Entity,
    registry: &ReplicationRegistry,
    spawn: bool,
    tick: NetTick,
    interp: bool,
) -> Result<Vec<u16>, DecodeError> {
    if interp {
        world.resource_mut::<ClientReplication>().step_capture = Some((e, u64::from(tick.0), spawn));
    }
    let applied = apply_entries(world, r, e, registry, spawn, tick);
    if interp {
        world.resource_mut::<ClientReplication>().step_capture = None;
    }
    applied
}

fn proxy_state(tick: NetTick, t: &NetTransform, teleport: bool, steady: bool) -> crate::interp::ProxyState {
    crate::interp::ProxyState {
        tick: u64::from(tick.0),
        translation: t.translation,
        rotation: t.rotation,
        scale: t.scale,
        velocity: t.velocity,
        teleport,
        steady,
    }
}

/// Client: a copy shown without interpolation (`interpolation: None`, or no interpolation clock)
/// whose newest record carries the steady bit (spec 120 § 8.2, ADR-8522). The server keeps such a
/// copy within its profile's extrapolation gate and sends a record only when the extrapolation
/// errs past it, so the copy moves along the record's velocity between records instead of
/// standing still: every client tick, its `Transform` translation is the record's moved for the
/// ticks from the record's tick to `shown_tick`. Locally owned and predicted copies, copies out of
/// scope and copies of an interrupted peer are not moved.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct NetSteady {
    /// The record's server tick.
    pub tick: u32,
    /// The record's position (this client's local frame).
    pub translation: Vec3,
    /// The record's velocity, units per second.
    pub velocity: Vec3,
    /// The server tick the copy was last shown at: the newest state frame's tick plus the local
    /// ticks since it arrived.
    pub shown_tick: f64,
}

/// Puts or removes [`NetSteady`] on a copy shown without interpolation, after its record was
/// written (`steady`: the record carried the steady bit and a velocity).
pub(super) fn set_steady(world: &mut World, e: Entity, tick: NetTick, t: &NetTransform, steady: bool) {
    let Ok(mut ent) = world.get_entity_mut(e) else {
        return;
    };
    let moving = t.velocity.filter(|v| v.is_finite() && v.length_squared() > 0.0);
    match moving.filter(|_| steady) {
        Some(velocity) => {
            ent.insert(NetSteady {
                tick: tick.0,
                translation: t.translation,
                velocity,
                shown_tick: f64::from(tick.0),
            });
        }
        None => {
            if ent.contains::<NetSteady>() {
                ent.remove::<NetSteady>();
            }
        }
    }
}

/// Client, every tick: moves each [`NetSteady`] copy along its record's velocity to the
/// estimated server tick (the newest state frame's tick plus the local ticks since it arrived;
/// ADR-8522). Only the translation moves: the gate keeps rotation within its 0.5 degrees.
pub(crate) fn advance_steady_copies(world: &mut World) {
    let Some(client) = world.get_resource::<ClientReplication>() else {
        return;
    };
    let local = world
        .get_resource::<crate::runtime::NetNode>()
        .map_or(0, |n| n.local_ticks);
    let Some(shown) = client.state.server_tick_estimate(local) else {
        return;
    };
    let hz = crate::runtime::tick_hz(world).max(1e-6);
    // As `net_interpolate`: a copy out of scope or of an interrupted peer holds where it is.
    let mut q = world.query_filtered::<(&mut NetSteady, &mut Transform), (
        Without<NetOwnedLocally>,
        Without<NetPredicted>,
        Without<NetOutOfScope>,
        Without<crate::model::NetInterrupted>,
    )>();
    for (mut s, mut t) in q.iter_mut(world) {
        let shown = shown.max(f64::from(s.tick));
        let dt = ((shown - f64::from(s.tick)) / hz) as f32;
        t.translation = s.translation + s.velocity * dt;
        s.shown_tick = shown;
    }
}

/// Whether the extrapolation gate holds back this tick's update of a copy (spec 120 § 8.2,
/// ADR-8522): the entity has a gate, its only news is the transform (the parent and every
/// component value are the baseline's, no teleport or resend is owed), and the client's
/// extrapolation of every record it may be showing is within the gate. Those records are the
/// update records sent since the acknowledged baseline that are still on their way, or the
/// baseline itself when none is. A predicted copy is never held back (reconciliation compares
/// every frame). Prunes the copy's `flight` list of resolved records.
fn gate_holds_back(
    s: &ServerEntity,
    ce: &mut ConnEntity,
    link: &StateLink,
    tick: NetTick,
    tick_hz: f64,
) -> bool {
    ce.gated = s.gate.is_some();
    let Some(gate) = s.gate else {
        return false;
    };
    let base_tick = ce.base_tick;
    ce.flight
        .retain(|(seq, v)| v.tick.0 > base_tick.0 && *seq != NO_PACKET && link.unresolved(*seq, tick));
    if ce.predicted
        || ce.teleport_due
        || ce.resend_due
        || ce.full_again
        || ce.out_of_scope
        || !ce.spawn_acked
        || s.teleport
        || s.resend
        || s.volatile
        // The client shows a far position, not the baseline's transform (ADR-10124).
        || ce.base_far
        || ce.flight.len() >= MAX_GATE_FLIGHT
    {
        return false;
    }
    let (Some(now), Some(base)) = (s.current.as_deref(), ce.base.as_deref()) else {
        return false;
    };
    let same_comps = Arc::ptr_eq(&now.comps, &base.comps)
        || (now.comps.len() == base.comps.len()
            && now
                .comps
                .iter()
                .zip(base.comps.iter())
                .all(|((a, x), (b, y))| a == b && Arc::ptr_eq(x, y)));
    if now.parent != base.parent || !same_comps {
        return false;
    }
    let Some(t) = now.transform.as_ref() else {
        return false;
    };
    let hz = tick_hz.max(1e-6);
    let within = |held: &TransformBits, held_tick: NetTick| {
        if !t.q.gate_comparable(&held.q) {
            return false;
        }
        // At this send tick, not the current version's: a stopped entity makes no new version,
        // but the client keeps moving the held record along its velocity.
        let seconds = (f64::from(tick.0.saturating_sub(held_tick.0)) / hz) as f32;
        let (moved, turned) = held.q.extrapolation_error(&t.q, seconds);
        moved <= gate && turned <= crate::profile::EXTRAPOLATION_GATE_DEG
    };
    if ce.flight.is_empty() {
        return base.transform.as_ref().is_some_and(|b| within(b, base_tick));
    }
    ce.flight
        .iter()
        .all(|(_, v)| v.transform.as_ref().is_some_and(|b| within(b, v.tick)))
}

/// Records of one gated copy on their way at most; past it the gate stops holding records back
/// until acknowledgments arrive (a bound on the per-copy list, never reached at one record per
/// error over the gate).
const MAX_GATE_FLIGHT: usize = 16;

/// The parts of a received transform that are not the shown pose: the mode, and the teleport
/// marker and event (an interpolated proxy's pose is written by `net_interpolate`).
fn note_transform(world: &mut World, e: Entity, mode: TransformReplication, teleport: bool) {
    let Ok(mut ent) = world.get_entity_mut(e) else {
        return;
    };
    ent.insert(NetTransformMode(mode));
    if teleport {
        ent.insert(NetTeleported);
        world.resource_mut::<NetEvents>().teleported.push(e);
    }
}

/// Client: the transform mode of a copy (from its spawn record).
#[derive(Component, Debug, Clone, Copy)]
pub struct NetTransformMode(pub TransformReplication);

fn write_transform(
    world: &mut World,
    e: Entity,
    mode: TransformReplication,
    t: &NetTransform,
    teleport: bool,
) {
    let Ok(mut ent) = world.get_entity_mut(e) else {
        return;
    };
    let mut tr = ent.get::<Transform>().copied().unwrap_or_default();
    tr.translation = t.translation;
    if matches!(mode, TransformReplication::Movement | TransformReplication::Full) {
        tr.rotation = t.rotation;
    }
    if mode == TransformReplication::Full {
        tr.scale = t.scale;
    }
    ent.insert((tr, NetTransformMode(mode)));
    if let Some(v) = t.velocity {
        ent.insert(NetVelocity(v));
    } else if is_replication_owned_velocity(&ent) {
        // A record without a velocity (a far record, a `PositionOnly` record, a server entity that
        // dropped its `NetVelocity`): the old replicated velocity no longer holds (ADR-10215).
        ent.remove::<NetVelocity>();
    }
    if teleport {
        ent.insert(NetTeleported);
        world.resource_mut::<NetEvents>().teleported.push(e);
    }
}

/// Whether a copy's `NetVelocity` is replication's to clear: a proxy's is (only received records
/// write it); a copy this client owns or predicts keeps whatever the local game or prediction
/// wrote when a record arrives without a velocity (ADR-10215).
pub(crate) fn is_replication_owned_velocity(ent: &EntityWorldMut<'_>) -> bool {
    ent.contains::<NetVelocity>() && !ent.contains::<NetOwnedLocally>() && !ent.contains::<NetPredicted>()
}

fn set_owned(world: &mut World, e: Entity, owned: bool, predicted: bool) {
    let Ok(mut ent) = world.get_entity_mut(e) else {
        return;
    };
    if owned {
        ent.insert(NetOwnedLocally);
    } else {
        ent.remove::<NetOwnedLocally>();
    }
    ent.insert(if owned && predicted {
        NetRole::Predicted
    } else {
        NetRole::Proxy
    });
}

/// Attaches a copy to its parent's copy, or (parent not here yet) removes the old parent and waits
/// for the parent's spawn.
/// A received transform in this client's local frame: a root's position came in the shared world
/// frame (spec 78 § 14); a child's, attached or waiting for its parent, is relative and stays.
fn to_local_frame(world: &World, e: Entity, mut t: NetTransform) -> NetTransform {
    let Some(offset) = crate::origin::wire_offset(world) else {
        return t;
    };
    let child = world.get::<ChildOf>(e).is_some()
        || world
            .resource::<ClientReplication>()
            .pending_parents
            .iter()
            .any(|(c, _)| *c == e);
    if !child {
        t.translation = crate::origin::wire_to_local(t.translation, offset);
    }
    t
}

fn set_parent(world: &mut World, e: Entity, parent: Option<NetId>) {
    {
        let mut client = world.resource_mut::<ClientReplication>();
        client.pending_parents.retain(|(child, _)| *child != e);
    }
    let target = parent.map(|p| (p, world.resource::<NetEntities>().entity(p)));
    match target {
        Some((_, Some(pe))) if pe != e => {
            if let Ok(mut ent) = world.get_entity_mut(e) {
                ent.insert(ChildOf(pe));
            }
        }
        Some((p, _)) => {
            if let Ok(mut ent) = world.get_entity_mut(e) {
                ent.remove::<ChildOf>();
            }
            world
                .resource_mut::<ClientReplication>()
                .pending_parents
                .push((e, p));
        }
        None => {
            if let Ok(mut ent) = world.get_entity_mut(e) {
                ent.remove::<ChildOf>();
            }
        }
    }
}

/// Level entities by `(level, guid)`, built once per applied frame when a level record needs it.
type LevelIndex = Option<HashMap<(u128, u128), Entity>>;

fn find_level_entity(world: &mut World, index: &mut LevelIndex, level: u128, guid: u128) -> Option<Entity> {
    let map = index.get_or_insert_with(|| {
        let mut q = world.query::<(Entity, &LevelMember, &EntityGuid)>();
        q.iter(world).map(|(e, l, g)| ((l.0, g.0.as_u128()), e)).collect()
    });
    map.get(&(level, guid))
        .copied()
        .filter(|e| world.get_entity(*e).is_ok())
}

/// Whether `guid` of `level` lives in a streamed cell (spec 78 § 14).
fn is_streamed(world: &World, level: u128, guid: u128) -> bool {
    world
        .get_resource::<StreamedReplicated>()
        .is_some_and(|table| table.cell_of(level, guid).is_some())
}

/// A streamed level entity the server removed while its cell was not spawned here.
fn remember_removed_streamed(world: &mut World, level: u128, guid: u128) {
    let mut client = world.resource_mut::<ClientReplication>();
    let cap = client.level_bind_cap.unwrap_or(MAX_BUFFERED_LEVEL_BINDS);
    if client.level_removed_streamed.len() < cap {
        client.level_removed_streamed.insert((level, guid));
    } else if !client.level_removed_streamed.contains(&(level, guid)) {
        client.counters.level_entity_buffer_dropped += 1;
    }
}

/// The client's copy for a spawn record. `None`: a level bind kept until the streamed cell of its
/// entity spawns (spec 71 § 4.5), or dropped over the cap; the record is not applied.
fn create_copy(world: &mut World, kind: SpawnKind, index: &mut LevelIndex, id: NetId) -> Option<Entity> {
    Some(match kind {
        SpawnKind::Bare => world.spawn_empty().id(),
        SpawnKind::Prefab(p) => {
            let spawner = world.remove_resource::<NetPrefabSpawner>();
            let made = match spawner.as_ref() {
                Some(s) => (s.0)(world, p),
                None => engine_prefab_spawn(world, p),
            };
            if let Some(s) = spawner {
                world.insert_resource(s);
            }
            match made {
                Some(e) => e,
                None => {
                    let mut client = world.resource_mut::<ClientReplication>();
                    client.counters.prefab_missing += 1;
                    if client.warned_prefab.insert(p) {
                        tracing::warn!(target: "net", prefab = %format!("{p:032x}"), "net.prefab_missing");
                    }
                    world.spawn_empty().id()
                }
            }
        }
        SpawnKind::Level { level, guid } => {
            let streamed = is_streamed(world, level, guid);
            match find_level_entity(world, index, level, guid) {
                Some(e) => {
                    let mut client = world.resource_mut::<ClientReplication>();
                    if client.take_level_bind(id).is_some() {
                        client.counters.level_entity_buffer_applied += 1;
                    }
                    if streamed {
                        client.streamed_copies.insert(id, (level, guid));
                    }
                    e
                }
                None if streamed => {
                    // Its cell has not spawned here yet: not a content mismatch.
                    world
                        .resource_mut::<ClientReplication>()
                        .buffer_level_bind(level, guid, id);
                    return None;
                }
                None => {
                    world
                        .resource_mut::<ClientReplication>()
                        .counters
                        .level_entity_missing += 1;
                    tracing::warn!(target: "net", guid = %format!("{guid:032x}"), "net.level_entity_missing");
                    world.spawn_empty().id()
                }
            }
        }
    })
}

fn read_net_id(r: &mut BitReader) -> Result<NetId, DecodeError> {
    u32::try_from(r.read_varint()?)
        .map(NetId)
        .map_err(|_| DecodeError::Invalid("net id"))
}

fn skip_entries(r: &mut BitReader) -> Result<(), DecodeError> {
    let n = read_entry_count(r, 1 << 16)?;
    for _ in 0..n {
        // The checks `apply_entries` makes for any component, so a frame that passes here cannot
        // fail there on its shape.
        u16::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("component"))?;
        if !matches!(r.read_bits(2)?, ENTRY_REMOVED | ENTRY_FULL | ENTRY_DELTA) {
            return Err(DecodeError::Invalid("entry kind"));
        }
        let len = r.read_len(r.remaining_bits())?;
        skip_bits(r, len)?;
    }
    Ok(())
}

fn skip_bits(r: &mut BitReader, mut len: usize) -> Result<(), DecodeError> {
    while len > 0 {
        let take = len.min(64);
        r.read_bits(take as u32)?;
        len -= take;
    }
    Ok(())
}

fn apply_entries(
    world: &mut World,
    r: &mut BitReader,
    e: Entity,
    registry: &ReplicationRegistry,
    spawn: bool,
    tick: NetTick,
) -> Result<Vec<u16>, DecodeError> {
    let n = read_entry_count(r, 1 << 16)?;
    let mut applied = Vec::with_capacity(n.min(registry.components().len()));
    let map = world.resource::<NetEntities>().clone();
    let asset_cx = world
        .get_resource::<crate::assets::NetAssets>()
        .and_then(|a| a.context());
    let max_refs = world
        .get_resource::<crate::settings::NetSettings>()
        .map_or(4096, |s| s.replication.max_pending_refs as usize);
    for _ in 0..n {
        let ci = u16::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("component"))?;
        let kind = r.read_bits(2)?;
        let len = r.read_len(r.remaining_bits())?;
        let start = r.bit_pos();
        let Some(c) = registry.get(ci) else {
            skip_bits(r, len)?;
            continue;
        };
        applied.push(ci);
        match kind {
            ENTRY_REMOVED => (c.remove)(world, e),
            ENTRY_FULL | ENTRY_DELTA => {
                let cx = CodecContext::new(&map).with_assets(asset_cx.as_deref());
                let outcome = (c.apply)(world, e, r, &cx, kind == ENTRY_FULL, spawn);
                record_asset_misses(world, &cx, c.name, tick);
                let used = r.bit_pos() - start;
                match outcome {
                    Ok(o) => {
                        if used != len {
                            return Err(DecodeError::Invalid("component entry length"));
                        }
                        record_refs(world, e, ci, o, tick, max_refs);
                    }
                    Err(_) => {
                        // An entry that read past its own length leaves the stream misaligned.
                        if used > len {
                            return Err(DecodeError::Invalid("component entry length"));
                        }
                        // A delta for a component the copy lost: skip it and ask for full state.
                        let rest = len - used;
                        skip_bits(r, rest)?;
                        if let Some(id) = world.get::<NetId>(e).copied() {
                            world.resource_mut::<ClientReplication>().resync.insert(id);
                        }
                    }
                }
            }
            _ => return Err(DecodeError::Invalid("entry kind")),
        }
    }
    Ok(applied)
}

/// Client: handle fields that decoded unset are listed in `NetAssets` and logged once per key
/// (`net.asset_missing`, spec 71 § 3).
fn record_asset_misses(world: &mut World, cx: &CodecContext, component: &str, tick: NetTick) {
    let misses = cx.take_asset_misses();
    if misses.is_empty() {
        return;
    }
    world
        .get_resource_or_insert_with(crate::assets::NetAssets::default)
        .record(misses, component, tick);
}

fn record_refs(world: &mut World, e: Entity, ci: u16, outcome: ApplyOutcome, tick: NetTick, max_refs: usize) {
    let mut client = world.resource_mut::<ClientReplication>();
    let client = &mut *client;
    if let Some(old) = client.refs_of.remove(&(e, ci)) {
        for id in old {
            if let Some(list) = client.referrers.get_mut(&id) {
                list.retain(|r| *r != (e, ci));
            }
        }
    }
    if outcome.unresolved.is_empty() && outcome.referenced.is_empty() && client.pending_refs.is_empty() {
        return;
    }
    for id in &outcome.referenced {
        let list = client.referrers.entry(*id).or_default();
        if !list.contains(&(e, ci)) {
            list.push((e, ci));
        }
    }
    if !outcome.referenced.is_empty() {
        client.refs_of.insert((e, ci), outcome.referenced.clone());
    }
    // The field changed again: its old pending references end. A field that was waiting and now
    // decodes with every target known was patched.
    let had_pending = client.pending_keys.remove(&(e, ci)).is_some();
    if had_pending {
        client
            .pending_refs
            .retain(|p| !(p.entity == e && p.component == ci));
    }
    if had_pending && outcome.unresolved.is_empty() {
        client.resolved_now.push((e, ci));
    }
    for id in outcome.unresolved {
        *client.pending_keys.entry((e, ci)).or_insert(0) += 1;
        client.pending_refs.push_back(PendingRef {
            entity: e,
            component: ci,
            waiting_for: id,
            since: tick,
        });
    }
    while client.pending_refs.len() > max_refs {
        if let Some(old) = client.pending_refs.pop_front() {
            let key = (old.entity, old.component);
            if let Some(n) = client.pending_keys.get_mut(&key) {
                *n -= 1;
                if *n == 0 {
                    client.pending_keys.remove(&key);
                }
            }
        }
        client.counters.pending_refs_dropped += 1;
    }
}

/// The client-side detection of copies its own gameplay despawned (spec 71 § 4.2): warns once per
/// copy and asks the server for full state.
pub(crate) fn client_check_lost_copies(world: &mut World) {
    let lost: Vec<NetId> = world
        .resource::<NetEntities>()
        .iter()
        .filter(|(_, e)| world.get_entity(*e).is_err())
        .map(|(id, _)| id)
        .collect();
    if lost.is_empty() {
        return;
    }
    let mut despawned = Vec::with_capacity(lost.len());
    for id in lost {
        world.resource_mut::<NetEntities>().remove(id);
        // A streamed level entity whose cell unloaded: its bind waits for the cell again (spec 78
        // § 14), with no warning and no request.
        let key = world
            .resource::<ClientReplication>()
            .streamed_copies
            .get(&id)
            .copied();
        if let Some((level, guid)) = key {
            let streamed = is_streamed(world, level, guid);
            let mut client = world.resource_mut::<ClientReplication>();
            client.streamed_copies.remove(&id);
            if streamed {
                client.buffer_level_bind(level, guid, id);
                continue;
            }
        }
        tracing::warn!(target: "net", net_id = id.0, "net.client_despawned_proxy");
        despawned.push(id);
    }
    let mut client = world.resource_mut::<ClientReplication>();
    client.counters.client_despawned_proxy += despawned.len() as u64;
    client.resync.extend(despawned);
}

/// Client: ends kept level binds and removed-entity notes of levels no longer streamed (a closed
/// level). Runs when the `StreamedReplicated` table changed.
fn prune_level_binds(world: &mut World) {
    let revision = world
        .get_resource::<StreamedReplicated>()
        .map(StreamedReplicated::revision);
    if world.resource::<ClientReplication>().streamed_revision == revision {
        return;
    }
    let table = world
        .get_resource::<StreamedReplicated>()
        .cloned()
        .unwrap_or_default();
    let mut client = world.resource_mut::<ClientReplication>();
    let client = &mut *client;
    client.streamed_revision = revision;
    let listed = |key: &(u128, u128)| table.cell_of(key.0, key.1).is_some();
    let gone: Vec<NetId> = client
        .level_binds
        .iter()
        .filter(|(key, _)| !listed(key))
        .map(|(_, id)| *id)
        .collect();
    for id in gone {
        if client.take_level_bind(id).is_some() {
            client.counters.level_entity_buffer_cleared += 1;
        }
    }
    client
        .level_removed_streamed
        .retain(|key| table.cell_of(key.0, key.1).is_some());
    client.streamed_copies.retain(|_, key| listed(key));
}

/// Client: inserts `NetUnbound` and `NetRole::Proxy` on level-placed replicated entities that are
/// not bound yet.
pub(crate) fn client_mark_unbound(world: &mut World) {
    let mut q = world.query_filtered::<Entity, (
        With<Replicated>,
        With<LevelMember>,
        Without<NetId>,
        Without<NetUnbound>,
    )>();
    let es: Vec<Entity> = q.iter(world).collect();
    prune_level_binds(world);
    let waiting = {
        let client = world.resource::<ClientReplication>();
        !client.level_binds.is_empty() || !client.level_removed_streamed.is_empty()
    };
    for e in es {
        // A streamed cell just spawned it: a kept bind asks for its full state now, and an entity
        // the server removed goes again (spec 71 § 4.5).
        if waiting
            && let (Some(level), Some(guid)) = (world.get::<LevelMember>(e), world.get::<EntityGuid>(e))
        {
            let key = (level.0, guid.0.as_u128());
            if world
                .resource::<ClientReplication>()
                .level_removed_streamed
                .contains(&key)
            {
                let _ = world.despawn(e);
                continue;
            }
            let mut client = world.resource_mut::<ClientReplication>();
            if let Some(id) = client.level_binds.get(&key).copied() {
                client.take_level_bind(id);
                client.counters.level_entity_buffer_applied += 1;
                client.resync.insert(id);
            }
        }
        if let Ok(mut ent) = world.get_entity_mut(e) {
            ent.insert((NetUnbound, NetRole::Proxy));
        }
    }
}

/// Client (spec 73 § 7): binds a fresh server copy to the local entity of a predicted spawn. The
/// copy's replicated components, transform, id and role move onto `local`; the copy is despawned
/// and this tick's spawn notice names `local`. False when either entity is gone.
pub(crate) fn rebind_copy(world: &mut World, copy: Entity, local: Entity) -> bool {
    if world.get_entity(copy).is_err() || world.get_entity(local).is_err() {
        return false;
    }
    let Some(id) = world.get::<NetId>(copy).copied() else {
        return false;
    };
    let registry = world.resource::<ReplicationRegistry>().clone();
    let map = world.resource::<NetEntities>().clone();
    let asset_cx = world
        .get_resource::<crate::assets::NetAssets>()
        .and_then(|a| a.context());
    let cx = CodecContext::new(&map).with_assets(asset_cx.as_deref());
    for c in registry.components() {
        let Some(v) = (c.snapshot)(world, copy) else {
            continue;
        };
        let mut w = BitWriter::new();
        (c.encode)(&v, &mut w, &cx);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        if (c.apply)(world, local, &mut r, &cx, true, true).is_err() {
            tracing::warn!(target: "net", component = c.name, "net: predicted spawn bind could not copy a component");
        }
    }
    let transform = world.get::<Transform>(copy).copied();
    let mode = world.get::<NetTransformMode>(copy).copied();
    let velocity = world.get::<NetVelocity>(copy).copied();
    let role = world.get::<NetRole>(copy).copied();
    let owned = world.get::<NetOwnedLocally>(copy).is_some();
    let parent = world.get::<ChildOf>(copy).map(|p| p.0);
    if let Ok(mut ent) = world.get_entity_mut(local) {
        ent.insert(id);
        if let Some(t) = transform {
            ent.insert(t);
        }
        if let Some(m) = mode {
            ent.insert(m);
        }
        if let Some(v) = velocity {
            ent.insert(v);
        }
        if let Some(r) = role {
            ent.insert(r);
        }
        if owned {
            ent.insert(NetOwnedLocally);
        } else {
            ent.remove::<NetOwnedLocally>();
        }
        if let Some(p) = parent {
            ent.insert(ChildOf(p));
        }
    }
    world.resource_mut::<NetEntities>().insert(id, local);
    for s in world.resource_mut::<NetEvents>().spawned.iter_mut() {
        if s.entity == copy {
            s.entity = local;
        }
    }
    let _ = world.despawn(copy);
    true
}

/// Average gained priority of the records packed on one send tick (ADR-1662).
pub(crate) fn send_share(gained: f32, picked: usize) -> f32 {
    if picked == 0 {
        0.0
    } else {
        gained.max(0.0) / picked as f32
    }
}

/// Accumulator after a send: the entity gives up `share`, and the debt is held to one share.
pub(crate) fn accumulator_after_send(acc: f32, share: f32) -> f32 {
    (acc - share).max(-share)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::bits::{BitReader, BitWriter};
    use crate::codec::{CodecContext, ReplicateCodec};

    thread_local! {
        static TRACK_CLONES: Cell<u64> = const { Cell::new(0) };
    }

    /// rev-claude-55 finding 1 (ADR-7495): a host sets the state channel's entity cap before it
    /// connects; the session start (`ClientEvent::Connected`) and the replay reset keep it, as they
    /// keep `level_bind_cap`.
    #[test]
    fn a_host_set_entity_cap_survives_session_start_and_the_replay_reset() {
        let mut world = World::new();
        world.init_resource::<ClientReplication>();
        world.resource_mut::<ClientReplication>().state.entity_cap = Some(4096);
        client_session_started(&mut world);
        assert_eq!(
            world.resource::<ClientReplication>().state.entity_cap,
            Some(4096),
            "session start keeps the cap"
        );
        world.resource_mut::<ClientReplication>().forget_everything();
        assert_eq!(
            world.resource::<ClientReplication>().state.entity_cap,
            Some(4096),
            "the replay reset keeps the cap"
        );
        // The default stays the default.
        let mut world = World::new();
        world.init_resource::<ClientReplication>();
        client_session_started(&mut world);
        assert_eq!(world.resource::<ClientReplication>().state.entity_cap, None);
    }

    #[derive(Debug, PartialEq)]
    struct Track(u32);

    impl Clone for Track {
        fn clone(&self) -> Self {
            TRACK_CLONES.with(|c| c.set(c.get() + 1));
            Track(self.0)
        }
    }

    impl ReplicateCodec for Track {
        fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
            self.0.encode(w, cx);
        }

        fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, crate::bits::DecodeError> {
            Ok(Track(u32::decode(r, cx)?))
        }
    }

    /// An unchanged element of a delta is not cloned.
    #[test]
    fn netvec_delta_does_not_clone_unchanged_elements() {
        let mut base = NetVec::new();
        for i in 0..64u32 {
            base.push(Track(i)).expect("room");
        }
        let mut next = base.clone();
        let id = next.ids()[3];
        next.get_mut(id).expect("element").0 = 99;
        TRACK_CLONES.with(|c| c.set(0));
        let mut w = BitWriter::new();
        let cx = CodecContext::detached();
        next.encode_delta(&base, &mut w, &cx);
        let decoded = NetVec::decode_delta(&base, &mut BitReader::new(w.as_bytes()), &cx).expect("delta");
        assert_eq!(decoded, next);
        let clones = TRACK_CLONES.with(|c| c.get());
        assert_eq!(clones, 0, "unchanged NetVec elements cloned: {clones}");
    }

    /// Priorities 10 and 1, one record per send tick. B is packed on send tick 6.
    /// A is -1 after send tick 1 (ADR-1662 share, not a reset to 0).
    #[test]
    fn share_rule_sends_the_quiet_entity_on_tick_six() {
        let mut a = 0.0f32;
        let mut b = 0.0f32;
        let mut sent_b = None;
        for tick in 1..=6 {
            a += 10.0;
            b += 1.0;
            let send_a = a >= b;
            let share = send_share(11.0, 1);
            if send_a {
                a = accumulator_after_send(a, share);
            } else {
                b = accumulator_after_send(b, share);
                sent_b = Some(tick);
            }
            if tick == 1 {
                assert_eq!(a, -1.0);
                assert_eq!(b, 1.0);
            }
        }
        assert_eq!(sent_b, Some(6));
        assert_eq!(a, 5.0);
        assert_eq!(b, -5.0);
    }

    /// `replication.netvec_max` is reserved: push stops at [`NETVEC_MAX`], not at the setting.
    #[test]
    fn netvec_max_setting_does_not_change_the_push_cap() {
        let settings = crate::settings::NetSettings {
            replication: crate::settings::ReplicationSettings {
                netvec_max: 8,
                ..crate::settings::ReplicationSettings::default()
            },
            ..crate::settings::NetSettings::default()
        };
        assert_eq!(settings.replication.netvec_max, 8);
        let mut list = NetVec::new();
        for i in 0..9u32 {
            assert!(list.push(i).is_ok(), "the 9th push still fits");
        }
        assert_eq!(list.len(), 9);
        while list.len() < NETVEC_MAX {
            list.push(0).expect("under the wire cap");
        }
        assert_eq!(list.push(0), Err(NetVecFull(NETVEC_MAX)));
    }

    /// Ids stop at `u32::MAX` instead of colliding there.
    #[test]
    fn netvec_ids_stop_instead_of_colliding_at_the_end_of_the_counter() {
        let mut list = NetVec::<u32> {
            items: Vec::new(),
            next_id: u32::MAX,
        };
        assert_eq!(list.push(7), Ok(u32::MAX));
        assert_eq!(list.push(8), Err(NetVecFull(NETVEC_MAX)));
        assert_eq!(list.len(), 1);
        assert_eq!(list.get(u32::MAX), Some(&7));
        assert_eq!(list.remove(u32::MAX), Some(7));
        assert_eq!(list.push(9), Err(NetVecFull(NETVEC_MAX)), "an id is never reused");
    }

    /// A child of a parent that is not replicated is not sent as a root before its global
    /// transform exists; with it, the world pose goes out.
    #[test]
    fn an_unpropagated_child_of_an_unreplicated_parent_sends_no_transform() {
        let mut world = World::new();
        let parent = world.spawn_empty().id();
        let child = world
            .spawn((
                ChildOf(parent),
                Transform::from_translation(Vec3::new(0.3, 0.0, 0.0)),
            ))
            .id();
        // `Transform` requires `GlobalTransform`, so only a removed one reaches the fallback.
        world.entity_mut(child).remove::<GlobalTransform>();
        assert!(server_transform(&world, child, false, None).is_none());
        world
            .entity_mut(child)
            .insert(GlobalTransform(glam::Affine3A::from_translation(Vec3::new(
                10.3, 0.0, 5.0,
            ))));
        let t = server_transform(&world, child, false, None).expect("transform");
        assert!((t.translation - Vec3::new(10.3, 0.0, 5.0)).length() < 1e-5);
        // A replicated parent keeps the relative transform with no global needed.
        let bare = world
            .spawn((
                ChildOf(parent),
                Transform::from_translation(Vec3::new(0.3, 0.0, 0.0)),
            ))
            .id();
        assert!(server_transform(&world, bare, true, None).is_some());
    }

    fn ranked(keys: &[u64]) -> Vec<Ranked> {
        keys.iter()
            .enumerate()
            .map(|(i, key)| Ranked {
                key: *key,
                slot: i as u32,
                depth: 0,
                gain: 0.0,
                spawn: false,
            })
            .collect()
    }

    fn copy_in_flight(predicted: bool, link: &mut StateLink) -> ConnEntity {
        let version = Arc::new(StateVersion {
            tick: NetTick(9),
            number: 3,
            parent: None,
            transform: None,
            comps: Arc::new(Vec::new()),
        });
        let (_, placed) = link.assemble(NetTick(10), Vec::new(), 1200, &mut Vec::new());
        assert!(placed.is_empty());
        ConnEntity {
            id: NetId(1),
            slot: 0,
            base: Some(version),
            base_number: 3,
            base_tick: NetTick(9),
            unmatched: Vec::new(),
            owned: true,
            predicted,
            last_sent: NetTick(10),
            out_of_scope: false,
            full_again: false,
            teleport_due: false,
            resend_due: false,
            born: NetTick(1),
            teleport_tick: NetTick(0),
            resend_tick: NetTick(0),
            spawn_acked: true,
            sent_seq: 0,
            sent_version: 4,
            flight: Vec::new(),
            gated: false,
            base_far: false,
        }
    }

    /// A version on its way is not offered again, unless the client predicts the copy: it compares
    /// every frame with what it ran, so each one has to hold the state.
    #[test]
    fn a_predicted_copy_stays_offered_until_acknowledged() {
        let peer = PeerId(7);
        for (predicted, offered) in [(false, 0), (true, 1)] {
            let mut link = StateLink::default();
            let mut ce = copy_in_flight(predicted, &mut link);
            let mut spawns = Vec::new();
            let mut scope_out = Vec::new();
            let mut updates = Vec::new();
            let mut own_changes = Vec::new();
            let mut lists = WalkLists {
                spawns: &mut spawns,
                scope_out: &mut scope_out,
                updates: &mut updates,
                own_changes: &mut own_changes,
            };
            let hot = TickHot {
                id: NetId(1),
                owner: peer,
                number: 4,
                profile: 0,
                flags: HOT_LIVE | if predicted { HOT_PREDICTED } else { 0 },
                priority_scale: 1.0,
                heat: (0.0, 0.0),
            };
            visit_acked(hot, &mut ce, peer, &link, NetTick(11), &mut lists);
            assert_eq!(updates.len(), offered, "predicted {predicted}");
        }
    }

    #[test]
    fn the_largest_transform_fits_inline() {
        let t = NetTransform {
            translation: Vec3::new(1.5, -2.5, 1e6),
            rotation: Quat::from_rotation_y(1.0),
            scale: Vec3::new(2.0, 3.0, 4.0),
            velocity: Some(Vec3::new(1.0, 2.0, 3.0)),
        };
        // Raw position (no grid) and a raw velocity past the velocity grid: the widest record.
        let t = NetTransform {
            velocity: Some(Vec3::new(100.0, 2.0, 3.0)),
            ..t
        };
        let q = QTransform::from_net(TransformReplication::Full, &t, None);
        let mut b = BitWriter::new();
        q.write_abs(&mut b);
        assert!(b.as_bytes().len() <= TRANSFORM_BYTES, "{}", b.as_bytes().len());
        let kept = TransformBits::from_net(TransformReplication::Full, &t, None);
        let mut again = BitWriter::new();
        again.append_slice(&kept.bytes, usize::from(kept.len));
        assert_eq!(again.as_bytes(), b.as_bytes());
        // The grid's widest: a far cell and a non-default step.
        let grid = crate::profile::PositionQuantizer::new(1e-5);
        let far = NetTransform {
            translation: Vec3::new(-21_000.0, 21_000.0, -21_000.0),
            ..t
        };
        let wide = TransformBits::from_net(TransformReplication::Full, &far, grid);
        assert!(
            usize::from(wide.len).div_ceil(8) <= TRANSFORM_BYTES,
            "{}",
            wide.len
        );
    }

    #[test]
    fn windows_give_the_order_of_a_full_sort() {
        let mut next = 1u64;
        let keys: Vec<u64> = (0..997)
            .map(|_| {
                next = next.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(11);
                next >> 40
            })
            .collect();
        let mut sorted = keys.clone();
        sorted.sort_unstable();
        for window in [1, 7, 150, 996, 997, 2000] {
            let mut r = ranked(&keys);
            let (mut at, mut ordered) = (0, order_window(&mut r, 0, window));
            let mut seen = Vec::new();
            while at < r.len() {
                if at == ordered {
                    ordered = order_window(&mut r, at, window);
                }
                seen.push(r[at].key);
                at += 1;
            }
            assert_eq!(seen, sorted, "window {window}");
        }
        assert_eq!(order_window(&mut [], 0, 4), 0);
    }

    /// A root whose profile was resolved on the first gather snaps with that quantizer on the next
    /// changed send tick without probing [`NetProfiles::get`] again. A later profile edit still
    /// snaps on the new grid.
    #[test]
    fn a_resolved_root_reuses_its_position_quantizer() {
        use crate::profile::{NetProfile, NetProfiles, PositionQuantizer, profile_gets, reset_profile_gets};

        let mut world = World::new();
        let mut profiles = NetProfiles::default();
        for (name, step) in [("coarse", 0.5_f32), ("fine", 0.01)] {
            profiles
                .insert(
                    name,
                    NetProfile {
                        position_step_m: Some(step),
                        ..NetProfile::default()
                    },
                )
                .expect("profile");
        }
        world.insert_resource(profiles);
        let p = Vec3::new(0.123_456, 7.777_7, -3.271_9);
        let e = world
            .spawn((
                Replicated {
                    profile: Some("coarse".into()),
                    transform: TransformReplication::PositionOnly,
                    ..Replicated::default()
                },
                Transform::from_translation(p),
            ))
            .id();
        reset_profile_gets();
        server_gather(&mut world, NetTick(1));
        let first = profile_gets();
        assert!(first >= 1, "the first gather resolves the profile ({first} gets)");

        world.get_mut::<Transform>(e).expect("transform").translation.x = 0.3;
        reset_profile_gets();
        server_gather(&mut world, NetTick(2));
        let id = world.get::<NetId>(e).copied().expect("id");
        let moved = Vec3::new(0.3, 7.777_7, -3.271_9);
        let coarse = PositionQuantizer::new(0.5).expect("q").snap(moved);
        let got = world
            .resource::<ServerReplication>()
            .entities
            .get(id)
            .and_then(|s| s.tcache)
            .expect("cached transform");
        let expect = TransformBits::from_net(
            TransformReplication::PositionOnly,
            &NetTransform {
                translation: coarse,
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
                velocity: None,
            },
            PositionQuantizer::new(0.5),
        );
        assert_eq!(got, expect, "second tick snaps on the coarse grid");
        let second = profile_gets();
        assert_eq!(
            second, 0,
            "a resolved root does not call NetProfiles::get again ({second})"
        );

        world.get_mut::<Replicated>(e).expect("replicated").profile = Some("fine".into());
        reset_profile_gets();
        server_gather(&mut world, NetTick(3));
        let fine = PositionQuantizer::new(0.01).expect("q").snap(moved);
        let got = world
            .resource::<ServerReplication>()
            .entities
            .get(id)
            .and_then(|s| s.tcache)
            .expect("cached transform");
        let expect = TransformBits::from_net(
            TransformReplication::PositionOnly,
            &NetTransform {
                translation: fine,
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
                velocity: None,
            },
            PositionQuantizer::new(0.01),
        );
        assert_eq!(got, expect, "a profile edit snaps on the new grid");
        assert!(profile_gets() >= 1, "a changed profile is resolved again");

        // Removing the resource must not keep the old grid. Generation 0 is a real profile set,
        // so absence is not stored as 0.
        world.remove_resource::<NetProfiles>();
        let raw = Vec3::new(1.006, 7.777_7, -3.271_9);
        world.get_mut::<Transform>(e).expect("transform").translation = raw;
        server_gather(&mut world, NetTick(4));
        let got = world
            .resource::<ServerReplication>()
            .entities
            .get(id)
            .and_then(|s| s.tcache)
            .expect("cached transform");
        let expect = TransformBits::from_net(
            TransformReplication::PositionOnly,
            &NetTransform {
                translation: raw,
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
                velocity: None,
            },
            None,
        );
        assert_eq!(got, expect, "without NetProfiles the position is not snapped");
    }

    /// Spec 71 § 4.1 step 2: when the `NetId` counter is exhausted, the entity stays local (no
    /// `NetId`, no role) and `net.netid_exhausted` is logged once, not once per entity or tick.
    #[test]
    fn an_exhausted_net_id_counter_leaves_entities_local_and_logs_once() {
        let mut world = World::new();
        world.init_resource::<NetEntities>();
        let mut rep = ServerReplication {
            next_net_id: u32::MAX - 1,
            ..ServerReplication::default()
        };
        let first = world.spawn(Replicated::default()).id();
        let second = world.spawn(Replicated::default()).id();
        assign_new(&mut world, &mut rep, NetTick(1));
        // Ids follow `Entity::to_bits` order (spec 71 § 12, ADR-5648): one of the two gets the last
        // id, the other stays local.
        let (got, local) = if world.get::<NetId>(first).is_some() {
            (first, second)
        } else {
            (second, first)
        };
        assert_eq!(world.get::<NetId>(got), Some(&NetId(u32::MAX - 1)));
        assert!(world.get::<NetId>(local).is_none(), "the counter ran out");
        assert!(world.get::<NetRole>(local).is_none(), "the entity stays local");
        assert!(rep.exhausted_logged);
        let third = world.spawn(Replicated::default()).id();
        assign_new(&mut world, &mut rep, NetTick(2));
        assert!(world.get::<NetId>(local).is_none());
        assert!(world.get::<NetId>(third).is_none());
        assert_eq!(rep.next_net_id, u32::MAX, "no id is ever handed out twice");
    }

    /// A component whose encoding is as long as its payload: 17 KiB is over the 16 KiB entity
    /// state cap (spec 72 § 7).
    #[derive(Component, Clone, Debug, PartialEq, Default)]
    struct Blob(Vec<u8>);

    impl ReplicateCodec for Blob {
        fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
            w.write_varint(self.0.len() as u64);
            for b in &self.0 {
                w.write_bits(u64::from(*b), 8);
            }
        }

        fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, crate::bits::DecodeError> {
            let n = r.read_len(1 << 20)?;
            let mut v = Vec::with_capacity(n);
            for _ in 0..n {
                v.push(r.read_bits(8)? as u8);
            }
            Ok(Self(v))
        }
    }

    /// The packing pipeline (where the 16 KiB cap is checked) runs with `NetInterest` installed,
    /// as in the spec 72 conformance tests.
    fn setup_blob(world: &mut World) {
        register_replicated::<Blob>(world, ReplicateOptions::default());
        if world.resource::<crate::model::NetMode>().is_server() {
            crate::relevance::install_net_interest(world);
            let profile = NetProfile {
                relevance: RelevanceShape::World,
                linger_ticks: 0,
                ..NetProfile::default()
            };
            let inserted = world
                .resource_mut::<crate::profile::NetProfiles>()
                .insert("everywhere", profile);
            assert!(inserted.is_ok(), "profile");
        }
    }

    fn warn_sets(hub: &crate::testing::LoopbackHub) -> (usize, usize, ReplicationCounters) {
        let rep = hub.server.world.resource::<ServerReplication>();
        (rep.warned_parent.len(), rep.warned_large.len(), rep.counters)
    }

    /// sprint-deep-zai-replication 1 and 2: the warn-once sets (`warned_parent`, `warned_large`)
    /// drop a despawned entity, so a long session that churns warned entities does not grow
    /// them. The warning still fires once per live entity (not once per tick), and again for a
    /// new entity spawned after the despawns.
    #[test]
    fn warn_once_sets_are_pruned_on_despawn() {
        const N: usize = 6;
        let mut hub =
            crate::testing::LoopbackHub::new(crate::model::NetMode::DedicatedServer, 8770, setup_blob);
        let _client = hub.add_client();
        let rep = || {
            (
                Replicated {
                    transform: TransformReplication::None,
                    ..Replicated::default()
                },
                crate::profile::NetProfileRef("everywhere".into()),
            )
        };
        let local_parent = hub.server.world.spawn_empty().id();
        let mut spawned = Vec::new();
        for i in 0..N {
            spawned.push(
                hub.server
                    .world
                    .spawn((rep(), Blob(vec![i as u8]), ChildOf(local_parent)))
                    .id(),
            );
            spawned.push(hub.server.world.spawn((rep(), Blob(vec![7; 17 * 1024]))).id());
        }
        hub.run(10);
        let (parents, large, counters) = warn_sets(&hub);
        assert_eq!((parents, large), (N, N), "one entry per live warned entity");
        assert_eq!(counters.parent_not_replicated, N as u64);
        assert_eq!(counters.state_too_large, N as u64);
        hub.run(3);
        let (_, _, counters) = warn_sets(&hub);
        assert_eq!(
            counters.parent_not_replicated, N as u64,
            "warned once, not per tick"
        );
        assert_eq!(counters.state_too_large, N as u64, "warned once, not per tick");

        for e in spawned {
            hub.server.world.despawn(e);
        }
        hub.run(2);
        let (parents, large, _) = warn_sets(&hub);
        assert_eq!(
            (parents, large),
            (0, 0),
            "despawned entities leave the warn-once sets"
        );

        // A new entity reuses nothing: it is warned about again.
        hub.server
            .world
            .spawn((rep(), Blob(vec![1]), ChildOf(local_parent)));
        hub.server.world.spawn((rep(), Blob(vec![7; 17 * 1024])));
        hub.run(10);
        let (parents, large, counters) = warn_sets(&hub);
        assert_eq!((parents, large), (1, 1));
        assert_eq!(counters.parent_not_replicated, N as u64 + 1);
        assert_eq!(counters.state_too_large, N as u64 + 1);
    }

    /// ADR-8850: a connection whose relevant set reaches 90 % of `MAX_CLIENT_ENTITIES` is warned
    /// about once — past the cap the client refuses the next spawn and the session ends
    /// (ADR-8760), so the server's copy count is the only early signal. The counter goes from 0
    /// to 1 exactly once per connection, never once per gather.
    #[test]
    fn a_relevant_set_near_the_cap_warns_once_per_connection() {
        let mut rep = ServerReplication::default();
        rep.add_conn(PeerId(1));
        rep.add_conn(PeerId(2));
        assert_eq!(rep.counters.relevant_near_cap, 0);
        // "Reaches 90 %" reads exactly: 65536 * 9 / 10 is 58982.4, so 58983 is the first warn.
        let near = (MAX_CLIENT_ENTITIES * 9).div_ceil(10);
        rep.note_relevant_set(PeerId(1), near - 1);
        assert_eq!(
            rep.counters.relevant_near_cap, 0,
            "below the threshold nothing warns"
        );
        rep.note_relevant_set(PeerId(1), near);
        assert_eq!(rep.counters.relevant_near_cap, 1, "crossing it warns once");
        rep.note_relevant_set(PeerId(1), MAX_CLIENT_ENTITIES);
        assert_eq!(
            rep.counters.relevant_near_cap, 1,
            "not again for the same connection"
        );
        rep.note_relevant_set(PeerId(2), MAX_CLIENT_ENTITIES);
        assert_eq!(rep.counters.relevant_near_cap, 2, "but once per connection");
    }

    /// ADR-10215: a received record without a velocity (a far record, a `PositionOnly` record)
    /// removes a proxy's old replicated `NetVelocity`, a record with one writes it again, and a
    /// copy this client owns or predicts keeps the velocity its game or prediction wrote.
    #[test]
    fn a_velocity_less_record_clears_only_a_proxys_velocity() {
        let mut world = World::new();
        let with = |v: Option<Vec3>| NetTransform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
            velocity: v,
        };
        let old = NetVelocity(Vec3::new(1.3125, 0.0, 0.15625));
        let proxy = world.spawn(old).id();
        let owned = world.spawn((old, NetOwnedLocally)).id();
        let predicted = world.spawn((old, NetPredicted)).id();
        let bare = world.spawn_empty().id();
        for e in [proxy, owned, predicted, bare] {
            write_transform(&mut world, e, TransformReplication::Movement, &with(None), false);
        }
        assert_eq!(
            world.get::<NetVelocity>(proxy),
            None,
            "the proxy's old velocity goes"
        );
        assert_eq!(
            world.get::<NetVelocity>(owned),
            Some(&old),
            "an owned copy keeps its own"
        );
        assert_eq!(
            world.get::<NetVelocity>(predicted),
            Some(&old),
            "a predicted copy keeps its own"
        );
        assert_eq!(
            world.get::<NetVelocity>(bare),
            None,
            "nothing to clear, nothing added"
        );
        assert_eq!(
            world.get::<Transform>(proxy).map(|t| t.translation),
            Some(Vec3::new(1.0, 2.0, 3.0)),
            "the pose is still written"
        );
        // An exact record with a velocity writes it again, on every copy.
        let now = Vec3::new(-0.5, 0.0, 1.25);
        for e in [proxy, owned, predicted, bare] {
            write_transform(
                &mut world,
                e,
                TransformReplication::Movement,
                &with(Some(now)),
                false,
            );
            assert_eq!(world.get::<NetVelocity>(e), Some(&NetVelocity(now)));
        }
        // A `PositionOnly` record carries no velocity: the proxy's goes again.
        write_transform(
            &mut world,
            proxy,
            TransformReplication::PositionOnly,
            &with(None),
            false,
        );
        assert_eq!(world.get::<NetVelocity>(proxy), None);
    }
}
