// SPDX-License-Identifier: MPL-2.0
//! Kinematic character controller (specs/15 § 4).
//!
//! Wraps rapier's `KinematicCharacterController`: `move_character` sweeps a capsule through
//! the world with autostep, slope limits, snap-to-ground, slide and a skin width
//! (`offset`), then applies impulses to the dynamic bodies it ran into (proportional to
//! the character's mass). Regia Engine adds the ground report (entity, normal, velocity at the
//! character's position — so moving *and rotating* platforms carry the character), ceiling
//! detection and a pre-move ground probe so a character standing still stays grounded.
//!
//! The framework's `CollisionQuery` adapter (`regia_framework::character`) calls
//! [`move_character`] and writes the [`CharacterGround`](crate::CharacterGround) state; the
//! movement state machine itself lives in the framework.
//!
//! Provenance: rapier3d `control::KinematicCharacterController` public API (Apache-2.0);
//! ground-velocity-at-point `v + ω × (point − center of mass)` is the standard rigid-body
//! point-velocity formula (`RigidBody::velocity_at_point`; `v` is the center-of-mass velocity).

use rapier3d::control::{
    CharacterAutostep, CharacterCollision, CharacterLength, KinematicCharacterController,
};
use rapier3d::math::Vector;
use rapier3d::parry::query::ShapeCastOptions;
use rapier3d::pipeline::QueryFilter;
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::*;

use crate::components::CharacterBody;
use crate::convert::{from_rapier_vec3, to_rapier_pose, to_rapier_vec3};
use crate::world::PhysicsWorld;

/// The result of one character move (spec 15 § 4).
#[derive(Debug, Clone)]
pub struct CharacterMove {
    /// The world-space translation to apply to the character's transform. It already
    /// includes the ground platform's motion this step (`ground_velocity * dt`), except from
    /// [`move_anchored_character`].
    pub translation: Vec3,
    /// Whether the character rests on ground after the move.
    pub grounded: bool,
    /// The ground surface normal (world space, up-ish when grounded).
    pub ground_normal: Vec3,
    /// The entity whose collider is the ground (placeholder if none).
    pub ground_entity: Entity,
    /// The ground body's velocity at the character's position (m/s). Informational: the
    /// platform's motion is already part of `translation`, so adding `ground_velocity * dt`
    /// again would move the character twice as far.
    pub ground_velocity: Vec3,
    /// The ground body's yaw rate (rad/s).
    pub ground_yaw_rate: f32,
    /// Whether the character's head touched a ceiling.
    pub ceiling_hit: bool,
    /// Whether the character is sliding down a too-steep slope.
    pub sliding: bool,
    /// Every entity whose collider the move touched (including walls and ceiling).
    pub hits: Vec<Entity>,
}

/// The character's up vector.
const UP: Vector = Vector::new(0.0, 1.0, 0.0);
/// How far outside the skin width a sweep over kinematic ground starts (metres).
const KINEMATIC_SKIN_MARGIN: f32 = 1.0e-3;
/// Below this squared length (metres squared) a platform's motion in one move counts as none.
const CARRY_EPSILON_SQ: f32 = 1.0e-12;
/// A body moves with the ground under a character when its velocity at the character differs
/// from the ground's by at most this fraction of the ground's speed (plus what the rounding of
/// two kinematic positions can add, see `carry_with_kinematic_ground`).
const MOVES_WITH_GROUND_FRACTION: f32 = 1.0e-3;

/// Collision buffers reused by every move on this thread. The returned `hits` list is the
/// one buffer the caller owns; these two are not.
struct MoveScratch {
    collisions: Vec<CharacterCollision>,
    stair: Vec<CharacterCollision>,
}

thread_local! {
    static MOVE_SCRATCH: std::cell::RefCell<MoveScratch> = const {
        std::cell::RefCell::new(MoveScratch {
            collisions: Vec::new(),
            stair: Vec::new(),
        })
    };
}

