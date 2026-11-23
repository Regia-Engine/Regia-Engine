// SPDX-License-Identifier: MPL-2.0
//! Opt-in swept CCD in the physics step: a time-of-impact substep for bodies that spin
//! or fly fast enough to pass through thin geometry in one step (ADR-2423).
//!
//! Add [`SweptCcd`] to a dynamic body. Each fixed step, after the solver has moved the
//! body, its whole step motion (the centre of mass translating, the body turning about it
//! at the step's angular velocity, any number of turns) is swept against the colliders
//! near its path with [`crate::ccd::time_of_impact_motion`], the conservative-advancement
//! angular sweep. If the sweep reaches another collider first, the body is put back at the
//! time of impact, just short of contact (`skin`), and an inelastic contact impulse at the
//! impact point (with the pair's restitution) removes the closing velocity. The solver's
//! speculative contacts take over from the next step, so the rest of the step's motion is
//! dropped (a "time-of-impact clamp").
//!
//! Rapier 0.36 already sweeps fast bodies against *fixed* colliders by itself (and bullets,
//! `RigidBody::ccd`, against every body type except other bullets). `SweptCcd` covers what
//! that leaves: thin kinematic and dynamic bodies (doors, platforms, vehicle panels, debris)
//! hit by bodies that are not bullets, and two bullets.
//!
//! It is off by default: without any `SweptCcd` body the step does no extra work and the
//! rapier world is bit-identical to a build without this module. It is independent of
//! [`RigidBody::ccd`](crate::RigidBody::ccd) (rapier's own translational CCD); a body may
//! use both.
//!
//! What it sweeps against: every non-sensor collider of another body (or a standalone
//! collider) whose collision and solver groups interact with the body's collider and
//! whose end-of-step bounds meet the swept bounds. A moving body is swept along its own step
//! motion (its exact one when it is a `SweptCcd` body too, else traced back from its pose and
//! velocity), so two fast bodies meeting head-on hit each other; fixed and resting ones are
//! held at their pose. Supported shapes
//! (both sides): sphere, box, capsule, cylinder, cone, convex hull, triangle, segment and
//! compounds of those; on the other side also triangle meshes and heightfields (their
//! triangles inside the swept bounds, at most [`MAX_TRIANGLES_PER_PAIR`]). Rounded shapes,
//! half-spaces, polylines and voxels are skipped. A pair already within the solver's
//! prediction distance at the start of the step is left to the solver, unless part of the
//! body swings deeper than that distance through the starting contact plane (a bar pivoting
//! on its tip into a wall): then the first such crossing is the impact. The impulse is
//! applied to the swept body only (the other body is treated as immovable for that
//! impulse; the solver resolves both from the next step).
//!
//! Provenance: conservative advancement (Mirtich 1996), see [`crate::ccd`]; the clamp and
//! impulse are the standard single-contact inelastic impulse
//! `j = (1 + e) v_n / (n · M⁻¹ n + (r × n) · I⁻¹ (r × n))`. Original code.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;

use rapier3d::dynamics::{RigidBodyHandle as RapierBodyHandle, RigidBodyType};
use rapier3d::geometry::{ColliderHandle as RapierColliderHandle, Shape, TypedShape};
use rapier3d::parry::bounding_volume::Aabb;
use rapier3d::pipeline::{QueryFilter, QueryFilterFlags};
use regia_core::math::{Isometry, Quat, Vec3, det_atan2, det_cos, det_sin};
use regia_ecs::prelude::*;

use crate::ccd::{
    ConvexInput, RigidMotion, ToiOptions, ToiResult, convex_distance, radius_about, time_of_impact_motion,
};
use crate::components::RigidBodyHandle;
use crate::convert::{from_rapier_pose, from_rapier_vec3, to_rapier_pose, to_rapier_vec3};
use crate::world::PhysicsWorld;

/// The most triangles of one mesh or heightfield swept against one body collider per step.
pub const MAX_TRIANGLES_PER_PAIR: usize = 256;

// Fresh point buffers, triangles, motion maps and motion lookups on this thread.
// A warm sweep must not bump the fresh-buffer count: the buffers are reused.
// The allocation test reads these; gameplay does not. Per thread so parallel tests
// do not count each other's sweeps.
thread_local! {
    static CCD_FRESH_POINT_VECS: Cell<u64> = const { Cell::new(0) };
    static CCD_TRIANGLES: Cell<u64> = const { Cell::new(0) };
    static CCD_MOTION_MAPS: Cell<u64> = const { Cell::new(0) };
    static CCD_MOTION_PROBES: Cell<u64> = const { Cell::new(0) };
}

fn ccd_add(counter: &'static std::thread::LocalKey<Cell<u64>>) {
    counter.with(|c| c.set(c.get().saturating_add(1)));
}

/// Reads a sweep counter. The integration-test build of the library does not run the unit
/// tests, so this stays out of that build.
#[cfg(test)]
fn ccd_read(counter: &'static std::thread::LocalKey<Cell<u64>>) -> u64 {
    counter.with(Cell::get)
}

/// Buffers a swept step reuses: candidate lists, convex parts and the triangle point pool.
struct SweepScratch {
    body_colliders: Vec<RapierColliderHandle>,
    candidates: Vec<RapierColliderHandle>,
    parts_a: Vec<(ConvexInput, Isometry)>,
    parts_b: Vec<(ConvexInput, Isometry)>,
    point_pool: Vec<Vec<Vec3>>,
    motions: BTreeMap<(u32, u32), (RigidMotion, Isometry)>,
}

thread_local! {
    static SWEEP_SCRATCH: RefCell<SweepScratch> = const {
        RefCell::new(SweepScratch {
            body_colliders: Vec::new(),
            candidates: Vec::new(),
            parts_a: Vec::new(),
            parts_b: Vec::new(),
            point_pool: Vec::new(),
            motions: BTreeMap::new(),
        })
    };
}

/// A point buffer from the pool, or a fresh one when the pool is empty.
fn take_points(pool: &mut Vec<Vec<Vec3>>) -> Vec<Vec3> {
    match pool.pop() {
        Some(mut buf) => {
            buf.clear();
            buf
        }
        None => {
            ccd_add(&CCD_FRESH_POINT_VECS);
            Vec::new()
        }
    }
}

/// Returns hull point buffers to the pool. Other convex parts own no buffer.
fn recycle_parts(parts: &mut Vec<(ConvexInput, Isometry)>, pool: &mut Vec<Vec<Vec3>>) {
    for (input, _) in parts.drain(..) {
        if let ConvexInput::ConvexHull { mut points } = input {
            points.clear();
            pool.push(points);
        }
    }
}

