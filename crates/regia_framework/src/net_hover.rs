// SPDX-License-Identifier: MPL-2.0
//! Hover hulls in the predicted schedule (`docs/specs/73` § 9.2, ADR-8819): the `net` feature.
//!
//! [`NetHoverPlugin`] runs [`tick_hull`] for every [`NetHover`] hull inside `regia_net`'s predicted
//! schedule: on the server for all of them, on the owning client ahead of the server, and again on
//! every re-run after a correction. The step is the pure hover step of [`crate::det_hover`], so a hull
//! re-runs per entity like a character (spec 73 § 9), not in a private world like a vehicle (§ 9.1).
//!
//! | Piece | What |
//! |---|---|
//! | [`HoverNetInput`] / [`HoverCommand`] | the default input command and how any command maps to [`DetHoverInput`] |
//! | [`NetHover`] | the marker: which local player drives, and the key of the hull's tuning |
//! | [`NetHoverHulls`] | the hull tunings this process knows, by [`hull_key`] |
//! | [`hover_tolerance`] | the one comparison over [`DetHoverState`] |
//! | [`NetHoverEffects`] | hop and jump launches, recorded once per tick (never on a re-run) |
//! | [`net_hover_rows`] | what `net.hovers` reports |
//!
//! # The order of one tick
//!
//! Per hull, in `(NetId, entity)` order: the hull tuning named by [`NetHover::hull`], the input
//! (the owner's command, or the hull's own [`DetHoverInput`] without an owner), on the server an
//! outside move of the `Transform` (knockback, `net.force_correction`) taken into the state,
//! [`tick_hull`], then [`DetHoverLean`] and `Transform` exactly as
//! [`present_det_hover_hulls`](crate::det_hover::present_det_hover_hulls) writes them. Hulls never
//! meet in the step, so the order fixes only the counters.
//!
//! # Corrections
//!
//! `DetHoverState` travels at full `f32` precision, so a rollback restarts from the server's bits
//! and the re-run reaches the server's result. `Transform` is predicted too (the rollback set and
//! the visual offset read it), but it is derived from the state, so [`hover_tolerance`] compares the
//! state alone. The rendered pose is `regia_net`'s `NetVisualOffset` over the simulated one.

use std::any::TypeId;
use std::collections::HashMap;
use std::f32::consts::TAU;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_detsim::hover::HoverState;
use regia_detsim::hover::host::HoverBody;
use regia_detsim::hover::hull_handshake_bytes;
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::{CodecContext, LayoutHasher, ReplicateCodec};
use regia_net::predict::{
    ClientPrediction, EntityTolerance, InputCommand, NetInput, NetVisualOffset, PredictError,
    PredictedValues, PredictionRegistry, PredictionState, Tolerance, add_net_input, add_predicted_systems,
    predict, predict_entity_tolerance, predict_local_writes, tick_step_seconds_of,
};
use regia_net::replication::{ReplicateOptions, register_replicated};
use regia_net::{NetId, NetMode, NetOwner, NetPlayer, NetPlayerId, NetRole, NetTick, Replicate};
use regia_scene::Transform;

use crate::det_hover::{
    DetHoverConfig, DetHoverFloor, DetHoverHull, DetHoverInput, DetHoverLean, DetHoverState,
    DetHoverSteppedExternally, HopSpeedCache, compute_lean_of, hull_rotation, tick_hull,
};

/// Position and hover height (`altitude`, `prev_terrain`) tolerance of the comparison, metres.
pub const TOLERANCE_POSITION_M: f32 = 0.01;
/// Angle tolerance (yaw, roll, pitch, cushion pitch, ground pitch and roll, slope), radians.
pub const TOLERANCE_ANGLE_RAD: f32 = 0.01;
/// Velocity tolerance (the velocity vector, `vertical_vel`, `coast_drop_left`), m/s.
pub const TOLERANCE_VELOCITY: f32 = 0.05;
/// Rate tolerance (yaw, roll and cushion pitch rates, `ground_climb`), per second.
pub const TOLERANCE_RATE: f32 = 0.05;
/// Timer tolerance (`hop_cooldown`), seconds.
pub const TOLERANCE_TIMER_S: f32 = 1.0 / 240.0;

/// Refusal code of a command with an axis the encoder never writes (`-128`).
pub const CODE_INPUT_RANGE: &str = "hover_input_range";
/// `net.hovers` fault of a hull whose [`NetHover::hull`] key is in no [`NetHoverHulls`] list.
pub const FAULT_HULL_UNKNOWN: &str = "hover_hull_unknown";

