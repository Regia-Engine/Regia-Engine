// SPDX-License-Identifier: MPL-2.0
// Source: standard hover-vehicle control: one downward ray per thruster, a spring-damper push
// along the ground normal that holds the craft at a target height, a drive impulse along the
// chassis' forward axis, a yaw torque and a lateral-slip damper. Textbook rigid-body mechanics
// (impulse = force * dt, point-velocity v + w x r); no third-party code.

//! Hover vehicles: a dynamic chassis held above the ground by ray-cast thrusters (ADR-2301).
//!
//! A [`HoverCraft`] is a box chassis plus one thruster per [`HoverConfig::thrusters`] point. Each
//! fixed step, [`HoverCraft::update_with_layers`] casts every thruster's ray down the chassis'
//! −Y and pushes the chassis up along the ground normal. Each thruster's force is
//! `mass · (stiffness · compression − damping · along) / n` newtons, clamped to
//! `max_thruster_force` (`compression` is `height − distance`, `along` is the point's speed
//! along the normal, `n` is the thruster count). It then applies the drive,
//! brake, yaw and lateral-slip impulses, which only act while some thruster has ground. The
//! world step integrates them. Conventions are the wheeled vehicle's: forward is local −Z, right
//! +X, up +Y, [`VehicleInput::steer`] +1 turns right.
//!
//! Spring and damper rates are per kilogram of chassis (like the wheeled vehicle's), so a tuning
//! carries over when the mass changes. The craft's total spring force is `mass · stiffness ·
//! compression` however many thrusters share it, so it settles at `height − g / stiffness`
//! above the ground.
//!
//! The wheel rays' layer filter applies: [`HoverConfig::ground_layers`] and
//! [`HoverConfig::query_layer`]. Drive it in the ECS with [`spawn_hovercraft`] and
//! [`VehiclePlugin`](crate::vehicle::VehiclePlugin) (it reads the entity's [`VehicleInput`]).

use rapier3d::dynamics::{MassProperties, RigidBodyBuilder};
use rapier3d::geometry::{ColliderBuilder, Ray};
use rapier3d::pipeline::QueryFilter;
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_scene::Transform;
use regia_scene::layers::LayerParked;
use thiserror::Error;
use tracing::debug_span;

use crate::components::{ColliderHandle, RigidBodyHandle};
use crate::convert::{from_rapier_pose, from_rapier_vec3, to_rapier_pose, to_rapier_vec3};
use crate::layers::PhysicsLayers;
use crate::vehicle::{VehicleInput, checked_pose, ray_groups};
use crate::world::PhysicsWorld;

/// The most thrusters one craft may have.
pub const MAX_THRUSTERS: usize = 16;

/// The chassis' local forward axis.
const FORWARD: Vec3 = Vec3::NEG_Z;
/// The chassis' local up axis (thrusters point down −Y).
const UP: Vec3 = Vec3::Y;
/// The chassis' local right axis.
const RIGHT: Vec3 = Vec3::X;

/// Why a hover craft could not be created, updated or read. [`HoverError::code`] gives a stable
/// lowercase identifier.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HoverError {
    /// The configuration lists no thrusters.
    #[error("hover config has no thrusters")]
    NoThrusters,
    /// More than [`MAX_THRUSTERS`] thrusters.
    #[error("hover config has {count} thrusters; the most is {MAX_THRUSTERS}")]
    TooManyThrusters {
        /// How many the configuration listed.
        count: usize,
    },
    /// A field is out of range or not finite.
    #[error("hover config: `{field}` is out of range or not finite")]
    InvalidConfig {
        /// The offending [`HoverConfig`] field.
        field: &'static str,
    },
    /// A spawn pose has a non-finite translation or a degenerate rotation.
    #[error("hover pose must be a finite translation and a non-zero finite rotation")]
    InvalidPose,
    /// The time step is zero, negative or not finite.
    #[error("hover time step must be finite and positive")]
    InvalidTimeStep,
    /// The chassis is gone, not dynamic, or has no mass.
    #[error("the craft's chassis body is missing, not dynamic or massless")]
    ChassisMissing,
    /// The app has no `PhysicsWorld` resource.
    #[error("no PhysicsWorld in this app; add regia_physics::PhysicsPlugin")]
    NoPhysicsWorld,
}

