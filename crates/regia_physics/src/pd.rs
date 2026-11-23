// SPDX-License-Identifier: MPL-2.0
//! Joint drive helpers for active ragdolls (`docs/specs/93-character-physics.md` § 10, ADR-2498, ADR-2564).
//!
//! Read-only helpers over the rapier world, plus the two calls that set a joint's drive and lock a body. Nothing here
//! changes the solver: the drive is rapier's own angular joint motor and the lock is rapier's own locked axes.
//!
//! - [`body_dynamics`]: a body's mass, world center of mass, world inverse inertia and velocities.
//! - [`touching_contact_points`]: the world points where a body's colliders touch colliders of other bodies.
//! - [`set_joint_drive`] and [`joint_drive_torque`]: an implicit spring-damper on a joint's three angular axes, and
//!   the torque it applied in the last step.
//! - [`set_body_locked`]: hold a body still (the solver treats it as infinitely heavy).
//! - [`on_fixed_ground`]: whether everything a body touches is fixed.
//! - [`rotation_error`]: the angle-axis rotation between two rotations.

use regia_core::math::{Mat3, Quat, Vec3};
use regia_ecs::prelude::Entity;

use crate::convert::{from_rapier_pose, from_rapier_vec3};
use crate::world::PhysicsWorld;

/// Mass properties and velocities of one rapier body, in world space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BodyDynamics {
    /// Mass, kg (0 for a body with infinite mass).
    pub mass: f32,
    /// World-space center of mass.
    pub center_of_mass: Vec3,
    /// World-space inverse inertia tensor about the center of mass (zero for a body that cannot rotate).
    pub inverse_inertia: Mat3,
    /// Linear velocity of the center of mass, m/s.
    pub linear: Vec3,
    /// Angular velocity, rad/s.
    pub angular: Vec3,
    /// Whether rapier simulates the body (dynamic). Kinematic and fixed bodies ignore forces.
    pub dynamic: bool,
}

/// The [`BodyDynamics`] of the rapier body that `entity` owns, or `None` when it has none yet (it is
/// created on the next step after the `RigidBody` component is added).
pub fn body_dynamics(world: &PhysicsWorld, entity: Entity) -> Option<BodyDynamics> {
    let handle = world.body_handle_of(entity)?;
    let body = world.bodies.get(handle)?;
    let props = &body.mass_properties().local_mprops;
    let (position, rotation) = from_rapier_pose(body.position());
    let local_com = from_rapier_vec3(props.local_com);
    let frame = rotation * crate::convert::from_rapier_rotation(props.principal_inertia_local_frame);
    let axes = Mat3::from_quat(frame);
    let inverse_principal = from_rapier_vec3(props.inv_principal_inertia);
    let inverse_inertia = axes * Mat3::from_diagonal(inverse_principal) * axes.transpose();
    let mass = if props.inv_mass > 0.0 {
        1.0 / props.inv_mass
    } else {
        0.0
    };
    Some(BodyDynamics {
        mass,
        center_of_mass: position + rotation * local_com,
        inverse_inertia,
        linear: from_rapier_vec3(body.linvel()),
        angular: from_rapier_vec3(body.angvel()),
        dynamic: body.is_dynamic(),
    })
}

/// Contact points at most this far apart (m) count as touching in [`touching_contact_points`]: a body resting on
/// the ground can sit a hair above it, inside rapier's prediction distance.
pub const TOUCH_TOLERANCE: f32 = 0.005;

/// Appends to `out` the world-space points where the colliders of `entity`'s body touch (separation at most
/// [`TOUCH_TOLERANCE`]) a collider whose owner is not in `exclude`. The points come from the narrow phase of the
/// last step, in its pair and manifold order; the result is empty when the body has no rapier body yet.
pub fn touching_contact_points(
    world: &PhysicsWorld,
    entity: Entity,
    exclude: &[Entity],
    out: &mut Vec<Vec3>,
) {
    let Some(handle) = world.body_handle_of(entity) else {
        return;
    };
    let Some(body) = world.bodies.get(handle) else {
        return;
    };
    for &collider in body.colliders() {
        for pair in world.narrow_phase.contact_pairs_with(collider) {
            if !pair.has_any_active_contact() {
                continue;
            }
            let ours_first = pair.collider1 == collider;
            let other = if ours_first {
                pair.collider2
            } else {
                pair.collider1
            };
            if world
                .entity_of_collider(other)
                .is_some_and(|e| exclude.contains(&e))
            {
                continue;
            }
            let Some(own) = world.colliders.get(collider) else {
                continue;
            };
            let (t, r) = from_rapier_pose(own.position());
            for manifold in pair.manifolds() {
                for point in &manifold.points {
                    if point.dist > TOUCH_TOLERANCE {
                        continue; // predicted, not touching
                    }
                    let local = if ours_first {
                        point.local_p1
                    } else {
                        point.local_p2
                    };
                    out.push(t + r * from_rapier_vec3(local));
                }
            }
        }
    }
}

