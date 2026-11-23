// SPDX-License-Identifier: MPL-2.0
//! Joints (specs/15 § 2 `Joint`): creating rapier impulse joints from the `Joint`
//! component, removing them, and `break_force` checking after each step.
//!
//! Break force uses rapier's per-joint impulse writeback: `ImpulseJoint::impulses`
//! (locked-axes dofs, linear first) plus the linear limit impulses of `GenericJoint`'s
//! `limits`. Rapier solves `solver_iterations` substeps per step and writes back the last
//! substep's impulses, so the impulse is converted to force by dividing by the substep dt
//! (the same time base the torque path uses). The sum-of-magnitudes is an upper bound when
//! several constraints are active at once; for a single constraint (e.g. a rope) it is exact.
//!
//! Provenance: rapier3d public API (Apache-2.0) — `JointAxesMask` dof order
//! (`LIN_X..LIN_Z` then `ANG_X..ANG_Z`), `ImpulseJoint::impulses`, typed joint builders.

use regia_app::FixedTime;
use regia_core::math::{Mat3, Quat, Vec3};
use regia_ecs::prelude::*;
use tracing::{debug_span, instrument};

use crate::components::{
    Joint, JointBreakTorque, JointHandle, JointKind, JointMotorConfig, JointNoContacts, RigidBodyHandle,
};
use crate::convert::{from_rapier_rotation, to_rapier_rotation, to_rapier_vec3};
use crate::events::RawEvent;
use crate::world::PhysicsWorld;

/// Query for every joint (`sync_joints` re-creates missing joints, not only changed ones).
type JointsQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        Ref<'static, Joint>,
        Option<&'static RigidBodyHandle>,
        Option<&'static JointHandle>,
        Has<JointNoContacts>,
    ),
>;

/// Normalizes a joint axis; rapier needs a unit axis (glam asserts on it in debug builds).
/// A zero or non-finite axis falls back to +X with a warning.
fn unit_axis(axis: Vec3) -> Vec3 {
    match axis.try_normalize() {
        Some(unit) => unit,
        None => {
            tracing::warn!(target: "regia_physics", "joint axis {axis:?} is zero or not finite; using +X");
            Vec3::X
        }
    }
}

/// The joint frame of a [`JointKind::SwingTwist`]: X along the twist axis, Y along the bend
/// axis made perpendicular to it, Z = X × Y. A bend axis parallel to the twist axis (or zero)
/// falls back to any perpendicular.
fn swing_twist_basis(twist_axis: Vec3, bend_axis: Vec3) -> Quat {
    let x = unit_axis(twist_axis);
    let y = (bend_axis - x * bend_axis.dot(x))
        .try_normalize()
        .unwrap_or_else(|| x.any_orthonormal_vector());
    let z = x.cross(y);
    Quat::from_mat3(&Mat3::from_cols(x, y, z)).normalize()
}

/// Builds the rapier joint data for a [`JointKind`] with anchors applied. `relative` is the
/// bodies' current relative rotation (`connected⁻¹ · this`), when both exist.
/// `q` scaled to unit length, or `None` when its length is zero or not finite (glam 0.33's `Quat`
/// has no `try_normalize`). It is `Quat::normalize` itself, checked afterwards: `Vec4::try_normalize`
/// multiplies by the reciprocal length and rounds differently, which moved the determinism pins.
fn unit_quat(q: Quat) -> Option<Quat> {
    let unit = q.normalize();
    unit.is_finite().then_some(unit)
}

