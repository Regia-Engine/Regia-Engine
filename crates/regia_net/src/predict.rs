// SPDX-License-Identifier: MPL-2.0
//! Client prediction and server reconciliation (spec 73): the input stream, the client's lead,
//! the server's input buffer, prediction history, rollback and resimulation.
//!
//! | Piece | What |
//! |---|---|
//! | [`NetInput`] / [`InputCommand`] / [`add_net_input`] | the game's per-tick input type and where it lives |
//! | input stream | `INPUT` frames (client to server, unreliable): every unacknowledged command, newest first |
//! | input acknowledgment | `IACK` frames (server to client, unreliable): newest input tick, buffer margin, repeated ticks |
//! | [`NetClock`] | the client tick, lead, round trip and jitter |
//! | [`predict`] / [`Tolerance`] / [`EntityTolerance`] | predicted components and what counts as a mismatch |
//! | [`PredictedUpdate`] / [`add_predicted_systems`] | the predicted schedule, run once per tick and again while resimulating |
//! | [`PredictionCx`] | the system parameter of predicted systems: tick, resimulation, first-time effects, predicted spawns |
//! | [`NetVisualOffset`] / [`NetVisualChild`] | correction smoothing |
//! | [`TickDrivenMotion`] | movers that are a pure function of the tick, so riders predict exactly |
//!
//! The loop (§ 1): the client stamps each fixed step's input with its client tick and sends it, runs
//! the predicted schedule for that tick and keeps the result; the server consumes exactly one command
//! per player per tick on its own clock and sends back its state for the tick; when the client's
//! remembered result for that tick differs, it rewinds every predicted entity to the server's state
//! and re-runs the ticks since with the inputs it remembered. Offline and on a listen-server host the
//! local command is consumed directly: no stream, no lead, no rollback.
//!
//! Deterministic: nothing here reads wall-clock time, frame `dt` or a random source. Dilation of
//! the client's tick rate is done by inserting or skipping a predicted tick at fixed steps (§ 5.1),
//! so the gameplay `dt` stays the nominal step.

use std::any::{Any, TypeId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Arc;

use glam::{Quat, Vec3};
use regia_app::App;
use regia_ecs::ecs::component::ComponentId;
use regia_ecs::ecs::system::{ScheduleSystem, SystemParam};
use regia_ecs::prelude::*;
use regia_scene::transform::Transform;

use crate::bits::{BitReader, BitWriter, DecodeError};
use crate::codec::{CodecContext, LayoutHasher, ReplicateCodec};
use crate::interp::{NetInterpolation, ViewTickWire, set_view_tick};
use crate::model::{
    LocalPeer, NetId, NetMode, NetOwnedLocally, NetOwner, NetOwnerLink, NetPlayer, NetPlayerId, NetRole,
    NetTick, NetTickFrac, PeerId,
};
use crate::replication::{NetEvents, NetPredicted, ReplicateOptions, register_replicated};
use crate::runtime::NetNode;
use crate::settings::{NetSettings, PredictionSettings};
use crate::stable_math::{
    angle_chord2, quat_mul, quat_nlerp, quat_normalize, quat_rot_chord2, quat_rotate_vec3, stable_pow2,
};

/// Spec 120 § 6.1: the input packet limit's bucket holds this many ticks of
/// `max_input_packets_per_tick` (a burst a stalled link flushes at once).
pub const INPUT_BURST_TICKS: u32 = 16;

/// Frame tag of the client's input stream.
pub(crate) const TAG_INPUT: u8 = 5;
/// Frame tag of the server's input acknowledgment.
pub(crate) const TAG_INPUT_ACK: u8 = 6;

/// Commands the server buffers ahead of its current tick (§ 5.2).
pub const INPUT_BUFFER_AHEAD: u32 = 64;
/// Ticks of input history the client keeps (§ 4).
pub const INPUT_HISTORY_TICKS: u32 = 128;
/// Commands whose arrival margins the server averages (§ 4).
pub const MARGIN_WINDOW: usize = 30;
/// Reports with a negative margin in a row that force a lead resync (§ 5.1).
pub const NEGATIVE_MARGIN_RESYNC: u32 = 10;
/// Jitter that raises the margin target by one tick, in milliseconds (§ 5.1).
pub const JITTER_MS_PER_MARGIN_TICK: f64 = 4.0;
/// Ticks over which the client counts corrections for `corrections_per_s`.
const RATE_WINDOW_TICKS: u64 = 60;
/// Most ticks of prediction history a project can keep (`prediction.history_ticks` is clamped to
/// it; spec 120 § 9.4). A rollback deeper than the history is not compared but still re-runs from
/// the client's input history; only one deeper than [`INPUT_HISTORY_TICKS`] snaps.
pub const MAX_HISTORY_TICKS: u32 = 128;
/// Fixed steps after the first server state that the prediction clock waits for the transport's
/// first round-trip sample before it starts with its fallback lead (spec 120 § 9.2, ADR-8219).
pub const START_WAIT_FOR_RTT_STEPS: u64 = 120;

fn default_step_ms() -> f64 {
    1000.0 / crate::send::DEFAULT_TICK_HZ
}

/// Milliseconds of one predicted tick in `world`: its fixed step (the session rate), or 60 Hz for
/// a world without a `FixedTime`.
fn step_ms_of(world: &World) -> f64 {
    let hz = crate::runtime::tick_hz(world);
    if hz.is_finite() && hz > 0.0 {
        1000.0 / hz
    } else {
        default_step_ms()
    }
}

/// The length of one predicted tick in seconds at the default 60 Hz. Use
/// [`tick_step_seconds_of`] where a world is at hand: a server or client at another rate steps
/// by its own fixed step.
pub fn tick_step_seconds() -> f32 {
    (default_step_ms() / 1000.0) as f32
}

/// The length of one predicted tick in seconds in `world`: the `dt` of predicted systems on both
/// sides, from the world's fixed step.
pub fn tick_step_seconds_of(world: &World) -> f32 {
    (step_ms_of(world) / 1000.0) as f32
}

// ---- input commands -----------------------------------------------------------------------------

/// One player's input for one tick (§ 3). Small and `Copy`-like; delta-encoded against the next
/// newer command in a packet.
pub trait NetInput:
    ReplicateCodec + Clone + Default + PartialEq + std::fmt::Debug + Send + Sync + 'static
{
    /// What the server uses when a tick's command is missing (§ 5.2). The default repeats the last
    /// command unchanged; input types with one-shot fields (`jump`) override it to clear them.
    fn on_missing(last: &Self) -> Self {
        last.clone()
    }

    /// Spec 120 § 6.3 and § 9.3: folds `late`, an input that has not been delivered on a tick yet
    /// (one a 0-tick frame latched, or one that reached the server after it consumed its tick),
    /// into `next`, the input of the tick being built. A merge keeps button presses and commands
    /// and takes the newest analog values.
    ///
    /// The default works from [`NetInput::on_missing`], which names the one-shot part of an input:
    /// when `late` carries a one-shot (`late != on_missing(late)`) and `next` does not, it returns
    /// `late` (the press is kept; its analog values are one frame older); otherwise `next`. An input
    /// type with one-shot fields should override it field by field (OR the presses, take `next`'s
    /// analog values). Returning `next.clone()` opts out (the late input is dropped).
    fn merge_late(late: &Self, next: &Self) -> Self {
        let late_one_shot = *late != Self::on_missing(late);
        let next_one_shot = *next != Self::on_missing(next);
        if late_one_shot && !next_one_shot {
            late.clone()
        } else {
            next.clone()
        }
    }
}

/// The input of one tick on a controller entity (an entity with [`NetPlayer`]). Written by the
/// game's input system on the owning client each fixed tick before [`crate::NetSet::Predict`]
/// (`regia_net` stamps `tick`); on the server `regia_net` writes the consumed command into it, so
/// predicted systems read input the same way on both sides.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct InputCommand<I: NetInput> {
    /// The tick the command is for.
    pub tick: NetTick,
    /// The game's input value.
    pub value: I,
}

/// Errors of the prediction setup calls.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PredictError {
    /// `add_net_input` was called twice (one input type per game).
    #[error("a net input type is already registered ({0})")]
    InputAlreadyRegistered(&'static str),
    /// `predict::<C>` for a component that is not replicated (spec 71).
    #[error("{0} is not a replicated component")]
    NotReplicated(&'static str),
    /// `predict::<C>` twice for the same type.
    #[error("{0} is already predicted")]
    AlreadyPredicted(&'static str),
}

// ---- registry -----------------------------------------------------------------------------------

/// A stored component value.
pub type Snap = Arc<dyn Any + Send + Sync>;

type EqFn = Arc<dyn Fn(&Snap, &Snap) -> bool + Send + Sync>;

/// One predicted component type, type-erased.
#[derive(Clone)]
struct PredictedComp {
    name: &'static str,
    type_id: TypeId,
    id: Option<ComponentId>,
    /// The codec layout at registration: the § 4 diagnostic fingerprints a predicted component
    /// by it, so two dev processes with different predicted layouts are named.
    layout: u64,
    snapshot: fn(&World, Entity) -> Option<Snap>,
    /// The component's change tick, when it is present. Unchanged snapshots reuse the previous `Arc`.
    changed_tick: fn(&World, Entity) -> Option<u32>,
    write: fn(&mut World, Entity, &Snap),
    eq: EqFn,
    debug: fn(&Snap) -> String,
    pose: fn(&Snap) -> Option<(Vec3, Quat)>,
}

/// The type-erased input stream of the registered [`NetInput`] type.
#[derive(Clone, Copy)]
struct InputVTable {
    name: &'static str,
    layout: u64,
    ensure_controller: fn(&mut World, PeerId),
    latch: fn(&mut World),
    capture: fn(&mut World, u32, Option<ViewTickWire>),
    restore: fn(&mut World, u32),
    record_pos: fn(&mut World, u32, Vec3),
    write_frame: fn(&World, u32, &PredictionSettings, bool) -> Option<Vec<u8>>,
    client_prune: fn(&mut World, u32),
    client_ack: fn(&mut World, u8, u32, &[u32]),
    server_read: fn(&mut World, PeerId, &mut BitReader, u32, &PredictionSettings) -> Result<(), DecodeError>,
    server_consume: fn(&mut World, u32),
    server_remove_peer: fn(&mut World, PeerId),
    server_reported_pos: fn(&World, NetPlayerId, u32) -> Option<Vec3>,
    history_value: fn(&World, u8, u32) -> Option<String>,
    client_truncate: fn(&mut World, u32),
    server_repeated: fn(&World, NetPlayerId, u32) -> Vec<u32>,
    server_buffered: fn(&World, NetPlayerId) -> usize,
}

/// Predicted component types and the input type. A resource, inserted by [`add_net_input`] and
/// [`predict`].
#[derive(Resource, Clone, Default)]
pub struct PredictionRegistry {
    comps: Vec<PredictedComp>,
    input: Option<InputVTable>,
    entity_tolerances: Vec<EntityTolerance>,
    local_writes: Vec<ComponentId>,
}

impl std::fmt::Debug for PredictionRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PredictionRegistry")
            .field("components", &self.component_names())
            .field("input", &self.input.map(|i| i.name))
            .finish()
    }
}

impl PredictionRegistry {
    /// The predicted component types, in registration order.
    pub fn component_names(&self) -> Vec<&'static str> {
        self.comps.iter().map(|c| c.name).collect()
    }

    /// The input type's path, when one is registered.
    pub fn input_name(&self) -> Option<&'static str> {
        self.input.map(|i| i.name)
    }

    /// One `(name, fingerprint)` per predicted type: the input as `predict.input <path>` (its
    /// layout), each predicted component as `predict <path>` (its codec layout; dev builds name
    /// the differing types in `protocol_mismatch`, spec 77 § 4).
    pub fn type_fingerprints(&self) -> Vec<(String, u64)> {
        let mut out = Vec::new();
        if let Some(i) = self.input {
            out.push((format!("predict.input {}", i.name), i.layout));
        }
        out.extend(
            self.comps
                .iter()
                .map(|c| (format!("predict {}", c.name), c.layout)),
        );
        out
    }

    /// Adds the input type's codec to the protocol hash (§ 10).
    pub fn hash_into(&self, h: &mut LayoutHasher) {
        if let Some(i) = self.input {
            h.write_str("predict.input");
            h.write_str(i.name);
            h.write_u64(i.layout);
        }
        for c in &self.comps {
            h.write_str(c.name);
        }
    }

    fn is_active(&self) -> bool {
        self.input.is_some() || !self.comps.is_empty()
    }
}

/// What counts as a mismatch between a predicted and a server value (§ 6.1).
pub enum Tolerance<C> {
    /// The values must travel as the same bits.
    Exact,
    /// A function that returns true when the values are close enough.
    Fn(fn(&C, &C) -> bool),
    /// A closure that returns true when the values are close enough (the helpers below).
    Within(ToleranceFn<C>),
}

/// A shared tolerance closure.
pub type ToleranceFn<C> = Arc<dyn Fn(&C, &C) -> bool + Send + Sync>;

impl<C: ReplicateCodec> Tolerance<C> {
    /// Whether `a` and `b` count as equal.
    pub fn matches(&self, a: &C, b: &C) -> bool {
        match self {
            Self::Exact => a.wire_eq(b, &CodecContext::detached()),
            Self::Fn(f) => f(a, b),
            Self::Within(f) => f(a, b),
        }
    }
}

impl Tolerance<Transform> {
    /// Translation within `meters`; rotation and scale must match exactly.
    pub fn position(meters: f32) -> Self {
        Self::pose(meters, 0.0)
    }

    /// Rotation within `radians`; translation and scale must match exactly.
    pub fn angle(radians: f32) -> Self {
        Self::pose(0.0, radians)
    }

    /// Translation within `meters` and rotation within `radians`; scale must match exactly.
    ///
    /// The rotation comparison is the bit-stable squared chord (`quat_rot_chord2` against an
    /// `angle_chord2` threshold), not `Quat::angle_between`, so the decision is identical on
    /// every platform (spec 73 § 11, ADR-4770). With `radians == 0.0` the rotation must
    /// match bit-for-bit.
    pub fn pose(meters: f32, radians: f32) -> Self {
        Self::Within(Arc::new(move |a: &Transform, b: &Transform| {
            a.translation.distance(b.translation) <= meters
                && quat_rot_chord2(a.rotation, b.rotation) <= angle_chord2(radians)
                && a.scale == b.scale
        }))
    }
}

/// The predicted values of one entity at one tick, as an [`EntityTolerance`] sees them.
pub struct PredictedValues<'a> {
    reg: &'a PredictionRegistry,
    values: &'a [Option<Snap>],
}

impl PredictedValues<'_> {
    /// The value of predicted component `C` (`None`: not predicted, or the entity has none).
    pub fn get<C: Any>(&self) -> Option<&C> {
        let index = self
            .reg
            .comps
            .iter()
            .position(|c| c.type_id == TypeId::of::<C>())?;
        self.values.get(index)?.as_ref()?.downcast_ref::<C>()
    }
}

/// A tolerance over several predicted components of one entity (§ 9: one function decides for a
/// character's `Transform` and `CharacterState` together; § 6.6: a rider compares its position on
/// the base, because the base's world pose differs between client and server).
#[derive(Clone)]
pub struct EntityTolerance {
    /// The component types the tolerance decides for. Their own [`Tolerance`] is not asked for an
    /// entity it accepts; the entity's other predicted components still are.
    pub covers: Vec<TypeId>,
    /// Compares the prediction (first) with the server's values; `None` leaves the entity to the
    /// per-component tolerances.
    pub matches: fn(&PredictedValues<'_>, &PredictedValues<'_>) -> Option<bool>,
    /// The position error of a mismatch in metres, for the correction stats (`None`: the distance
    /// between the two `Transform`s).
    pub error_m: fn(&PredictedValues<'_>, &PredictedValues<'_>) -> Option<f32>,
}

/// Adds an [`EntityTolerance`]; the first one that answers for an entity decides.
pub fn predict_entity_tolerance(world: &mut World, tolerance: EntityTolerance) {
    install_prediction(world);
    world
        .resource_mut::<PredictionRegistry>()
        .entity_tolerances
        .push(tolerance);
}

/// Declares `C` local working state of predicted systems: settings they derive from predicted
/// values, caches, per-step scratch. A re-run recreates it, so the debug check of § 6.1 does not warn
/// about writes to it.
pub fn predict_local_writes<C: Component>(world: &mut World) {
    install_prediction(world);
    let id = world.register_component::<C>();
    world.resource_mut::<PredictionRegistry>().local_writes.push(id);
}

/// `Transform` as a value (prediction history, the agent surface). The fast path of spec 71 § 9
/// carries it on the wire; this codec is full precision.
impl ReplicateCodec for Transform {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        self.translation.encode(w, cx);
        for v in self.rotation.to_array() {
            w.write_f32(v);
        }
        self.scale.encode(w, cx);
    }

    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        let translation = Vec3::decode(r, cx)?;
        let rotation = Quat::from_array([r.read_f32()?, r.read_f32()?, r.read_f32()?, r.read_f32()?]);
        let scale = Vec3::decode(r, cx)?;
        Ok(Self {
            translation,
            rotation,
            scale,
        })
    }
}

fn snapshot_of<C: Component + Clone>(world: &World, e: Entity) -> Option<Snap> {
    world.get::<C>(e).map(|c| Arc::new(c.clone()) as Snap)
}

/// The change tick of `C` on `e`, when the entity has that component.
fn changed_tick_of<C: Component>(world: &World, e: Entity) -> Option<u32> {
    world
        .get_entity(e)
        .ok()
        .and_then(|er| er.get_change_ticks::<C>().map(|t| t.changed.get()))
}

/// Reuses `prev` when the component has not changed since `prev_tick`.
fn snap_reused(
    world: &World,
    c: &PredictedComp,
    e: Entity,
    prev: Option<&Snap>,
    prev_tick: Option<u32>,
) -> (Option<Snap>, Option<u32>) {
    let tick = (c.changed_tick)(world, e);
    if let (Some(prev), Some(stored), Some(now)) = (prev, prev_tick, tick)
        && stored == now
    {
        return (Some(Arc::clone(prev)), Some(stored));
    }
    ((c.snapshot)(world, e), tick)
}

fn write_of<C: Component + Clone>(world: &mut World, e: Entity, v: &Snap) {
    if let (Some(v), Ok(mut ent)) = (v.downcast_ref::<C>(), world.get_entity_mut(e)) {
        ent.insert(v.clone());
    }
}

fn debug_of<C: std::fmt::Debug + 'static>(v: &Snap) -> String {
    v.downcast_ref::<C>()
        .map_or_else(String::new, |c| format!("{c:?}"))
}

fn pose_of(v: &Snap) -> Option<(Vec3, Quat)> {
    v.downcast_ref::<Transform>().map(|t| (t.translation, t.rotation))
}

/// Registers `C` as a predicted component (§ 6.1). `C` must already be replicated (spec 71);
/// `Transform` counts as replicated through the fast path. `tolerance` decides what counts as a
/// mismatch.
pub fn predict<C>(world: &mut World, tolerance: Tolerance<C>) -> Result<(), PredictError>
where
    C: Component + ReplicateCodec + Clone + std::fmt::Debug,
{
    let name = std::any::type_name::<C>();
    let is_transform = TypeId::of::<C>() == TypeId::of::<Transform>();
    let replicated = world
        .get_resource::<crate::replication::ReplicationRegistry>()
        .is_some_and(|r| r.find(name).is_some());
    if !is_transform && !replicated {
        tracing::warn!(target: "net", component = name, "net.predict_not_replicated");
        return Err(PredictError::NotReplicated(name));
    }
    install_prediction(world);
    let id = world.register_component::<C>();
    let mut reg = world.resource_mut::<PredictionRegistry>();
    if reg.comps.iter().any(|c| c.type_id == TypeId::of::<C>()) {
        return Err(PredictError::AlreadyPredicted(name));
    }
    let tol = Arc::new(tolerance);
    reg.comps.push(PredictedComp {
        name,
        type_id: TypeId::of::<C>(),
        id: Some(id),
        layout: crate::codec::layout_hash::<C>(),
        snapshot: snapshot_of::<C>,
        changed_tick: changed_tick_of::<C>,
        write: write_of::<C>,
        eq: Arc::new(
            move |a: &Snap, b: &Snap| match (a.downcast_ref::<C>(), b.downcast_ref::<C>()) {
                (Some(a), Some(b)) => tol.matches(a, b),
                _ => false,
            },
        ),
        debug: debug_of::<C>,
        pose: pose_of,
    });
    Ok(())
}

/// Registers the game's input type (§ 3). One per game: a second call is refused.
pub fn add_net_input<I: NetInput>(world: &mut World) -> Result<(), PredictError> {
    install_prediction(world);
    if let Some(existing) = world.resource::<PredictionRegistry>().input {
        tracing::error!(target: "net", existing = existing.name, "net.input_already_registered");
        return Err(PredictError::InputAlreadyRegistered(existing.name));
    }
    world.init_resource::<InputStream<I>>();
    world.register_component::<InputCommand<I>>();
    world.resource_mut::<PredictionRegistry>().input = Some(InputVTable {
        name: std::any::type_name::<I>(),
        layout: crate::codec::layout_hash::<I>(),
        ensure_controller: ensure_controller::<I>,
        latch: latch_input::<I>,
        capture: capture_input::<I>,
        restore: restore_input::<I>,
        record_pos: record_pos::<I>,
        write_frame: write_input_frame::<I>,
        client_prune: client_prune::<I>,
        client_ack: client_ack::<I>,
        server_read: server_read::<I>,
        server_consume: server_consume_typed::<I>,
        server_remove_peer: server_remove_typed::<I>,
        server_reported_pos: server_reported_pos::<I>,
        history_value: history_value::<I>,
        client_truncate: client_truncate::<I>,
        server_repeated: server_repeated::<I>,
        server_buffered: server_buffered::<I>,
    });
    // Predicted spawns and tick-driven movers travel as replicated components.
    let names = world
        .get_resource::<crate::replication::ReplicationRegistry>()
        .map(|r| {
            (
                r.find(std::any::type_name::<NetPredictionKey>()).is_some(),
                r.find(std::any::type_name::<TickDrivenMotion>()).is_some(),
            )
        })
        .unwrap_or((false, false));
    if !names.0 {
        register_replicated::<NetPredictionKey>(world, ReplicateOptions::default());
    }
    if !names.1 {
        register_replicated::<TickDrivenMotion>(world, ReplicateOptions::default());
    }
    Ok(())
}

/// Inserts the prediction resources (idempotent).
pub fn install_prediction(world: &mut World) {
    world.init_resource::<PredictionRegistry>();
    world.init_resource::<PredictionState>();
    world.init_resource::<NetResimulating>();
    world.init_resource::<PredictedSpawns>();
    world.init_resource::<PredictionEvents>();
    world.init_resource::<ClientPrediction>();
    world.init_resource::<ServerPrediction>();
    world.init_resource::<NetClock>();
    world.init_resource::<Schedules>();
    let mut schedules = world.resource_mut::<Schedules>();
    if !schedules.contains(PredictedUpdate) {
        schedules.insert(Schedule::new(PredictedUpdate));
    }
}

/// The predicted schedule (§ 6.1): fixed-step systems that run on the server for every entity and
/// on the owning client for `NetRole::Predicted` entities, once per tick and again during
/// resimulation.
#[derive(ScheduleLabel, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PredictedUpdate;

/// Adds systems to the predicted schedule on a world (what [`PredictedScheduleExt`] does).
pub fn add_predicted_systems<M>(world: &mut World, systems: impl IntoScheduleConfigs<ScheduleSystem, M>) {
    install_prediction(world);
    world
        .resource_mut::<Schedules>()
        .add_systems(PredictedUpdate, systems);
}

/// `App` registration of the input type.
pub trait NetInputAppExt {
    /// Registers the game's input type (one per game; a second call logs
    /// `net.input_already_registered` and is ignored).
    fn add_net_input<I: NetInput>(&mut self) -> &mut Self;
}

impl NetInputAppExt for App {
    fn add_net_input<I: NetInput>(&mut self) -> &mut Self {
        let _ = add_net_input::<I>(self.world_mut());
        self
    }
}

/// `App` registration of predicted components.
pub trait PredictAppExt {
    /// Registers `C` as predicted (see [`predict`]); a refusal is logged.
    fn predict<C: Component + ReplicateCodec + Clone + std::fmt::Debug>(
        &mut self,
        tolerance: Tolerance<C>,
    ) -> &mut Self;
}

impl PredictAppExt for App {
    fn predict<C: Component + ReplicateCodec + Clone + std::fmt::Debug>(
        &mut self,
        tolerance: Tolerance<C>,
    ) -> &mut Self {
        if let Err(e) = predict::<C>(self.world_mut(), tolerance) {
            tracing::warn!(target: "net", error = %e, "net.predict refused");
        }
        self
    }
}