/// A `dt` this far from the net tick step logs `net.hover_dt_mismatch`, seconds.
const DT_MISMATCH_S: f32 = 1e-4;
/// Steps of an axis on each side of zero: zero is exact, so a released stick stays released.
const AXIS_STEPS: f32 = 127.0;
/// Most [`NetHoverEffects`] kept when the game does not drain them.
const EFFECTS_CAP: usize = 256;

// ---- the input command --------------------------------------------------------------------------

fn quantize_axis(v: f32) -> i8 {
    if v.is_finite() {
        (v.clamp(-1.0, 1.0) * AXIS_STEPS).round() as i8
    } else {
        0
    }
}

fn dequantize_axis(q: i8) -> f32 {
    f32::from(q) / AXIS_STEPS
}

/// The default input command of a hover hull (ADR-8819).
///
/// Quantized on the wire: each axis 8 bits (-127 to 127, zero exact), `hop` and `jump` one bit. Both
/// sides step [`HoverNetInput::quantized`], so the client predicts with what the server receives.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HoverNetInput {
    /// Forward thrust, -1 (brake, then reverse) to 1.
    pub throttle: f32,
    /// Sideways thrust, -1 (left) to 1 (right).
    pub strafe: f32,
    /// Steering, -1 to 1.
    pub steer: f32,
    /// A hop pressed this tick (a press: a missing command does not repeat it).
    pub hop: bool,
    /// A jump pressed this tick (a press: a missing command does not repeat it).
    pub jump: bool,
}

impl HoverNetInput {
    /// The command as it travels. Idempotent.
    #[must_use]
    pub fn quantized(&self) -> Self {
        Self {
            throttle: dequantize_axis(quantize_axis(self.throttle)),
            strafe: dequantize_axis(quantize_axis(self.strafe)),
            steer: dequantize_axis(quantize_axis(self.steer)),
            ..*self
        }
    }

    /// A command from a hull input (the values before quantization).
    #[must_use]
    pub fn from_hover_input(input: &DetHoverInput) -> Self {
        Self {
            throttle: input.throttle,
            strafe: input.strafe,
            steer: input.steer,
            hop: input.hop,
            jump: input.jump,
        }
    }

    fn axes_key(&self) -> [i8; 3] {
        [
            quantize_axis(self.throttle),
            quantize_axis(self.strafe),
            quantize_axis(self.steer),
        ]
    }

    fn write_axes(&self, w: &mut BitWriter) {
        for q in self.axes_key() {
            w.write_bits(u64::from(q as u8), 8);
        }
    }

    fn read_axes(r: &mut BitReader) -> Result<[f32; 3], DecodeError> {
        let mut out = [0.0; 3];
        for v in &mut out {
            let q = r.read_bits(8)? as u8 as i8;
            if q == i8::MIN {
                return Err(DecodeError::Invalid(CODE_INPUT_RANGE));
            }
            *v = dequantize_axis(q);
        }
        Ok(out)
    }

    fn write_buttons(&self, w: &mut BitWriter) {
        w.write_bool(self.hop);
        w.write_bool(self.jump);
    }

    fn with_axes_and_buttons(axes: [f32; 3], r: &mut BitReader) -> Result<Self, DecodeError> {
        Ok(Self {
            throttle: axes[0],
            strafe: axes[1],
            steer: axes[2],
            hop: r.read_bool()?,
            jump: r.read_bool()?,
        })
    }
}

impl ReplicateCodec for HoverNetInput {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        self.write_axes(w);
        self.write_buttons(w);
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let axes = Self::read_axes(r)?;
        Self::with_axes_and_buttons(axes, r)
    }

    fn encode_delta(&self, base: &Self, w: &mut BitWriter, _cx: &CodecContext) {
        let moved = self.axes_key() != base.axes_key();
        w.write_bool(moved);
        if moved {
            self.write_axes(w);
        }
        self.write_buttons(w);
    }

    fn decode_delta(base: &Self, r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let axes = if r.read_bool()? {
            Self::read_axes(r)?
        } else {
            [base.throttle, base.strafe, base.steer]
        };
        Self::with_axes_and_buttons(axes, r)
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str(std::any::type_name::<Self>());
        h.write_u64(8);
        h.write_u64(3);
    }
}

