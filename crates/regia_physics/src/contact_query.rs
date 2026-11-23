// SPDX-License-Identifier: MPL-2.0
//! Read-only contact geometry for contact sounds and tools (ADR-3270).
//!
//! [`ContactForce`](crate::events::ContactForce) names two collider entities and a force; a sound
//! also needs where the bodies touch, along which normal, and how each body moves there. These
//! helpers read that from the stepped rapier world without changing it or the event structs other
//! systems read:
//!
//! - [`contact_sample`]: the mean touching point and the normal (from the first entity's collider
//!   towards the second's) of the pair's last step, with both bodies' motion.
//! - [`collider_body_motion`]: the velocities and world center of mass of the body a collider
//!   belongs to (zero motion for a collider without a body).
//! - [`BodyMotion::velocity_at`]: the rigid-body velocity of a world point, `v + ω × (p − c)`.
//! - [`normal_impulse`]: the impulse that changed a pair's relative normal velocity over a step,
//!   from the reduced mass (rapier reports only its last substep's impulse in contact-force
//!   events, so a short impact shows in the velocities, not in the reported force).
//!
//! Nothing here allocates. Provenance: the point-velocity formula is textbook rigid-body
//! kinematics; the manifold fields are rapier3d's public narrow-phase API (Apache-2.0).

use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;

use rapier3d::geometry::{ColliderHandle, ContactPair};

use crate::convert::{from_rapier_pose, from_rapier_vec3};
use crate::world::PhysicsWorld;

/// Contact points farther apart than this (metres) are predicted contacts, not touching ones.
pub const CONTACT_TOUCH_TOLERANCE: f32 = 0.005;

/// How one rigid body moves, in world space (all zero for a collider without a body).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BodyMotion {
    /// The body's entity, `None` for a collider without a body.
    pub body: Option<Entity>,
    /// Linear velocity of the center of mass, m/s.
    pub linear: Vec3,
    /// Angular velocity, rad/s.
    pub angular: Vec3,
    /// World-space center of mass.
    pub center_of_mass: Vec3,
    /// `true` for a dynamic body (fixed and kinematic bodies are `false`).
    pub dynamic: bool,
    /// Inverse mass, 1/kg (0 for fixed and kinematic bodies and colliders without a body).
    pub inverse_mass: f32,
}

impl BodyMotion {
    /// The velocity of the body's material at world point `point`: `linear + angular × (point − center)`.
    #[must_use]
    pub fn velocity_at(&self, point: Vec3) -> Vec3 {
        self.linear + self.angular.cross(point - self.center_of_mass)
    }
}

/// What the narrow phase knows about one touching pair after a step.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ContactSample {
    /// Mean of the touching contact points, world space.
    pub point: Vec3,
    /// Unit contact normal pointing from the first entity's collider towards the second's.
    pub normal: Vec3,
    /// Touching contact points the mean was taken over (at least 1).
    pub points: u32,
    /// Motion of the first entity's body.
    pub motion_a: BodyMotion,
    /// Motion of the second entity's body.
    pub motion_b: BodyMotion,
}

/// The motion of the body `collider` (a collider's entity) belongs to; `None` when the entity has
/// neither a rapier collider nor a body. A collider without a parent body has zero motion and
/// `dynamic: false`. An entity with several colliders and no single handle (a tiled terrain, whose
/// tiles hang on its body) answers with its body's motion.
#[must_use]
pub fn collider_body_motion(world: &PhysicsWorld, collider: Entity) -> Option<BodyMotion> {
    let Some(handle) = world.collider_handle_of(collider) else {
        // Corr-physics audit: a tiled terrain has tile colliders but no single handle, so contact
        // sounds on streamed ground found no motion at all.
        return body_motion(world, world.body_handles.get(&collider).copied()?);
    };
    let rapier_collider = world.colliders.get(handle)?;
    let Some(parent) = rapier_collider.parent() else {
        let (at, _) = from_rapier_pose(rapier_collider.position());
        return Some(BodyMotion {
            center_of_mass: at,
            ..BodyMotion::default()
        });
    };
    body_motion(world, parent)
}

/// The motion of rapier body `parent`; `None` when it is gone.
fn body_motion(world: &PhysicsWorld, parent: rapier3d::dynamics::RigidBodyHandle) -> Option<BodyMotion> {
    let body = world.bodies.get(parent)?;
    let inverse_mass = body.mass_properties().local_mprops.inv_mass;
    Some(BodyMotion {
        body: world.entity_of_body(parent),
        linear: from_rapier_vec3(body.linvel()),
        angular: from_rapier_vec3(body.angvel()),
        center_of_mass: from_rapier_vec3(body.center_of_mass()),
        dynamic: body.is_dynamic(),
        inverse_mass: if body.is_dynamic() && inverse_mass.is_finite() {
            inverse_mass
        } else {
            0.0
        },
    })
}