/// `App` registration of predicted systems.
pub trait PredictedScheduleExt {
    /// Adds fixed-step systems to the predicted schedule.
    fn add_predicted_systems<M>(&mut self, systems: impl IntoScheduleConfigs<ScheduleSystem, M>)
    -> &mut Self;
}

impl PredictedScheduleExt for App {
    fn add_predicted_systems<M>(
        &mut self,
        systems: impl IntoScheduleConfigs<ScheduleSystem, M>,
    ) -> &mut Self {
        install_prediction(self.world_mut());
        self.add_systems(PredictedUpdate, systems);
        self
    }
}

// ---- resources and components -------------------------------------------------------------------

/// The marker of spec 73 § 6.1: the owner of this entity predicts it. The same component as
/// [`NetPredicted`] (reflected, saved in prefabs).
pub type Predicted = NetPredicted;

/// What the predicted schedule is running now. Read through [`PredictionCx`].
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct PredictionState {
    /// The tick being simulated.
    pub tick: NetTick,
    /// Re-running a tick after a correction.
    pub resimulating: bool,
    /// Running a tick for the first time on this client.
    pub first_time: bool,
    /// This process is the server (offline, dedicated or listen).
    pub server: bool,
    /// This process's peer.
    pub local_peer: Option<PeerId>,
}

impl PredictionState {
    /// Whether this process simulates an entity with this role in the predicted schedule: every
    /// entity on the server, only `NetRole::Predicted` copies on a client.
    pub fn simulates(&self, role: Option<&NetRole>) -> bool {
        if self.server {
            !matches!(role, Some(NetRole::Proxy | NetRole::Predicted))
        } else {
            matches!(role, Some(NetRole::Predicted))
        }
    }

    /// The player whose input drives an entity: its `NetOwner` on the server, this client's first
    /// local player on a client (`None` for a server-owned entity on the server).
    pub fn owner_player(&self, owner: Option<&NetOwner>) -> Option<NetPlayerId> {
        if self.server {
            owner.map(|o| NetPlayerId { peer: o.0, local: 0 })
        } else {
            self.local_peer.map(|peer| NetPlayerId { peer, local: 0 })
        }
    }
}

/// True while the client re-runs ticks after a correction (§ 9): collision backends skip push
/// impulses on dynamic bodies then (they were applied the first time).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetResimulating(pub bool);

/// The client's prediction clock (§ 5.1). A resource; on servers it stays at its defaults.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct NetClock {
    /// The tick the client is predicting.
    pub client_tick: NetTick,
    /// The server tick estimated now (spec 74's clock when it runs, else the newest applied tick
    /// plus half the round trip).
    pub server_tick_estimate: NetTickFrac,
    /// The interpolation tick of spec 74 (0 when interpolation does not run).
    pub interp_tick: NetTickFrac,
    /// How many ticks the client runs ahead of `server_tick_estimate`.
    pub lead: i32,
    /// Round-trip estimate, in milliseconds.
    pub rtt_ms: f64,
    /// Jitter estimate (one-way), in milliseconds.
    pub jitter_ms: f64,
}

/// Client: the correction offset of a predicted entity's rendered pose (§ 6.4): rendered = simulated
/// + offset, decaying with `smooth_half_life_ms`.
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct NetVisualOffset {
    /// World-space translation offset.
    pub translation: Vec3,
    /// Rotation offset (applied before the simulated rotation).
    pub rotation: Quat,
}

impl Default for NetVisualOffset {
    fn default() -> Self {
        Self {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
        }
    }
}

impl NetVisualOffset {
    /// The smoothed (rendered) pose of a simulated transform.
    pub fn apply(&self, simulated: &Transform) -> Transform {
        Transform {
            translation: simulated.translation + self.translation,
            rotation: quat_normalize(quat_mul(self.rotation, simulated.rotation)),
            scale: simulated.scale,
        }
    }

    /// Whether the offset is (numerically) zero.
    pub fn is_zero(&self) -> bool {
        self.translation == Vec3::ZERO && self.rotation == Quat::IDENTITY
    }
}

/// Client: a visual child of a predicted entity. `regia_net` writes its local `Transform` as `base`
/// shifted by the parent's [`NetVisualOffset`], so the rendered pose is smoothed while the parent's
/// simulated pose jumps.
#[derive(Component, Debug, Clone, Copy, PartialEq, Default)]
pub struct NetVisualChild {
    /// The child's local transform without smoothing.
    pub base: Transform,
}

/// How a [`TickDrivenMotion`] continues after its last key.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MotionMode {
    /// Holds the last key.
    #[default]
    Once,
    /// Starts again from the first key.
    Loop,
    /// Plays backward to the first key, then forward again.
    PingPong,
}

/// A mover whose pose is a pure function of the server tick (§ 6.6): key poses at key times in
/// seconds, interpolated linearly with rotation nlerp, evaluated at `(tick − start_tick) × step`.
/// Replicated, so both sides hold the same keys; evaluated at the predicted tick on the client, so a
/// character standing on it predicts exactly. Replicate the mover with `TransformReplication::None`
/// (its pose is computed, not sent).
#[derive(Component, Reflect, Debug, Clone, PartialEq, Default)]
#[reflect(Component, Default)]
pub struct TickDrivenMotion {
    /// `(time in seconds, pose)`, in non-decreasing time order. Two keys at the same time are
    /// an instant step from the first pose to the second.
    pub keys: Vec<(f32, Transform)>,
    /// The tick at which time 0 is.
    pub start_tick: NetTick,
    /// What happens after the last key.
    pub mode: MotionMode,
}

impl TickDrivenMotion {
    /// The pose at `tick` with a fixed step of `step_s` seconds (`None` without keys, or when
    /// the key times are non-finite or go backwards). Equal times are an instant step.
    pub fn pose_at(&self, tick: NetTick, step_s: f32) -> Option<Transform> {
        if self.keys.iter().any(|(t, _)| !t.is_finite()) || self.keys.windows(2).any(|w| w[1].0 < w[0].0) {
            return None;
        }
        let first = self.keys.first()?;
        let last = self.keys.last()?;
        let delta = tick.0.saturating_sub(self.start_tick.0);
        let (t0, first_pose) = (first.0, first.1);
        let span = (last.0 - t0).max(0.0);
        // Past 2^24 a tick is not an exact `f32`, so `delta as f32` loses a second. Ticks that
        // still fit stay on the `f32` path so their poses keep the same bits.
        let local = if span <= 0.0 {
            0.0
        } else if delta <= (1u32 << 24) {
            let elapsed = delta as f32 * step_s;
            let t = (elapsed - 0.0).max(0.0);
            match self.mode {
                MotionMode::Once => t.min(span),
                MotionMode::Loop => t % span,
                MotionMode::PingPong => {
                    let m = t % (2.0 * span);
                    if m > span { 2.0 * span - m } else { m }
                }
            }
        } else {
            let span64 = f64::from(span);
            let t = (f64::from(delta) * f64::from(step_s)).max(0.0);
            let raw = match self.mode {
                MotionMode::Once => t.min(span64),
                MotionMode::Loop => t % span64,
                MotionMode::PingPong => {
                    let m = t % (2.0 * span64);
                    if m > span64 { 2.0 * span64 - m } else { m }
                }
            };
            raw as f32
        };
        let at = t0 + local;
        if at <= t0 {
            return Some(first_pose);
        }
        // Keys are stored in increasing time. The interval is the last key strictly before `at`
        // and the first key at or after it.
        let i = self.keys.partition_point(|(t, _)| *t < at);
        if i == 0 {
            return Some(first_pose);
        }
        if i >= self.keys.len() {
            return self.keys.last().map(|(_, pose)| *pose);
        }
        let (ta, a) = self.keys[i - 1];
        let (tb, b) = self.keys[i];
        let f = if tb > ta { (at - ta) / (tb - ta) } else { 1.0 };
        Some(Transform {
            translation: a.translation.lerp(b.translation, f),
            // Bit-stable scalar nlerp: the pose is a pure function of the tick
            // evaluated on server and client, so it must round identically on every
            // platform (glam's `Quat::lerp` is ISA-specific; ADR-4770).
            rotation: quat_nlerp(a.rotation, b.rotation, f),
            scale: a.scale.lerp(b.scale, f),
        })
    }
}

impl ReplicateCodec for TickDrivenMotion {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        w.write_varint(self.keys.len() as u64);
        for (t, pose) in &self.keys {
            w.write_f32(*t);
            pose.encode(w, cx);
        }
        w.write_varint(u64::from(self.start_tick.0));
        w.write_bits(
            match self.mode {
                MotionMode::Once => 0,
                MotionMode::Loop => 1,
                MotionMode::PingPong => 2,
            },
            2,
        );
    }

    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        let n = r.read_len(1024)?;
        let mut keys = Vec::with_capacity(n.min(r.remaining_bits() / 32));
        let mut prev: Option<f32> = None;
        for _ in 0..n {
            let t = r.read_f32()?;
            // Equal times are a step key (what `pose_at` evaluates); only a time that goes
            // backwards is malformed. Refusing equal times made a mover the server encodes
            // undecodable on every client (corr-net, ADR-5580).
            if !t.is_finite() || prev.is_some_and(|p| t < p) {
                return Err(DecodeError::Invalid("motion keys"));
            }
            prev = Some(t);
            keys.push((t, Transform::decode(r, cx)?));
        }
        let start_tick = NetTick(u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("tick"))?);
        let mode = match r.read_bits(2)? {
            0 => MotionMode::Once,
            1 => MotionMode::Loop,
            2 => MotionMode::PingPong,
            _ => return Err(DecodeError::Invalid("motion mode")),
        };
        Ok(Self {
            keys,
            start_tick,
            mode,
        })
    }
}

/// Poses every [`TickDrivenMotion`] entity at `tick` (both sides; the client at its predicted tick).
pub fn apply_tick_driven_motion(world: &mut World, tick: NetTick) {
    let _span = tracing::trace_span!("net_tick_driven_motion").entered();
    let step_s = tick_step_seconds_of(world);
    let mut q = world.query::<(Entity, &TickDrivenMotion, Option<&mut Transform>)>();
    let mut missing: Vec<(Entity, Transform)> = Vec::new();
    for (e, m, t) in q.iter_mut(world) {
        let Some(pose) = m.pose_at(tick, step_s) else {
            continue;
        };
        match t {
            Some(mut t) => {
                if *t != pose {
                    *t = pose;
                }
            }
            None => missing.push((e, pose)),
        }
    }
    for (e, pose) in missing {
        if let Ok(mut ent) = world.get_entity_mut(e) {
            ent.insert(pose);
        }
    }
}

/// Identifies a predicted spawn (§ 7): the player, the tick and the spawn's index among that
/// player's predicted spawns of the tick, in system order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PredictionKey {
    /// The player whose input spawned it.
    pub player: NetPlayerId,
    /// The tick.
    pub tick: NetTick,
    /// The index among the player's predicted spawns of that tick.
    pub index: u16,
}

/// The key of a predicted spawn, on the client's local entity and on the server's real one (a
/// replicated component, so the client can bind the spawn record to its entity).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPredictionKey(pub PredictionKey);

impl ReplicateCodec for NetPredictionKey {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        let k = self.0;
        w.write_varint(u64::from(k.player.peer.0));
        w.write_bits(u64::from(k.player.local), 3);
        w.write_varint(u64::from(k.tick.0));
        w.write_varint(u64::from(k.index));
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let peer = PeerId(u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("peer"))?);
        let local = r.read_bits(3)? as u8;
        let tick = NetTick(u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("tick"))?);
        let index = u16::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("index"))?;
        Ok(Self(PredictionKey {
            player: NetPlayerId { peer, local },
            tick,
            index,
        }))
    }
}

/// Client: a predicted spawn whose server spawn has not arrived yet.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPredictedSpawn;

/// Client: a predicted spawn the server never confirmed (§ 7); its local entity is despawned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPredictedSpawnRejected {
    /// The local entity (despawned).
    pub entity: Entity,
    /// Its key.
    pub key: PredictionKey,
}

/// Prediction notices of the current tick (cleared at the start of each fixed tick).
#[derive(Resource, Debug, Default)]
pub struct PredictionEvents {
    /// Predicted spawns rejected this tick.
    pub rejected: Vec<NetPredictedSpawnRejected>,
    /// Predicted spawns bound to their server spawn this tick: `(local entity, key)`.
    pub bound: Vec<(Entity, PredictionKey)>,
}

/// Predicted spawns by key.
#[derive(Resource, Debug, Default)]
pub struct PredictedSpawns {
    map: BTreeMap<PredictionKey, Entity>,
    counters: HashMap<NetPlayerId, u16>,
    tick: NetTick,
    /// Unbound spawns: the tick of their first history entry.
    first_tick: HashMap<Entity, u32>,
}

impl PredictedSpawns {
    fn begin_tick(&mut self, tick: NetTick) {
        self.tick = tick;
        self.counters.clear();
    }

    /// The local entity of a key, when the client has one.
    pub fn entity(&self, key: &PredictionKey) -> Option<Entity> {
        self.map.get(key).copied()
    }

    /// How many predicted spawns are tracked.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// Whether none are tracked.
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// The system parameter of predicted systems (§§ 6.1, 7, 8).
#[derive(SystemParam)]
pub struct PredictionCx<'w, 's> {
    state: Res<'w, PredictionState>,
    spawns: ResMut<'w, PredictedSpawns>,
    commands: Commands<'w, 's>,
}

impl PredictionCx<'_, '_> {
    /// The tick being simulated.
    pub fn tick(&self) -> NetTick {
        self.state.tick
    }

    /// Whether this run re-simulates a tick after a correction.
    pub fn is_resimulating(&self) -> bool {
        self.state.resimulating
    }

    /// True while running a tick for the first time on this client; false during resimulation and
    /// on the server (§ 8). Gate sounds, muzzle flashes and camera shakes on it.
    pub fn first_time(&self) -> bool {
        self.state.first_time
    }

    /// Whether this process is the server.
    pub fn is_server(&self) -> bool {
        self.state.server
    }

    /// Whether this process simulates an entity with this role in the predicted schedule: every
    /// entity on the server, only `NetRole::Predicted` copies on a client.
    pub fn simulates(&self, role: Option<&NetRole>) -> bool {
        self.state.simulates(role)
    }

    /// The player whose input drives an entity: its `NetOwner` on the server, this client's first
    /// local player on a client (`None` for a server-owned entity on the server).
    pub fn owner_player(&self, owner: Option<&NetOwner>) -> Option<NetPlayerId> {
        self.state.owner_player(owner)
    }

    /// Spawns an entity ahead of the server (§ 7). The server, running the same code for the same
    /// tick, spawns the real replicated entity with the same key; the client binds its spawn record
    /// to this entity. During resimulation a key that already exists returns the existing entity.
    /// The key's player is a parameter because the server simulates every player in one run.
    pub fn spawn_predicted(&mut self, player: NetPlayerId, bundle: impl Bundle) -> (Entity, PredictionKey) {
        let tick = self.state.tick;
        let counter = self.spawns.counters.entry(player).or_insert(0);
        let key = PredictionKey {
            player,
            tick,
            index: *counter,
        };
        *counter = counter.saturating_add(1);
        if let Some(e) = self.spawns.map.get(&key) {
            return (*e, key);
        }
        let mut ec = self.commands.spawn((bundle, NetPredictionKey(key)));
        if !self.state.server {
            ec.insert((NetRole::Predicted, NetOwnedLocally, NetPredictedSpawn));
        }
        let e = ec.id();
        if !self.state.server {
            self.spawns.map.insert(key, e);
            self.spawns.first_tick.insert(e, tick.0);
        }
        (e, key)
    }

    /// Writes a local message unless this run is a resimulation (§ 8).
    pub fn send_local_event<E: Message>(&mut self, e: E) {
        if !self.state.resimulating {
            self.commands.write_message(e);
        }
    }
}

// ---- input stream state -------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct ClientCmd<I> {
    value: I,
    view: Option<ViewTickWire>,
    pos: Option<Vec3>,
}

#[derive(Debug)]
struct ClientStream<I> {
    history: BTreeMap<u32, ClientCmd<I>>,
    acked: Option<u32>,
    /// The input latched from the frames since the last tick that took it (spec 120 § 9.3): a
    /// 0-tick frame leaves it here and the next frame merges into it with
    /// [`NetInput::merge_late`]; the first tick a frame runs takes it.
    pending: Option<I>,
}

impl<I> Default for ClientStream<I> {
    fn default() -> Self {
        Self {
            history: BTreeMap::new(),
            acked: None,
            pending: None,
        }
    }
}

#[derive(Debug, Clone)]
struct ServerCmd<I> {
    value: I,
    view: Option<ViewTickWire>,
    pos: Option<Vec3>,
}

#[derive(Debug)]
struct ServerStream<I> {
    buffer: BTreeMap<u32, ServerCmd<I>>,
    last: I,
    started: bool,
    repeated: BTreeSet<u32>,
    early_counted: u32,
    reported: Option<(u32, Vec3)>,
    /// Spec 120 § 6.3: commands that arrived after their (repeated) tick was consumed, merged
    /// with [`NetInput::merge_late`]; the next consumed tick takes them.
    late: Option<I>,
}

impl<I: Default> Default for ServerStream<I> {
    fn default() -> Self {
        Self {
            buffer: BTreeMap::new(),
            last: I::default(),
            started: false,
            repeated: BTreeSet::new(),
            early_counted: 0,
            reported: None,
            late: None,
        }
    }
}

/// The typed input stream: client input history per local player, server buffers per player.
#[derive(Resource, Debug)]
pub struct InputStream<I: NetInput> {
    client: BTreeMap<u8, ClientStream<I>>,
    server: BTreeMap<NetPlayerId, ServerStream<I>>,
}

impl<I: NetInput> Default for InputStream<I> {
    fn default() -> Self {
        Self {
            client: BTreeMap::new(),
            server: BTreeMap::new(),
        }
    }
}

impl<I: NetInput> InputStream<I> {
    /// Client: the remembered command of a local player for a tick.
    pub fn client_command(&self, local: u8, tick: NetTick) -> Option<&I> {
        self.client.get(&local)?.history.get(&tick.0).map(|c| &c.value)
    }

    /// Server: the buffered (not yet consumed) ticks of a player.
    pub fn buffered_ticks(&self, player: NetPlayerId) -> Vec<NetTick> {
        self.server
            .get(&player)
            .map(|s| s.buffer.keys().map(|t| NetTick(*t)).collect())
            .unwrap_or_default()
    }
}

/// Server: one player's input stream counters (`net.input_buffer`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlayerInputStats {
    /// The newest command tick received.
    pub newest_tick: Option<NetTick>,
    /// The buffer margin: how many ticks early the newest command arrived, averaged over the last
    /// [`MARGIN_WINDOW`] packets.
    pub margin: f32,
    /// Ticks consumed with a repeat because the command was missing.
    pub missing: u64,
    /// Commands that arrived for a tick already simulated.
    pub late: u64,
    /// Commands more than [`INPUT_BUFFER_AHEAD`] ticks ahead (dropped).
    pub early: u64,
    /// Input packets dropped by the packet limit: a token bucket refilled with
    /// `max_input_packets_per_tick` tokens per server tick and holding up to
    /// [`INPUT_BURST_TICKS`] ticks' worth, so a burst a stalled link flushes is kept while a peer
    /// that sends more than the limit every tick is cut to it.
    pub flood: u64,
    /// Ticks whose position the server adopted from the client (§ 6.8).
    pub trusted: u64,
    /// Metres of client-reported drift the server may still adopt (§ 6.8): refilled at
    /// `trust_client_within_m` metres per second, capped at `trust_client_within_m`.
    trust_budget: Option<f32>,
    margins: VecDeque<i32>,
    bucket_tick: Option<u32>,
    tokens: u32,
}

/// Server prediction state: input stream counters per player.
#[derive(Resource, Debug, Default)]
pub struct ServerPrediction {
    /// Per player.
    pub players: BTreeMap<NetPlayerId, PlayerInputStats>,
    /// Input frames dropped because they named a local-player slot the peer never declared in its
    /// `Hello` (only counted when a [`crate::NetSession`] tracks the declared players).
    pub undeclared_frames: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Feedback {
    acked: u32,
    margin: f32,
    count: u32,
}

/// Per predicted entity: the history ring and the newest server values.
#[derive(Debug, Default)]
struct EntityHistory {
    ring: VecDeque<(u32, Vec<Option<Snap>>)>,
    server: Option<Vec<Option<Snap>>>,
    predicted: Option<Vec<Option<Snap>>>,
    confirmed_through: Option<u32>,
    /// The server tick this entity was last compared at (its own reconcile tick, ADR-2413).
    reconciled_at: Option<u32>,
    /// Change tick of each component in the newest ring entry. A later tick reuses that `Arc`
    /// when the world component has not changed. Cleared when a rebase rewrites the ring, so the
    /// next record snapshots the unshifted world.
    snap_ticks: Vec<Option<u32>>,
}

/// Client prediction counters (`net.prediction`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PredictionStats {
    /// Mismatches that rolled back and resimulated.
    pub corrections: u64,
    /// The largest position error of a correction, in meters.
    pub max_error_m: f32,
    /// Ticks re-run by the last rollback.
    pub resim_ticks_last: u32,
    /// Rollbacks without history (or over the resim cap) that snapped to the server. The first
    /// server state of a predicted entity that is new to the client is not one.
    pub resets: u64,
    /// Teleports received for predicted entities.
    pub teleports: u64,
    /// Lead resyncs (`net.lead_resync`).
    pub resyncs: u64,
    /// Predicted ticks inserted (+) or skipped (−) by dilation.
    pub dilated: i64,
    /// Predicted spawns bound to their server spawn.
    pub bound_spawns: u64,
    /// Predicted spawns rejected.
    pub rejected_spawns: u64,
    /// Predicted entities moved back by the last rollback.
    pub last_rollback_entities: u32,
    /// `verify_resim` runs whose two results differed.
    pub verify_failures: u64,
    /// Rollbacks whose rendered pose jumped instead of smoothing: a teleport, a correction over
    /// `snap_distance_m` (or a quarter turn), or a rollback too deep to re-run (spec 120 § 9.4).
    pub snaps: u64,
    /// The largest pose change the last rollback (correction or reset) made on a predicted entity,
    /// in meters: the visual offset it added, or the jump when it snapped.
    pub last_correction_m: f32,
    /// Predicted entities left out of a state tick's comparison because that tick's frames did not
    /// carry them while the server must still have been sending them (a lost or budget-deferred
    /// record, ADR-2413); each keeps its own reconcile tick.
    pub deferred: u64,
}

/// Client prediction state.
#[derive(Resource, Debug)]
pub struct ClientPrediction {
    /// Prediction runs (the first server state arrived).
    pub started: bool,
    /// Counters.
    pub stats: PredictionStats,
    /// Test hook: ticks added to the start lead (positive = too far ahead).
    pub start_offset_ticks: i32,
    /// Test hook ("a modified client", § 13 test 7): run this many predicted ticks per fixed step
    /// with lead control off.
    pub ticks_per_step: Option<u32>,
    /// Round trip and jitter from the transport, in milliseconds (`None` = estimate them from the
    /// input acknowledgments).
    pub transport_rtt: Option<(f64, f64)>,
    /// A transport link is connected and will report a round trip: the clock waits for its first
    /// sample (at most [`START_WAIT_FOR_RTT_STEPS`] steps) before it starts.
    transport_expected: bool,
    /// Fixed steps since the first server state while the clock waited for a round trip.
    start_wait_steps: u64,
    client_tick: u32,
    /// Spec 120 § 9.3: how far the client's continuous (dilated) time is behind `client_tick`, in
    /// ticks, in `[0, 1)`. Dilation stretches or shrinks the tick length; a frame runs the ticks
    /// that start before its end, and the predicted pawns are drawn at the continuous time.
    lag: f64,
    /// The dilation part of each predicted entity's [`NetVisualOffset`] (translation, rotation),
    /// replaced every frame; the rest of the offset is the decaying correction.
    dilation_offsets: HashMap<Entity, (Vec3, Quat)>,
    adjustments: VecDeque<(u32, i32)>,
    negative_run: u32,
    feedback: Option<Feedback>,
    feedback_fresh: bool,
    max_sent: Option<u32>,
    hold_through: Option<u32>,
    sent_at: BTreeMap<u32, u64>,
    rtt_ms: Option<f64>,
    jitter_ms: f64,
    newest_server: Option<u32>,
    /// Whether `newest_server` came from a state frame of the state channel. Before that channel
    /// opens, tick frames set it (on the negotiating wire they carry no state).
    newest_from_state: bool,
    history: HashMap<Entity, EntityHistory>,
    /// Ticks between the last two state ticks compared (the server's send spacing as seen here).
    state_spacing: u32,
    corrections_at: VecDeque<u64>,
    warned_types: HashSet<ComponentId>,
    steps: u64,
    /// Milliseconds per predicted tick, refreshed from the world's fixed step each frame.
    step_ms: f64,
    /// Component type names the debug unregistered-write check built. Release builds leave this
    /// at zero: that check does not run.
    name_builds: u64,
    /// Component ids the debug check has already classified as input or a visual offset, so a
    /// later tick does not build their type names.
    name_skip: HashSet<ComponentId>,
}