/// Moves a capsule character through the world, resolving collisions (spec 15 § 4).
///
/// `pose` is the capsule's world pose (centre); `desired_translation` is where gameplay
/// wants to go this step. `max_slope_climb_angle`, `step_height` and `snap_to_ground` come
/// from the framework's movement settings; the rest from `settings` ([`CharacterBody`]).
/// `exclude_body` names the character's own rigid body (its collider is ignored by the
/// sweep, the ground probe and the push impulses — the classic setup where a gameplay
/// capsule body exists only so sensors can see the character).
///
/// `dt` is the duration of the whole fixed step this move belongs to (it sets the push
/// impulse target, `remaining_translation / dt`). `carry_dt` is this move's own share of
/// that step for the additive moving-platform carry (`ground_velocity * carry_dt`): a caller
/// that splits one step's motion into several sub-moves (root-motion quanta) passes
/// `carry_dt = dt / quanta` per sub-move so the carry totals one step, while the impulses
/// stay scaled to the whole step (audit fixrev-06-k H1).
///
/// Over moving kinematic ground the move has two parts (ADR-5972): the character first moves
/// with the ground, stopping at whatever does not move with it, and then makes its own move over
/// the ground as [`move_anchored_character`] does. The ground carries the character once,
/// whatever its sweep touches.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, target = "regia_physics")]
pub fn move_character(
    world: &mut PhysicsWorld,
    settings: &CharacterBody,
    radius: f32,
    half_height: f32,
    pose: (Vec3, Quat),
    desired_translation: Vec3,
    max_slope_climb_angle: f32,
    step_height: f32,
    snap_to_ground: f32,
    exclude_body: Option<rapier3d::dynamics::RigidBodyHandle>,
    dt: f32,
    carry_dt: f32,
) -> CharacterMove {
    MOVE_SCRATCH.with(|slot| {
        sweep_character(
            world,
            settings,
            radius,
            half_height,
            pose,
            desired_translation,
            max_slope_climb_angle,
            step_height,
            snap_to_ground,
            exclude_body,
            dt,
            carry_dt,
            true,
            pose.0,
            &mut slot.borrow_mut(),
        )
    })
}

/// [`move_character`] for a character its caller keeps on the ground itself (spec 73 § 6.6): the
/// ground is reported as usual, but its motion is not part of the move. The move runs as if the
/// ground stood still at its pose: it leaves out the carry [`move_character`] adds, and also
/// the carry rapier's controller adds on its own for kinematic ground once its cast has stopped
/// at a contact (ADR-5972). The same move over a kinematic body that moves and over one that
/// stands still gives the same translation. That holds for every kinematic body the sweep
/// touches, not only the ground: one that moves into the character does not push it in this
/// move.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, target = "regia_physics")]
pub fn move_anchored_character(
    world: &mut PhysicsWorld,
    settings: &CharacterBody,
    radius: f32,
    half_height: f32,
    pose: (Vec3, Quat),
    desired_translation: Vec3,
    max_slope_climb_angle: f32,
    step_height: f32,
    snap_to_ground: f32,
    exclude_body: Option<rapier3d::dynamics::RigidBodyHandle>,
    dt: f32,
    carry_dt: f32,
) -> CharacterMove {
    MOVE_SCRATCH.with(|slot| {
        sweep_character(
            world,
            settings,
            radius,
            half_height,
            pose,
            desired_translation,
            max_slope_climb_angle,
            step_height,
            snap_to_ground,
            exclude_body,
            dt,
            carry_dt,
            false,
            pose.0,
            &mut slot.borrow_mut(),
        )
    })
}

