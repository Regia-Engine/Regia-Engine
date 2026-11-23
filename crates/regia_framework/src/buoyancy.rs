// SPDX-License-Identifier: MPL-2.0
//! The physics bridge of the floating bodies (spec 87 § 5.2, ADR-1828): [`BuoyancyPlugin`] turns
//! each dynamic body's [`Floater`] (`regia_terrain::water_body::float`) into its
//! [`regia_physics::ExternalForce`] every fixed step. It lives here because layer-2 systems
//! (terrain's water, physics) do not depend on each other (`docs/02-architecture.md` § 1).
//!
//! ```text
//! FixedUpdate: advance_water_clock → apply_buoyancy → PhysicsStep (sync, step, events)
//! ```
//!
//! The force is computed from the pose, velocities and center of mass the last step left in the
//! rapier world, against the world surface query ([`WaterQuery`], shore switch included).
//! The bridge owns the floater's `ExternalForce` while the body is in the water: a host force on
//! the same body is replaced (put it on a jointed body instead). The step the body leaves the
//! water (no sphere touches it), the bridge removes the force it wrote, so the body falls under
//! gravity alone; a dry body gets no `ExternalForce` from it. Each body is independent of the
//! others, so the iteration order does not change the result.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::convert::{from_rapier_pose, from_rapier_vec3};
use regia_physics::{BodyKind, ExternalForce, PhysicsStep, PhysicsWorld, RigidBody, RigidBodyHandle};

use regia_terrain::water_body::float::{FloatPose, Floater, FloaterState, float_forces};
use regia_terrain::water_body::plugin::advance_water_clock;
use regia_terrain::water_body::query::WaterQuery;

/// Adds [`apply_buoyancy`] to `FixedUpdate`, after the water clock and before the physics step.
/// Needs `regia_physics::PhysicsPlugin`; the water bodies come from any host (with or without
/// `regia_terrain::WaterPlugin`: without it the clock stays at tick 0).
#[derive(Default)]
pub struct BuoyancyPlugin;

impl Plugin for BuoyancyPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            apply_buoyancy.after(advance_water_clock).before(PhysicsStep),
        );
    }
}

/// Body query of [`apply_buoyancy`].
type FloaterQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Floater,
        &'static RigidBody,
        &'static RigidBodyHandle,
        Option<&'static mut ExternalForce>,
        Option<&'static mut FloaterState>,
    ),
>;

/// Writes every dynamic floater's buoyancy and drag as its `ExternalForce` and its
/// [`FloaterState`] (span `water_buoyancy`); removes that `ExternalForce` the step the body
/// leaves the water.
pub fn apply_buoyancy(
    mut commands: Commands,
    physics: Res<PhysicsWorld>,
    gravity: Option<Res<regia_physics::Gravity>>,
    water: WaterQuery,
    mut floaters: FloaterQuery,
) {
    let _span = tracing::debug_span!("water_buoyancy").entered();
    let g = gravity.map_or(9.81, |g| -g.0.y);
    let tick = water.clock().tick;
    for (entity, floater, body, handle, force, state) in &mut floaters {
        if body.kind != BodyKind::Dynamic {
            continue;
        }
        let Some(rb) = physics.bodies.get(handle.to_handle()) else {
            continue;
        };
        let (position, rotation) = from_rapier_pose(rb.position());
        let pose = FloatPose {
            position,
            rotation,
            center_of_mass: from_rapier_vec3(rb.center_of_mass()),
            linear_velocity: from_rapier_vec3(rb.linvel()),
            angular_velocity: from_rapier_vec3(rb.angvel()),
        };
        let out = float_forces(floater, &pose, |x, z| water.surface(x, z).map(|(_, s)| s), g);
        // Spec 87 § 5.2: the bridge removes the force it wrote when the body leaves the water.
        // `was_wet` means the last step wrote a wet force, so a force a host puts on a body that
        // stays dry is left alone.
        let was_wet = state.as_ref().is_some_and(|s| s.forces.wet_points > 0);
        if out.wet_points == 0 {
            if was_wet && force.is_some() {
                commands.entity(entity).remove::<ExternalForce>();
            }
        } else {
            let next = ExternalForce {
                force: Vec3::from_array(out.force),
                torque: Vec3::from_array(out.torque),
            };
            match force {
                Some(mut current) => {
                    current.set_if_neq(next);
                }
                None => {
                    commands.entity(entity).insert(next);
                }
            }
        }
        let next_state = FloaterState { forces: out, tick };
        match state {
            Some(mut current) => *current = next_state,
            None => {
                commands.entity(entity).insert(next_state);
            }
        }
    }
}