fn build_joint_data(joint: &Joint, relative: Option<Quat>) -> (rapier3d::dynamics::GenericJoint, u64) {
    // Non-finite joint fields are dropped at the rapier seam and counted, like every other
    // input seam in this crate (ADR-1507): a NaN motor stiffness or limit bound would otherwise
    // flow into the solver and spread NaN through the joint's bodies and their contacts.
    let mut non_finite = 0u64;
    let (anchor1, anchor2) = (finite_anchor(joint.anchor), finite_anchor(joint.connected_anchor));
    if !joint.anchor.is_finite() {
        tracing::warn!(target: "regia_physics", "joint anchor {:?} is not finite; clamping to origin", joint.anchor);
        non_finite += 1;
    }
    if !joint.connected_anchor.is_finite() {
        tracing::warn!(target: "regia_physics", "joint connected_anchor {:?} is not finite; clamping to origin", joint.connected_anchor);
        non_finite += 1;
    }
    let data = match &joint.kind {
        JointKind::Fixed => {
            let mut data: rapier3d::dynamics::GenericJoint = rapier3d::dynamics::FixedJoint::new().into();
            data.set_local_anchor1(anchor1);
            data.set_local_anchor2(anchor2);
            data
        }
        JointKind::Revolute { axis, limits, motor } => {
            let mut revolute = rapier3d::dynamics::RevoluteJoint::new(to_rapier_vec3(unit_axis(*axis)));
            revolute.set_local_anchor1(anchor1);
            revolute.set_local_anchor2(anchor2);
            if let Some((min, max)) = finite_limits(*limits) {
                revolute.set_limits([min, max]);
            } else if limits.is_some() {
                non_finite += 1;
            }
            if let Some(motor) = motor {
                if motor.is_finite() {
                    apply_motor(&mut revolute, motor);
                } else {
                    non_finite += 1;
                }
            }
            revolute.into()
        }
        JointKind::Prismatic { axis, limits, motor } => {
            let mut prismatic = rapier3d::dynamics::PrismaticJoint::new(to_rapier_vec3(unit_axis(*axis)));
            prismatic.set_local_anchor1(anchor1);
            prismatic.set_local_anchor2(anchor2);
            if let Some((min, max)) = finite_limits(*limits) {
                prismatic.set_limits([min, max]);
            } else if limits.is_some() {
                non_finite += 1;
            }
            if let Some(motor) = motor {
                if motor.is_finite() {
                    apply_motor(&mut prismatic, motor);
                } else {
                    non_finite += 1;
                }
            }
            prismatic.into()
        }
        JointKind::Spherical { cone_limit } => {
            let mut spherical = rapier3d::dynamics::SphericalJoint::new();
            spherical.set_local_anchor1(anchor1);
            spherical.set_local_anchor2(anchor2);
            if let Some(angle) = cone_limit {
                if angle.is_finite() {
                    // A cone around the first body's Y axis bounds the swing (rotation about X
                    // and Z) and leaves the twist about Y free. Rapier limits each angular axis
                    // on its own, so the cone becomes its bounding square of swing angles: exact
                    // along X and Z, up to about √2 × the half-angle on the diagonals.
                    let half = angle.abs();
                    spherical.set_limits(rapier3d::dynamics::JointAxis::AngX, [-half, half]);
                    spherical.set_limits(rapier3d::dynamics::JointAxis::AngZ, [-half, half]);
                } else {
                    // A non-finite cone limit is dropped; the swing stays free.
                    non_finite += 1;
                }
            }
            spherical.into()
        }
        JointKind::SwingTwist {
            twist_axis,
            bend_axis,
            bend,
            side,
            twist,
            rest,
        } => {
            let mut data: rapier3d::dynamics::GenericJoint = rapier3d::dynamics::SphericalJoint::new().into();
            // Frame 1 on this body: X = twist, Y = bend, Z = twist × bend. Frame 2 on the
            // connected body is the same frame at rest, so every angle is zero there.
            let basis = swing_twist_basis(*twist_axis, *bend_axis);
            // A zero (or non-finite) `rest` normalizes to NaN, which is finite on input and so
            // passed the guards: it counts as absent (audit v033b).
            let rest = rest
                .and_then(unit_quat)
                .or_else(|| relative.and_then(unit_quat))
                .unwrap_or(Quat::IDENTITY);
            // The clamped anchors: a non-finite one was counted above and sits at the origin.
            data.set_local_frame1(anchored_pose(anchor1, basis));
            data.set_local_frame2(anchored_pose(anchor2, unit_quat(rest * basis).unwrap_or(basis)));
            // Rapier measures frame 1 → frame 2, the inverse of this body's rotation relative
            // to the connected one, so a range [min, max] becomes [-max, -min]. A non-finite
            // bound is dropped (the axis stays free) and counted.
            for (axis, (min, max)) in [
                (rapier3d::dynamics::JointAxis::AngX, twist),
                (rapier3d::dynamics::JointAxis::AngY, bend),
                (rapier3d::dynamics::JointAxis::AngZ, side),
            ] {
                let Some((lo, hi)) = finite_limits(Some((*min, *max))) else {
                    non_finite += 1;
                    continue;
                };
                let (lo, hi) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                data.set_limits(axis, [-hi, -lo]);
            }
            data
        }
        JointKind::Distance { min, max } => {
            // The rope pattern: coupled linear dofs limited on the coupled LinX axis.
            let mut data =
                rapier3d::dynamics::GenericJointBuilder::new(rapier3d::dynamics::JointAxesMask::empty())
                    .coupled_axes(rapier3d::dynamics::JointAxesMask::LIN_AXES)
                    .build();
            data.set_local_anchor1(anchor1);
            data.set_local_anchor2(anchor2);
            if let Some((min, max)) = finite_limits(Some((*min, *max))) {
                data.set_limits(rapier3d::dynamics::JointAxis::LinX, [min, max]);
            } else {
                non_finite += 1;
            }
            data
        }
    };
    (data, non_finite)
}

/// A motor config whose five fields are all finite; a NaN stiffness or damping would otherwise
/// flow into the solver and spread NaN through the joint's bodies.
impl JointMotorConfig {
    fn is_finite(&self) -> bool {
        self.target_position.is_finite()
            && self.target_velocity.is_finite()
            && self.stiffness.is_finite()
            && self.damping.is_finite()
            && self.max_force.is_finite()
    }
}