/// The sweep behind [`move_character`] (`carried_by_ground`) and [`move_anchored_character`].
/// `velocity_at` is where the reported ground velocity is taken: the position the caller's whole
/// move starts from, which the second part of a two-part carried move no longer starts from.
#[allow(clippy::too_many_arguments)]
fn sweep_character(
    world: &mut PhysicsWorld,
    settings: &CharacterBody,
    radius: f32,
    half_height: f32,
    pose: (Vec3, Quat),
    desired_translation: Vec3,
    max_slope_climb_angle: f32,
    step_height: f32,
    snap_to_ground: f32,
    exclude_body: Option<rapier3d::dynamics::RigidBodyHandle>,
    dt: f32,
    carry_dt: f32,
    carried_by_ground: bool,
    velocity_at: Vec3,
    scratch: &mut MoveScratch,
) -> CharacterMove {
    let shape = rapier3d::parry::shape::Capsule::new_y(half_height, radius);
    scratch.collisions.clear();
    scratch.stair.clear();
    let filter = QueryFilter::default().exclude_sensors();
    let filter = match exclude_body {
        Some(handle) => filter.exclude_rigid_body(handle),
        None => filter,
    };

    // 0. Over moving kinematic ground a carried move is two moves: with the ground, then over it
    //    (`carry_with_kinematic_ground`). The second is the anchored move below.
    if carried_by_ground {
        let probe_dist = settings.offset + snap_to_ground.max(0.0) + 1.0e-4;
        let limits = (max_slope_climb_angle, settings.min_slope_slide_angle);
        let with_ground = carry_with_kinematic_ground(
            world,
            settings,
            &shape,
            filter,
            pose,
            probe_dist,
            limits,
            dt,
            carry_dt,
            &mut scratch.collisions,
        );
        if let Some(with_ground) = with_ground {
            let mut result = sweep_character(
                world,
                settings,
                radius,
                half_height,
                (pose.0 + with_ground.translation, pose.1),
                desired_translation,
                max_slope_climb_angle,
                step_height,
                snap_to_ground,
                exclude_body,
                dt,
                carry_dt,
                false,
                velocity_at,
                scratch,
            );
            result.translation += with_ground.translation;
            result.ceiling_hit |= with_ground.ceiling_hit;
            result.hits.extend(with_ground.hits);
            return result;
        }
    }
    let MoveScratch { collisions, stair } = scratch;
    let rapier_pose = to_rapier_pose(pose.0, pose.1);

    // 1. Pre-move ground probe: a short downward cast finds the platform the character
    //    stands on, so its velocity (at the character's position) can be inherited this
    //    step — and so a character standing perfectly still stays grounded.
    let probe_dist = settings.offset + snap_to_ground.max(0.0) + 1.0e-4;
    let probe = {
        let qp = world.query_pipeline(filter);
        shape_cast_down(&qp, &shape, &rapier_pose, probe_dist)
    };
    let platform_velocity = probe
        .map(|(handle, _)| ground_point_velocity(world, handle, velocity_at))
        .unwrap_or(Vec3::ZERO);

    // 1b. The snap below leaves the capsule a fraction of a millimetre inside its skin width. On
    //     other ground the controller's normal nudge frees it; its kinematic-platform friction
    //     removes that nudge, so every cast hits at distance zero and the character stands stuck
    //     on the platform. Sweep from just outside the skin width instead (ADR-1905).
    let lift = probe
        .filter(|(handle, _)| collider_is_kinematic(world, *handle))
        .map_or(0.0, |(_, hit)| {
            (settings.offset.max(1.0e-4) + KINEMATIC_SKIN_MARGIN - hit.time_of_impact).max(0.0)
        });
    let rapier_pose = to_rapier_pose(pose.0 + Vec3::Y * lift, pose.1);

    // 2. Desired movement includes the platform's motion (crushed against walls = stopped).
    // `carry_dt` is this sub-move's share of the step (a root-motion quantum passes
    // dt / quanta) so the additive carry totals one step, never one per quantum. Moving
    // kinematic ground never gets here carried: step 0 took its motion.
    let desired = if carried_by_ground {
        desired_translation + platform_velocity * carry_dt
    } else {
        desired_translation
    };

    // 2b. rapier's controller has a carry of its own for kinematic ground: in every iteration
    //     after its cast stopped at a contact it adds that ground's motion over the `dt` it is
    //     given to the motion still to do. The caller of an anchored move has already moved the
    //     character with its base, so that carry came on top of the anchor: on a client, where
    //     a body the server simulates is a kinematic proxy, a rider gained the proxy's motion of
    //     one step whenever its sweep touched anything, while on the server, where that body is
    //     dynamic, it did not (ADR-5972). An anchored move gives the controller no time to carry
    //     over, so every kinematic body it touches stands still at its pose. `dt` reaches the
    //     controller for nothing else; the push impulses of step 6 keep the whole step.
    //     A carried move keeps the step: the ground it gets here with is not kinematic or does
    //     not move, so the controller's carry has nothing to double, and a kinematic body that
    //     moves into the character still pushes it after a contact.
    let controller_dt = if carried_by_ground { dt } else { 0.0 };

    // 3. The controller sweep.
    let controller = KinematicCharacterController {
        up: UP,
        offset: CharacterLength::Absolute(settings.offset.max(1.0e-4)),
        slide: settings.slide,
        autostep: (step_height > 0.0).then_some(CharacterAutostep {
            max_height: CharacterLength::Absolute(step_height),
            min_width: CharacterLength::Absolute(settings.autostep_min_width),
            include_dynamic_bodies: settings.autostep_include_dynamic,
        }),
        max_slope_climb_angle,
        min_slope_slide_angle: settings.min_slope_slide_angle,
        snap_to_ground: (snap_to_ground > 0.0).then_some(CharacterLength::Absolute(snap_to_ground)),
        normal_nudge_factor: 1.0e-4,
    };
    let mut movement = {
        let qp = world.query_pipeline(filter);
        controller.move_shape(
            controller_dt,
            &qp,
            &shape,
            &rapier_pose,
            to_rapier_vec3(desired),
            |c| collisions.push(c),
        )
    };
    // A surface counts as ground up to the controller's own climb angle (not a fixed 60°).
    // The deterministic cosine: this threshold splits a step's branch, so it must not follow libm.
    let ground_up = (regia_core::math::det_cos(max_slope_climb_angle) - 1.0e-3).clamp(0.01, 0.999);

    // 3b. rapier 0.36's autostep subtracts the whole step height from the motion still to do,
    //     so the sweep that follows pulls a character that got less than a capsule radius past
    //     the stair face back down it: at walking speed it never climbs (ADR-1387). When the
    //     move stopped at a stair it may step onto, step up, move and step down here instead.
    let shared: &PhysicsWorld = world;
    let horizontal = Vec3::new(desired.x, 0.0, desired.z);
    let advance = from_rapier_vec3(movement.translation).dot(horizontal.normalize_or_zero());
    // Any contact but flat ground or a ceiling may be a stair: its face (a wall) or its edge,
    // whose normal tilts with the contact point and can read as a climbable slope while the
    // move still makes no headway. `step_over_stair` decides whether there is a step to take.
    let at_wall = collisions.iter().any(|c| {
        let up_dot = from_rapier_vec3(c.hit.normal1).dot(Vec3::Y);
        up_dot < 0.98
            && up_dot > -0.3
            && (settings.autostep_include_dynamic || !collider_is_dynamic(shared, c.handle))
    });
    let stair_translation = if step_height > 0.0
        && horizontal.length() > 1.0e-5
        && advance < 0.5 * horizontal.length()
        && at_wall
    {
        step_over_stair(
            shared,
            filter,
            &controller,
            &shape,
            &rapier_pose,
            StairStep {
                horizontal,
                blocked_advance: advance,
                step_height,
                min_width: settings.autostep_min_width,
                offset: settings.offset.max(1.0e-4),
                ground_up,
                dt: controller_dt,
            },
            stair,
        )
    } else {
        None
    };
    if let Some(translation) = stair_translation {
        movement.translation = to_rapier_vec3(translation);
        movement.grounded = true;
        movement.is_sliding_down_slope = false;
        collisions.append(stair);
    }

    // 4. Ground/ceiling from the collisions the move reported.
    let mut ground_normal = Vec3::Y;
    let mut ground_entity = Entity::PLACEHOLDER;
    let mut ground_velocity = platform_velocity;
    let mut ground_yaw_rate = 0.0;
    let mut ceiling_hit = false;
    let mut best_up = -f32::MAX;
    let mut hits = Vec::new();
    for collision in collisions.iter() {
        let Some(entity) = world.entity_of_collider(collision.handle) else {
            continue;
        };
        hits.push(entity);
        let normal = from_rapier_vec3(collision.hit.normal1);
        let up_dot = normal.dot(Vec3::Y);
        if up_dot > ground_up && up_dot > best_up {
            best_up = up_dot;
            ground_normal = normal;
            ground_entity = entity;
            ground_velocity = ground_point_velocity(world, collision.handle, velocity_at);
            ground_yaw_rate = ground_yaw(world, collision.handle);
        }
        if up_dot < -0.3 {
            ceiling_hit = true;
        }
    }

    // 5. Standing still: move_shape depenetrates but never reports ground on a zero move;
    //    keep the pre-probe's verdict so the character does not micro-fall.
    let mut grounded = movement.grounded;
    if !grounded
        && ground_entity == Entity::PLACEHOLDER
        && desired_translation.length_squared() < 1.0e-8
        && let Some((handle, hit)) = probe
        && from_rapier_vec3(hit.normal1).dot(Vec3::Y) > ground_up
    {
        grounded = true;
        ground_entity = world.entity_of_collider(handle).unwrap_or(Entity::PLACEHOLDER);
        ground_normal = from_rapier_vec3(hit.normal1);
        ground_velocity = ground_point_velocity(world, handle, velocity_at);
        ground_yaw_rate = ground_yaw(world, handle);
    }

    // 5b. Grounded but the move never collided with an up-facing surface (e.g. held in
    //     place by snap-to-ground or by walls): report the probe's ground.
    if grounded
        && ground_entity == Entity::PLACEHOLDER
        && let Some((handle, hit)) = probe
        && from_rapier_vec3(hit.normal1).dot(Vec3::Y) > ground_up
    {
        ground_entity = world.entity_of_collider(handle).unwrap_or(Entity::PLACEHOLDER);
        ground_normal = from_rapier_vec3(hit.normal1);
        ground_velocity = ground_point_velocity(world, handle, velocity_at);
        ground_yaw_rate = ground_yaw(world, handle);
    }

    // 6. Push the dynamic bodies the character ran into (impulse ∝ character mass).
    if settings.mass > 0.0 && !collisions.is_empty() {
        let mut qp_mut = world.query_pipeline_mut(filter);
        controller.solve_character_collision_impulses(
            dt,
            &mut qp_mut,
            &shape,
            settings.mass,
            collisions.iter(),
        );
    }

    CharacterMove {
        translation: from_rapier_vec3(movement.translation) + Vec3::Y * lift,
        grounded,
        ground_normal,
        ground_entity,
        ground_velocity,
        ground_yaw_rate,
        ceiling_hit,
        sliding: movement.is_sliding_down_slope,
        hits,
    }
}