impl NetInput for HoverNetInput {
    fn on_missing(last: &Self) -> Self {
        Self {
            hop: false,
            jump: false,
            ..*last
        }
    }

    fn merge_late(late: &Self, next: &Self) -> Self {
        Self {
            hop: late.hop || next.hop,
            jump: late.jump || next.jump,
            ..*next
        }
    }
}

/// How a game's input command maps to a hull's input. Games with richer input than
/// [`HoverNetInput`] implement this on their own [`NetInput`] type and install it with
/// [`install_net_hover`].
pub trait HoverCommand: NetInput {
    /// The hull input of this command. Must be the same on both sides: use the values as they
    /// travel, not the raw ones.
    fn hover_input(&self) -> DetHoverInput;
}

impl HoverCommand for HoverNetInput {
    fn hover_input(&self) -> DetHoverInput {
        let q = self.quantized();
        DetHoverInput {
            throttle: q.throttle,
            strafe: q.strafe,
            steer: q.steer,
            hop: q.hop,
            jump: q.jump,
        }
    }
}

// ---- the hull's tuning --------------------------------------------------------------------------

/// The key of a hull's tuning: FNV-1a 64 over its 273 lockstep handshake bytes
/// ([`hull_handshake_bytes`], ADR-8632) and the footprint's `f32` bits. Integer work only: the same
/// on every target. Two hulls with one key step the same bits.
pub fn hull_key(hull: &DetHoverHull) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let bytes = hull_handshake_bytes(&hull.params, &hull.limits, hull.flight);
    let footprint = [
        hull.half_length.to_bits().to_le_bytes(),
        hull.half_width.to_bits().to_le_bytes(),
    ];
    bytes
        .iter()
        .chain(footprint.iter().flatten())
        .fold(OFFSET, |h, b| (h ^ u64::from(*b)).wrapping_mul(PRIME))
}

/// Makes a hover hull a networked one: replicated, stepped in the predicted schedule and not by
/// [`DetHoverPlugin`](crate::det_hover::DetHoverPlugin). Spawn it on the server with its
/// [`DetHoverHull`], [`DetHoverState`], `Replicated` (`TransformReplication::Full` or `Movement`: the
/// rendered pose of proxies is the replicated `Transform`), `NetPredicted` and `NetOwner`, and make
/// the marker with [`NetHover::for_hull`]. A hull without an owner is server-driven: it reads its own
/// [`DetHoverInput`].
#[derive(Component, Debug, Clone, Default, PartialEq, Eq, Replicate)]
#[require(DetHoverHull, DetHoverSteppedExternally, NetHoverMemo, Transform)]
pub struct NetHover {
    /// Which of the owning peer's local players drives it.
    pub local_player: u8,
    /// [`hull_key`] of the hull's tuning. Every process looks the tuning up in its
    /// [`NetHoverHulls`]; a key it does not know leaves the hull unstepped.
    pub hull: u64,
}

impl NetHover {
    /// The marker of a hull with this tuning, driven by `local_player`.
    pub fn for_hull(hull: &DetHoverHull, local_player: u8) -> Self {
        NetHover {
            local_player,
            hull: hull_key(hull),
        }
    }
}

/// The hull tunings this process knows. Both sides must list every tuning a networked hull uses
/// (the server's copy is checked too). Starts with [`DetHoverHull::DEFAULT`].
#[derive(Resource, Debug, Clone)]
pub struct NetHoverHulls {
    hulls: Vec<(u64, DetHoverHull)>,
}

impl Default for NetHoverHulls {
    fn default() -> Self {
        let mut hulls = NetHoverHulls { hulls: Vec::new() };
        hulls.insert(DetHoverHull::DEFAULT);
        hulls
    }
}

impl NetHoverHulls {
    /// Lists `hull` and returns its key. Listing a hull twice keeps one entry.
    pub fn insert(&mut self, hull: DetHoverHull) -> u64 {
        let key = hull_key(&hull);
        if !self.hulls.iter().any(|(k, _)| *k == key) {
            self.hulls.push((key, hull));
        }
        key
    }

    /// The hull with this key.
    pub fn get(&self, key: u64) -> Option<&DetHoverHull> {
        self.hulls.iter().find(|(k, _)| *k == key).map(|(_, h)| h)
    }

    /// Number of listed hulls.
    pub fn len(&self) -> usize {
        self.hulls.len()
    }

    /// Whether no hull is listed.
    pub fn is_empty(&self) -> bool {
        self.hulls.is_empty()
    }
}