impl Default for ClientPrediction {
    fn default() -> Self {
        Self {
            started: false,
            stats: PredictionStats::default(),
            start_offset_ticks: 0,
            ticks_per_step: None,
            transport_rtt: None,
            transport_expected: false,
            start_wait_steps: 0,
            client_tick: 0,
            lag: 0.0,
            dilation_offsets: HashMap::new(),
            adjustments: VecDeque::new(),
            negative_run: 0,
            feedback: None,
            feedback_fresh: false,
            max_sent: None,
            hold_through: None,
            sent_at: BTreeMap::new(),
            rtt_ms: None,
            jitter_ms: 0.0,
            newest_server: None,
            newest_from_state: false,
            history: HashMap::new(),
            state_spacing: 1,
            corrections_at: VecDeque::new(),
            warned_types: HashSet::new(),
            steps: 0,
            step_ms: default_step_ms(),
            name_builds: 0,
            name_skip: HashSet::new(),
        }
    }
}

/// Counters and caches for the prediction hot path. Private so `ServerPrediction` stays a
/// public struct of public fields.
#[derive(Resource, Debug, Default)]
struct PredictPerf {
    /// `NetPlayer` rows `controllers` walked.
    controller_rows: u64,
    /// Full predicted-pawn scans `trusted_entity` ran.
    trusted_scans: u64,
    /// World change tick of `controllers`.
    controller_tick: u32,
    /// `Entities::len` when `controllers` was built. A new entity invalidates the map.
    controller_entities: u32,
    /// `controllers` matches `controller_tick`.
    controller_ready: bool,
    /// Resolved `(entity, local)` per peer, in the historical sort order.
    controllers: HashMap<PeerId, Vec<(Entity, u8)>>,
    /// World change tick of `trust_of`.
    trust_tick: u32,
    /// `trust_of` was built.
    trust_ready: bool,
    /// Owners, links, parent pairs and predicted pawns. A match skips the pawn scan.
    trust_stamp: ((u64, u64), u64),
    /// Lowest-`NetId` predicted pawn of each peer.
    trust_of: HashMap<PeerId, Entity>,
}

impl ClientPrediction {
    /// A new connection came up: the tick numbering of the previous server means nothing to this
    /// one. Clears the clock, the server watermark, the input feedback and the rollback history so
    /// the next state frame restarts prediction; test hooks, counters and the transport RTT stay.
    pub fn reset_for_new_session(&mut self) {
        let fresh = Self::default();
        self.started = fresh.started;
        self.client_tick = fresh.client_tick;
        self.adjustments = fresh.adjustments;
        self.lag = fresh.lag;
        self.negative_run = fresh.negative_run;
        self.feedback = fresh.feedback;
        self.feedback_fresh = fresh.feedback_fresh;
        self.max_sent = fresh.max_sent;
        self.hold_through = fresh.hold_through;
        self.sent_at = fresh.sent_at;
        self.rtt_ms = fresh.rtt_ms;
        self.jitter_ms = fresh.jitter_ms;
        self.newest_server = fresh.newest_server;
        self.newest_from_state = fresh.newest_from_state;
        self.start_wait_steps = fresh.start_wait_steps;
        for h in self.history.values_mut() {
            h.predicted = None;
            h.ring.clear();
            h.snap_ticks.clear();
        }
        self.corrections_at = fresh.corrections_at;
    }

    /// Moves the predicted `Transform` history of root entities by `delta` (a floating-origin
    /// rebase, spec 78 § 12.2): the ring, the newest server values and the newest prediction, so a
    /// rebase is not taken for a misprediction. `is_root` says whether an entity has no parent (a
    /// child's `Transform` is relative and stays).
    pub fn shift_origin(&mut self, delta: Vec3, is_root: &dyn Fn(Entity) -> bool) {
        if !delta.is_finite() || delta == Vec3::ZERO {
            return;
        }
        let shift = |snaps: &mut Vec<Option<Snap>>| {
            for snap in snaps.iter_mut().flatten() {
                if let Some(t) = snap.downcast_ref::<Transform>() {
                    let mut moved = *t;
                    moved.translation += delta;
                    *snap = Arc::new(moved);
                }
            }
        };
        for (entity, history) in &mut self.history {
            if !is_root(*entity) {
                continue;
            }
            for (_, snaps) in &mut history.ring {
                shift(snaps);
            }
            // The world component is not shifted here. Drop the stored ticks so the next record
            // snapshots that world value instead of cloning the shifted ring entry.
            history.snap_ticks.clear();
            for snaps in [&mut history.server, &mut history.predicted]
                .into_iter()
                .flatten()
            {
                shift(snaps);
            }
        }
    }

    /// Spec 120 § 9.3: how far, in ticks (`0..1`), the client's continuous dilated time is behind
    /// [`ClientPrediction::client_tick`]. Predicted entities are drawn there (the dilation part of
    /// [`NetVisualOffset`]); 0 when the clock is not dilating or has caught up.
    pub fn dilation_lag(&self) -> f64 {
        self.lag
    }

    /// The tick being predicted.
    pub fn client_tick(&self) -> NetTick {
        NetTick(self.client_tick)
    }

    /// The newest buffer margin the server reported (averaged), in ticks.
    pub fn reported_margin(&self) -> Option<f32> {
        self.feedback.map(|f| f.margin)
    }

    /// The newest input tick the server acknowledged.
    pub fn acked_tick(&self) -> Option<NetTick> {
        self.feedback.map(|f| NetTick(f.acked))
    }

    /// The margin the clock steers to: `margin_target_ticks` plus one tick per 4 ms of jitter,
    /// clamped to `margin_max_ticks`.
    pub fn margin_target(&self, s: &PredictionSettings) -> f32 {
        let extra = (self.jitter_ms / JITTER_MS_PER_MARGIN_TICK).floor() as u32;
        (s.margin_target_ticks + extra).min(s.margin_max_ticks.max(s.margin_target_ticks)) as f32
    }

    /// Ticks of prediction history kept: `prediction.history_ticks`, clamped to
    /// `1..=`[`MAX_HISTORY_TICKS`].
    pub fn history_cap(s: &PredictionSettings) -> u32 {
        s.history_ticks.clamp(1, MAX_HISTORY_TICKS)
    }

    /// How deep a rollback compares and re-runs as a correction (spec 120 § 9.4, ADR-8219): the
    /// round trip plus the margin target, two ticks and one state send interval, at least
    /// `max_resim_ticks` and at most the history ([`Self::history_cap`]). A deeper rollback still
    /// moves to the server's state and re-runs to the client tick (prediction stays on); it counts
    /// as a reset.
    pub fn resim_depth(&self, s: &PredictionSettings) -> u32 {
        let rtt = self.rtt_ticks().ceil().clamp(0.0, f64::from(MAX_HISTORY_TICKS)) as u32;
        let need = rtt
            .saturating_add(self.margin_target(s).ceil() as u32)
            .saturating_add(2)
            .saturating_add(self.state_spacing);
        need.max(s.max_resim_ticks).min(Self::history_cap(s))
    }

    /// Input send times held for round-trip samples (bounded by [`INPUT_HISTORY_TICKS`]).
    pub fn pending_rtt_samples(&self) -> usize {
        self.sent_at.len()
    }

    /// Predicted entities with history.
    pub fn tracked_entities(&self) -> usize {
        self.history.len()
    }

    /// Whether an entity's history holds a tick.
    pub fn has_history(&self, entity: Entity, tick: NetTick) -> bool {
        self.history
            .get(&entity)
            .is_some_and(|h| h.ring.iter().any(|(t, _)| *t == tick.0))
    }

    fn rtt_ticks(&self) -> f64 {
        self.rtt_ms.map_or(0.0, |r| r / self.step_ms)
    }

    /// The server tick a predicted entity was last reconciled at (spec 73 § 6.3, ADR-2413). On the
    /// state channel two entities can differ: an entity the newest state tick did not carry keeps
    /// the tick it was last compared at.
    pub fn reconcile_tick(&self, entity: Entity) -> Option<NetTick> {
        self.history
            .get(&entity)
            .and_then(|h| h.reconciled_at)
            .map(NetTick)
    }

    /// Ticks after its last carried record during which an entity's absence from a state tick
    /// means the record was lost or deferred, not that it went quiet: the server resends a changed
    /// entity every send until the client's acknowledgment is back, one round trip later.
    fn quiet_after_ticks(&self) -> u32 {
        (self.rtt_ticks().ceil() as u32)
            .saturating_add(self.state_spacing.saturating_mul(2))
            .saturating_add(1)
    }
}

/// Client, state channel (ADR-2413): the predicted entities the frames of state tick `tick` did not
/// carry although the server was still sending them. Their newest server values are older than
/// `tick`, so they are compared at their own tick (left out now) instead of at `tick`.
fn deferred_at(world: &World, list: &[(Entity, Option<NetId>)], tick: u32) -> HashSet<Entity> {
    let Some(client) = world.get_resource::<crate::replication::ClientReplication>() else {
        return HashSet::new();
    };
    let window = world.resource::<ClientPrediction>().quiet_after_ticks();
    list.iter()
        .filter_map(|(e, id)| {
            let carried = client.state.carried_tick((*id)?)?.0;
            (carried < tick && tick - carried < window).then_some(*e)
        })
        .collect()
}

/// Client: a connected transport will report round trips (`NetLink` calls this every frame while
/// connected). The prediction clock then waits for the first one before it starts (spec 120
/// § 9.2), at most [`START_WAIT_FOR_RTT_STEPS`] fixed steps after the first server state.
pub fn expect_transport_rtt(world: &mut World) {
    if let Some(mut cp) = world.get_resource_mut::<ClientPrediction>()
        && !cp.transport_expected
    {
        cp.transport_expected = true;
    }
}

/// Client: sets the round trip and jitter from the transport (milliseconds); the lead uses them
/// instead of the estimate from input acknowledgments.
pub fn set_transport_rtt(world: &mut World, rtt_ms: f64, jitter_ms: f64) {
    install_prediction(world);
    world.resource_mut::<ClientPrediction>().transport_rtt = Some((rtt_ms.max(0.0), jitter_ms.max(0.0)));
}

// ---- helpers ------------------------------------------------------------------------------------

fn mode(world: &World) -> NetMode {
    world.get_resource::<NetMode>().copied().unwrap_or_default()
}

fn active(world: &World) -> Option<PredictionRegistry> {
    world
        .get_resource::<PredictionRegistry>()
        .filter(|r| r.is_active())
        .cloned()
}

fn settings(world: &World) -> PredictionSettings {
    world
        .get_resource::<NetSettings>()
        .map(|s| s.prediction.clone())
        .unwrap_or_default()
}

fn local_peer(world: &World) -> Option<PeerId> {
    world.get_resource::<LocalPeer>().and_then(|p| p.0)
}

/// Client: the predicted entities in `NetId` order (unbound predicted spawns last, by entity).
fn predicted_entities(world: &mut World) -> Vec<(Entity, Option<NetId>)> {
    let mut q = world.query::<(Entity, &NetRole, Option<&NetId>)>();
    let mut out: Vec<(Entity, Option<NetId>)> = q
        .iter(world)
        .filter(|(_, r, _)| **r == NetRole::Predicted)
        .map(|(e, _, id)| (e, id.copied()))
        .collect();
    out.sort_by_key(|(e, id)| (id.is_none(), id.map_or(0, |i| i.0), e.to_bits()));
    out
}

fn snapshot_all(world: &World, reg: &PredictionRegistry, e: Entity) -> Vec<Option<Snap>> {
    reg.comps.iter().map(|c| (c.snapshot)(world, e)).collect()
}

/// Predicted snaps for one entity. Unchanged components clone the previous `Arc`.
fn snaps_reused(
    world: &World,
    reg: &PredictionRegistry,
    e: Entity,
    prev: Option<&[Option<Snap>]>,
    ticks: &[Option<u32>],
) -> (Vec<Option<Snap>>, Vec<Option<u32>>) {
    let mut values = Vec::with_capacity(reg.comps.len());
    let mut new_ticks = Vec::with_capacity(reg.comps.len());
    for (i, c) in reg.comps.iter().enumerate() {
        let prev_snap = prev.and_then(|v| v.get(i)).and_then(|s| s.as_ref());
        let prev_tick = ticks.get(i).copied().flatten();
        let (snap, tick) = snap_reused(world, c, e, prev_snap, prev_tick);
        values.push(snap);
        new_ticks.push(tick);
    }
    (values, new_ticks)
}

fn write_all(world: &mut World, reg: &PredictionRegistry, e: Entity, values: &[Option<Snap>]) {
    for (c, v) in reg.comps.iter().zip(values) {
        if let Some(v) = v {
            (c.write)(world, e, v);
        }
    }
}

fn pose(reg: &PredictionRegistry, values: &[Option<Snap>]) -> Option<(Vec3, Quat)> {
    reg.comps
        .iter()
        .zip(values)
        .find_map(|(c, v)| v.as_ref().and_then(|v| (c.pose)(v)))
}

fn values_match(reg: &PredictionRegistry, a: &[Option<Snap>], b: &[Option<Snap>]) -> bool {
    let (va, vb) = (
        PredictedValues { reg, values: a },
        PredictedValues { reg, values: b },
    );
    let mut covered: &[TypeId] = &[];
    for t in &reg.entity_tolerances {
        match (t.matches)(&va, &vb) {
            Some(true) => {
                covered = &t.covers;
                break;
            }
            Some(false) => return false,
            None => {}
        }
    }
    reg.comps.iter().zip(a.iter().zip(b)).all(|(c, (a, b))| {
        covered.contains(&c.type_id)
            || match (a, b) {
                (Some(a), Some(b)) => (c.eq)(a, b),
                (None, None) => true,
                _ => false,
            }
    })
}

/// The position error of a mismatch (§ 6.3).
fn error_m(reg: &PredictionRegistry, a: &[Option<Snap>], b: &[Option<Snap>]) -> Option<f32> {
    let (va, vb) = (
        PredictedValues { reg, values: a },
        PredictedValues { reg, values: b },
    );
    reg.entity_tolerances
        .iter()
        .find_map(|t| (t.error_m)(&va, &vb))
        .or_else(|| match (pose(reg, a), pose(reg, b)) {
            (Some((a, _)), Some((b, _))) => Some(a.distance(b)),
            _ => None,
        })
}

fn client_view_tick(world: &World) -> Option<ViewTickWire> {
    let ni = world.get_resource::<NetInterpolation>()?;
    let t = ni.clock.interp_tick()?;
    let delay = ni.clock.delay_ms(ni.now_ms).unwrap_or(0.0);
    Some(ViewTickWire::new(t, delay))
}

// ---- typed input functions ----------------------------------------------------------------------

fn invalidate_controllers(world: &mut World) {
    if let Some(mut perf) = world.get_resource_mut::<PredictPerf>() {
        perf.controller_ready = false;
    }
}

fn controllers(world: &mut World, peer: PeerId) -> Vec<(Entity, u8)> {
    world.init_resource::<PredictPerf>();
    let tick = world.read_change_tick().get();
    let entities = world.entities().len();
    let cached = {
        let perf = world.resource::<PredictPerf>();
        perf.controller_ready && perf.controller_tick == tick && perf.controller_entities == entities
    };
    if cached {
        let rows = world
            .resource::<PredictPerf>()
            .controllers
            .get(&peer)
            .cloned()
            .unwrap_or_default();
        if rows.iter().all(|(e, _)| world.get_entity(*e).is_ok()) {
            return rows;
        }
    }
    let mut q = world.query::<(Entity, &NetPlayer)>();
    let mut grouped: HashMap<PeerId, Vec<(Entity, u8)>> = HashMap::new();
    let mut rows = 0u64;
    for (e, p) in q.iter(world) {
        rows += 1;
        grouped.entry(p.id.peer).or_default().push((e, p.id.local));
    }
    for list in grouped.values_mut() {
        list.sort_by_key(|(e, l)| (*l, e.to_bits()));
        list.dedup_by_key(|(_, l)| *l);
    }
    let out = grouped.get(&peer).cloned().unwrap_or_default();
    let mut perf = world.resource_mut::<PredictPerf>();
    perf.controller_rows += rows;
    perf.controller_tick = tick;
    perf.controller_entities = entities;
    perf.controller_ready = true;
    perf.controllers = grouped;
    out
}

fn ensure_controller<I: NetInput>(world: &mut World, peer: PeerId) {
    let found = controllers(world, peer);
    if found.is_empty() {
        world.spawn((
            NetPlayer {
                id: NetPlayerId { peer, local: 0 },
            },
            InputCommand::<I>::default(),
        ));
        invalidate_controllers(world);
        return;
    }
    for (e, _) in found {
        if world.get::<InputCommand<I>>(e).is_none()
            && let Ok(mut ent) = world.get_entity_mut(e)
        {
            ent.insert(InputCommand::<I>::default());
        }
    }
}

/// Spec 120 § 9.3: once per frame, before the frame's ticks run, folds the input the game wrote
/// this frame into the latched input of each local player.
fn latch_input<I: NetInput>(world: &mut World) {
    let Some(peer) = local_peer(world) else {
        return;
    };
    for (e, local) in controllers(world, peer) {
        let Some(value) = world.get::<InputCommand<I>>(e).map(|c| c.value.clone()) else {
            continue;
        };
        let mut stream = world.resource_mut::<InputStream<I>>();
        let s = stream.client.entry(local).or_default();
        s.pending = Some(match s.pending.take() {
            Some(late) => I::merge_late(&late, &value),
            None => value,
        });
    }
}

/// The input of a first-time tick. The first tick of a frame takes the latched input; a second
/// tick in the same frame gets `on_missing` of it, so a press is delivered on exactly one tick. A
/// tick the stream will not send (at or below `hold_through`, after a backward resync) keeps the
/// one-shot part latched for the first tick that is sent, so the server sees every press too.
fn capture_input<I: NetInput>(world: &mut World, tick: u32, view: Option<ViewTickWire>) {
    let Some(peer) = local_peer(world) else {
        return;
    };
    let held = world
        .get_resource::<ClientPrediction>()
        .and_then(|cp| cp.hold_through)
        .is_some_and(|h| tick <= h);
    for (e, local) in controllers(world, peer) {
        let Some(current) = world.get::<InputCommand<I>>(e).map(|c| c.value.clone()) else {
            continue;
        };
        let value = {
            let mut stream = world.resource_mut::<InputStream<I>>();
            let s = stream.client.entry(local).or_default();
            match s.pending.take() {
                Some(latched) if held => {
                    let v = I::on_missing(&latched);
                    s.pending = Some(latched);
                    v
                }
                Some(latched) => latched,
                None => I::on_missing(&current),
            }
        };
        if let Some(mut c) = world.get_mut::<InputCommand<I>>(e) {
            c.tick = NetTick(tick);
            if c.value != value {
                c.value = value.clone();
            }
        }
        let mut stream = world.resource_mut::<InputStream<I>>();
        let s = stream.client.entry(local).or_default();
        s.history.insert(
            tick,
            ClientCmd {
                value,
                view,
                pos: None,
            },
        );
        let keep_from = tick.saturating_sub(INPUT_HISTORY_TICKS);
        while s.history.first_key_value().is_some_and(|(t, _)| *t < keep_from) {
            s.history.pop_first();
        }
    }
}

fn restore_input<I: NetInput>(world: &mut World, tick: u32) {
    let Some(peer) = local_peer(world) else {
        return;
    };
    for (e, local) in controllers(world, peer) {
        let value = {
            let stream = world.resource::<InputStream<I>>();
            stream.client.get(&local).and_then(|s| {
                s.history.get(&tick).map(|c| c.value.clone()).or_else(|| {
                    s.history
                        .range(..tick)
                        .next_back()
                        .map(|(_, c)| I::on_missing(&c.value))
                })
            })
        }
        .unwrap_or_default();
        if let Some(mut c) = world.get_mut::<InputCommand<I>>(e) {
            c.tick = NetTick(tick);
            c.value = value;
        }
    }
}

fn record_pos<I: NetInput>(world: &mut World, tick: u32, pos: Vec3) {
    let mut stream = world.resource_mut::<InputStream<I>>();
    if let Some(s) = stream.client.get_mut(&0)
        && let Some(c) = s.history.get_mut(&tick)
    {
        c.pos = Some(pos);
    }
}

fn client_prune<I: NetInput>(world: &mut World, through: u32) {
    let mut stream = world.resource_mut::<InputStream<I>>();
    for s in stream.client.values_mut() {
        while s.history.first_key_value().is_some_and(|(t, _)| *t <= through) {
            s.history.pop_first();
        }
    }
}

fn client_ack<I: NetInput>(world: &mut World, local: u8, acked: u32, repeated: &[u32]) {
    let mut stream = world.resource_mut::<InputStream<I>>();
    let s = stream.client.entry(local).or_default();
    s.acked = Some(s.acked.map_or(acked, |a| a.max(acked)));
    // The server consumed repeats for these ticks: resimulation must use what the server used.
    for &t in repeated {
        if !s.history.contains_key(&t) {
            continue;
        }
        let prev = s
            .history
            .range(..t)
            .next_back()
            .map(|(_, c)| c.value.clone())
            .unwrap_or_default();
        if let Some(c) = s.history.get_mut(&t) {
            c.value = I::on_missing(&prev);
        }
    }
}

fn write_input_frame<I: NetInput>(
    world: &World,
    client_tick: u32,
    s: &PredictionSettings,
    trust: bool,
) -> Option<Vec<u8>> {
    let stream = world.get_resource::<InputStream<I>>()?;
    let client = world.get_resource::<ClientPrediction>()?;
    let hold = client.hold_through;
    let cx = CodecContext::detached();
    let mut w = BitWriter::new();
    w.write_bits(u64::from(TAG_INPUT), 8);
    w.write_varint(u64::from(client_tick));
    let players: Vec<(&u8, &ClientStream<I>)> = stream.client.iter().collect();
    w.write_varint(players.len() as u64);
    let mut any = false;
    for (local, cs) in players {
        w.write_bits(u64::from(*local), 3);
        let floor = cs.acked.max(hold);
        let cmds: Vec<(u32, &ClientCmd<I>)> = cs
            .history
            .range(..=client_tick)
            .rev()
            .filter(|(t, _)| floor.is_none_or(|f| **t > f))
            .take(s.input_redundancy.clamp(1, INPUT_BUFFER_AHEAD) as usize)
            .map(|(t, c)| (*t, c))
            .collect();
        // Only a contiguous run newest-first travels (a gap ends it).
        let mut run: Vec<(u32, &ClientCmd<I>)> = Vec::with_capacity(cmds.len());
        for (t, c) in cmds {
            if run.last().is_some_and(|(prev, _)| *prev != t + 1) {
                break;
            }
            run.push((t, c));
        }
        w.write_varint(run.len() as u64);
        let Some((newest, first)) = run.first() else {
            continue;
        };
        any = true;
        w.write_varint(u64::from(*newest));
        w.write_bool(first.view.is_some());
        if let Some(v) = first.view {
            v.encode(&mut w);
        }
        w.write_bool(trust);
        // The newest view is the header above. Each older command carries the view sampled
        // with that command (spec 74 § 8). A frame whose newest command has no view grows no
        // extra bits, so a hand-built frame with `has_view = false` still decodes.
        let header_view = first.view.is_some();
        let mut newer: Option<&I> = None;
        for (i, (_, c)) in run.iter().enumerate() {
            match newer {
                Some(n) if *n == c.value => w.write_bool(true),
                Some(_) => {
                    w.write_bool(false);
                    c.value.encode(&mut w, &cx);
                }
                None => c.value.encode(&mut w, &cx),
            }
            if i > 0 && header_view {
                w.write_bool(c.view.is_some());
                if let Some(v) = c.view {
                    v.encode(&mut w);
                }
            }
            if trust {
                w.write_bool(c.pos.is_some());
                if let Some(p) = c.pos {
                    w.write_f32(p.x);
                    w.write_f32(p.y);
                    w.write_f32(p.z);
                }
            }
            newer = Some(&c.value);
        }
    }
    any.then(|| w.finish())
}

