// SPDX-License-Identifier: MPL-2.0
//! Characters in the predicted schedule (`docs/specs/73` §§ 6.6, 6.7, 9): the `net` feature.
//!
//! [`NetCharacterPlugin`] runs [`step_character`] for every [`NetCharacter`] inside
//! `regia_net`'s predicted schedule: on the server for all of them, on the owning client ahead of
//! the server, again on every re-run after a correction.
//!
//! | Piece | What |
//! |---|---|
//! | [`CharacterNetInput`] / [`CharacterCommand`] | the default input command and how any command maps to [`CharacterInput`] |
//! | [`NetCharacter`] | the marker that brings the character components and takes the entity from the movement system |
//! | [`NetCharacterBase`] | the entity a character stands on and its position there (§ 6.6) |
//! | [`NetMontagePlay`] | the replicated play state root motion is sampled from (§ 6.7) |
//! | [`net_character_rows`] | what `net.characters` reports |
//!
//! # Moving bases
//!
//! A base that is not tick-driven (a raft on physics) is a proxy on the client, some ticks behind
//! the server, so the client and the server disagree about where a rider is in the world while
//! agreeing about where it is on the raft. Both sides therefore keep the rider's position relative
//! to the base and put it back on the base at the start of every step, with the pose of the base
//! they know: that replaces the carry of the collision backend ([`CharacterAnchored`]), holds for a
//! re-run tick (where the base stands still at its latest known pose) and draws the rider on the
//! raft the client sees.
//! Prediction compares the relative position, so a raft the client merely sees late is no
//! correction; a real difference is corrected and smoothed by `regia_net`.
//!
//! # Root motion
//!
//! A montage moves a networked character only from replicated play state: [`NetMontagePlay`] names
//! the montage, the start section, the rate and the start tick, and the step samples
//! [`MontagePlayback`] for tick `T` at index `T - start`. Frame-time root motion of the rig is
//! drained and dropped.

use std::any::TypeId;
use std::collections::HashMap;
use std::sync::Arc;

use regia_animation::components::{Animator, AnimatorSource, SkinnedMesh};
use regia_animation::montage::Montage;
use regia_animation::pose_eval::{
    AnimStateSnapshot, CaptureError, ParamLayout, capture_state_into, restore_state_ahead,
};
use regia_animation::{MontageId, MontagePlayback, MontagePlayer, PlayOptions};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3, det_yaw_of, det_yaw_quat};
use regia_net::bits::{BitReader, BitWriter, DecodeError};
use regia_net::codec::{Angle, CodecContext, LayoutHasher, ReplicateCodec};
use regia_net::predict::{
    EntityTolerance, InputCommand, NetClock, NetInput, NetResimulating, PredictError, PredictedValues,
    PredictionRegistry, PredictionState, Tolerance, add_net_input, add_predicted_systems, predict,
    predict_entity_tolerance, predict_local_writes, tick_step_seconds_of,
};
use regia_net::replication::{ReplicateOptions, register_replicated};
use regia_net::runtime::{NetEntities, NetNode};
use regia_net::{NetId, NetMode, NetOwner, NetPlayer, NetPlayerId, NetRole, NetSet, NetTick, Replicate};
use regia_physics::{BodyKind, PhysicsStep, RigidBody};
use regia_scene::Transform;

use crate::character::{
    Character, CharacterAnchored, CharacterCollisionQuery, CharacterInput, CharacterMovement, CharacterState,
    CharacterSteppedExternally, CollisionQuery, MoveResult, MovementMode, RootMotionApply, RootMotionDriven,
    RotationMode, SampledRootMotion, step_character,
};
use crate::character_control::camera_relative_move;
use crate::traversal::CharacterTraversal;

/// Translation tolerance of the character comparison (§ 9), metres.
pub const TOLERANCE_POSITION_M: f32 = 0.01;
/// Rotation tolerance of the character comparison (§ 9), radians.
pub const TOLERANCE_ROTATION_RAD: f32 = 0.01;
/// Velocity tolerance of the character comparison (§ 9), m/s.
pub const TOLERANCE_VELOCITY: f32 = 0.05;
/// Timer tolerance of the character comparison (§ 9), seconds.
pub const TOLERANCE_TIMER_S: f32 = 1.0 / 240.0;

/// Steps of a `move_dir` axis on each side of zero: zero is exact, so a released stick stands
/// still after the wire.
const MOVE_AXIS_STEPS: f32 = 127.0;
/// Refusal code of an input command with a stick axis the encoder cannot write (`-128`), as the
/// vehicle input's `vehicle_input_range`.
pub const CODE_MOVE_RANGE: &str = "character_input_range";
/// Yaw and pitch on the wire.
const LOOK_ANGLE: Angle = Angle { bits: 16 };

// ---- the input command --------------------------------------------------------------------------

fn quantize_axis(v: f32) -> i8 {
    if v.is_finite() {
        (v.clamp(-1.0, 1.0) * MOVE_AXIS_STEPS).round() as i8
    } else {
        0
    }
}

fn dequantize_axis(q: i8) -> f32 {
    f32::from(q) / MOVE_AXIS_STEPS
}

/// The default input command of a character (§ 9).
///
/// Quantized on the wire: `move_dir` 8 bits per axis, `yaw` and `pitch` 16 bits. The step runs on
/// [`CharacterNetInput::quantized`] on both sides, so the client predicts with exactly what the
/// server receives.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CharacterNetInput {
    /// Stick or WASD (`x` right, `y` forward), relative to `yaw`; length at most 1.
    pub move_dir: Vec2,
    /// Look yaw, radians about +Y (0 looks down -Z).
    pub yaw: f32,
    /// Look pitch, radians.
    pub pitch: f32,
    /// Jump pressed this tick (a one-shot: a missing command does not repeat it).
    pub jump: bool,
    /// Crouch held.
    pub crouch: bool,
    /// Sprint held.
    pub sprint: bool,
}

impl CharacterNetInput {
    /// The command as it travels. Idempotent.
    #[must_use]
    pub fn quantized(&self) -> Self {
        Self {
            move_dir: Vec2::new(
                dequantize_axis(quantize_axis(self.move_dir.x)),
                dequantize_axis(quantize_axis(self.move_dir.y)),
            ),
            yaw: LOOK_ANGLE.dequantize(LOOK_ANGLE.quantize(self.yaw)),
            pitch: LOOK_ANGLE.dequantize(LOOK_ANGLE.quantize(self.pitch)),
            ..self.clone()
        }
    }

    /// A command from a world-space [`CharacterInput`] and the look angles: `move_dir` goes into
    /// the frame of `yaw`. The rotation is the deterministic kernel, not glam's libm: this
    /// feeds lockstep prediction, so both peers must compute the same bits (fixrev-06-k L8).
    #[must_use]
    pub fn from_character_input(input: &CharacterInput, yaw: f32, pitch: f32) -> Self {
        let local = det_yaw_quat(-yaw) * input.move_dir;
        Self {
            move_dir: Vec2::new(local.x, -local.z),
            yaw,
            pitch,
            jump: input.jump,
            crouch: input.crouch,
            sprint: input.sprint,
        }
    }

    fn write_move(&self, w: &mut BitWriter) {
        w.write_bits(u64::from(quantize_axis(self.move_dir.x) as u8), 8);
        w.write_bits(u64::from(quantize_axis(self.move_dir.y) as u8), 8);
    }

    fn write_look(&self, w: &mut BitWriter) {
        w.write_bits(LOOK_ANGLE.quantize(self.yaw), LOOK_ANGLE.bits);
        w.write_bits(LOOK_ANGLE.quantize(self.pitch), LOOK_ANGLE.bits);
    }

    fn write_buttons(&self, w: &mut BitWriter) {
        w.write_bool(self.jump);
        w.write_bool(self.crouch);
        w.write_bool(self.sprint);
    }

    /// The stick. `i8::MIN` is refused: the encoder never writes it, and it reads back past the
    /// unit stick (the vehicle input refuses it the same way; rc1-rev-net-k 6).
    fn read_move(r: &mut BitReader) -> Result<Vec2, DecodeError> {
        let axis = |raw: u64| -> Result<f32, DecodeError> {
            let q = raw as u8 as i8;
            if q == i8::MIN {
                return Err(DecodeError::Invalid(CODE_MOVE_RANGE));
            }
            Ok(dequantize_axis(q))
        };
        let x = axis(r.read_bits(8)?)?;
        let y = axis(r.read_bits(8)?)?;
        Ok(Vec2::new(x, y))
    }

    fn read_look(r: &mut BitReader) -> Result<(f32, f32), DecodeError> {
        let yaw = LOOK_ANGLE.dequantize(r.read_bits(LOOK_ANGLE.bits)?);
        let pitch = LOOK_ANGLE.dequantize(r.read_bits(LOOK_ANGLE.bits)?);
        Ok((yaw, pitch))
    }

    fn move_key(&self) -> (i8, i8) {
        (quantize_axis(self.move_dir.x), quantize_axis(self.move_dir.y))
    }

    fn look_key(&self) -> (u64, u64) {
        (LOOK_ANGLE.quantize(self.yaw), LOOK_ANGLE.quantize(self.pitch))
    }
}

impl ReplicateCodec for CharacterNetInput {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        self.write_move(w);
        self.write_look(w);
        self.write_buttons(w);
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let move_dir = Self::read_move(r)?;
        let (yaw, pitch) = Self::read_look(r)?;
        Ok(Self {
            move_dir,
            yaw,
            pitch,
            jump: r.read_bool()?,
            crouch: r.read_bool()?,
            sprint: r.read_bool()?,
        })
    }

    fn encode_delta(&self, base: &Self, w: &mut BitWriter, _cx: &CodecContext) {
        let moved = self.move_key() != base.move_key();
        let looked = self.look_key() != base.look_key();
        w.write_bool(moved);
        if moved {
            self.write_move(w);
        }
        w.write_bool(looked);
        if looked {
            self.write_look(w);
        }
        self.write_buttons(w);
    }

    fn decode_delta(base: &Self, r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        let move_dir = if r.read_bool()? {
            Self::read_move(r)?
        } else {
            base.move_dir
        };
        let (yaw, pitch) = if r.read_bool()? {
            Self::read_look(r)?
        } else {
            (base.yaw, base.pitch)
        };
        Ok(Self {
            move_dir,
            yaw,
            pitch,
            jump: r.read_bool()?,
            crouch: r.read_bool()?,
            sprint: r.read_bool()?,
        })
    }

    fn layout(h: &mut LayoutHasher) {
        h.write_str(std::any::type_name::<Self>());
        h.write_u64(8);
        h.write_u64(u64::from(LOOK_ANGLE.bits));
    }
}