/// What moving with the ground did to a character ([`carry_with_kinematic_ground`]).
struct GroundCarry {
    /// How far the character moved with the ground (world space, metres).
    translation: Vec3,
    /// The entities the move stopped at.
    hits: Vec<Entity>,
    /// Whether the move pressed the character's head against something.
    ceiling_hit: bool,
}

/// The first part of a carried move over moving kinematic ground (ADR-5972): the character moves
/// with the ground, by the ground's motion over `carry_dt` at the character's position. Bodies
/// that move with the ground (the ground itself, a railing on it) cannot stop this move, because
/// the character does not move relative to them; everything else can (crushed against a wall =
/// stopped). Returns `None` when the character is not over kinematic ground that moves.
///
/// The one-sweep carried move added the ground's motion to the desired move and swept the sum.
/// Over kinematic ground rapier's controller then added the ground's motion again after every
/// contact, so a character pressed against a wall rode a platform at twice its speed along the
/// wall, and one whose cast grazed the deck jumped ahead by a step of the platform. The sum also
/// swept the character along bodies that had already moved with the ground, at the skin width,
/// where a cast often reports a contact at distance zero and the carry was lost instead.
#[allow(clippy::too_many_arguments)]
fn carry_with_kinematic_ground(
    world: &mut PhysicsWorld,
    settings: &CharacterBody,
    shape: &rapier3d::parry::shape::Capsule,
    filter: QueryFilter<'_>,
    pose: (Vec3, Quat),
    probe_dist: f32,
    (max_slope_climb_angle, min_slope_slide_angle): (f32, f32),
    dt: f32,
    carry_dt: f32,
    collisions: &mut Vec<CharacterCollision>,
) -> Option<GroundCarry> {
    let rapier_pose = to_rapier_pose(pose.0, pose.1);
    let shared: &PhysicsWorld = world;
    let (ground, _) = {
        let qp = shared.query_pipeline(filter);
        shape_cast_down(&qp, shape, &rapier_pose, probe_dist)
    }?;
    if !collider_is_kinematic(shared, ground) {
        return None;
    }
    let ground_velocity = ground_point_velocity(shared, ground, pose.0);
    let carry = ground_velocity * carry_dt;
    if carry.length_squared() <= CARRY_EPSILON_SQ {
        return None;
    }

    // What moves with the ground. A kinematic body's velocity is its last move divided by the
    // step, so two bodies that gameplay moved by the same amount differ by the rounding of their
    // positions: a few units in the last place of a coordinate, per step. The allowance for it
    // is a precaution from that arithmetic; no failure was reproduced without it.
    let rounding = 4.0 * f32::EPSILON * pose.0.abs().max_element().max(1.0) / dt.max(1.0e-6);
    let slack = MOVES_WITH_GROUND_FRACTION * ground_velocity.length() + rounding;
    let slack = slack * slack;
    // The move with the ground: no time step (ADR-5972: the controller adds no carry of its own
    // and every other kinematic body stands still for it), no stairs, no snapping.
    let stands_apart = |_: rapier3d::geometry::ColliderHandle, collider: &rapier3d::geometry::Collider| {
        let Some(body) = collider.parent().and_then(|parent| shared.bodies.get(parent)) else {
            return true;
        };
        if body.is_fixed() {
            return true;
        }
        let velocity = from_rapier_vec3(body.velocity_at_point(to_rapier_vec3(pose.0)));
        (velocity - ground_velocity).length_squared() > slack
    };
    let controller = KinematicCharacterController {
        up: UP,
        offset: CharacterLength::Absolute(settings.offset.max(1.0e-4)),
        slide: settings.slide,
        autostep: None,
        max_slope_climb_angle,
        min_slope_slide_angle,
        snap_to_ground: None,
        normal_nudge_factor: 1.0e-4,
    };
    collisions.clear();
    let moved = {
        let qp = shared.query_pipeline(filter.predicate(&stands_apart));
        controller.move_shape(0.0, &qp, shape, &rapier_pose, to_rapier_vec3(carry), |c| {
            collisions.push(c)
        })
    };
    let mut hits = Vec::new();
    let mut ceiling_hit = false;
    for collision in collisions.iter() {
        if let Some(entity) = shared.entity_of_collider(collision.handle) {
            hits.push(entity);
        }
        if from_rapier_vec3(collision.hit.normal1).dot(Vec3::Y) < -0.3 {
            ceiling_hit = true;
        }
    }
    // The ground pushes the dynamic bodies it carries the character into.
    if settings.mass > 0.0 && !collisions.is_empty() {
        let mut qp_mut = world.query_pipeline_mut(filter);
        controller.solve_character_collision_impulses(
            dt,
            &mut qp_mut,
            shape,
            settings.mass,
            collisions.iter(),
        );
    }
    Some(GroundCarry {
        translation: from_rapier_vec3(moved.translation),
        hits,
        ceiling_hit,
    })
}