fn server_read<I: NetInput>(
    world: &mut World,
    peer: PeerId,
    r: &mut BitReader,
    server_tick: u32,
    s: &PredictionSettings,
) -> Result<(), DecodeError> {
    let cx = CodecContext::detached();
    let _client_tick = r.read_varint()?;
    let n_players = r.read_len(usize::from(crate::model::MAX_LOCAL_PLAYERS))?;
    for _ in 0..n_players {
        let local = r.read_bits(3)? as u8;
        let count = r.read_len(INPUT_BUFFER_AHEAD as usize)?;
        if count == 0 {
            continue;
        }
        let newest = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("tick"))?;
        let header_view = r.read_bool()?;
        let newest_view = if header_view {
            Some(ViewTickWire::decode(r)?)
        } else {
            None
        };
        let trust = r.read_bool()?;
        let mut cmds: Vec<(u32, I, Option<Vec3>, Option<ViewTickWire>)> = Vec::with_capacity(count);
        let mut newer: Option<I> = None;
        for i in 0..count {
            let value = match &newer {
                Some(n) if r.read_bool()? => n.clone(),
                _ => I::decode(r, &cx)?,
            };
            let cmd_view = if i == 0 {
                newest_view
            } else if header_view {
                if r.read_bool()? {
                    Some(ViewTickWire::decode(r)?)
                } else {
                    None
                }
            } else {
                None
            };
            let pos = if trust && r.read_bool()? {
                Some(Vec3::new(r.read_f32()?, r.read_f32()?, r.read_f32()?))
            } else {
                None
            };
            let tick = newest
                .checked_sub(i as u32)
                .ok_or(DecodeError::Invalid("input tick"))?;
            newer = Some(value.clone());
            cmds.push((tick, value, pos, cmd_view));
        }
        let player = NetPlayerId { peer, local };
        // The dedicated app can host NetLink alongside an unrelated NetSession (ADR-4861).
        // A live replication-link peer keeps that input path; all other peers still need their
        // session's declared slot. Merely installing an idle link grants no exemption.
        let link_peer = world
            .get_resource::<crate::NetLink>()
            .is_some_and(|link| link.has_peer(peer));
        let undeclared = !link_peer
            && world
                .get_resource::<crate::NetSession>()
                .is_some_and(|ns| peer != PeerId::SERVER && ns.player_entity(player).is_none());
        if undeclared {
            world.resource_mut::<ServerPrediction>().undeclared_frames += 1;
            continue;
        }
        // Flood limit (spec 70 § 6.6 with its own budget; spec 120 § 6.1): a token bucket, so a
        // burst of packets a Wi-Fi stall held back is kept, and the sustained rate stays capped.
        {
            let mut sp = world.resource_mut::<ServerPrediction>();
            let st = sp.players.entry(player).or_default();
            let rate = s.max_input_packets_per_tick.max(1);
            let cap = rate.saturating_mul(INPUT_BURST_TICKS);
            st.tokens = match st.bucket_tick {
                None => cap,
                Some(t) if server_tick > t => st
                    .tokens
                    .saturating_add(rate.saturating_mul(server_tick - t))
                    .min(cap),
                Some(_) => st.tokens,
            };
            st.bucket_tick = Some(st.bucket_tick.map_or(server_tick, |t| t.max(server_tick)));
            if st.tokens == 0 {
                st.flood += 1;
                continue;
            }
            st.tokens -= 1;
            let margin = i64::from(newest) - i64::from(server_tick);
            st.margins.push_back(margin.clamp(-1000, 1000) as i32);
            while st.margins.len() > MARGIN_WINDOW {
                st.margins.pop_front();
            }
            st.margin = st.margins.iter().map(|m| *m as f32).sum::<f32>() / st.margins.len() as f32;
            st.newest_tick = Some(NetTick(st.newest_tick.map_or(newest, |t| t.0.max(newest))));
        }
        let (mut late, mut early) = (0u64, 0u64);
        {
            let mut stream = world.resource_mut::<InputStream<I>>();
            let ss = stream.server.entry(player).or_default();
            // Commands for ticks already consumed with a repeat, oldest first (spec 120 § 6.3).
            let mut missed: Vec<(u32, I)> = Vec::new();
            for (i, (tick, value, pos, cmd_view)) in cmds.into_iter().enumerate() {
                if tick < server_tick {
                    if ss.repeated.remove(&tick) {
                        late += 1;
                        missed.push((tick, value));
                    }
                    continue;
                }
                if tick > server_tick.saturating_add(INPUT_BUFFER_AHEAD) {
                    if tick > ss.early_counted {
                        ss.early_counted = tick;
                        early += 1;
                    }
                    continue;
                }
                ss.started = true;
                // Each command keeps the view sampled with it. Reconstructing an older command
                // as the newest view minus one tick is wrong when two commands were sampled at
                // one interpolation tick (spec 74 § 8). A packet where this tick is the newest
                // replaces a view an earlier redundant copy stored; a redundant copy fills a hole.
                match ss.buffer.entry(tick) {
                    std::collections::btree_map::Entry::Vacant(slot) => {
                        slot.insert(ServerCmd {
                            value,
                            view: cmd_view,
                            pos,
                        });
                    }
                    std::collections::btree_map::Entry::Occupied(mut slot) => {
                        let stored = slot.get_mut();
                        if cmd_view.is_some() && (i == 0 || stored.view.is_none()) {
                            stored.view = cmd_view;
                        }
                    }
                }
            }
            // A command that missed its tick is merged into the next unbuilt tick: its presses
            // are kept, the newest analog values win (`NetInput::merge_late`).
            missed.sort_by_key(|(t, _)| *t);
            for (_, value) in missed {
                ss.late = Some(match ss.late.take() {
                    Some(older) => I::merge_late(&older, &value),
                    None => value,
                });
            }
        }
        let mut sp = world.resource_mut::<ServerPrediction>();
        let st = sp.players.entry(player).or_default();
        st.late += late;
        st.early += early;
    }
    Ok(())
}

fn server_consume_typed<I: NetInput>(world: &mut World, tick: u32) {
    let players: Vec<NetPlayerId> = world
        .resource::<InputStream<I>>()
        .server
        .keys()
        .copied()
        .collect();
    for player in players {
        let (value, view, missing) = {
            let mut stream = world.resource_mut::<InputStream<I>>();
            let Some(ss) = stream.server.get_mut(&player) else {
                continue;
            };
            if !ss.started {
                continue;
            }
            while ss.buffer.first_key_value().is_some_and(|(t, _)| *t < tick) {
                ss.buffer.pop_first();
            }
            let keep = tick.saturating_sub(INPUT_BUFFER_AHEAD);
            while ss.repeated.first().is_some_and(|t| *t < keep) {
                ss.repeated.pop_first();
            }
            let late = ss.late.take();
            let (value, view, missing) = match ss.buffer.remove(&tick) {
                Some(c) => {
                    ss.reported = c.pos.map(|p| (tick, p));
                    (c.value, c.view, false)
                }
                None => {
                    let v = I::on_missing(&ss.last);
                    ss.repeated.insert(tick);
                    ss.reported = None;
                    (v, None, true)
                }
            };
            let value = match late {
                Some(late) => I::merge_late(&late, &value),
                None => value,
            };
            ss.last = value.clone();
            (value, view, missing)
        };
        if missing {
            world
                .resource_mut::<ServerPrediction>()
                .players
                .entry(player)
                .or_default()
                .missing += 1;
        }
        let controller = match controllers(world, player.peer)
            .into_iter()
            .find(|(_, l)| *l == player.local)
        {
            Some((e, _)) => e,
            None => {
                invalidate_controllers(world);
                world.spawn(NetPlayer { id: player }).id()
            }
        };
        if let Ok(mut ent) = world.get_entity_mut(controller) {
            ent.insert(InputCommand {
                tick: NetTick(tick),
                value,
            });
        }
        set_view_tick(world, controller, view);
    }
    // Local players (offline, listen host): their command is consumed directly at its tick.
    let mut q = world.query::<(&NetPlayer, &mut InputCommand<I>)>();
    for (p, mut c) in q.iter_mut(world) {
        if p.id.peer == PeerId::SERVER && c.tick.0 != tick {
            c.tick = NetTick(tick);
        }
    }
}

fn server_remove_typed<I: NetInput>(world: &mut World, peer: PeerId) {
    if let Some(mut s) = world.get_resource_mut::<InputStream<I>>() {
        s.server.retain(|p, _| p.peer != peer);
    }
}

fn server_reported_pos<I: NetInput>(world: &World, player: NetPlayerId, tick: u32) -> Option<Vec3> {
    let s = world.get_resource::<InputStream<I>>()?;
    s.server
        .get(&player)?
        .reported
        .filter(|(t, _)| *t == tick)
        .map(|(_, p)| p)
}

fn history_value<I: NetInput>(world: &World, local: u8, tick: u32) -> Option<String> {
    let s = world.get_resource::<InputStream<I>>()?;
    s.client
        .get(&local)?
        .history
        .get(&tick)
        .map(|c| format!("{:?}", c.value))
}

fn client_truncate<I: NetInput>(world: &mut World, above: u32) {
    let mut stream = world.resource_mut::<InputStream<I>>();
    for s in stream.client.values_mut() {
        while s.history.last_key_value().is_some_and(|(t, _)| *t > above) {
            s.history.pop_last();
        }
    }
}

fn server_repeated<I: NetInput>(world: &World, player: NetPlayerId, from: u32) -> Vec<u32> {
    world
        .get_resource::<InputStream<I>>()
        .and_then(|s| s.server.get(&player))
        .map(|ss| ss.repeated.range(from..).copied().collect())
        .unwrap_or_default()
}

fn server_buffered<I: NetInput>(world: &World, player: NetPlayerId) -> usize {
    world
        .get_resource::<InputStream<I>>()
        .and_then(|s| s.server.get(&player))
        .map_or(0, |ss| ss.buffer.len())
}

// ---- runtime hooks ------------------------------------------------------------------------------

/// Clears last tick's prediction notices (`NetSet::Apply`).
pub(crate) fn clear_events(world: &mut World) {
    if let Some(mut ev) = world.get_resource_mut::<PredictionEvents>() {
        ev.rejected.clear();
        ev.bound.clear();
    }
}

fn set_state(world: &mut World, tick: u32, resimulating: bool, server: bool) {
    let local_peer = local_peer(world);
    *world.resource_mut::<PredictionState>() = PredictionState {
        tick: NetTick(tick),
        resimulating,
        first_time: !server && !resimulating,
        server,
        local_peer,
    };
    let mut r = world.resource_mut::<NetResimulating>();
    if r.0 != resimulating {
        r.0 = resimulating;
    }
    world.resource_mut::<PredictedSpawns>().begin_tick(NetTick(tick));
}

fn run_predicted_schedule(world: &mut World) {
    if world.try_run_schedule(PredictedUpdate).is_err() {
        // No predicted systems registered: nothing to run.
    }
}

/// `NetSet::Predict`: the predicted schedule for this fixed tick. On the server it runs once for
/// the tick being simulated (every entity); on a client it runs for the client tick (or twice, or
/// not at all, while the lead is dilated), records the prediction history and decays correction
/// offsets.
pub fn net_predict(world: &mut World) {
    let Some(reg) = active(world) else {
        return;
    };
    let _span = tracing::trace_span!("net_predict").entered();
    if mode(world).is_server() {
        server_predict(world, &reg);
    } else {
        client_step(world, &reg);
    }
}

fn server_predict(world: &mut World, reg: &PredictionRegistry) {
    let tick = world.resource::<NetNode>().tick.0;
    set_state(world, tick, false, true);
    apply_tick_driven_motion(world, NetTick(tick));
    run_predicted_schedule(world);
    trust_adopt(world, reg, tick);
}

/// § 6.8: adopts a client's reported position when the server's result is within the tolerance.
fn trust_adopt(world: &mut World, reg: &PredictionRegistry, tick: u32) {
    let Some(tol) = settings(world).trust_client_within_m else {
        return;
    };
    let Some(input) = reg.input else {
        return;
    };
    let players: Vec<NetPlayerId> = world
        .resource::<ServerPrediction>()
        .players
        .keys()
        .copied()
        .collect();
    let refill = tol * (step_ms_of(world) / 1000.0) as f32;
    refresh_trust(world);
    for player in players {
        // The budget refills every tick, whether or not a position was reported, so the adopted
        // drift is bounded per second, not per tick.
        let budget = {
            let mut sp = world.resource_mut::<ServerPrediction>();
            let st = sp.players.entry(player).or_default();
            let b = st.trust_budget.map_or(tol, |b| (b + refill).min(tol));
            st.trust_budget = Some(b);
            b
        };
        let Some(pos) = (input.server_reported_pos)(world, player, tick) else {
            continue;
        };
        let Some(e) = trusted_entity(world, player.peer) else {
            continue;
        };
        let Some(mut t) = world.get_mut::<Transform>(e) else {
            continue;
        };
        let d = t.translation.distance(pos);
        if d <= tol && d <= budget && d > 0.0 {
            t.translation = pos;
            let mut sp = world.resource_mut::<ServerPrediction>();
            let st = sp.players.entry(player).or_default();
            st.trust_budget = Some(budget - d);
            st.trusted += 1;
        }
    }
}

/// Owners, owner links, every parent link and predicted pawns. The pawn scan is skipped while
/// this is unchanged. The parent pairs are part of the stamp (not only each pawn's own parent):
/// `resolve_owner` walks the whole chain, so reparenting a non-predicted link in it (a pawn
/// seated in a vehicle the owner's root carries) changes the pawn's owner and must rescan
/// (corr-low, ADR-5789).
fn trust_stamp(world: &mut World) -> ((u64, u64), u64) {
    let owners_links_parents = crate::replication::owner_record_signature(world);
    let mut q = world.query_filtered::<(
        Entity,
        &NetId,
        Option<&NetOwner>,
        Option<&NetOwnerLink>,
        Option<&ChildOf>,
        Option<&Transform>,
    ), With<NetPredicted>>();
    let mut pawns = 0u64;
    for (e, id, owner, link, child, tf) in q.iter(world) {
        let mut z = e.to_bits() ^ u64::from(id.0).rotate_left(17);
        z ^= owner.map(|o| u64::from(o.0.0)).unwrap_or(0).rotate_left(7);
        z ^= link.map(|l| l.0.to_bits()).unwrap_or(0).rotate_left(13);
        z ^= child.map(|c| c.parent().to_bits()).unwrap_or(0).rotate_left(19);
        z ^= u64::from(tf.is_some());
        pawns = pawns.wrapping_add(z.wrapping_mul(0x9e37_79b9_7f4a_7c15));
    }
    (owners_links_parents, pawns)
}

/// Fills `PredictPerf::trust_of` once per tick, and again only when the stamp changes.
fn refresh_trust(world: &mut World) {
    world.init_resource::<PredictPerf>();
    let tick = world.read_change_tick().get();
    if world.resource::<PredictPerf>().trust_ready && world.resource::<PredictPerf>().trust_tick == tick {
        return;
    }
    let stamp = trust_stamp(world);
    if world.resource::<PredictPerf>().trust_ready && world.resource::<PredictPerf>().trust_stamp == stamp {
        world.resource_mut::<PredictPerf>().trust_tick = tick;
        return;
    }
    world.resource_mut::<PredictPerf>().trusted_scans += 1;
    let mut q = world.query_filtered::<(Entity, &NetId), (With<NetPredicted>, With<Transform>)>();
    let mut found: Vec<(NetId, Entity)> = q.iter(world).map(|(e, id)| (*id, e)).collect();
    found.sort();
    let mut trust_of: HashMap<PeerId, Entity> = HashMap::new();
    for (_, e) in found {
        let owner = crate::model::resolve_owner(world, e).peer();
        trust_of.entry(owner).or_insert(e);
    }
    let mut perf = world.resource_mut::<PredictPerf>();
    perf.trust_of = trust_of;
    perf.trust_stamp = stamp;
    perf.trust_tick = tick;
    perf.trust_ready = true;
}

/// Server: the predicted entity whose position a peer reports (its lowest-`NetId` predicted
/// entity with a transform).
fn trusted_entity(world: &mut World, peer: PeerId) -> Option<Entity> {
    refresh_trust(world);
    world.resource::<PredictPerf>().trust_of.get(&peer).copied()
}

/// Client: the predicted entity whose position the input stream reports.
fn reported_entity(world: &mut World) -> Option<Entity> {
    predicted_entities(world)
        .into_iter()
        .find(|(e, id)| id.is_some() && world.get::<Transform>(*e).is_some())
        .map(|(e, _)| e)
}

/// Copies the world's fixed step into the prediction state, which converts round trips to ticks.
fn refresh_step(world: &mut World) {
    let step = step_ms_of(world);
    if let Some(mut cp) = world.get_resource_mut::<ClientPrediction>() {
        cp.step_ms = step;
    }
}

fn client_step(world: &mut World, reg: &PredictionRegistry) {
    refresh_step(world);
    let s = settings(world);
    if let (Some(input), Some(peer)) = (reg.input, local_peer(world)) {
        (input.ensure_controller)(world, peer);
    }
    if !world.resource::<ClientPrediction>().started {
        let mut cp = world.resource_mut::<ClientPrediction>();
        if cp.newest_server.is_some() {
            cp.start_wait_steps = cp.start_wait_steps.saturating_add(1);
        }
        return;
    }
    world.resource_mut::<ClientPrediction>().steps += 1;
    if let Some(input) = reg.input {
        (input.latch)(world);
    }
    let n = clock_control(world, reg, &s);
    for _ in 0..n {
        let tick = {
            let mut cp = world.resource_mut::<ClientPrediction>();
            cp.client_tick = cp.client_tick.saturating_add(1);
            cp.client_tick
        };
        run_client_tick(world, reg, tick, false, &s);
    }
    // The new dilation parts first: an entity that gets one keeps its offset component through the
    // decay (no remove and re-insert every dilated frame).
    let parts = dilation_parts(world, reg);
    take_dilation_offsets(world);
    decay_offsets(world, &s, &parts);
    put_dilation_offsets(world, parts);
    apply_visual_children(world);
    reject_spawns(world, &s);
    update_clock(world);
}

fn adjustment_pending(cp: &ClientPrediction, fb: &Feedback) -> f32 {
    let count = fb.count.max(1) as f32;
    cp.adjustments
        .iter()
        .map(|(at, d)| {
            // Subtract in integers. Past 2^24 both ticks round to the same `f32`, so a
            // one-tick gap would read as zero and the adjustment would never wear off.
            let delta = i64::from(fb.acked) - i64::from(*at);
            let frac = (delta as f32 / count).clamp(0.0, 1.0);
            *d as f32 * (1.0 - frac)
        })
        .sum()
}

/// Signed distance from `old` to `new` that fits in `i32`. Narrowing each `u32` tick before
/// subtracting overflows when the two straddle `2^31` (a debug panic, a wrapped sign in
/// release) even when the mathematical delta is +1 or -1.
fn tick_delta_i32(new: u32, old: u32) -> i32 {
    let delta = i64::from(new) - i64::from(old);
    if delta > i64::from(i32::MAX) {
        i32::MAX
    } else if delta < i64::from(i32::MIN) {
        i32::MIN
    } else {
        delta as i32
    }
}

/// § 5.1 and spec 120 § 9.3: steers the client tick so the server's buffer margin sits on its
/// target. Returns how many predicted ticks this fixed step runs.
///
/// Dilation is a time scale: while the margin is off by a tick or more, the client's continuous
/// time advances `1 -+ max_dilation` ticks per fixed step, and the step runs the ticks that start
/// before its end (0, 1 or 2 at a dilation under 50 %). The predicted pawns are drawn at the
/// continuous time (`put_dilation_offsets`), so they move `1 -+ max_dilation` ticks' worth each
/// frame, not 0 or 2 (the P2 row). With the margin on target the continuous time catches up with
/// the tick at the same rate, without running an extra tick.
fn clock_control(world: &mut World, reg: &PredictionRegistry, s: &PredictionSettings) -> u32 {
    let mut cp = world.resource_mut::<ClientPrediction>();
    if let Some(k) = cp.ticks_per_step {
        cp.lag = 0.0;
        return k;
    }
    let Some(fb) = cp.feedback else {
        return advance(&mut cp, 0.0, s);
    };
    // Adjustments the report already reflects in full are forgotten.
    let count = fb.count.max(1);
    cp.adjustments
        .retain(|(at, _)| fb.acked < at.saturating_add(count));
    let target = cp.margin_target(s);
    // The margin this lead produces once the server's average holds only commands sent after
    // every adjustment so far: the reported average minus its stale part.
    let expected = fb.margin + adjustment_pending(&cp, &fb);
    let error = expected - target;
    let fresh = std::mem::take(&mut cp.feedback_fresh);
    if fresh {
        // A report whose negative average is only the part an adjustment already made up for is
        // not a negative margin (ADR-4915).
        if expected < 0.0 {
            cp.negative_run += 1;
        } else {
            cp.negative_run = 0;
        }
    }
    let window = s.dilation_window_ticks as f32;
    if fresh && (error.abs() > window || cp.negative_run >= NEGATIVE_MARGIN_RESYNC) {
        let jump = -error.round() as i64;
        resync(world, reg, jump);
        world.resource_mut::<ClientPrediction>().lag = 0.0;
        return 1;
    }
    let rate = if error >= 1.0 {
        -1.0
    } else if error <= -1.0 {
        1.0
    } else {
        0.0
    };
    advance(&mut cp, rate, s)
}

/// Advances the client's continuous time by one fixed step at `rate` (-1 slows by
/// `max_dilation`, +1 speeds up by it, 0 runs at the server's rate and lets the continuous time
/// catch up with the tick) and returns the ticks that start in the step. Each inserted or skipped
/// tick is an adjustment of the lead (`stats.dilated`).
fn advance(cp: &mut ClientPrediction, rate: f64, s: &PredictionSettings) -> u32 {
    let max = if s.max_dilation.is_finite() {
        s.max_dilation.clamp(0.0, MAX_DILATION)
    } else {
        0.0
    };
    let lag = cp.lag.clamp(0.0, 1.0);
    let step = if rate == 0.0 {
        1.0 + lag.min(max)
    } else {
        1.0 + rate * max
    };
    // The continuous time after the step, relative to the current tick: `step - lag`. The ticks
    // whose start it reaches run now; a remainder within rounding of a tick start counts as reached.
    let ahead = step - lag;
    let n = (ahead - DILATION_EPSILON).ceil().max(0.0);
    cp.lag = (n - ahead).clamp(0.0, 1.0 - DILATION_EPSILON);
    let n = n as u32;
    let at = cp.client_tick;
    if n == 0 {
        cp.adjustments.push_back((at, -1));
        cp.stats.dilated -= 1;
    } else if n >= 2 {
        let extra = (n - 1) as i32;
        cp.adjustments.push_back((at, extra));
        cp.stats.dilated += i64::from(extra);
    }
    n
}

/// The largest `max_dilation` the clock honors: under 50 % a fixed step runs 0, 1 or 2 ticks.
const MAX_DILATION: f64 = 0.5;

/// Rounding slack of the continuous clock, in ticks.
const DILATION_EPSILON: f64 = 1e-9;

fn resync(world: &mut World, reg: &PredictionRegistry, jump: i64) {
    let (old, new) = {
        let mut cp = world.resource_mut::<ClientPrediction>();
        let old = cp.client_tick;
        let floor = cp.newest_server.unwrap_or(0);
        let new = (i64::from(old) + jump).clamp(i64::from(floor), i64::from(u32::MAX - 1)) as u32;
        cp.stats.resyncs += 1;
        cp.negative_run = 0;
        cp.lag = 0.0;
        if new < old {
            // Ticks jumped backward are not sent again: the stream holds until it passes them.
            cp.hold_through = cp.max_sent.max(cp.hold_through);
            for h in cp.history.values_mut() {
                h.ring.retain(|(t, _)| *t <= new);
            }
        }
        // The server's margin reflects the jump from the first command sent after it: forward,
        // the newest tick goes straight to `new`; backward, the stream resumes after the hold.
        // Keyed at `old`, a forward jump counted as `jump` samples already reflected and the
        // rest of the stale average read as a fresh error (ADR-4915).
        let edge = if new > old {
            new
        } else {
            cp.hold_through.map_or(old, |h| h.max(old))
        };
        cp.adjustments.push_back((edge, tick_delta_i32(new, old)));
        cp.client_tick = new;
        (old, new)
    };
    if new < old
        && let Some(input) = reg.input
    {
        (input.client_truncate)(world, new);
    }
    tracing::info!(target: "net", from = old, to = new, "net.lead_resync");
}