impl NetInput for CharacterNetInput {
    fn on_missing(last: &Self) -> Self {
        Self {
            jump: false,
            ..last.clone()
        }
    }
}

/// How a game's input command maps to a character's input. Games with richer input than
/// [`CharacterNetInput`] implement this on their own [`NetInput`] type and install it with
/// [`install_net_character`].
pub trait CharacterCommand: NetInput {
    /// The character input of this command (world-space `move_dir`) and the yaw a
    /// [`RotationMode::UseControllerYaw`] character faces. Must be the same on both sides: use
    /// the values as they travel, not the raw ones.
    fn character_input(&self) -> (CharacterInput, f32);
}

impl CharacterCommand for CharacterNetInput {
    fn character_input(&self) -> (CharacterInput, f32) {
        let q = self.quantized();
        (
            CharacterInput {
                move_dir: camera_relative_move(q.yaw, q.move_dir),
                jump: q.jump,
                crouch: q.crouch,
                sprint: q.sprint,
            },
            q.yaw,
        )
    }
}

// ---- components ---------------------------------------------------------------------------------

/// Makes a character a networked one: replicated, stepped in the predicted schedule and not by
/// the movement system of [`CharacterPlugin`](crate::character::CharacterPlugin). The components
/// it requires are defaults for whatever a prefab did not set. Spawn it with `Replicated`
/// (`TransformReplication::Movement` or `Full`: the comparison needs the rotation), `NetPredicted`
/// and `NetOwner`. A character without an owner is server-driven: it reads its own
/// [`CharacterInput`]. Both sides must hold the same [`CharacterMovement`] tuning.
#[derive(Component, Reflect, Debug, Clone, Default, PartialEq, Eq, Replicate)]
#[reflect(Component, Default)]
#[require(
    Character,
    CharacterMovement,
    CharacterInput,
    CharacterState,
    CharacterSteppedExternally,
    CharacterAnchored,
    NetCharacterBase,
    NetMontagePlay,
    NetCharacterMemo
)]
pub struct NetCharacter {
    /// Which of the owning peer's local players drives it.
    pub local_player: u8,
}

/// What a step leaves that the `Transform` on the wire cannot say: the entity a character stands
/// on and its position there (§ 6.6), and its exact facing. Predicted and replicated; written by the
/// step. `base` is `None` when the ground is not a replicated entity or the character is in the air:
/// the position is then the world position.
#[derive(Component, Debug, Clone, Default, PartialEq, Replicate)]
pub struct NetCharacterBase {
    /// The base.
    pub base: Option<NetId>,
    /// The character's position in the base's frame (rotation and translation, no scale).
    pub local: Vec3,
    /// The world position the step left it at, on the writer's side. A `Transform` that differs at
    /// the next step was moved by someone else (knockback, a forced correction), and the base-relative
    /// position takes that move on. Travels with the state, so a rollback sees no move.
    pub stepped_at: Vec3,
    /// The yaw the step left, full precision. The fast path quantizes rotations, and root motion
    /// and turning build on the facing, so the next step goes on from this value, not from the
    /// quantized one. A rotation that differs by more than the wire can explain stays.
    pub stepped_yaw: f32,
}

/// The replicated play state root motion is sampled from (§ 6.7). Predicted: write it from
/// predicted code with [`NetMontagePlay::start`]; a re-run reproduces the start. The montage comes
/// from the rig's [`MontagePlayer`] library on both sides. Cleared by the step when it ends.
#[derive(Component, Debug, Clone, Default, PartialEq, Replicate)]
pub struct NetMontagePlay {
    /// The montage's name; empty when nothing plays.
    #[replicate(max_len = 64)]
    pub montage: String,
    /// The start section's index.
    pub section: u16,
    /// The tick of the first step that moves (raw [`NetTick`]).
    pub start_tick: u32,
    /// Rate on top of the montage's own.
    pub rate: f32,
}

impl NetMontagePlay {
    /// Starts `montage` at `section`, first moving at `tick`.
    pub fn start(&mut self, montage: impl Into<String>, section: u16, tick: NetTick, rate: f32) {
        *self = Self {
            montage: montage.into(),
            section,
            start_tick: tick.0,
            rate,
        };
    }

    /// Whether a montage plays.
    pub fn is_playing(&self) -> bool {
        !self.montage.is_empty()
    }
}

/// What the last step left in [`CharacterState::mode`]: a different value later means someone
/// wrote the state (a rollback, a reset), and [`CharacterMovement::mode`] follows it.
#[derive(Component, Debug, Clone, Default)]
pub(crate) struct NetCharacterMemo {
    mode: MovementMode,
}

/// The resolved playback of a [`NetMontagePlay`].
#[derive(Component, Default)]
struct NetMontageCache {
    key: Option<(String, u16, u32)>,
    playback: Option<MontagePlayback>,
}

/// What [`play_montage_visuals`] started on the rig.
#[derive(Component, Default)]
struct NetMontageShown {
    play: NetMontagePlay,
    id: Option<MontageId>,
}

// ---- codecs of framework types ------------------------------------------------------------------

impl ReplicateCodec for MovementMode {
    fn encode(&self, w: &mut BitWriter, _cx: &CodecContext) {
        let (tag, custom) = match *self {
            Self::Walking => (0, None),
            Self::Falling => (1, None),
            Self::Flying => (2, None),
            Self::Swimming => (3, None),
            Self::Custom(id) => (4, Some(id)),
        };
        w.write_bits(tag, 3);
        if let Some(id) = custom {
            w.write_varint(u64::from(id));
        }
    }

    fn decode(r: &mut BitReader, _cx: &CodecContext) -> Result<Self, DecodeError> {
        Ok(match r.read_bits(3)? {
            0 => Self::Walking,
            1 => Self::Falling,
            2 => Self::Flying,
            3 => Self::Swimming,
            4 => Self::Custom(u32::try_from(r.read_varint()?).map_err(|_| DecodeError::Invalid("mode"))?),
            _ => return Err(DecodeError::Invalid("movement mode")),
        })
    }
}

impl ReplicateCodec for CharacterState {
    fn encode(&self, w: &mut BitWriter, cx: &CodecContext) {
        self.velocity.encode(w, cx);
        self.mode.encode(w, cx);
        w.write_f32(self.time_in_air);
        w.write_f32(self.coyote_time_left);
        w.write_f32(self.jump_buffer_time_left);
        self.ground_velocity.encode(w, cx);
        w.write_bits(u64::from(self.jumps_used), 8);
        w.write_bool(self.grounded);
        w.write_bool(self.landed_this_frame);
        w.write_bool(self.crouched);
    }

    fn decode(r: &mut BitReader, cx: &CodecContext) -> Result<Self, DecodeError> {
        Ok(Self {
            velocity: Vec3::decode(r, cx)?,
            mode: MovementMode::decode(r, cx)?,
            time_in_air: r.read_f32()?,
            coyote_time_left: r.read_f32()?,
            jump_buffer_time_left: r.read_f32()?,
            ground_velocity: Vec3::decode(r, cx)?,
            jumps_used: r.read_bits(8)? as u8,
            grounded: r.read_bool()?,
            landed_this_frame: r.read_bool()?,
            crouched: r.read_bool()?,
        })
    }
}

// ---- the comparison -----------------------------------------------------------------------------

/// Whether a rider stands on the same base in both values, and the base's identity.
fn same_base(a: &NetCharacterBase, b: &NetCharacterBase) -> bool {
    a.base.is_some() && a.base == b.base
}

fn state_matches(a: &CharacterState, b: &CharacterState, on_base: bool) -> bool {
    let timer = |x: f32, y: f32| (x - y).abs() <= TOLERANCE_TIMER_S;
    a.mode == b.mode
        && a.grounded == b.grounded
        && a.jumps_used == b.jumps_used
        && a.crouched == b.crouched
        && a.velocity.distance(b.velocity) <= TOLERANCE_VELOCITY
        // A proxy base's velocity is the client's estimate, late like its pose.
        && (on_base || a.ground_velocity.distance(b.ground_velocity) <= TOLERANCE_VELOCITY)
        && timer(a.time_in_air, b.time_in_air)
        && timer(a.coyote_time_left, b.coyote_time_left)
        && timer(a.jump_buffer_time_left, b.jump_buffer_time_left)
}

/// The comparison of § 9 over one character's values.
fn pose_and_state_match(
    (ta, sa, ba): (&Transform, &CharacterState, &NetCharacterBase),
    (tb, sb, bb): (&Transform, &CharacterState, &NetCharacterBase),
) -> bool {
    if ba.base != bb.base {
        return false;
    }
    let on_base = same_base(ba, bb);
    let position = if on_base {
        ba.local.distance(bb.local)
    } else {
        ta.translation.distance(tb.translation)
    };
    position <= TOLERANCE_POSITION_M
        && ta.rotation.angle_between(tb.rotation) <= TOLERANCE_ROTATION_RAD
        && ta.scale == tb.scale
        && state_matches(sa, sb, on_base)
}

fn character_matches(predicted: &PredictedValues<'_>, server: &PredictedValues<'_>) -> Option<bool> {
    let none = NetCharacterBase::default();
    Some(pose_and_state_match(
        (
            predicted.get::<Transform>()?,
            predicted.get::<CharacterState>()?,
            predicted.get::<NetCharacterBase>().unwrap_or(&none),
        ),
        (
            server.get::<Transform>()?,
            server.get::<CharacterState>()?,
            server.get::<NetCharacterBase>().unwrap_or(&none),
        ),
    ))
}

/// A rider's position error is the one on the base: the world error is mostly the base's lag.
fn character_error(predicted: &PredictedValues<'_>, server: &PredictedValues<'_>) -> Option<f32> {
    let (ba, bb) = (
        predicted.get::<NetCharacterBase>()?,
        server.get::<NetCharacterBase>()?,
    );
    same_base(ba, bb).then(|| ba.local.distance(bb.local))
}

// ---- the step -----------------------------------------------------------------------------------

/// The pose of a base: its `Transform`, or its `GlobalTransform` when it has a parent.
fn base_pose(world: &World, base: Entity) -> Option<(Vec3, Quat)> {
    if world.get::<ChildOf>(base).is_some()
        && let Some(global) = world.get::<regia_scene::GlobalTransform>(base)
    {
        let (_, rotation, translation) = global.0.to_scale_rotation_translation();
        return Some((translation, rotation));
    }
    world.get::<Transform>(base).map(|t| (t.translation, t.rotation))
}