impl HoverError {
    /// A stable identifier: lowercase ASCII, digits and `_`.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::NoThrusters => "hover_no_thrusters",
            Self::TooManyThrusters { .. } => "hover_too_many_thrusters",
            Self::InvalidConfig { .. } => "hover_invalid_config",
            Self::InvalidPose => "hover_invalid_pose",
            Self::InvalidTimeStep => "hover_invalid_time_step",
            Self::ChassisMissing => "hover_chassis_missing",
            Self::NoPhysicsWorld => "hover_no_physics_world",
        }
    }
}

/// A hover craft: chassis box, mass, thrusters and handling.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverConfig {
    /// Half extents of the chassis box, meters (each > 0).
    pub chassis_half_extents: Vec3,
    /// Chassis mass, kilograms (> 0).
    pub chassis_mass: f32,
    /// Center of mass in chassis space, meters.
    pub center_of_mass: Vec3,
    /// Thruster mount points in chassis space, meters; 1 to [`MAX_THRUSTERS`]. Four spread
    /// points also level the craft.
    pub thrusters: Vec<Vec3>,
    /// Target height of a thruster above the ground, meters (> 0). A ray reaches twice this.
    pub hover_height: f32,
    /// Spring rate per kilogram of chassis (1/s², ≥ 0).
    pub stiffness: f32,
    /// Damping on a thruster point's speed along the ground normal, per kilogram (1/s, ≥ 0).
    pub damping: f32,
    /// The most force one thruster applies, newtons (≥ 0).
    pub max_thruster_force: f32,
    /// Drive force along forward at full throttle, newtons (≥ 0).
    pub max_drive_force: f32,
    /// Brake force against the forward speed at full brake, newtons (≥ 0).
    pub max_brake_force: f32,
    /// Yaw torque at full steer, newton-meters (≥ 0).
    pub max_yaw_torque: f32,
    /// Rate at which sideways speed is removed, in 1/s (≥ 0). 0 is ice. One step removes
    /// `min(lateral_grip * dt, 1)` of the sideways speed, so cancelling it in a single step
    /// needs `lateral_grip * dt >= 1` (60 at 60 Hz). The default 3 removes 3/60 of the slide
    /// on a 60 Hz step.
    pub lateral_grip: f32,
    /// Rate at which angular velocity is removed while grounded, in 1/s (≥ 0). One step keeps
    /// `(1 - angular_damping * dt)` of it, clamped to 0..=1, so the default 2 removes 2/60 of
    /// the yaw on a 60 Hz step. The steady turn rate is `yaw torque / (inertia · damping)`.
    pub angular_damping: f32,
    /// Layers the thruster rays can hit (`1 << index` bitmask); `None` hits all.
    pub ground_layers: Option<u32>,
    /// The layer the rays belong to (a row of the query matrix); `None` for no layer.
    pub query_layer: Option<u16>,
}

impl Default for HoverConfig {
    /// A 600 kg craft, 2 m × 0.4 m × 3 m, four thrusters at the corners holding 1 m.
    fn default() -> Self {
        Self {
            chassis_half_extents: Vec3::new(1.0, 0.2, 1.5),
            chassis_mass: 600.0,
            center_of_mass: Vec3::new(0.0, -0.1, 0.0),
            thrusters: vec![
                Vec3::new(-0.9, 0.0, -1.3),
                Vec3::new(0.9, 0.0, -1.3),
                Vec3::new(-0.9, 0.0, 1.3),
                Vec3::new(0.9, 0.0, 1.3),
            ],
            hover_height: 1.0,
            stiffness: 40.0,
            damping: 9.0,
            max_thruster_force: 20_000.0,
            max_drive_force: 6000.0,
            max_brake_force: 8000.0,
            max_yaw_torque: 1500.0,
            lateral_grip: 3.0,
            angular_damping: 2.0,
            ground_layers: None,
            query_layer: None,
        }
    }
}