fn run_client_tick(
    world: &mut World,
    reg: &PredictionRegistry,
    tick: u32,
    resim: bool,
    s: &PredictionSettings,
) {
    set_state(world, tick, resim, false);
    if let Some(input) = reg.input {
        if resim {
            (input.restore)(world, tick);
        } else {
            let view = client_view_tick(world);
            (input.capture)(world, tick, view);
        }
    }
    apply_tick_driven_motion(world, NetTick(tick));
    // The first system of the schedule writes at the world's current change tick, and
    // `is_changed(before, ..)` only counts ticks newer than `before`: the window starts one tick
    // earlier so that system's writes count, while the writes above (input, movers) stay outside
    // it (ADR-5649). Only the debug checker reads it.
    let before = if cfg!(debug_assertions) {
        world.increment_change_tick()
    } else {
        world.change_tick()
    };
    if !resim && s.verify_resim {
        verify_tick(world, reg, tick);
    } else {
        run_predicted_schedule(world);
    }
    if cfg!(debug_assertions) {
        check_unregistered_writes(world, reg, before, resim);
    }
    if resim {
        // A predicted spawn made after the rollback tick starts again from its first recorded state.
        let restore: Vec<(Entity, Vec<Option<Snap>>)> = {
            let spawns = world.resource::<PredictedSpawns>();
            let cp = world.resource::<ClientPrediction>();
            spawns
                .first_tick
                .iter()
                .filter(|(_, f)| **f == tick)
                .filter_map(|(e, _)| {
                    cp.history
                        .get(e)
                        .and_then(|h| h.ring.iter().find(|(t, _)| *t == tick))
                        .map(|(_, v)| (*e, v.clone()))
                })
                .collect()
        };
        for (e, v) in restore {
            write_all(world, reg, e, &v);
        }
    }
    record_history(world, reg, tick, s);
    if !resim
        && s.trust_client_within_m.is_some()
        && let Some(input) = reg.input
        && let Some(e) = reported_entity(world)
        && let Some(t) = world.get::<Transform>(e).copied()
    {
        (input.record_pos)(world, tick, t.translation);
    }
    if resim {
        set_state(world, tick, false, false);
        world.resource_mut::<PredictionState>().first_time = false;
    }
}

/// `prediction.verify_resim`: runs the tick twice from the same start and compares.
fn verify_tick(world: &mut World, reg: &PredictionRegistry, tick: u32) {
    let list = predicted_entities(world);
    let start: Vec<(Entity, Vec<Option<Snap>>)> = list
        .iter()
        .map(|(e, _)| (*e, snapshot_all(world, reg, *e)))
        .collect();
    run_predicted_schedule(world);
    let first: Vec<Vec<Option<Snap>>> = list.iter().map(|(e, _)| snapshot_all(world, reg, *e)).collect();
    for (e, v) in &start {
        write_all(world, reg, *e, v);
    }
    set_state(world, tick, true, false);
    if let Some(input) = reg.input {
        (input.restore)(world, tick);
    }
    run_predicted_schedule(world);
    set_state(world, tick, false, false);
    let same = list
        .iter()
        .zip(&first)
        .all(|((e, _), a)| values_match(reg, a, &snapshot_all(world, reg, *e)));
    if !same {
        world.resource_mut::<ClientPrediction>().stats.verify_failures += 1;
        tracing::warn!(target: "net", tick, "net.predict_nondeterministic");
    }
}

/// Debug builds: warns once per component type the predicted schedule wrote on a predicted entity
/// without it being registered with [`predict`] (§ 6.1).
fn check_unregistered_writes(
    world: &mut World,
    reg: &PredictionRegistry,
    before: regia_ecs::ecs::change_detection::Tick,
    resim: bool,
) {
    // A resim replays ticks that already ran. The warning is for the live schedule.
    if resim {
        return;
    }
    let this = world.read_change_tick();
    let registered: HashSet<ComponentId> = reg
        .comps
        .iter()
        .filter_map(|c| c.id)
        .chain(reg.local_writes.iter().copied())
        .collect();
    let skip = world.resource::<ClientPrediction>().name_skip.clone();
    let warned = world.resource::<ClientPrediction>().warned_types.clone();
    let mut found: Vec<(ComponentId, String)> = Vec::new();
    let mut built = 0u64;
    let mut skip_now: Vec<ComponentId> = Vec::new();
    for (e, _) in predicted_entities(world) {
        let Ok(infos) = world.inspect_entity(e) else {
            continue;
        };
        let ids: Vec<ComponentId> = infos
            .filter(|(id, _)| !registered.contains(id) && !skip.contains(id) && !warned.contains(id))
            .map(|(id, _)| id)
            .collect();
        let Ok(er) = world.get_entity(e) else {
            continue;
        };
        let changed: Vec<ComponentId> = ids
            .into_iter()
            .filter(|id| {
                er.get_change_ticks_by_id(*id)
                    .is_some_and(|t| t.is_changed(before, this) && !t.is_added(before, this))
            })
            .collect();
        if changed.is_empty() {
            continue;
        }
        let Ok(infos) = world.inspect_entity(e) else {
            continue;
        };
        for (id, info) in infos {
            if !changed.contains(&id) {
                continue;
            }
            built += 1;
            let name = info.name().to_string();
            if name.contains("InputCommand") || name.contains("NetVisualOffset") {
                skip_now.push(id);
                continue;
            }
            found.push((id, name));
        }
    }
    let mut cp = world.resource_mut::<ClientPrediction>();
    cp.name_builds += built;
    cp.name_skip.extend(skip_now);
    for (id, name) in found {
        if cp.warned_types.insert(id) {
            tracing::warn!(target: "net", component = %name, "net.predict_writes_unregistered");
        }
    }
}

struct ReusedHistory {
    entity: Entity,
    snaps: Option<Vec<Option<Snap>>>,
    ticks: Vec<Option<u32>>,
}

struct FreshHistory {
    entity: Entity,
    snaps: Vec<Option<Snap>>,
    ticks: Vec<Option<u32>>,
}

fn record_history(world: &mut World, reg: &PredictionRegistry, tick: u32, s: &PredictionSettings) {
    let mut list = predicted_entities(world);
    // A re-run before a predicted spawn's own tick records nothing for it: it did not exist yet,
    // and its spawn-tick record must survive for the restore (ADR-1793 (7), ADR-5649).
    {
        let spawns = world.resource::<PredictedSpawns>();
        list.retain(|(e, id)| id.is_some() || spawns.first_tick.get(e).is_none_or(|f| *f <= tick));
    }
    let cap = ClientPrediction::history_cap(s) as usize;
    let prevs: Vec<ReusedHistory> = {
        let cp = world.resource::<ClientPrediction>();
        list.iter()
            .map(|(e, _)| {
                let h = cp.history.get(e);
                let reusable = h.and_then(|h| h.ring.back()).filter(|(t, _)| *t < tick);
                let snaps = reusable.map(|(_, v)| v.clone());
                let ticks = if reusable.is_some() {
                    h.map(|h| h.snap_ticks.clone()).unwrap_or_default()
                } else {
                    Vec::new()
                };
                ReusedHistory {
                    entity: *e,
                    snaps,
                    ticks,
                }
            })
            .collect()
    };
    let values: Vec<FreshHistory> = prevs
        .iter()
        .map(|prev| {
            let (snaps, ticks) = snaps_reused(world, reg, prev.entity, prev.snaps.as_deref(), &prev.ticks);
            FreshHistory {
                entity: prev.entity,
                snaps,
                ticks,
            }
        })
        .collect();
    let mut cp = world.resource_mut::<ClientPrediction>();
    for row in values {
        let h = cp.history.entry(row.entity).or_default();
        h.ring.retain(|(t, _)| *t < tick);
        h.ring.push_back((tick, row.snaps));
        while h.ring.len() > cap {
            h.ring.pop_front();
        }
        h.snap_ticks = row.ticks;
    }
}

/// Decays each correction offset; a zero offset loses its component unless its entity is in
/// `keep` (a dilation part is about to be added to it).
fn decay_offsets(world: &mut World, s: &PredictionSettings, keep: &HashMap<Entity, (Vec3, Quat)>) {
    // Bit-stable decay: `2^-x` by a fixed polynomial instead of libm `powf`, and a scalar
    // nlerp instead of glam's ISA-specific `Quat::slerp` (ADR-4770).
    let f = if s.smooth_half_life_ms > 0.0 {
        stable_pow2(-((step_ms_of(world) / s.smooth_half_life_ms) as f32))
    } else {
        0.0
    };
    let mut clear: Vec<Entity> = Vec::new();
    let mut q = world.query::<(Entity, &mut NetVisualOffset)>();
    for (e, mut o) in q.iter_mut(world) {
        if o.is_zero() {
            if !keep.contains_key(&e) {
                clear.push(e);
            }
            continue;
        }
        let t = o.translation * f;
        let r = quat_nlerp(Quat::IDENTITY, o.rotation, f);
        if t.length() < 1e-6 && quat_rot_chord2(r, Quat::IDENTITY) < angle_chord2(1e-6) {
            *o = NetVisualOffset::default();
            if !keep.contains_key(&e) {
                clear.push(e);
            }
        } else {
            o.translation = t;
            o.rotation = r;
        }
    }
    for e in clear {
        if let Ok(mut ent) = world.get_entity_mut(e) {
            ent.remove::<NetVisualOffset>();
        }
    }
}

/// Removes last frame's dilation part from each [`NetVisualOffset`], leaving the correction part
/// (which `decay_offsets` then decays). The offset composes as correction then dilation:
/// translation `c + d`, rotation `c * d`.
fn take_dilation_offsets(world: &mut World) {
    let parts = std::mem::take(&mut world.resource_mut::<ClientPrediction>().dilation_offsets);
    for (e, (t, r)) in parts {
        if let Some(mut o) = world.get_mut::<NetVisualOffset>(e) {
            o.translation -= t;
            o.rotation = quat_normalize(quat_mul(o.rotation, r.conjugate()));
        }
    }
}

/// Spec 120 § 9.3: the dilation part of each predicted entity's offset, which draws it at the
/// client's continuous time, `lag` ticks behind its newest predicted tick: the pose between the
/// last two predicted ticks (translation lerp, rotation nlerp). Empty when the clock has no lag.
fn dilation_parts(world: &World, reg: &PredictionRegistry) -> HashMap<Entity, (Vec3, Quat)> {
    let cp = world.resource::<ClientPrediction>();
    let (lag, tick) = (cp.lag, cp.client_tick);
    if lag <= 0.0 || tick == 0 {
        return HashMap::new();
    }
    let f = lag as f32;
    cp.history
        .iter()
        .filter_map(|(e, h)| {
            let mut back = h.ring.iter().rev();
            let (t1, v1) = back.next()?;
            let (t0, v0) = back.next()?;
            if *t1 != tick || *t0 != tick - 1 {
                return None;
            }
            let (p1, q1) = pose(reg, v1)?;
            let (p0, q0) = pose(reg, v0)?;
            let t = (p0 - p1) * f;
            let r = quat_mul(quat_nlerp(q1, q0, f), q1.conjugate());
            // The thresholds `decay_offsets` clears an offset under: a still entity gets none.
            let moved = t.length() >= 1e-6 || quat_rot_chord2(r, Quat::IDENTITY) >= angle_chord2(1e-6);
            (moved && t.is_finite()).then_some((*e, (t, r)))
        })
        .collect()
}

/// Adds each dilation part to its entity's [`NetVisualOffset`] and remembers it for
/// `take_dilation_offsets`.
fn put_dilation_offsets(world: &mut World, parts: HashMap<Entity, (Vec3, Quat)>) {
    let mut kept: HashMap<Entity, (Vec3, Quat)> = HashMap::with_capacity(parts.len());
    for (e, (t, r)) in parts {
        let Ok(mut ent) = world.get_entity_mut(e) else {
            continue;
        };
        match ent.get_mut::<NetVisualOffset>() {
            Some(mut o) => {
                o.translation += t;
                o.rotation = quat_normalize(quat_mul(o.rotation, r));
            }
            None => {
                ent.insert(NetVisualOffset {
                    translation: t,
                    rotation: quat_normalize(r),
                });
            }
        }
        kept.insert(e, (t, r));
    }
    world.resource_mut::<ClientPrediction>().dilation_offsets = kept;
}

fn apply_visual_children(world: &mut World) {
    let has_child = world.query::<&NetVisualChild>().iter(world).next().is_some();
    if !has_child {
        return;
    }
    let mut parents = world.query::<(Entity, &Transform, &NetVisualOffset)>();
    let offsets: HashMap<Entity, (Quat, NetVisualOffset)> = parents
        .iter(world)
        .map(|(e, t, o)| (e, (t.rotation, *o)))
        .collect();
    let mut q = world.query::<(&ChildOf, &NetVisualChild, &mut Transform)>();
    for (parent, child, mut t) in q.iter_mut(world) {
        let (rot, o) = offsets
            .get(&parent.0)
            .copied()
            .unwrap_or((Quat::IDENTITY, NetVisualOffset::default()));
        // Bit-stable scalar compose; glam's `Quat * Quat` / `Quat * Vec3` fuse and sum
        // differently per ISA (ADR-4770).
        let inv = rot.conjugate();
        let mut local = child.base;
        local.translation += quat_rotate_vec3(inv, o.translation);
        local.rotation = quat_normalize(quat_mul(
            quat_mul(quat_mul(inv, o.rotation), rot),
            child.base.rotation,
        ));
        if *t != local {
            *t = local;
        }
    }
}

fn reject_spawns(world: &mut World, s: &PredictionSettings) {
    let acked = world.resource::<ClientPrediction>().feedback.map(|f| f.acked);
    let client_tick = world.resource::<ClientPrediction>().client_tick;
    let entries: Vec<(PredictionKey, Entity)> = world
        .resource::<PredictedSpawns>()
        .map
        .iter()
        .map(|(k, e)| (*k, *e))
        .collect();
    for (key, e) in entries {
        let alive = world.get_entity(e).is_ok();
        let unbound = alive && world.get::<NetPredictedSpawn>(e).is_some();
        let expired_bound = !unbound && key.tick.0.saturating_add(s.history_ticks) < client_tick;
        if !alive || expired_bound {
            let mut sp = world.resource_mut::<PredictedSpawns>();
            sp.map.remove(&key);
            sp.first_tick.remove(&e);
            continue;
        }
        if unbound && acked.is_some_and(|a| a >= key.tick.0.saturating_add(s.spawn_grace_ticks)) {
            let _ = world.despawn(e);
            {
                let mut sp = world.resource_mut::<PredictedSpawns>();
                sp.map.remove(&key);
                sp.first_tick.remove(&e);
            }
            world.resource_mut::<ClientPrediction>().history.remove(&e);
            world.resource_mut::<ClientPrediction>().stats.rejected_spawns += 1;
            world
                .resource_mut::<PredictionEvents>()
                .rejected
                .push(NetPredictedSpawnRejected { entity: e, key });
            tracing::debug!(target: "net", tick = key.tick.0, index = key.index, "net.predicted_spawn_rejected");
        }
    }
}

fn update_clock(world: &mut World) {
    let (client_tick, newest, rtt_ticks, rtt_ms, jitter_ms) = {
        let cp = world.resource::<ClientPrediction>();
        (
            cp.client_tick,
            cp.newest_server,
            cp.rtt_ticks(),
            cp.rtt_ms.unwrap_or(0.0),
            cp.jitter_ms,
        )
    };
    let interp = world
        .get_resource::<NetInterpolation>()
        .map(|ni| (ni.clock.server_tick_estimate(ni.now_ms), ni.clock.interp_tick()));
    let estimate = interp
        .and_then(|(e, _)| e)
        .unwrap_or_else(|| f64::from(newest.unwrap_or(0)) + rtt_ticks / 2.0);
    let interp_tick = interp.and_then(|(_, i)| i).unwrap_or(0.0);
    *world.resource_mut::<NetClock>() = NetClock {
        client_tick: NetTick(client_tick),
        server_tick_estimate: NetTickFrac(estimate),
        interp_tick: NetTickFrac(interp_tick),
        lead: (f64::from(client_tick) - estimate).round() as i32,
        rtt_ms,
        jitter_ms,
    };
}

/// Client, before queued server ticks apply: predicted components go back to the newest server
/// values, so field deltas decode against what the server sent. The predicted values are kept.
pub(crate) fn client_before_apply(world: &mut World, has_frames: bool) {
    refresh_step(world);
    if !has_frames {
        return;
    }
    let Some(reg) = active(world) else {
        return;
    };
    let list = predicted_entities(world);
    for (e, id) in list {
        let (prev, ticks) = {
            let cp = world.resource::<ClientPrediction>();
            let h = cp.history.get(&e);
            let prev = h.and_then(|h| h.ring.back().map(|(_, v)| v.clone()));
            let ticks = h.map(|h| h.snap_ticks.clone()).unwrap_or_default();
            (prev, ticks)
        };
        // The fresh ticks describe `h.predicted`, not the newest ring entry: `snap_ticks` is
        // read back by `record_history` paired with `ring.back()`, which this step does not
        // change. Storing them here let a component changed outside the predicted schedule match
        // the pre-apply tick, and the next record reused the ring's older snap (corr-low,
        // ADR-5789).
        let (predicted, _predicted_ticks) = snaps_reused(world, &reg, e, prev.as_deref(), &ticks);
        let server = {
            let mut cp = world.resource_mut::<ClientPrediction>();
            let h = cp.history.entry(e).or_default();
            h.predicted = Some(predicted);
            h.server.clone()
        };
        // An unbound predicted spawn has no server state yet; its prediction for the client tick
        // is kept too, so a bind this frame comes back to it (§ 7, ADR-5649).
        if id.is_none() {
            continue;
        }
        if let Some(server) = server {
            write_all(world, &reg, e, &server);
        }
    }
}

/// Client, after queued server ticks applied (§ 6.3): binds predicted spawns, then compares the
/// server's state for the newest applied tick with the prediction history, and on a mismatch rolls
/// every predicted entity back to the server's state and re-runs the ticks since.
pub(crate) fn client_after_apply(world: &mut World, has_frames: bool) {
    if !has_frames {
        return;
    }
    let Some(reg) = active(world) else {
        return;
    };
    let Some((server_tick, from_state)) = world
        .get_resource::<crate::replication::ClientReplication>()
        .and_then(|c| c.state_tick().map(|t| (t, c.state.active())))
    else {
        return;
    };
    let _span = tracing::trace_span!("net_reconcile").entered();
    let s = settings(world);
    let tick = server_tick.0;
    // A late state frame brings nothing to compare: it arrived after a newer one. The first state
    // frame of the state channel is not a repeat of the same tick taken from a tick frame: that
    // frame carried no state (the negotiating wire's empty ticks), so the state's new predicted
    // entities still adopt (audit wave 5, fix030b M1). The watermark never moves back.
    let repeated = {
        let cp = world.resource::<ClientPrediction>();
        let opening = from_state && !cp.newest_from_state;
        cp.newest_server
            .is_some_and(|n| tick < n || (tick == n && !opening))
    };
    {
        let mut cp = world.resource_mut::<ClientPrediction>();
        if cp.newest_server.is_none_or(|n| tick >= n) {
            if from_state
                && cp.newest_from_state
                && let Some(n) = cp.newest_server
                && tick > n
            {
                cp.state_spacing = tick - n;
            }
            cp.newest_server = Some(tick);
            cp.newest_from_state = from_state;
        }
    }
    bind_spawns(world, &reg);
    let list = predicted_entities(world);
    let present: HashSet<Entity> = list.iter().map(|(e, _)| *e).collect();
    world
        .resource_mut::<ClientPrediction>()
        .history
        .retain(|e, _| present.contains(e));
    let teleported: HashSet<Entity> = world.resource::<NetEvents>().teleported.iter().copied().collect();
    // The server values of tick S, now in the world.
    for (e, id) in &list {
        if id.is_none() {
            continue;
        }
        let server = snapshot_all(world, &reg, *e);
        world
            .resource_mut::<ClientPrediction>()
            .history
            .entry(*e)
            .or_default()
            .server = Some(server);
    }
    if repeated {
        for (e, id) in &list {
            if id.is_none() {
                continue;
            }
            let predicted = world
                .resource_mut::<ClientPrediction>()
                .history
                .entry(*e)
                .or_default()
                .predicted
                .take();
            if let Some(p) = predicted {
                write_all(world, &reg, *e, &p);
            }
        }
        return;
    }
    if let Some(input) = reg.input {
        // The command of S itself stays: a missing later command repeats it.
        (input.client_prune)(world, tick.saturating_sub(1));
    }
    if !world.resource::<ClientPrediction>().started {
        start_clock(world, &reg, tick, &s);
        return;
    }
    let client_tick = world.resource::<ClientPrediction>().client_tick;
    // Per-entity reconcile ticks (ADR-2413): on the state channel a tick's state can come in
    // several frames, and an entity whose record was lost still holds an older server state.
    let deferred = if from_state {
        deferred_at(world, &list, tick)
    } else {
        HashSet::new()
    };
    if !deferred.is_empty() {
        world.resource_mut::<ClientPrediction>().stats.deferred += deferred.len() as u64;
        tracing::debug!(target: "net", tick, entities = deferred.len(), "net.prediction_deferred");
    }
    let mut mismatch = false;
    let mut reset = tick > client_tick;
    // A predicted entity on its first server state (spawned or made predicted since the last
    // reconcile) has no history because it is new, not because S left the ring (§ 6.3 step 1):
    // it catches up from S to the client tick with the others, and that is not a reset.
    let mut adopt = false;
    let mut max_err = 0.0f32;
    let mut teleport_now: Vec<Entity> = Vec::new();
    {
        let cp = world.resource::<ClientPrediction>();
        for (e, id) in &list {
            if id.is_none() || deferred.contains(e) {
                continue;
            }
            if teleported.contains(e) {
                teleport_now.push(*e);
                continue;
            }
            let Some(h) = cp.history.get(e) else {
                adopt = true;
                continue;
            };
            let Some(server) = &h.server else {
                continue;
            };
            match h.ring.iter().find(|(t, _)| *t == tick) {
                Some((_, predicted)) => {
                    if !values_match(&reg, predicted, server) {
                        mismatch = true;
                        if let Some(e) = error_m(&reg, predicted, server) {
                            max_err = max_err.max(e);
                        }
                    }
                }
                // Never predicted and never confirmed: new to prediction.
                None if h.ring.is_empty() && h.confirmed_through.is_none() => adopt = true,
                None => reset = true,
            }
        }
    }
    if adopt {
        tracing::debug!(target: "net", tick, client_tick, "net.prediction_adopted");
    }
    if !mismatch && !reset && !adopt && teleport_now.is_empty() {
        // The common case: no correction. Predicted values come back; history at or below S goes.
        for (e, id) in &list {
            if id.is_none() {
                continue;
            }
            let predicted = {
                let mut cp = world.resource_mut::<ClientPrediction>();
                let h = cp.history.entry(*e).or_default();
                if !deferred.contains(e) {
                    h.confirmed_through = Some(tick);
                    h.reconciled_at = Some(tick);
                }
                h.ring.retain(|(t, _)| *t > tick);
                h.predicted.take()
            };
            if let Some(p) = predicted {
                write_all(world, &reg, *e, &p);
            }
        }
        return;
    }
    rollback(
        world,
        &reg,
        &list,
        tick,
        client_tick,
        &teleport_now,
        &deferred,
        mismatch,
        reset,
        max_err,
        &s,
    );
}