/// The torque (N·m) of the drive on `entity`'s joint in the last step: the length of the three axes' torques, zero
/// with no drive. `substep_dt` is the step divided by the solver iterations the joint's bodies ran: rapier writes back
/// the last substep's impulses.
pub fn joint_drive_torque(world: &PhysicsWorld, entity: Entity, substep_dt: f32) -> f32 {
    let Some(joint) = world
        .joint_handles
        .get(&entity)
        .and_then(|handle| world.impulse_joints.get(*handle))
    else {
        return 0.0;
    };
    if substep_dt <= 0.0 {
        return 0.0;
    }
    let squared: f32 = ANGULAR_AXES
        .into_iter()
        .filter_map(|axis| joint.data.motor(axis))
        .map(|motor| motor.impulse * motor.impulse)
        .sum();
    let torque = squared.sqrt() / substep_dt;
    if torque.is_finite() { torque } else { 0.0 }
}

/// Locks (or frees) every axis of `entity`'s body: the solver treats a locked body as infinitely heavy and it does not
/// move, but it still has contacts. Locking stops it, also when the body was already locked and moving (after
/// [`set_body_locked_moving`]).
pub fn set_body_locked(world: &mut PhysicsWorld, entity: Entity, locked: bool) {
    use rapier3d::prelude::LockedAxes;
    let Some(handle) = world.body_handle_of(entity) else {
        return;
    };
    let Some(body) = world.bodies.get_mut(handle) else {
        return;
    };
    let wanted = if locked {
        LockedAxes::all()
    } else {
        LockedAxes::empty()
    };
    if body.locked_axes() != wanted {
        body.set_locked_axes(wanted, true);
        if locked {
            // A locked axis keeps the velocity it had.
            body.set_linvel(rapier3d::math::Vector::ZERO, true);
            body.set_angvel(rapier3d::math::AngVector::ZERO, true);
        }
    } else if locked {
        // Already locked, but still moving: a foot that followed a moving support
        // (`set_body_locked_moving`) and now locks to fixed ground. A stationary lock stops it. A body
        // that is already still is left alone (no wake-up), so a plain lock repeated every step is unchanged.
        let linear = from_rapier_vec3(body.linvel());
        let angular = from_rapier_vec3(body.angvel());
        if linear != Vec3::ZERO || angular != Vec3::ZERO {
            body.set_linvel(rapier3d::math::Vector::ZERO, true);
            body.set_angvel(rapier3d::math::AngVector::ZERO, true);
        }
    }
}

/// The rotation (axis times angle, rad) that takes `current` to `target`, the short way round: zero for
/// equal rotations and for non-finite input.
pub fn rotation_error(current: Quat, target: Quat) -> Vec3 {
    let mut delta = target * current.inverse();
    if delta.w < 0.0 {
        delta = -delta;
    }
    let (axis, angle) = delta.to_axis_angle();
    let error = axis * angle;
    if error.is_finite() { error } else { Vec3::ZERO }
}

/// Whether every collider `entity`'s body touches (see [`touching_contact_points`]) that is not in `exclude` belongs to a
/// fixed body. `false` when it touches nothing.
pub fn on_fixed_ground(world: &PhysicsWorld, entity: Entity, exclude: &[Entity]) -> bool {
    let Some(body) = world.body_handle_of(entity).and_then(|h| world.bodies.get(h)) else {
        return false;
    };
    let mut touches = false;
    for &collider in body.colliders() {
        for pair in world.narrow_phase.contact_pairs_with(collider) {
            if !pair.has_any_active_contact() {
                continue;
            }
            let other = if pair.collider1 == collider {
                pair.collider2
            } else {
                pair.collider1
            };
            if world
                .entity_of_collider(other)
                .is_some_and(|e| exclude.contains(&e))
            {
                continue;
            }
            let fixed = world
                .colliders
                .get(other)
                .and_then(|c| c.parent())
                .and_then(|h| world.bodies.get(h))
                .is_none_or(|b| b.is_fixed());
            if !fixed {
                return false;
            }
            touches = true;
        }
    }
    touches
}