/// Opt-in swept CCD for one dynamic body (ADR-2423): its step motion, rotation included,
/// is swept against nearby colliders and clamped at the first impact. See
/// [`crate::ccd_step`] for what it does and does not cover. Off unless the component is
/// present; the defaults suit bodies from bullets to thrown bars.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default)]
pub struct SweptCcd {
    /// Separation at which an impact stops the body, metres (default 0.005). Clamped to
    /// half the solver's prediction distance (0.02 m by default) so the solver always picks
    /// the contact up on the next step.
    pub skin: f32,
    /// Conservative-advance budget per swept pair (default 64). A pair that runs out is
    /// counted in `failures` and not clamped.
    pub max_iterations: u32,
    /// Steps on which this body was clamped at an impact (written by the step).
    pub hits: u32,
    /// Sweeps that could not answer (iteration budget, degenerate motion; written by the step).
    pub failures: u32,
    /// Normalized time of impact (`0..=1` of the step) of the last clamp, if any.
    pub last_toi: Option<f32>,
}

impl Default for SweptCcd {
    fn default() -> Self {
        Self {
            skin: 0.005,
            max_iterations: 64,
            hits: 0,
            failures: 0,
            last_toi: None,
        }
    }
}

/// One swept body's pose before the step.
#[derive(Clone, Copy)]
pub(crate) struct SweepStart {
    entity: Entity,
    handle: RapierBodyHandle,
    pose: rapier3d::math::Pose,
    config: SweptCcd,
}

/// What the sweep did to one body this step.
pub(crate) struct SweepOutcome {
    entity: Entity,
    toi: Option<f32>,
    failures: u32,
}

/// The query the step hook reads and writes.
pub(crate) type SweptQuery<'w, 's> = Query<'w, 's, (Entity, &'static RigidBodyHandle, &'static mut SweptCcd)>;

/// Deterministic quaternion conversions for the sweep: the glam originals go through the
/// platform libm (`atan2`, `sin`, `cos`), whose last digit differs between glibc, musl, ucrt
/// and bionic; a grazing sweep could clamp on one target and tunnel on another with
/// `physics-cross-platform-determinism` on. These use [`regia_core`]'s deterministic
/// transcendentals instead (spec 15 § 5). Provenance: the algorithms are the public-domain
/// textbook identities (axis-angle ↔ quaternion, minimal arc); the code is Regia Engine's.
///
/// [`Quat::to_scaled_axis`] with deterministic transcendentals, for a normalized `q` with
/// `w >= 0` (the step rotations this is used on are normalized and sign-flipped above).
fn det_scaled_axis(q: Quat) -> Vec3 {
    let v = Vec3::new(q.x, q.y, q.z);
    let len = v.length();
    if len <= 1.0e-6 {
        // sin(theta) ~ theta: the scaled axis is 2v/w (w ~ 1 for a tiny rotation).
        return v * (2.0 / q.w);
    }
    v * (2.0 * det_atan2(len, q.w) / len)
}

/// [`Quat::from_scaled_axis`] with deterministic transcendentals.
fn det_quat_from_scaled_axis(v: Vec3) -> Quat {
    let angle = v.length();
    if angle <= 1.0e-6 {
        return Quat::from_xyzw(v.x * 0.5, v.y * 0.5, v.z * 0.5, 1.0);
    }
    let scale = det_sin(angle * 0.5) / angle;
    Quat::from_xyzw(v.x * scale, v.y * scale, v.z * scale, det_cos(angle * 0.5))
}

/// [`Quat::from_axis_angle`] with deterministic transcendentals.
fn det_quat_from_axis_angle(axis: Vec3, angle: f32) -> Quat {
    let scale = det_sin(angle * 0.5);
    Quat::from_xyzw(
        axis.x * scale,
        axis.y * scale,
        axis.z * scale,
        det_cos(angle * 0.5),
    )
}

/// [`Quat::from_rotation_arc`] with deterministic transcendentals. Only the 180° branch
/// transcends: the general branch is `+`/`*` and one exact `sqrt` (normalization), which is
/// already bit-identical on every target. Inputs must be unit vectors.
fn det_quat_from_rotation_arc(from: Vec3, to: Vec3) -> Quat {
    const ONE_MINUS_EPS: f32 = 1.0 - 2.0 * f32::EPSILON;
    let dot = from.dot(to);
    if dot > ONE_MINUS_EPS {
        Quat::IDENTITY
    } else if dot < -ONE_MINUS_EPS {
        // 180°: any axis in the from/to plane works; `any_orthonormal_vector` picks one
        // deterministically (plain arithmetic and one exact `sqrt`).
        det_quat_from_axis_angle(from.any_orthonormal_vector(), core::f32::consts::PI)
    } else {
        let c = from.cross(to);
        Quat::from_xyzw(c.x, c.y, c.z, 1.0 + dot).normalize()
    }
}

/// Snapshots the pre-step pose of every `SweptCcd` body, in body-handle (rapier insertion)
/// order. Empty (and free) when no body opted in.
pub(crate) fn capture_starts(physics: &PhysicsWorld, swept: &SweptQuery<'_, '_>) -> Vec<SweepStart> {
    if swept.is_empty() {
        return Vec::new();
    }
    let mut starts: Vec<SweepStart> = swept
        .iter()
        .filter_map(|(entity, handle, config)| {
            let rapier = handle.to_handle();
            let rb = physics.bodies.get(rapier)?;
            rb.is_dynamic().then(|| SweepStart {
                entity,
                handle: rapier,
                pose: *rb.position(),
                config: *config,
            })
        })
        .collect();
    sort_starts(&mut starts);
    starts
}

/// The order `resolve` sweeps bodies in: rapier body-handle raw parts (insertion order, which
/// `sync_added_bodies` already forces to follow `SpawnOrder<RigidBody>`), so the sweep order
/// does not follow entity ids that parallel `Commands::spawn` reserve in thread-timing order
/// (ADR-1347).
fn sort_starts(starts: &mut [SweepStart]) {
    starts.sort_by_key(|s| s.handle.into_raw_parts());
}

/// Writes the outcomes back into the components.
pub(crate) fn record_outcomes(outcomes: Vec<SweepOutcome>, swept: &mut SweptQuery<'_, '_>) {
    for outcome in outcomes {
        // Untouched bodies keep their change tick: only a hit or a failure writes.
        if outcome.toi.is_none() && outcome.failures == 0 {
            continue;
        }
        if let Ok((_, _, mut config)) = swept.get_mut(outcome.entity) {
            config.failures = config.failures.saturating_add(outcome.failures);
            if let Some(t) = outcome.toi {
                config.hits = config.hits.saturating_add(1);
                config.last_toi = Some(t);
            }
        }
    }
}

