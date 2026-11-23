// SPDX-License-Identifier: MPL-2.0
//! Character movement (`docs/specs/14-scene-and-framework.md` § B4).
//!
//! A [`Character`] is a capsule-shaped movable entity: it carries a [`CharacterMovement`] settings
//! component that defines speeds, accelerations, jump mechanics, and the movement-mode state
//! machine. The physics step (sweep-and-resolve through rapier, `specs/15` § 4) is plugged in
//! through the [`CollisionQuery`] trait: the real backend is [`RapierCharacterQuery`] (installed
//! with [`use_rapier_character_controller`]), which wraps `regia_physics`' kinematic character
//! controller — autostep, slope limits, snap-to-ground, moving/rotating platform riding and
//! pushing dynamic bodies. The framework also ships stub backends ([`FlatPlaneQuery`],
//! [`HeightFieldQuery`]) so movement can be tested without physics.
//!
//! # Architecture
//!
//! 1. **Input**: a controller (player or AI) writes [`CharacterInput`] to the character entity
//!    each frame.
//! 2. **State machine**: the movement mode ([`MovementMode`]) governs which forces apply:
//!    `Walking` (on ground; `braking_deceleration` stops idle input; `ground_friction` is stored
//!    and not applied), `Falling` (in air, air control), `Flying` (no gravity, still colliding),
//!    `Swimming` (with [`crate::traversal`], which also runs ladders and ledge moves), or `Custom`
//!    (script-driven).
//! 3. **Integration**: a system in `FixedUpdate` reads the input and mode, computes a desired
//!    displacement, calls [`CollisionQuery::move_shape`] to resolve collisions, and writes the
//!    resulting [`Transform`] and [`CharacterState`] (for animation).
//! 4. **Root motion**: when an animation provides a root-motion delta, the system adds it after
//!    the physics step (Tier 1: stored as a resource; Tier 2: per-bone override).
//!
//! # Jump mechanics
//!
//! Jumps use the three standard platformer windows (§ B4 settings): **coyote time** (a jump is
//! still accepted shortly after walking off a ledge), a **jump buffer** (a jump pressed shortly
//! before landing fires on touchdown) and `max_jumps` (double jumps). The bookkeeping lives on
//! [`CharacterState`] so it is visible to animation and survives reflection round trips.

use regia_app::FixedTime;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det, det_atan2, det_sin_cos, det_yaw_of};
use regia_scene::Transform;

/// Gravitational acceleration (m/s², world −Y). A character's `gravity_scale` multiplies this.
pub const GRAVITY: f32 = -9.81;

/// Distance the heightfield stub samples the ground on each side of a point to measure slope.
const TERRAIN_SAMPLE: f32 = 0.05;

// ---------------------------------------------------------------------------
// Movement modes
// ---------------------------------------------------------------------------

/// The movement mode the character is currently in. Determines which acceleration/friction values
/// apply and whether gravity acts.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Default)]
pub enum MovementMode {
    /// On the ground: friction, walking speed, can jump, slope limits.
    #[default]
    Walking,
    /// In the air: gravity, air control (reduced horizontal influence), coyote time.
    Falling,
    /// Flight with no gravity: velocity follows the move direction at `max_walk_speed`, then the
    /// capsule still resolves collisions through `move_shape`. Toggled by cheats or scripts.
    Flying,
    /// In the water. A character with `crate::traversal::TraversalSettings` swims (float line, swim
    /// speed, dive; `docs/specs/traversal.md`); on any other it keeps its gravity and walk speed.
    Swimming,
    /// A custom mode identified by `id`; script-driven movement (e.g. wall-running, grappling).
    Custom(u32),
}

impl MovementMode {
    /// On a ladder (`docs/specs/traversal.md`). A reserved [`Self::Custom`] id: custom ids at or above
    /// `0xFFFF_0000` belong to the engine.
    pub const CLIMBING: Self = Self::Custom(0xFFFF_0001);
    /// On a scripted traversal curve: a mantle, a vault, a step on or off a ladder, an authored jump
    /// (`docs/specs/traversal.md`). A reserved [`Self::Custom`] id.
    pub const TRAVERSING: Self = Self::Custom(0xFFFF_0002);

    /// Whether gravity applies in this mode.
    pub fn has_gravity(self) -> bool {
        matches!(self, Self::Walking | Self::Falling | Self::Swimming)
    }
}

/// How the character's facing direction is controlled.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Default)]
#[reflect(Default)]
pub enum RotationMode {
    /// Face the direction of movement, rotating at `turn_rate` rad/s.
    #[default]
    OrientToMovement,
    /// Face the controller's desired yaw (from look input).
    UseControllerYaw,
}

// ---------------------------------------------------------------------------
// Character components
// ---------------------------------------------------------------------------

/// The settings for character movement (§ B4).
///
/// All distances are in metres, speeds in m/s, accelerations in m/s², angular rates in rad/s.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component)]
pub struct CharacterMovement {
    /// Capsule radius.
    pub radius: f32,
    /// Length of the cylindrical section from the centre to either end of that section. Tip-to-tip
    /// height is `2 * (half_height + radius)`. A resting capsule sits with its centre at
    /// `ground + radius + half_height`.
    pub half_height: f32,
    /// Maximum horizontal speed while walking.
    pub max_walk_speed: f32,
    /// Maximum horizontal speed while sprinting (multiplier on walk speed when sprint input is
    /// active).
    pub sprint_speed_multiplier: f32,
    /// Ground acceleration (m/s²).
    pub acceleration: f32,
    /// Deceleration when no input is given (m/s²).
    pub braking_deceleration: f32,
    /// Stored ground-friction coefficient (sanitized to at least 0; default 0.9). The step does
    /// not read it. A grounded character with no input stops under `braking_deceleration`.
    pub ground_friction: f32,
    /// Air control fraction: how much of the input acceleration applies while falling
    /// (0 = none, 1 = full).
    pub air_control: f32,
    /// Initial vertical velocity on jump (m/s).
    pub jump_velocity: f32,
    /// Maximum number of consecutive jumps (1 = single jump, 2 = double jump, 0 = none).
    pub max_jumps: u8,
    /// How long after walking off a ledge the character can still jump (seconds).
    pub coyote_time: f32,
    /// How early before landing a jump input is buffered (seconds).
    pub jump_buffer_time: f32,
    /// Gravity scale multiplier (1.0 = normal, 0.0 = no gravity in Falling mode).
    pub gravity_scale: f32,
    /// Maximum walkable slope angle in radians.
    pub max_slope_angle: f32,
    /// Maximum step height the character can climb onto without jumping (metres).
    pub step_height: f32,
    /// Snap-to-ground distance: when resting within this distance above the ground after a step,
    /// snap down onto it (walking downhill stays grounded).
    pub snap_to_ground_distance: f32,
    /// Crouch half-height; when crouching, the capsule shrinks to this. Must be ≤ `half_height`.
    pub crouch_half_height: f32,
    /// Horizontal speed multiplier while crouching.
    pub crouch_speed_multiplier: f32,
    /// Mass in kg for pushing dynamic bodies. Used by [`RapierCharacterQuery`] when the entity has
    /// no `regia_physics::CharacterBody`; a `CharacterBody` carries its own `mass` and wins.
    pub mass: f32,
    /// How the character's forward direction is controlled.
    pub rotation_mode: RotationMode,
    /// The rate at which the character turns towards the movement direction (rad/s).
    pub turn_rate: f32,
    /// Current movement mode (state).
    pub mode: MovementMode,
}

impl CharacterMovement {
    /// The settings one step uses (audit-v035-numerics H4/H5): a non-finite field takes its
    /// default, and the sizes, speeds, accelerations, times and the turn rate are floored at 0.
    /// `jump_velocity` and `gravity_scale` keep their sign. Finite in-range settings come back
    /// unchanged, so valid characters step bit-exactly as before.
    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        let any = |value: f32, fallback: f32| if value.is_finite() { value } else { fallback };
        let at_least_zero = |value: f32, fallback: f32| any(value, fallback).max(0.0);
        Self {
            radius: at_least_zero(self.radius, d.radius),
            half_height: at_least_zero(self.half_height, d.half_height),
            max_walk_speed: at_least_zero(self.max_walk_speed, d.max_walk_speed),
            sprint_speed_multiplier: at_least_zero(self.sprint_speed_multiplier, d.sprint_speed_multiplier),
            acceleration: at_least_zero(self.acceleration, d.acceleration),
            braking_deceleration: at_least_zero(self.braking_deceleration, d.braking_deceleration),
            ground_friction: at_least_zero(self.ground_friction, d.ground_friction),
            air_control: at_least_zero(self.air_control, d.air_control),
            jump_velocity: any(self.jump_velocity, d.jump_velocity),
            max_jumps: self.max_jumps,
            coyote_time: at_least_zero(self.coyote_time, d.coyote_time),
            jump_buffer_time: at_least_zero(self.jump_buffer_time, d.jump_buffer_time),
            gravity_scale: any(self.gravity_scale, d.gravity_scale),
            max_slope_angle: at_least_zero(self.max_slope_angle, d.max_slope_angle),
            step_height: at_least_zero(self.step_height, d.step_height),
            snap_to_ground_distance: at_least_zero(self.snap_to_ground_distance, d.snap_to_ground_distance),
            crouch_half_height: at_least_zero(self.crouch_half_height, d.crouch_half_height),
            crouch_speed_multiplier: at_least_zero(self.crouch_speed_multiplier, d.crouch_speed_multiplier),
            mass: at_least_zero(self.mass, d.mass),
            rotation_mode: self.rotation_mode,
            turn_rate: at_least_zero(self.turn_rate, d.turn_rate),
            mode: self.mode,
        }
    }
}

impl Default for CharacterMovement {
    fn default() -> Self {
        Self {
            radius: 0.3,
            half_height: 0.9,
            max_walk_speed: 6.0,
            sprint_speed_multiplier: 1.6,
            acceleration: 20.0,
            braking_deceleration: 30.0,
            ground_friction: 0.9,
            air_control: 0.3,
            jump_velocity: 8.0,
            max_jumps: 1,
            coyote_time: 0.15,
            jump_buffer_time: 0.15,
            gravity_scale: 1.0,
            max_slope_angle: 0.785, // ~45 degrees
            step_height: 0.3,
            snap_to_ground_distance: 0.5,
            crouch_half_height: 0.5,
            crouch_speed_multiplier: 0.5,
            mass: 70.0,
            rotation_mode: RotationMode::OrientToMovement,
            turn_rate: 720.0_f32.to_radians(),
            mode: MovementMode::Walking,
        }
    }
}

/// The root marker for a character entity.
///
/// Attach to any entity that is a character (player or AI). The movement system processes every
/// entity with `Character` + `CharacterMovement` + `Transform`.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct Character;

/// Desired input from the controller, written each frame.
///
/// The movement system reads this in `FixedUpdate`, accelerates towards `move_dir` (world space),
/// and clears the jump flag after consuming it.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component)]
pub struct CharacterInput {
    /// Desired movement direction in world space (magnitude ≤ 1).
    pub move_dir: Vec3,
    /// Whether the jump button is pressed this tick (consumed after the jump attempt).
    pub jump: bool,
    /// Whether the crouch button is held.
    pub crouch: bool,
    /// Whether the sprint button is held.
    pub sprint: bool,
}