/// The contact of colliders `a` and `b` (collider entities) after the last step: the mean touching
/// point (or, when no point is within [`CONTACT_TOUCH_TOLERANCE`], the mean of the pair's predicted
/// points: the solver stops a fast body before its shapes meet), the normal from `a` towards `b`,
/// and both bodies' motion. `None` when either entity has no collider, the pair is not in the
/// narrow phase, or it has no contact point at all.
#[must_use]
pub fn contact_sample(world: &PhysicsWorld, a: Entity, b: Entity) -> Option<ContactSample> {
    let (pair, a_first) = pair_of(world, a, b)?;
    let first = world.colliders.get(pair.collider1)?;
    let (t, r) = from_rapier_pose(first.position());
    // Touching points when there are any; otherwise every point of the pair (a speculative contact
    // the solver already pushed on: a fast body is stopped the step before its shapes meet).
    let mut sum = Vec3::ZERO;
    let mut normal = Vec3::ZERO;
    let mut points = 0u32;
    for touching_only in [true, false] {
        for manifold in pair.manifolds() {
            // A compound collider's manifold points are local to the touching sub-shape.
            let (st, sr) = manifold
                .subshape_pos1()
                .map_or((Vec3::ZERO, regia_core::math::Quat::IDENTITY), from_rapier_pose);
            let mut used = false;
            for point in &manifold.points {
                if touching_only && point.dist > CONTACT_TOUCH_TOLERANCE {
                    continue; // predicted, not touching
                }
                sum += t + r * (st + sr * from_rapier_vec3(point.local_p1));
                points += 1;
                used = true;
            }
            if used {
                normal += from_rapier_vec3(manifold.data.normal);
            }
        }
        if points > 0 {
            break;
        }
    }
    if points == 0 {
        return None;
    }
    let normal = normal.normalize_or_zero();
    if normal == Vec3::ZERO {
        return None;
    }
    let normal = if a_first { normal } else { -normal };
    Some(ContactSample {
        point: sum / points as f32,
        normal,
        points,
        motion_a: collider_body_motion(world, a)?,
        motion_b: collider_body_motion(world, b)?,
    })
}

/// The narrow-phase pair of entities `a` and `b`, and whether `a` owns its first collider. An
/// entity with several colliders and no single handle (a tiled terrain's tiles) is found through
/// the other entity's pairs (corr-physics audit: it answered `None` for every contact with
/// streamed ground).
fn pair_of(world: &PhysicsWorld, a: Entity, b: Entity) -> Option<(&ContactPair, bool)> {
    match (world.collider_handle_of(a), world.collider_handle_of(b)) {
        (Some(handle_a), Some(handle_b)) => {
            let pair = world.narrow_phase.contact_pair(handle_a, handle_b)?;
            Some((pair, pair.collider1 == handle_a))
        }
        (Some(handle_a), None) => {
            pair_with_entity(world, handle_a, b).map(|pair| (pair, pair.collider1 == handle_a))
        }
        (None, Some(handle_b)) => {
            pair_with_entity(world, handle_b, a).map(|pair| (pair, pair.collider1 != handle_b))
        }
        (None, None) => None,
    }
}

/// Of `one`'s pairs, the one whose other collider belongs to `other`: the most touching points,
/// then the most points, the narrow phase's order on ties (deterministic for one history).
fn pair_with_entity(world: &PhysicsWorld, one: ColliderHandle, other: Entity) -> Option<&ContactPair> {
    let mut best: Option<(&ContactPair, (usize, usize))> = None;
    for pair in world.narrow_phase.contact_pairs_with(one) {
        let far = if pair.collider1 == one {
            pair.collider2
        } else {
            pair.collider1
        };
        if world.entity_of_collider(far) != Some(other) {
            continue;
        }
        let mut score = (0usize, 0usize);
        for manifold in pair.manifolds() {
            for point in &manifold.points {
                score.0 += usize::from(point.dist <= CONTACT_TOUCH_TOLERANCE);
                score.1 += 1;
            }
        }
        if best.is_none_or(|(_, s)| score > s) {
            best = Some((pair, score));
        }
    }
    best.map(|(pair, _)| pair)
}

/// The normal impulse (N·s) that turned the relative normal velocity `approach_before` (m/s, positive
/// when closing) into `approach_after` for bodies with inverse masses `inverse_mass_a` and
/// `inverse_mass_b`: `max(0, before − after) / (1/m_a + 1/m_b)`, ignoring rotation. Zero when
/// neither body is dynamic or the pair did not slow down.
#[must_use]
pub fn normal_impulse(
    approach_before: f32,
    approach_after: f32,
    inverse_mass_a: f32,
    inverse_mass_b: f32,
) -> f32 {
    let inverse = inverse_mass_a.max(0.0) + inverse_mass_b.max(0.0);
    let change = approach_before - approach_after;
    if !(inverse.is_finite() && change.is_finite()) || inverse <= 0.0 || change <= 0.0 {
        return 0.0;
    }
    change / inverse
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_velocity_adds_the_spin() {
        let motion = BodyMotion {
            linear: Vec3::new(1.0, 0.0, 0.0),
            angular: Vec3::new(0.0, 0.0, 2.0),
            center_of_mass: Vec3::new(0.0, 1.0, 0.0),
            ..BodyMotion::default()
        };
        // The bottom of a ball spinning about +z: ω × (0, −1, 0) = (2, 0, 0), plus 1 m/s forward.
        let v = motion.velocity_at(Vec3::ZERO);
        assert!((v - Vec3::new(3.0, 0.0, 0.0)).length() < 1e-6, "{v:?}");
        assert_eq!(motion.velocity_at(motion.center_of_mass), motion.linear);
    }

    #[test]
    fn normal_impulse_uses_the_reduced_mass() {
        // A 2 kg body stopping from 3 m/s against the ground: 6 N·s.
        assert!((normal_impulse(3.0, 0.0, 0.5, 0.0) - 6.0).abs() < 1e-6);
        // Two 2 kg bodies closing at 3 m/s and stopping: reduced mass 1 kg, 3 N·s.
        assert!((normal_impulse(3.0, 0.0, 0.5, 0.5) - 3.0).abs() < 1e-6);
        assert_eq!(normal_impulse(0.0, 1.0, 0.5, 0.0), 0.0, "separating: no impulse");
        assert_eq!(normal_impulse(3.0, 0.0, 0.0, 0.0), 0.0, "two fixed bodies");
    }
}