/// The convex pieces of a rapier shape with their poses in the shape's frame. `local_bounds`
/// (the query region in the shape's frame) selects the triangles of meshes and heightfields;
/// without it those shapes give nothing (a swept body is never a mesh).
fn convex_parts(
    shape: &dyn Shape,
    local_bounds: Option<&Aabb>,
    out: &mut Vec<(ConvexInput, Isometry)>,
    pool: &mut Vec<Vec<Vec3>>,
) {
    let here = Isometry::IDENTITY;
    match shape.as_typed_shape() {
        TypedShape::Ball(ball) => out.push((ConvexInput::sphere(ball.radius), here)),
        TypedShape::Cuboid(cuboid) => {
            out.push((ConvexInput::cuboid(from_rapier_vec3(cuboid.half_extents)), here))
        }
        TypedShape::Capsule(capsule) => {
            let a = from_rapier_vec3(capsule.segment.a);
            let b = from_rapier_vec3(capsule.segment.b);
            let center = (a + b) * 0.5;
            let half = (b - a).length() * 0.5;
            let pose = if half > 1e-9 {
                Isometry::new(
                    det_quat_from_rotation_arc(Vec3::Y, (b - a) / (2.0 * half)),
                    center,
                )
            } else {
                Isometry::from_translation(center)
            };
            out.push((
                ConvexInput::Capsule {
                    half_height: half,
                    radius: capsule.radius,
                    axis: crate::components::ShapeAxis::Y,
                },
                pose,
            ));
        }
        TypedShape::Cylinder(cylinder) => out.push((
            ConvexInput::Cylinder {
                half_height: cylinder.half_height,
                radius: cylinder.radius,
                axis: crate::components::ShapeAxis::Y,
            },
            here,
        )),
        TypedShape::Cone(cone) => out.push((
            ConvexInput::Cone {
                half_height: cone.half_height,
                radius: cone.radius,
                axis: crate::components::ShapeAxis::Y,
            },
            here,
        )),
        TypedShape::ConvexPolyhedron(poly) => {
            let mut points = take_points(pool);
            points.extend(poly.points().iter().map(|p| from_rapier_vec3(*p)));
            out.push((ConvexInput::ConvexHull { points }, here));
        }
        TypedShape::Triangle(tri) => out.push((triangle_input(tri, pool), here)),
        TypedShape::Segment(segment) => out.push((
            ConvexInput::Segment {
                a: from_rapier_vec3(segment.a),
                b: from_rapier_vec3(segment.b),
            },
            here,
        )),
        TypedShape::Compound(compound) => {
            for (pose, child) in compound.shapes() {
                let (t, r) = from_rapier_pose(pose);
                let child_pose = Isometry::new(r, t);
                // The query region in the child's frame: a heightfield child selects its triangles
                // from it as a top-level one does (`shape_to_shared_shape` refuses mesh children).
                let child_bounds = local_bounds.map(|bounds| bounds.transform_by(&pose.inverse()));
                let start = out.len();
                convex_parts(&**child, child_bounds.as_ref(), out, pool);
                for part in &mut out[start..] {
                    part.1 = child_pose * part.1;
                }
            }
        }
        TypedShape::TriMesh(mesh) => {
            if let Some(bounds) = local_bounds {
                for index in mesh.bvh().intersect_aabb(bounds).take(MAX_TRIANGLES_PER_PAIR) {
                    out.push((triangle_input(&mesh.triangle(index), pool), here));
                }
            }
        }
        TypedShape::HeightField(field) => {
            if let Some(bounds) = local_bounds {
                let mut taken = 0usize;
                field.map_elements_in_local_aabb(bounds, &mut |_, tri| {
                    if taken < MAX_TRIANGLES_PER_PAIR {
                        out.push((triangle_input(tri, pool), here));
                        taken += 1;
                    }
                });
            }
        }
        _ => {}
    }
}

/// A triangle as a three-point hull. The point buffer comes from `pool` when one is spare.
fn triangle_input(tri: &rapier3d::parry::shape::Triangle, pool: &mut Vec<Vec<Vec3>>) -> ConvexInput {
    ccd_add(&CCD_TRIANGLES);
    let mut points = take_points(pool);
    points.extend([
        from_rapier_vec3(tri.a),
        from_rapier_vec3(tri.b),
        from_rapier_vec3(tri.c),
    ]);
    ConvexInput::ConvexHull { points }
}

/// `pose` as an [`Isometry`].
fn iso(pose: &rapier3d::math::Pose) -> Isometry {
    let (t, r) = from_rapier_pose(pose);
    Isometry::new(r, t)
}

/// The world bounds `[min, max]` brought into the frame of `pose` (as an AABB there).
fn local_aabb(pose: Isometry, min: Vec3, max: Vec3) -> Aabb {
    let inv = pose.inverse();
    let mut lo = Vec3::splat(f32::INFINITY);
    let mut hi = Vec3::splat(f32::NEG_INFINITY);
    for corner in 0..8u32 {
        let p = Vec3::new(
            if corner & 1 == 0 { min.x } else { max.x },
            if corner & 2 == 0 { min.y } else { max.y },
            if corner & 4 == 0 { min.z } else { max.z },
        );
        let q = inv.transform_point(p);
        lo = lo.min(q);
        hi = hi.max(q);
    }
    Aabb::new(to_rapier_vec3(lo), to_rapier_vec3(hi))
}

/// The earliest impact found for one body.
struct Impact {
    t: f32,
    normal: Vec3,
    point: Vec3,
    restitution: f32,
}

/// One map of the captured bodies' step motions, keyed by rapier body-handle raw parts.
fn fill_motion_map(
    physics: &PhysicsWorld,
    starts: &[SweepStart],
    dt: f32,
    motions: &mut BTreeMap<(u32, u32), (RigidMotion, Isometry)>,
) {
    motions.clear();
    ccd_add(&CCD_MOTION_MAPS);
    for start in starts {
        let Some(rb) = physics.bodies.get(start.handle) else {
            continue;
        };
        if rb.is_fixed() {
            continue;
        }
        let motion = step_motion(iso(&start.pose), rb, dt);
        if motion.linear.is_finite() && motion.angular.is_finite() {
            motions.insert(start.handle.into_raw_parts(), (motion, iso(rb.position())));
        }
    }
}