/// Returns the limits when both bounds are finite, else `None` (the axis stays free).
fn finite_limits(limits: Option<(f32, f32)>) -> Option<(f32, f32)> {
    limits.filter(|(min, max)| min.is_finite() && max.is_finite())
}

/// A finite anchor, else the origin (clamped before reaching rapier).
fn finite_anchor(a: Vec3) -> rapier3d::math::Vector {
    if a.is_finite() {
        to_rapier_vec3(a)
    } else {
        rapier3d::math::Vector::ZERO
    }
}

/// A joint frame at an anchor already passed through `finite_anchor`.
fn anchored_pose(anchor: rapier3d::math::Vector, rotation: Quat) -> rapier3d::math::Pose {
    rapier3d::math::Pose::from_parts(anchor, to_rapier_rotation(rotation))
}

/// Applies a motor config to a revolute/prismatic joint.
fn apply_motor(joint: &mut impl Motorized, motor: &JointMotorConfig) {
    joint.set_motor(
        motor.target_position,
        motor.target_velocity,
        motor.stiffness,
        motor.damping,
    );
    if motor.max_force > 0.0 {
        joint.set_motor_max_force(motor.max_force);
    }
}

/// The motor surface shared by rapier's revolute and prismatic joints.
trait Motorized {
    /// Spring-damper motor towards a position/velocity.
    fn set_motor(&mut self, target_pos: f32, target_vel: f32, stiffness: f32, damping: f32);
    /// Cap the motor force (0 = unlimited).
    fn set_motor_max_force(&mut self, max_force: f32);
}

impl Motorized for rapier3d::dynamics::RevoluteJoint {
    fn set_motor(&mut self, target_pos: f32, target_vel: f32, stiffness: f32, damping: f32) {
        rapier3d::dynamics::RevoluteJoint::set_motor(self, target_pos, target_vel, stiffness, damping);
    }
    fn set_motor_max_force(&mut self, max_force: f32) {
        rapier3d::dynamics::RevoluteJoint::set_motor_max_force(self, max_force);
    }
}

impl Motorized for rapier3d::dynamics::PrismaticJoint {
    fn set_motor(&mut self, target_pos: f32, target_vel: f32, stiffness: f32, damping: f32) {
        rapier3d::dynamics::PrismaticJoint::set_motor(self, target_pos, target_vel, stiffness, damping);
    }
    fn set_motor_max_force(&mut self, max_force: f32) {
        rapier3d::dynamics::PrismaticJoint::set_motor_max_force(self, max_force);
    }
}

/// The total linear constraint force a joint applied on the last substep (newtons).
/// `dt` must be the substep duration (`substep_dt`), matching rapier's writeback of the
/// last substep's impulses.
fn joint_linear_force(joint: &rapier3d::dynamics::ImpulseJoint, dt: f32) -> f32 {
    if dt <= 0.0 {
        return 0.0;
    }
    // Locked-axes linear dofs (0..3 in rapier's JointAxesMask order), then linear limits.
    let impulses = joint.impulses;
    let mut force = Vec3::new(impulses[0], impulses[1], impulses[2]).length();
    for axis in 0..3u8 {
        if joint.data.limit_axes.bits() & (1 << axis) != 0 {
            force += joint.data.limits[axis as usize].impulse.abs();
        }
    }
    force / dt
}

/// The seconds one constraint substep covers: rapier solves `solver_iterations` substeps per
/// step and writes back the last substep's impulses. The count is clamped like the step clamps
/// it ([`crate::world::MAX_SOLVER_ITERATIONS`]): above it, dividing by the raw setting overstated
/// the force and broke joints early (corr-physics audit).
fn substep_dt(step_dt: f32, solver_iterations: usize) -> f32 {
    step_dt / solver_iterations.clamp(1, crate::world::MAX_SOLVER_ITERATIONS) as f32
}

/// The torque (N·m) a joint carries, estimated from the last substep's impulses: the locked
/// angular rows (3..6 in rapier's `JointAxesMask` order, in the joint frame), plus the angular
/// limit impulses, plus the moment of the linear reaction about the joint point
/// (`(center of mass − anchor) × F` on the connected body). The last term matters: rapier solves
/// the linear rows with their lever-arm coupling projected off the angular rows, so a block
/// hanging off to the side of a joint reads zero in the angular rows and its load shows only as
/// the linear force. It is an upper bound when several constraints are active at once.
fn joint_torque(physics: &PhysicsWorld, joint: &rapier3d::dynamics::ImpulseJoint, dt: f32) -> f32 {
    if dt <= 0.0 {
        return 0.0;
    }
    let impulses = joint.impulses;
    let mut torque = Vec3::new(impulses[3], impulses[4], impulses[5]).length();
    for axis in 3..6u8 {
        if joint.data.limit_axes.bits() & (1 << axis) != 0 {
            torque += joint.data.limits[axis as usize].impulse.abs();
        }
    }
    if let (Some(first), Some(second)) = (
        physics.bodies.get(joint.body1()),
        physics.bodies.get(joint.body2()),
    ) {
        let (_, r1) = crate::convert::from_rapier_pose(first.position());
        let (t2, r2) = crate::convert::from_rapier_pose(second.position());
        let (_, frame) = crate::convert::from_rapier_pose(&joint.data.local_frame1);
        // The linear rows lie along the first body's joint frame (world axes = body · frame).
        let force = (r1 * frame) * Vec3::new(impulses[0], impulses[1], impulses[2]);
        let anchor = t2 + r2 * crate::convert::from_rapier_vec3(joint.data.local_anchor2());
        let com = crate::convert::from_rapier_vec3(second.center_of_mass());
        torque += (com - anchor).cross(force).length();
    }
    torque / dt
}