fn base_entity(world: &World, base: NetId) -> Option<Entity> {
    world.get_resource::<NetEntities>()?.entity(base)
}

/// The largest rotation error the wire's quantization explains, radians.
const WIRE_ROTATION_ERROR_RAD: f32 = 0.01;

/// Undoes the quantization of a rotation that arrived from the wire (a spawn, a rollback).
fn restore_exact_yaw(world: &mut World, entity: Entity) {
    let Some(yaw) = world.get::<NetCharacterBase>(entity).map(|b| b.stepped_yaw) else {
        return;
    };
    let Some(mut transform) = world.get_mut::<Transform>(entity) else {
        return;
    };
    let exact = det_yaw_quat(yaw);
    if transform.rotation != exact && transform.rotation.angle_between(exact) <= WIRE_ROTATION_ERROR_RAD {
        transform.rotation = exact;
    }
}

/// Puts a rider back on its base; true when it is on one.
fn anchor_to_base(world: &mut World, entity: Entity) -> bool {
    let Some(rider) = world.get::<NetCharacterBase>(entity).cloned() else {
        return false;
    };
    let Some(id) = rider.base else {
        return false;
    };
    let Some((position, rotation)) = base_entity(world, id).and_then(|base| base_pose(world, base)) else {
        return false;
    };
    let Some(mut transform) = world.get_mut::<Transform>(entity) else {
        return false;
    };
    let local = rider.local + rotation.inverse() * (transform.translation - rider.stepped_at);
    let on_base = position + rotation * local;
    if transform.translation != on_base {
        transform.translation = on_base;
    }
    true
}

/// Records where the step left the character relative to what it stands on.
fn record_base(world: &mut World, entity: Entity, result: &MoveResult) {
    let id = result
        .ground_entity
        .filter(|ground| *ground != entity)
        .and_then(|ground| world.get::<NetId>(ground).copied().zip(Some(ground)));
    let (position, facing) = world
        .get::<Transform>(entity)
        .map_or((Vec3::ZERO, Quat::IDENTITY), |t| (t.translation, t.rotation));
    // The deterministic extraction (spec 15 § 5), not `to_euler`: both peers must store the same
    // bits, whatever their platform libm says.
    let stepped_yaw = det_yaw_of(facing);
    let next = match id.and_then(|(id, ground)| base_pose(world, ground).map(|pose| (id, pose))) {
        Some((id, (at, rotation))) => NetCharacterBase {
            base: Some(id),
            local: rotation.inverse() * (position - at),
            stepped_at: position,
            stepped_yaw,
        },
        None => NetCharacterBase {
            stepped_yaw,
            ..NetCharacterBase::default()
        },
    };
    if let Some(mut rider) = world.get_mut::<NetCharacterBase>(entity)
        && *rider != next
    {
        *rider = next;
    }
}

/// A rollback writes [`CharacterState`], the movement state machine reads
/// [`CharacterMovement::mode`]: the mode follows the state when someone other than the step wrote
/// it. A mode the game set on the settings component stays.
fn adopt_written_mode(world: &mut World, entity: Entity) {
    let (Some(state), Some(memo)) = (
        world.get::<CharacterState>(entity).map(|s| s.mode),
        world.get::<NetCharacterMemo>(entity).map(|m| m.mode),
    ) else {
        return;
    };
    if state != memo
        && let Some(mut movement) = world.get_mut::<CharacterMovement>(entity)
    {
        movement.mode = state;
    }
}

fn remember_mode(world: &mut World, entity: Entity) {
    let Some(mode) = world.get::<CharacterState>(entity).map(|s| s.mode) else {
        return;
    };
    if let Some(mut memo) = world.get_mut::<NetCharacterMemo>(entity) {
        memo.mode = mode;
    }
}

/// The playback of `play` on the rig, resolved once per play state. A rig that has no player yet
/// (its prefab is still loading) is asked again next step.
fn resolve_playback(world: &mut World, entity: Entity, rig: Entity, play: &NetMontagePlay, step: f32) {
    let key = (play.montage.clone(), play.section, play.rate.to_bits());
    if world
        .get::<NetMontageCache>(entity)
        .is_some_and(|c| c.key.as_ref() == Some(&key))
    {
        return;
    }
    let Some(player) = world.get::<MontagePlayer>(rig) else {
        if let Ok(mut ent) = world.get_entity_mut(entity) {
            ent.remove::<NetMontageCache>();
        }
        return;
    };
    let montage: Option<Arc<Montage>> = player.montage(&play.montage).cloned();
    let clips = player.clips.clone();
    if montage.is_none() {
        tracing::warn!(target: "net", montage = %play.montage, "net.montage_not_in_library");
    }
    let playback = montage.and_then(|montage| match montage.layout(|name| clips.get(name).cloned()) {
        Ok(layout) => Some(MontagePlayback::new(
            montage,
            Arc::new(layout),
            usize::from(play.section),
            0.0,
            play.rate,
            step,
        )),
        Err(error) => {
            tracing::warn!(target: "net", montage = %play.montage, %error, "net.montage_unresolved");
            None
        }
    });
    if let Ok(mut ent) = world.get_entity_mut(entity) {
        ent.insert(NetMontageCache {
            key: Some(key),
            playback,
        });
    }
}

/// Queues the root motion of `tick` for the step (§ 6.7) and ends a finished montage.
fn sample_root_motion(world: &mut World, entity: Entity, tick: NetTick, step: f32) {
    let Some(driven) = world
        .get::<RootMotionDriven>(entity)
        .filter(|d| d.mode != RootMotionApply::Off)
        .cloned()
    else {
        return;
    };
    let rig = driven.rig.unwrap_or(entity);
    let play = world.get::<NetMontagePlay>(entity).cloned().unwrap_or_default();
    let mut sampled = SampledRootMotion::NONE;
    if play.is_playing() && tick.0 >= play.start_tick {
        resolve_playback(world, entity, rig, &play, step);
        let index = tick.0 - play.start_tick;
        let playback = world
            .get::<NetMontageCache>(entity)
            .and_then(|c| c.playback.as_ref());
        if let Some(playback) = playback {
            if let Some((translation, rotation)) = playback.root_motion_of_step(index) {
                sampled = SampledRootMotion {
                    translation,
                    rotation,
                    seconds: step,
                };
            }
            if playback.cursor_after(index + 1).ended
                && let Some(mut play) = world.get_mut::<NetMontagePlay>(entity)
            {
                *play = NetMontagePlay::default();
            }
        }
    }
    if let Ok(mut ent) = world.get_entity_mut(entity) {
        ent.insert(sampled);
    }
}

/// One character's step in the predicted schedule.
fn step_one(
    world: &mut World,
    collision: &dyn CollisionQuery,
    entity: Entity,
    command: Option<(CharacterInput, f32)>,
    tick: NetTick,
    dt: f32,
) {
    adopt_written_mode(world, entity);
    restore_exact_yaw(world, entity);
    let anchored = anchor_to_base(world, entity);
    if let Some(mut mark) = world.get_mut::<CharacterAnchored>(entity)
        && mark.0 != anchored
    {
        mark.0 = anchored;
    }
    let input = match command {
        Some((input, yaw)) => {
            let faces_look = world
                .get::<CharacterMovement>(entity)
                .is_some_and(|m| m.rotation_mode == RotationMode::UseControllerYaw);
            if faces_look && let Some(mut transform) = world.get_mut::<Transform>(entity) {
                // The deterministic yaw quaternion (spec 15 § 5), not `Quat::from_rotation_y`:
                // both peers must write the same bits, whatever their platform libm says.
                let rotation = det_yaw_quat(yaw);
                if transform.rotation != rotation {
                    transform.rotation = rotation;
                }
            }
            if let Some(mut stored) = world.get_mut::<CharacterInput>(entity) {
                *stored = input.clone();
            }
            input
        }
        None => world.get::<CharacterInput>(entity).cloned().unwrap_or_default(),
    };
    sample_root_motion(world, entity, tick, dt);
    let result = step_character(world, collision, entity, &input, dt);
    record_base(world, entity, &result);
    remember_mode(world, entity);
}

/// The predicted system: steps every [`NetCharacter`] this process simulates, in `NetId` order.
pub fn net_character_step<I: CharacterCommand>(world: &mut World) {
    let _span = tracing::trace_span!(target: "regia_framework", "net_character_step").entered();
    let Some(state) = world.get_resource::<PredictionState>().copied() else {
        return;
    };
    let Some(collision) = world.remove_resource::<CharacterCollisionQuery>() else {
        return;
    };
    let commands: HashMap<NetPlayerId, I> = {
        let mut q = world.query::<(&NetPlayer, &InputCommand<I>)>();
        q.iter(world).map(|(p, c)| (p.id, c.value.clone())).collect()
    };
    let mut characters: Vec<(Option<NetId>, Entity, Option<NetPlayerId>)> = {
        let mut q = world.query::<(
            Entity,
            &NetCharacter,
            Option<&NetId>,
            Option<&NetOwner>,
            Option<&NetRole>,
            Option<&crate::net_bots::NetBotDriven>,
        )>();
        q.iter(world)
            .filter(|(_, _, _, _, role, _)| state.simulates(*role))
            .map(|(entity, character, id, owner, _, bot)| {
                // A reconnect takeover keeps the original owner for replication and scoring, but
                // the connectionless bot supplies CharacterInput until the player resumes.
                let player = if bot.is_some() {
                    None
                } else {
                    state.owner_player(owner).map(|mut player| {
                        player.local = character.local_player;
                        player
                    })
                };
                (id.copied(), entity, player)
            })
            .collect()
    };
    characters.sort_unstable_by_key(|(id, entity, _)| (*id, *entity));
    let dt = tick_step_seconds_of(world);
    for (_, entity, player) in characters {
        let command = player.map(|player| {
            commands
                .get(&player)
                .map_or_else(|| I::default().character_input(), I::character_input)
        });
        step_one(world, collision.query.as_ref(), entity, command, state.tick, dt);
    }
    world.insert_resource(collision);
}