/// Sweeps every captured body over the step just taken and clamps the ones that hit
/// something first. `dt` is the step in seconds; `prediction` the solver's prediction
/// distance. Deterministic: bodies in body-handle (insertion) order, candidates in
/// collider-handle order, the earliest impact wins and ties keep the first found.
/// The captured bodies' motions are collected once, then each candidate looks that map up.
pub(crate) fn resolve(
    physics: &mut PhysicsWorld,
    starts: &[SweepStart],
    dt: f32,
    prediction: f32,
) -> Vec<SweepOutcome> {
    let _span = tracing::debug_span!("swept_ccd", bodies = starts.len()).entered();
    SWEEP_SCRATCH.with(|slot| {
        let scratch = &mut *slot.borrow_mut();
        fill_motion_map(physics, starts, dt, &mut scratch.motions);
        let mut outcomes = Vec::with_capacity(starts.len());
        for start in starts {
            let (impact, failures, motion) = sweep_body(physics, start, dt, prediction, scratch);
            let mut toi = None;
            if let (Some(impact), Some(motion)) = (impact, motion) {
                apply_impact(physics, start.handle, &motion, &impact);
                toi = Some(impact.t);
                // The next body reads this one from the map. Refresh it: the clamp just
                // rewrote the pose and the velocity, which the old per-candidate scan saw live.
                if let Some(rb) = physics.bodies.get(start.handle) {
                    let updated = step_motion(iso(&start.pose), rb, dt);
                    if updated.linear.is_finite() && updated.angular.is_finite() {
                        scratch
                            .motions
                            .insert(start.handle.into_raw_parts(), (updated, iso(rb.position())));
                    }
                }
            }
            outcomes.push(SweepOutcome {
                entity: start.entity,
                toi,
                failures,
            });
        }
        if outcomes.iter().any(|o| o.toi.is_some()) {
            let PhysicsWorld {
                bodies, colliders, ..
            } = physics;
            bodies.propagate_modified_body_positions_to_colliders(colliders);
        }
        outcomes
    })
}

/// The motion of body `rb` over the step just taken, from its pre-step pose `pose0` to its
/// current pose, about its centre of mass: the endpoint rotation, or the step's angular
/// velocity when that turns more than half a turn (the endpoints cannot tell those apart).
fn step_motion(pose0: Isometry, rb: &rapier3d::dynamics::RigidBody, dt: f32) -> RigidMotion {
    let pose1 = iso(rb.position());
    let local_com = from_rapier_vec3(rb.local_center_of_mass());
    let mut turn = (pose1.rotation * pose0.rotation.inverse()).normalize();
    if turn.w < 0.0 {
        turn = -turn;
    }
    let spin = from_rapier_vec3(rb.angvel()) * dt;
    let angular = if spin.is_finite() && spin.length() > core::f32::consts::PI {
        spin
    } else {
        det_scaled_axis(turn)
    };
    RigidMotion {
        start: pose0,
        local_center: local_com,
        linear: pose1.transform_point(local_com) - pose0.transform_point(local_com),
        angular,
    }
}

/// The step motion of the body owning another collider, so two fast bodies meeting head-on
/// are swept against each other's paths, not against each other's end poses. A `SweptCcd`
/// body uses its captured pre-step pose; any other moving dynamic or kinematic body is traced
/// back from its current pose and velocity. `None` for fixed and resting bodies (swept as
/// stationary at their current pose).
fn other_body_motion(
    physics: &PhysicsWorld,
    motions: &BTreeMap<(u32, u32), (RigidMotion, Isometry)>,
    handle: Option<RapierBodyHandle>,
    dt: f32,
) -> Option<(RigidMotion, Isometry)> {
    ccd_add(&CCD_MOTION_PROBES);
    let handle = handle?;
    let rb = physics.bodies.get(handle)?;
    if rb.is_fixed() {
        return None;
    }
    let end = iso(rb.position());
    if let Some(found) = motions.get(&handle.into_raw_parts()) {
        return Some(*found);
    }
    let linear = from_rapier_vec3(rb.linvel()) * dt;
    let angular = from_rapier_vec3(rb.angvel()) * dt;
    if !linear.is_finite() || !angular.is_finite() || (linear == Vec3::ZERO && angular == Vec3::ZERO) {
        return None;
    }
    let local_com = from_rapier_vec3(rb.local_center_of_mass());
    let com1 = end.transform_point(local_com);
    let rotation0 = (det_quat_from_scaled_axis(-angular) * end.rotation).normalize();
    let com0 = com1 - linear;
    let start = Isometry::new(rotation0, com0 - rotation0 * local_com);
    Some((
        RigidMotion {
            start,
            local_center: local_com,
            linear,
            angular,
        },
        end,
    ))
}