/// What the last step left in `Transform`, so the server sees an outside move.
#[derive(Component, Debug, Clone, Copy, Default)]
pub(crate) struct NetHoverMemo {
    stepped_at: Option<Vec3>,
}

// ---- the state on the wire ----------------------------------------------------------------------

/// `f32` fields of a [`HoverBody`] on the wire, in order.
const STATE_FLOATS: usize = 21;
/// `bool` fields of a [`HoverState`] on the wire.
const STATE_FLAGS: usize = 5;

/// The state as words: the floats, the flags and `prev_terrain`. Exhaustive destructuring, so a new
/// `HoverState` field does not compile until it travels.
fn state_words(body: &HoverBody) -> ([f32; STATE_FLOATS], [bool; STATE_FLAGS], Option<f32>) {
    let HoverState {
        velocity,
        vertical_vel,
        yaw,
        yaw_rate,
        airborne,
        jump_active,
        altitude,
        prev_terrain,
        hop_cooldown,
        ground_climb,
        slope,
        roll,
        roll_rate,
        pitch,
        level_off,
        yaw_modern,
        coast_drop_left,
        thrusting,
        ground_pitch,
        ground_roll,
        cushion_pitch,
        cushion_pitch_rate,
    } = body.state;
    let [x, y, z] = body.pos;
    (
        [
            x,
            y,
            z,
            velocity[0],
            velocity[1],
            velocity[2],
            vertical_vel,
            yaw,
            yaw_rate,
            altitude,
            hop_cooldown,
            ground_climb,
            slope,
            roll,
            roll_rate,
            pitch,
            coast_drop_left,
            ground_pitch,
            ground_roll,
            cushion_pitch,
            cushion_pitch_rate,
        ],
        [airborne, jump_active, level_off, yaw_modern, thrusting],
        prev_terrain,
    )
}

fn state_from_words(f: [f32; STATE_FLOATS], b: [bool; STATE_FLAGS], prev_terrain: Option<f32>) -> HoverBody {
    HoverBody {
        pos: [f[0], f[1], f[2]],
        state: HoverState {
            velocity: [f[3], f[4], f[5]],
            vertical_vel: f[6],
            yaw: f[7],
            yaw_rate: f[8],
            airborne: b[0],
            jump_active: b[1],
            altitude: f[9],
            prev_terrain,
            hop_cooldown: f[10],
            ground_climb: f[11],
            slope: f[12],
            roll: f[13],
            roll_rate: f[14],
            pitch: f[15],
            level_off: b[2],
            yaw_modern: b[3],
            coast_drop_left: f[16],
            thrusting: b[4],
            ground_pitch: f[17],
            ground_roll: f[18],
            cushion_pitch: f[19],
            cushion_pitch_rate: f[20],
        },
    }
}

fn write_prev_terrain(w: &mut BitWriter, v: Option<f32>) {
    w.write_bool(v.is_some());
    if let Some(v) = v {
        w.write_f32(v);
    }
}

fn read_prev_terrain(r: &mut BitReader) -> Result<Option<f32>, DecodeError> {
    Ok(if r.read_bool()? { Some(r.read_f32()?) } else { None })
}

fn read_flags(r: &mut BitReader) -> Result<[bool; STATE_FLAGS], DecodeError> {
    let mut flags = [false; STATE_FLAGS];
    for f in &mut flags {
        *f = r.read_bool()?;
    }
    Ok(flags)
}

/// The full state at full precision (a rollback restarts from the server's bits); the delta sends
/// one change bit per field and only the changed fields.
impl ReplicateCodec for DetHoverState {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        let (floats, flags, prev) = state_words(&self.0);
        for v in floats {
            w.write_f32(v);
        }
        for f in flags {
            w.write_bool(f);
        }
        write_prev_terrain(w, prev);
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let mut floats = [0.0; STATE_FLOATS];
        for v in &mut floats {
            *v = r.read_f32()?;
        }
        let flags = read_flags(r)?;
        let prev = read_prev_terrain(r)?;
        Ok(DetHoverState(state_from_words(floats, flags, prev)))
    }

    fn encode_delta(&self, base: &Self, w: &mut BitWriter, _cx: &CodecContext) {
        let (floats, flags, prev) = state_words(&self.0);
        let (old, _, old_prev) = state_words(&base.0);
        for (v, o) in floats.iter().zip(old.iter()) {
            let changed = v.to_bits() != o.to_bits();
            w.write_bool(changed);
            if changed {
                w.write_f32(*v);
            }
        }
        for f in flags {
            w.write_bool(f);
        }
        let prev_changed = prev.map(f32::to_bits) != old_prev.map(f32::to_bits);
        w.write_bool(prev_changed);
        if prev_changed {
            write_prev_terrain(w, prev);
        }
    }

    fn decode_delta(base: &Self, r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let (mut floats, _, mut prev) = state_words(&base.0);
        for v in &mut floats {
            if r.read_bool()? {
                *v = r.read_f32()?;
            }
        }
        let flags = read_flags(r)?;
        if r.read_bool()? {
            prev = read_prev_terrain(r)?;
        }
        Ok(DetHoverState(state_from_words(floats, flags, prev)))
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str(std::any::type_name::<Self>());
        h.write_u64(STATE_FLOATS as u64);
        h.write_u64(STATE_FLAGS as u64);
    }
}