/// `FixedUpdate`, after the predicted schedule: makes the rig show the play state that came out of
/// it. A start undone by a correction is stopped; a late start (a proxy, a correction) begins at
/// the time the montage has reached.
fn play_montage_visuals(world: &mut World) {
    let tick = world.get_resource::<PredictionState>().map_or(0, |s| s.tick.0);
    let interp_tick = world
        .get_resource::<NetClock>()
        .map_or(tick, |c| c.interp_tick.0 as u32);
    let step = tick_step_seconds_of(world);
    let rows: Vec<(Entity, Entity, NetMontagePlay, bool)> = {
        let mut q = world.query::<(
            Entity,
            &NetMontagePlay,
            Option<&RootMotionDriven>,
            Option<&NetRole>,
            Option<&NetMontageShown>,
        )>();
        q.iter(world)
            .filter(|(_, play, _, _, shown)| shown.map_or(play.is_playing(), |s| s.play != **play))
            .map(|(entity, play, driven, role, _)| {
                (
                    entity,
                    driven.and_then(|d| d.rig).unwrap_or(entity),
                    play.clone(),
                    matches!(role, Some(NetRole::Proxy)),
                )
            })
            .collect()
    };
    for (entity, rig, play, proxy) in rows {
        let old = world.get::<NetMontageShown>(entity).and_then(|s| s.id);
        let now = if proxy { interp_tick } else { tick };
        let id = world.get_mut::<MontagePlayer>(rig).and_then(|mut player| {
            // A montage that ran to its end is gone from the player already.
            if old.is_some_and(|id| player.status(id).is_some()) {
                player.stop(old, None);
            }
            if !play.is_playing() {
                return None;
            }
            let section = player
                .montage(&play.montage)
                .map(|m| m.section_name(usize::from(play.section)).to_string());
            player.play_named(
                &play.montage,
                PlayOptions {
                    rate: play.rate,
                    section,
                    start_time: now.saturating_sub(play.start_tick) as f32 * step * play.rate,
                    ..PlayOptions::default()
                },
            )
        });
        if let Ok(mut ent) = world.get_entity_mut(entity) {
            ent.insert(NetMontageShown { play, id });
        }
    }
}

/// `RapierCharacterQuery`'s settings for a step in the predicted schedule (§ 9).
///
/// A re-run tick pushes nothing: the first run already did and the server applies pushes for real
/// (`move_character` applies no impulse without mass). A networked character treats dynamic bodies
/// like any other ground: they are kinematic proxies on the client, and the sweep must not depend on
/// a body's kind or the two sides walk differently on the same deck.
pub(crate) fn predicted_body_settings(
    world: &World,
    entity: Entity,
    mut settings: regia_physics::CharacterBody,
) -> regia_physics::CharacterBody {
    if world.get_resource::<NetResimulating>().is_some_and(|r| r.0) {
        settings.mass = 0.0;
    }
    if world.get::<NetCharacter>(entity).is_some() {
        settings.autostep_include_dynamic = true;
    }
    settings
}

// ---- installation -------------------------------------------------------------------------------

/// The character tolerance of § 9 over `Transform`, [`CharacterState`] and [`NetCharacterBase`].
pub fn character_tolerance() -> EntityTolerance {
    EntityTolerance {
        covers: vec![
            TypeId::of::<Transform>(),
            TypeId::of::<CharacterState>(),
            TypeId::of::<NetCharacterBase>(),
        ],
        matches: character_matches,
        error_m: character_error,
    }
}

/// Installs networked characters on a world with input command `I`: what [`NetCharacterPlugin`]
/// does for [`CharacterNetInput`]. A `predict::<Transform>` the game made first is kept (it applies
/// to its other predicted entities).
///
/// # Errors
///
/// [`PredictError::InputAlreadyRegistered`]: the game registered another input type.
pub fn install_net_character<I: CharacterCommand>(world: &mut World) -> Result<(), PredictError> {
    register_replicated::<NetCharacter>(world, ReplicateOptions::default());
    register_replicated::<CharacterState>(world, ReplicateOptions::default());
    register_replicated::<NetCharacterBase>(world, ReplicateOptions::default());
    register_replicated::<NetMontagePlay>(world, ReplicateOptions::default());
    // Only a character with `TraversalSettings` has one (`docs/specs/traversal.md` § 8).
    register_replicated::<CharacterTraversal>(world, ReplicateOptions::default());
    add_net_input::<I>(world)?;
    let pose = predict::<Transform>(
        world,
        Tolerance::pose(TOLERANCE_POSITION_M, TOLERANCE_ROTATION_RAD),
    );
    if !matches!(pose, Ok(()) | Err(PredictError::AlreadyPredicted(_))) {
        return pose;
    }
    predict::<CharacterState>(world, Tolerance::Exact)?;
    predict::<NetCharacterBase>(world, Tolerance::Exact)?;
    predict::<NetMontagePlay>(world, Tolerance::Exact)?;
    predict::<CharacterTraversal>(world, Tolerance::Fn(crate::traversal::net::traversal_matches))?;
    predict_entity_tolerance(world, character_tolerance());
    // Derived each step (the mode follows the state, the input comes from the command).
    predict_local_writes::<CharacterMovement>(world);
    predict_local_writes::<CharacterInput>(world);
    predict_local_writes::<CharacterAnchored>(world);
    predict_local_writes::<NetCharacterMemo>(world);
    predict_local_writes::<NetMontageCache>(world);
    predict_local_writes::<regia_physics::CharacterBody>(world);
    add_predicted_systems(world, net_character_step::<I>);
    Ok(())
}

/// Networked characters with [`CharacterNetInput`] (§ 9). Add after `NetPlugin` and
/// [`CharacterPlugin`](crate::character::CharacterPlugin). The net step runs after the physics step,
/// like the movement system. Games with their own input type call [`install_net_character`] and
/// [`add_montage_visuals`] instead.
pub struct NetCharacterPlugin;

impl Plugin for NetCharacterPlugin {
    fn build(&self, app: &mut App) {
        if let Err(error) = install_net_character::<CharacterNetInput>(app.world_mut()) {
            tracing::error!(target: "net", %error, "net.character_input_refused");
        }
        add_montage_visuals(app);
    }
}

type NewProxyBody = Or<(Changed<RigidBody>, Added<NetRole>)>;

/// `FixedUpdate`: a body the server simulates only follows on a client. A dynamic proxy turns
/// kinematic, so rapier takes its pose from the replicated `Transform` and riders stand on it.
fn proxy_bodies_follow(mut bodies: Query<(&mut RigidBody, &NetRole), NewProxyBody>) {
    for (mut body, role) in &mut bodies {
        if *role == NetRole::Proxy && body.kind == BodyKind::Dynamic {
            body.kind = BodyKind::Kinematic;
        }
    }
}

/// Orders the net step after the physics step, makes proxy bodies follow the server and adds the
/// system that shows [`NetMontagePlay`] on rigs.
pub fn add_montage_visuals(app: &mut App) {
    app.register_type::<NetCharacter>()
        .configure_sets(
            FixedUpdate,
            (
                NetSet::Apply.before(PhysicsStep),
                NetSet::Predict.after(PhysicsStep),
            ),
        )
        .add_systems(FixedUpdate, proxy_bodies_follow.before(PhysicsStep))
        .add_systems(
            FixedUpdate,
            play_montage_visuals.after(NetSet::Predict).before(NetSet::Assign),
        );
}

// ---- the agent surface --------------------------------------------------------------------------

/// One character of [`net_character_rows`].
#[derive(Debug, Clone, PartialEq)]
pub struct NetCharacterRow {
    /// The entity.
    pub entity: Entity,
    /// Its network id, once assigned.
    pub net_id: Option<NetId>,
    /// `authority`, `predicted` or `proxy`.
    pub role: &'static str,
    /// The current position.
    pub position: Vec3,
    /// The current velocity.
    pub velocity: Vec3,
    /// On the ground.
    pub grounded: bool,
    /// The movement mode.
    pub mode: MovementMode,
    /// The base it stands on.
    pub base: Option<NetId>,
    /// Its position on the base (the world position without one).
    pub base_local: Vec3,
    /// The montage root motion comes from (`None`: nothing plays).
    pub montage: Option<NetMontagePlay>,
}

/// Whether [`install_net_character`] ran on this world.
pub fn net_characters_installed(world: &World) -> bool {
    world.get_resource::<PredictionRegistry>().is_some_and(|r| {
        r.component_names()
            .contains(&std::any::type_name::<CharacterState>())
    })
}

/// Every networked character in this process, in entity order.
pub fn net_character_rows(world: &mut World) -> Vec<NetCharacterRow> {
    let mut q = world.query::<(
        Entity,
        &NetCharacter,
        &Transform,
        &CharacterState,
        &NetCharacterBase,
        &NetMontagePlay,
        Option<&NetId>,
        Option<&NetRole>,
    )>();
    let mut rows: Vec<NetCharacterRow> = q
        .iter(world)
        .map(
            |(entity, _, transform, state, base, play, id, role)| NetCharacterRow {
                entity,
                net_id: id.copied(),
                role: match role {
                    Some(NetRole::Predicted) => "predicted",
                    Some(NetRole::Proxy) => "proxy",
                    _ => "authority",
                },
                position: transform.translation,
                velocity: state.velocity,
                grounded: state.grounded,
                mode: state.mode,
                base: base.base,
                base_local: if base.base.is_some() {
                    base.local
                } else {
                    transform.translation
                },
                montage: play.is_playing().then(|| play.clone()),
            },
        )
        .collect();
    rows.sort_by_key(|r| r.entity);
    rows
}

// ---- remote animation (ADR-2551) ----------------------------------------------------------------

/// Descendants searched for the animated rig under a replicated root.
const ANIM_RIG_SEARCH_LIMIT: usize = 64;

/// Fraction of a tick under which a shown tick counts as the next state's tick
/// ([`lerp_anim_state`]).
const ANIM_TICK_SNAP: f32 = 1e-3;

/// The replicated animation state of a character's rig (ADR-2551): the server's playback state
/// and declared parameters on one tick. Interpolated on clients (`regia_net::interp`): the float
/// and vector parameters blend, the rest step, and [`net_anim_drive`] drives the proxy's
/// [`Animator`] from it, so a remote character animates from its server's state at the
/// interpolation delay instead of from its own clock.
///
/// Add it to a replicated root whose rig (the root or a descendant with an [`Animator`]) plays the
/// same graph on every process; [`NetAnimationPlugin`] does the rest. Graph parameters travel in
/// the order the graph declares them: declare every parameter the graph reads.
#[derive(Component, Debug, Clone, Default, PartialEq, Replicate)]
pub struct NetAnimState {
    /// The server tick the state was captured on (raw [`NetTick`]).
    pub tick: u32,
    /// The playback state (`regia_animation::pose_eval::AnimStateSnapshot::words`).
    #[replicate(max_len = 512)]
    pub words: Vec<u32>,
    /// The declared parameters' lanes (`ParamLayout::declared_shared` builds them once per graph).
    #[replicate(max_len = 96)]
    pub params: Vec<u32>,
    /// Bit `i` set: parameter lane `i` blends (a float or vector component); lanes past 63 step.
    pub blend_mask: u64,
    /// Client: ticks the shown interpolation tick is past `tick` (the blend writes it; 0 on the
    /// server).
    pub past: f32,
}

