// SPDX-License-Identifier: MPL-2.0
//! Rigid-body coupling with `regia_physics` (feature `physics`, ADR-2577).
//!
//! A physics body with a [`FluidBody`] becomes a moving solid of the fluid simulation it names:
//! every `FixedUpdate`, [`gather_fluid_bodies`] reads its pose, velocity and collider shape and
//! hands the list to the simulation ([`crate::FluidSim::set_moving_solids`]) before the fluid
//! steps; the liquid then moves with the body and cannot flow through it. For a dynamic body with
//! `two_way`, [`apply_fluid_impulses`] turns the pressure impulse the step integrated over the
//! body's surface into an `ExternalImpulse` the physics step applies in the same `FixedUpdate`
//! (buoyancy, drag, a paddle's push back).
//!
//! The coupling only reads physics components and writes `ExternalImpulse`, the physics crate's
//! public impulse input; it never edits the physics world. A dynamic body gets impulses from its
//! second fixed step on (once it has a `PhysicsPose`): the physics step sets up a new body's mass
//! properties in its first step, and an impulse before that would act on the wrong mass.
//!
//! Supported collider shapes: box, sphere and capsule (on the body's own entity, any axis and
//! offset). A body with another shape is listed in [`FluidCoupling`] as `unsupported` and does not
//! act on the liquid. Kinematic bodies take their velocity from the change of their `Transform`
//! between steps; dynamic bodies from their `Velocity` (about the body origin, the centre of mass
//! of the symmetric shapes the coupling supports).

use regia_app::FixedTime;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_physics::{
    BodyKind, Collider, ColliderShape, ExternalImpulse, PhysicsPose, PhysicsStep, RigidBody, Velocity,
};
use regia_scene::Transform;

use crate::plugin::{FluidStep, FluidWorld};
use crate::sim::MAX_SHAPES;
use crate::solid::{MovingSolid, SolidMotion, SolidShape};

/// Makes a physics body a moving solid of fluid simulation `fluid` (a `FluidWorld` id).
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct FluidBody {
    /// The simulation (`FluidWorld` id, the `id` of `fluid.create`).
    pub fluid: u32,
    /// Apply the liquid's pressure impulses back to the body (dynamic bodies only).
    pub two_way: bool,
    /// Most linear impulse applied per step (N s); 0 means no cap.
    pub max_impulse: f32,
}

impl FluidBody {
    /// A two-way coupled body of simulation `fluid`, no impulse cap.
    #[must_use]
    pub fn new(fluid: u32) -> Self {
        Self {
            fluid,
            two_way: true,
            max_impulse: 0.0,
        }
    }
}

/// One coupled body's state after the last step (one entry of `fluid.solids` `coupled_bodies`).
#[derive(Debug, Clone, PartialEq)]
pub struct CoupledBody {
    /// The body.
    pub entity: Entity,
    /// Its simulation.
    pub fluid: u32,
    /// Kinematic or dynamic (static bodies count as kinematic at rest).
    pub motion: SolidMotion,
    /// The body's position (its origin, about which the angular impulse is given).
    pub origin: Vec3,
    /// The solid it became, `None` when its collider shape is not supported.
    pub solid: Option<MovingSolid>,
    /// Whether the impulses go back to the body.
    pub two_way: bool,
    /// Linear impulse of the last step (N s).
    pub impulse: Vec3,
    /// Angular impulse about the body origin of the last step (N m s).
    pub angular_impulse: Vec3,
    /// Whether the physics step has run the body yet (it has a `PhysicsPose`). A body it has not
    /// stepped gets no impulse: its mass properties are only set up in its first step, so an
    /// impulse before that would divide by the wrong mass.
    pub simulated: bool,
    /// Whether the last impulse was applied (two-way dynamic body, simulated, finite, after the cap).
    pub applied: bool,
    /// Surface faces that touched liquid in the last step.
    pub wetted_faces: u32,
}