/// One joint's implicit drive for [`set_joint_drive`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct JointDrive {
    /// The rotation of the joint's body relative to its connected (parent) body the drive pulls towards
    /// (`parent⁻¹ · child`).
    pub target: Quat,
    /// Stiffness, N·m/rad.
    pub kp: f32,
    /// Damping, N·m·s/rad.
    pub kd: f32,
    /// Largest torque about the world X, Y and Z axes, N·m. Each of the joint's three angular axes gets the limit its
    /// direction picks out of these.
    pub max_torque: Vec3,
}

const ANGULAR_AXES: [rapier3d::dynamics::JointAxis; 3] = [
    rapier3d::dynamics::JointAxis::AngX,
    rapier3d::dynamics::JointAxis::AngY,
    rapier3d::dynamics::JointAxis::AngZ,
];

/// Sets (or, with `None`, clears) the implicit drive of the joint whose `Joint` component is on `entity`: rapier's own
/// angular motors, force based, one per axis, solved with the joint's other constraint rows. A no-op when the joint
/// does not exist yet. Clearing a joint that has no drive leaves it untouched. Setting a drive wakes the two bodies.
pub fn set_joint_drive(world: &mut PhysicsWorld, entity: Entity, drive: Option<JointDrive>) {
    use rapier3d::dynamics::{JointAxesMask, MotorModel};
    let Some(&handle) = world.joint_handles.get(&entity) else {
        return;
    };
    let Some(drive) = drive else {
        let driven = world
            .impulse_joints
            .get(handle)
            .is_some_and(|j| !(j.data.motor_axes & JointAxesMask::ANG_AXES).is_empty());
        if driven && let Some(joint) = world.impulse_joints.get_mut(handle, false) {
            joint.data.motor_axes.remove(JointAxesMask::ANG_AXES);
        }
        return;
    };
    if !(drive.target.is_finite()
        && drive.kp.is_finite()
        && drive.kd.is_finite()
        && drive.max_torque.is_finite())
    {
        return;
    }
    let Some(joint) = world.impulse_joints.get(handle) else {
        return;
    };
    let rotation = world.bodies.get(joint.body1()).map_or(Quat::IDENTITY, |b| {
        crate::convert::from_rapier_rotation(*b.rotation())
    });
    let Some(joint) = world.impulse_joints.get_mut(handle, true) else {
        return;
    };
    // Rapier measures frame 1 -> frame 2, `F1⁻¹ · child⁻¹ · parent · F2`, one axis at a time from the quaternion's
    // imaginary part (twice its arcsine).
    let frame1 = crate::convert::from_rapier_rotation(joint.data.local_frame1.rotation);
    let frame2 = crate::convert::from_rapier_rotation(joint.data.local_frame2.rotation);
    let mut measured = (frame1.inverse() * drive.target.inverse() * frame2).normalize();
    if measured.w < 0.0 {
        measured = -measured;
    }
    let imaginary = [measured.x, measured.y, measured.z];
    let axes = Mat3::from_quat(rotation * frame1);
    for (index, (axis, part)) in ANGULAR_AXES.into_iter().zip(imaginary).enumerate() {
        let angle = part.clamp(-1.0, 1.0).asin() * 2.0;
        let limit = axes.col(index).abs().dot(drive.max_torque.abs());
        joint.data.set_motor(
            axis,
            if angle.is_finite() { angle } else { 0.0 },
            0.0,
            drive.kp,
            drive.kd,
        );
        joint.data.set_motor_model(axis, MotorModel::ForceBased);
        joint.data.set_motor_max_force(axis, limit);
    }
}