#[allow(clippy::too_many_arguments)]
fn rollback(
    world: &mut World,
    reg: &PredictionRegistry,
    list: &[(Entity, Option<NetId>)],
    tick: u32,
    client_tick: u32,
    teleported: &[Entity],
    deferred: &HashSet<Entity>,
    mismatch: bool,
    reset: bool,
    max_err: f32,
    s: &PredictionSettings,
) {
    let _span = tracing::trace_span!("net_resim").entered();
    // Poses before the correction (the prediction for the client tick).
    let pre: HashMap<Entity, (Vec3, Quat)> = {
        let cp = world.resource::<ClientPrediction>();
        list.iter()
            .filter_map(|(e, _)| {
                cp.history
                    .get(e)
                    .and_then(|h| h.predicted.as_ref())
                    .and_then(|p| pose(reg, p))
                    .map(|p| (*e, p))
            })
            .collect()
    };
    let need = client_tick.saturating_sub(tick);
    // Spec 120 § 9.4 (ADR-8219): over the RTT-scaled depth the client still moves to the server's
    // state and re-runs to the client tick, so prediction stays on; only a rollback the input
    // history cannot replay (or a server state ahead of the client tick) clears the history and
    // snaps, as every over-cap rollback did before.
    let cleared = tick > client_tick || need > INPUT_HISTORY_TICKS;
    let over_cap = cleared || need > world.resource::<ClientPrediction>().resim_depth(s);
    {
        let mut cp = world.resource_mut::<ClientPrediction>();
        cp.stats.last_rollback_entities = list.len() as u32;
        cp.stats.teleports += teleported.len() as u64;
        if mismatch && !over_cap {
            cp.stats.corrections += 1;
            cp.stats.max_error_m = cp.stats.max_error_m.max(max_err);
            let now = cp.steps;
            cp.corrections_at.push_back(now);
        }
        if (reset && !mismatch) || over_cap {
            cp.stats.resets += 1;
        }
    }
    // Every predicted entity moves back to S together; unbound predicted spawns to their own history.
    let unbound: Vec<(Entity, Option<Vec<Option<Snap>>>)> = {
        let cp = world.resource::<ClientPrediction>();
        list.iter()
            .filter(|(_, id)| id.is_none())
            .map(|(e, _)| {
                (
                    *e,
                    cp.history
                        .get(e)
                        .and_then(|h| h.ring.iter().find(|(t, _)| *t == tick).map(|(_, v)| v.clone())),
                )
            })
            .collect()
    };
    for (e, v) in unbound {
        if let Some(v) = v {
            write_all(world, reg, e, &v);
        }
    }
    // An entity left out of this tick (its state at S is not here) moves back to its own
    // prediction for S, not to its older server state (ADR-2413).
    let held: Vec<(Entity, Vec<Option<Snap>>)> = {
        let cp = world.resource::<ClientPrediction>();
        list.iter()
            .filter(|(e, _)| deferred.contains(e))
            .filter_map(|(e, _)| {
                cp.history.get(e).and_then(|h| {
                    h.ring
                        .iter()
                        .find(|(t, _)| *t == tick)
                        .map(|(_, v)| (*e, v.clone()))
                })
            })
            .collect()
    };
    for (e, v) in &held {
        write_all(world, reg, *e, v);
    }
    // An unbound spawn made after S keeps the record of its spawn tick: the re-run restores it
    // there (`run_client_tick`, ADR-1793 (7)). Without it the entity kept its client-tick values
    // and the re-run moved it once more over every re-run tick (ADR-5649).
    let spawn_ticks: HashMap<Entity, u32> = {
        let spawns = world.resource::<PredictedSpawns>();
        list.iter()
            .filter(|(_, id)| id.is_none())
            .filter_map(|(e, _)| spawns.first_tick.get(e).map(|f| (*e, *f)))
            .filter(|(_, f)| *f > tick)
            .collect()
    };
    {
        let mut cp = world.resource_mut::<ClientPrediction>();
        for (e, id) in list {
            let Some(h) = cp.history.get_mut(e) else {
                continue;
            };
            h.predicted = None;
            if cleared {
                h.ring.clear();
                h.snap_ticks.clear();
            } else if deferred.contains(e) {
                h.ring.retain(|(t, _)| *t <= tick);
            } else if id.is_some() {
                h.ring.retain(|(t, _)| *t < tick);
                if let Some(server) = h.server.clone() {
                    h.ring.push_back((tick, server));
                }
                h.confirmed_through = Some(tick);
                h.reconciled_at = Some(tick);
            } else {
                let spawned = spawn_ticks.get(e).copied();
                h.ring.retain(|(t, _)| *t <= tick || Some(*t) == spawned);
            }
        }
    }
    if !cleared {
        for t in tick.saturating_add(1)..=client_tick {
            run_client_tick(world, reg, t, true, s);
        }
        world.resource_mut::<ClientPrediction>().stats.resim_ticks_last = need;
    } else {
        world.resource_mut::<ClientPrediction>().stats.resim_ticks_last = 0;
    }
    if mismatch || reset || over_cap {
        tracing::debug!(target: "net", tick, resim = need, error_m = max_err, over_cap, "net.prediction_correction");
    }
    // The rendered pose keeps the pre-correction pose and decays to the corrected one. A rollback
    // over the depth re-ran like any correction, so it smooths too; only a teleport, a jump over
    // the snap threshold or a cleared history snaps (spec 120 § 9.4).
    let snap_rot = std::f32::consts::FRAC_PI_2;
    let mut largest = 0.0f32;
    let mut snapped = false;
    for (e, _) in list {
        let Some((pt, pr)) = pre.get(e).copied() else {
            continue;
        };
        let Some(post) = world.get::<Transform>(*e).copied() else {
            continue;
        };
        let jump = pt - post.translation;
        largest = largest.max(jump.length());
        let mut o = world.get::<NetVisualOffset>(*e).copied().unwrap_or_default();
        o.translation += jump;
        o.rotation = quat_normalize(quat_mul(quat_mul(pr, post.rotation.conjugate()), o.rotation));
        let snap = cleared
            || teleported.contains(e)
            || o.translation.length() > s.snap_distance_m
            || quat_rot_chord2(o.rotation, Quat::IDENTITY) > angle_chord2(snap_rot);
        if snap {
            snapped |= !o.is_zero();
            o = NetVisualOffset::default();
        }
        if let Ok(mut ent) = world.get_entity_mut(*e) {
            ent.insert(o);
        }
    }
    let mut cp = world.resource_mut::<ClientPrediction>();
    cp.stats.last_correction_m = largest;
    if snapped {
        cp.stats.snaps += 1;
    }
}

fn start_clock(world: &mut World, reg: &PredictionRegistry, tick: u32, s: &PredictionSettings) {
    let start = {
        let mut cp = world.resource_mut::<ClientPrediction>();
        // Spec 120 § 9.2: the first lead comes from a measured round trip. A connected transport
        // reports its first sample about one round trip after the connect; until then (at most
        // `START_WAIT_FOR_RTT_STEPS`) the clock waits instead of starting short and resyncing.
        if cp.transport_expected
            && cp.transport_rtt.is_none()
            && cp.rtt_ms.is_none()
            && cp.start_wait_steps < START_WAIT_FOR_RTT_STEPS
        {
            return;
        }
        if let Some((rtt, jitter)) = cp.transport_rtt {
            cp.rtt_ms = Some(rtt);
            cp.jitter_ms = jitter;
        }
        let target = cp.margin_target(s) as i64;
        let lead = match cp.rtt_ms {
            Some(_) => cp.rtt_ticks().ceil() as i64 + target,
            None => target + 8,
        } + i64::from(cp.start_offset_ticks);
        let start = (i64::from(tick) + lead.max(0)).clamp(0, i64::from(u32::MAX - 1)) as u32;
        cp.started = true;
        cp.client_tick = start;
        cp.lag = 0.0;
        for h in cp.history.values_mut() {
            h.predicted = None;
            h.ring.clear();
            h.snap_ticks.clear();
        }
        start
    };
    tracing::info!(target: "net", server_tick = tick, client_tick = start, "net.prediction_started");
    // The predicted entities advance from S to the client tick with the inputs known (none yet:
    // `on_missing` of the default, as the server does).
    let list = predicted_entities(world);
    let need = start.saturating_sub(tick);
    // Up to the input history, like a rollback (spec 120 § 9.4): a high-RTT session starts with
    // its predicted entities at the client tick, not a lead behind it.
    if !list.is_empty() && need <= INPUT_HISTORY_TICKS {
        for t in tick.saturating_add(1)..=start {
            run_client_tick(world, reg, t, true, s);
        }
    }
    update_clock(world);
}

fn bind_spawns(world: &mut World, reg: &PredictionRegistry) {
    let spawned: Vec<Entity> = world
        .resource::<NetEvents>()
        .spawned
        .iter()
        .map(|s| s.entity)
        .collect();
    for copy in spawned {
        let Some(key) = world.get::<NetPredictionKey>(copy).map(|k| k.0) else {
            continue;
        };
        let Some(local) = world.resource::<PredictedSpawns>().entity(&key) else {
            continue;
        };
        if local == copy || world.get::<NetPredictedSpawn>(local).is_none() {
            continue;
        }
        if !crate::replication::rebind_copy(world, copy, local) {
            continue;
        }
        if let Ok(mut ent) = world.get_entity_mut(local) {
            ent.remove::<NetPredictedSpawn>();
        }
        world.resource_mut::<PredictedSpawns>().first_tick.remove(&local);
        // The bound entity now holds the server's state for S, a lead behind its prediction. The
        // reconcile below compares it with the history at S like any predicted entity: a right
        // prediction comes back to its client-tick values (kept by `client_before_apply`), a wrong
        // one rolls back and re-runs, and that rollback sets the visual offset (§ 6.4). An offset
        // from the S state here would leave the entity a lead behind (ADR-5649).
        world.resource_mut::<ClientPrediction>().stats.bound_spawns += 1;
        world.resource_mut::<PredictionEvents>().bound.push((local, key));
        let _ = reg;
    }
}

/// Client (`NetSet::Send`): the input frame of this tick, when prediction runs.
pub(crate) fn client_input_frame(world: &mut World) -> Option<Vec<u8>> {
    let reg = active(world)?;
    let input = reg.input?;
    if !world.resource::<ClientPrediction>().started {
        return None;
    }
    let s = settings(world);
    let tick = world.resource::<ClientPrediction>().client_tick;
    let trust = s.trust_client_within_m.is_some();
    let frame = (input.write_frame)(world, tick, &s, trust)?;
    let now = world.resource::<NetNode>().local_ticks;
    let mut cp = world.resource_mut::<ClientPrediction>();
    cp.max_sent = Some(cp.max_sent.map_or(tick, |m| m.max(tick)));
    cp.sent_at.entry(tick).or_insert(now);
    // Bounded without acknowledgments too (a server that never acks, a long outage): a send time
    // older than the input history can no longer be matched to a useful round trip.
    let keep_from = tick.saturating_sub(INPUT_HISTORY_TICKS);
    while cp.sent_at.first_key_value().is_some_and(|(t, _)| *t < keep_from) {
        cp.sent_at.pop_first();
    }
    Some(frame)
}

/// Server (`NetSet::Receive`): an input frame from a client.
pub(crate) fn server_read_input(
    world: &mut World,
    peer: PeerId,
    r: &mut BitReader,
) -> Result<(), DecodeError> {
    let _span = tracing::trace_span!("net_input").entered();
    let Some(input) = active(world).and_then(|r| r.input) else {
        return Err(DecodeError::Invalid("no input type registered"));
    };
    let s = settings(world);
    let tick = world.resource::<NetNode>().tick.0;
    (input.server_read)(world, peer, r, tick, &s)
}

/// Server (`NetSet::Apply`): consumes one command per player for the tick being simulated.
pub(crate) fn server_consume(world: &mut World) {
    let Some(input) = active(world).and_then(|r| r.input) else {
        return;
    };
    let _span = tracing::trace_span!("net_input").entered();
    let tick = world.resource::<NetNode>().tick.0;
    (input.server_consume)(world, tick);
}

/// Server: a peer left.
pub(crate) fn server_remove_peer(world: &mut World, peer: PeerId) {
    if let Some(input) = active(world).and_then(|r| r.input) {
        (input.server_remove_peer)(world, peer);
    }
    if let Some(mut sp) = world.get_resource_mut::<ServerPrediction>() {
        sp.players.retain(|p, _| p.peer != peer);
    }
    if let Some(mut w) = world.get_resource_mut::<crate::lagcomp::LagCompWindows>() {
        w.remove_peer(peer);
    }
}

/// Server (`NetSet::Send`): the input acknowledgment frame per peer: for each of its players the
/// newest input tick received, the averaged buffer margin and the ticks consumed with repeats.
pub(crate) fn server_ack_frames(world: &World, tick: NetTick) -> Vec<(PeerId, Vec<u8>)> {
    let Some(reg) = world.get_resource::<PredictionRegistry>() else {
        return Vec::new();
    };
    let Some(input) = reg.input else {
        return Vec::new();
    };
    let Some(sp) = world.get_resource::<ServerPrediction>() else {
        return Vec::new();
    };
    let mut by_peer: BTreeMap<PeerId, Vec<(NetPlayerId, &PlayerInputStats)>> = BTreeMap::new();
    for (p, st) in &sp.players {
        if st.newest_tick.is_some() {
            by_peer.entry(p.peer).or_default().push((*p, st));
        }
    }
    let from = tick.0.saturating_sub(INPUT_BUFFER_AHEAD);
    let mut out = Vec::with_capacity(by_peer.len());
    for (peer, players) in by_peer {
        let mut w = BitWriter::new();
        w.write_bits(u64::from(TAG_INPUT_ACK), 8);
        w.write_varint(u64::from(tick.0));
        w.write_varint(players.len() as u64);
        for (p, st) in players {
            w.write_bits(u64::from(p.local), 3);
            w.write_varint(u64::from(st.newest_tick.map_or(0, |t| t.0)));
            w.write_zigzag((st.margin * 16.0).round() as i64);
            w.write_varint(st.margins.len() as u64);
            let repeated = (input.server_repeated)(world, p, from);
            w.write_varint(repeated.len() as u64);
            let mut prev = from;
            for t in repeated {
                w.write_varint(u64::from(t - prev));
                prev = t;
            }
        }
        out.push((peer, w.finish()));
    }
    out
}

/// Client (`NetSet::Receive`): the server's input acknowledgment.
pub(crate) fn client_read_ack(world: &mut World, r: &mut BitReader) -> Result<(), DecodeError> {
    let server_tick = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("tick"))?;
    let n = r.read_len(usize::from(crate::model::MAX_LOCAL_PLAYERS))?;
    let from = server_tick.saturating_sub(INPUT_BUFFER_AHEAD);
    let mut rows = Vec::with_capacity(n);
    for _ in 0..n {
        let local = r.read_bits(3)? as u8;
        let acked = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("tick"))?;
        let margin = r.read_zigzag()? as f32 / 16.0;
        let count = u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("count"))?;
        let k = r.read_len(INPUT_BUFFER_AHEAD as usize + 1)?;
        let mut repeated = Vec::with_capacity(k);
        let mut prev = from;
        for _ in 0..k {
            prev = prev
                .checked_add(u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("tick"))?)
                .ok_or(DecodeError::Invalid("tick"))?;
            repeated.push(prev);
        }
        rows.push((local, acked, margin, count, repeated));
    }
    let Some(reg) = active(world) else {
        return Ok(());
    };
    let Some(input) = reg.input else {
        return Ok(());
    };
    let now = world.get_resource::<NetNode>().map_or(0, |n| n.local_ticks);
    for (local, acked, margin, count, repeated) in rows {
        (input.client_ack)(world, local, acked, &repeated);
        if local != 0 {
            continue;
        }
        let mut cp = world.resource_mut::<ClientPrediction>();
        let prev = cp.feedback;
        if prev.is_some_and(|p| acked < p.acked) {
            continue;
        }
        if prev.is_none_or(|p| acked > p.acked)
            && let Some(sent) = cp.sent_at.get(&acked).copied()
        {
            let sample = now.saturating_sub(sent) as f64 * cp.step_ms;
            match cp.transport_rtt {
                Some((rtt, jitter)) => {
                    cp.rtt_ms = Some(rtt);
                    cp.jitter_ms = jitter;
                }
                None => {
                    let rtt = cp.rtt_ms.map_or(sample, |r| r + (sample - r) / 8.0);
                    let dev = (sample - rtt).abs() / 2.0;
                    cp.jitter_ms += (dev - cp.jitter_ms) / 4.0;
                    cp.rtt_ms = Some(rtt);
                }
            }
        }
        while cp.sent_at.first_key_value().is_some_and(|(t, _)| *t <= acked) {
            cp.sent_at.pop_first();
        }
        cp.feedback = Some(Feedback { acked, margin, count });
        cp.feedback_fresh = true;
    }
    Ok(())
}

// ---- agent data ---------------------------------------------------------------------------------

/// Why a prediction query failed (agent error codes, `book/agents/net-prediction.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PredictQueryError {
    /// The method reads client state and this process is a server: `net_not_client`.
    NotClient,
    /// The method reads server state and this process is a client: `net_not_server`.
    NotServer,
    /// The entity is not predicted here: `entity_not_predicted`.
    EntityNotPredicted,
    /// No such peer: `peer_unknown`.
    PeerUnknown,
    /// Prediction is not installed or has not started: `net_not_running`.
    NotRunning,
}

impl PredictQueryError {
    /// The lowercase reason code.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotClient => "net_not_client",
            Self::NotServer => "net_not_server",
            Self::EntityNotPredicted => "entity_not_predicted",
            Self::PeerUnknown => "peer_unknown",
            Self::NotRunning => "net_not_running",
        }
    }
}

/// `net.prediction`: the client's clock and correction counters.
#[derive(Debug, Clone, PartialEq)]
pub struct PredictionReport {
    /// The predicted tick.
    pub client_tick: u32,
    /// Ticks ahead of the server tick estimate.
    pub lead: i32,
    /// Round trip, in milliseconds.
    pub rtt_ms: f64,
    /// Jitter, in milliseconds.
    pub jitter_ms: f64,
    /// The newest averaged buffer margin the server reported, in ticks.
    pub margin: Option<f32>,
    /// Corrections over the last second.
    pub corrections_per_s: f64,
    /// Ticks a rollback compares and re-runs as a correction now (RTT-scaled, spec 120 § 9.4).
    pub resim_depth: u32,
    /// Counters.
    pub stats: PredictionStats,
}

/// `net.prediction` (client).
pub fn prediction_report(world: &World) -> Result<PredictionReport, PredictQueryError> {
    if mode(world).is_server() {
        return Err(PredictQueryError::NotClient);
    }
    let cp = world
        .get_resource::<ClientPrediction>()
        .filter(|c| c.started)
        .ok_or(PredictQueryError::NotRunning)?;
    let clock = world.get_resource::<NetClock>().copied().unwrap_or_default();
    let window = cp
        .corrections_at
        .iter()
        .filter(|s| cp.steps.saturating_sub(**s) < RATE_WINDOW_TICKS)
        .count();
    Ok(PredictionReport {
        client_tick: cp.client_tick,
        lead: clock.lead,
        rtt_ms: clock.rtt_ms,
        jitter_ms: clock.jitter_ms,
        margin: cp.reported_margin(),
        corrections_per_s: window as f64 * crate::runtime::tick_hz(world) / RATE_WINDOW_TICKS as f64,
        resim_depth: cp.resim_depth(&settings(world)),
        stats: cp.stats.clone(),
    })
}

/// One player row of `net.input_buffer`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlayerBufferReport {
    /// The player's split-screen slot.
    pub local: u8,
    /// Its counters.
    pub stats: PlayerInputStats,
    /// Commands buffered now.
    pub buffered: usize,
}

/// `net.input_buffer` (server): the input stream of each player of a peer.
pub fn input_buffer_report(
    world: &World,
    peer: PeerId,
) -> Result<Vec<PlayerBufferReport>, PredictQueryError> {
    if !mode(world).is_server() {
        return Err(PredictQueryError::NotServer);
    }
    if !world.get_resource::<NetNode>().is_some_and(|n| n.has_peer(peer)) {
        return Err(PredictQueryError::PeerUnknown);
    }
    let reg = world
        .get_resource::<PredictionRegistry>()
        .filter(|r| r.input.is_some())
        .ok_or(PredictQueryError::NotRunning)?;
    let Some(input) = reg.input else {
        return Err(PredictQueryError::NotRunning);
    };
    let sp = world
        .get_resource::<ServerPrediction>()
        .ok_or(PredictQueryError::NotRunning)?;
    Ok(sp
        .players
        .iter()
        .filter(|(p, _)| p.peer == peer)
        .map(|(p, st)| PlayerBufferReport {
            local: p.local,
            stats: st.clone(),
            buffered: (input.server_buffered)(world, *p),
        })
        .collect())
}