/// Output state computed by the movement system for animation and gameplay.
///
/// A script or animation system reads this to drive blend spaces and IK. The `*_left`/`used`
/// fields are the movement system's jump bookkeeping (§ module docs) and reset on landing.
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component)]
pub struct CharacterState {
    /// Current world-space velocity.
    pub velocity: Vec3,
    /// Whether the character is on the ground this frame.
    pub grounded: bool,
    /// Current movement mode.
    pub mode: MovementMode,
    /// Time spent in the air since the last ground contact (seconds).
    pub time_in_air: f32,
    /// True on the frame the character lands (transition from air to ground).
    pub landed_this_frame: bool,
    /// How many of `max_jumps` have been used since the last landing.
    pub jumps_used: u8,
    /// Coyote time left: how much longer a jump is still accepted after leaving the ground
    /// (seconds).
    pub coyote_time_left: f32,
    /// Jump-buffer time left: a jump pressed this many seconds before landing fires on touchdown
    /// (seconds).
    pub jump_buffer_time_left: f32,
    /// Whether the capsule is crouched (`crouch_half_height`, spec 93 § 2.2). Stays true while
    /// a ceiling leaves no room to stand after crouch is released.
    pub crouched: bool,
    /// The ground's velocity at the character this step (m/s; zero in the air). Carried into
    /// the jump when the character leaves the ground (spec 93 § 2.2).
    pub ground_velocity: Vec3,
}

// ---------------------------------------------------------------------------
// Collision query trait (pluggable)
// ---------------------------------------------------------------------------

/// Result of a collision query: where the character ended up and what it touched.
#[derive(Debug, Clone, Default)]
pub struct MoveResult {
    /// The resulting position.
    pub position: Vec3,
    /// Whether the character is on the ground after the move.
    pub grounded: bool,
    /// The ground surface normal (world-space, up), if grounded.
    pub ground_normal: Vec3,
    /// The angle of the ground surface in radians (0 = flat, π/2 = vertical wall).
    pub ground_angle: f32,
    /// Entities that were hit during the movement.
    pub hits: Vec<Entity>,
    /// Whether the capsule's top touched a ceiling during the move (spec 93 § 2.2).
    pub ceiling_hit: bool,
    /// The velocity of the ground the character stood on at the start of the move (m/s): the
    /// backend already moved the character with it (unless the character is
    /// [`CharacterAnchored`]), so the movement step subtracts it when it measures the motion
    /// achieved and carries it into a jump (spec 93 § 2.2).
    pub ground_velocity: Vec3,
    /// The entity the character stands on, from the collision backend (spec 73 § 9); `None` in
    /// the air and for the stub backends, whose ground is not an entity.
    pub ground_entity: Option<Entity>,
}

/// The physics-query surface the movement system needs (§ B4, specs/15 § 4).
///
/// The real implementation is [`RapierCharacterQuery`] (wrapping `regia_physics`' character
/// controller); the framework also ships stub backends ([`FlatPlaneQuery`],
/// [`HeightFieldQuery`]) so movement can be tested without physics. Backends receive
/// `&mut World` (rapier applies impulses to the bodies a character pushes) and the moving
/// `entity` (per-character settings and ground-state write-back via
/// `regia_physics::CharacterBody`). A backend with moving ground carries characters with it,
/// except those marked [`CharacterAnchored`].
pub trait CollisionQuery: Send + Sync + 'static {
    /// Moves a capsule of `radius` and `half_height` from `position` by `delta`, resolving
    /// collisions with the world. Returns the resolved position and ground status.
    ///
    /// * `dt` — the duration of the whole fixed step this move belongs to. The backend's
    ///   dynamic-body push impulses are scaled to this step (`remaining_translation / dt`).
    /// * `carry_dt` — this move's own share of the step for the additive moving-platform
    ///   carry (`ground_velocity * carry_dt`). A normal move passes `dt`; a caller that
    ///   splits one step's motion into sub-moves (root-motion banked across a hitch, swept
    ///   in fixed-step quanta) passes `dt / quanta` per sub-move, so the carry — unlike the
    ///   impulses, which belong to the whole step — totals exactly one step no matter how
    ///   many quanta sweep it (audit fixrev-06-k H1).
    /// * `max_slope_angle` — ground steeper than this is a wall, not a floor.
    /// * `step_height` — terrain rises up to this are stepped up (while grounded).
    /// * `snap_to_ground` — when resting within this distance above the ground, snap down onto
    ///   it (downhill walking stays grounded).
    #[allow(clippy::too_many_arguments)]
    fn move_shape(
        &self,
        world: &mut World,
        entity: Entity,
        position: Vec3,
        radius: f32,
        half_height: f32,
        delta: Vec3,
        dt: f32,
        carry_dt: f32,
        max_slope_angle: f32,
        step_height: f32,
        snap_to_ground: f32,
    ) -> MoveResult;

    /// Whether a capsule of `radius` and `half_height` centred at `position` can rise by `rise`
    /// metres without hitting anything (a crouched character standing up, spec 93 § 2.2). The
    /// default says yes: backends without ceilings never refuse.
    fn has_headroom(
        &self,
        world: &mut World,
        entity: Entity,
        position: Vec3,
        radius: f32,
        half_height: f32,
        rise: f32,
    ) -> bool {
        let _ = (world, entity, position, radius, half_height, rise);
        true
    }

    /// The backend's concrete type, for downcasting (tests and debug tooling).
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A flat-ground stub: an infinite plane at Y = 0. The default backend until a physics world
/// is installed.
///
/// Movement is stopped at the capsule's resting height over the plane; horizontal motion is
/// unobstructed. See [`HeightFieldQuery`] for a stub with steps and slopes.
pub struct FlatPlaneQuery;