/// `interpolate` blend of [`NetAnimState`]: the older state with the shown fraction, the lanes
/// of [`NetAnimState::blend_mask`] blended, the rest stepped.
pub fn lerp_anim_state(a: &NetAnimState, b: &NetAnimState, t: f32) -> NetAnimState {
    let t = if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 };
    // A shown tick a rounding error short of `b` is `b`: re-anchoring on `a` with a whole tick of
    // lead would cover two server evaluations with one and shift a state change by a tick.
    let span = b.tick.saturating_sub(a.tick) as f32;
    if span > 0.0 && (1.0 - t) * span < ANIM_TICK_SNAP {
        let mut out = b.clone();
        out.past = 0.0;
        return out;
    }
    let mut out = a.clone();
    out.past = t * span;
    if t > 0.0 && a.params.len() == b.params.len() && a.blend_mask == b.blend_mask {
        for (i, (o, y)) in out.params.iter_mut().zip(&b.params).enumerate() {
            if i >= 64 || a.blend_mask & (1u64 << i) == 0 {
                continue;
            }
            let (x, y) = (f32::from_bits(*o), f32::from_bits(*y));
            let v = x + (y - x) * t;
            if v.is_finite() {
                *o = v.to_bits();
            }
        }
    }
    out
}

/// Client: the tick of the state last restored into the rig's animator.
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetAnimShown {
    /// The [`NetAnimState::tick`] restored.
    pub tick: u32,
}

#[cfg(test)]
thread_local! {
    /// Parameter layouts the net animation systems looked up on this thread (tests call the
    /// systems directly, so the count is race-free under parallel tests).
    static LAYOUT_BUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one parameter layout lookup (tests).
#[cfg(test)]
fn note_layout_build() {
    LAYOUT_BUILDS.with(|c| c.set(c.get() + 1));
}

/// Parameter layout lookups on this thread so far (tests).
#[cfg(test)]
fn layout_builds() -> u64 {
    LAYOUT_BUILDS.with(std::cell::Cell::get)
}

/// The rig animated for `root`: the root, or its nearest descendant with an [`Animator`].
fn anim_rig(world: &World, root: Entity) -> Option<Entity> {
    anim_rig_in(world, root, &mut std::collections::VecDeque::new())
}

/// [`anim_rig`] with a caller-kept search queue: the same breadth-first walk, no heap request
/// once the queue's capacity covers the walk (none at all when the root is the rig).
fn anim_rig_in(
    world: &World,
    root: Entity,
    queue: &mut std::collections::VecDeque<Entity>,
) -> Option<Entity> {
    let is_rig = |ent: &EntityRef| ent.contains::<Animator>() && ent.contains::<SkinnedMesh>();
    let root_ent = world.get_entity(root).ok()?;
    if is_rig(&root_ent) {
        return Some(root);
    }
    queue.clear();
    if let Some(c) = root_ent.get::<Children>() {
        queue.extend(c.iter());
    }
    let mut seen = 0usize;
    while let Some(e) = queue.pop_front() {
        let ent = world.get_entity(e).ok()?;
        if is_rig(&ent) {
            return Some(e);
        }
        seen += 1;
        if seen >= ANIM_RIG_SEARCH_LIMIT {
            return None;
        }
        if let Some(c) = ent.get::<Children>() {
            queue.extend(c.iter());
        }
    }
    None
}

/// The parameter layout a replicated root's rig was last captured or driven with, kept so a tick
/// does not look the layout up again (audit framework4 finding 3). Keyed by the graph `Arc`
/// itself (held, so its address cannot be reused by another graph while the key lives).
#[derive(Component, Clone, Debug)]
struct NetAnimLayout {
    /// The animator's graph (`None`: a single-clip animator, which declares no parameters).
    graph: Option<Arc<regia_animation::AnimGraph>>,
    /// The declared lanes (`ParamLayout::declared_shared`).
    layout: Arc<ParamLayout>,
    /// `layout.blend_mask()` packed into the replicated bitset.
    blend_mask: u64,
}

fn blend_mask_of(layout: &ParamLayout) -> u64 {
    layout
        .blend_mask()
        .iter()
        .take(64)
        .enumerate()
        .fold(0u64, |m, (i, b)| if *b { m | (1u64 << i) } else { m })
}

/// The layout for `animator`: the root's cached one while the graph is the same `Arc`, else a
/// new lookup and the component to store (third value).
fn layout_for(
    cached: Option<&NetAnimLayout>,
    animator: &Animator,
) -> (Arc<ParamLayout>, u64, Option<NetAnimLayout>) {
    let graph = match &animator.source {
        AnimatorSource::Graph { graph, .. } => Some(graph),
        AnimatorSource::SingleClip { .. } => None,
    };
    if let Some(cached) = cached {
        let same = match (&cached.graph, graph) {
            (Some(a), Some(b)) => Arc::ptr_eq(a, b),
            (None, None) => true,
            _ => false,
        };
        if same {
            return (Arc::clone(&cached.layout), cached.blend_mask, None);
        }
    }
    #[cfg(test)]
    note_layout_build();
    let layout = graph.map_or_else(
        || Arc::new(ParamLayout::default()),
        |graph| ParamLayout::declared_shared(graph),
    );
    let blend_mask = blend_mask_of(&layout);
    let fresh = NetAnimLayout {
        graph: graph.cloned(),
        layout: Arc::clone(&layout),
        blend_mask,
    };
    (layout, blend_mask, Some(fresh))
}

/// Buffers the net animation systems keep in the world between runs, so a steady tick or frame
/// makes no heap request (audit framework4 finding 3).
#[derive(Resource, Default)]
struct NetAnimScratch {
    /// Roots with a [`NetAnimState`] (server capture).
    capture_query: Option<QueryState<Entity, With<NetAnimState>>>,
    /// Roots and their state (client drive).
    drive_query: Option<QueryState<(Entity, &'static NetAnimState)>>,
    /// This run's roots.
    roots: Vec<Entity>,
    /// The rig search queue ([`anim_rig_in`]).
    queue: std::collections::VecDeque<Entity>,
    /// Captured words (server) or the shown state's words (client re-anchor).
    words: Vec<u32>,
    /// Encoded (server) or shown (client) parameter lanes.
    params: Vec<u32>,
}

/// Runs `f` with the world's [`NetAnimScratch`] (inserted on first use).
fn with_net_anim_scratch(world: &mut World, f: impl FnOnce(&mut World, &mut NetAnimScratch)) {
    if !world.contains_resource::<NetAnimScratch>() {
        world.init_resource::<NetAnimScratch>();
    }
    world.resource_scope(|world, mut scratch: Mut<NetAnimScratch>| f(world, &mut scratch));
}

/// Server, each fixed step before the state is gathered: writes every [`NetAnimState`] from its
/// rig's animator (the state the tick's evaluation starts from and its declared parameters). A rig
/// whose state a snapshot cannot hold (an inertialized transition, sync groups) keeps its last
/// written state; the code is logged once per entity. The parameter layout is looked up when the
/// rig's graph changes, and the state is rewritten in place in the component's buffers.
pub fn net_anim_capture(world: &mut World) {
    let _span = tracing::trace_span!("net_anim_capture").entered();
    if !world.get_resource::<NetMode>().is_some_and(|m| m.is_server()) {
        return;
    }
    let tick = world.get_resource::<NetNode>().map_or(0, |n| n.tick.0);
    with_net_anim_scratch(world, |world, scratch| capture_all(world, tick, scratch));
}

fn capture_all(world: &mut World, tick: u32, scratch: &mut NetAnimScratch) {
    let NetAnimScratch {
        capture_query,
        roots,
        queue,
        words,
        params,
        ..
    } = scratch;
    let query = capture_query.get_or_insert_with(|| world.query_filtered::<Entity, With<NetAnimState>>());
    roots.clear();
    roots.extend(query.iter(world));
    for &root in roots.iter() {
        let Some(rig) = anim_rig_in(world, root, queue) else {
            continue;
        };
        let Some(animator) = world.get::<Animator>(rig) else {
            continue;
        };
        match capture_state_into(animator, words) {
            Ok(()) => {}
            Err(CaptureError::NotEvaluated) => continue,
            Err(e) => {
                if world.get::<NetAnimRefused>(root).is_none() {
                    tracing::warn!(target: "net", code = e.code(), entity = root.to_bits(), "net.anim_state_refused");
                    if let Ok(mut ent) = world.get_entity_mut(root) {
                        ent.insert(NetAnimRefused);
                    }
                }
                continue;
            }
        }
        let (layout, blend_mask, fresh) = layout_for(world.get::<NetAnimLayout>(root), animator);
        layout.encode_into(&animator.parameters, params);
        let Ok(mut ent) = world.get_entity_mut(root) else {
            continue;
        };
        if ent.contains::<NetAnimRefused>() {
            ent.remove::<NetAnimRefused>();
        }
        if let Some(fresh) = fresh {
            ent.insert(fresh);
        }
        if let Some(mut current) = ent.get_mut::<NetAnimState>()
            && (current.tick != tick
                || current.words != *words
                || current.params != *params
                || current.blend_mask != blend_mask
                || current.past != 0.0)
        {
            let current = &mut *current;
            current.tick = tick;
            current.words.clone_from(words);
            current.params.clone_from(params);
            current.blend_mask = blend_mask;
            current.past = 0.0;
        }
    }
}

/// Server: the rig's state was refused by the snapshot (logged once).
#[derive(Component, Debug, Clone, Copy)]
struct NetAnimRefused;

/// Client, each frame before the animators evaluate: drives every interpolated proxy's rig from
/// its shown [`NetAnimState`]. When the shown state's tick changes, the animator is re-anchored on
/// it with all its parameters ([`restore_state_ahead`]: the fraction of a tick the interpolation is
/// past the state is added to the next evaluation's delta); in between it plays on with the
/// blended float and vector parameters. A locally owned or predicted copy is left alone (its own
/// game code animates it).
pub fn net_anim_drive(world: &mut World) {
    let _span = tracing::trace_span!("net_anim_drive").entered();
    if world.get_resource::<NetMode>().is_some_and(|m| m.is_server()) {
        return;
    }
    let tick_secs = tick_step_seconds_of(world);
    with_net_anim_scratch(world, |world, scratch| drive_all(world, tick_secs, scratch));
}

fn drive_all(world: &mut World, tick_secs: f32, scratch: &mut NetAnimScratch) {
    let NetAnimScratch {
        drive_query,
        roots,
        queue,
        words,
        params,
        ..
    } = scratch;
    let query = drive_query.get_or_insert_with(|| world.query::<(Entity, &NetAnimState)>());
    roots.clear();
    roots.extend(
        query
            .iter(world)
            .filter(|(_, state)| !state.words.is_empty())
            .map(|(entity, _)| entity),
    );
    for &root in roots.iter() {
        if !regia_net::interp::is_interpolated_proxy(world, root) {
            continue;
        }
        let Some(state) = world.get::<NetAnimState>(root) else {
            continue;
        };
        let reanchor = world.get::<NetAnimShown>(root).map(|shown| shown.tick) != Some(state.tick);
        let (state_tick, state_past) = (state.tick, state.past);
        params.clone_from(&state.params);
        if reanchor {
            words.clone_from(&state.words);
        }
        let Some(rig) = anim_rig_in(world, root, queue) else {
            continue;
        };
        let Some(animator) = world.get::<Animator>(rig) else {
            continue;
        };
        let (layout, _, fresh) = layout_for(world.get::<NetAnimLayout>(root), animator);
        if let Some(fresh) = fresh
            && let Ok(mut ent) = world.get_entity_mut(root)
        {
            ent.insert(fresh);
        }
        let Some(mut animator) = world.get_mut::<Animator>(rig) else {
            continue;
        };
        if !reanchor {
            layout.apply_blending(params, &mut animator.parameters);
            continue;
        }
        layout.apply(params, &mut animator.parameters);
        let snapshot = AnimStateSnapshot::from_words(words);
        let past = if state_past.is_finite() {
            state_past.max(0.0)
        } else {
            0.0
        };
        if let Err(error) = restore_state_ahead(&mut animator, &snapshot, past * tick_secs) {
            tracing::debug!(target: "net", code = error.code(), entity = root.to_bits(), "net.anim_state_mismatch");
            continue;
        }
        if let Ok(mut ent) = world.get_entity_mut(root) {
            ent.insert(NetAnimShown { tick: state_tick });
        }
    }
}

/// One entity's remote-animation state (agent `net.anim_state`).
#[derive(Debug, Clone, PartialEq)]
pub struct NetAnimReport {
    /// `server`, `proxy` (an interpolated client copy the drive animates) or `local` (a client
    /// copy its own game code animates: owned or predicted).
    pub role: &'static str,
    /// The state's server tick.
    pub tick: u32,
    /// Ticks the shown interpolation tick is past it (clients).
    pub past: f32,
    /// The tick last restored into the rig's animator (proxies).
    pub shown_tick: Option<u32>,
    /// Words of the playback state.
    pub words: usize,
    /// Parameter lanes, decoded as `f32` where they blend and as raw `u32` otherwise.
    pub params: Vec<f64>,
    /// The rig entity (bits), when found.
    pub rig: Option<u64>,
    /// The rig's current state-machine state, when it plays a state machine.
    pub rig_state: Option<String>,
    /// The server refused the rig's state (an inertialized transition, sync groups).
    pub refused: bool,
}

/// Why `net.anim_state` failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetAnimQueryError {
    /// No `regia_net` runtime: `net_not_running`.
    NotRunning,
    /// The entity does not exist: `entity_not_found`.
    NotFound,
    /// The entity has no [`NetAnimState`]: `entity_not_anim_replicated`.
    NotAnimated,
}

impl NetAnimQueryError {
    /// The protocol code.
    pub fn code(self) -> &'static str {
        match self {
            Self::NotRunning => "net_not_running",
            Self::NotFound => "entity_not_found",
            Self::NotAnimated => "entity_not_anim_replicated",
        }
    }
}