// ---- the comparison -----------------------------------------------------------------------------

/// The difference of two angles, wrapped to `[0, PI]`.
fn angle_diff(a: f32, b: f32) -> f32 {
    let d = (a - b).rem_euclid(TAU);
    d.min(TAU - d)
}

/// The comparison of ADR-8819 (4) over two hull states: true when they count as equal.
pub fn hover_state_matches(a: &DetHoverState, b: &DetHoverState) -> bool {
    let (fa, ba, pa) = state_words(&a.0);
    let (fb, bb, pb) = state_words(&b.0);
    let within = |i: usize, tol: f32| (fa[i] - fb[i]).abs() <= tol;
    let angle = |i: usize| angle_diff(fa[i], fb[i]) <= TOLERANCE_ANGLE_RAD;
    let prev = match (pa, pb) {
        (None, None) => true,
        (Some(x), Some(y)) => (x - y).abs() <= TOLERANCE_POSITION_M,
        _ => false,
    };
    let pos = Vec3::new(fa[0], fa[1], fa[2]).distance(Vec3::new(fb[0], fb[1], fb[2]));
    let vel = Vec3::new(fa[3], fa[4], fa[5]).distance(Vec3::new(fb[3], fb[4], fb[5]));
    ba == bb
        && prev
        && pos <= TOLERANCE_POSITION_M
        && vel <= TOLERANCE_VELOCITY
        && within(6, TOLERANCE_VELOCITY)
        && angle(7)
        && within(8, TOLERANCE_RATE)
        && within(9, TOLERANCE_POSITION_M)
        && within(10, TOLERANCE_TIMER_S)
        && within(11, TOLERANCE_RATE)
        && angle(12)
        && angle(13)
        && within(14, TOLERANCE_RATE)
        && angle(15)
        && within(16, TOLERANCE_VELOCITY)
        && angle(17)
        && angle(18)
        && angle(19)
        && within(20, TOLERANCE_RATE)
}

fn hover_matches(predicted: &PredictedValues<'_>, server: &PredictedValues<'_>) -> Option<bool> {
    Some(hover_state_matches(
        predicted.get::<DetHoverState>()?,
        server.get::<DetHoverState>()?,
    ))
}

fn hover_error(predicted: &PredictedValues<'_>, server: &PredictedValues<'_>) -> Option<f32> {
    let (a, b) = (predicted.get::<DetHoverState>()?, server.get::<DetHoverState>()?);
    Some(Vec3::from_array(a.0.pos).distance(Vec3::from_array(b.0.pos)))
}

/// The hover tolerance of ADR-8819 over `Transform` and [`DetHoverState`]: it decides on the state
/// alone (the `Transform` is derived from it).
pub fn hover_tolerance() -> EntityTolerance {
    EntityTolerance {
        covers: vec![TypeId::of::<Transform>(), TypeId::of::<DetHoverState>()],
        matches: hover_matches,
        error_m: hover_error,
    }
}

// ---- the step -----------------------------------------------------------------------------------

/// A hull's hop or jump launch, recorded once per tick ([`NetHoverEffects`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetHoverEffectKind {
    /// A fan-pulse hop fired.
    Hop,
    /// A jump fired.
    Jump,
}

/// One launch of [`NetHoverEffects`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetHoverEffect {
    /// The hull.
    pub entity: Entity,
    /// The tick it fired on.
    pub tick: NetTick,
    /// What fired.
    pub kind: NetHoverEffectKind,
}