/// The geometry of one [`step_over_stair`] attempt.
struct StairStep {
    /// The horizontal part of the desired move (world space, metres).
    horizontal: Vec3,
    /// How far along `horizontal` the controller's own move got.
    blocked_advance: f32,
    /// Highest step to climb (metres).
    step_height: f32,
    /// Free width the step must offer ahead of the character (metres).
    min_width: f32,
    /// The controller's skin width (metres).
    offset: f32,
    /// Cosine of the steepest walkable slope.
    ground_up: f32,
    /// The time the raised move's controller may carry the character with kinematic ground
    /// over, seconds: the fixed step, or 0 for an anchored move (ADR-5972).
    dt: f32,
}

/// Climbs a stair rapier's autostep left the character stuck at (ADR-1387): casts the capsule
/// up by at most `step_height`, sweeps the horizontal move from there and casts it back down.
/// The step is taken only when the raised capsule has `min_width` of room ahead, walkable
/// ground lies under that point within the rise (the same tests rapier's autostep makes), the
/// raised move gets further than the blocked one, and the capsule lands on walkable footing:
/// a short move that would leave it on the stair's edge is lengthened up to `min_width` (the
/// step then carries the character a little further than asked, like rapier's own nudge). The
/// capsule never ends below its start. Returns the translation of the raised sweep and
/// writes that sweep's collisions into `collisions` (cleared on every attempt).
fn step_over_stair(
    world: &PhysicsWorld,
    filter: QueryFilter<'_>,
    controller: &KinematicCharacterController,
    shape: &dyn rapier3d::geometry::Shape,
    start: &rapier3d::math::Pose,
    step: StairStep,
    collisions: &mut Vec<CharacterCollision>,
) -> Option<Vec3> {
    let length = step.horizontal.length();
    let dir = step.horizontal / length;
    let cast = |pose: &rapier3d::math::Pose, towards: Vec3, max: f32| {
        let qp = world.query_pipeline(filter);
        qp.cast_shape(
            pose,
            to_rapier_vec3(towards),
            shape,
            ShapeCastOptions {
                max_time_of_impact: max,
                target_distance: step.offset,
                stop_at_penetration: false,
                compute_impact_geometry_on_penetration: true,
            },
        )
    };
    let shifted = |by: Vec3, pose: &rapier3d::math::Pose| {
        rapier3d::math::Pose::from_translation(to_rapier_vec3(by)) * *pose
    };

    // 1. Up, as far as the head allows.
    let rise = cast(start, Vec3::Y, step.step_height).map_or(step.step_height, |(_, hit)| hit.time_of_impact);
    if rise <= step.offset {
        return None;
    }
    let raised = shifted(Vec3::Y * rise, start);
    // 2. Room on the step and walkable ground under it (rapier's autostep tests): `min_width`
    //    ahead, not the whole move. A move longer than that may end at the next riser of the
    //    stair; the raised move below stops there.
    if cast(&raised, dir, step.min_width).is_some() {
        return None;
    }
    let (_, ground) = cast(&shifted(dir * step.min_width, &raised), Vec3::NEG_Y, rise)?;
    if from_rapier_vec3(ground.normal1).dot(Vec3::Y) <= step.ground_up {
        return None;
    }
    // 3. The horizontal move from the raised pose, without autostep or snapping; 4. back down
    //    onto the step, never below the start height. A capsule that would land on the stair's
    //    edge steeper than it may climb slides back off next step, so the move is lengthened
    //    (doubling, at most to `reach`) until it lands on walkable footing.
    let reach = step.min_width.max(length);
    let flat = KinematicCharacterController {
        autostep: None,
        snap_to_ground: None,
        ..*controller
    };
    let mut wanted = length;
    loop {
        collisions.clear();
        let moved = {
            let qp = world.query_pipeline(filter);
            flat.move_shape(step.dt, &qp, shape, &raised, to_rapier_vec3(dir * wanted), |c| {
                collisions.push(c)
            })
        };
        let moved = from_rapier_vec3(moved.translation);
        if moved.dot(dir) <= step.blocked_advance + 1.0e-4 {
            return None;
        }
        let landing = cast(&shifted(moved, &raised), Vec3::NEG_Y, rise);
        let footing = landing
            .as_ref()
            .is_none_or(|(_, hit)| from_rapier_vec3(hit.normal1).dot(Vec3::Y) > step.ground_up);
        if footing {
            let drop = landing.as_ref().map_or(rise, |(_, hit)| hit.time_of_impact);
            return Some(Vec3::Y * (rise - drop) + moved);
        }
        if wanted >= reach {
            return None;
        }
        wanted = (wanted * 2.0).min(reach);
    }
}