/// What a body stands on, from [`foot_support`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FootSupport {
    /// The entity owning the touched collider with the lowest collider handle (index, then
    /// generation), when it has one. Not narrow-phase order, which follows the contact graph's
    /// history: the choice must not depend on how pairs were found (spec 93 § 10).
    pub body: Option<Entity>,
    /// Every touched collider belongs to a fixed body (or to no body).
    pub fixed: bool,
    /// Every touched collider belongs to the same rapier body (or to no body).
    pub single: bool,
    /// That single body is dynamic (it takes forces); `false` for fixed and kinematic bodies.
    pub dynamic: bool,
}

/// What `entity`'s body touches (see [`touching_contact_points`]) among colliders whose owner is not in `exclude`:
/// whether all of it is fixed, whether it is all one body, and that body's entity. `None` when it touches nothing.
pub fn foot_support(world: &PhysicsWorld, entity: Entity, exclude: &[Entity]) -> Option<FootSupport> {
    let body = world.body_handle_of(entity).and_then(|h| world.bodies.get(h))?;
    let mut found: Option<FootSupport> = None;
    let mut first_handle = None;
    let mut lowest = (u32::MAX, u32::MAX);
    for &collider in body.colliders() {
        for pair in world.narrow_phase.contact_pairs_with(collider) {
            if !pair.has_any_active_contact() {
                continue;
            }
            let other = if pair.collider1 == collider {
                pair.collider2
            } else {
                pair.collider1
            };
            let owner = world.entity_of_collider(other);
            if owner.is_some_and(|e| exclude.contains(&e)) {
                continue;
            }
            let touching = pair
                .manifolds()
                .iter()
                .any(|m| m.points.iter().any(|p| p.dist <= TOUCH_TOLERANCE));
            if !touching {
                continue;
            }
            let handle = world.colliders.get(other).and_then(|c| c.parent());
            let other_body = handle.and_then(|h| world.bodies.get(h));
            let fixed = other_body.is_none_or(|b| b.is_fixed());
            let dynamic = other_body.is_some_and(|b| b.is_dynamic());
            let key = other.into_raw_parts();
            match found.as_mut() {
                None => {
                    first_handle = handle;
                    lowest = key;
                    found = Some(FootSupport {
                        body: owner,
                        fixed,
                        single: true,
                        dynamic,
                    });
                }
                Some(support) => {
                    support.fixed &= fixed;
                    if handle != first_handle {
                        support.single = false;
                        support.dynamic = false;
                    }
                    if key < lowest {
                        lowest = key;
                        support.body = owner;
                    }
                }
            }
        }
    }
    found
}

/// World pose (position, rotation) of `entity`'s rapier body, `None` without one.
pub fn body_pose(world: &PhysicsWorld, entity: Entity) -> Option<(Vec3, Quat)> {
    let body = world.body_handle_of(entity).and_then(|h| world.bodies.get(h))?;
    Some(from_rapier_pose(body.position()))
}

/// Velocity (m/s) of the material point of `entity`'s body that is at `point` now: the linear velocity plus the
/// angular velocity crossed with the lever from the center of mass. `None` without a body.
pub fn point_velocity(world: &PhysicsWorld, entity: Entity, point: Vec3) -> Option<Vec3> {
    let d = body_dynamics(world, entity)?;
    let v = d.linear + d.angular.cross(point - d.center_of_mass);
    Some(if v.is_finite() { v } else { Vec3::ZERO })
}

/// Locks every axis of `entity`'s body, as [`set_body_locked`] does, and sets its velocity: a locked body keeps the
/// velocity it has (forces, gravity and contacts do not change it), so it moves along exactly that. Non-finite input
/// sets zero velocity.
pub fn set_body_locked_moving(world: &mut PhysicsWorld, entity: Entity, linear: Vec3, angular: Vec3) {
    use rapier3d::prelude::LockedAxes;
    let Some(body) = world.body_handle_of(entity).and_then(|h| world.bodies.get_mut(h)) else {
        return;
    };
    if body.locked_axes() != LockedAxes::all() {
        body.set_locked_axes(LockedAxes::all(), true);
    }
    let (linear, angular) = if linear.is_finite() && angular.is_finite() {
        (linear, angular)
    } else {
        (Vec3::ZERO, Vec3::ZERO)
    };
    body.set_linvel(crate::convert::to_rapier_vec3(linear), true);
    body.set_angvel(crate::convert::to_rapier_vec3(angular), true);
}