/// Every coupled body, in entity order; rebuilt each `FixedUpdate`.
#[derive(Resource, Debug, Clone, Default)]
pub struct FluidCoupling {
    /// The bodies.
    pub bodies: Vec<CoupledBody>,
    /// Bodies skipped because their simulation already had [`MAX_SHAPES`] solids or did not exist.
    pub skipped: u32,
    /// Coupling steps run.
    pub steps: u64,
    /// The pose and spin of each kinematic body at the last gather (for its velocity).
    previous: Vec<(Entity, Vec3, Quat, Vec3)>,
}

/// Couples `FluidBody` physics bodies with their simulations. Add after `FluidPlugin` and
/// `PhysicsPlugin`; the simulation's `dt` should be the fixed step (`1 / FixedTime::hz`).
#[derive(Debug, Default, Clone, Copy)]
pub struct FluidPhysicsPlugin;

impl Plugin for FluidPhysicsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<FluidCoupling>()
            .configure_sets(FixedUpdate, FluidStep.before(PhysicsStep))
            .add_systems(
                FixedUpdate,
                (
                    gather_fluid_bodies.before(FluidStep),
                    apply_fluid_impulses.after(FluidStep).before(PhysicsStep),
                ),
            );
    }
}

/// The collider's shape as a solid, and its pose relative to the body.
fn solid_shape(collider: &Collider) -> Option<(SolidShape, Vec3, Quat)> {
    let (shape, turn) = match &collider.shape {
        ColliderShape::Box { half_extents } => (
            SolidShape::Box {
                half_extents: *half_extents,
            },
            Quat::IDENTITY,
        ),
        ColliderShape::Sphere { radius } => (SolidShape::Sphere { radius: *radius }, Quat::IDENTITY),
        ColliderShape::Capsule {
            half_height,
            radius,
            axis,
        } => (
            SolidShape::Capsule {
                half_height: *half_height,
                radius: *radius,
            },
            axis.from_y_rotation(),
        ),
        _ => return None,
    };
    Some((
        shape,
        collider.offset.translation,
        collider.offset.rotation * turn,
    ))
}

type BodyItem<'a> = (
    Entity,
    &'a FluidBody,
    &'a RigidBody,
    Option<&'a Collider>,
    &'a Transform,
    Option<&'a PhysicsPose>,
    Option<&'a Velocity>,
);