/// Hop and jump launches for sounds and particles (spec 73 § 8): written on the first run of a tick
/// on a client and on the server, never on a re-run. The game drains `events`; past 256 undrained
/// events the oldest are dropped and counted.
#[derive(Resource, Debug, Clone, Default)]
pub struct NetHoverEffects {
    /// The launches not drained yet, oldest first.
    pub events: Vec<NetHoverEffect>,
    /// Launches dropped because 256 were waiting.
    pub dropped: u64,
}

/// Counters of the networked hover step.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct NetHoverStats {
    /// Hull steps run for the first time (and every server step).
    pub steps: u64,
    /// Hull steps re-run after a correction.
    pub resim_steps: u64,
    /// Hops that fired on a first run.
    pub hops: u64,
    /// Jumps that fired on a first run.
    pub jumps: u64,
    /// Steps whose bad-state guard reset the hull.
    pub recovered: u64,
    /// Hull steps skipped because the [`NetHover::hull`] key is in no [`NetHoverHulls`] list.
    pub unknown_hull: u64,
    /// Server: outside moves of a hull's `Transform` taken into its state.
    pub adopted_moves: u64,
    /// `DetHoverConfig::dt` is more than 1e-4 s off the net tick step (logged once).
    pub dt_mismatch: bool,
}

/// Scratch kept between steps (the hop speeds already solved).
#[derive(Resource, Default)]
struct NetHoverScratch {
    hops: HopSpeedCache,
}

/// The tuning a hull steps with: its own when its key matches, else the listed hull with the key
/// (which then becomes its own). `None`: nobody listed the key.
fn resolve_hull(world: &mut World, entity: Entity, key: u64) -> Option<DetHoverHull> {
    let own = world.get::<DetHoverHull>(entity).copied();
    if let Some(own) = own
        && hull_key(&own) == key
    {
        return Some(own);
    }
    let listed = world.get_resource::<NetHoverHulls>()?.get(key).copied()?;
    if let Ok(mut ent) = world.get_entity_mut(entity) {
        ent.insert(listed);
    }
    Some(listed)
}