impl CollisionQuery for FlatPlaneQuery {
    fn move_shape(
        &self,
        world: &mut World,
        entity: Entity,
        position: Vec3,
        radius: f32,
        half_height: f32,
        delta: Vec3,
        dt: f32,
        carry_dt: f32,
        max_slope_angle: f32,
        step_height: f32,
        snap_to_ground: f32,
    ) -> MoveResult {
        let _ = (entity, dt, carry_dt);
        heightfield_move(
            &|_, _| 0.0,
            world,
            position,
            radius,
            half_height,
            delta,
            max_slope_angle,
            step_height,
            snap_to_ground,
        )
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A stub collision backend over a height function `h(x, z)` (ground height in metres).
///
/// Models the world as a heightfield:
/// - terrain rises up to `step_height` are stepped up (while resting on the ground);
/// - ground steeper than `max_slope_angle`, or rises above `step_height`, blocks the horizontal
///   move (vertical motion, e.g. falling, still applies);
/// - a character at or above the target's floor (falling over it, flying) moves freely;
/// - resting characters within `snap_to_ground` above the floor snap down (downhill stays
///   grounded).
///
/// The real rapier backend ([`RapierCharacterQuery`]) replaces this; the stub keeps the
/// movement state machine testable end to end without a physics world.
pub struct HeightFieldQuery {
    height: Box<dyn Fn(f32, f32) -> f32 + Send + Sync>,
}

impl HeightFieldQuery {
    /// Wraps a ground-height function, e.g. a test step, ramp or island shape.
    pub fn new(height: impl Fn(f32, f32) -> f32 + Send + Sync + 'static) -> Self {
        Self {
            height: Box::new(height),
        }
    }
}

impl CollisionQuery for HeightFieldQuery {
    fn move_shape(
        &self,
        world: &mut World,
        entity: Entity,
        position: Vec3,
        radius: f32,
        half_height: f32,
        delta: Vec3,
        dt: f32,
        carry_dt: f32,
        max_slope_angle: f32,
        step_height: f32,
        snap_to_ground: f32,
    ) -> MoveResult {
        let _ = (entity, dt, carry_dt);
        heightfield_move(
            self.height.as_ref(),
            world,
            position,
            radius,
            half_height,
            delta,
            max_slope_angle,
            step_height,
            snap_to_ground,
        )
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Shared stub implementation: the flat plane is the heightfield `h(x, z) = 0`.
#[allow(clippy::too_many_arguments)]
fn heightfield_move(
    height: &dyn Fn(f32, f32) -> f32,
    _world: &World,
    position: Vec3,
    radius: f32,
    half_height: f32,
    delta: Vec3,
    max_slope_angle: f32,
    step_height: f32,
    snap_to_ground: f32,
) -> MoveResult {
    let eps = 1e-4;
    let rest_center = |ground: f32| ground + radius + half_height;

    let ground_here = height(position.x, position.z);
    let target_x = position.x + delta.x;
    let target_z = position.z + delta.z;
    let ground_target = height(target_x, target_z);
    let floor_here = rest_center(ground_here);
    let floor_target = rest_center(ground_target);

    let resting = position.y <= floor_here + eps;
    let above_target = position.y >= floor_target - eps;
    let normal_target = terrain_normal(height, target_x, target_z);
    let angle_target = angle_from_up(normal_target);

    // The horizontal move is allowed when the ground ahead is walkable and steppable (from a
    // resting position), or already cleared (the character is at/above its floor).
    let walkable = angle_target <= max_slope_angle + eps;
    let steppable = ground_target - ground_here <= step_height + eps;
    let horizontal_ok = above_target || (resting && walkable && steppable);

    let (x, z, ground) = if horizontal_ok {
        (target_x, target_z, ground_target)
    } else {
        (position.x, position.z, ground_here)
    };
    let floor = rest_center(ground);
    let normal = terrain_normal(height, x, z);
    let angle = angle_from_up(normal);

    let target_y = position.y + delta.y;
    // Clamp up when coming from above (landing); snap down onto the floor when resting and not
    // moving up (walking downhill); otherwise free vertical motion.
    let y = if target_y < floor || (resting && delta.y <= 0.0 && target_y - floor <= snap_to_ground + eps) {
        floor
    } else {
        target_y
    };
    let grounded = y <= floor + eps;

    MoveResult {
        position: Vec3::new(x, y, z),
        grounded,
        ground_normal: normal,
        ground_angle: if grounded {
            angle
        } else {
            std::f32::consts::FRAC_PI_2
        },
        hits: vec![],
        ceiling_hit: false,
        ground_velocity: Vec3::ZERO,
        ground_entity: None,
    }
}

/// The terrain surface normal at `(x, z)` from a central finite-difference gradient.
fn terrain_normal(height: &dyn Fn(f32, f32) -> f32, x: f32, z: f32) -> Vec3 {
    let e = TERRAIN_SAMPLE;
    let dhdx = (height(x + e, z) - height(x - e, z)) / (2.0 * e);
    let dhdz = (height(x, z + e) - height(x, z - e)) / (2.0 * e);
    Vec3::new(-dhdx, 1.0, -dhdz).normalize()
}

/// The angle between a surface normal and world up (0 = flat ground, π/2 = a wall), from the
/// deterministic kernel: `ground_angle` feeds the lockstep path, so it must not follow libm
/// (fixrev-06-k H2).
fn angle_from_up(normal: Vec3) -> f32 {
    det::acos(normal.y.clamp(-1.0, 1.0))
}

/// Current collision query implementation (set by [`CharacterPlugin`]).
///
/// Games or the physics plugin replace this resource with their own backend.
#[derive(Resource)]
pub struct CharacterCollisionQuery {
    /// The boxed query implementation.
    pub query: Box<dyn CollisionQuery>,
}

impl Default for CharacterCollisionQuery {
    fn default() -> Self {
        Self {
            query: Box::new(FlatPlaneQuery),
        }
    }
}

// ---------------------------------------------------------------------------
// Rapier backend (specs/15 § 4)
// ---------------------------------------------------------------------------

/// The rapier-backed [`CollisionQuery`] (specs/15 § 4): sweeps the character's capsule with
/// `regia_physics`' kinematic character controller — autostep, slope limits, snap-to-ground,
/// slide, skin width — and adds ground reporting, moving/rotating platform riding (the ground
/// body's velocity at the character's position) and pushing of dynamic bodies (impulses
/// proportional to the character's mass; none while `regia_net` resimulates a tick, spec 73 § 9).
///
/// Install with [`use_rapier_character_controller`] (requires `PhysicsPlugin` so the
/// `PhysicsWorld` exists). Per-character physics settings and the ground state (ground entity,
/// normal, platform velocity, ceiling) live on `regia_physics::CharacterBody`; attach one to
/// override them.
pub struct RapierCharacterQuery;

/// The physics settings for `entity`'s sweep: its `CharacterBody` when attached, else the body
/// default with the push mass taken from [`CharacterMovement::mass`].
fn sweep_body_settings(world: &World, entity: Entity) -> regia_physics::CharacterBody {
    if let Some(body) = world.get::<regia_physics::CharacterBody>(entity) {
        return body.clone();
    }
    let mut body = regia_physics::CharacterBody::default();
    if let Some(movement) = world.get::<CharacterMovement>(entity)
        && movement.mass.is_finite()
        && movement.mass > 0.0
    {
        body.mass = movement.mass;
    }
    body
}

impl CollisionQuery for RapierCharacterQuery {
    #[allow(clippy::too_many_arguments)]
    fn move_shape(
        &self,
        world: &mut World,
        entity: Entity,
        position: Vec3,
        radius: f32,
        half_height: f32,
        delta: Vec3,
        dt: f32,
        carry_dt: f32,
        max_slope_angle: f32,
        step_height: f32,
        snap_to_ground: f32,
    ) -> MoveResult {
        let settings = sweep_body_settings(world, entity);
        #[cfg(feature = "net")]
        let settings = crate::net_character::predicted_body_settings(world, entity, settings);
        // `dt` (impulse scaling) is the whole fixed step; `carry_dt` is this move's share
        // (dt / quanta for a root-motion quantum) so the additive platform carry totals one
        // step instead of one per quantum (audit fixrev-06-k H1).
        // Never sweep against the character's own body: its collider exists so sensors can
        // see the character, not to obstruct the controller.
        let own_body = world
            .get::<regia_physics::RigidBodyHandle>(entity)
            .map(|handle| handle.to_handle());
        let anchored = is_anchored(world, entity);
        let Some(mut physics) = world.get_resource_mut::<regia_physics::PhysicsWorld>() else {
            tracing::warn!(
                target: "regia_framework",
                "RapierCharacterQuery needs PhysicsPlugin's PhysicsWorld; the character did not move"
            );
            return MoveResult {
                position,
                grounded: false,
                ground_normal: Vec3::Y,
                ground_angle: std::f32::consts::FRAC_PI_2,
                hits: Vec::new(),
                ceiling_hit: false,
                ground_velocity: Vec3::ZERO,
                ground_entity: None,
            };
        };

        let sweep = if anchored {
            regia_physics::character::move_anchored_character
        } else {
            regia_physics::character::move_character
        };
        let result = sweep(
            &mut physics,
            &settings,
            radius,
            half_height,
            (position, Quat::IDENTITY),
            delta,
            max_slope_angle,
            step_height,
            snap_to_ground,
            own_body,
            dt,
            carry_dt,
        );

        // Write the ground state back so gameplay and scripts can read it (specs/15 § 4).
        if let Some(mut body) = world.get_mut::<regia_physics::CharacterBody>(entity) {
            body.ground = regia_physics::CharacterGround {
                grounded: result.grounded,
                ground_normal: result.ground_normal,
                ground_entity: result.ground_entity,
                ground_velocity: result.ground_velocity,
                ground_angular_velocity: result.ground_yaw_rate,
                ceiling_hit: result.ceiling_hit,
                sliding: result.sliding,
            };
        }

        MoveResult {
            position: position + result.translation,
            grounded: result.grounded,
            ground_normal: result.ground_normal,
            ground_angle: if result.grounded {
                angle_from_up(result.ground_normal)
            } else {
                std::f32::consts::FRAC_PI_2
            },
            hits: result.hits,
            ceiling_hit: result.ceiling_hit,
            ground_velocity: result.ground_velocity,
            ground_entity: (result.grounded && result.ground_entity != Entity::PLACEHOLDER)
                .then_some(result.ground_entity),
        }
    }

    fn has_headroom(
        &self,
        world: &mut World,
        entity: Entity,
        position: Vec3,
        radius: f32,
        half_height: f32,
        rise: f32,
    ) -> bool {
        let own_body = world
            .get::<regia_physics::RigidBodyHandle>(entity)
            .map(|handle| handle.to_handle());
        let offset = world
            .get::<regia_physics::CharacterBody>(entity)
            .map_or(regia_physics::CharacterBody::default().offset, |b| b.offset);
        match world.get_resource::<regia_physics::PhysicsWorld>() {
            Some(physics) => regia_physics::character::capsule_has_headroom(
                physics,
                radius,
                half_height,
                position,
                rise,
                offset,
                own_body,
            ),
            None => true,
        }
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Installs the rapier-backed character controller ([`RapierCharacterQuery`]) as the
/// [`CharacterCollisionQuery`] for this app (specs/15 § 4).
///
/// The app must also add `regia_physics::PhysicsPlugin` — the backend moves against its
/// colliders. Without it, the backend warns and characters do not move.
pub fn use_rapier_character_controller(app: &mut App) {
    app.insert_resource(CharacterCollisionQuery {
        query: Box::new(RapierCharacterQuery),
    });
}

// ---------------------------------------------------------------------------
// Plugin and systems
// ---------------------------------------------------------------------------

/// Registers character components and the movement system (§ B4).
///
/// The system runs in `FixedUpdate`, after input is gathered, and reads [`CharacterInput`] to
/// drive the state machine. Replace the default [`FlatPlaneQuery`] by inserting a custom
/// [`CharacterCollisionQuery`].
pub struct CharacterPlugin;

impl Plugin for CharacterPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<Character>()
            .register_type::<CharacterMovement>()
            .register_type::<CharacterInput>()
            .register_type::<CharacterState>()
            .register_type::<MovementMode>()
            .register_type::<RotationMode>()
            .register_type::<RootMotionDriven>()
            .register_type::<RootMotionApply>()
            .register_type::<CharacterSteppedExternally>()
            .init_resource::<CharacterCollisionQuery>()
            .init_resource::<RootMotionDelta>()
            // After the physics step (specs/15 § 1/§ 4): the backend moves against the
            // freshly stepped world — platform velocities are current — and its push
            // impulses persist into the next step. A no-op without `PhysicsPlugin`.
            // Before the streaming sets: a rebase moves transforms and rapier together, and this
            // reads both.
            .add_systems(
                FixedUpdate,
                movement_step
                    .after(regia_physics::PhysicsStep)
                    .before(regia_scene::streaming::StreamSet::Sources),
            )
            // Foot placement (ADR-8388). The probe is installed only when the host has not
            // supplied one. Grounded is copied before both the graph pass (Update) and the pose
            // pass (PostUpdate). The physics pointer is published around each pass and cleared
            // after it; the PostUpdate publish waits until the debris systems have finished
            // mutating the physics world.
            .add_systems(Startup, crate::foot_probe::install_physics_ground_probe)
            .add_systems(
                Update,
                (
                    crate::foot_probe::sync_foot_grounded,
                    crate::foot_probe::foot_placement_on_respawn,
                    crate::foot_probe::publish_physics_probe,
                )
                    .before(regia_animation::systems::prepare_foot_placement),
            )
            .add_systems(
                Update,
                crate::foot_probe::clear_physics_probe
                    .after(regia_animation::systems::prepare_foot_placement),
            )
            .add_systems(
                PostUpdate,
                crate::foot_probe::sync_foot_grounded.before(regia_animation::systems::apply_animation),
            )
            .add_systems(
                PostUpdate,
                crate::foot_probe::publish_physics_probe
                    .after(regia_physics::despawn_expired_debris)
                    .after(regia_physics::enforce_body_budget)
                    .before(regia_animation::systems::apply_animation),
            )
            .add_systems(
                PostUpdate,
                crate::foot_probe::clear_physics_probe.after(regia_animation::systems::apply_animation),
            );
    }
}

// ---------------------------------------------------------------------------
// Root motion
// ---------------------------------------------------------------------------

/// Which parts of an animation's root motion move a [`Character`].
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Default)]
pub enum RootMotionApply {
    /// Ignore root motion; input drives the character.
    Off,
    /// Horizontal translation (and yaw) from the animation while walking on the ground;
    /// gravity and jumps stay physical, and air control applies while falling.
    #[default]
    Horizontal,
    /// Full translation from the animation (climbs, vaults, scripted moves): no gravity
    /// while root motion drives, in any movement mode.
    Full,
}

/// Root-motion mode for a character (`specs/17` § 4): the rig's accumulated
/// `regia_animation::RootMotionOutput` replaces the input-driven displacement and goes
/// through the same [`CollisionQuery`] sweep, so walls, steps and slopes still stop or
/// redirect it.
///
/// The rig's object space is mapped to the world by the rig entity's transform
/// (`GlobalTransform` when present, else `Transform`), so a rig scaled or turned relative
/// to the capsule moves the capsule correctly.
#[derive(Component, Reflect, Debug, Clone, PartialEq)]
#[reflect(Component, Default)]
pub struct RootMotionDriven {
    /// The entity with the `Animator` (`None`: the character entity itself).
    pub rig: Option<Entity>,
    /// What the root motion drives.
    pub mode: RootMotionApply,
    /// Apply the animation's yaw to the character's facing (overrides `rotation_mode`).
    pub rotation: bool,
    /// Multiplier on the translation (gameplay speed tuning).
    pub scale: f32,
}

impl Default for RootMotionDriven {
    fn default() -> Self {
        RootMotionDriven {
            rig: None,
            mode: RootMotionApply::Horizontal,
            rotation: true,
            scale: 1.0,
        }
    }
}

/// Root motion a caller worked out for the next [`step_character`], in the rig's object space
/// (`regia_framework::net_character` samples it from replicated play state, spec 73 § 6.7).
///
/// While a character has one, the step ignores what the rig's animator accumulated from frame
/// time (it is still drained) and maps the motion to the world with the character's and the
/// rig's `Transform`s, never `GlobalTransform`: a re-run tick sees the same values as the first
/// run. The step removes the component. [`SampledRootMotion::NONE`] says "no root motion this
/// step": input drives the character as if it had no [`RootMotionDriven`].
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct SampledRootMotion {
    /// Translation in rig object space.
    pub translation: Vec3,
    /// Rotation in rig object space.
    pub rotation: Quat,
    /// The seconds of playback this motion covers (0 = no root motion this step).
    pub seconds: f32,
}

impl SampledRootMotion {
    /// No root motion this step.
    pub const NONE: Self = Self {
        translation: Vec3::ZERO,
        rotation: Quat::IDENTITY,
        seconds: 0.0,
    };
}

/// Root motion drained from a rig for one fixed step, in world space.
struct RootMotionStep {
    translation: Vec3,
    yaw: f32,
    seconds: f32,
}

/// Maps rig-space root motion to world space.
fn root_motion_to_world(
    rig_matrix: regia_core::glam::Affine3A,
    translation: Vec3,
    rotation: Quat,
    seconds: f32,
    driven: &RootMotionDriven,
) -> RootMotionStep {
    let (_, rig_rotation, _) = rig_matrix.to_scale_rotation_translation();
    let world_rotation = rig_rotation * rotation * rig_rotation.inverse();
    // The world-space root yaw is the YXZ Euler yaw on the deterministic kernel (see
    // `rotation_yaw`): the pre-fix `to_euler(YXZ)` semantics, with no platform libm in the
    // lockstep path.
    let yaw = rotation_yaw(world_rotation);
    // A rig scaled to zero on an axis has no rotation (the decomposed quaternion is NaN): its
    // root motion turns nothing and moves nothing this step, so a NaN never reaches the
    // character's `Transform`, where every later step would keep multiplying it (audit v033b).
    let translation = rig_matrix.transform_vector3(translation) * driven.scale;
    RootMotionStep {
        translation: if translation.is_finite() {
            translation
        } else {
            Vec3::ZERO
        },
        yaw: if yaw.is_finite() { yaw } else { 0.0 },
        seconds,
    }
}

/// Drain the rig's pending root motion and map it to world space.
fn take_root_motion(
    world: &mut World,
    character: Entity,
    driven: &RootMotionDriven,
) -> Option<RootMotionStep> {
    let rig = driven.rig.unwrap_or(character);
    let rig_matrix = match world.get::<regia_scene::GlobalTransform>(rig) {
        Some(global) => global.0,
        None => {
            let t = world.get::<Transform>(rig).copied()?;
            regia_core::glam::Affine3A::from_scale_rotation_translation(t.scale, t.rotation, t.translation)
        }
    };
    let mut output = world.get_mut::<regia_animation::RootMotionOutput>(rig)?;
    let (translation, rotation, seconds) = output.take();
    if seconds <= 0.0 {
        return None;
    }
    Some(root_motion_to_world(
        rig_matrix,
        translation,
        rotation,
        seconds,
        driven,
    ))
}

/// Take a caller's [`SampledRootMotion`], when the character has one: `Some(None)` means no root
/// motion this step. The rig's matrix comes from `Transform`s only (a re-run tick must not read
/// the frame's `GlobalTransform`).
fn take_sampled_root_motion(
    world: &mut World,
    character: Entity,
    driven: &RootMotionDriven,
) -> Option<Option<RootMotionStep>> {
    let sampled = world.get::<SampledRootMotion>(character).copied()?;
    if let Ok(mut entity) = world.get_entity_mut(character) {
        entity.remove::<SampledRootMotion>();
    }
    let rig = driven.rig.unwrap_or(character);
    // Frame-time motion must not pile up and come back later.
    if let Some(mut output) = world.get_mut::<regia_animation::RootMotionOutput>(rig) {
        output.take();
    }
    if sampled.seconds <= 0.0 {
        return Some(None);
    }
    let matrix_of = |t: &Transform| {
        regia_core::glam::Affine3A::from_scale_rotation_translation(t.scale, t.rotation, t.translation)
    };
    let mut rig_matrix = world
        .get::<Transform>(character)
        .map_or_else(Default::default, matrix_of);
    if rig != character
        && let Some(local) = world.get::<Transform>(rig)
    {
        rig_matrix *= matrix_of(local);
    }
    Some(Some(root_motion_to_world(
        rig_matrix,
        sampled.translation,
        sampled.rotation,
        sampled.seconds,
        driven,
    )))
}

/// Drains the root motion of a character whose step a traversal move took: motion that piled up
/// during the move is not replayed when it ends.
pub(crate) fn drain_root_motion(world: &mut World, entity: Entity) {
    let Some(driven) = world
        .get::<RootMotionDriven>(entity)
        .filter(|d| d.mode != RootMotionApply::Off)
        .cloned()
    else {
        return;
    };
    if take_sampled_root_motion(world, entity, &driven).is_none() {
        let _ = take_root_motion(world, entity, &driven);
    }
}

/// A global root-motion delta (the Tier-1 placeholder; nothing reads it any more).
///
/// Superseded by the per-character [`RootMotionDriven`] component, which drains each rig's
/// `regia_animation::RootMotionOutput`. Kept so existing code that inserts it still compiles.
#[derive(Resource, Debug, Clone, Default)]
pub struct RootMotionDelta {
    /// World-space translation to apply this frame.
    pub translation: Vec3,
    /// World-space rotation delta.
    pub rotation: Quat,
}

// ---------------------------------------------------------------------------
// Movement step
// ---------------------------------------------------------------------------

/// A character whose caller keeps it on its ground (spec 73 § 6.6): it puts the character back on
/// the base before each step, so the collision backend reports the ground but does not carry the
/// character with it. Off (`false`) by default.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CharacterAnchored(pub bool);

/// Whether a character is [`CharacterAnchored`].
pub fn is_anchored(world: &World, entity: Entity) -> bool {
    world.get::<CharacterAnchored>(entity).is_some_and(|a| a.0)
}

/// Marks a character whose step someone else runs: the `FixedUpdate` movement system of
/// [`CharacterPlugin`] leaves it alone and the owner calls [`step_character`] (the predicted
/// schedule, `regia_framework::net_character`, spec 73 § 9).
#[derive(Component, Reflect, Debug, Clone, Default)]
#[reflect(Component, Default)]
pub struct CharacterSteppedExternally;

/// Writes one step's result to the entity's character components.
pub(crate) fn write_step(
    world: &mut World,
    entity: Entity,
    transform: Transform,
    state: CharacterState,
    mode: MovementMode,
) {
    if let Some(mut t) = world.get_mut::<Transform>(entity) {
        *t = transform;
    }
    if let Some(mut cs) = world.get_mut::<CharacterState>(entity) {
        *cs = state;
    }
    if let Some(mut cm) = world.get_mut::<CharacterMovement>(entity) {
        cm.mode = mode;
    }
    if let Some(mut ci) = world.get_mut::<CharacterInput>(entity) {
        ci.jump = false;
    }
}

/// Runs one movement step for one character (spec 73 § 9): the state machine, the sweep through
/// `collision`, and the write-back to the entity's [`Transform`], [`CharacterState`] and
/// [`CharacterMovement::mode`] (it also clears [`CharacterInput::jump`]). It writes nothing else
/// of its own; the collision backend may push bodies and report ground state.
///
/// The step is deterministic given world geometry, the character's components, `input` and `dt`,
/// so it can run again for a past tick (`specs/14` § B4, refined by ADR-1599). The backend is a
/// parameter because the movement system takes it out of the world while stepping; `input` is one
/// because the predicted schedule reads it from a net command, not from the entity.
///
/// Returns the sweep's result with the character's final position; an entity without
/// [`CharacterMovement`] and [`Transform`] gets the default result and no step, and a limp
/// ragdoll reports where it waits.
///
/// The step runs on [`CharacterMovement::sanitized`] settings. A `dt` that is not finite and
/// positive moves nothing and returns the current position; a move that comes out non-finite
/// keeps the previous position and stops the character.
pub fn step_character(
    world: &mut World,
    collision: &dyn CollisionQuery,
    entity: Entity,
    input: &CharacterInput,
    dt: f32,
) -> MoveResult {
    let (Some(movement), Some(transform)) = (
        world.get::<CharacterMovement>(entity).cloned(),
        world.get::<Transform>(entity).copied(),
    ) else {
        return MoveResult::default();
    };
    // Numeric seam (audit-v035-numerics H4/H5): the step runs on sanitized settings (the
    // component keeps what the host wrote), a step that is not finite and positive moves
    // nothing, and a non-finite stored velocity restarts from rest. Without these a NaN or
    // negative turn rate panicked in `clamp`, and one NaN parameter latched a NaN velocity and
    // Transform for good.
    let mut movement = movement.sanitized();
    if !(dt.is_finite() && dt > 0.0) {
        return MoveResult {
            position: transform.translation,
            grounded: world.get::<CharacterState>(entity).is_some_and(|s| s.grounded),
            ground_normal: Vec3::Y,
            ..MoveResult::default()
        };
    }
    let mut state = world.get::<CharacterState>(entity).cloned().unwrap_or_default();
    if !state.velocity.is_finite() {
        state.velocity = Vec3::ZERO;
    }
    if !state.ground_velocity.is_finite() {
        state.ground_velocity = Vec3::ZERO;
    }
    let root_motion = world.get::<RootMotionDriven>(entity).cloned();

    // --- Ragdoll hand-off: a limp or active ragdoll owns the rig; the capsule waits (spec 93 § 10) --
    if world.get::<crate::ragdoll::Ragdoll>(entity).is_some_and(|r| {
        matches!(
            r.mode,
            crate::ragdoll::RagdollMode::Limp | crate::ragdoll::RagdollMode::Active
        )
    }) {
        state.velocity = Vec3::ZERO;
        state.ground_velocity = Vec3::ZERO;
        state.landed_this_frame = false;
        state.jump_buffer_time_left = 0.0;
        let result = MoveResult {
            position: transform.translation,
            grounded: state.grounded,
            ground_normal: Vec3::Y,
            ..MoveResult::default()
        };
        write_step(world, entity, transform, state, movement.mode);
        return result;
    }

    // --- Traversal (docs/specs/traversal.md): only a character that asked for it ---------
    // One component lookup for everyone else, who then steps exactly as before.
    let spent_input;
    let mut input = input;
    if world.get::<crate::traversal::TraversalSettings>(entity).is_some() {
        match crate::traversal::step_traversal(world, collision, entity, &movement, input, dt) {
            crate::traversal::TraversalStep::Done(result) => return result,
            crate::traversal::TraversalStep::PlainInputSpent => {
                spent_input = CharacterInput {
                    jump: false,
                    ..input.clone()
                };
                input = &spent_input;
            }
            crate::traversal::TraversalStep::Plain => {}
        }
        // A move may just have ended, leaving its mode and velocity for the plain step below.
        if let Some(mode) = world.get::<CharacterMovement>(entity).map(|m| m.mode) {
            movement.mode = mode;
        }
        state = world.get::<CharacterState>(entity).cloned().unwrap_or_default();
        if !state.velocity.is_finite() {
            state.velocity = Vec3::ZERO;
        }
        if !state.ground_velocity.is_finite() {
            state.ground_velocity = Vec3::ZERO;
        }
    } else if matches!(movement.mode, MovementMode::CLIMBING | MovementMode::TRAVERSING) {
        // The settings were removed during a move: the move is over.
        movement.mode = MovementMode::Falling;
    }

    let pos = transform.translation;
    let mut vel = state.velocity;
    let was_grounded = state.grounded;
    let previous_ground_velocity = state.ground_velocity;

    // --- Crouch: a shorter capsule with its bottom where the standing one's is -------
    // The transform stays at the standing capsule's centre; the sweep uses the crouched
    // capsule `crouch_drop` lower. Standing up needs headroom (spec 93 § 2.2).
    let crouch_drop = (movement.half_height - movement.crouch_half_height.max(0.0)).max(0.0);
    if input.crouch && crouch_drop > 0.0 {
        state.crouched = true;
    } else if state.crouched {
        let crouched_center = pos - Vec3::Y * crouch_drop;
        state.crouched = crouch_drop > 0.0
            && !collision.has_headroom(
                world,
                entity,
                crouched_center,
                movement.radius,
                movement.crouch_half_height,
                2.0 * crouch_drop,
            );
    }
    let (sweep_half_height, sweep_drop) = if state.crouched {
        (movement.crouch_half_height.max(0.0), crouch_drop)
    } else {
        (movement.half_height, 0.0)
    };

    // --- Timers ------------------------------------------------------
    state.coyote_time_left = (state.coyote_time_left - dt).max(0.0);
    state.jump_buffer_time_left = (state.jump_buffer_time_left - dt).max(0.0);
    if was_grounded {
        state.coyote_time_left = movement.coyote_time;
        state.jumps_used = 0;
    }
    if input.jump {
        state.jump_buffer_time_left = movement.jump_buffer_time;
    }

    // --- Jump (coyote time, jump buffer, extra jumps) --------------------
    let grounded_or_coyote = was_grounded || state.coyote_time_left > 0.0;
    if !grounded_or_coyote && state.jumps_used == 0 {
        // Walked off a ledge and coyote time ran out: the ground jump is gone, so only the
        // air jumps (`max_jumps - 1`) remain.
        state.jumps_used = 1;
    }
    let has_extra_jumps = state.jumps_used > 0 && state.jumps_used < movement.max_jumps;
    let wants_jump = state.jump_buffer_time_left > 0.0;
    if movement.mode.has_gravity()
        && movement.max_jumps > 0
        && wants_jump
        && (grounded_or_coyote || has_extra_jumps)
    {
        vel.y = movement.jump_velocity;
        state.jump_buffer_time_left = 0.0;
        state.jumps_used += 1;
        state.grounded = false;
        state.coyote_time_left = 0.0;
        state.time_in_air = 0.0;
        movement.mode = MovementMode::Falling;
    }

    // --- Horizontal velocity --------------------------------------------
    let max_speed = if state.crouched || input.crouch {
        movement.max_walk_speed * movement.crouch_speed_multiplier
    } else if input.sprint {
        movement.max_walk_speed * movement.sprint_speed_multiplier
    } else {
        movement.max_walk_speed
    };

    let move_dir = input.move_dir.clamp_length_max(1.0);
    let input_mag = move_dir.length().min(1.0);
    let h_vel = Vec3::new(vel.x, 0.0, vel.z);
    let airborne = !state.grounded;
    let accel = if airborne {
        movement.acceleration * movement.air_control
    } else {
        movement.acceleration
    };

    let new_h_vel = if input_mag > 1e-3 {
        let desired = move_dir * max_speed;
        let dv = desired - h_vel;
        let dv_mag = dv.length();
        if dv_mag < accel * dt {
            desired
        } else {
            h_vel + dv.normalize_or_zero() * accel * dt
        }
    } else {
        let brake = if airborne {
            0.0
        } else {
            movement.braking_deceleration
        };
        if h_vel.length() < brake * dt {
            Vec3::ZERO
        } else {
            let dir = h_vel.normalize_or_zero();
            h_vel - dir * brake * dt
        }
    };

    // --- Vertical: flight or gravity --------------------------------------
    if movement.mode == MovementMode::Flying {
        vel = move_dir * max_speed;
    } else if movement.mode.has_gravity() {
        vel = Vec3::new(new_h_vel.x, vel.y, new_h_vel.z);
        vel.y += GRAVITY * movement.gravity_scale * dt;
        if state.grounded && vel.y < 0.0 {
            vel.y = 0.0;
        }
    } else {
        vel = Vec3::new(new_h_vel.x, vel.y, new_h_vel.z);
    }

    // --- Root motion: the animation's displacement replaces the input's --------
    let mut delta = vel * dt;
    let mut root_yaw = None;
    // Animation seconds the root-motion sweep covers (one fixed step when input-driven).
    let mut sweep_seconds = dt;
    if let Some(driven) = root_motion.as_ref().filter(|d| d.mode != RootMotionApply::Off) {
        // Always drain, so motion accumulated in the air is not replayed on landing.
        let sampled = take_sampled_root_motion(world, entity, driven);
        let idle = matches!(sampled, Some(None));
        let step = match sampled {
            Some(step) => step,
            None => take_root_motion(world, entity, driven),
        };
        let full = driven.mode == RootMotionApply::Full;
        if !idle && (full || state.grounded) {
            let seconds = step.as_ref().map_or(dt, |s| s.seconds.max(dt));
            sweep_seconds = seconds;
            let translation = step.as_ref().map_or(Vec3::ZERO, |s| s.translation);
            if step.is_some() {
                vel.x = translation.x / seconds;
                vel.z = translation.z / seconds;
            }
            delta.x = translation.x;
            delta.z = translation.z;
            if full {
                if step.is_some() {
                    vel.y = translation.y / seconds;
                }
                delta.y = translation.y;
            }
            if driven.rotation {
                root_yaw = step.as_ref().map(|s| s.yaw);
            }
        }
    }

    // --- Move through the collision backend -------------------------------
    // Root motion accumulated over several presentation frames (an LOD catch-up or a frame
    // hitch) must not sweep in a single move_shape call: a contact resolves differently than
    // the same animation time moved in fixed-step-sized pieces, and a thin wall can be
    // tunneled. Split the sweep into fixed-step quanta and move through each in order, so any
    // frame pacing that banks the same animation time performs the same sequence of sweeps
    // and stops at the same contact (audit-v040 H1). Each quantum passes the full `dt` (the
    // push-impulse target is remaining_translation / dt, which already reflects the quantum's
    // smaller share) and `dt / quanta` as the additive platform carry, so the carry — the
    // only per-quantum side effect that would otherwise multiply — totals exactly one fixed
    // step per step_character (audit fixrev-06-k H1). The count covers every bankable window
    // (pending motion is capped at MAX_PENDING_SECONDS plus one evaluation), so legitimate
    // quanta never exceed one fixed step of motion each; a pathological caller-supplied
    // SampledRootMotion::seconds stays bounded by the same cap.
    let max_quanta = ((regia_animation::RootMotionOutput::MAX_PENDING_SECONDS / dt).ceil() as u32) + 2;
    let quanta = if sweep_seconds > dt {
        ((sweep_seconds / dt).ceil() as u32).clamp(2, max_quanta)
    } else {
        1
    };
    let carry_dt = dt / quanta as f32;
    let mut result = if quanta > 1 {
        let part = delta / quanta as f32;
        let mut sweep_pos = pos - Vec3::Y * sweep_drop;
        let mut result = collision.move_shape(
            world,
            entity,
            sweep_pos,
            movement.radius,
            sweep_half_height,
            part,
            dt,
            carry_dt,
            movement.max_slope_angle,
            movement.step_height,
            movement.snap_to_ground_distance,
        );
        sweep_pos = result.position;
        // Each quantum carries the character with its ground for `carry_dt` only, so the step
        // carries it once, not `quanta` times (fixrev-06-k H1; corr-framework-a F3 found the same
        // bug, its test `split_root_motion_sweeps_carry_the_ground_once` pins it).
        for _ in 1..quanta {
            let next = collision.move_shape(
                world,
                entity,
                sweep_pos,
                movement.radius,
                sweep_half_height,
                part,
                dt,
                carry_dt,
                movement.max_slope_angle,
                movement.step_height,
                movement.snap_to_ground_distance,
            );
            result.hits.extend_from_slice(&next.hits);
            result.ground_velocity = next.ground_velocity;
            result.ground_entity = next.ground_entity;
            result.ceiling_hit |= next.ceiling_hit;
            sweep_pos = next.position;
            result.position = next.position;
            result.grounded = next.grounded;
            result.ground_normal = next.ground_normal;
            result.ground_angle = next.ground_angle;
        }
        result
    } else {
        collision.move_shape(
            world,
            entity,
            pos - Vec3::Y * sweep_drop,
            movement.radius,
            sweep_half_height,
            delta,
            dt,
            dt,
            movement.max_slope_angle,
            movement.step_height,
            movement.snap_to_ground_distance,
        )
    };
    result.position += Vec3::Y * sweep_drop;

    // --- Polish (spec 93 § 2.2): achieved velocity, ceiling, platform momentum ---------
    // Walking: the horizontal velocity is what the move achieved (minus the ground's own
    // motion), so a wall reads as standing still. Slopes and steps keep the speed (the
    // horizontal motion is at least what a slope slide leaves of the ask); a push against a
    // dynamic body keeps the push.
    let carried = if is_anchored(world, entity) {
        Vec3::ZERO
    } else {
        result.ground_velocity * dt
    };
    let achieved = result.position - pos - carried;
    let intended = Vec3::new(delta.x, 0.0, delta.z);
    let pushing = result.hits.iter().any(|&hit| {
        world
            .get::<regia_physics::RigidBody>(hit)
            .is_some_and(|body| body.kind == regia_physics::BodyKind::Dynamic)
    });
    // The horizontal motion is compared with what a slope slide leaves of a horizontal move
    // (cos² of the ground angle), ADR-7417: comparing the 3D motion let a descent mask a blocked
    // walk, and clamped every climb (a 30° climb keeps cos 30° < 0.9 of the ask in 3D, and the
    // clamp fed back into the next step's speed until the character crawled up at about 1 m/s).
    let slope_cos = result.ground_normal.normalize_or_zero().y.clamp(0.0, 1.0);
    let achieved_horizontal = Vec3::new(achieved.x, 0.0, achieved.z).length();
    if was_grounded
        && result.grounded
        && !pushing
        && intended.length_squared() > 1.0e-10
        && achieved_horizontal < 0.9 * slope_cos * slope_cos * intended.length()
    {
        vel.x = achieved.x / dt;
        vel.z = achieved.z / dt;
    }
    if result.ceiling_hit && vel.y > 0.0 {
        vel.y = 0.0;
    }
    let ground_velocity = if result.grounded {
        result.ground_velocity
    } else {
        Vec3::ZERO
    };
    if was_grounded && !result.grounded {
        // Left the ground (a jump or a ledge): keep the platform's motion.
        vel += previous_ground_velocity;
    } else if !was_grounded && result.grounded {
        // Landed: velocity is relative to the new ground, which the backend carries.
        vel.x -= ground_velocity.x;
        vel.z -= ground_velocity.z;
    }
    state.ground_velocity = ground_velocity;

    // --- Post-state: grounded, landing, mode transitions -------------------
    state.grounded = result.grounded;
    if result.grounded {
        if vel.y < 0.0 {
            vel.y = 0.0;
        }
        if movement.mode == MovementMode::Falling {
            movement.mode = MovementMode::Walking;
        }
        state.time_in_air = 0.0;
    } else {
        state.time_in_air += dt;
        if movement.mode == MovementMode::Walking {
            movement.mode = MovementMode::Falling;
        }
    }
    state.landed_this_frame = !was_grounded && result.grounded;
    state.mode = movement.mode;
    // A move that overflowed (finite but huge settings) or a backend that answered with a
    // non-finite position keeps the last finite pose and stops, instead of latching NaN.
    if !(vel.is_finite() && result.position.is_finite()) {
        vel = Vec3::ZERO;
        result.position = pos;
    }
    state.velocity = vel;

    let new_rotation = turned_rotation(&movement, &transform, move_dir, root_yaw, dt);

    let new_transform = Transform {
        translation: result.position,
        rotation: new_rotation,
        ..transform
    };
    write_step(world, entity, new_transform, state, movement.mode);
    result
}

/// The facing a step ends with: the root motion's yaw, a turn towards the movement direction, or no
/// change (`rotation_mode`). `turn_rate` and `dt` must be finite and at least 0 (sanitized settings).
pub(crate) fn turned_rotation(
    movement: &CharacterMovement,
    transform: &Transform,
    move_dir: Vec3,
    root_yaw: Option<f32>,
    dt: f32,
) -> Quat {
    let input_mag = move_dir.length().min(1.0);
    match (root_yaw, movement.rotation_mode) {
        (Some(yaw), _) => (yaw_rotation(yaw) * transform.rotation).normalize(),
        (None, RotationMode::OrientToMovement) => {
            if input_mag > 1e-3 {
                // Deterministic kernel, not the platform libm: the facing bits must be identical
                // on every target, and read back from the written rotation instead of
                // round-tripping through glam's libm `to_euler` (framework3 determinism audit).
                let target_yaw = det_atan2(-move_dir.x, -move_dir.z);
                let current_yaw = rotation_yaw(transform.rotation);
                let diff = angle_diff(target_yaw, current_yaw);
                // `turn_rate` and `dt` are finite and >= 0 here (sanitized above); the product can
                // still overflow to infinity, which `clamp` accepts.
                let max_turn = movement.turn_rate * dt;
                let step = diff.clamp(-max_turn, max_turn);
                yaw_rotation(current_yaw + step)
            } else {
                transform.rotation
            }
        }
        (None, RotationMode::UseControllerYaw) => transform.rotation,
    }
}

/// While a streamed level is open, characters carry `StreamGuarded` so the streaming guard can hold
/// them (spec 78 § 10.4). Without one nothing is tagged: an app that never streams keeps its
/// components and archetypes.
fn tag_streamed_characters(world: &mut World) {
    if !world
        .get_resource::<regia_scene::streaming::StreamingActive>()
        .is_some_and(|active| active.0)
    {
        return;
    }
    let untagged: Vec<Entity> = world
        .query_filtered::<Entity, (
            With<CharacterMovement>,
            Without<regia_scene::streaming::StreamGuarded>,
        )>()
        .iter(world)
        .collect();
    for entity in untagged {
        world
            .entity_mut(entity)
            .insert(regia_scene::streaming::StreamGuarded);
    }
}

/// `FixedUpdate`: steps every character with [`step_character`], except those marked
/// [`CharacterSteppedExternally`].
///
/// Fixed-step tests drive it with `TimeUpdateStrategy::ManualDuration`.
#[tracing::instrument(skip_all, target = "regia_framework", name = "character_movement")]
pub(crate) fn movement_step(world: &mut World) {
    tag_streamed_characters(world);
    // The fixed step is the integration dt (never the render frame's delta).
    let dt = world
        .get_resource::<FixedTime>()
        .map(|f| 1.0 / f.hz.max(1.0))
        .unwrap_or(1.0 / 60.0) as f32;
    if dt <= 0.0 {
        return;
    }
    // Take the backend out of the world: a real physics backend mutates the world during
    // `move_shape` (rapier pushes bodies), which cannot alias the borrowed resource.
    let Some(collision) = world.remove_resource::<CharacterCollisionQuery>() else {
        return;
    };

    let characters: Vec<(Entity, CharacterInput)> = {
        let mut q = world.query_filtered::<(Entity, Option<&CharacterInput>), (
            With<CharacterMovement>,
            With<Transform>,
            Without<CharacterSteppedExternally>,
            // A character the streaming guard froze waits for the ground to load (spec 78 § 10.4).
            Without<regia_scene::streaming::StreamFrozen>,
        )>();
        q.iter(world)
            .map(|(entity, input)| (entity, input.cloned().unwrap_or_default()))
            .collect()
    };

    for (entity, input) in characters {
        step_character(world, collision.query.as_ref(), entity, &input, dt);
    }

    // Put the backend back for the next step.
    world.insert_resource(collision);
}

/// The yaw-only rotation of `yaw`, built with the deterministic kernel (no platform libm): the
/// same bits on every target, whatever its libm says (spec 15 § 5; the character facing feeds
/// lockstep replays and `NetCharacterBase.stepped_yaw`).
#[must_use]
pub fn yaw_rotation(yaw: f32) -> Quat {
    let (sin, cos) = det_sin_cos(yaw * 0.5);
    Quat::from_xyzw(0.0, sin, 0.0, cos)
}

/// The yaw [`yaw_rotation`] (or a product with one) wrote, read back with the deterministic
/// kernel instead of glam's libm `to_euler`. This is the YXZ Euler yaw (the same extraction
/// `det_yaw_of` performs), preserving the pre-fix `to_euler(YXZ)` semantics bit-for-bit modulo
/// the kernel: a facing authored with pitch or roll reads back its true yaw, not the
/// twist `2 * atan2(q.y, q.w)`. Well-defined away from the gimbal pose (an X rotation of
/// ±π/2), which the characters that travel this path never hold.
#[must_use]
pub fn rotation_yaw(rotation: Quat) -> f32 {
    det_yaw_of(rotation)
}

/// Signed smallest angle from `from` to `to` (in radians).
fn angle_diff(to: f32, from: f32) -> f32 {
    let mut d = to - from;
    while d > std::f32::consts::PI {
        d -= 2.0 * std::f32::consts::PI;
    }
    while d < -std::f32::consts::PI {
        d += 2.0 * std::f32::consts::PI;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Resting capsule-centre height for the default capsule (radius 0.3, half height 0.9).
    const REST_Y: f32 = 1.2;
    /// One fixed step of the test clock (62.5 Hz == a 16 ms manual frame, one step per update).
    const DT: f32 = 1.0 / 62.5;

    /// framework3 review (Muse) medium 2: the pre-fix twist read-back `2 * atan2(q.y, q.w)`
    /// returned 68.9° for a facing authored as Ry(80°)·Rx(30°)·Rz(40°). `rotation_yaw` must
    /// extract the YXZ Euler yaw (the pre-fix `to_euler(YXZ)` semantics) on the deterministic
    /// kernel: the authored yaw, within the kernel's error against the platform libm.
    #[test]
    fn rotation_yaw_reads_the_yxz_yaw_of_a_pitched_facing() {
        let yaw = 80.0f32.to_radians();
        let facing = Quat::from_euler(regia_core::math::EulerRot::YXZ, yaw, 0.5, 0.7);
        let got = rotation_yaw(facing);
        let (want, _, _) = facing.to_euler(regia_core::math::EulerRot::YXZ);
        assert!(
            (got - want).abs() <= 1.0e-4,
            "rotation_yaw {got} vs the YXZ yaw {want}"
        );
        assert!(
            (got - yaw).abs() <= 1.0e-3,
            "the authored yaw reads back, not the twist"
        );
    }

    /// The root-motion yaw a pitched or rolled rig conjugates into world space must be the YXZ
    /// Euler yaw of the world rotation, not the twist: a rig pitched 30° about X conjugating a
    /// root yaw used to come out several degrees off under the twist extraction.
    #[test]
    fn root_motion_yaw_through_a_pitched_rig_is_the_yxz_yaw() {
        let rig = regia_core::glam::Affine3A::from_rotation_translation(
            Quat::from_euler(regia_core::math::EulerRot::YXZ, 0.3, 0.5, 0.1),
            Vec3::ZERO,
        );
        let motion = Quat::from_rotation_y(0.7);
        let driven = RootMotionDriven {
            rotation: true,
            ..RootMotionDriven::default()
        };
        let step = root_motion_to_world(rig, Vec3::ZERO, motion, 0.0, &driven);
        let world = Quat::from_euler(regia_core::math::EulerRot::YXZ, 0.3, 0.5, 0.1)
            * motion
            * Quat::from_euler(regia_core::math::EulerRot::YXZ, 0.3, 0.5, 0.1).inverse();
        let (want, _, _) = world.to_euler(regia_core::math::EulerRot::YXZ);
        assert!(
            (step.yaw - want).abs() <= 1.0e-4,
            "root yaw {} vs the world rotation's YXZ yaw {want}",
            step.yaw
        );
    }

    #[test]
    fn character_movement_defaults_are_reasonable() {
        let cm = CharacterMovement::default();
        assert!(cm.radius > 0.0);
        assert!(cm.half_height > cm.radius);
        assert!(cm.max_walk_speed > 0.0);
        assert!(cm.jump_velocity > 0.0);
        assert!(cm.mode == MovementMode::Walking);
        assert_eq!(cm.max_jumps, 1, "single jump by default");
    }

    #[test]
    fn flat_plane_stops_at_ground() {
        let query = FlatPlaneQuery;
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);

        let pos = Vec3::new(0.0, 1.5, 0.0);
        let result = query.move_shape(
            app.world_mut(),
            Entity::PLACEHOLDER,
            pos,
            0.3,                       // radius
            0.9,                       // half_height
            Vec3::new(2.0, -5.0, 1.0), // delta
            1.0 / 60.0,                // dt
            1.0 / 60.0,                // carry_dt
            0.785,
            0.3,
            0.5,
        );
        // The capsule centre cannot go below radius + half_height = 1.2
        assert!(result.position.y >= 1.2);
        assert!(result.grounded);
        assert_eq!(result.ground_angle, 0.0);
    }

    #[test]
    fn flat_plane_allows_lateral_movement() {
        let query = FlatPlaneQuery;
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);

        let pos = Vec3::new(0.0, 5.0, 0.0);
        let result = query.move_shape(
            app.world_mut(),
            Entity::PLACEHOLDER,
            pos,
            0.3,
            0.9,
            Vec3::new(3.0, 0.0, 0.0),
            1.0 / 60.0,
            1.0 / 60.0,
            0.785,
            0.3,
            0.5,
        );
        assert_eq!(result.position, Vec3::new(3.0, 5.0, 0.0));
        assert!(!result.grounded);
    }

    #[test]
    fn movement_mode_has_gravity() {
        assert!(MovementMode::Walking.has_gravity());
        assert!(MovementMode::Falling.has_gravity());
        assert!(!MovementMode::Flying.has_gravity());
        assert!(MovementMode::Swimming.has_gravity());
        assert!(!MovementMode::Custom(42).has_gravity());
    }

    #[test]
    fn character_state_default() {
        let state = CharacterState::default();
        assert!(!state.grounded);
        assert!(!state.landed_this_frame);
        assert_eq!(state.mode, MovementMode::default());
        assert_eq!(state.jumps_used, 0);
    }

    #[test]
    fn angle_diff_wraps_correctly() {
        assert!((angle_diff(3.0, 0.0) - 3.0).abs() < 0.001);
        assert!((angle_diff(0.0, 3.0) + 3.0).abs() < 0.001);
        // Across the ±π boundary: from 3.0 to -3.0 is -6.0, normalised to 2π - 6.0 ≈ 0.283
        let expected = 2.0 * std::f32::consts::PI - 6.0;
        assert!((angle_diff(-3.0, 3.0) - expected).abs() < 0.01);
    }

    // -----------------------------------------------------------------------
    // Fixed-step system tests (one FixedUpdate per app.update())
    // -----------------------------------------------------------------------

    /// A backend that behaves like the rapier one on a conveyor: every call moves by the request
    /// plus the ground's motion over `carry_dt` (the trait contract), and reports that ground
    /// velocity.
    struct Conveyor {
        velocity: Vec3,
    }

    impl CollisionQuery for Conveyor {
        fn move_shape(
            &self,
            _world: &mut World,
            _entity: Entity,
            position: Vec3,
            _radius: f32,
            _half_height: f32,
            delta: Vec3,
            _dt: f32,
            carry_dt: f32,
            _max_slope_angle: f32,
            _step_height: f32,
            _snap_to_ground: f32,
        ) -> MoveResult {
            MoveResult {
                position: position + delta + self.velocity * carry_dt,
                grounded: true,
                ground_normal: Vec3::Y,
                ground_angle: 0.0,
                hits: Vec::new(),
                ceiling_hit: false,
                ground_velocity: self.velocity,
                ground_entity: None,
            }
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    /// corr-framework-a F3: root motion banked over two steps sweeps in two quanta, but the
    /// character rides its moving ground for one step only. Before the fix each quantum added a
    /// step of the conveyor's motion, so the character moved twice as far with the ground.
    #[test]
    fn split_root_motion_sweeps_carry_the_ground_once() {
        let mut world = World::new();
        let velocity = Vec3::new(3.0, 0.0, 0.0);
        let entity = world
            .spawn((
                Transform::from_translation(Vec3::new(0.0, REST_Y, 0.0)),
                Character,
                CharacterMovement::default(),
                CharacterState {
                    grounded: true,
                    ..CharacterState::default()
                },
                RootMotionDriven {
                    mode: RootMotionApply::Full,
                    ..RootMotionDriven::default()
                },
                SampledRootMotion {
                    translation: Vec3::new(0.0, 0.0, -0.1),
                    rotation: Quat::IDENTITY,
                    seconds: 2.0 * DT,
                },
            ))
            .id();
        let result = step_character(
            &mut world,
            &Conveyor { velocity },
            entity,
            &CharacterInput::default(),
            DT,
        );
        assert!(
            (result.position.x - velocity.x * DT).abs() < 1.0e-5,
            "one step of ground carry ({}), got {}",
            velocity.x * DT,
            result.position.x
        );
        assert!((result.position.z + 0.1).abs() < 1.0e-5, "{}", result.position.z);
    }

    /// A headless app with the character plugin, a custom collision backend and one character.
    fn character_app(query: Box<dyn CollisionQuery>, start: Vec3) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CharacterPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
                DT as f64,
            )));
        let mut fixed = FixedTime::default();
        fixed.hz = 62.5; // step == the manual frame: exactly one FixedUpdate per update()
        app.insert_resource(fixed)
            .insert_resource(CharacterCollisionQuery { query });
        let entity = app
            .world_mut()
            .spawn((
                Transform {
                    translation: start,
                    ..Default::default()
                },
                Character,
                CharacterMovement::default(),
                CharacterInput::default(),
                CharacterState::default(),
            ))
            .id();
        (app, entity)
    }

    /// Writes the character's input for the next step.
    fn set_input(app: &mut App, entity: Entity, move_dir: Vec3, jump: bool) {
        let world = app.world_mut();
        if let Some(mut input) = world.get_mut::<CharacterInput>(entity) {
            input.move_dir = move_dir;
            input.jump = jump;
        }
    }

    /// The character's current state.
    fn state(app: &App, entity: Entity) -> CharacterState {
        app.world()
            .get::<CharacterState>(entity)
            .cloned()
            .expect("character state")
    }

    /// The character's current transform.
    fn translation(app: &App, entity: Entity) -> Vec3 {
        app.world()
            .get::<Transform>(entity)
            .expect("transform")
            .translation
    }

    /// A diagonal stick written as `(1, 0, 1)` requests at most `max_walk_speed`, not 1.41x.
    #[test]
    fn oversized_move_dir_is_clamped_to_unit_length() {
        let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, REST_Y, 0.0));
        set_input(&mut app, e, Vec3::new(1.0, 0.0, 1.0), false);
        for _ in 0..120 {
            app.update();
        }
        let v = state(&app, e).velocity;
        let h = Vec3::new(v.x, 0.0, v.z).length();
        let max = CharacterMovement::default().max_walk_speed;
        assert!(h > max * 0.9, "reaches walk speed: {h}");
        assert!(h <= max + 1e-3, "never above max_walk_speed: {h} > {max}");
    }

    /// Spec 14 B4 `CharacterInput::sprint`: sprinting raises the speed cap to
    /// `max_walk_speed * sprint_speed_multiplier`; releasing it brings the cap back.
    #[test]
    fn sprint_raises_the_speed_cap_by_the_multiplier() {
        let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, REST_Y, 0.0));
        let movement = CharacterMovement::default();
        let horizontal = |app: &App| {
            let v = state(app, e).velocity;
            Vec3::new(v.x, 0.0, v.z).length()
        };
        set_input(&mut app, e, Vec3::X, false);
        if let Some(mut input) = app.world_mut().get_mut::<CharacterInput>(e) {
            input.sprint = true;
        }
        for _ in 0..120 {
            app.update();
        }
        let sprint_cap = movement.max_walk_speed * movement.sprint_speed_multiplier;
        let sprinting = horizontal(&app);
        assert!(
            (sprinting - sprint_cap).abs() < 1e-3,
            "sprint reaches walk speed x multiplier: {sprinting} vs {sprint_cap}"
        );
        if let Some(mut input) = app.world_mut().get_mut::<CharacterInput>(e) {
            input.sprint = false;
        }
        for _ in 0..120 {
            app.update();
        }
        let walking = horizontal(&app);
        assert!(
            (walking - movement.max_walk_speed).abs() < 1e-3,
            "without sprint the cap is max_walk_speed again: {walking}"
        );
    }

    /// Spec 14 B4: `ground_friction` is stored and sanitized but not applied by the step: two
    /// characters that differ only in it move identically (braking is what stops them).
    #[test]
    fn ground_friction_is_stored_but_not_applied() {
        let run = |friction: f32| {
            let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, REST_Y, 0.0));
            if let Some(mut movement) = app.world_mut().get_mut::<CharacterMovement>(e) {
                movement.ground_friction = friction;
            }
            let mut trace = Vec::new();
            for step in 0..90 {
                let dir = if step < 45 { Vec3::X } else { Vec3::ZERO };
                set_input(&mut app, e, dir, false);
                app.update();
                trace.push((translation(&app, e), state(&app, e).velocity));
            }
            trace
        };
        assert_eq!(run(0.0), run(8.0), "ground_friction changes nothing in the step");
        let sanitized = CharacterMovement {
            ground_friction: -3.0,
            ..CharacterMovement::default()
        }
        .sanitized();
        assert!(
            sanitized.ground_friction >= 0.0,
            "a negative friction is sanitized: {}",
            sanitized.ground_friction
        );
    }

    /// Spec 14 B4 `air_control` scales the acceleration in the air (0 = no steering while
    /// falling), and `gravity_scale` scales the fall.
    #[test]
    fn air_control_and_gravity_scale_shape_the_fall() {
        let fall = |air_control: f32, gravity_scale: f32| {
            let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, 100.0, 0.0));
            if let Some(mut movement) = app.world_mut().get_mut::<CharacterMovement>(e) {
                movement.air_control = air_control;
                movement.gravity_scale = gravity_scale;
            }
            set_input(&mut app, e, Vec3::X, false);
            for _ in 0..20 {
                app.update();
            }
            let s = state(&app, e);
            assert!(!s.grounded, "still in the air");
            s.velocity
        };
        let none = fall(0.0, 1.0);
        assert_eq!(none.x, 0.0, "no air control: input does not steer a fall");
        let full = fall(1.0, 1.0);
        assert!(full.x > 0.5, "full air control steers: {full}");
        let half = fall(0.5, 1.0);
        assert!(
            half.x > 0.0 && half.x < full.x,
            "half control steers less: {half} vs {full}"
        );
        let heavy = fall(0.0, 2.0);
        assert!(none.y < 0.0, "falling: {none}");
        assert!(
            (heavy.y / none.y - 2.0).abs() < 1.0e-3,
            "gravity_scale 2 doubles the fall speed: {heavy} vs {none}"
        );
    }

    /// Spec 14 B4: `Flying` has no gravity: an idle flying character holds its height, and the
    /// input's vertical component moves it.
    #[test]
    fn a_flying_character_has_no_gravity_and_flies_where_input_points() {
        let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, 5.0, 0.0));
        if let Some(mut movement) = app.world_mut().get_mut::<CharacterMovement>(e) {
            movement.mode = MovementMode::Flying;
        }
        set_input(&mut app, e, Vec3::ZERO, false);
        for _ in 0..60 {
            app.update();
        }
        let idle = translation(&app, e);
        assert!((idle.y - 5.0).abs() < 1e-4, "no gravity while flying: {idle}");
        assert_eq!(
            state(&app, e).mode,
            MovementMode::Flying,
            "the mode holds in the air"
        );

        set_input(&mut app, e, Vec3::Y, false);
        for _ in 0..10 {
            app.update();
        }
        let up = translation(&app, e);
        assert!(up.y > 5.5, "upward input climbs: {up}");
    }

    /// Gravity/jump/landing: the grounded transitions over fixed steps (§ B4 state machine).
    #[test]
    fn gravity_jump_and_landing_transition_grounded_over_fixed_steps() {
        let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, 5.0, 0.0));
        set_input(&mut app, e, Vec3::ZERO, false);

        // Falling: grounded false, mode Falling, time in air grows, velocity.y is negative.
        app.update();
        let mut last_air = 0.0;
        for _ in 0..40 {
            app.update();
            let s = state(&app, e);
            assert!(!s.grounded, "still falling");
            assert_eq!(s.mode, MovementMode::Falling);
            assert!(s.time_in_air >= last_air, "time in air grows monotonically");
            assert!(s.velocity.y < 0.0, "falling velocity is negative");
            last_air = s.time_in_air;
        }

        // Land: grounded flips true, landed_this_frame fires for exactly one step.
        let mut landed_step = None;
        for step in 0..200 {
            app.update();
            let s = state(&app, e);
            if s.grounded {
                assert!(s.landed_this_frame, "the landing step is marked");
                assert_eq!(s.mode, MovementMode::Walking);
                assert_eq!(s.time_in_air, 0.0);
                assert!((translation(&app, e).y - REST_Y).abs() < 1e-3);
                landed_step = Some(step);
                break;
            }
        }
        let landed_step = landed_step.expect("the character lands within the step budget");
        assert!(landed_step > 0 && landed_step < 100, "reasonable fall time");

        app.update();
        let s = state(&app, e);
        assert!(s.grounded);
        assert!(!s.landed_this_frame, "landed_this_frame lasts exactly one step");
        assert_eq!(s.jumps_used, 0, "landing resets the jump counter");

        // Jump: leaves the ground with the configured velocity.
        set_input(&mut app, e, Vec3::ZERO, true);
        app.update();
        let s = state(&app, e);
        assert!(!s.grounded);
        assert_eq!(s.mode, MovementMode::Falling);
        assert_eq!(s.jumps_used, 1);
        let expected_vy = CharacterMovement::default().jump_velocity + GRAVITY * DT;
        assert!((s.velocity.y - expected_vy).abs() < 1e-4, "jump velocity applied");

        // Airborne jump requests are refused with max_jumps = 1 (no double jump).
        set_input(&mut app, e, Vec3::ZERO, true);
        app.update();
        let s = state(&app, e);
        assert_eq!(s.jumps_used, 1, "no extra jump beyond max_jumps");

        // Falls back down and lands again.
        let mut landed_again = false;
        for _ in 0..200 {
            app.update();
            if state(&app, e).grounded {
                assert!(state(&app, e).landed_this_frame);
                landed_again = true;
                break;
            }
        }
        assert!(landed_again, "the jump ends with a landing");
    }

    /// Walks a character up a 25 cm step (within the 30 cm step height): the character climbs
    /// without leaving the ground.
    #[test]
    fn walking_climbs_a_step_upon_reaching_it() {
        // A step of 0.25 m at x = 2.0.
        let (mut app, e) = character_app(
            Box::new(HeightFieldQuery::new(|x, _z| if x >= 2.0 { 0.25 } else { 0.0 })),
            Vec3::new(0.0, REST_Y, 0.0),
        );
        set_input(&mut app, e, Vec3::new(1.0, 0.0, 0.0), false);

        let mut stepped_up = false;
        for _ in 0..120 {
            app.update();
            let s = state(&app, e);
            assert!(s.grounded, "walking never leaves the ground");
            assert_eq!(s.mode, MovementMode::Walking);
            let y = translation(&app, e).y;
            if y > REST_Y + 0.1 {
                // Past the step: resting on top at the new height.
                assert!((y - (REST_Y + 0.25)).abs() < 1e-3, "resting on the step top: {y}");
                assert!(s.velocity.y == 0.0, "no vertical velocity after stepping up");
                stepped_up = true;
                break;
            }
        }
        assert!(stepped_up, "the character reached the step top");
        assert!(
            translation(&app, e).x > 2.0,
            "the character crossed the step edge"
        );
    }

    /// A step higher than `step_height` blocks horizontal movement; the character stays on the
    /// low side.
    #[test]
    fn a_step_taller_than_step_height_blocks() {
        // A 1.0 m step at x = 2.0 (step height limit is 0.3 m).
        let (mut app, e) = character_app(
            Box::new(HeightFieldQuery::new(|x, _z| if x >= 2.0 { 1.0 } else { 0.0 })),
            Vec3::new(1.7, REST_Y, 0.0),
        );
        set_input(&mut app, e, Vec3::new(1.0, 0.0, 0.0), false);
        for _ in 0..60 {
            app.update();
        }
        let s = state(&app, e);
        assert!(s.grounded, "stays on the ground");
        assert!(
            translation(&app, e).x < 2.0,
            "never crosses the wall: {}",
            translation(&app, e).x
        );
        assert!(
            (translation(&app, e).y - REST_Y).abs() < 1e-3,
            "stays at the low level"
        );
    }

    /// Walks onto a walkable slope: the character follows the terrain height, stays grounded and
    /// keeps walking mode.
    #[test]
    fn walking_follows_a_walkable_slope() {
        // A 0.5 rise per metre (≈26.57°, under the 45° limit).
        let (mut app, e) = character_app(
            Box::new(HeightFieldQuery::new(|x, _z| 0.5 * x)),
            Vec3::new(0.0, REST_Y, 0.0),
        );
        set_input(&mut app, e, Vec3::new(1.0, 0.0, 0.0), false);

        for _ in 0..80 {
            app.update();
            let s = state(&app, e);
            let p = translation(&app, e);
            let ground = 0.5 * p.x;
            assert!(s.grounded, "walking uphill stays grounded");
            assert_eq!(s.mode, MovementMode::Walking);
            assert!(
                (p.y - (ground + REST_Y)).abs() < 1e-3,
                "height follows the ramp: y={} expected {}",
                p.y,
                ground + REST_Y
            );
            if p.x > 5.0 {
                break;
            }
        }
        assert!(translation(&app, e).x > 5.0, "the character made it up the ramp");
    }

    /// Coyote time: a jump pressed right after walking off a ledge still fires.
    #[test]
    fn coyote_time_allows_a_late_jump_off_a_ledge() {
        // A cliff: the ground drops away at x = 3.
        let (mut app, e) = character_app(
            Box::new(HeightFieldQuery::new(|x, _z| if x < 3.0 { 0.0 } else { -10.0 })),
            Vec3::new(0.0, REST_Y, 0.0),
        );
        set_input(&mut app, e, Vec3::new(1.0, 0.0, 0.0), false);

        // Walk off the ledge.
        let mut off_ledge_step = None;
        for step in 0..200 {
            app.update();
            if !state(&app, e).grounded {
                off_ledge_step = Some(step);
                break;
            }
        }
        let _ = off_ledge_step.expect("the character walks off the ledge");

        // Within coyote time (default 0.15 s ≈ 9 steps): the jump still fires.
        set_input(&mut app, e, Vec3::new(1.0, 0.0, 0.0), true);
        app.update();
        let s = state(&app, e);
        assert!(!s.grounded);
        assert_eq!(s.jumps_used, 1, "the coyote jump fires");
        let expected_vy = CharacterMovement::default().jump_velocity + GRAVITY * DT;
        assert!(
            (s.velocity.y - expected_vy).abs() < 1e-4,
            "the late jump applies full velocity"
        );
    }

    /// The jump buffer: a jump pressed shortly before landing fires on touchdown.
    #[test]
    fn jump_buffer_fires_on_touchdown() {
        let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, 1.6, 0.0));
        set_input(&mut app, e, Vec3::ZERO, false);

        // Fall a few steps, then press jump mid-air (buffered, not usable while falling with
        // max_jumps = 1).
        for _ in 0..3 {
            app.update();
        }
        set_input(&mut app, e, Vec3::ZERO, true);
        for _ in 0..3 {
            app.update();
        }
        let before = state(&app, e);
        assert!(!before.grounded, "still falling");
        assert!(
            before.velocity.y < 0.0,
            "the buffered jump has not fired yet: {:?}",
            before.velocity
        );
        // Airborne past coyote time, the ground jump counts as spent (the ledge-jump rule), so
        // with `max_jumps = 1` nothing can fire before touchdown.
        assert_eq!(before.jumps_used, 1, "the spent ground jump, not a fired one");

        // The landing step consumes the buffer and immediately jumps again.
        let mut rebounded = false;
        for _ in 0..50 {
            app.update();
            let s = state(&app, e);
            if s.grounded && s.velocity.y >= 0.0 {
                // Touchdown: the buffered jump launches on the same step.
                rebounded = true;
                break;
            }
        }
        assert!(rebounded, "the buffered jump fires on touchdown");
    }

    /// Double jump: with `max_jumps = 2` a second jump works mid-air; a third does not.
    #[test]
    fn max_jumps_allows_a_double_jump_but_not_a_triple() {
        let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, REST_Y, 0.0));
        if let Some(mut movement) = app.world_mut().get_mut::<CharacterMovement>(e) {
            movement.max_jumps = 2;
        }
        app.update(); // settle: the first step discovers the ground (state starts airborne)
        assert!(state(&app, e).grounded, "resting on the plane");

        set_input(&mut app, e, Vec3::ZERO, true);
        app.update();
        assert_eq!(state(&app, e).jumps_used, 1, "first jump");

        // Still rising: the second jump fires mid-air.
        set_input(&mut app, e, Vec3::ZERO, true);
        app.update();
        let s = state(&app, e);
        assert_eq!(s.jumps_used, 2, "second (double) jump");
        let expected_vy = CharacterMovement::default().jump_velocity + GRAVITY * DT;
        assert!(
            (s.velocity.y - expected_vy).abs() < 1e-4,
            "the double jump applies full velocity"
        );

        // A third press does nothing.
        set_input(&mut app, e, Vec3::ZERO, true);
        app.update();
        assert_eq!(state(&app, e).jumps_used, 2, "no triple jump");
    }

    /// Falling with no coyote time left (ledge exit) still allows the air jumps of `max_jumps`.
    #[test]
    fn ledge_exit_keeps_the_air_jumps() {
        let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, 50.0, 0.0));
        if let Some(mut movement) = app.world_mut().get_mut::<CharacterMovement>(e) {
            movement.max_jumps = 2;
        }
        app.update();
        assert!(!state(&app, e).grounded, "falling");

        set_input(&mut app, e, Vec3::ZERO, true);
        app.update();
        assert_eq!(state(&app, e).jumps_used, 2, "the ground jump counts as used");
        assert!(state(&app, e).velocity.y > 0.0, "the air jump fires");

        set_input(&mut app, e, Vec3::ZERO, true);
        app.update();
        assert_eq!(state(&app, e).jumps_used, 2, "no further jump");
    }

    /// The rapier backend without a `PhysicsWorld` warns and leaves the character in place.
    #[test]
    fn rapier_backend_without_physics_world_does_not_move() {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        let result = RapierCharacterQuery.move_shape(
            app.world_mut(),
            Entity::PLACEHOLDER,
            Vec3::new(1.0, 2.0, 3.0),
            0.3,
            0.9,
            Vec3::new(1.0, 0.0, 0.0),
            1.0 / 60.0,
            1.0 / 60.0,
            0.785,
            0.3,
            0.5,
        );
        assert_eq!(result.position, Vec3::new(1.0, 2.0, 3.0), "no movement");
        assert!(!result.grounded);
        assert_eq!(result.ground_angle, std::f32::consts::FRAC_PI_2);
    }

    /// `use_rapier_character_controller` installs the backend as the app's query resource.
    #[test]
    fn rapier_backend_installs_as_the_app_query_resource() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CharacterPlugin));
        assert!(
            app.world()
                .resource::<CharacterCollisionQuery>()
                .query
                .as_any()
                .is::<FlatPlaneQuery>()
        );
        use_rapier_character_controller(&mut app);
        assert!(
            app.world()
                .resource::<CharacterCollisionQuery>()
                .query
                .as_any()
                .is::<RapierCharacterQuery>()
        );
    }

    /// Braking: no input decelerates a walking character to a stop within the braking budget.
    #[test]
    fn braking_stops_a_walking_character() {
        let (mut app, e) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, REST_Y, 0.0));
        set_input(&mut app, e, Vec3::new(1.0, 0.0, 0.0), false);
        for _ in 0..30 {
            app.update();
        }
        let speed = state(&app, e).velocity.length();
        assert!(speed > 5.0, "reached walking speed: {speed}");

        set_input(&mut app, e, Vec3::ZERO, false);
        let mut stopped = false;
        for _ in 0..60 {
            app.update();
            if state(&app, e).velocity.x <= 1e-4 {
                stopped = true;
                break;
            }
        }
        assert!(stopped, "braking brings the character to a stop");
        assert!(state(&app, e).grounded);
    }

    /// Subscriber and probe for `character_movement_span_wraps_the_step`.
    ///
    /// The layer and the `CollisionQuery` impl live here: a trait impl is not allowed inside the
    /// test function.
    mod movement_span {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};

        use tracing::Subscriber;
        use tracing::span::Id;
        use tracing_subscriber::layer::{Context, Layer};
        use tracing_subscriber::prelude::*;
        use tracing_subscriber::registry::LookupSpan;

        use super::*;

        thread_local! {
            static STACK: std::cell::RefCell<Vec<&'static str>> = const { std::cell::RefCell::new(Vec::new()) };
        }

        struct Events(Arc<Mutex<Vec<String>>>);

        struct Record(Events);

        impl<S> Layer<S> for Record
        where
            S: Subscriber + for<'a> LookupSpan<'a>,
        {
            fn on_enter(&self, id: &Id, ctx: Context<'_, S>) {
                let Some(span) = ctx.span(id) else {
                    return;
                };
                let name = span.name();
                STACK.with(|stack| stack.borrow_mut().push(name));
                if let Ok(mut events) = self.0.0.lock() {
                    events.push(format!("enter {name}"));
                }
            }

            fn on_exit(&self, id: &Id, ctx: Context<'_, S>) {
                let Some(span) = ctx.span(id) else {
                    return;
                };
                STACK.with(|stack| {
                    stack.borrow_mut().pop();
                });
                if let Ok(mut events) = self.0.0.lock() {
                    events.push(format!("exit {}", span.name()));
                }
            }
        }

        struct Probe {
            events: Events,
            inside: Arc<AtomicBool>,
        }

        impl CollisionQuery for Probe {
            fn move_shape(
                &self,
                _world: &mut World,
                _entity: Entity,
                position: Vec3,
                _radius: f32,
                _half_height: f32,
                _delta: Vec3,
                _dt: f32,
                _carry_dt: f32,
                _max_slope_angle: f32,
                _step_height: f32,
                _snap_to_ground: f32,
            ) -> MoveResult {
                let inside = STACK.with(|stack| stack.borrow().contains(&"character_movement"));
                self.inside.store(inside, Ordering::Relaxed);
                if let Ok(mut events) = self.events.0.lock() {
                    let stack = STACK.with(|stack| stack.borrow().clone());
                    events.push(format!("move_shape stack={stack:?}"));
                }
                MoveResult {
                    position,
                    grounded: true,
                    ground_normal: Vec3::Y,
                    ground_angle: 0.0,
                    hits: Vec::new(),
                    ceiling_hit: false,
                    ground_velocity: Vec3::ZERO,
                    ground_entity: None,
                }
            }

            fn as_any(&self) -> &dyn std::any::Any {
                self
            }
        }

        /// One update with streaming off. The sweep must observe `character_movement`.
        pub(super) fn run() {
            let events = Events(Arc::new(Mutex::new(Vec::new())));
            let inside = Arc::new(AtomicBool::new(false));
            // Callsite interest is process-wide: another character test can initialize the
            // movement callsite on a thread without this test's scoped subscriber, leaving
            // the probe's span disabled. Other tests also install a global WARN subscriber.
            // This isolated process installs capture before app setup, for every thread.
            let subscriber = tracing_subscriber::registry().with(Record(Events(Arc::clone(&events.0))));
            tracing::subscriber::set_global_default(subscriber).expect("isolated capture subscriber");
            // Make another thread use the movement callsite first. A scoped subscriber
            // on only this thread lets first-use interest caching disable that callsite.
            std::thread::spawn(|| {
                let (mut other, _) = character_app(Box::new(FlatPlaneQuery), Vec3::new(0.0, REST_Y, 0.0));
                other.update();
            })
            .join()
            .expect("parallel character step");
            let (mut app, _entity) = character_app(
                Box::new(Probe {
                    events: Events(Arc::clone(&events.0)),
                    inside: Arc::clone(&inside),
                }),
                Vec3::new(0.0, REST_Y, 0.0),
            );
            app.update();
            let log = events.0.lock().map(|guard| guard.clone()).unwrap_or_default();
            assert!(
                inside.load(Ordering::Relaxed),
                "character_movement must be current while move_shape runs (streaming is off); {log:?}"
            );
            assert!(
                log.iter()
                    .any(|line| line.starts_with("enter character_movement")),
                "the span was never entered; {log:?}"
            );
        }
    }

    /// `character_movement` stays entered while the sweep runs, with streaming off.
    #[test]
    fn character_movement_span_wraps_the_step() {
        const CHILD_ENV: &str = "REGIA_FRAMEWORK_MOVEMENT_SPAN_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "character::tests::character_movement_span_wraps_the_step",
                    "--nocapture",
                ])
                .env(CHILD_ENV, "1")
                .output()
                .expect("run isolated span test");
            assert!(
                output.status.success(),
                "isolated span test failed: {}\n{}\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert!(
                String::from_utf8_lossy(&output.stdout).contains("1 passed"),
                "the child must execute the span assertion: {}",
                String::from_utf8_lossy(&output.stdout),
            );
            return;
        }
        movement_span::run();
    }
}