/// The body's step motion and its earliest impact, plus the number of failed sweeps.
fn sweep_body(
    physics: &PhysicsWorld,
    start: &SweepStart,
    dt: f32,
    prediction: f32,
    scratch: &mut SweepScratch,
) -> (Option<Impact>, u32, Option<RigidMotion>) {
    let Some(rb) = physics.bodies.get(start.handle) else {
        return (None, 0, None);
    };
    if !rb.is_dynamic() || *rb.position() == start.pose {
        return (None, 0, None);
    }
    let pose0 = iso(&start.pose);
    let local_com = from_rapier_vec3(rb.local_center_of_mass());
    let motion = step_motion(pose0, rb, dt);
    let com0 = pose0.transform_point(local_com);
    let com1 = motion.pose_at(1.0).transform_point(local_com);
    if !motion.linear.is_finite() || !motion.angular.is_finite() {
        return (None, 1, None);
    }
    let skin = start.config.skin.clamp(0.0, 0.5 * prediction);
    let options = ToiOptions {
        target_gap: skin,
        tolerance: (0.25 * prediction).max(1e-5),
        max_iterations: start.config.max_iterations.max(1),
    };
    let SweepScratch {
        body_colliders,
        candidates,
        parts_a,
        parts_b,
        point_pool,
        motions,
    } = scratch;
    body_colliders.clear();
    body_colliders.extend(rb.colliders().iter().copied());
    let mut best: Option<Impact> = None;
    let mut failures = 0u32;
    for collider_handle in body_colliders.iter().copied() {
        let Some(collider) = physics.colliders.get(collider_handle) else {
            continue;
        };
        if collider.is_sensor() || !collider.is_enabled() {
            continue;
        }
        let relative = collider
            .position_wrt_parent()
            .map(iso)
            .unwrap_or(Isometry::IDENTITY);
        let groups = collider.collision_groups();
        let solver_groups = collider.solver_groups();
        let restitution_a = collider.restitution();
        recycle_parts(parts_a, point_pool);
        convex_parts(collider.shape(), None, parts_a, point_pool);
        // Owned for this collider so the triangle pool can refill `parts_b` while A is read.
        let mut owned_a = std::mem::take(parts_a);
        for (shape_a, part_pose) in &owned_a {
            let shape_local = relative * *part_pose;
            let motion_a = RigidMotion {
                start: pose0 * shape_local,
                local_center: shape_local.inverse().transform_point(local_com),
                ..motion
            };
            let reach = radius_about(shape_a, motion_a.local_center) + skin + prediction;
            if !reach.is_finite() {
                failures += 1;
                continue;
            }
            let min = com0.min(com1) - Vec3::splat(reach);
            let max = com0.max(com1) + Vec3::splat(reach);
            let filter = QueryFilter {
                flags: QueryFilterFlags::EXCLUDE_SENSORS,
                groups: Some(groups),
                exclude_collider: None,
                exclude_rigid_body: Some(start.handle),
                predicate: None,
            };
            candidates.clear();
            candidates.extend(
                physics
                    .query_pipeline(filter)
                    .intersect_aabb_conservative(Aabb::new(to_rapier_vec3(min), to_rapier_vec3(max)))
                    .map(|(handle, _)| handle),
            );
            candidates.sort_by_key(|h| h.into_raw_parts());
            for other_handle in candidates.iter().copied() {
                let (other_pose, restitution, other_parent) = {
                    let Some(other) = physics.colliders.get(other_handle) else {
                        continue;
                    };
                    if !groups.test(other.collision_groups()) || !solver_groups.test(other.solver_groups()) {
                        continue;
                    }
                    (
                        iso(other.position()),
                        0.5 * (restitution_a + other.restitution()),
                        other.parent(),
                    )
                };
                let other_motion = other_body_motion(physics, motions, other_parent, dt);
                let Some(other) = physics.colliders.get(other_handle) else {
                    continue;
                };
                recycle_parts(parts_b, point_pool);
                let bounds = local_aabb(other_pose, min, max);
                convex_parts(other.shape(), Some(&bounds), parts_b, point_pool);
                for (shape_b, pose_b_local) in parts_b.iter() {
                    let pose_b = other_pose * *pose_b_local;
                    // B along its own step motion when its body moves, else at rest.
                    let motion_b = match other_motion {
                        Some((body, body_end)) => {
                            let relative_b = body_end.inverse() * pose_b;
                            RigidMotion {
                                start: body.start * relative_b,
                                local_center: relative_b.inverse().transform_point(body.local_center),
                                ..body
                            }
                        }
                        None => RigidMotion::stationary(pose_b),
                    };
                    match convex_distance(shape_a, motion_a.start, shape_b, motion_b.start) {
                        Ok(d) if !d.overlapped && d.distance > prediction => {}
                        // Already in the solver's contact range at the start: the solver holds
                        // that contact. Only a part of the body swinging deep through the
                        // contact plane (the far end of a bar pivoting on its tip) is ours.
                        // (A moving B in contact at the start stays with the solver.)
                        Ok(d) if !d.overlapped && other_motion.is_none() => {
                            let crossing = plane_crossing(
                                shape_a,
                                &motion_a,
                                shape_b,
                                &motion_b,
                                d.normal,
                                prediction,
                                options.max_iterations,
                            );
                            if let Some((t, point)) = crossing
                                && best.as_ref().is_none_or(|b| t < b.t)
                            {
                                best = Some(Impact {
                                    t,
                                    normal: d.normal,
                                    point,
                                    restitution,
                                });
                            }
                            continue;
                        }
                        Ok(_) => continue,
                        Err(_) => {
                            failures += 1;
                            continue;
                        }
                    }
                    match time_of_impact_motion(shape_a, &motion_a, shape_b, &motion_b, options) {
                        ToiResult::Hit { t, normal, point } => {
                            if best.as_ref().is_none_or(|b| t < b.t) {
                                best = Some(Impact {
                                    t,
                                    normal,
                                    point,
                                    restitution,
                                });
                            }
                        }
                        ToiResult::Separated | ToiResult::OverlappedAtStart => {}
                        ToiResult::Failed(reason) => {
                            tracing::debug!(target: "regia_physics", reason = reason.code(), "swept CCD sweep failed");
                            failures += 1;
                        }
                    }
                }
            }
        }
        recycle_parts(&mut owned_a, point_pool);
        *parts_a = owned_a;
    }
    recycle_parts(parts_a, point_pool);
    recycle_parts(parts_b, point_pool);
    (best, failures, Some(motion))
}

/// For a pair already in contact at the start, with `n` the start normal (A toward B): the
/// first time some point of A passes `depth` beyond B's support plane along `n`, while A and B
/// really overlap there, with that point. `None` when A never crosses that deep (resting and
/// sliding contacts, whose penetration stays within the solver's slop) or crosses only beside
/// B. The separation along the fixed axis `n`, `n · s_B(−n) − n · s_A(n)`, shrinks no faster than
/// `max(0, n · v_A) + |n × ω_A| R_A`, so conservative advancement finds the first crossing.
fn plane_crossing(
    a: &ConvexInput,
    motion_a: &RigidMotion,
    b: &ConvexInput,
    motion_b: &RigidMotion,
    n: Vec3,
    depth: f32,
    max_iterations: u32,
) -> Option<(f32, Vec3)> {
    let pose_b = motion_b.start;
    let plane = n.dot(b.support_point(pose_b, -n));
    let separation = |t: f32| plane - n.dot(a.support_point(motion_a.pose_at(t), n));
    let end = separation(1.0);
    if end.is_nan() || end >= -depth {
        return None;
    }
    let radius = radius_about(a, motion_a.local_center);
    let closing = n.dot(motion_a.linear).max(0.0) + n.cross(motion_a.angular).length() * radius;
    if !closing.is_finite() || closing <= 1e-12 {
        return None;
    }
    let mut t = 0.0_f32;
    for _ in 0..max_iterations {
        let s = separation(t);
        if s <= -depth {
            let pose_a = motion_a.pose_at(t);
            let overlapping =
                convex_distance(a, pose_a, b, pose_b).is_ok_and(|d| d.overlapped || d.distance <= 1e-4);
            return overlapping.then(|| (t, a.support_point(pose_a, n)));
        }
        let dt = (s + depth) / closing;
        let next = (t + dt.max(1e-6)).min(1.0);
        if next <= t {
            return None;
        }
        t = next;
    }
    None
}