/// Whether a capsule (`radius`, `half_height`, centred at `center`, upright) can rise by
/// `rise` metres plus the skin `offset` without touching a non-sensor collider (spec 93
/// § 2.2: a crouched character standing up). `exclude_body` is the character's own body.
/// Contacts the capsule already has and moves away from (the floor) do not count.
pub fn capsule_has_headroom(
    world: &PhysicsWorld,
    radius: f32,
    half_height: f32,
    center: Vec3,
    rise: f32,
    offset: f32,
    exclude_body: Option<rapier3d::dynamics::RigidBodyHandle>,
) -> bool {
    if rise <= 0.0 {
        return true;
    }
    let shape = rapier3d::parry::shape::Capsule::new_y(half_height.max(0.0), radius);
    let filter = QueryFilter::default().exclude_sensors();
    let filter = match exclude_body {
        Some(handle) => filter.exclude_rigid_body(handle),
        None => filter,
    };
    let qp = world.query_pipeline(filter);
    let options = ShapeCastOptions {
        max_time_of_impact: rise + offset.max(0.0),
        target_distance: 0.0,
        stop_at_penetration: false,
        compute_impact_geometry_on_penetration: false,
    };
    qp.cast_shape(&to_rapier_pose(center, Quat::IDENTITY), UP, &shape, options)
        .is_none()
}