/// One tick of `net.prediction_history`.
#[derive(Debug, Clone, PartialEq)]
pub struct HistoryRow {
    /// The tick.
    pub tick: u32,
    /// `(component path, value)` per predicted component the entity has.
    pub values: Vec<(&'static str, String)>,
    /// Whether the server's state confirmed this tick (compared equal, or replaced it).
    pub confirmed: bool,
}

/// `net.prediction_history` (client): the entity's prediction history, newest last (at most
/// `ticks` rows).
pub fn prediction_history(
    world: &World,
    entity: Entity,
    ticks: Option<usize>,
) -> Result<Vec<HistoryRow>, PredictQueryError> {
    if mode(world).is_server() {
        return Err(PredictQueryError::NotClient);
    }
    let reg = world
        .get_resource::<PredictionRegistry>()
        .ok_or(PredictQueryError::NotRunning)?;
    let cp = world
        .get_resource::<ClientPrediction>()
        .filter(|c| c.started)
        .ok_or(PredictQueryError::NotRunning)?;
    if world.get::<NetRole>(entity) != Some(&NetRole::Predicted) {
        return Err(PredictQueryError::EntityNotPredicted);
    }
    let Some(h) = cp.history.get(&entity) else {
        return Ok(Vec::new());
    };
    let limit = ticks.unwrap_or(usize::MAX);
    let skip = h.ring.len().saturating_sub(limit);
    Ok(h.ring
        .iter()
        .skip(skip)
        .map(|(t, vals)| HistoryRow {
            tick: *t,
            values: reg
                .comps
                .iter()
                .zip(vals)
                .filter_map(|(c, v)| v.as_ref().map(|v| (c.name, (c.debug)(v))))
                .collect(),
            confirmed: h.confirmed_through.is_some_and(|c| *t <= c),
        })
        .collect())
}

/// `net.force_correction` (server, dev builds): moves the server copy of a predicted entity so its
/// owner must correct. Returns the tick whose state first carries the move.
pub fn force_correction(
    world: &mut World,
    entity: Entity,
    offset: Vec3,
) -> Result<NetTick, PredictQueryError> {
    if !mode(world).is_server() {
        return Err(PredictQueryError::NotServer);
    }
    if world.get::<NetPredicted>(entity).is_none() {
        return Err(PredictQueryError::EntityNotPredicted);
    }
    let tick = world.get_resource::<NetNode>().map_or(NetTick(0), |n| n.tick);
    let Some(mut t) = world.get_mut::<Transform>(entity) else {
        return Err(PredictQueryError::EntityNotPredicted);
    };
    t.translation += offset;
    tracing::info!(target: "net", entity = ?entity, tick = tick.0, "net.force_correction");
    Ok(tick)
}

/// Client: the remembered input of a local player for a tick, formatted (`net.prediction_history`).
pub fn input_history_value(world: &World, local: u8, tick: NetTick) -> Option<String> {
    let input = world.get_resource::<PredictionRegistry>()?.input?;
    (input.history_value)(world, local, tick.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spec 78 § 12.2, ADR-2022: a local rebase moves the `Transform` snapshots of root entities in
    /// the ring, the server values and the prediction; other components and children stay.
    #[test]
    fn a_rebase_moves_the_predicted_transform_history_of_roots() {
        let mut world = World::new();
        let root = world.spawn_empty().id();
        let child = world.spawn_empty().id();
        let at = |x: f32| -> Snap { Arc::new(Transform::from_translation(Vec3::new(x, 0.0, 0.0))) };
        let other: Snap = Arc::new(7u32);
        let mut p = ClientPrediction::default();
        for e in [root, child] {
            p.history.insert(
                e,
                EntityHistory {
                    ring: VecDeque::from([(5, vec![Some(at(4100.0)), Some(Arc::clone(&other))])]),
                    server: Some(vec![Some(at(4101.0))]),
                    predicted: Some(vec![Some(at(4102.0))]),
                    confirmed_through: None,
                    reconciled_at: None,
                    snap_ticks: Vec::new(),
                },
            );
        }
        p.shift_origin(Vec3::new(-4096.0, 0.0, 0.0), &|e| e == root);
        let x = |snap: &Option<Snap>| {
            snap.as_ref()
                .and_then(|s| s.downcast_ref::<Transform>())
                .map(|t| t.translation.x)
        };
        let h = &p.history[&root];
        assert_eq!(x(&h.ring[0].1[0]), Some(4.0));
        assert_eq!(
            h.ring[0].1[1].as_ref().and_then(|s| s.downcast_ref::<u32>()),
            Some(&7)
        );
        assert_eq!(x(&h.server.as_ref().expect("server")[0]), Some(5.0));
        assert_eq!(x(&h.predicted.as_ref().expect("predicted")[0]), Some(6.0));
        let c = &p.history[&child];
        assert_eq!(x(&c.ring[0].1[0]), Some(4100.0), "a child is relative and stays");
    }

    /// Spec 77 § 4 item 2: a predicted component's fingerprint is its codec layout, so a dev
    /// process with a different predicted layout is named in `protocol_mismatch` (the constant
    /// placeholder 1 could never name it).
    #[test]
    fn predicted_components_report_their_codec_layout() {
        #[derive(Component, Clone, Debug, PartialEq, Default, crate::Replicate)]
        struct PredPos {
            x: f32,
        }

        let mut world = World::new();
        register_replicated::<PredPos>(&mut world, ReplicateOptions::default());
        predict::<PredPos>(&mut world, Tolerance::Exact).expect("predicted");
        let fingerprints = world.resource::<PredictionRegistry>().type_fingerprints();
        let layout = crate::codec::layout_hash::<PredPos>();
        assert_ne!(layout, 1, "a codec layout never equals the old placeholder");
        assert_eq!(
            fingerprints,
            vec![(format!("predict {}", std::any::type_name::<PredPos>()), layout)],
            "the fingerprint is the component's codec layout, not a constant"
        );
    }

    fn key_at(y: f32) -> Transform {
        Transform::from_translation(Vec3::new(0.0, y, 0.0))
    }

    #[test]
    fn tick_driven_motion_modes() {
        let step = 0.5;
        let mut m = TickDrivenMotion {
            keys: vec![(0.0, key_at(0.0)), (2.0, key_at(4.0))],
            start_tick: NetTick(10),
            mode: MotionMode::Once,
        };
        let y = |m: &TickDrivenMotion, t: u32| m.pose_at(NetTick(t), step).expect("keys").translation.y;
        assert_eq!(y(&m, 5), 0.0, "before the start: the first key");
        assert_eq!(y(&m, 11), 1.0);
        assert_eq!(y(&m, 14), 4.0);
        assert_eq!(y(&m, 20), 4.0, "once holds the last key");
        m.mode = MotionMode::Loop;
        assert_eq!(y(&m, 15), 1.0);
        m.mode = MotionMode::PingPong;
        assert_eq!(y(&m, 15), 3.0, "on the way back");
        assert_eq!(y(&m, 18), 0.0);
        assert!(TickDrivenMotion::default().pose_at(NetTick(3), step).is_none());
        let back = crate::codec::decode_from_slice::<TickDrivenMotion>(
            &crate::codec::encode_to_vec(&m, &CodecContext::detached()),
            &CodecContext::detached(),
        )
        .expect("round trip");
        assert_eq!(back, m);
    }

    /// Two predicted ticks in one step (`clock_control` returns 2) sample the same
    /// `interp_tick`. Both server buffer entries keep that view. Subtracting one tick per
    /// redundant command stores the older command a tick early, and `or_insert` would keep it.
    #[test]
    fn catch_up_commands_keep_the_sampled_view() {
        let mut client = World::new();
        client.insert_resource(NetMode::Client);
        client.insert_resource(LocalPeer(Some(PeerId(4))));
        add_net_input::<Pad>(&mut client).expect("input");
        ensure_controller::<Pad>(&mut client, PeerId(4));
        crate::interp::install_interpolation(&mut client, 60.0, 1.0);
        {
            let mut ni = client.resource_mut::<crate::interp::NetInterpolation>();
            ni.clock.set_delay_override(Some(0.0));
            let ms = 100.0 * ni.clock.ms_per_tick();
            ni.now_ms = ms;
            ni.clock.on_state(100, ms);
            assert_eq!(ni.clock.advance(ms), Some(100.0));
        }
        {
            let mut cp = client.resource_mut::<ClientPrediction>();
            cp.started = true;
            cp.ticks_per_step = Some(2);
        }
        net_predict(&mut client);
        let s = PredictionSettings::default();
        let frame = write_input_frame::<Pad>(&client, 2, &s, false).expect("frame");
        let mut server = World::new();
        server.insert_resource(NetMode::DedicatedServer);
        add_net_input::<Pad>(&mut server).expect("input");
        let mut r = BitReader::new(&frame);
        assert_eq!(r.read_bits(8).expect("tag") as u8, TAG_INPUT);
        server_read::<Pad>(&mut server, PeerId(4), &mut r, 1, &s).expect("decode");
        let player = NetPlayerId {
            peer: PeerId(4),
            local: 0,
        };
        let (older, newer) = {
            let stream = server.resource::<InputStream<Pad>>();
            let buffer = &stream.server[&player].buffer;
            (
                buffer[&1].view.map(|v| v.tick_256),
                buffer[&2].view.map(|v| v.tick_256),
            )
        };
        assert_eq!(newer, Some(25_600));
        assert_eq!(
            older,
            Some(25_600),
            "both commands were sampled at interp_tick 100"
        );
    }

    /// `f32` cannot represent every integer past 2^24. Elapsed time is a `f64` tick delta, so
    /// tick 16_777_217 is not sampled as tick 16_777_216. Loop span 3 makes that one-second
    /// error a different key fraction; a Once span past 2^24 hides it inside one `f32` ulp.
    #[test]
    fn tick_driven_elapsed_past_24_bits_is_not_short_a_second() {
        let m = TickDrivenMotion {
            keys: vec![(0.0, key_at(0.0)), (3.0, key_at(3.0))],
            start_tick: NetTick(0),
            mode: MotionMode::Loop,
        };
        let tick = 16_777_217u32;
        let pose = m.pose_at(NetTick(tick), 1.0).expect("pose");
        let elapsed = f64::from(tick);
        let expected = ((elapsed % 3.0) as f32 / 3.0) * 3.0;
        assert_eq!(pose.translation.y, expected);
        assert_ne!(pose.translation.y, 1.0, "the lost ulp samples one second early");
    }

    /// Key times are documented in increasing order. A decreasing list is not a pose, and a
    /// list that decodes still evaluates those keys.
    #[test]
    fn unsorted_motion_keys_are_not_decoded() {
        let bad = TickDrivenMotion {
            keys: vec![(0.0, key_at(0.0)), (2.0, key_at(2.0)), (1.0, key_at(1.0))],
            start_tick: NetTick(0),
            mode: MotionMode::Once,
        };
        let err = crate::codec::decode_from_slice::<TickDrivenMotion>(
            &crate::codec::encode_to_vec(&bad, &CodecContext::detached()),
            &CodecContext::detached(),
        );
        assert!(err.is_err(), "times 0, 2, 1 are not a motion");
        assert!(bad.pose_at(NetTick(1), 1.0).is_none());
        let good = TickDrivenMotion {
            keys: vec![(0.0, key_at(0.0)), (1.0, key_at(2.0))],
            start_tick: NetTick(0),
            mode: MotionMode::Once,
        };
        let back = crate::codec::decode_from_slice::<TickDrivenMotion>(
            &crate::codec::encode_to_vec(&good, &CodecContext::detached()),
            &CodecContext::detached(),
        )
        .expect("sorted keys decode");
        assert_eq!(
            back.pose_at(NetTick(1), 1.0).expect("pose").translation.y,
            good.pose_at(NetTick(1), 1.0).expect("pose").translation.y
        );
    }

    /// corr-net (ADR-5580): two keys at one time are a step (the interval code evaluates them,
    /// `f = 1` when `tb == ta`). Decode refused them although encode wrote them, so the server
    /// replicated a mover every client failed to decode (the record resynced forever), and
    /// `pose_at` froze the mover on the server.
    #[test]
    fn corr_net_equal_key_times_are_a_step_and_round_trip() {
        let m = TickDrivenMotion {
            keys: vec![
                (0.0, key_at(0.0)),
                (1.0, key_at(1.0)),
                (1.0, key_at(5.0)),
                (2.0, key_at(6.0)),
            ],
            start_tick: NetTick(0),
            mode: MotionMode::Once,
        };
        let back = crate::codec::decode_from_slice::<TickDrivenMotion>(
            &crate::codec::encode_to_vec(&m, &CodecContext::detached()),
            &CodecContext::detached(),
        )
        .expect("a step key decodes");
        assert_eq!(back, m);
        // Ticks of 0.25 s: tick 2 is time 0.5, tick 4 is time 1, tick 6 is time 1.5.
        let y = |tick: u32| m.pose_at(NetTick(tick), 0.25).expect("pose").translation.y;
        assert_eq!(y(2), 0.5);
        assert_eq!(y(4), 1.0, "at the step time: the first of the two keys");
        assert_eq!(y(6), 5.5, "after the step: from the second key");
    }

    /// Past 2^24, `acked as f32 - at as f32` collapses a one-tick gap to 0.
    #[test]
    fn adjustment_pending_subtracts_in_integer_ticks() {
        let cp = ClientPrediction {
            adjustments: VecDeque::from([(16_777_216, 1)]),
            ..ClientPrediction::default()
        };
        let fb = Feedback {
            acked: 16_777_217,
            margin: 0.0,
            count: 2,
        };
        assert_eq!(adjustment_pending(&cp, &fb), 0.5);
    }

    #[test]
    fn tolerance_helpers() {
        let a = Transform::default();
        let mut b = a;
        b.translation.x = 0.005;
        assert!(Tolerance::position(0.01).matches(&a, &b));
        b.translation.x = 0.02;
        assert!(!Tolerance::position(0.01).matches(&a, &b));
        let mut c = a;
        c.rotation = Quat::from_rotation_y(0.005);
        assert!(Tolerance::angle(0.01).matches(&a, &c));
        assert!(
            !Tolerance::position(0.01).matches(&a, &c),
            "position() wants the same rotation"
        );
        assert!(Tolerance::<Transform>::Exact.matches(&a, &a));
        assert!(!Tolerance::<Transform>::Exact.matches(&a, &b));
    }

    fn q_from_bits(x: u32, y: u32, z: u32, w: u32) -> Quat {
        Quat::from_xyzw(
            f32::from_bits(x),
            f32::from_bits(y),
            f32::from_bits(z),
            f32::from_bits(w),
        )
    }

    /// Net determinism audit, finding 1: `TickDrivenMotion::pose_at` rotation is a scalar
    /// nlerp with a left-fold dot and normalize, bit-identical on every platform. This key
    /// pair straddles a zero dot: glam's SSE2 dot sums `(x + z) + (y + w)` and reads
    /// negative (flipping the end quaternion into the other hemisphere) while the left fold
    /// `((x + y) + z) + w` reads positive; NEON's pairwise sum reads 0. Before the fix an
    /// x86_64 server and client computed different mover poses for the same tick.
    #[test]
    fn tick_driven_rotation_nlerp_is_bit_stable() {
        let a = q_from_bits(0xbf48_d19d, 0xbeaa_4236, 0xbe43_9863, 0xbef9_8eaa);
        let b = q_from_bits(0x3e8c_a04d, 0xbdf0_6fbb, 0xbf74_4b24, 0x3c44_f4a7);
        let m = TickDrivenMotion {
            keys: vec![
                (
                    0.0,
                    Transform {
                        rotation: a,
                        ..Default::default()
                    },
                ),
                (
                    1.0,
                    Transform {
                        rotation: b,
                        ..Default::default()
                    },
                ),
            ],
            start_tick: NetTick(0),
            mode: MotionMode::Once,
        };
        let pose = m.pose_at(NetTick(1), 0.5).expect("keys");
        assert_eq!(
            pose.rotation,
            q_from_bits(0xbeb8_901e, 0xbea2_e506, 0xbf4f_517c, 0xbeac_1c8d),
            "scalar nlerp bits"
        );
    }

    /// Net determinism audit, finding 2: `Tolerance::angle` decides with the left-fold chord
    /// comparison, not glam's ISA-specific dot and `acos`. This pair has
    /// `Quat::angle_between` = `0x3db96581` on x86_64 (SSE2 dot) against a left-fold angle of
    /// `0x3db9641f`; at `r = 0x3db96500` the SIMD path answered "no match" and the scalar
    /// path answers "match". One ulp below the scalar angle (`0x3db9641f`) both answer "no".
    #[test]
    fn pose_tolerance_decision_is_left_fold_stable() {
        let a = q_from_bits(0x3e0a_5c08, 0x3f19_89da, 0xbf33_7eff, 0xbeb8_e560);
        let b = q_from_bits(0x3de9_ae35, 0x3f12_20bd, 0xbf3a_9764, 0xbeb8_885a);
        let ta = Transform {
            rotation: a,
            ..Default::default()
        };
        let tb = Transform {
            rotation: b,
            ..Default::default()
        };
        assert!(
            Tolerance::angle(f32::from_bits(0x3db9_6500)).matches(&ta, &tb),
            "left-fold scalar decision is a match"
        );
        assert!(
            !Tolerance::angle(f32::from_bits(0x3db9_641f)).matches(&ta, &tb),
            "one ulp below the scalar angle is not a match"
        );
    }

    /// Net determinism audit, finding 3: `decay_offsets` decays with a bit-stable `2^-x`
    /// (no libm `powf`) and a scalar nlerp (no glam `Quat::slerp`). With the default
    /// settings (60 Hz step, 40 ms half-life) one step from this offset must produce exactly
    /// these bits on every platform.
    #[test]
    fn decay_offsets_is_bit_stable() {
        let o = NetVisualOffset {
            translation: Vec3::new(0.01, -0.02, 0.03),
            rotation: q_from_bits(0x3f00_0000, 0, 0, 0xbf00_0000),
        };
        let mut world = World::new();
        let e = world.spawn(o).id();
        decay_offsets(&mut world, &PredictionSettings::default(), &HashMap::new());
        let left = world.get::<NetVisualOffset>(e).expect("offset");
        assert_eq!(
            left.translation,
            Vec3::new(
                f32::from_bits(0x3bf5_7b8e),
                f32::from_bits(0xbc75_7b8e),
                f32::from_bits(0x3cb8_1caa),
            ),
            "translation times the 2^-x factor"
        );
        assert_eq!(
            left.rotation,
            q_from_bits(0xbf03_8939, 0, 0, 0x3f5b_9f7e),
            "scalar nlerp bits"
        );
    }

    /// Finding 3 (compose sites): `NetVisualOffset::apply` composes rotations with scalar
    /// multiply and normalize, not glam's fused/simd `Quat * Quat` + `Quat::normalize`.
    #[test]
    fn visual_offset_compose_is_bit_stable() {
        let q1 = q_from_bits(0x3e0a_5c08, 0x3f19_89da, 0xbf33_7eff, 0xbeb8_e560);
        let q2 = q_from_bits(0xbf48_d19d, 0xbeaa_4236, 0xbe43_9863, 0xbef9_8eaa);
        let o = NetVisualOffset {
            translation: Vec3::ZERO,
            rotation: q1,
        };
        let t = o.apply(&Transform {
            rotation: q2,
            ..Default::default()
        });
        assert_eq!(
            t.rotation,
            q_from_bits(0xbe05_6c0b, 0x3ece_a29d, 0x3f56_16ae, 0x3eb1_eea1)
        );
    }

    /// Finding 3 (`apply_visual_children`): the child's local pose is the scalar rotation of
    /// the offset by the inverse parent rotation plus a scalar quaternion compose.
    #[test]
    fn apply_visual_children_writes_bit_stable_local_pose() {
        let mut world = World::new();
        let prot = q_from_bits(0x3f4b_aec2, 0x3e64_4f80, 0x3f0b_c793, 0x3e0d_9a06);
        let oq = q_from_bits(0x3de9_ae35, 0x3f12_20bd, 0xbf3a_9764, 0xbeb8_885a);
        let parent = world
            .spawn((
                Transform {
                    rotation: prot,
                    ..Default::default()
                },
                NetVisualOffset {
                    translation: Vec3::new(0.1, 0.2, -0.05),
                    rotation: oq,
                },
            ))
            .id();
        let child = world
            .spawn((
                ChildOf(parent),
                NetVisualChild {
                    base: Transform {
                        rotation: q_from_bits(0x3f00_0000, 0xbe2a_aaab, 0x3f00_0000, 0xbe2a_aaab),
                        ..Default::default()
                    },
                },
                Transform::default(),
            ))
            .id();
        apply_visual_children(&mut world);
        let t = world.get::<Transform>(child).expect("child transform");
        assert_eq!(
            t.translation,
            Vec3::new(
                f32::from_bits(0x3dba_d78c),
                f32::from_bits(0xbe33_78a6),
                f32::from_bits(0x3ded_9843),
            ),
            "offset translation rotated by the inverse parent rotation"
        );
        assert_eq!(
            t.rotation,
            q_from_bits(0xbf23_31a8, 0x3f32_9660, 0x3e8b_778f, 0xbe39_5d65)
        );
    }

    #[test]
    fn prediction_key_and_transform_codecs_round_trip() {
        let k = NetPredictionKey(PredictionKey {
            player: NetPlayerId {
                peer: PeerId(7),
                local: 3,
            },
            tick: NetTick(123_456),
            index: 9,
        });
        let cx = CodecContext::detached();
        let back =
            crate::codec::decode_from_slice::<NetPredictionKey>(&crate::codec::encode_to_vec(&k, &cx), &cx)
                .expect("key");
        assert_eq!(back, k);
        let t = Transform {
            translation: Vec3::new(1.5, -2.25, 3.0),
            rotation: Quat::from_rotation_z(0.3),
            scale: Vec3::splat(2.0),
        };
        let back = crate::codec::decode_from_slice::<Transform>(&crate::codec::encode_to_vec(&t, &cx), &cx)
            .expect("t");
        assert_eq!(back, t, "full precision");
    }

    #[test]
    fn visual_offset_applies_and_decays() {
        let o = NetVisualOffset {
            translation: Vec3::X,
            rotation: Quat::IDENTITY,
        };
        let t = o.apply(&Transform::default());
        assert_eq!(t.translation, Vec3::X);
        let mut world = World::new();
        let e = world.spawn(o).id();
        let s = PredictionSettings::default();
        // 40 ms half-life at 60 Hz: 24 steps (400 ms) are 10 half-lives.
        for _ in 0..24 {
            decay_offsets(&mut world, &s, &HashMap::new());
        }
        let left = world
            .get::<NetVisualOffset>(e)
            .expect("offset")
            .translation
            .length();
        assert!(left < 1e-3 && left > 1e-4, "{left}");
    }

    /// Spec 120 § 9.3 (ADR-8217): dilation is a time scale. Slowed by 5 % for 100 steps the clock
    /// runs 95 ticks, at most one per step, the skips evenly spaced; sped up it runs 105, at least
    /// one per step; back on target the continuous time catches up without a 0- or 2-tick step.
    #[test]
    fn dilation_runs_a_fractional_tick_rate() {
        let s = PredictionSettings::default();
        let mut cp = ClientPrediction::default();
        let run = |cp: &mut ClientPrediction, rate: f64, steps: usize| -> Vec<u32> {
            (0..steps)
                .map(|_| {
                    let n = advance(cp, rate, &s);
                    cp.client_tick += n;
                    n
                })
                .collect()
        };
        let slow = run(&mut cp, -1.0, 100);
        assert_eq!(slow.iter().sum::<u32>(), 95, "{slow:?}");
        assert!(slow.iter().all(|n| *n <= 1), "{slow:?}");
        let skips: Vec<usize> = slow
            .iter()
            .enumerate()
            .filter(|(_, n)| **n == 0)
            .map(|(i, _)| i)
            .collect();
        assert!(skips.windows(2).all(|w| w[1] - w[0] == 20), "{skips:?}");
        assert_eq!(cp.stats.dilated, -5);
        let lag = cp.lag;
        let fast = run(&mut cp, 1.0, 100);
        assert!(fast.iter().all(|n| (1..=2).contains(n)), "{fast:?}");
        let extra: u32 = fast.iter().map(|n| n - 1).sum();
        assert!((5..=6).contains(&extra), "{extra} with lag {lag}: {fast:?}");
        let calm = run(&mut cp, 0.0, 40);
        assert!(calm.iter().all(|n| *n == 1), "{calm:?}");
        assert_eq!(cp.lag, 0.0, "the continuous time caught up with the tick");
        // A huge or broken `max_dilation` is clamped: still 0, 1 or 2 ticks.
        let wild = PredictionSettings {
            max_dilation: f64::INFINITY,
            ..PredictionSettings::default()
        };
        assert_eq!(advance(&mut cp, 1.0, &wild), 1);
        let big = PredictionSettings {
            max_dilation: 5.0,
            ..PredictionSettings::default()
        };
        assert!((0..=2).contains(&advance(&mut cp, 1.0, &big)));
    }

    /// A one-shot input of the `merge_late` tests.
    #[derive(Debug, Clone, Default, PartialEq)]
    struct Shot {
        x: i32,
        fire: bool,
    }

    impl ReplicateCodec for Shot {
        fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
            w.write_zigzag(i64::from(self.x));
            w.write_bool(self.fire);
        }
        fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
            Ok(Self {
                x: r.read_zigzag()? as i32,
                fire: r.read_bool()?,
            })
        }
    }

    impl NetInput for Shot {
        fn on_missing(last: &Self) -> Self {
            Self {
                fire: false,
                ..last.clone()
            }
        }
    }

    /// Spec 120 § 6.3: the default `merge_late` keeps a press that only the late input carries and
    /// otherwise takes the next input; an input type without one-shot fields takes the next.
    #[test]
    fn default_merge_late_keeps_a_lone_press() {
        let press = Shot { x: 1, fire: true };
        let walk = Shot { x: 2, fire: false };
        assert_eq!(Shot::merge_late(&press, &walk), press, "the press is kept");
        assert_eq!(Shot::merge_late(&walk, &press), press);
        assert_eq!(
            Shot::merge_late(&walk, &Shot { x: 3, fire: false }).x,
            3,
            "newest analog"
        );
        assert_eq!(
            Pad::merge_late(&Pad(1), &Pad(2)),
            Pad(2),
            "no one-shot: the next wins"
        );
    }

    /// Spec 120 § 9.3 on a client world: a frame that runs no tick keeps its press latched for the
    /// next frame's tick, and a frame that runs two ticks delivers the press on the first only.
    #[test]
    fn a_press_is_latched_across_frames_and_delivered_on_one_tick() {
        let mut world = World::new();
        world.insert_resource(NetMode::Client);
        world.insert_resource(LocalPeer(Some(PeerId(1))));
        add_net_input::<Shot>(&mut world).expect("input");
        world.init_resource::<ClientPrediction>();
        ensure_controller::<Shot>(&mut world, PeerId(1));
        let e = controllers(&mut world, PeerId(1))[0].0;
        let frame = |world: &mut World, value: Shot, ticks: &[u32]| -> Vec<Shot> {
            world.get_mut::<InputCommand<Shot>>(e).expect("command").value = value;
            latch_input::<Shot>(world);
            ticks
                .iter()
                .map(|t| {
                    capture_input::<Shot>(world, *t, None);
                    world.get::<InputCommand<Shot>>(e).expect("command").value.clone()
                })
                .collect()
        };
        // A 0-tick frame with a press, then a frame without one: the press lands on tick 1.
        assert!(frame(&mut world, Shot { x: 1, fire: true }, &[]).is_empty());
        assert_eq!(
            frame(&mut world, Shot { x: 1, fire: false }, &[1]),
            vec![Shot { x: 1, fire: true }]
        );
        // A 2-tick frame with a press: tick 2 gets it, tick 3 its `on_missing`.
        assert_eq!(
            frame(&mut world, Shot { x: 2, fire: true }, &[2, 3]),
            vec![Shot { x: 2, fire: true }, Shot { x: 2, fire: false }]
        );
        let stream = world.resource::<InputStream<Shot>>();
        let fired: Vec<u32> = (1..=3)
            .filter(|t| stream.client_command(0, NetTick(*t)).is_some_and(|c| c.fire))
            .collect();
        assert_eq!(fired, vec![1, 2], "each press on exactly one tick");
    }

    #[derive(Debug, Clone, Default, PartialEq)]
    struct Pad(i32);