/// Puts the body at the impact pose and removes its closing velocity at the impact point.
fn apply_impact(physics: &mut PhysicsWorld, handle: RapierBodyHandle, motion: &RigidMotion, impact: &Impact) {
    let Some(rb) = physics.bodies.get_mut(handle) else {
        return;
    };
    let pose_t = motion.pose_at(impact.t);
    let rapier_pose = to_rapier_pose(pose_t.translation, pose_t.rotation);
    let mut mprops = rb.mass_properties().clone();
    mprops.update_world_mass_properties(RigidBodyType::Dynamic, &rapier_pose);
    rb.set_position(rapier_pose, true);
    let com = pose_t.transform_point(motion.local_center);
    let r = impact.point - com;
    let n = impact.normal;
    let linvel = from_rapier_vec3(rb.linvel());
    let angvel = from_rapier_vec3(rb.angvel());
    let closing = (linvel + angvel.cross(r)).dot(n);
    if closing.is_nan() || closing <= 0.0 || !closing.is_finite() {
        return;
    }
    let inv_mass = from_rapier_vec3(mprops.effective_inv_mass);
    let rn = r.cross(n);
    let inv_inertia_rn = from_rapier_vec3(mprops.effective_world_inv_inertia * to_rapier_vec3(rn));
    let k = n.dot(inv_mass * n) + rn.dot(inv_inertia_rn);
    if k.is_nan() || k <= 1e-12 || !k.is_finite() {
        return;
    }
    let e = if impact.restitution.is_finite() {
        impact.restitution.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let j = (1.0 + e) * closing / k;
    let new_linvel = linvel - inv_mass * n * j;
    let new_angvel = angvel - inv_inertia_rn * j;
    if new_linvel.is_finite() && new_angvel.is_finite() {
        rb.set_linvel(to_rapier_vec3(new_linvel), true);
        rb.set_angvel(to_rapier_vec3(new_angvel), true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::to_rapier_pose;

    /// Audit g-physics2 finding 10 (low): a heightfield child of a compound gave swept CCD no
    /// triangles (the compound arm dropped the query bounds), so a `SweptCcd` body could tunnel
    /// through it. The child now gives the triangles a top-level heightfield gives for the same
    /// region, posed by the child's offset.
    #[test]
    fn a_heightfield_child_of_a_compound_gives_its_triangles_to_the_sweep() {
        use crate::components::ColliderShape;
        let field = ColliderShape::HeightField {
            nx: 5,
            nz: 5,
            heights: vec![0.0; 25],
            scale: Vec3::new(8.0, 1.0, 8.0),
        };
        let offset = Vec3::new(10.0, 2.0, -3.0);
        let compound = ColliderShape::Compound {
            shapes: vec![(regia_scene::Transform::from_translation(offset), field.clone())],
        };
        let half = Vec3::new(1.0, 0.5, 1.0);
        let mut nested = Vec::new();
        let mut pool = Vec::new();
        let compound = crate::step::shape_to_shared_shape(&compound).expect("compound");
        let bounds = Aabb::new(to_rapier_vec3(offset - half), to_rapier_vec3(offset + half));
        convex_parts(&*compound, Some(&bounds), &mut nested, &mut pool);
        let mut direct = Vec::new();
        let field = crate::step::shape_to_shared_shape(&field).expect("heightfield");
        convex_parts(
            &*field,
            Some(&Aabb::new(to_rapier_vec3(-half), to_rapier_vec3(half))),
            &mut direct,
            &mut pool,
        );
        assert!(!direct.is_empty(), "the region covers some of the field");
        assert_eq!(nested.len(), direct.len(), "the child gives the same triangles");
        for (_, pose) in &nested {
            assert!((pose.translation - offset).length() < 1e-5, "{pose:?}");
        }
    }

    /// The deterministic `to_scaled_axis`: axis `v/|v|` times `2 atan2(|v|, w)`, with
    /// [`det_atan2`] instead of the platform libm (spec 15 § 5).
    fn expected_scaled_axis(q: Quat) -> Vec3 {
        let v = Vec3::new(q.x, q.y, q.z);
        let len = v.length();
        v * (2.0 * det_atan2(len, q.w) / len)
    }

    /// The deterministic `from_scaled_axis` (spec 15 § 5).
    fn expected_quat_from_scaled_axis(v: Vec3) -> Quat {
        let angle = v.length();
        let scale = det_sin(angle * 0.5) / angle;
        Quat::from_xyzw(v.x * scale, v.y * scale, v.z * scale, det_cos(angle * 0.5))
    }

    /// The deterministic `from_axis_angle` (spec 15 § 5).
    fn expected_quat_from_axis_angle(axis: Vec3, angle: f32) -> Quat {
        let scale = det_sin(angle * 0.5);
        Quat::from_xyzw(
            axis.x * scale,
            axis.y * scale,
            axis.z * scale,
            det_cos(angle * 0.5),
        )
    }

    /// Bit-exact quaternion comparison (glam has no `Quat::to_bits`).
    fn quat_bits(q: Quat) -> [u32; 4] {
        [q.x.to_bits(), q.y.to_bits(), q.z.to_bits(), q.w.to_bits()]
    }

    fn start(entity: Entity, index: usize, generation: u32, x: f32) -> SweepStart {
        SweepStart {
            entity,
            handle: RapierBodyHandle::from_raw_parts(index as u32, generation),
            pose: to_rapier_pose(Vec3::new(x, 0.0, 0.0), Quat::IDENTITY),
            config: SweptCcd::default(),
        }
    }

    /// `resolve` sweeps `starts` in order and clamps each body before the next is swept, so
    /// the order must follow rapier body handles (insertion order), not entity ids: parallel
    /// `Commands::spawn` reserves ids in thread-timing order (ADR-1347), and two swept bodies
    /// aimed at each other give different surviving poses when the sweep order flips.
    #[test]
    fn starts_are_ordered_by_body_handle_not_entity_id() {
        // Two entities whose ids are the reverse of their body handles (one thread reserved
        // the second system's id first, the other applied first).
        let low_entity = Entity::from_raw_u32(7).expect("an index");
        let high_entity = Entity::from_raw_u32(9).expect("an index");
        let mut starts = vec![start(high_entity, 0, 0, 0.0), start(low_entity, 1, 0, 2.0)];
        sort_starts(&mut starts);
        assert_eq!(
            starts.iter().map(|s| s.entity).collect::<Vec<_>>(),
            vec![high_entity, low_entity],
            "the lower body handle sweeps first, whatever the entity ids"
        );
    }

    /// A step rotation below half a turn goes through `det_scaled_axis`, not glam's
    /// `to_scaled_axis` (platform `atan2`): the angular displacement bits must equal the
    /// deterministic series, on every target.
    #[test]
    fn step_motion_scales_sub_half_turn_rotations_deterministically() {
        let pose0 = Isometry::new(Quat::IDENTITY, Vec3::ZERO);
        let turn = Quat::from_rotation_z(0.6);
        let rb = rapier3d::dynamics::RigidBodyBuilder::dynamic()
            .pose(to_rapier_pose(Vec3::new(0.5, 0.25, 0.0), turn))
            .angvel(to_rapier_vec3(Vec3::new(0.0, 0.0, 30.0)))
            .build();
        // 30 rad/s at a 60 Hz step is 0.5 rad (< π): the endpoint branch, `det_scaled_axis`.
        let motion = step_motion(pose0, &rb, 1.0 / 60.0);
        let expected = expected_scaled_axis(turn);
        assert_eq!(motion.angular.x.to_bits(), expected.x.to_bits());
        assert_eq!(motion.angular.y.to_bits(), expected.y.to_bits());
        assert_eq!(motion.angular.z.to_bits(), expected.z.to_bits());
    }

    /// A spin above half a turn per step keeps the velocity vector (the endpoints cannot
    /// tell those apart); that branch is unchanged and still not libm-dependent.
    #[test]
    fn step_motion_keeps_the_velocity_vector_for_fast_spins() {
        let pose0 = Isometry::new(Quat::IDENTITY, Vec3::ZERO);
        let rb = rapier3d::dynamics::RigidBodyBuilder::dynamic()
            .pose(to_rapier_pose(Vec3::ZERO, Quat::from_rotation_z(0.1)))
            .angvel(to_rapier_vec3(Vec3::new(0.0, 0.0, 400.0)))
            .build();
        let motion = step_motion(pose0, &rb, 1.0 / 60.0);
        assert_eq!(motion.angular.z.to_bits(), (400.0f32 * (1.0 / 60.0)).to_bits());
    }

    /// The start rotation a moving body's step motion is traced back to is rebuilt with
    /// `det_quat_from_scaled_axis`, not glam's `from_scaled_axis` (platform `sin`/`cos`).
    #[test]
    fn other_body_motion_rebuilds_the_start_rotation_deterministically() {
        let mut physics = PhysicsWorld::default();
        let end_rotation = Quat::from_rotation_y(0.35);
        let end = to_rapier_pose(Vec3::new(1.0, 2.0, 0.0), end_rotation);
        let handle = physics
            .bodies
            .insert(rapier3d::dynamics::RigidBodyBuilder::dynamic().pose(end).build());
        let linear = Vec3::new(0.5, -0.25, 0.0);
        let angular = Vec3::new(0.0, 0.2, 0.1);
        let dt = 1.0f32 / 60.0;
        {
            let rb = physics.bodies.get_mut(handle).expect("body");
            rb.set_linvel(to_rapier_vec3(linear / dt), false);
            rb.set_angvel(to_rapier_vec3(angular / dt), false);
        }
        // The step reads the velocity back and scales by `dt`: mirror that round trip so the
        // expectation is bit-exact.
        let angular_back = (angular / dt) * dt;
        let motions = BTreeMap::new();
        let Some((motion, _)) = other_body_motion(&physics, &motions, Some(handle), dt) else {
            panic!("a moving dynamic body has a step motion");
        };
        let expected = (expected_quat_from_scaled_axis(-angular_back) * end_rotation).normalize();
        assert_eq!(quat_bits(motion.start.rotation), quat_bits(expected));
    }

    /// A capsule whose segment is the negative Y axis takes the 180° branch of the arc
    /// rotation; its quaternion must come from the deterministic transcendentals (the
    /// platform `sin`/`cos` of π/2 differ from the series in the last digits).
    #[test]
    fn convex_parts_orients_a_reverse_capsule_deterministically() {
        let capsule = rapier3d::parry::shape::Capsule::new(
            to_rapier_vec3(Vec3::new(0.0, 1.0, 0.0)),
            to_rapier_vec3(Vec3::new(0.0, -1.0, 0.0)),
            0.25,
        );
        let mut parts = Vec::new();
        let mut pool = Vec::new();
        convex_parts(&capsule, None, &mut parts, &mut pool);
        assert_eq!(parts.len(), 1);
        // 180° branch: `any_orthonormal_vector` is plain arithmetic and one exact `sqrt`;
        // only the `sin`/`cos` of the half angle must be the deterministic series.
        let axis = Vec3::Y.any_orthonormal_vector();
        let expected = expected_quat_from_axis_angle(axis, core::f32::consts::PI);
        assert_eq!(quat_bits(parts[0].1.rotation), quat_bits(expected));
    }

    #[test]
    fn convex_parts_maps_a_capsule_onto_its_segment() {
        let capsule = rapier3d::parry::shape::Capsule::new(
            to_rapier_vec3(Vec3::new(-1.0, 0.0, 0.0)),
            to_rapier_vec3(Vec3::new(1.0, 0.0, 0.0)),
            0.25,
        );
        let mut parts = Vec::new();
        let mut pool = Vec::new();
        convex_parts(&capsule, None, &mut parts, &mut pool);
        assert_eq!(parts.len(), 1);
        let (shape, pose) = &parts[0];
        let ConvexInput::Capsule {
            half_height, radius, ..
        } = shape
        else {
            panic!("expected a capsule, got {shape:?}");
        };
        assert!((half_height - 1.0).abs() < 1e-6 && (radius - 0.25).abs() < 1e-6);
        // The local Y axis is carried onto the segment direction.
        assert!((pose.rotation * Vec3::Y).abs().dot(Vec3::X) > 0.9999);
    }

    #[test]
    fn local_aabb_covers_the_rotated_box() {
        let pose = Isometry::new(
            Quat::from_rotation_z(core::f32::consts::FRAC_PI_4),
            Vec3::new(1.0, 0.0, 0.0),
        );
        let aabb = local_aabb(pose, Vec3::new(0.0, -1.0, -1.0), Vec3::new(2.0, 1.0, 1.0));
        let lo = from_rapier_vec3(aabb.mins);
        let hi = from_rapier_vec3(aabb.maxs);
        // The world box is 2 x 2 centred on the pose origin, so locally it spans +-sqrt(2).
        assert!(
            (hi.x - 2f32.sqrt()).abs() < 1e-5 && (lo.x + 2f32.sqrt()).abs() < 1e-5,
            "{lo} {hi}"
        );
    }

    /// Pose bits `apply_impact` writes for the impact below, pinned (ADR-4773, audit G
    /// finding 2): the written pose is `motion.pose_at(impact.t)` and must be bit-identical
    /// on every host.
    const IMPACT_POSE_BITS: [u32; 7] = [
        3_203_101_549,
        1_017_370_378,
        0,
        0,
        1_012_608_800,
        0,
        1_065_351_715,
    ];

    #[test]
    fn apply_impact_writes_deterministic_pose_bits() {
        use rapier3d::dynamics::RigidBodyBuilder;
        use rapier3d::geometry::ColliderBuilder;
        let mut physics = PhysicsWorld::default();
        let body = physics.bodies.insert(
            RigidBodyBuilder::dynamic()
                .translation(to_rapier_vec3(Vec3::new(-0.5, 0.02, 0.0)))
                .linvel(to_rapier_vec3(Vec3::new(3.0, 0.0, 0.0)))
                .angvel(to_rapier_vec3(Vec3::new(0.0, 2.0, 0.0)))
                .build(),
        );
        let PhysicsWorld {
            bodies, colliders, ..
        } = &mut physics;
        colliders.insert_with_parent(ColliderBuilder::cuboid(0.1, 0.02, 0.02).build(), body, bodies);
        let motion = RigidMotion {
            start: Isometry::new(Quat::IDENTITY, Vec3::new(-0.5, 0.02, 0.0)),
            local_center: Vec3::ZERO,
            linear: Vec3::new(3.0 * (1.0 / 60.0), 0.0, 0.0),
            angular: Vec3::new(0.0, 2.0 * (1.0 / 60.0), 0.0),
        };
        // 0.8027: the written pose's angle is one ulp apart between platform libm and the
        // deterministic kernel (scanned), so the pin binds on every host.
        let impact = Impact {
            t: 0.8027,
            // The impact convention: `closing = velocity · normal` must be positive, so the
            // normal runs along the motion (the code writes the pose either way).
            normal: Vec3::X,
            point: Vec3::new(-0.2, 0.02, 0.0),
            restitution: 0.0,
        };
        apply_impact(&mut physics, body, &motion, &impact);
        let (translation, rotation) = from_rapier_pose(physics.bodies[body].position());
        assert_eq!(
            [
                translation.x.to_bits(),
                translation.y.to_bits(),
                translation.z.to_bits(),
                rotation.x.to_bits(),
                rotation.y.to_bits(),
                rotation.z.to_bits(),
                rotation.w.to_bits(),
            ],
            IMPACT_POSE_BITS,
            "the clamped body lands on the deterministic pose bits"
        );
    }

    /// A sphere whose swept bounds cover a fine heightfield must reuse triangle point
    /// buffers after the first sweep. The second sweep still visits the triangles.
    #[test]
    fn swept_triangles_reuse_point_buffers() {
        use rapier3d::dynamics::RigidBodyBuilder;
        use rapier3d::geometry::ColliderBuilder;

        const N: usize = 33;
        let mut physics = PhysicsWorld::default();
        let ground = physics.bodies.insert(RigidBodyBuilder::fixed().build());
        let ball = physics.bodies.insert(
            RigidBodyBuilder::dynamic()
                .translation(to_rapier_vec3(Vec3::new(0.0, 0.2, 0.0)))
                .linvel(to_rapier_vec3(Vec3::new(2.0, 0.0, 0.0)))
                .gravity_scale(0.0)
                .can_sleep(false)
                .build(),
        );
        let heights = rapier3d::geometry::Array2::new(N, N, vec![0.0f32; N * N]);
        let PhysicsWorld {
            bodies, colliders, ..
        } = &mut physics;
        colliders.insert_with_parent(
            ColliderBuilder::heightfield(heights, to_rapier_vec3(Vec3::new(8.0, 1.0, 8.0))).build(),
            ground,
            bodies,
        );
        colliders.insert_with_parent(ColliderBuilder::ball(0.5).build(), ball, bodies);
        let start_pose = *physics.bodies[ball].position();
        step_world(&mut physics);
        let moved = *physics.bodies[ball].position();
        assert_ne!(moved, start_pose, "the ball has to move before the sweep");
        let start = SweepStart {
            entity: Entity::PLACEHOLDER,
            handle: ball,
            pose: start_pose,
            config: SweptCcd::default(),
        };
        let dt = 1.0 / 60.0;
        // First sweep fills whatever buffer the implementation keeps. The next one is measured.
        let _ = resolve(&mut physics, &[start], dt, 0.02);
        {
            let PhysicsWorld {
                bodies, colliders, ..
            } = &mut physics;
            bodies.get_mut(ball).expect("ball").set_position(moved, true);
            bodies.propagate_modified_body_positions_to_colliders(colliders);
        }
        let fresh_before = ccd_read(&CCD_FRESH_POINT_VECS);
        let tris_before = ccd_read(&CCD_TRIANGLES);
        let maps_before = ccd_read(&CCD_MOTION_MAPS);
        let probes_before = ccd_read(&CCD_MOTION_PROBES);
        let _ = resolve(&mut physics, &[start], dt, 0.02);
        let fresh = ccd_read(&CCD_FRESH_POINT_VECS) - fresh_before;
        let tris = ccd_read(&CCD_TRIANGLES) - tris_before;
        let maps = ccd_read(&CCD_MOTION_MAPS) - maps_before;
        let probes = ccd_read(&CCD_MOTION_PROBES) - probes_before;
        assert!(
            tris > 8,
            "the warm sweep saw {tris} triangles; the sphere missed the heightfield"
        );
        assert_eq!(
            fresh, 0,
            "a warm sweep allocated {fresh} triangle point buffers for {tris} triangles"
        );
        assert_eq!(maps, 1, "one resolve builds the motion map once, built {maps}");
        assert!(
            probes >= 1,
            "the warm sweep looked up {probes} other-body motions"
        );
    }

    fn step_world(physics: &mut PhysicsWorld) {
        use rapier3d::dynamics::IntegrationParameters;
        let integration = IntegrationParameters {
            dt: 1.0 / 60.0,
            ..Default::default()
        };
        let PhysicsWorld {
            pipeline,
            islands,
            broad_phase,
            narrow_phase,
            bodies,
            colliders,
            impulse_joints,
            multibody_joints,
            soft_bodies,
            ccd_solver,
            ..
        } = physics;
        pipeline.step(
            to_rapier_vec3(Vec3::ZERO),
            &integration,
            islands,
            broad_phase,
            narrow_phase,
            bodies,
            colliders,
            impulse_joints,
            multibody_joints,
            soft_bodies,
            ccd_solver,
            &(),
            &(),
        );
    }
}