/// The predicted system: steps every [`NetHover`] hull this process simulates, in `NetId` order.
pub fn net_hover_step<I: HoverCommand>(world: &mut World) {
    let _span = tracing::trace_span!(target: "regia_framework", "net_hover_step").entered();
    let Some(state) = world.get_resource::<PredictionState>().copied() else {
        return;
    };
    let commands: HashMap<NetPlayerId, I> = {
        let mut q = world.query::<(&NetPlayer, &InputCommand<I>)>();
        q.iter(world).map(|(p, c)| (p.id, c.value.clone())).collect()
    };
    let mut hulls: Vec<(Option<NetId>, Entity, Option<NetPlayerId>, u64)> = {
        let mut q = world.query::<(
            Entity,
            &NetHover,
            Option<&NetId>,
            Option<&NetOwner>,
            Option<&NetRole>,
        )>();
        q.iter(world)
            .filter(|(_, _, _, _, role)| state.simulates(*role))
            .map(|(entity, hover, id, owner, _)| {
                let player = state.owner_player(owner).map(|mut player| {
                    player.local = hover.local_player;
                    player
                });
                (id.copied(), entity, player, hover.hull)
            })
            .collect()
    };
    if hulls.is_empty() {
        return;
    }
    hulls.sort_unstable_by_key(|(id, entity, _, _)| (*id, *entity));
    let config = world
        .get_resource::<DetHoverConfig>()
        .copied()
        .unwrap_or_default();
    let floor = world.get_resource::<DetHoverFloor>().cloned().unwrap_or_default();
    let dt = config.dt;
    let net_dt = tick_step_seconds_of(world);
    let mut stats = world.get_resource::<NetHoverStats>().copied().unwrap_or_default();
    if (dt - net_dt).abs() > DT_MISMATCH_S && !stats.dt_mismatch {
        stats.dt_mismatch = true;
        tracing::warn!(target: "net", dt, net_dt, "net.hover_dt_mismatch");
    }
    let mut scratch = world.remove_resource::<NetHoverScratch>().unwrap_or_default();
    let mut effects: Vec<NetHoverEffect> = Vec::new();
    let first_run = state.server || !state.resimulating;
    for (_, entity, player, key) in hulls {
        let Some(hull) = resolve_hull(world, entity, key) else {
            stats.unknown_hull += 1;
            continue;
        };
        let Some(mut body) = world.get::<DetHoverState>(entity).map(|s| s.0) else {
            continue;
        };
        let input = match player {
            Some(player) => commands
                .get(&player)
                .map_or_else(|| I::default().hover_input(), I::hover_input),
            None => world.get::<DetHoverInput>(entity).copied().unwrap_or_default(),
        };
        if state.server {
            let moved_to = world
                .get::<NetHoverMemo>(entity)
                .and_then(|m| m.stepped_at)
                .zip(world.get::<Transform>(entity).map(|t| t.translation))
                .and_then(|(left, now)| (left != now).then_some(now));
            if let Some(now) = moved_to {
                body.pos = now.to_array();
                stats.adopted_moves += 1;
            }
        }
        let speed = if input.hop {
            scratch.hops.get(&hull, &config.world, dt)
        } else {
            0.0
        };
        let tick = tick_hull(&hull, &mut body, &input, &*floor.0, &config.world, speed, dt);
        let lean = compute_lean_of(&hull, &body.state, &input);
        let at = Vec3::from_array(body.pos);
        let rotation = hull_rotation(&body.state, &lean);
        if let Ok(mut ent) = world.get_entity_mut(entity) {
            if let Some(mut stored) = ent.get_mut::<DetHoverInput>()
                && *stored != input
            {
                *stored = input;
            }
            if let Some(mut s) = ent.get_mut::<DetHoverState>() {
                s.0 = body;
            }
            if let Some(mut l) = ent.get_mut::<DetHoverLean>() {
                *l = lean;
            }
            if let Some(mut t) = ent.get_mut::<Transform>() {
                t.translation = at;
                t.rotation = rotation;
            }
            if let Some(mut memo) = ent.get_mut::<NetHoverMemo>() {
                memo.stepped_at = Some(at);
            }
        }
        if first_run {
            stats.steps += 1;
            stats.recovered += u64::from(tick.recovered);
            stats.hops += u64::from(tick.hopped);
            stats.jumps += u64::from(tick.jumped);
            let launches = [
                (tick.hopped, NetHoverEffectKind::Hop),
                (tick.jumped, NetHoverEffectKind::Jump),
            ];
            for (fired, kind) in launches {
                if fired {
                    effects.push(NetHoverEffect {
                        entity,
                        tick: state.tick,
                        kind,
                    });
                }
            }
        } else {
            stats.resim_steps += 1;
        }
    }
    world.insert_resource(scratch);
    world.insert_resource(stats);
    if !effects.is_empty() {
        let mut store = world.get_resource_or_insert_with(NetHoverEffects::default);
        store.events.extend(effects);
        let over = store.events.len().saturating_sub(EFFECTS_CAP);
        if over > 0 {
            store.events.drain(..over);
            store.dropped += over as u64;
        }
    }
}

// ---- installation -------------------------------------------------------------------------------

/// Installs networked hover hulls on a world with input command `I`: what [`NetHoverPlugin`] does
/// for [`HoverNetInput`]. A `predict::<Transform>` made first is kept, and so is `I` already
/// registered by another installer (characters, vehicles) with the same type.
///
/// # Errors
///
/// [`PredictError::InputAlreadyRegistered`]: the game registered another input type.
pub fn install_net_hover<I: HoverCommand>(world: &mut World) -> Result<(), PredictError> {
    register_replicated::<NetHover>(world, ReplicateOptions::default());
    register_replicated::<DetHoverState>(world, ReplicateOptions::default());
    match add_net_input::<I>(world) {
        Ok(()) => {}
        Err(PredictError::InputAlreadyRegistered(name)) if name == std::any::type_name::<I>() => {}
        Err(error) => return Err(error),
    }
    let pose = predict::<Transform>(world, Tolerance::pose(TOLERANCE_POSITION_M, TOLERANCE_ANGLE_RAD));
    if !matches!(pose, Ok(()) | Err(PredictError::AlreadyPredicted(_))) {
        return pose;
    }
    predict::<DetHoverState>(world, Tolerance::Fn(hover_state_matches))?;
    predict_entity_tolerance(world, hover_tolerance());
    // Derived each step: the input from the command, the lean and the tuning from the key.
    predict_local_writes::<DetHoverInput>(world);
    predict_local_writes::<DetHoverLean>(world);
    predict_local_writes::<DetHoverHull>(world);
    predict_local_writes::<NetHoverMemo>(world);
    world.init_resource::<DetHoverConfig>();
    world.init_resource::<DetHoverFloor>();
    world.init_resource::<NetHoverHulls>();
    world.init_resource::<NetHoverStats>();
    world.init_resource::<NetHoverEffects>();
    add_predicted_systems(world, net_hover_step::<I>);
    Ok(())
}