/// Reads every `FluidBody`'s pose, velocity and shape and sets each simulation's moving solids.
pub fn gather_fluid_bodies(
    bodies: Query<BodyItem<'_>>,
    mut fluids: ResMut<FluidWorld>,
    mut coupling: ResMut<FluidCoupling>,
    time: Option<Res<FixedTime>>,
) {
    let _span = tracing::info_span!(target: "regia_fluid", "fluid_gather_bodies").entered();
    let mut items: Vec<BodyItem<'_>> = bodies.iter().collect();
    // Entity order, so the solid list (and the digest) does not depend on query order.
    items.sort_by_key(|item| item.0.to_bits());
    let fixed_dt = time.map_or(1.0 / 60.0, |t| 1.0 / t.hz.max(1.0) as f32);
    let mut previous = Vec::with_capacity(items.len());
    let mut list = Vec::with_capacity(items.len());
    let mut per_fluid: Vec<(u32, Vec<MovingSolid>)> = Vec::new();
    let mut skipped = 0u32;
    for (entity, link, body, collider, transform, pose, velocity) in items {
        let (position, rotation) = match (body.kind, pose) {
            (BodyKind::Dynamic, Some(pose)) => (pose.translation, pose.rotation),
            _ => (transform.translation, transform.rotation),
        };
        let motion = if body.kind == BodyKind::Dynamic {
            SolidMotion::Dynamic
        } else {
            SolidMotion::Kinematic
        };
        let (linear, angular) = match body.kind {
            BodyKind::Dynamic => velocity.map_or((Vec3::ZERO, Vec3::ZERO), |v| (v.linear, v.angular)),
            BodyKind::Kinematic => {
                let dt = fluids.get(link.fluid).map_or(fixed_dt, |e| e.sim.params().dt);
                let velocities = match coupling.previous.iter().find(|(e, ..)| *e == entity) {
                    Some((_, p0, q0, w0)) if dt > 0.0 => {
                        // The software log map: platform `atan2`/`sqrt` would fork the spin (and
                        // everything the solid stirs) across platforms (ADR-4753).
                        let turn = crate::math::quat_to_scaled_axis(crate::math::normalize_quat(
                            rotation * q0.inverse(),
                        ));
                        ((position - *p0) / dt, unwrap_turn(turn, *w0 * dt) / dt)
                    }
                    _ => (Vec3::ZERO, Vec3::ZERO),
                };
                previous.push((entity, position, rotation, velocities.1));
                velocities
            }
            BodyKind::Static => (Vec3::ZERO, Vec3::ZERO),
        };
        let solid = collider.and_then(solid_shape).map(|(shape, offset, turn)| {
            let center = position + rotation * offset;
            MovingSolid {
                shape,
                position: center,
                rotation: crate::math::normalize_quat(rotation * turn),
                // The velocity of the shape's centre.
                linear_velocity: linear + angular.cross(center - position),
                angular_velocity: angular,
                motion,
                tag: entity.to_bits(),
            }
        });
        let solid = solid.filter(MovingSolid::is_valid);
        if let Some(s) = solid {
            let slot = match per_fluid.iter().position(|(id, _)| *id == link.fluid) {
                Some(i) => i,
                None => {
                    per_fluid.push((link.fluid, Vec::new()));
                    per_fluid.len() - 1
                }
            };
            if per_fluid[slot].1.len() < MAX_SHAPES && fluids.get(link.fluid).is_some() {
                per_fluid[slot].1.push(s);
            } else {
                skipped += 1;
            }
        }
        list.push(CoupledBody {
            entity,
            fluid: link.fluid,
            motion,
            origin: position,
            solid,
            two_way: link.two_way,
            impulse: Vec3::ZERO,
            angular_impulse: Vec3::ZERO,
            simulated: pose.is_some(),
            applied: false,
            wetted_faces: 0,
        });
    }
    // Every simulation gets its list (an empty one when its last body left).
    let ids: Vec<u32> = fluids.entries().iter().map(|e| e.id).collect();
    for id in ids {
        let solids = per_fluid
            .iter_mut()
            .find(|(f, _)| *f == id)
            .map(|(_, s)| std::mem::take(s))
            .unwrap_or_default();
        let Some(entry) = fluids.get_mut(id) else {
            continue;
        };
        if solids.is_empty() && entry.sim.moving_solids().is_empty() {
            continue;
        }
        if entry.sim.set_moving_solids(solids).is_err() {
            skipped += 1;
        }
    }
    coupling.bodies = list;
    coupling.skipped = skipped;
    coupling.previous = previous;
}

/// `linear` shortened to length `cap` when `cap > 0` and the impulse is longer.
///
/// Each component is squared in f64. An f32 `length_squared` overflows to infinity past about
/// 1e19 per component, and dividing by that infinity would scale a finite impulse to zero.
fn clamp_linear_impulse(linear: Vec3, cap: f32) -> Vec3 {
    // `cap > 0` is false for NaN. `cap <= 0` would treat that NaN as a real cap.
    if cap.partial_cmp(&0.0) != Some(std::cmp::Ordering::Greater) {
        return linear;
    }
    let sq = f64::from(linear.x) * f64::from(linear.x)
        + f64::from(linear.y) * f64::from(linear.y)
        + f64::from(linear.z) * f64::from(linear.z);
    let cap64 = f64::from(cap);
    if sq > cap64 * cap64 {
        let factor = cap64 / crate::math::sqrt_f64(sq);
        return Vec3::new(
            (f64::from(linear.x) * factor) as f32,
            (f64::from(linear.y) * factor) as f32,
            (f64::from(linear.z) * factor) as f32,
        );
    }
    linear
}