/// Applies `impulse` (N·s) at the world `point` of `entity`'s body when it is dynamic (wakes it). Non-finite input and
/// bodies that are not dynamic are ignored.
pub fn apply_impulse_at_point(world: &mut PhysicsWorld, entity: Entity, impulse: Vec3, point: Vec3) {
    if !(impulse.is_finite() && point.is_finite()) {
        return;
    }
    let Some(body) = world.body_handle_of(entity).and_then(|h| world.bodies.get_mut(h)) else {
        return;
    };
    if body.is_dynamic() {
        body.apply_impulse_at_point(
            crate::convert::to_rapier_vec3(impulse),
            crate::convert::to_rapier_vec3(point),
            true,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_error_is_the_short_way_round() {
        let a = Quat::from_rotation_y(0.2);
        let b = Quat::from_rotation_y(0.5);
        let e = rotation_error(a, b);
        assert!((e - Vec3::new(0.0, 0.3, 0.0)).length() < 1e-5, "{e:?}");
        // The negated quaternion is the same rotation.
        let e2 = rotation_error(a, -b);
        assert!((e2 - e).length() < 1e-5);
        assert_eq!(rotation_error(a, a), Vec3::ZERO);
    }

    /// rc1 physics review, finding 9: the support body was the first touched collider in
    /// narrow-phase order, which follows the order the pairs were found. It is the lowest collider
    /// handle now: a foot on two platforms names the same one whichever came under it first.
    #[test]
    fn the_support_body_does_not_depend_on_which_platform_came_first() {
        use rapier3d::dynamics::RigidBodyBuilder;
        use rapier3d::geometry::ColliderBuilder;

        let mut ecs = regia_ecs::prelude::World::new();
        for late_is_low in [false, true] {
            let mut world = PhysicsWorld::default();
            let mut platform = |world: &mut PhysicsWorld, x: f32, y: f32| {
                let body = world.bodies.insert(
                    RigidBodyBuilder::fixed()
                        .translation(crate::convert::to_rapier_vec3(Vec3::new(x, y, 0.0))),
                );
                let PhysicsWorld {
                    bodies, colliders, ..
                } = &mut *world;
                let collider =
                    colliders.insert_with_parent(ColliderBuilder::cuboid(0.5, 0.5, 0.5), body, bodies);
                let entity = ecs.spawn_empty().id();
                world.register_body(body, entity);
                world.register_collider(collider, entity);
                (body, collider, entity)
            };
            // The low-handle platform is inserted first; one of the two starts far below and is
            // raised under the foot after the other one's contact exists.
            let low = platform(&mut world, -0.45, if late_is_low { -50.0 } else { -0.5 });
            let high = platform(&mut world, 0.45, if late_is_low { -0.5 } else { -50.0 });
            assert!(low.1.into_raw_parts() < high.1.into_raw_parts());
            let foot_body = world.bodies.insert(
                RigidBodyBuilder::dynamic()
                    .translation(crate::convert::to_rapier_vec3(Vec3::new(0.0, 0.2, 0.0))),
            );
            let PhysicsWorld {
                bodies, colliders, ..
            } = &mut world;
            let foot_collider =
                colliders.insert_with_parent(ColliderBuilder::cuboid(0.2, 0.2, 0.2), foot_body, bodies);
            let foot = ecs.spawn_empty().id();
            world.register_body(foot_body, foot);
            world.register_collider(foot_collider, foot);
            for _ in 0..20 {
                crate::destruction::step_standalone(&mut world, 1.0 / 60.0, [0.0, -9.81, 0.0]);
            }
            let late = if late_is_low { low.0 } else { high.0 };
            world.bodies.get_mut(late).expect("platform").set_translation(
                crate::convert::to_rapier_vec3(Vec3::new(if late_is_low { -0.45 } else { 0.45 }, -0.5, 0.0)),
                true,
            );
            for _ in 0..10 {
                crate::destruction::step_standalone(&mut world, 1.0 / 60.0, [0.0, -9.81, 0.0]);
            }
            let support = foot_support(&world, foot, &[]).expect("the foot stands on something");
            assert!(
                !support.single,
                "late_is_low {late_is_low}: the foot touches both platforms"
            );
            assert!(support.fixed);
            assert_eq!(
                support.body,
                Some(low.2),
                "late_is_low {late_is_low}: the lowest handle's owner"
            );
        }
    }
}