/// True when the collider belongs to a dynamic body.
fn collider_is_dynamic(world: &PhysicsWorld, collider: rapier3d::geometry::ColliderHandle) -> bool {
    world
        .colliders
        .get(collider)
        .and_then(|c| c.parent())
        .and_then(|body| world.bodies.get(body))
        .is_some_and(|body| body.is_dynamic())
}

fn collider_is_kinematic(world: &PhysicsWorld, collider: rapier3d::geometry::ColliderHandle) -> bool {
    world
        .colliders
        .get(collider)
        .and_then(|c| c.parent())
        .and_then(|body| world.bodies.get(body))
        .is_some_and(|body| body.is_kinematic())
}

/// Casts `shape` straight down from `pose`; returns the first hit within `max_dist`.
fn shape_cast_down(
    qp: &rapier3d::pipeline::QueryPipeline<'_>,
    shape: &dyn rapier3d::geometry::Shape,
    pose: &rapier3d::math::Pose,
    max_dist: f32,
) -> Option<(
    rapier3d::geometry::ColliderHandle,
    rapier3d::geometry::ShapeCastHit,
)> {
    let options = ShapeCastOptions {
        max_time_of_impact: max_dist,
        target_distance: 0.0,
        stop_at_penetration: true,
        compute_impact_geometry_on_penetration: true,
    };
    qp.cast_shape(pose, rapier3d::math::Vector::new(0.0, -1.0, 0.0), shape, options)
}

/// The velocity of a collider's parent body at `point` (m/s): `v + ω × r` (spec 15 § 4
/// moving/rotating platform support). Rapier's `linvel` is the velocity of the center of mass, so
/// `r` runs from the center of mass, not from the body origin.
pub(crate) fn ground_point_velocity(
    world: &PhysicsWorld,
    collider: rapier3d::geometry::ColliderHandle,
    point: Vec3,
) -> Vec3 {
    let Some(collider) = world.colliders.get(collider) else {
        return Vec3::ZERO;
    };
    let Some(parent) = collider.parent() else {
        return Vec3::ZERO;
    };
    let Some(body) = world.bodies.get(parent) else {
        return Vec3::ZERO;
    };
    let linvel = from_rapier_vec3(body.linvel());
    let angvel = from_rapier_vec3(body.angvel());
    let com = from_rapier_vec3(body.center_of_mass());
    linvel + angvel.cross(point - com)
}