/// Networked hover hulls with [`HoverNetInput`] (spec 73 § 9.2). Add it after `NetPlugin`, on every
/// process. Games with their own input type call [`install_net_hover`] instead.
pub struct NetHoverPlugin;

impl Plugin for NetHoverPlugin {
    fn build(&self, app: &mut App) {
        if let Err(error) = install_net_hover::<HoverNetInput>(app.world_mut()) {
            tracing::error!(target: "net", %error, "net.hover_input_refused");
        }
    }
}

// ---- the agent surface --------------------------------------------------------------------------

/// One hull of [`net_hover_rows`].
#[derive(Debug, Clone, PartialEq)]
pub struct NetHoverRow {
    /// The entity.
    pub entity: Entity,
    /// Its network id, once assigned.
    pub net_id: Option<NetId>,
    /// `authority`, `predicted` or `proxy`.
    pub role: &'static str,
    /// [`NetHover::hull`].
    pub hull_key: u64,
    /// Whether the key names the hull's own tuning or one in [`NetHoverHulls`].
    pub hull_known: bool,
    /// The simulated position ([`DetHoverState`]).
    pub position: Vec3,
    /// What is drawn: the `Transform` plus the correction offset.
    pub rendered_position: Vec3,
    /// The velocity, m/s.
    pub velocity: Vec3,
    /// The heading, radians.
    pub yaw: f32,
    /// The height above the floor, metres.
    pub altitude: f32,
    /// Ballistic after a jump.
    pub airborne: bool,
    /// [`FAULT_HULL_UNKNOWN`] when the hull cannot step.
    pub fault: Option<&'static str>,
}

/// Whether [`install_net_hover`] ran on this world.
pub fn net_hovers_installed(world: &World) -> bool {
    world.get_resource::<PredictionRegistry>().is_some_and(|r| {
        r.component_names()
            .contains(&std::any::type_name::<DetHoverState>())
    })
}

/// Every networked hull in this process, in entity order.
pub fn net_hover_rows(world: &mut World) -> Vec<NetHoverRow> {
    let listed: Vec<u64> = world
        .get_resource::<NetHoverHulls>()
        .map(|h| h.hulls.iter().map(|(k, _)| *k).collect())
        .unwrap_or_default();
    let mut q = world.query::<(
        Entity,
        &NetHover,
        &DetHoverHull,
        &DetHoverState,
        Option<&Transform>,
        Option<&NetVisualOffset>,
        Option<&NetId>,
        Option<&NetRole>,
    )>();
    let mut rows: Vec<NetHoverRow> = q
        .iter(world)
        .map(|(entity, hover, hull, state, transform, offset, id, role)| {
            let known = hull_key(hull) == hover.hull || listed.contains(&hover.hull);
            let shown = transform.map_or(Vec3::from_array(state.0.pos), |t| t.translation);
            let s = &state.0.state;
            NetHoverRow {
                entity,
                net_id: id.copied(),
                role: match role {
                    Some(NetRole::Predicted) => "predicted",
                    Some(NetRole::Proxy) => "proxy",
                    _ => "authority",
                },
                hull_key: hover.hull,
                hull_known: known,
                position: Vec3::from_array(state.0.pos),
                rendered_position: shown + offset.map_or(Vec3::ZERO, |o| o.translation),
                velocity: Vec3::from_array(s.velocity),
                yaw: s.yaw,
                altitude: s.altitude,
                airborne: s.airborne,
                fault: (!known).then_some(FAULT_HULL_UNKNOWN),
            }
        })
        .collect();
    rows.sort_unstable_by_key(|r| r.entity);
    rows
}

/// The client's correction counters for `net.hovers`: `(corrections, snaps, last_correction_m)`.
/// `None` on a server (offline, dedicated or listen), which never corrects its own hulls.
pub fn net_hover_corrections(world: &World) -> Option<(u64, u64, f32)> {
    if world.get_resource::<NetMode>().is_none_or(|m| m.is_server()) {
        return None;
    }
    world
        .get_resource::<ClientPrediction>()
        .map(|cp| (cp.stats.corrections, cp.stats.snaps, cp.stats.last_correction_m))
}

#[cfg(test)]
mod tests;