fn non_negative(v: f32) -> bool {
    v.is_finite() && v >= 0.0
}

fn positive(v: f32) -> bool {
    v.is_finite() && v > 0.0
}

impl HoverConfig {
    /// Checks every field; the first problem is reported.
    ///
    /// # Errors
    /// A [`HoverError`] naming the field, or the thruster count.
    pub fn validate(&self) -> Result<(), HoverError> {
        let half = self.chassis_half_extents;
        let checks: [(bool, &'static str); 12] = [
            (
                half.is_finite() && half.min_element() > 0.0,
                "chassis_half_extents",
            ),
            (positive(self.chassis_mass), "chassis_mass"),
            (self.center_of_mass.is_finite(), "center_of_mass"),
            (positive(self.hover_height), "hover_height"),
            (non_negative(self.stiffness), "stiffness"),
            (non_negative(self.damping), "damping"),
            (non_negative(self.max_thruster_force), "max_thruster_force"),
            (non_negative(self.max_drive_force), "max_drive_force"),
            (non_negative(self.max_brake_force), "max_brake_force"),
            (non_negative(self.max_yaw_torque), "max_yaw_torque"),
            (non_negative(self.lateral_grip), "lateral_grip"),
            (non_negative(self.angular_damping), "angular_damping"),
        ];
        if let Some((_, field)) = checks.into_iter().find(|(ok, _)| !ok) {
            return Err(HoverError::InvalidConfig { field });
        }
        if self
            .query_layer
            .is_some_and(|layer| usize::from(layer) >= crate::layers::MAX_LAYERS)
        {
            return Err(HoverError::InvalidConfig { field: "query_layer" });
        }
        if self.thrusters.is_empty() {
            return Err(HoverError::NoThrusters);
        }
        if self.thrusters.len() > MAX_THRUSTERS {
            return Err(HoverError::TooManyThrusters {
                count: self.thrusters.len(),
            });
        }
        if self.thrusters.iter().any(|p| !p.is_finite()) {
            return Err(HoverError::InvalidConfig { field: "thrusters" });
        }
        Ok(())
    }
}

/// One thruster after the last update.
#[derive(Debug, Clone, PartialEq)]
pub struct ThrusterState {
    /// Index in [`HoverConfig::thrusters`].
    pub index: usize,
    /// The ray found ground within its reach.
    pub grounded: bool,
    /// Distance from the mount point to the ground along the ray, meters; the ray's full reach
    /// when there is none.
    pub distance: f32,
    /// How far the spring is compressed below the target height, meters (0 when not).
    pub compression: f32,
    /// Force this thruster applied, newtons.
    pub force: f32,
    /// The ground collider's world position of the hit, if grounded.
    pub contact_point: Option<Vec3>,
}

/// The craft's pose, velocity and thrusters.
#[derive(Debug, Clone, PartialEq)]
pub struct HoverState {
    /// Chassis translation, world meters.
    pub translation: Vec3,
    /// Chassis rotation.
    pub rotation: Quat,
    /// World forward axis.
    pub forward: Vec3,
    /// Linear velocity, m/s.
    pub linear_velocity: Vec3,
    /// Speed along forward, m/s.
    pub forward_speed: f32,
    /// Speed along the chassis' right, m/s.
    pub lateral_speed: f32,
    /// Thrusters with ground in reach.
    pub grounded_thrusters: usize,
    /// Every thruster.
    pub thrusters: Vec<ThrusterState>,
}

/// A hover craft in a [`PhysicsWorld`].
#[derive(Debug, Clone)]
pub struct HoverCraft {
    config: HoverConfig,
    chassis: RigidBodyHandle,
    chassis_collider: Option<rapier3d::geometry::ColliderHandle>,
    last: Vec<ThrusterState>,
    steps: u64,
}

impl HoverCraft {
    /// Creates the craft: a dynamic box chassis (never sleeps) at the given pose.
    ///
    /// # Errors
    /// An invalid configuration or pose; nothing is added to the world then.
    pub fn spawn(
        physics: &mut PhysicsWorld,
        config: HoverConfig,
        translation: Vec3,
        rotation: Quat,
    ) -> Result<Self, HoverError> {
        config.validate()?;
        let pose = checked_pose(translation, rotation).map_err(|_| HoverError::InvalidPose)?;
        let body = RigidBodyBuilder::dynamic()
            .pose(to_rapier_pose(pose.0, pose.1))
            .can_sleep(false)
            .build();
        let chassis = physics.bodies.insert(body);
        let half = config.chassis_half_extents;
        let sq = half * half;
        let inertia = Vec3::new(sq.y + sq.z, sq.x + sq.z, sq.x + sq.y) * (config.chassis_mass / 3.0);
        let mass = MassProperties::new(
            to_rapier_vec3(config.center_of_mass),
            config.chassis_mass,
            to_rapier_vec3(inertia),
        );
        let collider = ColliderBuilder::cuboid(half.x, half.y, half.z)
            .mass_properties(mass)
            .build();
        let handle = {
            let PhysicsWorld {
                bodies, colliders, ..
            } = physics;
            colliders.insert_with_parent(collider, chassis, bodies)
        };
        Ok(Self {
            last: Vec::new(),
            config,
            chassis: RigidBodyHandle::from_rapier(chassis),
            chassis_collider: Some(handle),
            steps: 0,
        })
    }