/// Enables or disables contacts between the two bodies of an existing joint.
fn set_joint_contacts(physics: &mut PhysicsWorld, handle: JointHandle, enabled: bool) {
    if let Some(joint) = physics.impulse_joints.get_mut(handle.to_handle(), true) {
        joint.data.set_contacts_enabled(enabled);
    }
}

/// Creates rapier joints for new and changed `Joint` components, and re-creates joints
/// that lost their rapier object (spec 15 § 2). Runs before the step.
///
/// A joint exists in rapier while both its entity and `connected` carry a rigid body. When
/// either body goes away (component removed, entity despawned) rapier drops the joint, and
/// this system builds it again as soon as both bodies exist; a joint whose `connected`
/// entity gets its body later is created on that step. A joint broken by `break_force`
/// stays broken until its `Joint` component changes. A joint connecting a body to itself
/// is skipped (rapier needs two distinct bodies).
#[instrument(skip_all, target = "regia_physics")]
pub fn sync_joints(
    mut physics: ResMut<PhysicsWorld>,
    joints: JointsQuery,
    handles: Query<&'static RigidBodyHandle>,
    marker_added: Query<&'static JointHandle, Added<JointNoContacts>>,
    order: Option<Res<regia_ecs::spawn_order::SpawnOrder<Joint>>>,
    live_joints: Query<(&'static JointHandle, Has<JointNoContacts>)>,
    mut commands: Commands,
) {
    let _span = debug_span!("sync_joints").entered();

    // The marker toggled on a live joint: flip contacts on the existing rapier joint (no
    // rebuild needed). Joints (re)built below read the marker themselves.
    for handle in &marker_added {
        set_joint_contacts(&mut physics, *handle, false);
    }
    // Removals come from the plugin's observer: `RemovedComponents` is cleared every frame and
    // loses a removal made on a frame without a fixed step (ADR-2396).
    let marker_removed = std::mem::take(&mut physics.pending_removals.joint_no_contacts);
    for entity in marker_removed {
        if let Ok((handle, false)) = live_joints.get(entity) {
            set_joint_contacts(&mut physics, *handle, true);
        }
    }

    // Only joints that need work: changed ones, and live-less ones not held broken.
    let mut pending: Vec<_> = joints
        .iter()
        .filter(|(entity, joint, ..)| {
            joint.is_changed()
                || (!physics.broken_joints.contains(entity) && !joint_is_live(&physics, *entity))
        })
        .collect();
    // Spawn order (see `step::sync_added_bodies`): joint handles follow insertion order.
    pending.sort_by_cached_key(|row| crate::step::spawn_key(order.as_deref(), row.0));
    for (entity, joint, own_handle, existing, no_contacts) in pending {
        let changed = joint.is_changed();
        if changed {
            // A property change rebuilds (impulse joints have no incremental update) and
            // lifts a break.
            physics.broken_joints.remove(&entity);
            physics.remove_joint_of(entity);
        }
        let bodies = own_handle
            .map(|h| h.to_handle())
            .zip(handles.get(joint.connected).ok().map(|h| h.to_handle()));
        let Some((handle_a, handle_b)) = bodies.filter(|(a, b)| a != b) else {
            if changed {
                tracing::debug!(
                    target: "regia_physics",
                    "joint on {entity:?} waits for rigid bodies on itself and {connected:?} \
                     (a joint needs two distinct bodies)",
                    connected = joint.connected
                );
            }
            // No rapier joint now: drop a stale handle component (a body went away).
            if existing.is_some() {
                physics.joint_handles.remove(&entity);
                commands.entity(entity).remove::<JointHandle>();
            }
            continue;
        };
        if !physics.bodies.contains(handle_a) || !physics.bodies.contains(handle_b) {
            continue; // a handle component outlived its body; the removal sync catches up
        }

        let rotation_of = |h: rapier3d::dynamics::RigidBodyHandle| {
            physics.bodies.get(h).map(|b| from_rapier_rotation(*b.rotation()))
        };
        let relative = match (rotation_of(handle_a), rotation_of(handle_b)) {
            (Some(a), Some(b)) => Some((b.inverse() * a).normalize()),
            _ => None,
        };
        let (mut data, non_finite) = build_joint_data(&joint, relative);
        physics.non_finite_inputs += non_finite;
        if matches!(joint.kind, JointKind::Fixed)
            && let Some(relative) = relative
        {
            // A weld keeps the relative rotation the bodies have now instead of snapping both
            // frames to identity. The anchor is the clamped one `build_joint_data` counted.
            data.set_local_frame2(anchored_pose(finite_anchor(joint.connected_anchor), relative));
        }
        if no_contacts {
            data.set_contacts_enabled(false);
        }
        let jh = physics.impulse_joints.insert(handle_a, handle_b, data, true);
        physics.joint_handles.insert(entity, jh);
        commands.entity(entity).insert(JointHandle::from_rapier(jh));
    }
}

/// True when `entity`'s rapier joint exists.
fn joint_is_live(physics: &PhysicsWorld, entity: Entity) -> bool {
    physics
        .joint_handles
        .get(&entity)
        .is_some_and(|handle| physics.impulse_joints.contains(*handle))
}

/// Removes joints whose `break_force` (or [`JointBreakTorque`]) was exceeded on the last step
/// (spec 15 § 2 `break_force`). Raises [`JointBroken`](crate::events::JointBroken) through the event
/// buffer (flushed the same step). A broken joint stays broken until its `Joint` changes.
#[instrument(skip_all, target = "regia_physics")]
pub fn check_joint_breaks(
    fixed: Res<FixedTime>,
    settings: Res<crate::world::PhysicsSettings>,
    mut physics: ResMut<PhysicsWorld>,
    joints: Query<(Entity, &'static Joint, Option<&'static JointBreakTorque>)>,
    mut commands: Commands,
) {
    let _span = debug_span!("check_joint_breaks").entered();
    let dt = fixed.step().as_secs_f32();
    let mut broken = Vec::new();
    for (entity, joint, break_torque) in &joints {
        let max_torque = break_torque.map_or(0.0, |t| t.0);
        let by_force = joint.break_force > 0.0;
        let by_torque = max_torque > 0.0;
        if !by_force && !by_torque {
            continue;
        }
        let Some(rapier_joint) = physics
            .joint_handles
            .get(&entity)
            .and_then(|handle| physics.impulse_joints.get(*handle))
        else {
            continue;
        };
        if (by_force
            && joint_linear_force(rapier_joint, substep_dt(dt, settings.solver_iterations))
                > joint.break_force)
            || (by_torque
                && joint_torque(&physics, rapier_joint, substep_dt(dt, settings.solver_iterations))
                    > max_torque)
        {
            let order = physics.joint_handles.get(&entity).map(|h| h.into_raw_parts());
            broken.push((order, entity, joint.connected));
        }
    }
    // Neither archetype nor entity-id order (thread timing under parallel spawns): removal
    // order decides which joint slots rapier reuses next, and so the solve order of later
    // joints. Handle order is insertion order, which follows spawn order (ADR-2396).
    broken.sort_by_key(|(order, entity, _)| (*order, crate::step::entity_order(*entity)));
    for (_, entity, connected) in broken {
        physics.remove_joint_of(entity);
        physics.broken_joints.insert(entity);
        commands.entity(entity).remove::<JointHandle>();
        physics
            .event_buffer
            .push(RawEvent::BrokenJoint { entity, connected });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rapier3d::dynamics::{FixedJoint, RigidBodyBuilder};

    #[test]
    fn the_break_substep_follows_the_clamped_solver_iterations() {
        let dt = 1.0 / 60.0;
        let max = crate::world::MAX_SOLVER_ITERATIONS;
        assert_eq!(substep_dt(dt, 4), dt / 4.0);
        assert_eq!(substep_dt(dt, 0), dt);
        assert_eq!(substep_dt(dt, max * 2), dt / max as f32);
        assert_eq!(substep_dt(dt, usize::MAX), dt / max as f32);
    }

    #[test]
    fn fixed_joint_data_has_anchors() {
        let joint = Joint {
            kind: JointKind::Fixed,
            connected: Entity::PLACEHOLDER,
            anchor: Vec3::new(1.0, 0.0, 0.0),
            connected_anchor: Vec3::new(0.0, 2.0, 0.0),
            break_force: 0.0,
        };
        let (data, non_finite) = build_joint_data(&joint, None);
        assert_eq!(non_finite, 0);
        assert!((data.local_anchor1().x - 1.0).abs() < 1e-6);
        assert!((data.local_anchor2().y - 2.0).abs() < 1e-6);
    }

    #[test]
    fn non_unit_and_zero_axes_are_normalized() {
        for axis in [
            Vec3::new(0.0, 2.0, 0.0),
            Vec3::ZERO,
            Vec3::new(f32::NAN, 0.0, 0.0),
        ] {
            let joint = Joint {
                kind: JointKind::Revolute {
                    axis,
                    limits: None,
                    motor: None,
                },
                connected: Entity::PLACEHOLDER,
                anchor: Vec3::ZERO,
                connected_anchor: Vec3::ZERO,
                break_force: 0.0,
            };
            let (data, _) = build_joint_data(&joint, None);
            let got = data.local_axis1();
            assert!((got.length() - 1.0).abs() < 1e-5, "axis {axis:?} -> {got:?}");
        }
        assert_eq!(unit_axis(Vec3::new(0.0, 2.0, 0.0)), Vec3::Y);
    }

    /// Audit v033b (Med): `rest: Some(zero quaternion)` normalized to NaN and went into the
    /// joint's second frame. A zero rest counts as absent; the frame stays finite.
    #[test]
    fn a_zero_swing_twist_rest_never_reaches_the_joint_frame() {
        for rest in [
            Some(Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)),
            Some(Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0)),
        ] {
            let joint = Joint {
                kind: JointKind::SwingTwist {
                    twist_axis: Vec3::X,
                    bend_axis: Vec3::Y,
                    bend: (-0.5, 0.5),
                    side: (-0.5, 0.5),
                    twist: (-0.5, 0.5),
                    rest,
                },
                connected: Entity::PLACEHOLDER,
                anchor: Vec3::ZERO,
                connected_anchor: Vec3::ZERO,
                break_force: 0.0,
            };
            let (data, _) = build_joint_data(&joint, None);
            let (_, r2) = crate::convert::from_rapier_pose(&data.local_frame2);
            let (_, r1) = crate::convert::from_rapier_pose(&data.local_frame1);
            assert!(r2.is_finite(), "{rest:?} -> {r2:?}");
            assert!(
                r2.angle_between(r1) < 1e-5,
                "an absent rest is the identity: {r2:?} vs {r1:?}"
            );
        }
    }

    #[test]
    fn spherical_cone_limit_bounds_swing_and_leaves_twist_free() {
        let joint = Joint {
            kind: JointKind::Spherical {
                cone_limit: Some(0.4),
            },
            connected: Entity::PLACEHOLDER,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::ZERO,
            break_force: 0.0,
        };
        let (data, _) = build_joint_data(&joint, None);
        for axis in [
            rapier3d::dynamics::JointAxis::AngX,
            rapier3d::dynamics::JointAxis::AngZ,
        ] {
            let limits = data.limits(axis).expect("swing limit set");
            assert!((limits.min + 0.4).abs() < 1e-6 && (limits.max - 0.4).abs() < 1e-6);
        }
        assert!(
            data.limits(rapier3d::dynamics::JointAxis::AngY).is_none(),
            "twist stays free"
        );
    }

    #[test]
    fn swing_twist_frames_meet_at_rest_and_limits_are_mirrored() {
        use rapier3d::dynamics::JointAxis;
        let rest = Quat::from_rotation_y(0.7);
        let joint = Joint {
            kind: JointKind::SwingTwist {
                twist_axis: Vec3::NEG_Y,
                bend_axis: Vec3::new(1.0, 0.3, 0.0),
                bend: (0.0, 2.0),
                side: (-0.2, 0.1),
                twist: (0.4, -0.3),
                rest: Some(rest),
            },
            connected: Entity::PLACEHOLDER,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::new(0.0, -0.4, 0.0),
            break_force: 0.0,
        };
        let (data, _) = build_joint_data(&joint, Some(Quat::IDENTITY));
        let (_, r1) = crate::convert::from_rapier_pose(&data.local_frame1);
        let (a2, r2) = crate::convert::from_rapier_pose(&data.local_frame2);
        assert!((a2 - Vec3::new(0.0, -0.4, 0.0)).length() < 1e-6);
        // Frame X is the twist axis, Y the bend axis made perpendicular to it.
        assert!((r1 * Vec3::X - Vec3::NEG_Y).length() < 1e-5);
        assert!((r1 * Vec3::Y - Vec3::X).length() < 1e-5);
        // At rest (this = connected · rest) both frames coincide in world space.
        assert!((rest * r1).dot(r2).abs() > 1.0 - 1e-5);
        let limits = |axis| {
            let l = data.limits(axis).expect("limited");
            (l.min, l.max)
        };
        // Rapier measures the inverse rotation: [min, max] becomes [-max, -min]; a swapped
        // pair is ordered first.
        assert_eq!(limits(JointAxis::AngY), (-2.0, 0.0));
        assert_eq!(limits(JointAxis::AngZ), (-0.1, 0.2));
        assert_eq!(limits(JointAxis::AngX), (-0.4, 0.3));
    }

    #[test]
    fn swing_twist_basis_survives_a_parallel_bend_axis() {
        let q = swing_twist_basis(Vec3::Y, Vec3::Y * 3.0);
        assert!((q * Vec3::X - Vec3::Y).length() < 1e-5);
        assert!(
            (q * Vec3::Y).dot(Vec3::Y).abs() < 1e-5,
            "bend axis made perpendicular"
        );
        assert!((q.length() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn distance_joint_limits_the_coupled_linear_dofs() {
        let joint = Joint {
            kind: JointKind::Distance { min: 1.0, max: 3.0 },
            connected: Entity::PLACEHOLDER,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::ZERO,
            break_force: 0.0,
        };
        let (data, _) = build_joint_data(&joint, None);
        assert!(
            data.coupled_axes
                .contains(rapier3d::dynamics::JointAxesMask::LIN_AXES)
        );
        let limits = data
            .limits(rapier3d::dynamics::JointAxis::LinX)
            .expect("linear limit set");
        assert!((limits.min - 1.0).abs() < 1e-6);
        assert!((limits.max - 3.0).abs() < 1e-6);
    }

    #[test]
    fn spherical_cone_limit_bounds_the_swing_axes_symmetrically() {
        use rapier3d::dynamics::JointAxis;
        let joint = Joint {
            kind: JointKind::Spherical {
                cone_limit: Some(0.5),
            },
            connected: Entity::PLACEHOLDER,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::ZERO,
            break_force: 0.0,
        };
        let (data, _) = build_joint_data(&joint, None);
        for axis in [JointAxis::AngX, JointAxis::AngZ] {
            let limits = data.limits(axis).expect("swing axis limited");
            assert!((limits.min + 0.5).abs() < 1e-6, "{axis:?} min {}", limits.min);
            assert!((limits.max - 0.5).abs() < 1e-6, "{axis:?} max {}", limits.max);
        }
        assert!(
            data.limits(JointAxis::AngY).is_none(),
            "the twist about Y stays free"
        );
    }

    #[test]
    fn joint_force_sums_locked_and_limit_impulses() {
        let mut pw = PhysicsWorld::default();
        let b1 = pw.bodies.insert(RigidBodyBuilder::dynamic().build());
        let b2 = pw.bodies.insert(RigidBodyBuilder::dynamic().build());
        let jh = pw.impulse_joints.insert(b1, b2, FixedJoint::new(), true);
        // Zero movement → zero impulse → zero force.
        let joint = pw.impulse_joints.get(jh).expect("joint exists");
        assert_eq!(joint_linear_force(joint, 1.0 / 60.0), 0.0);
    }

    /// Audit k28-physics-1: non-finite joint inputs (NaN anchors, motor fields, limit bounds,
    /// cone limit) must be dropped at the rapier seam — never flow into the solver and spread
    /// NaN through the joint's bodies — and each dropped input must be counted so the count
    /// threads up through `sync_joints` into `physics.non_finite_inputs`.
    #[test]
    fn non_finite_joint_inputs_are_dropped_and_counted() {
        let base = Joint {
            kind: JointKind::Fixed,
            connected: Entity::PLACEHOLDER,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::ZERO,
            break_force: 0.0,
        };
        let count = |joint: &Joint| build_joint_data(joint, None).1;

        // NaN anchors are clamped to the origin and counted (one each).
        let j = Joint {
            anchor: Vec3::new(f32::NAN, 0.0, 0.0),
            connected_anchor: Vec3::new(0.0, f32::NAN, 0.0),
            ..base.clone()
        };
        assert_eq!(count(&j), 2, "NaN anchors counted");

        // NaN revolute limits: the axis stays free and the pair counts once.
        let j = Joint {
            kind: JointKind::Revolute {
                axis: Vec3::X,
                limits: Some((f32::NAN, 1.0)),
                motor: None,
            },
            ..base.clone()
        };
        let (data, n) = build_joint_data(&j, None);
        assert_eq!(n, 1, "NaN revolute limit counted");
        assert!(data.limits(rapier3d::dynamics::JointAxis::AngX).is_none());

        // NaN prismatic limits: the axis stays free and the pair counts once.
        let j = Joint {
            kind: JointKind::Prismatic {
                axis: Vec3::X,
                limits: Some((0.0, f32::NAN)),
                motor: None,
            },
            ..base.clone()
        };
        let (data, n) = build_joint_data(&j, None);
        assert_eq!(n, 1, "NaN prismatic limit counted");
        assert!(data.limits(rapier3d::dynamics::JointAxis::LinX).is_none());

        // NaN motor: the motor is dropped and counted once.
        let j = Joint {
            kind: JointKind::Revolute {
                axis: Vec3::X,
                limits: None,
                motor: Some(JointMotorConfig {
                    target_position: 0.0,
                    target_velocity: 0.0,
                    stiffness: f32::NAN,
                    damping: 0.0,
                    max_force: 0.0,
                }),
            },
            ..base.clone()
        };
        assert_eq!(count(&j), 1, "NaN motor counted");

        // NaN cone limit: the swing stays free and it counts once.
        let j = Joint {
            kind: JointKind::Spherical {
                cone_limit: Some(f32::NAN),
            },
            ..base.clone()
        };
        let (data, n) = build_joint_data(&j, None);
        assert_eq!(n, 1, "NaN cone limit counted");
        assert!(data.limits(rapier3d::dynamics::JointAxis::AngX).is_none());
        assert!(data.limits(rapier3d::dynamics::JointAxis::AngZ).is_none());

        // NaN distance bounds: the rope limit is dropped and it counts once.
        let j = Joint {
            kind: JointKind::Distance {
                min: 1.0,
                max: f32::NAN,
            },
            ..base.clone()
        };
        let (data, n) = build_joint_data(&j, None);
        assert_eq!(n, 1, "NaN distance bound counted");
        assert!(data.limits(rapier3d::dynamics::JointAxis::LinX).is_none());

        // NaN swing-twist bounds: each dropped axis counts once.
        let j = Joint {
            kind: JointKind::SwingTwist {
                twist_axis: Vec3::X,
                bend_axis: Vec3::Y,
                bend: (-0.5, 0.5),
                side: (f32::NAN, 0.5),
                twist: (-0.5, f32::NAN),
                rest: None,
            },
            ..base.clone()
        };
        let (data, n) = build_joint_data(&j, None);
        assert_eq!(n, 2, "each NaN swing-twist bound counted");
        assert!(data.limits(rapier3d::dynamics::JointAxis::AngZ).is_none());
        assert!(data.limits(rapier3d::dynamics::JointAxis::AngX).is_none());

        // Audit g-physics2 finding 2: NaN swing-twist anchors are clamped in both joint frames,
        // not only counted (the frames used to be built from the raw anchors).
        for (anchor, connected_anchor) in [
            (Vec3::new(f32::NAN, 0.0, 0.0), Vec3::new(0.0, -0.4, 0.0)),
            (Vec3::new(0.0, 0.4, 0.0), Vec3::new(0.0, f32::INFINITY, 0.0)),
        ] {
            let j = Joint {
                kind: JointKind::SwingTwist {
                    twist_axis: Vec3::X,
                    bend_axis: Vec3::Y,
                    bend: (-0.5, 0.5),
                    side: (-0.5, 0.5),
                    twist: (-0.5, 0.5),
                    rest: None,
                },
                anchor,
                connected_anchor,
                ..base.clone()
            };
            let (data, n) = build_joint_data(&j, Some(Quat::IDENTITY));
            assert_eq!(n, 1, "the non-finite swing-twist anchor counted");
            let (t1, r1) = crate::convert::from_rapier_pose(&data.local_frame1);
            let (t2, r2) = crate::convert::from_rapier_pose(&data.local_frame2);
            assert!(
                t1.is_finite() && t2.is_finite() && r1.is_finite() && r2.is_finite(),
                "{anchor:?} / {connected_anchor:?} -> frame 1 {t1:?}, frame 2 {t2:?}"
            );
            // The finite anchor is kept; the non-finite one sits at the origin.
            let expect = |a: Vec3| if a.is_finite() { a } else { Vec3::ZERO };
            assert_eq!(t1, expect(anchor));
            assert_eq!(t2, expect(connected_anchor));
        }
    }

    /// Audit g-physics2 finding 1: `sync_joints` rebuilt a weld's frame 2 from the raw
    /// `connected_anchor` after `build_joint_data` had clamped and counted it, so a NaN anchor
    /// reached the solver and spread NaN into both bodies.
    #[test]
    fn a_fixed_joint_with_a_non_finite_connected_anchor_stays_finite_in_rapier() {
        use crate::components::{Collider, RigidBody};
        use regia_app::{App, TimeUpdateStrategy};
        use regia_scene::Transform;

        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(crate::PhysicsPlugin);
        let body = |app: &mut App, at: Vec3, rotation: Quat| {
            let mut transform = Transform::from_translation(at);
            transform.rotation = rotation;
            app.world_mut()
                .spawn((transform, RigidBody::dynamic(), Collider::default()))
                .id()
        };
        // A rotated connected body, so the weld takes the relative-rotation branch.
        let a = body(&mut app, Vec3::new(0.0, 5.0, 0.0), Quat::IDENTITY);
        let b = body(&mut app, Vec3::new(2.0, 5.0, 0.0), Quat::from_rotation_y(0.5));
        app.world_mut().entity_mut(a).insert(Joint {
            kind: JointKind::Fixed,
            connected: b,
            anchor: Vec3::ZERO,
            connected_anchor: Vec3::new(f32::NAN, 0.0, 0.0),
            break_force: 0.0,
        });
        for _ in 0..10 {
            app.update();
        }
        let physics = app.world().resource::<PhysicsWorld>();
        let handle = *physics.joint_handles.get(&a).expect("the weld exists in rapier");
        let joint = physics.impulse_joints.get(handle).expect("live joint");
        let (t2, r2) = crate::convert::from_rapier_pose(&joint.data.local_frame2);
        assert!(t2.is_finite() && r2.is_finite(), "frame 2 {t2:?} {r2:?}");
        assert_eq!(t2, Vec3::ZERO, "the NaN anchor is clamped to the origin");
        assert!(physics.non_finite_inputs >= 1, "the clamp is counted");
        for entity in [a, b] {
            let h = physics.body_handles[&entity];
            let (t, r) = crate::convert::from_rapier_pose(physics.bodies[h].position());
            assert!(t.is_finite() && r.is_finite(), "{entity:?} pose {t:?} {r:?}");
        }
    }
}