/// The turn `turn` (a rotation vector of at most half a turn, the shortest arc between two poses)
/// unwrapped to the equivalent rotation about the same axis nearest `predicted` (the last step's
/// spin times `dt`): a body spinning up past half a turn per step keeps the sign of its spin
/// instead of flipping to the shortest arc. Two poses alone cannot tell +270 degrees from -90
/// degrees; a spin that got there step by step can. Only correctly rounded operations, so the
/// result is the same on every platform (ADR-4753).
fn unwrap_turn(turn: Vec3, predicted: Vec3) -> Vec3 {
    let angle = crate::math::sqrt_f32(turn.length_squared());
    if angle <= 0.0 || !predicted.is_finite() {
        return turn;
    }
    let axis = turn / angle;
    let tau = std::f32::consts::TAU;
    let k = ((axis.dot(predicted) - angle) / tau).round();
    if k == 0.0 || !k.is_finite() {
        return turn;
    }
    axis * (angle + k * tau)
}

/// Applies each two-way dynamic body's pressure impulse of the step as an `ExternalImpulse`.
///
/// The impulse is the one of this tick's fluid step: `gather_fluid_bodies` hands the solids over
/// with `set_moving_solids`, which drops the last step's impulses, so a paused simulation or one
/// playing a cache gives its bodies nothing.
pub fn apply_fluid_impulses(
    mut commands: Commands,
    fluids: Res<FluidWorld>,
    mut coupling: ResMut<FluidCoupling>,
    links: Query<&FluidBody>,
    mut impulses: Query<&mut ExternalImpulse>,
) {
    let _span = tracing::info_span!(target: "regia_fluid", "fluid_apply_impulses").entered();
    coupling.steps += 1;
    for body in &mut coupling.bodies {
        let Some(entry) = fluids.get(body.fluid) else {
            continue;
        };
        let tag = body.entity.to_bits();
        let Some(found) = entry.sim.solid_impulses().iter().find(|i| i.tag == tag) else {
            continue;
        };
        let cap = links.get(body.entity).map_or(0.0, |l| l.max_impulse);
        let linear = clamp_linear_impulse(found.linear, cap);
        // The step's angular impulse is about the shape's centre; the body turns about its origin.
        let center = body.solid.map_or(body.origin, |s| s.position);
        let angular = found.angular + (center - body.origin).cross(linear);
        body.impulse = linear;
        body.angular_impulse = angular;
        body.wetted_faces = found.wetted_faces;
        if !(body.two_way && body.motion == SolidMotion::Dynamic && body.simulated) {
            continue;
        }
        if !linear.is_finite() || !angular.is_finite() {
            continue;
        }
        body.applied = true;
        if let Ok(mut existing) = impulses.get_mut(body.entity) {
            existing.impulse += linear;
            existing.torque_impulse += angular;
        } else {
            commands.entity(body.entity).insert(ExternalImpulse {
                impulse: linear,
                torque_impulse: angular,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A finite impulse past ~1e19 per component must shorten to `cap`, not collapse to zero.
    /// `length_squared` overflows that square to infinity in f32.
    #[test]
    fn large_finite_impulse_clamps_to_the_cap() {
        let v = Vec3::new(3.0, 4.0, 0.0);
        assert_eq!(clamp_linear_impulse(v, 10.0), v);
        let out = clamp_linear_impulse(Vec3::splat(1.0e20), 10.0);
        let len_sq = f64::from(out.x) * f64::from(out.x)
            + f64::from(out.y) * f64::from(out.y)
            + f64::from(out.z) * f64::from(out.z);
        assert!(
            (len_sq - 100.0).abs() < 1.0e-3,
            "clamped impulse {out:?} has length squared {len_sq}, want 100"
        );
        assert!(out.x > 0.0 && out.y > 0.0 && out.z > 0.0, "{out:?}");
        assert_eq!(clamp_linear_impulse(v, 0.0), v);
        assert_eq!(clamp_linear_impulse(v, -1.0), v);
        assert_eq!(clamp_linear_impulse(v, f32::NAN), v);
    }
}