    /// The chassis body.
    #[must_use]
    pub fn chassis(&self) -> RigidBodyHandle {
        self.chassis
    }

    /// The chassis collider.
    #[must_use]
    pub fn chassis_collider(&self) -> Option<ColliderHandle> {
        self.chassis_collider.map(ColliderHandle::from_rapier)
    }

    /// The configuration.
    #[must_use]
    pub fn config(&self) -> &HoverConfig {
        &self.config
    }

    /// Fixed steps applied so far.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Re-expresses the thrusters' last contact points after a floating-origin rebase (the chassis
    /// body moves with the rest of the physics world).
    pub fn shift_origin(&mut self, shift: Vec3) {
        for thruster in &mut self.last {
            if let Some(point) = &mut thruster.contact_point {
                *point -= shift;
            }
        }
    }

    /// Applies one step: thruster springs, then (with ground) drive, brake, yaw and lateral
    /// grip. Call once per fixed step before the world steps. `layers` is the matrix the rays'
    /// `query_layer` reads.
    ///
    /// # Errors
    /// A bad `dt` or a missing chassis; the world is unchanged then.
    pub fn update_with_layers(
        &mut self,
        physics: &mut PhysicsWorld,
        input: VehicleInput,
        dt: f32,
        layers: Option<&PhysicsLayers>,
    ) -> Result<(), HoverError> {
        if !(dt.is_finite() && dt > 0.0) {
            return Err(HoverError::InvalidTimeStep);
        }
        let handle = self.chassis.to_handle();
        let (pose, mass, linvel, angvel) = match physics.bodies.get(handle) {
            Some(b) if b.is_dynamic() && positive(b.mass()) => (
                *b.position(),
                b.mass(),
                from_rapier_vec3(b.linvel()),
                from_rapier_vec3(b.angvel()),
            ),
            _ => return Err(HoverError::ChassisMissing),
        };
        let input = input.sanitized();
        let (centre, rotation) = from_rapier_pose(&pose);
        let up = rotation * UP;
        let forward = rotation * FORWARD;
        let right = rotation * RIGHT;
        let com_world = centre + rotation * self.config.center_of_mass;
        let reach = self.config.hover_height * 2.0;
        let n = self.config.thrusters.len() as f32;

        // Cast the rays and gather the spring impulses first (the query borrows the world).
        let mut filter = QueryFilter::default()
            .exclude_sensors()
            .exclude_rigid_body(handle);
        if let Some(groups) = ray_groups(self.config.ground_layers, self.config.query_layer, layers) {
            filter = filter.groups(groups);
        }
        let mut states = Vec::with_capacity(self.config.thrusters.len());
        let mut impulses: Vec<(Vec3, Vec3)> = Vec::new();
        {
            let pipeline = physics.query_pipeline(filter);
            for (index, mount) in self.config.thrusters.iter().enumerate() {
                let origin = centre + rotation * *mount;
                let ray = Ray::new(to_rapier_vec3(origin), to_rapier_vec3(-up));
                let hit = pipeline.cast_ray_and_get_normal(&ray, reach, true);
                let Some((_, hit)) = hit else {
                    states.push(ThrusterState {
                        index,
                        grounded: false,
                        distance: reach,
                        compression: 0.0,
                        force: 0.0,
                        contact_point: None,
                    });
                    continue;
                };
                let distance = hit.time_of_impact;
                let normal = from_rapier_vec3(hit.normal).normalize_or_zero();
                let normal = if normal == Vec3::ZERO { up } else { normal };
                let compression = (self.config.hover_height - distance).max(0.0);
                let point_velocity = linvel + angvel.cross(origin - com_world);
                let along = point_velocity.dot(normal);
                let force = (mass * (self.config.stiffness * compression - self.config.damping * along) / n)
                    .clamp(0.0, self.config.max_thruster_force);
                impulses.push((origin, normal * (force * dt)));
                states.push(ThrusterState {
                    index,
                    grounded: true,
                    distance,
                    compression,
                    force,
                    contact_point: Some(origin - up * distance),
                });
            }
        }
        let grounded = !impulses.is_empty();
        let Some(body) = physics.bodies.get_mut(handle) else {
            return Err(HoverError::ChassisMissing);
        };
        if body.is_sleeping() {
            body.wake_up(true);
        }
        for (point, impulse) in impulses {
            body.apply_impulse_at_point(to_rapier_vec3(impulse), to_rapier_vec3(point), true);
        }
        if grounded {
            let forward_speed = linvel.dot(forward);
            let lateral_speed = linvel.dot(right);
            let drive = input.throttle * self.config.max_drive_force * dt;
            // The brake opposes the forward speed and never reverses it within a step.
            let brake_cap = mass * forward_speed.abs();
            let brake =
                (input.brake * self.config.max_brake_force * dt).min(brake_cap) * -forward_speed.signum();
            let grip_cap = mass * lateral_speed.abs();
            let grip = (mass * lateral_speed.abs() * self.config.lateral_grip * dt).min(grip_cap)
                * -lateral_speed.signum();
            let linear = forward * (drive + brake) + right * grip;
            body.apply_impulse(to_rapier_vec3(linear), true);
            let yaw = -input.steer * self.config.max_yaw_torque * dt;
            body.apply_torque_impulse(to_rapier_vec3(up * yaw), true);
            let keep = (1.0 - self.config.angular_damping * dt).clamp(0.0, 1.0);
            let w = body.angvel();
            body.set_angvel(w * keep, true);
        }
        self.last = states;
        self.steps += 1;
        Ok(())
    }