/// Data of `net.anim_state`.
///
/// # Errors
///
/// [`NetAnimQueryError`].
pub fn net_anim_report(world: &World, entity: Entity) -> Result<NetAnimReport, NetAnimQueryError> {
    if world.get_resource::<NetNode>().is_none() {
        return Err(NetAnimQueryError::NotRunning);
    }
    let ent = world
        .get_entity(entity)
        .map_err(|_| NetAnimQueryError::NotFound)?;
    let state = ent.get::<NetAnimState>().ok_or(NetAnimQueryError::NotAnimated)?;
    let server = world.get_resource::<NetMode>().is_some_and(|m| m.is_server());
    let role = if server {
        "server"
    } else if regia_net::interp::is_interpolated_proxy(world, entity) {
        "proxy"
    } else {
        "local"
    };
    let rig = anim_rig(world, entity);
    let params = state
        .params
        .iter()
        .enumerate()
        .map(|(i, v)| {
            if i < 64 && state.blend_mask & (1u64 << i) != 0 {
                f64::from(f32::from_bits(*v))
            } else {
                f64::from(*v)
            }
        })
        .collect();
    Ok(NetAnimReport {
        role,
        tick: state.tick,
        past: state.past,
        shown_tick: ent.get::<NetAnimShown>().map(|s| s.tick),
        words: state.words.len(),
        params,
        rig: rig.map(Entity::to_bits),
        rig_state: rig
            .and_then(|r| world.get::<Animator>(r))
            .and_then(Animator::current_state),
        refused: ent.contains::<NetAnimRefused>(),
    })
}

/// Registers [`NetAnimState`] (replicated, interpolated) on a world (what [`NetAnimationPlugin`]
/// does, for worlds without an `App`). Every process registers it.
pub fn install_net_animation(world: &mut World) {
    register_replicated::<NetAnimState>(world, ReplicateOptions::default());
    regia_net::interp::register_interpolated::<NetAnimState>(world, lerp_anim_state);
}

/// Remote characters animate from replicated state (ADR-2551): [`NetAnimState`] replicated and
/// interpolated, [`net_anim_capture`] on the server at the end of each fixed step (after the
/// predicted step, before ids are assigned), [`net_anim_drive`] on clients before the animators
/// evaluate. Opt-in: add it after `NetPlugin` (and `NetInterpolationPlugin` on clients) on every
/// process.
#[derive(Debug, Clone, Copy, Default)]
pub struct NetAnimationPlugin;