/// The yaw rate of a collider's parent body (rad/s around world up).
fn ground_yaw(world: &PhysicsWorld, collider: rapier3d::geometry::ColliderHandle) -> f32 {
    let Some(collider) = world.colliders.get(collider) else {
        return 0.0;
    };
    let Some(parent) = collider.parent() else {
        return 0.0;
    };
    world
        .bodies
        .get(parent)
        .map(|body| body.angvel().y)
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn character_move_defaults() {
        let m = CharacterMove {
            translation: Vec3::ZERO,
            grounded: false,
            ground_normal: Vec3::Y,
            ground_entity: Entity::PLACEHOLDER,
            ground_velocity: Vec3::ZERO,
            ground_yaw_rate: 0.0,
            ceiling_hit: false,
            sliding: false,
            hits: Vec::new(),
        };
        assert!(!m.grounded);
        assert_eq!(m.ground_normal, Vec3::Y);
    }

    /// Audit g-physics2 finding 9: the clearance on the raised step is `min_width`, as the docs
    /// and rapier's own autostep test, not the whole desired move. A 0.40 m move at a stair
    /// whose 0.35 m tread ends at a wall used to be refused.
    #[test]
    fn a_long_move_climbs_a_tread_that_ends_within_the_move() {
        use crate::components::{Collider, ColliderShape, RigidBody};
        use regia_app::{App, FixedTime, TimeUpdateStrategy};
        use regia_scene::Transform;

        const RADIUS: f32 = 0.3;
        const HALF_HEIGHT: f32 = 0.6;
        const OFFSET: f32 = 0.02;
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(crate::PhysicsPlugin);
        let mut static_box = |half: Vec3, centre: Vec3| {
            app.world_mut().spawn((
                Transform::from_translation(centre),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box { half_extents: half },
                    ..Collider::default()
                },
            ));
        };
        // Ground at y = 0; a 0.15 m riser at x = 0.5, then a 0.35 m tread up to a wall at
        // x = 0.85 (tall, so the raised capsule cannot pass over it the way its rounded bottom
        // clears a low riser's corner).
        static_box(Vec3::new(20.0, 0.5, 20.0), Vec3::new(0.0, -0.5, 0.0));
        static_box(Vec3::new(2.25, 0.075, 3.0), Vec3::new(2.75, 0.075, 0.0));
        static_box(Vec3::new(2.075, 0.75, 3.0), Vec3::new(2.925, 0.75, 0.0));
        for _ in 0..5 {
            app.update();
        }
        let world = app.world().resource::<PhysicsWorld>();
        let shape = rapier3d::geometry::SharedShape::capsule_y(HALF_HEIGHT, RADIUS);
        let controller = KinematicCharacterController {
            offset: CharacterLength::Absolute(OFFSET),
            ..KinematicCharacterController::default()
        };
        // Stopped 1 cm outside the skin width in front of the first riser, standing on the
        // ground (where the controller's sweep leaves it; exactly at the skin width the upward
        // cast already counts the riser face as a hit).
        let start = to_rapier_pose(
            Vec3::new(0.5 - RADIUS - OFFSET - 0.01, HALF_HEIGHT + RADIUS + OFFSET, 0.0),
            Quat::IDENTITY,
        );
        let mut collisions = Vec::new();
        let stepped = step_over_stair(
            world,
            QueryFilter::default(),
            &controller,
            &*shape,
            &start,
            StairStep {
                horizontal: Vec3::new(0.40, 0.0, 0.0),
                blocked_advance: 0.0,
                step_height: 0.2,
                min_width: 0.15,
                offset: OFFSET,
                ground_up: 0.64,
                dt: 1.0 / 60.0,
            },
            &mut collisions,
        );
        let translation = stepped.expect("the 0.35 m tread has min_width of room: climb");
        assert!(
            (translation.y - 0.15).abs() < 0.03,
            "risen by the 0.15 m riser: {translation:?}"
        );
        assert!(
            translation.x > 0.15 && translation.x < 0.37,
            "onto the tread, short of the wall: {translation:?}"
        );

        // A tread narrower than min_width is still refused (the existing contract).
        let narrow = step_over_stair(
            world,
            QueryFilter::default(),
            &controller,
            &*shape,
            &start,
            StairStep {
                horizontal: Vec3::new(0.40, 0.0, 0.0),
                blocked_advance: 0.0,
                step_height: 0.2,
                min_width: 0.40,
                offset: OFFSET,
                ground_up: 0.64,
                dt: 1.0 / 60.0,
            },
            &mut collisions,
        );
        assert!(narrow.is_none(), "{narrow:?}");
    }
}