    /// The pose, velocity and every thruster after the last update.
    ///
    /// # Errors
    /// [`HoverError::ChassisMissing`] when the body is gone.
    pub fn state(&self, physics: &PhysicsWorld) -> Result<HoverState, HoverError> {
        let Some(body) = physics.bodies.get(self.chassis.to_handle()) else {
            return Err(HoverError::ChassisMissing);
        };
        let (translation, rotation) = from_rapier_pose(body.position());
        let forward = rotation * FORWARD;
        let linear_velocity = from_rapier_vec3(body.linvel());
        let thrusters = if self.last.is_empty() {
            (0..self.config.thrusters.len())
                .map(|index| ThrusterState {
                    index,
                    grounded: false,
                    distance: self.config.hover_height * 2.0,
                    compression: 0.0,
                    force: 0.0,
                    contact_point: None,
                })
                .collect()
        } else {
            self.last.clone()
        };
        Ok(HoverState {
            translation,
            rotation,
            forward,
            linear_velocity,
            forward_speed: linear_velocity.dot(forward),
            lateral_speed: linear_velocity.dot(rotation * RIGHT),
            grounded_thrusters: thrusters.iter().filter(|t| t.grounded).count(),
            thrusters,
        })
    }

    /// Removes the chassis from the world; `false` when it was already gone.
    pub fn remove(self, physics: &mut PhysicsWorld) -> bool {
        let handle = self.chassis.to_handle();
        if !physics.bodies.contains(handle) {
            return false;
        }
        physics.remove_body(handle);
        true
    }
}

/// A hover craft on an ECS entity, driven by [`VehiclePlugin`](crate::vehicle::VehiclePlugin)
/// from the entity's [`VehicleInput`].
#[derive(Component, Debug, Clone)]
pub struct HoverController {
    /// The craft.
    pub craft: HoverCraft,
    /// The error from the last fixed-step update, `None` when it succeeded.
    pub last_error: Option<HoverError>,
}

/// Creates a hover craft in the app's `PhysicsWorld` and an entity that owns it (chassis body and
/// collider handles, a `Transform`, a [`HoverController`] and a released [`VehicleInput`]).
///
/// # Errors
/// [`HoverError::NoPhysicsWorld`] without a world; a configuration or pose error. Nothing is
/// spawned on error.
pub fn spawn_hovercraft(
    world: &mut World,
    config: HoverConfig,
    translation: Vec3,
    rotation: Quat,
) -> Result<Entity, HoverError> {
    let craft = {
        let Some(mut physics) = world.get_resource_mut::<PhysicsWorld>() else {
            return Err(HoverError::NoPhysicsWorld);
        };
        HoverCraft::spawn(&mut physics, config, translation, rotation)?
    };
    let chassis = craft.chassis();
    let collider = craft.chassis_collider;
    let pose = world.get_resource::<PhysicsWorld>().and_then(|p| {
        p.bodies
            .get(chassis.to_handle())
            .map(|b| from_rapier_pose(b.position()))
    });
    let (translation, rotation) = pose.unwrap_or((translation, rotation));
    let mut entity = world.spawn((
        chassis,
        Transform {
            translation,
            rotation,
            scale: Vec3::ONE,
        },
        HoverController {
            craft,
            last_error: None,
        },
        VehicleInput::default(),
    ));
    if let Some(collider) = collider {
        entity.insert(ColliderHandle::from_rapier(collider));
    }
    let entity = entity.id();
    if let Some(mut physics) = world.get_resource_mut::<PhysicsWorld>() {
        physics.register_body(chassis.to_handle(), entity);
        if let Some(collider) = collider {
            physics.register_collider(collider, entity);
        }
    }
    Ok(entity)
}

/// Updates every [`HoverController`] from its entity's [`VehicleInput`], in entity spawn order. A
/// craft of a parked data layer is not driven (as cars are not).
pub fn drive_hovercraft(
    fixed: Option<Res<FixedTime>>,
    physics: Option<ResMut<PhysicsWorld>>,
    layers: Option<Res<PhysicsLayers>>,
    mut craft: Query<(Entity, &mut HoverController, Option<&VehicleInput>), Without<LayerParked>>,
) {
    let (Some(fixed), Some(mut physics)) = (fixed, physics) else {
        return;
    };
    let _span = debug_span!("drive_hovercraft").entered();
    let dt = fixed.step().as_secs_f32();
    let mut order: Vec<Entity> = craft.iter().map(|(entity, _, _)| entity).collect();
    order.sort_by_key(|entity| crate::step::entity_order(*entity));
    for entity in order {
        let Ok((_, mut controller, input)) = craft.get_mut(entity) else {
            continue;
        };
        let input = input.copied().unwrap_or_default();
        let error = controller
            .craft
            .update_with_layers(&mut physics, input, dt, layers.as_deref())
            .err();
        if controller.last_error != error {
            controller.last_error = error;
        }
    }
}