impl Plugin for NetAnimationPlugin {
    fn build(&self, app: &mut App) {
        install_net_animation(app.world_mut());
        app.add_systems(
            FixedUpdate,
            net_anim_capture.after(NetSet::Predict).before(NetSet::Assign),
        )
        .add_systems(
            Update,
            net_anim_drive.before(regia_animation::systems::evaluate_animators),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_net::codec::encode_to_vec;

    fn round_trip(v: &CharacterNetInput) -> CharacterNetInput {
        let cx = CodecContext::detached();
        let bytes = encode_to_vec(v, &cx);
        let mut r = BitReader::new(&bytes);
        CharacterNetInput::decode(&mut r, &cx).expect("decode")
    }

    /// The wire value is what `quantized` gives, quantizing twice changes nothing, zero is exact.
    #[test]
    fn input_quantization_is_the_wire_value_and_idempotent() {
        let v = CharacterNetInput {
            move_dir: Vec2::new(0.337, -1.0),
            yaw: 1.2345,
            pitch: -0.4321,
            jump: true,
            crouch: false,
            sprint: true,
        };
        let q = v.quantized();
        assert_eq!(round_trip(&v), q);
        assert_eq!(q.quantized(), q);
        assert_eq!(round_trip(&q), q);
        assert_eq!(CharacterNetInput::default().quantized().move_dir, Vec2::ZERO);
        assert_eq!(dequantize_axis(quantize_axis(f32::NAN)), 0.0);
        assert_eq!(dequantize_axis(quantize_axis(9.0)), 1.0);
    }

    /// rc1-rev-net-k 6: the encoder never writes `i8::MIN` for a stick axis (it decodes to about
    /// -1.008, past the unit stick), so a command carrying it is refused like the vehicle's.
    #[test]
    fn a_stick_axis_the_encoder_cannot_write_is_refused() {
        let cx = CodecContext::detached();
        for (x, y) in [(0x80u64, 0u64), (0, 0x80), (0x80, 0x80)] {
            let mut w = BitWriter::new();
            w.write_bits(x, 8);
            w.write_bits(y, 8);
            w.write_bits(0, LOOK_ANGLE.bits);
            w.write_bits(0, LOOK_ANGLE.bits);
            w.write_bool(false);
            w.write_bool(false);
            w.write_bool(false);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            assert!(
                matches!(
                    CharacterNetInput::decode(&mut r, &cx),
                    Err(DecodeError::Invalid(CODE_MOVE_RANGE))
                ),
                "({x:#x}, {y:#x})"
            );
        }
        // The encoder's own extremes still read back.
        let edge = CharacterNetInput {
            move_dir: Vec2::new(-1.0, 1.0),
            ..CharacterNetInput::default()
        };
        assert_eq!(round_trip(&edge).move_dir, Vec2::new(-1.0, 1.0));
    }

    /// Every value the wire can carry survives another round: the server re-quantizes what it
    /// decoded, and the client's prediction must match it.
    #[test]
    fn every_wire_value_is_a_fixed_point() {
        for q in -127..=127i8 {
            assert_eq!(quantize_axis(dequantize_axis(q)), q);
        }
        for q in 0..(1u64 << LOOK_ANGLE.bits) {
            assert_eq!(LOOK_ANGLE.quantize(LOOK_ANGLE.dequantize(q)), q);
        }
    }

    /// A delta against the next newer command reads back, and an unchanged stick costs bits only
    /// for its flags.
    #[test]
    fn input_delta_round_trips_and_is_small() {
        let cx = CodecContext::detached();
        let base = CharacterNetInput {
            move_dir: Vec2::new(0.5, 0.25),
            yaw: 0.5,
            pitch: 0.1,
            ..CharacterNetInput::default()
        }
        .quantized();
        let next = CharacterNetInput {
            jump: true,
            ..base.clone()
        };
        let mut w = BitWriter::new();
        next.encode_delta(&base, &mut w, &cx);
        assert!(w.bit_len() <= 5, "{}", w.bit_len());
        let bytes = w.as_bytes().to_vec();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            CharacterNetInput::decode_delta(&base, &mut r, &cx).expect("delta"),
            next
        );
        let moved = CharacterNetInput {
            move_dir: Vec2::new(-1.0, 0.0),
            yaw: -2.0,
            ..base.clone()
        }
        .quantized();
        let mut w = BitWriter::new();
        moved.encode_delta(&base, &mut w, &cx);
        let bytes = w.as_bytes().to_vec();
        let mut r = BitReader::new(&bytes);
        assert_eq!(
            CharacterNetInput::decode_delta(&base, &mut r, &cx).expect("delta"),
            moved
        );
    }

    /// § 5.2: a held stick repeats, a jump does not.
    #[test]
    fn a_missing_command_repeats_the_stick_and_not_the_jump() {
        let last = CharacterNetInput {
            move_dir: Vec2::Y,
            jump: true,
            sprint: true,
            ..CharacterNetInput::default()
        };
        let next = CharacterNetInput::on_missing(&last);
        assert!(!next.jump);
        assert_eq!(next.move_dir, Vec2::Y);
        assert!(next.sprint);
    }

    /// `from_character_input` and the command's mapping are inverses (up to quantization).
    #[test]
    fn world_move_dir_survives_the_look_frame() {
        let input = CharacterInput {
            move_dir: Vec3::new(0.6, 0.0, -0.8),
            ..CharacterInput::default()
        };
        let command = CharacterNetInput::from_character_input(&input, 1.1, 0.0);
        let (back, yaw) = command.character_input();
        assert!(
            back.move_dir.distance(input.move_dir) < 0.02,
            "{:?}",
            back.move_dir
        );
        assert!((yaw - 1.1).abs() < 1e-3);
    }

    fn values() -> (Transform, CharacterState, NetCharacterBase) {
        (
            Transform::from_translation(Vec3::new(1.0, 2.0, 3.0)),
            CharacterState {
                grounded: true,
                ..CharacterState::default()
            },
            NetCharacterBase::default(),
        )
    }

    fn check(change: impl FnOnce(&mut Transform, &mut CharacterState, &mut NetCharacterBase)) -> bool {
        let a = values();
        let mut b = values();
        change(&mut b.0, &mut b.1, &mut b.2);
        pose_and_state_match((&a.0, &a.1, &a.2), (&b.0, &b.1, &b.2))
    }

    /// § 9: each field's tolerance, inside and outside.
    #[test]
    fn tolerance_matrix() {
        assert!(check(|_, _, _| {}));
        assert!(check(|t, _, _| t.translation.x += 0.009));
        assert!(!check(|t, _, _| t.translation.x += 0.011));
        assert!(check(|t, _, _| t.rotation = Quat::from_rotation_y(0.009)));
        assert!(!check(|t, _, _| t.rotation = Quat::from_rotation_y(0.02)));
        assert!(check(|_, s, _| s.velocity.x = 0.04));
        assert!(!check(|_, s, _| s.velocity.x = 0.06));
        assert!(check(|_, s, _| s.time_in_air = 0.004));
        assert!(!check(|_, s, _| s.time_in_air = 0.005));
        assert!(!check(|_, s, _| s.coyote_time_left = 0.005));
        assert!(!check(|_, s, _| s.jump_buffer_time_left = 0.005));
        assert!(!check(|_, s, _| s.mode = MovementMode::Falling));
        assert!(!check(|_, s, _| s.grounded = false));
        assert!(!check(|_, s, _| s.jumps_used = 1));
        assert!(!check(|_, s, _| s.crouched = true));
        assert!(
            check(|_, s, _| s.landed_this_frame = true),
            "recomputed, never compared"
        );
        assert!(!check(|_, s, _| s.ground_velocity.x = 0.06));
    }

    /// § 6.6: a rider compares its place on the base, whatever the world says; different bases
    /// never match.
    #[test]
    fn a_rider_compares_on_the_base() {
        let rider = |id: u32, x: f32| NetCharacterBase {
            base: Some(NetId(id)),
            local: Vec3::new(x, 1.0, 0.0),
            stepped_at: Vec3::ZERO,
            stepped_yaw: 0.0,
        };
        let (t, s, _) = values();
        let far = Transform::from_translation(Vec3::new(9.0, 2.0, 3.0));
        let late = CharacterState {
            ground_velocity: Vec3::X,
            ..s.clone()
        };
        assert!(pose_and_state_match(
            (&t, &s, &rider(4, 0.5)),
            (&far, &late, &rider(4, 0.505))
        ));
        assert!(!pose_and_state_match(
            (&t, &s, &rider(4, 0.5)),
            (&far, &s, &rider(4, 0.52))
        ));
        assert!(!pose_and_state_match(
            (&t, &s, &rider(4, 0.5)),
            (&t, &s, &rider(5, 0.5))
        ));
        assert!(!pose_and_state_match(
            (&t, &s, &rider(4, 0.5)),
            (&t, &s, &NetCharacterBase::default())
        ));
    }

    /// A child base's pose is its global one; a root base's is its `Transform`.
    #[test]
    fn a_base_pose_reads_the_right_transform() {
        let mut world = World::new();
        let parent = world.spawn_empty().id();
        let at = Vec3::new(1.0, 2.0, 3.0);
        let global = regia_scene::GlobalTransform(regia_core::glam::Affine3A::from_translation(at));
        let child = world.spawn((Transform::default(), global, ChildOf(parent))).id();
        assert_eq!(base_pose(&world, child).map(|p| p.0), Some(at));
        let root = world
            .spawn((
                Transform::from_translation(at),
                regia_scene::GlobalTransform::default(),
            ))
            .id();
        assert_eq!(base_pose(&world, root).map(|p| p.0), Some(at));
    }

    /// Determinism (spec 15 § 5): the predicted step builds the facing and the stick basis from
    /// the deterministic kernel, not the platform libm. Yaw 0.75 quantizes to a value whose
    /// half-angle sine is one ulp off glibc's, so the libm path fails this test on glibc; on
    /// other yaws glibc and the Windows CRT disagree with each other. The pins hold on every
    /// target.
    #[test]
    fn predicted_yaw_matches_the_deterministic_kernel() {
        struct FlatGround;
        impl CollisionQuery for FlatGround {
            #[allow(clippy::too_many_arguments)] // the trait's shape
            fn move_shape(
                &self,
                _world: &mut World,
                _entity: Entity,
                position: Vec3,
                _radius: f32,
                _half_height: f32,
                delta: Vec3,
                _dt: f32,
                _carry_dt: f32,
                _max_slope_angle: f32,
                _step_height: f32,
                _snap_to_ground: f32,
            ) -> MoveResult {
                let target = position + delta;
                if target.y <= 1.2 {
                    MoveResult {
                        position: Vec3::new(target.x, 1.2, target.z),
                        grounded: true,
                        ground_normal: Vec3::Y,
                        ..MoveResult::default()
                    }
                } else {
                    MoveResult {
                        position: target,
                        ..MoveResult::default()
                    }
                }
            }
            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        let mut world = World::new();
        let entity = world
            .spawn((
                NetCharacter::default(),
                Transform::from_translation(Vec3::new(0.0, 1.2, 0.0)),
            ))
            .id();
        world
            .get_mut::<CharacterMovement>(entity)
            .expect("movement")
            .rotation_mode = RotationMode::UseControllerYaw;
        let input = CharacterNetInput {
            yaw: 0.75,
            move_dir: Vec2::Y,
            ..CharacterNetInput::default()
        }
        .quantized();
        step_one(
            &mut world,
            &FlatGround,
            entity,
            Some(input.character_input()),
            NetTick(0),
            1.0 / 60.0,
        );

        let yaw = input.yaw;
        let rotation = world.get::<Transform>(entity).expect("transform").rotation;
        let expected = det_yaw_quat(yaw);
        assert_eq!(
            rotation.to_array().map(f32::to_bits),
            expected.to_array().map(f32::to_bits),
            "the facing came from the platform libm, not the deterministic kernel"
        );
        let (command, _) = input.character_input();
        let forward = camera_relative_move(yaw, Vec2::Y);
        assert_eq!(
            command.move_dir.to_array().map(f32::to_bits),
            forward.to_array().map(f32::to_bits),
            "the move vector came from the platform libm, not the deterministic kernel"
        );
        let base = world.get::<NetCharacterBase>(entity).expect("base");
        assert_eq!(
            base.stepped_yaw.to_bits(),
            det_yaw_of(expected).to_bits(),
            "the recorded yaw is not the deterministic extraction of the facing"
        );
    }

    /// A quantized copy of the rotation goes back to the exact yaw; a real turn stays.
    #[test]
    fn the_exact_yaw_undoes_only_quantization() {
        let mut world = World::new();
        let e = world
            .spawn((
                Transform::from_rotation(Quat::from_rotation_y(1.003)),
                NetCharacterBase {
                    stepped_yaw: 1.0,
                    ..NetCharacterBase::default()
                },
            ))
            .id();
        restore_exact_yaw(&mut world, e);
        assert_eq!(world.get::<Transform>(e).expect("t").rotation, det_yaw_quat(1.0));
        let turned = Quat::from_rotation_y(2.0);
        world.get_mut::<Transform>(e).expect("t").rotation = turned;
        restore_exact_yaw(&mut world, e);
        assert_eq!(world.get::<Transform>(e).expect("t").rotation, turned);
    }

    /// A re-run pushes nothing, and a networked character steps up dynamic bodies like the
    /// kinematic proxies they are on a client; other characters keep their settings.
    #[test]
    fn predicted_settings_follow_the_rules() {
        let mut world = World::new();
        let networked = world.spawn(NetCharacter::default()).id();
        let plain = world.spawn_empty().id();
        let body = regia_physics::CharacterBody::default();
        let first = predicted_body_settings(&world, networked, body.clone());
        assert!(first.autostep_include_dynamic);
        assert_eq!(first.mass, body.mass);
        assert_eq!(predicted_body_settings(&world, plain, body.clone()), body);
        world.insert_resource(NetResimulating(true));
        assert_eq!(predicted_body_settings(&world, plain, body.clone()).mass, 0.0);
    }

    /// The mode follows a written state and leaves a game's change alone.
    #[test]
    fn the_settings_mode_follows_a_written_state() {
        let mut world = World::new();
        let e = world.spawn(NetCharacter::default()).id();
        world.get_mut::<CharacterState>(e).expect("state").mode = MovementMode::Falling;
        adopt_written_mode(&mut world, e);
        assert_eq!(
            world.get::<CharacterMovement>(e).expect("movement").mode,
            MovementMode::Falling
        );
        remember_mode(&mut world, e);
        world.get_mut::<CharacterMovement>(e).expect("movement").mode = MovementMode::Flying;
        adopt_written_mode(&mut world, e);
        assert_eq!(
            world.get::<CharacterMovement>(e).expect("movement").mode,
            MovementMode::Flying
        );
    }

    fn one_bone() -> Arc<regia_animation::Skeleton> {
        use regia_animation::skeleton::LocalBind;
        Arc::new(
            regia_animation::Skeleton::from_local_bind(
                vec!["root".into()],
                vec![None],
                vec![LocalBind::default()],
            )
            .expect("skeleton"),
        )
    }

    fn speed_graph() -> (regia_animation::AnimGraph, regia_animation::ClipLibrary) {
        use regia_animation::graph::ClipNode;
        use regia_animation::params::{ParamDecl, ParamValue};
        use regia_animation::{AnimGraph, AnimNode, AnimationClip, ClipLibrary};
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("idle", 1.0));
        let graph = AnimGraph {
            parameters: vec![
                ParamDecl("Speed".into(), ParamValue::Float(0.25)),
                ParamDecl("Aim".into(), ParamValue::Vec2(Vec2::new(0.5, -0.5))),
                ParamDecl("Armed".into(), ParamValue::Bool(true)),
            ],
            root: AnimNode::Clip(ClipNode {
                clip: "idle".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        (graph, clips)
    }

    /// A server app (`NetMode::Offline` captures) with one replicated rig whose clip plays.
    fn capture_scene() -> (App, Entity) {
        use std::time::Duration;

        use regia_animation::AnimationPlugin;
        use regia_animation::components::spawn_skeleton;

        let (graph, clips) = speed_graph();
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, AnimationPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                1.0 / 60.0,
            )));
        app.insert_resource(NetMode::Offline);
        let binding = spawn_skeleton(app.world_mut(), one_bone());
        let root = app
            .world_mut()
            .spawn((
                Transform::default(),
                binding,
                Animator::graph(graph, clips),
                NetAnimState::default(),
            ))
            .id();
        app.update();
        app.update();
        (app, root)
    }

    /// Audit framework4 finding 3 (server): after the first capture a tick does not look the
    /// parameter layout up again, and a capture whose clip clock moved rewrites the component's
    /// word and parameter buffers in place without a heap request.
    #[test]
    fn capture_reuses_the_layout_and_the_state_buffers() {
        let (mut app, root) = capture_scene();
        net_anim_capture(app.world_mut());
        let first = app.world().get::<NetAnimState>(root).expect("state").clone();
        assert!(!first.words.is_empty(), "the evaluated clip was captured");
        assert_eq!(first.params.len(), 4, "Speed, Aim.x, Aim.y, Armed");
        assert_eq!(first.blend_mask, 0b0111, "the float and vector lanes blend");
        // A second tick first: the first one stored the cached layout on the root, which moved
        // it to a new archetype the capture's query state picks up once (not a steady cost).
        app.update();
        net_anim_capture(app.world_mut());
        app.update();
        let builds = layout_builds();
        let words_at = app
            .world()
            .get::<NetAnimState>(root)
            .expect("state")
            .words
            .as_ptr() as usize;
        let allocated = crate::test_alloc::count(|| net_anim_capture(app.world_mut()));
        let again = app.world().get::<NetAnimState>(root).expect("state");
        assert_ne!(
            again.words, first.words,
            "the clip clock moved, so the words changed"
        );
        assert_eq!(again.params, first.params);
        assert_eq!(
            layout_builds() - builds,
            0,
            "a later tick looked the layout up again"
        );
        assert_eq!(
            again.words.as_ptr() as usize,
            words_at,
            "the capture replaced the word buffer instead of rewriting it"
        );
        assert_eq!(allocated, 0, "a steady capture made {allocated} heap requests");
    }

    /// The cached layout follows the graph: an animator given a new graph is captured with the
    /// new graph's lanes on the next tick.
    #[test]
    fn capture_rereads_the_layout_when_the_graph_changes() {
        use regia_animation::params::{ParamDecl, ParamValue};

        let (mut app, root) = capture_scene();
        net_anim_capture(app.world_mut());
        assert_eq!(
            app.world().get::<NetAnimState>(root).expect("state").params.len(),
            4
        );
        let (mut graph, clips) = speed_graph();
        graph.parameters = vec![ParamDecl("Lean".into(), ParamValue::Float(0.75))];
        let mut animator = Animator::graph(graph, clips);
        animator.parameters.set_float("Lean", 0.75);
        *app.world_mut().get_mut::<Animator>(root).expect("animator") = animator;
        app.update();
        net_anim_capture(app.world_mut());
        let state = app.world().get::<NetAnimState>(root).expect("state");
        assert_eq!(state.params, vec![0.75f32.to_bits()], "the new graph's one lane");
        assert_eq!(state.blend_mask, 1);
    }

    /// A client app with one interpolated proxy whose shown tick already matches its state.
    fn drive_scene() -> (App, Entity) {
        use regia_animation::components::spawn_skeleton;
        use regia_net::interp::NetInterpolation;
        use regia_net::settings::InterpolationSettings;

        let (graph, clips) = speed_graph();
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app.insert_resource(NetMode::Client);
        app.insert_resource(NetInterpolation::new(InterpolationSettings::default(), 60.0, 1.0));
        let binding = spawn_skeleton(app.world_mut(), one_bone());
        let root = app
            .world_mut()
            .spawn((
                Transform::default(),
                binding,
                Animator::graph(graph, clips),
                NetAnimState {
                    tick: 4,
                    words: vec![1, 2, 3],
                    params: vec![1.5f32.to_bits(), 2.0f32.to_bits(), 3.0f32.to_bits(), 0],
                    blend_mask: 0b0111,
                    past: 0.0,
                },
                NetAnimShown { tick: 4 },
            ))
            .id();
        (app, root)
    }

    /// Audit framework4 finding 3 (client): a proxy frame whose tick did not change neither
    /// looks the layout up again nor copies the replicated state, and still writes the blended
    /// parameters.
    #[test]
    fn drive_reuses_the_layout_between_ticks() {
        let (mut app, root) = drive_scene();
        net_anim_drive(app.world_mut());
        let animator = app.world().get::<Animator>(root).expect("animator");
        assert_eq!(
            animator.parameters.float("Speed"),
            1.5,
            "the blended lane was applied"
        );
        assert_eq!(animator.parameters.vec2("Aim"), Vec2::new(2.0, 3.0));
        let builds = layout_builds();
        let allocated = crate::test_alloc::count(|| net_anim_drive(app.world_mut()));
        assert_eq!(
            layout_builds() - builds,
            0,
            "a frame with the same tick looked the layout up again"
        );
        assert_eq!(
            allocated, 0,
            "a steady proxy frame made {allocated} heap requests"
        );
        // A changed replicated lane still reaches the animator on the next frame.
        app.world_mut()
            .get_mut::<NetAnimState>(root)
            .expect("state")
            .params[0] = 0.5f32.to_bits();
        net_anim_drive(app.world_mut());
        assert_eq!(
            app.world()
                .get::<Animator>(root)
                .expect("animator")
                .parameters
                .float("Speed"),
            0.5
        );
    }

    /// Timing of `net_anim_capture` (64 server rigs) and `net_anim_drive` (64 proxies on steady
    /// frames). Run by hand with `--ignored --nocapture`; prints, asserts nothing about time.
    #[test]
    #[ignore = "manual bench: cargo test -p regia_framework --lib net_character::tests::bench_ -- --ignored --nocapture"]
    fn bench_net_anim_capture_and_drive() {
        use std::time::{Duration, Instant};

        use regia_animation::AnimationPlugin;
        use regia_animation::components::spawn_skeleton;
        use regia_net::interp::NetInterpolation;
        use regia_net::settings::InterpolationSettings;

        const RIGS: usize = 64;
        let mut server = App::new();
        server
            .add_plugins((MinimalPlugins, AnimationPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                1.0 / 60.0,
            )));
        server.insert_resource(NetMode::Offline);
        for _ in 0..RIGS {
            let (graph, clips) = speed_graph();
            let binding = spawn_skeleton(server.world_mut(), one_bone());
            server.world_mut().spawn((
                Transform::default(),
                binding,
                Animator::graph(graph, clips),
                NetAnimState::default(),
            ));
        }
        server.update();
        server.update();
        let ticks = 2000u32;
        let mut capture = Duration::ZERO;
        let mut capture_allocs = 0usize;
        for _ in 0..ticks {
            server.update();
            let start = Instant::now();
            capture_allocs += crate::test_alloc::count(|| net_anim_capture(server.world_mut()));
            capture += start.elapsed();
        }

        let mut client = App::new();
        client.add_plugins(MinimalPlugins);
        client.insert_resource(NetMode::Client);
        client.insert_resource(NetInterpolation::new(InterpolationSettings::default(), 60.0, 1.0));
        for _ in 0..RIGS {
            let (graph, clips) = speed_graph();
            let binding = spawn_skeleton(client.world_mut(), one_bone());
            client.world_mut().spawn((
                Transform::default(),
                binding,
                Animator::graph(graph, clips),
                NetAnimState {
                    tick: 4,
                    words: vec![1, 2, 3],
                    params: vec![1.5f32.to_bits(), 2.0f32.to_bits(), 3.0f32.to_bits(), 0],
                    blend_mask: 0b0111,
                    past: 0.0,
                },
                NetAnimShown { tick: 4 },
            ));
        }
        net_anim_drive(client.world_mut());
        let frames = 4000u32;
        let mut drive_allocs = 0usize;
        let start = Instant::now();
        for _ in 0..frames {
            drive_allocs += crate::test_alloc::count(|| net_anim_drive(client.world_mut()));
        }
        let drive = start.elapsed();
        eprintln!(
            "bench net_anim {RIGS} rigs: capture {:.3} us/tick ({:.1} allocs/tick), drive {:.3} us/frame ({:.1} allocs/frame)",
            capture.as_secs_f64() * 1e6 / f64::from(ticks),
            capture_allocs as f64 / f64::from(ticks),
            drive.as_secs_f64() * 1e6 / f64::from(frames),
            drive_allocs as f64 / f64::from(frames),
        );
    }
}