    impl ReplicateCodec for Pad {
        fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
            w.write_zigzag(i64::from(self.0));
        }
        fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
            Ok(Self(r.read_zigzag()? as i32))
        }
    }

    impl NetInput for Pad {}

    /// The input frame carries the unacknowledged commands newest first, delta-coded, and the
    /// server buffers them by tick: on time, late after a repeat, early beyond 64 ticks.
    #[test]
    fn input_frame_round_trip_and_server_buffer_rules() {
        let mut client = World::new();
        client.insert_resource(NetMode::Client);
        client.insert_resource(LocalPeer(Some(PeerId(4))));
        add_net_input::<Pad>(&mut client).expect("input");
        ensure_controller::<Pad>(&mut client, PeerId(4));
        for t in 1..=20u32 {
            let mut q = client.query::<&mut InputCommand<Pad>>();
            for mut c in q.iter_mut(&mut client) {
                c.value = Pad(i32::from(t >= 10));
            }
            capture_input::<Pad>(&mut client, t, None);
        }
        let s = PredictionSettings::default();
        let frame = write_input_frame::<Pad>(&client, 20, &s, false).expect("frame");
        // 16 commands, 15 of them a repeat bit or a small value: a few bytes.
        assert!(frame.len() < 16, "{} bytes", frame.len());

        let mut server = World::new();
        server.insert_resource(NetMode::DedicatedServer);
        add_net_input::<Pad>(&mut server).expect("input");
        let mut r = BitReader::new(&frame);
        assert_eq!(r.read_bits(8).expect("tag") as u8, TAG_INPUT);
        server_read::<Pad>(&mut server, PeerId(4), &mut r, 8, &s).expect("decode");
        let player = NetPlayerId {
            peer: PeerId(4),
            local: 0,
        };
        let buffered = server.resource::<InputStream<Pad>>().buffered_ticks(player);
        assert_eq!(
            buffered,
            (8..=20).map(NetTick).collect::<Vec<_>>(),
            "older than the server tick are not kept"
        );
        let st = server.resource::<ServerPrediction>().players[&player].clone();
        assert_eq!(st.margin, 12.0);
        assert_eq!((st.late, st.early, st.missing), (0, 0, 0));
        // Consume 8..=21: tick 21 is missing and repeats tick 20's value.
        for t in 8..=21 {
            server_consume_typed::<Pad>(&mut server, t);
        }
        let mut q = server.query::<(&NetPlayer, &InputCommand<Pad>)>();
        let (_, c) = q.iter(&server).next().expect("controller spawned");
        assert_eq!((c.tick, c.value.clone()), (NetTick(21), Pad(1)));
        assert_eq!(server.resource::<ServerPrediction>().players[&player].missing, 1);
        assert_eq!(server_repeated::<Pad>(&server, player, 0), vec![21]);
        // A late command for the repeated tick counts once; one far ahead is early.
        let mut late = BitWriter::new();
        late.write_varint(21);
        late.write_varint(1);
        late.write_bits(0, 3);
        late.write_varint(1);
        late.write_varint(21);
        late.write_bool(false);
        late.write_bool(false);
        Pad(5).encode(&mut late, &CodecContext::detached());
        let bytes = late.finish();
        server_read::<Pad>(&mut server, PeerId(4), &mut BitReader::new(&bytes), 22, &s).expect("late");
        let mut early = BitWriter::new();
        early.write_varint(200);
        early.write_varint(1);
        early.write_bits(0, 3);
        early.write_varint(1);
        early.write_varint(200);
        early.write_bool(false);
        early.write_bool(false);
        Pad(5).encode(&mut early, &CodecContext::detached());
        let bytes = early.finish();
        server_read::<Pad>(&mut server, PeerId(4), &mut BitReader::new(&bytes), 22, &s).expect("early");
        let st = server.resource::<ServerPrediction>().players[&player].clone();
        assert_eq!((st.late, st.early), (1, 1));
        // The packet limit (spec 120 § 6.1, ADR-8217): a burst of 6 in one tick from a peer that
        // was quiet is kept whole (the bucket holds `INPUT_BURST_TICKS` ticks of the limit)...
        for _ in 0..6 {
            server_read::<Pad>(&mut server, PeerId(4), &mut BitReader::new(&bytes), 23, &s).expect("burst");
        }
        assert_eq!(server.resource::<ServerPrediction>().players[&player].flood, 0);
        // ...while a peer that sends 6 every tick drains it and is then cut to the limit of 4 per
        // tick: 2 of each tick's 6 are dropped.
        for t in 24..24 + 2 * INPUT_BURST_TICKS {
            for _ in 0..6 {
                server_read::<Pad>(&mut server, PeerId(4), &mut BitReader::new(&bytes), t, &s)
                    .expect("flood");
            }
        }
        let before = server.resource::<ServerPrediction>().players[&player].flood;
        assert!(before > 0, "a sustained flood is cut");
        let t = 24 + 2 * INPUT_BURST_TICKS;
        for _ in 0..6 {
            server_read::<Pad>(&mut server, PeerId(4), &mut BitReader::new(&bytes), t, &s).expect("flood");
        }
        assert_eq!(
            server.resource::<ServerPrediction>().players[&player].flood - before,
            2
        );

        // Redundant commands carry the view sampled with each one. Reconstructing an older
        // command as newest minus one tick is wrong when two commands were sampled at one
        // interpolation tick, and wrong when the interpolation tick did not move by 1.
        let mut viewed = World::new();
        viewed.insert_resource(NetMode::Client);
        viewed.insert_resource(LocalPeer(Some(PeerId(4))));
        add_net_input::<Pad>(&mut viewed).expect("input");
        ensure_controller::<Pad>(&mut viewed, PeerId(4));
        capture_input::<Pad>(
            &mut viewed,
            10,
            Some(crate::interp::ViewTickWire::new(100.0, 40.0)),
        );
        capture_input::<Pad>(
            &mut viewed,
            11,
            Some(crate::interp::ViewTickWire::new(101.0, 41.0)),
        );
        let view_frame = write_input_frame::<Pad>(&viewed, 11, &s, false).expect("view frame");
        let mut view_server = World::new();
        view_server.insert_resource(NetMode::DedicatedServer);
        add_net_input::<Pad>(&mut view_server).expect("input");
        let mut r = BitReader::new(&view_frame);
        assert_eq!(r.read_bits(8).expect("tag") as u8, TAG_INPUT);
        server_read::<Pad>(&mut view_server, PeerId(4), &mut r, 10, &s).expect("decode views");
        let (older, newer) = {
            let stream = view_server.resource::<InputStream<Pad>>();
            let buffer = &stream.server[&player].buffer;
            (buffer[&10].view, buffer[&11].view)
        };
        assert_eq!(older.map(|v| v.tick_256), Some(25_600));
        assert_eq!(older.map(|v| v.delay_ms), Some(40));
        assert_eq!(newer.map(|v| v.tick_256), Some(25_856));
        assert_eq!(newer.map(|v| v.delay_ms), Some(41));
        // Truncated frames are errors, never panics.
        for cut in 0..frame.len() {
            let mut r = BitReader::new(&frame[..cut]);
            let _ = r.read_bits(8);
            let _ = server_read::<Pad>(&mut server, PeerId(4), &mut r, 8, &s);
        }
    }

    #[test]
    fn input_for_an_undeclared_local_slot_is_dropped_not_spawned() {
        check_undeclared_slot(false);
    }

    #[test]
    fn an_idle_link_does_not_bypass_session_declared_slots() {
        check_undeclared_slot(true);
    }

    fn check_undeclared_slot(with_idle_link: bool) {
        let s = PredictionSettings::default();
        let frame_for = |local: u32| {
            let mut w = BitWriter::new();
            w.write_bits(u64::from(TAG_INPUT), 8);
            w.write_varint(1);
            w.write_varint(1); // one player
            w.write_bits(u64::from(local), 3);
            w.write_varint(1); // one command
            w.write_varint(10); // newest tick
            w.write_bool(false); // no view tick
            w.write_bool(false); // no trust
            Pad(5).encode(&mut w, &CodecContext::detached());
            w.finish()
        };
        let mut server = World::new();
        server.insert_resource(NetMode::DedicatedServer);
        if with_idle_link {
            server.insert_resource(crate::NetLink::default());
        }
        add_net_input::<Pad>(&mut server).expect("input");
        let mut ns = crate::NetSession::default();
        let declared = NetPlayerId {
            peer: PeerId(4),
            local: 0,
        };
        let ent = server.spawn(NetPlayer { id: declared }).id();
        ns.declare_player_for_test(declared, ent);
        server.insert_resource(ns);
        for local in [0u32, 5] {
            let bytes = frame_for(local);
            let mut r = BitReader::new(&bytes);
            r.read_bits(8).expect("tag");
            server_read::<Pad>(&mut server, PeerId(4), &mut r, 8, &s).expect("decode");
        }
        let stream = server.resource::<InputStream<Pad>>();
        assert_eq!(
            stream.buffered_ticks(declared).len(),
            1,
            "the declared slot is read"
        );
        let undeclared = NetPlayerId {
            peer: PeerId(4),
            local: 5,
        };
        assert!(stream.buffered_ticks(undeclared).is_empty());
        assert!(
            !server
                .resource::<ServerPrediction>()
                .players
                .contains_key(&undeclared)
        );
        assert_eq!(server.resource::<ServerPrediction>().undeclared_frames, 1);
        for _ in 0..3 {
            server_consume_typed::<Pad>(&mut server, 10);
        }
        let mut q = server.query::<&NetPlayer>();
        assert_eq!(q.iter(&server).count(), 1, "no controller is spawned for slot 5");
    }

    #[test]
    fn predict_refuses_unreplicated_and_duplicate_types() {
        #[derive(Component, Clone, Debug, Default, PartialEq)]
        struct Local(u8);
        impl ReplicateCodec for Local {
            fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
                w.write_bits(u64::from(self.0), 8);
            }
            fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
                Ok(Self(r.read_bits(8)? as u8))
            }
        }
        let mut world = World::new();
        assert!(matches!(
            predict::<Local>(&mut world, Tolerance::Exact),
            Err(PredictError::NotReplicated(_))
        ));
        predict::<Transform>(&mut world, Tolerance::position(0.01))
            .expect("transform is replicated by the fast path");
        assert!(matches!(
            predict::<Transform>(&mut world, Tolerance::Exact),
            Err(PredictError::AlreadyPredicted(_))
        ));
        add_net_input::<Pad>(&mut world).expect("first");
        assert!(matches!(
            add_net_input::<Pad>(&mut world),
            Err(PredictError::InputAlreadyRegistered(_))
        ));
    }

    #[test]
    fn query_errors_carry_their_codes() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        crate::runtime::ensure_resources(&mut world);
        assert_eq!(
            prediction_report(&world).err(),
            Some(PredictQueryError::NotClient)
        );
        assert_eq!(
            input_buffer_report(&world, PeerId(9)).err(),
            Some(PredictQueryError::PeerUnknown)
        );
        let e = world.spawn(Transform::default()).id();
        assert_eq!(
            force_correction(&mut world, e, Vec3::X).err(),
            Some(PredictQueryError::EntityNotPredicted)
        );
        world.insert_resource(NetMode::Client);
        assert_eq!(
            prediction_report(&world).err(),
            Some(PredictQueryError::NotRunning)
        );
        assert_eq!(
            input_buffer_report(&world, PeerId(9)).err(),
            Some(PredictQueryError::NotServer)
        );
        assert_eq!(
            force_correction(&mut world, e, Vec3::X).err(),
            Some(PredictQueryError::NotServer)
        );
        let codes: Vec<&str> = [
            PredictQueryError::NotClient,
            PredictQueryError::NotServer,
            PredictQueryError::EntityNotPredicted,
            PredictQueryError::PeerUnknown,
            PredictQueryError::NotRunning,
        ]
        .iter()
        .map(|e| e.as_str())
        .collect();
        assert_eq!(
            codes,
            [
                "net_not_client",
                "net_not_server",
                "entity_not_predicted",
                "peer_unknown",
                "net_not_running"
            ]
        );
    }
    /// A new connection restarts prediction and the snapshot rings: the previous server's tick
    /// numbers must not swallow the new server's frames. Test hooks stay.
    #[test]
    fn a_new_session_restarts_prediction_and_snapshot_rings() {
        use crate::interp::{NetInterpolation, NetSnapshotBuffer, ProxyState, SnapshotBuffer};
        let mut world = World::new();
        let cp = ClientPrediction {
            started: true,
            client_tick: 50_000,
            newest_server: Some(49_990),
            newest_from_state: true,
            start_offset_ticks: 3,
            max_sent: Some(50_000),
            ..Default::default()
        };
        world.insert_resource(cp);
        world.insert_resource(NetInterpolation::new(Default::default(), 60.0, 1.0));
        world
            .resource_mut::<NetInterpolation>()
            .clock
            .on_state(49_990, 1000.0);
        let mut buf = SnapshotBuffer::new(4);
        for tick in 50_000u64..50_004 {
            buf.push(ProxyState {
                tick,
                translation: Vec3::ZERO,
                rotation: Quat::IDENTITY,
                scale: Vec3::ONE,
                velocity: None,
                teleport: false,
                steady: false,
            });
        }
        let e = world.spawn(NetSnapshotBuffer(buf)).id();
        crate::replication::client_session_started(&mut world);
        let cp = world.resource::<ClientPrediction>();
        assert!(!cp.started);
        assert_eq!(cp.client_tick, 0);
        assert_eq!(cp.newest_server, None);
        assert_eq!(cp.max_sent, None);
        assert_eq!(cp.start_offset_ticks, 3, "test hooks survive");
        assert!(
            world
                .get::<NetSnapshotBuffer>(e)
                .is_some_and(|b| b.0.ticks().is_empty())
        );
        assert_eq!(world.resource::<NetInterpolation>().clock.interp_tick(), None);
    }
    /// Predicted time follows the world's fixed step: a 30 Hz world steps 1/30 s, converts a
    /// 100 ms round trip to 3 ticks, and 128 Hz is not taken for 60.
    #[test]
    fn predicted_time_follows_the_fixed_step() {
        let mut world = World::new();
        assert!(
            (tick_step_seconds_of(&world) - 1.0 / 60.0).abs() < 1e-6,
            "default 60 Hz"
        );
        world.insert_resource(regia_app::FixedTime::default());
        for hz in [30.0f64, 128.0] {
            world.resource_mut::<regia_app::FixedTime>().hz = hz;
            assert!((f64::from(tick_step_seconds_of(&world)) - 1.0 / hz).abs() < 1e-6);
            world.insert_resource(ClientPrediction::default());
            refresh_step(&mut world);
            let mut cp = world.resource_mut::<ClientPrediction>();
            cp.rtt_ms = Some(100.0);
            let ticks = cp.rtt_ticks();
            assert!((ticks - 100.0 * hz / 1000.0).abs() < 1e-6, "{hz}: {ticks}");
        }
    }

    fn resync_delta(old: u32, jump: i64, floor: u32) -> (u32, i32) {
        let mut world = World::new();
        world.insert_resource(ClientPrediction {
            client_tick: old,
            newest_server: Some(floor),
            ..Default::default()
        });
        resync(&mut world, &PredictionRegistry::default(), jump);
        let cp = world.resource::<ClientPrediction>();
        assert_eq!(cp.adjustments.len(), 1);
        (cp.client_tick, cp.adjustments[0].1)
    }

    /// A one-tick lead resync stores a delta of +1 for any client tick, including the step
    /// across `2^31`. Subtracting the ticks after narrowing each to `i32` overflows that step
    /// in debug (about 414 days at 60 Hz) and wraps the sign in release.
    #[test]
    fn lead_resync_across_i32_max_stores_the_signed_delta() {
        let (tick, delta) = resync_delta(100, 1, 0);
        assert_eq!((tick, delta), (101, 1));
        let (tick, delta) = resync_delta(100, -3, 90);
        assert_eq!((tick, delta), (97, -3));

        let old = i32::MAX as u32;
        let (tick, delta) = resync_delta(old, 1, old);
        assert_eq!((tick, delta), (old + 1, 1), "a +1 step across 2^31");

        let old = 1u32 << 31;
        let (tick, delta) = resync_delta(old, -1, old - 1);
        assert_eq!((tick, delta), (old - 1, -1), "a -1 step across 2^31");
    }

    /// A resync whose tick gap does not fit in `i32` clamps the stored delta instead of
    /// truncating through a wrapping cast.
    #[test]
    fn lead_resync_clamps_a_delta_past_i32() {
        let jump = i64::from(i32::MAX) + 5;
        let (tick, delta) = resync_delta(0, jump, 0);
        assert_eq!(tick, (jump) as u32);
        assert_eq!(delta, i32::MAX);
    }

    /// The fraction of an adjustment the server has already reflected is `(acked - at) / count`.
    /// Casting the tick numbers to `f32` first drops a gap of a few ticks once they pass `2^24`,
    /// so a finished adjustment stays fully pending and the lead controller keeps dilating.
    #[test]
    fn pending_adjustment_counts_ticks_past_the_f32_exact_range() {
        let pending = |at: u32, acked: u32, count: u32, d: i32| {
            let mut cp = ClientPrediction::default();
            cp.adjustments.push_back((at, d));
            adjustment_pending(
                &cp,
                &Feedback {
                    acked,
                    margin: 0.0,
                    count,
                },
            )
        };
        assert!((pending(100, 102, 32, 8) - 7.5).abs() < 1e-5);
        let at = 1u32 << 25;
        let got = pending(at, at + 2, 32, 8);
        assert!(
            (got - 7.5).abs() < 1e-4,
            "gap of 2 ticks at 2^25 must retire 1/16 of the adjustment, got {got}"
        );
    }

    #[derive(Component, Default)]
    struct ExtraMarker;

    /// Audit net1 perf H2: quiet predicted ticks, including a resim, do not build component
    /// type names. The debug check looks at change ticks first.
    #[test]
    fn quiet_predicted_ticks_do_not_build_component_names() {
        let mut world = World::new();
        world.insert_resource(NetMode::Client);
        world.insert_resource(LocalPeer(Some(PeerId(1))));
        world.insert_resource(NetSettings::default());
        predict::<Transform>(&mut world, Tolerance::Exact).expect("transform");
        world.spawn((
            Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)),
            NetRole::Predicted,
            NetId(1),
            ExtraMarker,
        ));
        world.resource_mut::<ClientPrediction>().started = true;
        let reg = world.resource::<PredictionRegistry>().clone();
        let s = PredictionSettings::default();
        for t in 1..=4 {
            run_client_tick(&mut world, &reg, t, false, &s);
        }
        run_client_tick(&mut world, &reg, 3, true, &s);
        run_client_tick(&mut world, &reg, 4, true, &s);
        let built = world.resource::<ClientPrediction>().name_builds;
        assert_eq!(
            built, 0,
            "built {built} component names with no unregistered write"
        );
    }

    /// Audit net1 perf M3: an unchanged predicted component keeps one snapshot `Arc` across ticks.
    #[test]
    fn unchanged_prediction_reuses_the_snapshot_arc() {
        let mut world = World::new();
        world.insert_resource(NetSettings::default());
        predict::<Transform>(&mut world, Tolerance::Exact).expect("transform");
        let e = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                NetRole::Predicted,
                NetId(3),
            ))
            .id();
        let reg = world.resource::<PredictionRegistry>().clone();
        let s = PredictionSettings::default();
        for t in 1..=3 {
            record_history(&mut world, &reg, t, &s);
        }
        let h = &world.resource::<ClientPrediction>().history[&e];
        let snap = |i: usize| h.ring[i].1[0].as_ref().expect("snap");
        assert!(
            Arc::ptr_eq(snap(0), snap(1)) && Arc::ptr_eq(snap(1), snap(2)),
            "unchanged transform must share one snap"
        );
    }

    /// Audit net1 perf M3: the pre-apply predicted snap is the ring's snap, which is what a
    /// matching server frame writes back.
    #[test]
    fn before_apply_keeps_the_ring_snap() {
        let mut world = World::new();
        world.insert_resource(NetSettings::default());
        predict::<Transform>(&mut world, Tolerance::Exact).expect("transform");
        let e = world
            .spawn((
                Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)),
                NetRole::Predicted,
                NetId(4),
            ))
            .id();
        let reg = world.resource::<PredictionRegistry>().clone();
        let s = PredictionSettings::default();
        record_history(&mut world, &reg, 1, &s);
        client_before_apply(&mut world, true);
        let h = &world.resource::<ClientPrediction>().history[&e];
        let ring = h.ring.back().expect("ring").1[0].as_ref().expect("snap");
        let pred = h.predicted.as_ref().expect("predicted")[0]
            .as_ref()
            .expect("snap");
        assert!(Arc::ptr_eq(ring, pred), "the stored prediction is the ring snap");
    }

    /// corr-low (corr-net LOW): `snap_ticks` describes the ring's newest snaps, which the next
    /// record reuses while a component's change tick still matches. The pre-apply step stored
    /// the change ticks of the values it had just snapshotted instead: a component changed
    /// outside the predicted schedule (and not rewritten by a server value) then matched the new
    /// tick, and the next record reused the ring's older snap, so the history held a value the
    /// world never had at that tick.
    #[test]
    fn a_change_before_apply_is_recorded_at_the_next_tick() {
        let mut world = World::new();
        world.insert_resource(NetSettings::default());
        predict::<Transform>(&mut world, Tolerance::Exact).expect("transform");
        let e = world
            .spawn((
                Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
                NetRole::Predicted,
                NetId(6),
            ))
            .id();
        let reg = world.resource::<PredictionRegistry>().clone();
        let s = PredictionSettings::default();
        record_history(&mut world, &reg, 1, &s);
        // A gameplay system outside the predicted schedule moves the entity.
        world.increment_change_tick();
        world.get_mut::<Transform>(e).expect("transform").translation.x = 5.0;
        // No server value is held yet, so nothing is written back before the apply.
        client_before_apply(&mut world, true);
        record_history(&mut world, &reg, 2, &s);
        let h = &world.resource::<ClientPrediction>().history[&e];
        let (tick, snaps) = h.ring.back().expect("ring");
        assert_eq!(*tick, 2);
        let x = snaps[0]
            .as_ref()
            .and_then(|v| v.downcast_ref::<Transform>())
            .expect("transform snap")
            .translation
            .x;
        assert_eq!(
            x, 5.0,
            "tick 2 records the world's transform, not the tick 1 snap"
        );
    }

    /// Audit net1 perf M4: consuming every player's input walks the `NetPlayer` rows once per
    /// tick, not once per player.
    #[test]
    fn controllers_are_resolved_once_per_tick() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        world.insert_resource(NetSettings::default());
        add_net_input::<Pad>(&mut world).expect("input");
        const N: u8 = 16;
        for i in 0..N {
            let player = NetPlayerId {
                peer: PeerId(u32::from(i) + 1),
                local: 0,
            };
            world.spawn(NetPlayer { id: player });
            world.resource_mut::<InputStream<Pad>>().server.insert(
                player,
                ServerStream {
                    started: true,
                    buffer: BTreeMap::from([(
                        1,
                        ServerCmd {
                            value: Pad(1),
                            view: None,
                            pos: None,
                        },
                    )]),
                    ..Default::default()
                },
            );
        }
        server_consume_typed::<Pad>(&mut world, 1);
        let rows = world.resource::<PredictPerf>().controller_rows;
        assert!(
            rows <= u64::from(N),
            "controllers walked {rows} rows for {N} players"
        );
        server_consume_typed::<Pad>(&mut world, 1);
        let again = world.resource::<PredictPerf>().controller_rows;
        assert_eq!(again, rows, "a second consume in the same tick must not rescan");
    }

    /// Audit net1 perf M6: adopting trusted client positions scans the predicted pawns once per
    /// tick, then reuses that result.
    #[test]
    fn trust_adopt_scans_predicted_pawns_once() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        let mut settings = NetSettings::default();
        settings.prediction.trust_client_within_m = Some(1.0);
        world.insert_resource(settings);
        add_net_input::<Pad>(&mut world).expect("input");
        predict::<Transform>(&mut world, Tolerance::Exact).expect("transform");
        let reg = world.resource::<PredictionRegistry>().clone();
        const N: u32 = 8;
        for i in 0..N {
            let peer = PeerId(i + 1);
            let player = NetPlayerId { peer, local: 0 };
            world.spawn((
                NetPlayer { id: player },
                NetOwner(peer),
                NetPredicted,
                Transform::default(),
                NetId(i + 1),
                NetRole::Authority,
            ));
            world
                .resource_mut::<ServerPrediction>()
                .players
                .insert(player, PlayerInputStats::default());
            world.resource_mut::<InputStream<Pad>>().server.insert(
                player,
                ServerStream {
                    started: true,
                    reported: Some((1, Vec3::ZERO)),
                    ..Default::default()
                },
            );
        }
        trust_adopt(&mut world, &reg, 1);
        let scans = world.resource::<PredictPerf>().trusted_scans;
        assert_eq!(scans, 1, "one scan for {N} pawns, got {scans}");
        trust_adopt(&mut world, &reg, 1);
        let again = world.resource::<PredictPerf>().trusted_scans;
        assert_eq!(again, 1, "an unchanged tick must not scan again, got {again}");
    }

    /// corr-low (corr-net LOW): the trusted-pawn cache was keyed on the owners, the owner links
    /// and each predicted pawn's own parent. A pawn whose owner comes through a parent that is
    /// not predicted (a pawn seated in a vehicle the owner's root carries) changes owner when
    /// that parent is reparented, and the cache kept the old owner: the previous owner's reported
    /// position was adopted onto the pawn another player now controls.
    #[test]
    fn reparenting_an_unpredicted_link_moves_the_trusted_pawn() {
        let mut world = World::new();
        world.insert_resource(NetMode::DedicatedServer);
        let root1 = world.spawn(NetOwner(PeerId(1))).id();
        let root2 = world.spawn(NetOwner(PeerId(2))).id();
        let vehicle = world.spawn(ChildOf(root1)).id();
        let pawn = world
            .spawn((NetPredicted, Transform::default(), NetId(5), ChildOf(vehicle)))
            .id();
        assert_eq!(trusted_entity(&mut world, PeerId(1)), Some(pawn));
        assert_eq!(trusted_entity(&mut world, PeerId(2)), None);
        // The vehicle changes hands; the pawn itself is not touched.
        world.entity_mut(vehicle).insert(ChildOf(root2));
        world.increment_change_tick();
        assert_eq!(
            trusted_entity(&mut world, PeerId(2)),
            Some(pawn),
            "the pawn's owner is now peer 2"
        );
        assert_eq!(
            trusted_entity(&mut world, PeerId(1)),
            None,
            "peer 1 no longer owns it"
        );
    }

    /// Audit net1 perf L8: a correction offset that has decayed to zero leaves the entity.
    #[test]
    fn a_decayed_visual_offset_is_removed() {
        let mut world = World::new();
        let e = world.spawn(NetVisualOffset::default()).id();
        decay_offsets(&mut world, &PredictionSettings::default(), &HashMap::new());
        assert!(
            world.get::<NetVisualOffset>(e).is_none(),
            "a zero offset is not a component"
        );
    }

    /// Audit net1 perf L9: a tick inside the last key interval returns that interval's pose.
    #[test]
    fn pose_at_reads_the_last_key_interval() {
        let keys: Vec<(f32, Transform)> = (0..32).map(|i| (i as f32, key_at(i as f32))).collect();
        let m = TickDrivenMotion {
            keys,
            start_tick: NetTick(0),
            mode: MotionMode::Once,
        };
        let y = m.pose_at(NetTick(61), 0.5).expect("keys").translation.y;
        assert!(
            (y - 30.5).abs() < 1e-4,
            "tick 61 at 0.5 s is halfway through the last interval, got {y}"
        );
    }
}
