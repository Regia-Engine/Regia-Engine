// SPDX-License-Identifier: MPL-2.0
//! FixedUpdate physics sync and step (specs/15 § 1).
//!
//! The chain (all in `FixedUpdate`, in this order):
//!
//! 0. [`sync_parked`] — entities of a parked data layer leave the rapier world (spec 37 § 13).
//! 1. [`sync_removed`] — rapier objects for removed/despawned entities and components.
//! 2. [`sync_added_bodies`]/[`sync_added_colliders`] — rapier objects for new components.
//! 3. [`sync_joints`](crate::joints::sync_joints) — joints between bodies.
//! 4. [`sync_body_properties`] — changed `RigidBody`/`Mass` fields, teleports.
//! 5. [`sync_kinematic_poses`] — Kinematic `Transform`s pushed as next poses.
//! 6. [`sync_velocity_and_forces`] — `Velocity`, `ExternalForce`, `ExternalImpulse`.
//! 7. [`step_physics`] — `PhysicsPipeline::step` with the event bridge, pose/velocity
//!    write-back, debris lifetime (M14).
//! 8. [`check_joint_breaks`](crate::joints::check_joint_breaks) — `break_force`.
//! 9. [`flush_physics_events`] — buffered rapier events → ECS messages.
//!
//! [`interpolate_transforms`](crate::sync::interpolate_transforms) runs in
//! `PostUpdate/PhysicsSync` and blends the rendered `Transform` between the two most
//! recent fixed-step poses (`FixedTime::overstep_fraction`, spec 15 § 1).
//!
//! Provenance: rapier3d public API (Apache-2.0), rapier.rs/docs — rigid-body/collider
//! builders and setters, the step pipeline; the sync layout is Regia Engine's per the spec.

use rapier3d::dynamics::{
    IntegrationParameters, MassProperties, RigidBodyActivation, RigidBodyBuilder, RigidBodyType,
};
use rapier3d::geometry::SharedShape;
use rapier3d::pipeline::ActiveEvents;
use regia_app::FixedTime;
use regia_core::math::{Affine3A, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_ecs::spawn_order::{SpawnKey, SpawnOrder};
use regia_scene::layers::LayerParked;
use regia_scene::level::LevelSettings;
use regia_scene::{GlobalTransform, Transform};
use tracing::{debug_span, instrument};

use crate::components::{
    BodyKind, Collider as ColliderComp, ColliderHandle, ColliderShape, DebrisTag, ExternalForce,
    ExternalImpulse, Joint, JointHandle, Mass, PhysicsPose, PreviousPhysicsPose, RigidBody as BodyComp,
    RigidBodyHandle, Teleport, Velocity,
};
use crate::convert::{from_rapier_pose, to_rapier_pose, to_rapier_rotation, to_rapier_vec3};
use crate::events::{CollisionEnded, CollisionStarted, ContactForce, JointBroken, TriggerEnter, TriggerExit};
use crate::hover::HoverController;
use crate::layers::PhysicsLayers;
use crate::vehicle::VehicleController;
use crate::world::{BodyBudget, Gravity, PhysicsSettings, PhysicsWorld};

// ---------------------------------------------------------------------------
// Shape conversion (spec 15 § 2 ColliderShape)
// ---------------------------------------------------------------------------

/// Converts a [`ColliderShape`] to a rapier [`SharedShape`]. Returns `None` if the shape is
/// empty or invalid.
///
/// Capsule/cylinder/cone are built +Y-aligned; pair with [`shape_axis_rotation`] to place
/// them on their configured axis.
///
/// Provenance: rapier3d public API (Apache-2.0), rapier.rs/docs.
pub fn shape_to_shared_shape(shape: &ColliderShape) -> Option<SharedShape> {
    if !shape_params_valid(shape) {
        return None;
    }
    match shape {
        ColliderShape::Box { half_extents } => Some(SharedShape::cuboid(
            half_extents.x,
            half_extents.y,
            half_extents.z,
        )),
        ColliderShape::Sphere { radius } => Some(SharedShape::ball(*radius)),
        ColliderShape::Capsule {
            half_height, radius, ..
        } => Some(SharedShape::capsule_y(*half_height, *radius)),
        ColliderShape::Cylinder {
            half_height, radius, ..
        } => Some(SharedShape::cylinder(*half_height, *radius)),
        ColliderShape::Cone {
            half_height, radius, ..
        } => Some(SharedShape::cone(*half_height, *radius)),
        ColliderShape::ConvexHull { points } => {
            if points.len() < 3 {
                return None;
            }
            let pts: Vec<rapier3d::math::Vector> = points.iter().map(|p| to_rapier_vec3(*p)).collect();
            SharedShape::convex_hull(&pts)
        }
        ColliderShape::TriMesh { vertices, indices } => {
            let verts: Vec<rapier3d::math::Vector> = vertices.iter().map(|p| to_rapier_vec3(*p)).collect();
            if verts.len() < 3 || indices.is_empty() {
                return None;
            }
            // Parry builds the BVH before validating, so an out-of-range index would panic.
            if indices
                .iter()
                .any(|tri| tri.iter().any(|&i| i as usize >= verts.len()))
            {
                tracing::warn!("trimesh collider has an index past its vertex buffer");
                return None;
            }
            match SharedShape::trimesh(verts, indices.clone()) {
                Ok(shape) => Some(shape),
                Err(error) => {
                    tracing::warn!(target: "regia_physics", %error, "invalid trimesh collider");
                    None
                }
            }
        }
        ColliderShape::HeightField {
            nx,
            nz,
            heights,
            scale,
        } => {
            if *nx < 2 || *nz < 2 || nx.checked_mul(*nz) != Some(heights.len()) {
                return None;
            }
            if heights.iter().all(|h| h.is_finite()) {
                let array2 = rapier3d::geometry::Array2::new(*nx, *nz, heights.clone());
                return Some(SharedShape::heightfield(array2, to_rapier_vec3(*scale)));
            }
            Some(SharedShape::new(holed_heightfield(*nx, *nz, heights, *scale)?))
        }
        ColliderShape::Compound { shapes } => {
            if shapes.is_empty() {
                return None;
            }
            let mut rapier_shapes = Vec::with_capacity(shapes.len());
            for (index, (offset, child)) in shapes.iter().enumerate() {
                // Parry panics on nested composites (compound or trimesh children).
                if matches!(
                    child,
                    ColliderShape::Compound { .. } | ColliderShape::TriMesh { .. }
                ) {
                    tracing::warn!(index, "compound collider child is a composite shape; not allowed");
                    return None;
                }
                let Some(shared) = shape_to_shared_shape(child) else {
                    tracing::warn!(index, "compound collider child is invalid");
                    return None;
                };
                let pose = to_rapier_pose(offset.translation, offset.rotation * shape_axis_rotation(child));
                rapier_shapes.push((pose, shared));
            }
            Some(SharedShape::compound(rapier_shapes))
        }
    }
}

/// Whether a shape's numbers can build a collider: sizes finite and not negative (zero is a legal,
/// if useless, size), points and offsets finite, trimesh indices inside the vertex list. A
/// heightfield may hold NaN (a hole) but not infinity.
fn shape_params_valid(shape: &ColliderShape) -> bool {
    let size_ok = |values: &[f32]| values.iter().all(|v| v.is_finite() && *v >= 0.0);
    match shape {
        ColliderShape::Box { half_extents } => size_ok(&half_extents.to_array()),
        ColliderShape::Sphere { radius } => size_ok(&[*radius]),
        ColliderShape::Capsule {
            half_height, radius, ..
        }
        | ColliderShape::Cone {
            half_height, radius, ..
        } => size_ok(&[*half_height, *radius]),
        // Parry asserts `is_sign_positive` on a cylinder's sizes, which `-0.0` fails.
        ColliderShape::Cylinder {
            half_height, radius, ..
        } => size_ok(&[*half_height, *radius]) && half_height.is_sign_positive() && radius.is_sign_positive(),
        ColliderShape::ConvexHull { points } => points.iter().all(|p| p.is_finite()),
        ColliderShape::TriMesh { vertices, indices } => {
            vertices.iter().all(|p| p.is_finite())
                && indices.iter().flatten().all(|&i| (i as usize) < vertices.len())
        }
        ColliderShape::HeightField { heights, scale, .. } => {
            scale.is_finite() && heights.iter().all(|h| !h.is_infinite())
        }
        ColliderShape::Compound { shapes } => shapes.iter().all(|(offset, child)| {
            offset.translation.is_finite() && offset.rotation.is_finite() && shape_params_valid(child)
        }),
    }
}

/// The local rotation that turns a +Y-aligned shape into its configured axis.
pub fn shape_axis_rotation(shape: &ColliderShape) -> Quat {
    match shape {
        ColliderShape::Capsule { axis, .. }
        | ColliderShape::Cylinder { axis, .. }
        | ColliderShape::Cone { axis, .. } => axis.from_y_rotation(),
        _ => Quat::IDENTITY,
    }
}

// ---------------------------------------------------------------------------
// Helpers shared by the sync systems
// ---------------------------------------------------------------------------

/// The world pose of `transform`, composed with `parent`'s global transform when set.
fn world_pose(transform: &Transform, parent: Option<&GlobalTransform>) -> (Vec3, Quat) {
    let Some(parent) = parent else {
        return (transform.translation, transform.rotation);
    };
    let global = parent.mul_transform(transform).to_transform();
    (global.translation, global.rotation)
}

/// Whether a world pose is safe to hand rapier: every translation and rotation component
/// finite (ADR-1507, the body-pose half of ADR-383's "not done"). A non-finite pose is
/// dropped at this seam — rapier would spread it through contacts into every body it
/// touches — and counted in [`PhysicsWorld::non_finite_inputs`].
fn finite_pose(pose: &(Vec3, Quat)) -> bool {
    pose.0.is_finite() && pose.1.is_finite()
}

/// The identity pose a body falls back to when its spawn `Transform` was non-finite: the
/// body must still exist (colliders and joints reference its handle), but it starts parked
/// at the origin instead of poisoning the solver.
fn origin_pose() -> (Vec3, Quat) {
    (Vec3::ZERO, Quat::IDENTITY)
}

/// The pose a new body hands rapier, and whether a bad input was dropped (counted once,
/// ADR-1507). A non-finite pose parks the body at the origin; a finite rotation goes through
/// `solver_rotation`, so a non-unit quaternion is normalized and a zero one becomes the identity
/// at the body's translation. Rapier copies the pose bits and never renormalizes, so a skewed or
/// zero rotation would otherwise scale its colliders and inertia for the body's whole life (the
/// teleport and kinematic paths already guard this; corr-physics audit).
fn spawn_pose(pose: (Vec3, Quat)) -> ((Vec3, Quat), bool) {
    if !finite_pose(&pose) {
        return (origin_pose(), true);
    }
    match solver_rotation(pose.1) {
        Some(rotation) => ((pose.0, rotation), false),
        None => ((pose.0, Quat::IDENTITY), true),
    }
}

/// `q` scaled to unit length, or `None` when its length is zero or not finite (glam 0.33's `Quat`
/// has no `try_normalize`). It is `Quat::normalize` itself, checked afterwards: `Vec4::try_normalize`
/// multiplies by the reciprocal length and rounds differently, which moved the determinism pins.
fn unit_quat(q: Quat) -> Option<Quat> {
    let unit = q.normalize();
    unit.is_finite().then_some(unit)
}

/// A rotation rapier may take as a body's pose: unit length within rendering tolerance passes
/// bit for bit (renormalizing a unit quaternion would round it and move the determinism pins),
/// a finite non-unit one is normalized, and a zero or non-finite one is `None` (the caller drops
/// it and counts it, ADR-1507). Rapier assumes unit rotations: a zero quaternion is a zero
/// rotation matrix (audit-v035-numerics M9).
fn solver_rotation(q: Quat) -> Option<Quat> {
    if !q.is_finite() {
        return None;
    }
    if (q.length_squared() - 1.0).abs() <= 1.0e-3 {
        return Some(q);
    }
    unit_quat(q)
}

/// A world pose converted back to the entity's local `Transform` when it has a parent.
///
/// Physics bodies are recommended to be hierarchy roots (spec 15 § 1); parented bodies use
/// the parent's latest propagated `GlobalTransform` (one frame of latency at most). A scaled
/// parent leaves its inverse scale in the local matrix, so the rotation is decomposed out of it
/// rather than read as if the matrix were a pure rotation.
///
/// `None` when the parent is singular (a zero scale axis): its inverse is NaN, and writing that
/// into the body's local `Transform` would own it forever (audit v033b). The caller then keeps
/// the existing local `Transform`.
pub(crate) fn local_pose(world: (Vec3, Quat), parent: Option<&GlobalTransform>) -> Option<(Vec3, Quat)> {
    let Some(parent) = parent else {
        return Some(world);
    };
    let (pt, pr) = world;
    let local = parent.0.inverse() * Affine3A::from_translation(pt) * Affine3A::from_quat(pr);
    let (_, rotation, translation) = local.to_scale_rotation_translation();
    let rotation = unit_quat(rotation)?;
    (translation.is_finite() && rotation.is_finite()).then_some((translation, rotation))
}

/// An explicit mass the solver may take.
///
/// Parry stores `1/x` for every non-zero component and `0` for zero. A negative mass makes
/// `inv_mass` negative, so a separating contact accelerates the body into the contact, and a
/// zero principal moment stores inverse inertia `0`, so a torque about that axis does not turn
/// the body. The mass and each principal moment must be finite and strictly positive.
fn explicit_mass_usable(mass: f32, center_of_mass: Vec3, principal_inertia: Vec3) -> bool {
    mass.is_finite()
        && mass > 0.0
        && center_of_mass.is_finite()
        && principal_inertia.is_finite()
        && principal_inertia.x > 0.0
        && principal_inertia.y > 0.0
        && principal_inertia.z > 0.0
}

/// Builds a rapier body from the ECS components (spec 15 §§ 1–2).
///
/// Non-finite `damping` or `gravity_scale`, and an explicit `Mass` that is not finite and
/// strictly positive in its mass and principal moments, are substituted with the builder
/// defaults (0, 1, no extra mass) rather than handed to rapier; the returned count is the
/// number of substituted fields (counted once per field in
/// [`PhysicsWorld::non_finite_inputs`] by the caller, ADR-1507).
fn build_rapier_body(
    body: &BodyComp,
    mass: Option<&Mass>,
    pose: (Vec3, Quat),
) -> (rapier3d::dynamics::RigidBody, u32) {
    let mut non_finite = 0u32;
    let linear_damping = if body.linear_damping.is_finite() {
        body.linear_damping
    } else {
        non_finite += 1;
        0.0
    };
    let angular_damping = if body.angular_damping.is_finite() {
        body.angular_damping
    } else {
        non_finite += 1;
        0.0
    };
    let gravity_scale = if body.gravity_scale.is_finite() {
        body.gravity_scale
    } else {
        non_finite += 1;
        1.0
    };
    let body_type = match body.kind {
        BodyKind::Dynamic => RigidBodyType::Dynamic,
        BodyKind::Kinematic => RigidBodyType::KinematicPositionBased,
        BodyKind::Static => RigidBodyType::Fixed,
    };
    // `pose`, not `translation` + `rotation(axis-angle)`: the axis-angle vector goes through
    // `atan2` and rapier's `sin`/`cos`, whose last digit differs per platform libm, while the
    // pose copies the quaternion bits (spec 15 § 5, cross-platform lockstep).
    let mut builder = RigidBodyBuilder::new(body_type)
        .pose(to_rapier_pose(pose.0, pose.1))
        .linear_damping(linear_damping)
        .angular_damping(angular_damping)
        .gravity_scale(gravity_scale)
        .locked_axes(body.locked_axes.to_rapier())
        .dominance_group(body.dominance_group)
        .can_sleep(body.can_sleep);
    if body.ccd {
        builder = builder.ccd_enabled(true);
    }
    if let Some(Mass::Explicit {
        mass,
        center_of_mass,
        principal_inertia,
    }) = mass
    {
        if explicit_mass_usable(*mass, *center_of_mass, *principal_inertia) {
            // Rapier adds these on top of the collider-derived mass (see `Mass` docs).
            builder = builder.additional_mass_properties(MassProperties::new(
                to_rapier_vec3(*center_of_mass),
                *mass,
                to_rapier_vec3(*principal_inertia),
            ));
        } else {
            non_finite += 1;
        }
    }
    (builder.build(), non_finite)
}

/// A collider's material numbers as rapier may take them, and how many were substituted.
///
/// Density, friction and restitution must be finite and not negative (a NaN or infinite
/// density gives the body NaN mass properties, a negative one a negative mass); the contact
/// force threshold must not be NaN (infinity means "never"). An out-of-domain value becomes the
/// `Collider` default (ADR-1507, audit-v035-numerics H2). Valid values pass unchanged.
fn collider_material(collider: &ColliderComp) -> ([f32; 4], u32) {
    let defaults = ColliderComp::default();
    let mut substituted = 0u32;
    let mut pick = |value: f32, default: f32, ok: bool| {
        if ok {
            value
        } else {
            substituted += 1;
            default
        }
    };
    let in_domain = |v: f32| v.is_finite() && v >= 0.0;
    let material = [
        pick(collider.friction, defaults.friction, in_domain(collider.friction)),
        pick(
            collider.restitution,
            defaults.restitution,
            in_domain(collider.restitution),
        ),
        pick(collider.density, defaults.density, in_domain(collider.density)),
        pick(
            collider.contact_force_threshold,
            defaults.contact_force_threshold,
            !collider.contact_force_threshold.is_nan(),
        ),
    ];
    (material, substituted)
}

/// Builds a rapier collider builder from the ECS `Collider` component (spec 15 § 2).
///
/// `None` for an invalid shape. The second value counts the material numbers substituted with
/// defaults (see `collider_material`), for [`PhysicsWorld::non_finite_inputs`].
pub(crate) fn build_rapier_collider(
    collider: &ColliderComp,
    layers: &PhysicsLayers,
) -> Option<(rapier3d::geometry::ColliderBuilder, u32)> {
    let shared = shape_to_shared_shape(&collider.shape)?;
    let ([friction, restitution, density, contact_force_threshold], substituted) =
        collider_material(collider);
    let offset = to_rapier_pose(
        collider.offset.translation,
        collider.offset.rotation * shape_axis_rotation(&collider.shape),
    );
    let mut events = ActiveEvents::empty();
    if collider.active_events.collision {
        events |= ActiveEvents::COLLISION_EVENTS;
    }
    if collider.active_events.contact_force {
        events |= ActiveEvents::CONTACT_FORCE_EVENTS;
    }
    let groups = layers.interaction_groups(&collider.layer);
    Some((
        rapier3d::geometry::ColliderBuilder::new(shared)
            .position(offset)
            .sensor(collider.sensor)
            .friction(friction)
            .restitution(restitution)
            .density(density)
            .friction_combine_rule(collider.friction_combine.to_rapier())
            .restitution_combine_rule(collider.restitution_combine.to_rapier())
            .collision_groups(groups)
            .solver_groups(groups)
            .active_events(events)
            .contact_force_event_threshold(contact_force_threshold),
        substituted,
    ))
}

// ---------------------------------------------------------------------------
// ECS → rapier: additions
// ---------------------------------------------------------------------------

/// Query for new `RigidBody` components (spec 15 § 1 sync step 1).
type NewBodiesQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static BodyComp,
        Option<&'static Mass>,
        Option<&'static Transform>,
        Option<&'static ChildOf>,
    ),
    (Without<RigidBodyHandle>, Without<LayerParked>),
>;

/// Query for legacy M14 debris bodies (`BodyKind` without the new `RigidBody`).
type LegacyBodiesQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static BodyKind,
        Option<&'static Transform>,
        Option<&'static ChildOf>,
    ),
    (Without<RigidBodyHandle>, Without<BodyComp>, Without<LayerParked>),
>;

/// Sort key that puts entities in spawn (index) order. `Entity`'s own `Ord` is not index
/// order (the index is stored inverted), so sort on the index explicitly.
pub(crate) fn entity_order(entity: Entity) -> (u32, u64) {
    (entity.index().index(), entity.to_bits())
}

/// Sort key for rapier insertion: the order `C` arrived in the world ([`SpawnOrder`], kept by
/// the hooks `PhysicsPlugin` installs), then `entity_order`. Entity ids alone are not
/// repeatable: `Commands::spawn` reserves them when the command is recorded, so two spawning
/// systems running in parallel get them in thread-timing order (ADR-1347).
pub(crate) fn spawn_key<C: Component>(order: Option<&SpawnOrder<C>>, entity: Entity) -> SpawnKey {
    match order {
        Some(order) => order.key(entity),
        None => {
            let (index, bits) = entity_order(entity);
            (u64::MAX, index, bits)
        }
    }
}

/// Arrival order of `RigidBody` and legacy `BodyKind` (ADR-1347).
type BodySpawnOrders<'w> = (
    Option<Res<'w, SpawnOrder<BodyComp>>>,
    Option<Res<'w, SpawnOrder<BodyKind>>>,
);

/// Arrival order of `Collider` and legacy `ColliderShape` (ADR-1347).
type ColliderSpawnOrders<'w> = (
    Option<Res<'w, SpawnOrder<ColliderComp>>>,
    Option<Res<'w, SpawnOrder<ColliderShape>>>,
);

/// Creates rapier bodies for new `RigidBody` (and legacy M14 `BodyKind`) components.
#[instrument(skip_all, target = "regia_physics")]
pub fn sync_added_bodies(
    mut physics: ResMut<PhysicsWorld>,
    new_bodies: NewBodiesQuery,
    legacy_bodies: LegacyBodiesQuery,
    parents: Query<&'static GlobalTransform>,
    (body_order, legacy_order): BodySpawnOrders,
    mut commands: Commands,
) {
    let _span = debug_span!("sync_added_bodies").entered();
    // Non-finite inputs dropped in this pass (poses fallen back to the origin, body fields
    // substituted with defaults); added to the world's counter once the field borrows end.
    let mut non_finite = 0u64;
    {
        let PhysicsWorld {
            bodies,
            body_entities,
            body_handles,
            ..
        } = &mut *physics;

        // Insert in spawn order, not query (archetype) order: rapier hands out body handles in
        // insertion order and solves in handle order, so an archetype order that differs between
        // two runs (one more resource registered, a different startup order) otherwise changes
        // the simulation. Not entity-id order either: ids from parallel `Commands::spawn` calls
        // depend on thread timing (ADR-1347). Deterministic replays depend on this.
        let mut new_bodies: Vec<_> = new_bodies.iter().collect();
        new_bodies.sort_by_cached_key(|row| spawn_key(body_order.as_deref(), row.0));
        for (entity, body, mass, transform, parent) in new_bodies {
            let transform = transform.copied().unwrap_or_default();
            let parent_global = parent.and_then(|p| parents.get(p.parent()).ok());
            let pose = world_pose(&transform, parent_global);
            // A non-finite spawn pose parks the body at the origin (ADR-1507): the body must
            // still exist — colliders and joints reference its handle — but rapier never sees
            // the NaN. A zero or non-finite spawn rotation is dropped the same way (identity,
            // counted): rapier assumes unit rotations (a zero quaternion is a zero rotation
            // matrix, M9) and the kinematic/teleport seams already run `solver_rotation`; the
            // spawn seam must too (`spawn_pose`).
            let (pose, dropped) = spawn_pose(pose);
            non_finite += u64::from(dropped);
            let (rb, substituted) = build_rapier_body(body, mass, pose);
            non_finite += u64::from(substituted);
            let handle = bodies.insert(rb);
            body_entities.insert(handle, entity);
            body_handles.insert(entity, handle);
            commands
                .entity(entity)
                .insert(RigidBodyHandle::from_rapier(handle));
        }

        // Legacy M14 debris path: `BodyKind` + `Transform`.
        let mut legacy_bodies: Vec<_> = legacy_bodies.iter().collect();
        legacy_bodies.sort_by_cached_key(|row| spawn_key(legacy_order.as_deref(), row.0));
        for (entity, kind, transform, parent) in legacy_bodies {
            let transform = transform.copied().unwrap_or_default();
            let parent_global = parent.and_then(|p| parents.get(p.parent()).ok());
            let pose = world_pose(&transform, parent_global);
            // The origin-park guard of the `BodyComp` path above, at the legacy seam: non-finite
            // poses park at the origin, zero/non-finite rotations at the identity, and both count
            // (ADR-1507, M9).
            let (pose, dropped) = spawn_pose(pose);
            non_finite += u64::from(dropped);
            let body_type = match kind {
                BodyKind::Dynamic => RigidBodyType::Dynamic,
                BodyKind::Kinematic => RigidBodyType::KinematicPositionBased,
                BodyKind::Static => RigidBodyType::Fixed,
            };
            let rb = RigidBodyBuilder::new(body_type)
                .pose(to_rapier_pose(pose.0, pose.1))
                .can_sleep(true)
                .sleeping(false)
                .build();
            let handle = bodies.insert(rb);
            body_entities.insert(handle, entity);
            body_handles.insert(entity, handle);
            commands
                .entity(entity)
                .insert(RigidBodyHandle::from_rapier(handle));
        }
    }
    physics.non_finite_inputs += non_finite;
}

/// Query for new user `Collider` components.
type NewCollidersQuery<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static ColliderComp, Option<&'static RigidBodyHandle>),
    (
        Without<ColliderHandle>,
        Without<LayerParked>,
        // A tiled terrain is built tile by tile (`terrain_tiles`, ADR-2317).
        Without<regia_scene::streaming::TerrainTileCollision>,
    ),
>;

/// Query for legacy M14 debris colliders (`ColliderShape` on a body entity).
type LegacyCollidersQuery<'w, 's> = Query<
    'w,
    's,
    (Entity, &'static ColliderShape, &'static RigidBodyHandle),
    (Without<ColliderHandle>, Without<ColliderComp>),
>;

/// Creates rapier colliders for new `Collider` components, attaching child-entity colliders
/// to the nearest ancestor body (compound, spec 15 § 2).
#[instrument(skip_all, target = "regia_physics")]
#[allow(clippy::too_many_arguments)]
pub fn sync_added_colliders(
    mut physics: ResMut<PhysicsWorld>,
    layers: Res<PhysicsLayers>,
    new_colliders: NewCollidersQuery,
    legacy_colliders: LegacyCollidersQuery,
    handles: Query<&'static RigidBodyHandle>,
    child_of: Query<&'static ChildOf>,
    transforms: Query<&'static Transform>,
    (collider_order, legacy_order): ColliderSpawnOrders,
    mut commands: Commands,
) {
    let _span = debug_span!("sync_added_colliders").entered();
    let mut non_finite = 0u64;

    // Spawn order (see `sync_added_bodies`): collider handles and broad-phase order follow it.
    let mut new_colliders: Vec<_> = new_colliders.iter().collect();
    new_colliders.sort_by_cached_key(|row| spawn_key(collider_order.as_deref(), row.0));
    for (entity, collider, own_handle) in new_colliders {
        // Resolve the body: the entity's own handle, else the nearest ancestor's.
        let body_handle = match own_handle {
            Some(handle) => Some(handle.to_handle()),
            None => find_ancestor_body(entity, &handles, &child_of).map(|h| h.to_handle()),
        };
        let Some(body_handle) = body_handle else {
            tracing::debug!(
                target: "regia_physics",
                "collider on {entity:?} has no body in its ancestry; skipped"
            );
            continue;
        };

        // The collider's offset from the body: the user offset composed with the chain of
        // local transforms between the collider entity and the body entity (compound).
        let (offset, skipped_non_finite) =
            composed_collider_offset(entity, collider, &handles, &child_of, &transforms);
        non_finite += skipped_non_finite as u64;

        if is_dynamic_trimesh(&physics, body_handle, &collider.shape) {
            tracing::warn!(
                target: "regia_physics",
                "triangle-mesh collider on {entity:?} is attached to a dynamic body; skipped (static/kinematic only)"
            );
            continue;
        }

        let mut composed = collider.clone();
        composed.offset = offset;
        let Some((builder, substituted)) = build_rapier_collider(&composed, &layers) else {
            tracing::warn!(
                target: "regia_physics",
                "invalid collider shape on {entity:?}; skipped"
            );
            continue;
        };
        non_finite += u64::from(substituted);
        insert_collider(&mut physics, entity, body_handle, builder.build(), &mut commands);
    }

    // Legacy M14 path: `ColliderShape` directly on a body entity, default material.
    let mut legacy_colliders: Vec<_> = legacy_colliders.iter().collect();
    legacy_colliders.sort_by_cached_key(|row| spawn_key(legacy_order.as_deref(), row.0));
    for (entity, shape, body_handle) in legacy_colliders {
        let Some(shared) = shape_to_shared_shape(shape) else {
            continue;
        };
        let builder = rapier3d::geometry::ColliderBuilder::new(shared);
        insert_collider(
            &mut physics,
            entity,
            body_handle.to_handle(),
            builder.build(),
            &mut commands,
        );
    }
    physics.non_finite_inputs += non_finite;
}

/// The collider's offset from its body: the user offset composed with the chain of local
/// transforms between the collider entity and the body entity (compound colliders).
///
/// A non-finite local transform in the chain is skipped — that node composes like the
/// identity — instead of moving the collider to NaN, where it would poison every contact
/// it takes part in. The second return value reports a skip so the caller can count it
/// (ADR-1507).
fn composed_collider_offset(
    entity: Entity,
    collider: &ColliderComp,
    handles: &Query<&'static RigidBodyHandle>,
    child_of: &Query<&'static ChildOf>,
    transforms: &Query<&'static Transform>,
) -> (Transform, bool) {
    let mut offset = collider.offset;
    let mut skipped_non_finite = false;
    let mut current = Some(entity);
    let mut guard = 0;
    while let Some(node) = current {
        guard += 1;
        if guard > 64 || handles.get(node).is_ok() {
            break; // reached the body (or a hierarchy loop)
        }
        if let Ok(local) = transforms.get(node) {
            if local.translation.is_finite() && local.rotation.is_finite() {
                offset = Transform::from_affine(local.compute_affine() * offset.compute_affine());
            } else {
                skipped_non_finite = true;
            }
        }
        current = child_of.get(node).ok().map(|p| p.parent());
    }
    (offset, skipped_non_finite)
}

/// Whether `shape` is a triangle mesh and `body` is a dynamic body: spec 15 § 2 allows
/// triangle meshes on static and kinematic bodies only.
fn is_dynamic_trimesh(
    physics: &PhysicsWorld,
    body: rapier3d::dynamics::RigidBodyHandle,
    shape: &ColliderShape,
) -> bool {
    matches!(shape, ColliderShape::TriMesh { .. }) && physics.bodies.get(body).is_some_and(|b| b.is_dynamic())
}

/// Walks the `ChildOf` chain from `entity` (exclusive) up to the nearest ancestor with a
/// rapier body.
fn find_ancestor_body(
    entity: Entity,
    handles: &Query<&'static RigidBodyHandle>,
    child_of: &Query<&'static ChildOf>,
) -> Option<RigidBodyHandle> {
    let mut current = child_of.get(entity).ok().map(|p| p.parent());
    let mut guard = 0;
    while let Some(ancestor) = current {
        guard += 1;
        if guard > 64 {
            return None; // hierarchy loop guard
        }
        if let Ok(handle) = handles.get(ancestor) {
            return Some(*handle);
        }
        current = child_of.get(ancestor).ok().map(|p| p.parent());
    }
    None
}

/// Inserts a built collider against `body`, records the entity maps and handle component.
fn insert_collider(
    physics: &mut PhysicsWorld,
    entity: Entity,
    body: rapier3d::dynamics::RigidBodyHandle,
    collider: rapier3d::geometry::Collider,
    commands: &mut Commands,
) {
    let handle = {
        let PhysicsWorld {
            bodies, colliders, ..
        } = physics;
        colliders.insert_with_parent(collider, body, bodies)
    };
    physics.register_collider(handle, entity);
    commands
        .entity(entity)
        .insert(ColliderHandle::from_rapier(handle));
}

/// Pushes changed `Collider` fields (shape, offset, material, sensor, layer, events) by
/// rebuilding the rapier collider (spec 15 § 1).
#[instrument(skip_all, target = "regia_physics")]
#[allow(clippy::too_many_arguments)]
pub fn sync_collider_changes(
    mut physics: ResMut<PhysicsWorld>,
    layers: Res<PhysicsLayers>,
    changed: Query<(Entity, &'static ColliderComp, &'static ColliderHandle), Changed<ColliderComp>>,
    added: Query<(), Added<ColliderComp>>,
    handles: Query<&'static RigidBodyHandle>,
    child_of: Query<&'static ChildOf>,
    transforms: Query<&'static Transform>,
    mut commands: Commands,
) {
    let _span = debug_span!("sync_collider_changes").entered();
    let mut non_finite = 0u64;
    // Not archetype order, and not entity ids (thread timing under parallel `Commands::spawn`):
    // each rebuild hands out a fresh rapier handle, and the order decides the broad phase and
    // solve order of the rebuilt colliders. The existing handle order is the insertion order,
    // which follows spawn order (ADR-1347, ADR-2396).
    let mut changed: Vec<_> = changed.iter().collect();
    changed.sort_by_key(|row| row.2.to_handle().into_raw_parts());
    for (entity, collider, handle) in changed {
        // Fresh colliders were built by `sync_added_colliders` this tick; skip the rebuild.
        if added.contains(entity) {
            continue;
        }
        let handle = handle.to_handle();
        // The parent body must be read before the removal detaches the collider.
        let body = physics.colliders.get(handle).and_then(|c| c.parent());
        physics.remove_collider(handle);
        let Some(body) = body else {
            tracing::warn!(
                target: "regia_physics",
                "collider on {entity:?} lost its parent; removed"
            );
            commands.entity(entity).remove::<ColliderHandle>();
            continue;
        };
        if is_dynamic_trimesh(&physics, body, &collider.shape) {
            tracing::warn!(
                target: "regia_physics",
                "triangle-mesh collider on {entity:?} is attached to a dynamic body; collider removed"
            );
            commands.entity(entity).remove::<ColliderHandle>();
            continue;
        }
        let mut composed = collider.clone();
        let (offset, skipped_non_finite) =
            composed_collider_offset(entity, collider, &handles, &child_of, &transforms);
        non_finite += skipped_non_finite as u64;
        composed.offset = offset;
        let Some((builder, substituted)) = build_rapier_collider(&composed, &layers) else {
            tracing::warn!(
                target: "regia_physics",
                "invalid collider shape on {entity:?}; collider removed"
            );
            commands.entity(entity).remove::<ColliderHandle>();
            continue;
        };
        non_finite += u64::from(substituted);
        insert_collider(&mut physics, entity, body, builder.build(), &mut commands);
    }
    physics.non_finite_inputs += non_finite;
}

// ---------------------------------------------------------------------------
// ECS → rapier: parked data layers
// ---------------------------------------------------------------------------

/// Left on an entity whose rapier objects went away because it was parked, so the entity's
/// [`Velocity`] goes back into the rebuilt body when it is live again.
#[derive(Component, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ParkedPhysics;

/// Entities that leave the rapier world because they were parked. A vehicle or hover craft stays:
/// its controller owns the chassis handle and would not rebuild it.
type ParkingQuery<'w, 's> = Query<
    'w,
    's,
    (Entity, Option<&'static RigidBodyHandle>),
    (
        With<LayerParked>,
        Or<(With<RigidBodyHandle>, With<ColliderHandle>, With<BodyComp>)>,
        Without<VehicleController>,
        Without<HoverController>,
        Without<ParkedPhysics>,
    ),
>;

/// Entities back from a parked layer, with the velocity they left with.
type ResumingQuery<'w, 's> =
    Query<'w, 's, (Entity, Option<&'static mut Velocity>), (With<ParkedPhysics>, Without<LayerParked>)>;

/// Vehicle and hover chassis handles with whether their layer is parked: `sync_parked` disables
/// them instead of removing them.
type ParkedChassisQuery<'w, 's> = Query<
    'w,
    's,
    (&'static RigidBodyHandle, Has<LayerParked>),
    Or<(With<VehicleController>, With<HoverController>)>,
>;

/// Takes the entities of a parked data layer out of the rapier world and lets the add-sync
/// rebuild them once they are live (spec 37 § 13: parked rigid bodies are not in the physics
/// world). Removing the handle components goes through the usual removal records, so
/// [`sync_removed`], the next system of the chain, drops the rapier objects (and the joints on
/// them, which [`sync_joints`](crate::joints::sync_joints) rebuilds). Their components stay,
/// `NewBodiesQuery` and `NewCollidersQuery` skip them while parked. A resuming body starts
/// from its `Transform` and the [`Velocity`] it had; a body without that component gets one here,
/// holding its motion at the moment it left. A body parked before it ever reached rapier is marked
/// too, so the `Velocity` it was authored with still applies on its first live step.
///
/// A [`VehicleController`] or [`HoverController`] keeps its chassis, which is disabled instead:
/// the controller's suspension or thruster data is built around that handle and has no rebuild
/// path.
#[instrument(skip_all, target = "regia_physics")]
pub fn sync_parked(
    mut physics: ResMut<PhysicsWorld>,
    parking: ParkingQuery,
    mut resuming: ResumingQuery,
    vehicles: ParkedChassisQuery,
    mut commands: Commands,
) {
    let _span = tracing::debug_span!("sync_parked").entered();
    for (entity, handle) in &parking {
        // The motion leaves with the body, so it comes back even if nothing tracked it in ECS.
        if let Some(rb) = handle.and_then(|handle| physics.bodies.get(handle.to_handle())) {
            commands.entity(entity).insert_if_new(Velocity {
                linear: crate::convert::from_rapier_vec3(rb.linvel()),
                angular: crate::convert::from_rapier_vec3(rb.angvel()),
            });
        }
        commands
            .entity(entity)
            .remove::<(RigidBodyHandle, ColliderHandle, PhysicsPose, PreviousPhysicsPose)>()
            .insert(ParkedPhysics);
    }
    for (entity, velocity) in &mut resuming {
        if let Some(mut velocity) = velocity {
            velocity.set_changed();
        }
        commands.entity(entity).remove::<ParkedPhysics>();
    }
    for (handle, parked) in &vehicles {
        if let Some(rb) = physics.bodies.get_mut(handle.to_handle())
            && rb.is_enabled() == parked
        {
            rb.set_enabled(!parked);
        }
    }
}

// ---------------------------------------------------------------------------
// ECS → rapier: removals
// ---------------------------------------------------------------------------

/// Removes rapier objects for removed components and despawned entities (spec 15 § 1).
///
/// Works from the records the plugin's `Remove` observers keep in [`PhysicsWorld`] (see
/// `PendingRemovals`), not from `RemovedComponents`: those messages are cleared every frame,
/// and a frame without a fixed step would lose them. A body removed with its `RigidBody`
/// component also strips the `ColliderHandle` of every collider that went with it, so the
/// colliders are rebuilt if a body comes back.
#[instrument(skip_all, target = "regia_physics")]
pub fn sync_removed(
    mut physics: ResMut<PhysicsWorld>,
    live_handles: Query<&'static RigidBodyHandle>,
    live_collider_handles: Query<&'static ColliderHandle>,
    live_joint_handles: Query<(), With<JointHandle>>,
    mut commands: Commands,
) {
    let _span = debug_span!("sync_removed").entered();
    let mut pending = std::mem::take(&mut physics.pending_removals);
    // Marker removals are `sync_joints`' to apply (it runs after this system).
    physics.pending_removals.joint_no_contacts = std::mem::take(&mut pending.joint_no_contacts);

    // A handle component left its entity (despawn, or physics removed it): drop the rapier
    // object it named, unless that entity has been given a different one since.
    for (entity, handle) in pending.body_handles {
        if physics.body_handle_of(entity) == Some(handle) {
            let owners = physics.remove_body_listing_colliders(handle);
            strip_collider_handles(owners, &live_collider_handles, &mut commands);
        }
    }
    for (entity, handle) in pending.collider_handles {
        if physics.collider_handle_of(entity) == Some(handle) {
            physics.remove_collider(handle);
        }
    }
    // The user removed the component while the entity lives: drop the rapier object too.
    for entity in pending.bodies {
        if let Ok(handle) = live_handles.get(entity) {
            let owners = physics.remove_body_listing_colliders(handle.to_handle());
            strip_collider_handles(owners, &live_collider_handles, &mut commands);
            // The interpolation poses go too: the `Transform` belongs to gameplay again.
            commands
                .entity(entity)
                .remove::<(RigidBodyHandle, PhysicsPose, PreviousPhysicsPose)>();
        }
    }
    for entity in pending.colliders {
        if let Ok(handle) = live_collider_handles.get(entity) {
            physics.remove_collider(handle.to_handle());
            commands.entity(entity).remove::<ColliderHandle>();
        }
    }
    for entity in pending.joints {
        physics.remove_joint_of(entity);
        physics.broken_joints.remove(&entity);
        if live_joint_handles.get(entity).is_ok() {
            commands.entity(entity).remove::<JointHandle>();
        }
    }
    // A removed `ExternalForce` stops pushing (rapier keeps a user force until it is reset).
    for entity in pending.forces {
        if let Ok(handle) = live_handles.get(entity)
            && let Some(rb) = physics.bodies.get_mut(handle.to_handle())
        {
            rb.reset_forces(true);
            rb.reset_torques(true);
        }
    }
}

/// Strips the `ColliderHandle` of each live entity whose collider left with a body, so the
/// add-sync rebuilds the collider when a body is there again.
fn strip_collider_handles(
    owners: Vec<Entity>,
    live_collider_handles: &Query<&'static ColliderHandle>,
    commands: &mut Commands,
) {
    for owner in owners {
        if live_collider_handles.get(owner).is_ok() {
            commands.entity(owner).remove::<ColliderHandle>();
        }
    }
}

/// Observer: a `RigidBodyHandle` is leaving its entity (removal or despawn).
pub(crate) fn record_body_handle_removal(
    on: On<Remove<RigidBodyHandle>>,
    handles: Query<&'static RigidBodyHandle>,
    physics: Option<ResMut<PhysicsWorld>>,
) {
    let entity = on.event().entity;
    if let (Some(mut physics), Ok(handle)) = (physics, handles.get(entity)) {
        physics
            .pending_removals
            .body_handles
            .push((entity, handle.to_handle()));
    }
}

/// Observer: a `ColliderHandle` is leaving its entity (removal or despawn).
pub(crate) fn record_collider_handle_removal(
    on: On<Remove<ColliderHandle>>,
    handles: Query<&'static ColliderHandle>,
    physics: Option<ResMut<PhysicsWorld>>,
) {
    let entity = on.event().entity;
    if let (Some(mut physics), Ok(handle)) = (physics, handles.get(entity)) {
        physics
            .pending_removals
            .collider_handles
            .push((entity, handle.to_handle()));
    }
}

/// Observer: a `RigidBody` component is leaving its entity.
pub(crate) fn record_body_removal(on: On<Remove<BodyComp>>, physics: Option<ResMut<PhysicsWorld>>) {
    if let Some(mut physics) = physics {
        physics.pending_removals.bodies.push(on.event().entity);
    }
}

/// Observer: a `Collider` component is leaving its entity.
pub(crate) fn record_collider_removal(on: On<Remove<ColliderComp>>, physics: Option<ResMut<PhysicsWorld>>) {
    if let Some(mut physics) = physics {
        physics.pending_removals.colliders.push(on.event().entity);
    }
}

/// Observer: a `Joint` component is leaving its entity.
pub(crate) fn record_joint_removal(on: On<Remove<Joint>>, physics: Option<ResMut<PhysicsWorld>>) {
    if let Some(mut physics) = physics {
        physics.pending_removals.joints.push(on.event().entity);
    }
}

/// Observer: a `JointNoContacts` marker is leaving its entity. Recorded here because
/// `sync_joints` runs on the fixed step and `RemovedComponents` loses a removal made on a frame
/// without one (audit 11-03 ecs H1, ADR-2396).
pub(crate) fn record_joint_no_contacts_removal(
    on: On<Remove<crate::components::JointNoContacts>>,
    physics: Option<ResMut<PhysicsWorld>>,
) {
    if let Some(mut physics) = physics {
        physics.pending_removals.joint_no_contacts.push(on.event().entity);
    }
}

/// Observer: an `ExternalForce` component is leaving its entity.
pub(crate) fn record_force_removal(on: On<Remove<ExternalForce>>, physics: Option<ResMut<PhysicsWorld>>) {
    if let Some(mut physics) = physics {
        physics.pending_removals.forces.push(on.event().entity);
    }
}

// ---------------------------------------------------------------------------
// ECS → rapier: property changes, kinematic poses, velocity and forces
// ---------------------------------------------------------------------------

/// Query for bodies whose `RigidBody`/`Mass` components changed.
type ChangedBodiesQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static RigidBodyHandle,
        &'static BodyComp,
        Option<&'static Mass>,
    ),
    Or<(Changed<BodyComp>, Changed<Mass>)>,
>;

/// Pushes changed `RigidBody`/`Mass` fields and applies `Teleport` markers (spec 15 § 1).
#[instrument(skip_all, target = "regia_physics")]
pub fn sync_body_properties(
    mut physics: ResMut<PhysicsWorld>,
    changed: ChangedBodiesQuery,
    teleports: Query<
        (
            Entity,
            &'static RigidBodyHandle,
            &'static Transform,
            Option<&'static ChildOf>,
        ),
        With<Teleport>,
    >,
    parents: Query<&'static GlobalTransform>,
    mut commands: Commands,
) {
    let _span = debug_span!("sync_body_properties").entered();
    // Non-finite field pushes dropped in this pass, added to the world's counter after the
    // rapier body borrows end.
    let mut non_finite = 0u64;

    for (entity, handle, body, mass) in &changed {
        let Some(rb) = physics.bodies.get_mut(handle.to_handle()) else {
            continue;
        };
        // A kind change at runtime (a ragdoll going limp, a prop picked up) switches the
        // rapier body type in place; the body keeps its colliders, joints and pose.
        let kind = match body.kind {
            BodyKind::Dynamic => RigidBodyType::Dynamic,
            BodyKind::Kinematic => RigidBodyType::KinematicPositionBased,
            BodyKind::Static => RigidBodyType::Fixed,
        };
        if rb.body_type() != kind {
            rb.set_body_type(kind, true);
            if kind != RigidBodyType::Dynamic {
                // Only dynamic bodies are interpolated: stale poses would make
                // `interpolate_transforms` overwrite the gameplay-driven `Transform` of a
                // kinematic or static body every frame. Becoming dynamic again re-pins them.
                commands
                    .entity(entity)
                    .remove::<(PhysicsPose, PreviousPhysicsPose)>();
            }
        }
        if rb.body_type() == RigidBodyType::Dynamic {
            // Non-finite values are not pushed: the body keeps its last valid damping and
            // gravity scale instead of handing rapier a NaN (ADR-1507).
            if body.linear_damping.is_finite() {
                rb.set_linear_damping(body.linear_damping);
            } else {
                non_finite += 1;
            }
            if body.angular_damping.is_finite() {
                rb.set_angular_damping(body.angular_damping);
            } else {
                non_finite += 1;
            }
            if body.gravity_scale.is_finite() {
                rb.set_gravity_scale(body.gravity_scale, true);
            } else {
                non_finite += 1;
            }
            rb.enable_ccd(body.ccd);
            rb.set_locked_axes(body.locked_axes.to_rapier(), true);
            rb.set_dominance_group(body.dominance_group);
            if !body.can_sleep {
                // Negative thresholds keep the body awake for good (a one-off wake-up
                // would let it fall asleep again once idle).
                *rb.activation_mut() = RigidBodyActivation::cannot_sleep();
            } else if rb.activation().normalized_linear_threshold < 0.0 {
                *rb.activation_mut() = RigidBodyActivation::active();
            }
        }
        if let Some(Mass::Explicit {
            mass,
            center_of_mass,
            principal_inertia,
        }) = mass
        {
            if explicit_mass_usable(*mass, *center_of_mass, *principal_inertia) {
                rb.set_additional_mass_properties(
                    MassProperties::new(
                        to_rapier_vec3(*center_of_mass),
                        *mass,
                        to_rapier_vec3(*principal_inertia),
                    ),
                    true,
                );
            } else {
                // A non-finite or non-positive explicit mass is not pushed; the body
                // keeps the mass it already has (ADR-1507).
                non_finite += 1;
            }
        } else if let Some(Mass::Auto) = mass {
            // Back to density-derived mass: drop an explicit mass applied earlier.
            rb.set_additional_mass_properties(MassProperties::default(), true);
        }
    }
    physics.non_finite_inputs += non_finite;

    for (entity, handle, transform, parent) in &teleports {
        let parent_global = parent.and_then(|p| parents.get(p.parent()).ok());
        let pose = world_pose(transform, parent_global);
        let mut entity_commands = commands.entity(entity);
        // The marker is consumed even when the body is gone, so it never lingers.
        entity_commands.remove::<Teleport>();
        // A non-finite teleport target is ignored: the body keeps its last pose rather than
        // being pushed to NaN (ADR-1507).
        // So is a zero rotation; a non-unit one is normalized (audit-v035-numerics M9).
        let rotation = solver_rotation(pose.1).filter(|_| pose.0.is_finite());
        let Some(rotation) = rotation else {
            physics.non_finite_inputs += 1;
            continue;
        };
        let pose = (pose.0, rotation);
        let Some(rb) = physics.bodies.get_mut(handle.to_handle()) else {
            continue;
        };
        rb.set_position(to_rapier_pose(pose.0, pose.1), true);
        // No interpolation blending across a teleport: pin both poses to the target. Only
        // dynamic bodies are interpolated; poses on a kinematic or static body would never
        // be updated again and would overwrite its `Transform` every frame.
        if rb.is_dynamic() {
            entity_commands.insert((
                PhysicsPose {
                    translation: pose.0,
                    rotation: pose.1,
                },
                PreviousPhysicsPose {
                    translation: pose.0,
                    rotation: pose.1,
                },
            ));
        }
    }
}

/// Pushes Kinematic bodies' `Transform`s as their next poses (spec 15 § 1).
#[instrument(skip_all, target = "regia_physics")]
pub fn sync_kinematic_poses(
    mut physics: ResMut<PhysicsWorld>,
    kinematic: Query<(
        &'static RigidBodyHandle,
        &'static BodyComp,
        &'static Transform,
        Option<&'static ChildOf>,
    )>,
    parents: Query<&'static GlobalTransform>,
) {
    let _span = debug_span!("sync_kinematic_poses").entered();
    for (handle, body, transform, parent) in &kinematic {
        if body.kind != BodyKind::Kinematic {
            continue;
        }
        let parent_global = parent.and_then(|p| parents.get(p.parent()).ok());
        let pose = world_pose(transform, parent_global);
        // The body keeps its last valid pose; a NaN `Transform` never reaches the solver
        // (ADR-1507), nor does a zero rotation; a non-unit one is normalized (M9).
        let rotation = solver_rotation(pose.1).filter(|_| pose.0.is_finite());
        let Some(rotation) = rotation else {
            physics.non_finite_inputs += 1;
            continue;
        };
        if let Some(rb) = physics.bodies.get_mut(handle.to_handle()) {
            rb.set_next_kinematic_translation(to_rapier_vec3(pose.0));
            rb.set_next_kinematic_rotation(to_rapier_rotation(rotation));
        }
    }
}

/// Query for `ExternalForce`s that must be (re)pushed to rapier: the force changed, the body
/// changed (a kind switch to Dynamic re-enables forces) or the body was just created.
type ChangedForcesQuery<'w, 's> = Query<
    'w,
    's,
    (&'static RigidBodyHandle, &'static ExternalForce),
    Or<(Changed<ExternalForce>, Changed<BodyComp>, Added<RigidBodyHandle>)>,
>;

/// Pushes `Velocity` edits and applies `ExternalForce`/`ExternalImpulse` before the
/// step (spec 15 §§ 1–2).
///
/// A `Velocity` write is applied once (on its change tick): afterwards the simulation owns
/// the value and writes results back without marking the component changed again — so
/// impulses applied between steps (character pushes, collisions) survive.
///
/// Rapier keeps a user force until it is reset, so an `ExternalForce` is pushed only when
/// it (or its body) changes; removing the component clears the force (`sync_removed`). A constant force
/// therefore does not wake its body every step: a crate pressed against a wall by a steady
/// force can fall asleep, and the next edit of the force wakes it.
#[instrument(skip_all, target = "regia_physics")]
pub fn sync_velocity_and_forces(
    mut physics: ResMut<PhysicsWorld>,
    velocities: Query<(&'static RigidBodyHandle, &'static BodyComp, &'static Velocity), Changed<Velocity>>,
    forces: ChangedForcesQuery,
    impulses: Query<(Entity, &'static RigidBodyHandle, &'static ExternalImpulse)>,
    mut commands: Commands,
) {
    let _span = debug_span!("sync_velocity_and_forces").entered();

    for (handle, body, velocity) in &velocities {
        if body.kind != BodyKind::Dynamic {
            continue; // kinematic velocity comes from the next pose; static never moves
        }
        if !velocity.linear.is_finite() || !velocity.angular.is_finite() {
            // A non-finite velocity write is ignored: the simulation keeps integrating the
            // body's last valid velocity instead of launching it to NaN (ADR-1507).
            physics.non_finite_inputs += 1;
            continue;
        }
        if let Some(rb) = physics.bodies.get_mut(handle.to_handle()) {
            rb.set_linvel(to_rapier_vec3(velocity.linear), true);
            rb.set_angvel(to_rapier_vec3(velocity.angular), true);
        }
    }

    for (handle, force) in &forces {
        if !force.force.is_finite() || !force.torque.is_finite() {
            // The body keeps the force it had; rapier never accumulates a NaN (ADR-1507).
            physics.non_finite_inputs += 1;
            continue;
        }
        if let Some(rb) = physics.bodies.get_mut(handle.to_handle()) {
            rb.reset_forces(true);
            rb.reset_torques(true);
            rb.add_force(to_rapier_vec3(force.force), true);
            rb.add_torque(to_rapier_vec3(force.torque), true);
        }
    }

    for (entity, handle, impulse) in &impulses {
        if !impulse.impulse.is_finite() || !impulse.torque_impulse.is_finite() {
            // A garbage impulse is dropped, not retried: the component is consumed like a
            // valid one would be (ADR-1507).
            tracing::debug!(
                target: "regia_physics",
                "non-finite impulse on body {entity:?} dropped"
            );
            physics.non_finite_inputs += 1;
            commands.entity(entity).remove::<ExternalImpulse>();
            continue;
        }
        let Some(rb) = physics.bodies.get_mut(handle.to_handle()) else {
            continue;
        };
        if !rb.is_dynamic() {
            // Rapier ignores impulses on non-dynamic bodies: keep the component so the
            // hit lands once the body becomes dynamic.
            tracing::debug!(target: "regia_physics", "impulse on non-dynamic body {entity:?} deferred");
            continue;
        }
        rb.apply_impulse(to_rapier_vec3(impulse.impulse), true);
        rb.apply_torque_impulse(to_rapier_vec3(impulse.torque_impulse), true);
        commands.entity(entity).remove::<ExternalImpulse>();
    }
}

// ---------------------------------------------------------------------------
// The step
// ---------------------------------------------------------------------------

/// Write-back query: dynamic bodies receive `Transform`/`Velocity` updates and the
/// interpolation poses.
type StepQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static RigidBodyHandle,
        Option<&'static mut Transform>,
        Option<&'static mut Velocity>,
        Option<&'static mut PhysicsPose>,
        Option<&'static mut PreviousPhysicsPose>,
        Option<&'static ChildOf>,
        Option<&'static mut DebrisTag>,
    ),
>;

/// Steps the rapier world once per `FixedUpdate` run and writes dynamic poses back to the
/// ECS (spec 15 § 1). Events are buffered by the [`crate::events`] bridge and flushed by
/// [`flush_physics_events`].
#[instrument(skip_all, target = "regia_physics")]
#[allow(clippy::too_many_arguments)]
pub fn step_physics(
    settings: Res<PhysicsSettings>,
    level: Option<Res<LevelSettings>>,
    fixed: Res<FixedTime>,
    mut gravity: ResMut<Gravity>,
    mut physics: ResMut<PhysicsWorld>,
    mut bodies: StepQuery,
    parents: Query<&'static GlobalTransform>,
    mut budget: ResMut<BodyBudget>,
    mut commands: Commands,
    mut swept: crate::ccd_step::SweptQuery,
) {
    let _span = debug_span!("step_physics").entered();
    let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Physics);

    // Gravity: the level's when a level is loaded, else the settings' (specs/15 § 1).
    let requested = level.map(|l| l.gravity).unwrap_or(settings.gravity);
    // A non-finite gravity (a level file's `NaN`, a script's division by zero) would be
    // integrated into every dynamic body inside rapier, where it stays after the gravity is
    // fixed: the step keeps the last finite gravity instead and counts the drop (ADR-1507,
    // audit-v035-numerics H1).
    let gravity_vec = if requested.is_finite() {
        to_rapier_vec3(requested)
    } else {
        physics.non_finite_inputs += 1;
        let last = gravity.0;
        if last.x.is_finite() && last.y.is_finite() && last.z.is_finite() {
            last
        } else {
            Gravity::default().0
        }
    };
    if gravity.0 != gravity_vec {
        // Rapier does not wake sleeping bodies when gravity changes (a level switch, a
        // scripted flip): without this, resting bodies hang in place under the new gravity.
        let sleepers: Vec<_> = physics
            .bodies
            .iter()
            .filter(|(_, rb)| rb.is_dynamic() && rb.is_sleeping())
            .map(|(handle, _)| handle)
            .collect();
        for handle in sleepers {
            if let Some(rb) = physics.bodies.get_mut(handle) {
                rb.wake_up(true);
            }
        }
    }
    gravity.0 = gravity_vec;

    let integration = IntegrationParameters {
        dt: fixed.step().as_secs_f32(),
        num_solver_iterations: settings
            .solver_iterations
            .clamp(1, crate::world::MAX_SOLVER_ITERATIONS),
        max_ccd_substeps: settings.ccd_substeps.clamp(1, crate::world::MAX_CCD_SUBSTEPS),
        ..Default::default()
    };

    // Swept CCD hook (ADR-2423), part 1: pre-step poses of opt-in `SweptCcd` bodies (none: no work).
    let ccd_starts = crate::ccd_step::capture_starts(&physics, &swept);

    {
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
            event_buffer,
            removed_awaiting_step,
            removed_stepped,
            step_count,
            fixed_elapsed,
            last_step_secs,
            ..
        } = &mut *physics;
        // Colliders removed since the last step get their `Stopped` pairs in this step; the
        // flush needs their entities until then.
        removed_stepped.extend(removed_awaiting_step.drain());
        let handler = crate::events::StepEventHandler { buffer: event_buffer };
        let started = std::time::Instant::now();
        pipeline.step(
            gravity_vec,
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
            &handler,
        );
        *last_step_secs = started.elapsed().as_secs_f64();
        *step_count += 1;
        *fixed_elapsed = fixed_elapsed.saturating_add(fixed.step());
    }

    // Swept CCD hook (ADR-2423), part 2: clamp opt-in bodies at their first impact before write-back.
    if !ccd_starts.is_empty() {
        let prediction = integration.prediction_distance();
        let outcomes = crate::ccd_step::resolve(&mut physics, &ccd_starts, integration.dt, prediction);
        crate::ccd_step::record_outcomes(outcomes, &mut swept);
    }

    // Debris lifetimes run on the fixed clock, not the frame clock: the step a chunk leaves the
    // solver on must not depend on the frame rate (record vs replay, two peers; ADR-1660).
    let current_time = physics.fixed_elapsed_secs();
    let mut to_despawn = Vec::new();

    for (entity, handle, mut transform, mut velocity, mut pose, mut previous_pose, parent, debris_tag) in
        &mut bodies
    {
        let rapier_handle = handle.to_handle();
        // Snapshot everything needed in one lookup, before any mutable access.
        let Some(rb) = physics.bodies.get(rapier_handle) else {
            continue;
        };
        let (is_dynamic, is_kinematic) = (rb.is_dynamic(), rb.is_kinematic());
        let (world_t, world_r) = from_rapier_pose(rb.position());
        let linvel = crate::convert::from_rapier_vec3(rb.linvel());
        let angvel = crate::convert::from_rapier_vec3(rb.angvel());
        let pose_finite = world_t.is_finite() && world_r.is_finite();
        let velocity_finite = linvel.is_finite() && angvel.is_finite();
        if !pose_finite || !velocity_finite {
            // A simulated pose or velocity that went non-finite (a NaN that predates the
            // input guards, or one born inside a joint target) stays inside the
            // simulation: it is never written back into the rendered `Transform` or
            // `Velocity`, where it would own gameplay forever after (ADR-1507).
            physics.non_finite_inputs += 1;
        }

        if is_dynamic && pose_finite {
            let parent_global = parent.and_then(|p| parents.get(p.parent()).ok());
            match local_pose((world_t, world_r), parent_global) {
                Some((local_t, local_r)) => {
                    if let Some(ref mut t) = transform {
                        let t = t.bypass_change_detection();
                        t.translation = local_t;
                        t.rotation = local_r;
                    }
                }
                // A singular parent (zero scale): keep the local `Transform` as it was.
                None => physics.non_finite_inputs += 1,
            }
            // Interpolation bookkeeping (spec 15 § 1): previous ← the last current, then
            // current ← the stepped pose. First step: both pin to the stepped pose.
            let (prev_t, prev_r) = match pose {
                Some(ref p) => (p.translation, p.rotation),
                None => (world_t, world_r),
            };
            match previous_pose {
                Some(ref mut prev) => {
                    prev.translation = prev_t;
                    prev.rotation = prev_r;
                }
                None => {
                    commands.entity(entity).insert(PreviousPhysicsPose {
                        translation: prev_t,
                        rotation: prev_r,
                    });
                }
            }
            match pose {
                Some(ref mut p) => {
                    p.translation = world_t;
                    p.rotation = world_r;
                }
                None => {
                    commands.entity(entity).insert(PhysicsPose {
                        translation: world_t,
                        rotation: world_r,
                    });
                }
            }
        }

        // Velocity read-back for dynamic and kinematic bodies (read/write, spec 15 § 2).
        // Bypassing change detection keeps the simulation the owner: only a gameplay
        // write (`get_mut`) marks the component changed and is pushed back (§ 1).
        if (is_dynamic || is_kinematic)
            && velocity_finite
            && let Some(ref mut v) = velocity
        {
            let v = v.bypass_change_detection();
            v.linear = linvel;
            v.angular = angvel;
        }

        // Debris lifetime (M14): a chunk's clock starts at the end of the first step that
        // simulates it, and it is despawned past its lifetime.
        if let Some(mut tag) = debris_tag {
            let spawn_time = match tag.spawn_time_secs {
                Some(t) => t,
                None => {
                    tag.spawn_time_secs = Some(current_time);
                    current_time
                }
            };
            if crate::sync::debris_expired(current_time, spawn_time, tag.lifetime_secs) {
                budget.remove(entity);
                to_despawn.push((entity, rapier_handle));
            }
        }
    }

    // Rapier reuses freed handles in removal order, so this order must not follow the query's,
    // nor entity ids (thread timing under parallel `Commands::spawn`): handle order is the order
    // the bodies were inserted in, which follows spawn order (ADR-1347, ADR-2396).
    to_despawn.sort_by_key(|row| row.1.into_raw_parts());
    for (entity, handle) in to_despawn {
        physics.remove_body(handle);
        commands.entity(entity).despawn();
    }
}

// ---------------------------------------------------------------------------
// rapier → ECS: event flush
// ---------------------------------------------------------------------------

/// Drains the step's buffered events into ECS messages (specs/15 § 3).
///
/// Sensor pairs become trigger events — the sensor side is the trigger; if both are
/// sensors the first is the trigger. Pairs whose colliders have no ECS entity (raw rapier
/// colliders, e.g. the M14 test path) are dropped.
///
/// A pair whose collider was removed before the step (component removed, entity
/// despawned) still reports `CollisionEnded`/`TriggerExit` for the entity that owned it,
/// so "touching" bookkeeping in gameplay never leaks; that entity may already be
/// despawned.
#[instrument(skip_all, target = "regia_physics")]
pub fn flush_physics_events(
    mut physics: ResMut<PhysicsWorld>,
    mut started: ResMut<Messages<CollisionStarted>>,
    mut ended: ResMut<Messages<CollisionEnded>>,
    mut trigger_enter: ResMut<Messages<TriggerEnter>>,
    mut trigger_exit: ResMut<Messages<TriggerExit>>,
    mut contact_force: ResMut<Messages<ContactForce>>,
    mut joint_broken: ResMut<Messages<JointBroken>>,
) {
    let _span = debug_span!("flush_physics_events").entered();

    for event in physics.event_buffer.drain() {
        match event {
            crate::events::RawEvent::Pair { started: began, a, b } => {
                let (Some((entity_a, sensor_a)), Some((entity_b, sensor_b))) =
                    (physics.event_participant(a), physics.event_participant(b))
                else {
                    continue;
                };
                if sensor_a || sensor_b {
                    let (trigger, other) = if sensor_a {
                        (entity_a, entity_b)
                    } else {
                        (entity_b, entity_a)
                    };
                    if began {
                        trigger_enter.write(TriggerEnter { trigger, other });
                    } else {
                        trigger_exit.write(TriggerExit { trigger, other });
                    }
                } else if began {
                    started.write(CollisionStarted { entity_a, entity_b });
                } else {
                    ended.write(CollisionEnded { entity_a, entity_b });
                }
            }
            crate::events::RawEvent::Force { a, b, magnitude } => {
                let (Some(entity_a), Some(entity_b)) =
                    (physics.entity_of_collider(a), physics.entity_of_collider(b))
                else {
                    continue;
                };
                contact_force.write(ContactForce {
                    entity_a,
                    entity_b,
                    total_force: magnitude,
                });
            }
            crate::events::RawEvent::BrokenJoint { entity, connected } => {
                joint_broken.write(JointBroken { entity, connected });
            }
        }
    }
    // Removed colliders' pairs were reported by the step that just ran.
    physics.removed_stepped.clear();
}

/// A heightfield whose non-finite samples are holes: each cell touching one is removed
/// (`CELL_REMOVED`), and the sample itself takes the lowest finite height so the bounds
/// stay finite. `None` when no sample is finite.
fn holed_heightfield(
    nx: usize,
    nz: usize,
    heights: &[f32],
    scale: Vec3,
) -> Option<rapier3d::parry::shape::HeightField> {
    use rapier3d::parry::shape::{HeightField, HeightFieldCellStatus};
    let floor = heights
        .iter()
        .copied()
        .filter(|h| h.is_finite())
        .reduce(f32::min)?;
    let filled: Vec<f32> = heights
        .iter()
        .map(|&h| if h.is_finite() { h } else { floor })
        .collect();
    let mut field = HeightField::new(
        rapier3d::geometry::Array2::new(nx, nz, filled),
        to_rapier_vec3(scale),
    );
    // `Array2` is column-major: sample (i, j) is `heights[i + j * nx]`; cell (i, j) spans
    // samples i..=i + 1 and j..=j + 1.
    let hole = |i: usize, j: usize| !heights[i + j * nx].is_finite();
    for j in 0..nz - 1 {
        for i in 0..nx - 1 {
            if hole(i, j) || hole(i + 1, j) || hole(i, j + 1) || hole(i + 1, j + 1) {
                field.set_cell_status(i, j, HeightFieldCellStatus::CELL_REMOVED);
            }
        }
    }
    Some(field)
}

#[cfg(test)]
mod non_finite_tests {
    use super::*;

    #[test]
    fn finite_pose_accepts_finite_and_rejects_non_finite() {
        assert!(finite_pose(&(Vec3::new(1.0, -2.0, 3.0), Quat::IDENTITY)));
        assert!(!finite_pose(&(Vec3::NAN, Quat::IDENTITY)), "NaN translation");
        assert!(
            !finite_pose(&(Vec3::new(f32::INFINITY, 0.0, 0.0), Quat::IDENTITY)),
            "infinite translation"
        );
        assert!(
            !finite_pose(&(Vec3::ZERO, Quat::from_xyzw(0.0, 0.0, 0.0, f32::NAN))),
            "NaN rotation"
        );
    }

    #[test]
    fn a_zero_scale_parent_gives_no_local_pose() {
        // The write-back and the interpolation both keep the old `Transform` when there is no
        // local pose; this pins the premise (a zero-scale parent has no inverse).
        let parent = GlobalTransform(Affine3A::from_scale(Vec3::ZERO));
        assert!(local_pose((Vec3::new(1.0, 2.0, 3.0), Quat::IDENTITY), Some(&parent)).is_none());
        let ok = GlobalTransform(Affine3A::from_scale(Vec3::splat(2.0)));
        let (t, r) =
            local_pose((Vec3::new(2.0, 0.0, 0.0), Quat::IDENTITY), Some(&ok)).expect("invertible parent");
        assert!(t.is_finite() && r.is_finite());
    }
}

#[cfg(test)]
mod debris_sleep_tests {
    use std::time::Duration;

    use regia_app::{App, FixedTime, TimeUpdateStrategy};
    use regia_core::math::Vec3;
    use regia_scene::Transform;

    use super::{BodyComp, ColliderComp, ColliderShape, DebrisTag, RigidBodyHandle, Velocity};
    use crate::plugin::PhysicsPlugin;
    use crate::world::PhysicsWorld;

    #[test]
    fn settled_debris_stays_asleep() {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(PhysicsPlugin);
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            BodyComp::static_body(),
            ColliderComp {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(10.0, 0.5, 10.0),
                },
                friction: 0.8,
                ..ColliderComp::default()
            },
        ));
        let chunk = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.5, 0.0)),
                BodyComp::dynamic(),
                ColliderComp {
                    friction: 0.8,
                    ..ColliderComp::default()
                },
                Velocity::default(),
                DebrisTag {
                    spawn_time_secs: None,
                    lifetime_secs: 120.0,
                },
            ))
            .id();

        let sleeping = |app: &App| {
            let handle = app
                .world()
                .get::<RigidBodyHandle>(chunk)
                .expect("debris body")
                .to_handle();
            app.world()
                .resource::<PhysicsWorld>()
                .bodies
                .get(handle)
                .expect("rapier body")
                .is_sleeping()
        };
        let mut slept = false;
        for _ in 0..400 {
            app.update();
            if sleeping(&app) {
                slept = true;
                break;
            }
        }
        assert!(slept, "debris was never asleep after a physics step");
        app.update();
        assert!(sleeping(&app), "the next step woke settled debris");
        assert!(
            app.world()
                .get::<DebrisTag>(chunk)
                .expect("tag")
                .spawn_time_secs
                .is_some()
        );
    }
    #[test]
    fn a_zero_spawn_rotation_does_not_reach_the_rapier_body() {
        // Corr-physics audit: the spawn path copied the quaternion bits into rapier.
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(PhysicsPlugin);
        let body = app
            .world_mut()
            .spawn((
                Transform {
                    rotation: regia_core::math::Quat::from_xyzw(0.0, 0.0, 0.0, 0.0),
                    ..Transform::from_translation(Vec3::new(0.0, 5.0, 0.0))
                },
                BodyComp::dynamic(),
                ColliderComp::default(),
                Velocity::default(),
            ))
            .id();
        let mut handle = None;
        for _ in 0..10 {
            app.update();
            handle = app.world().get::<RigidBodyHandle>(body).map(|h| h.to_handle());
            if handle.is_some() {
                break;
            }
        }
        let handle = handle.expect("the body was never spawned");
        let physics = app.world().resource::<PhysicsWorld>();
        let rotation =
            crate::convert::from_rapier_pose(physics.bodies.get(handle).expect("rapier body").position()).1;
        assert!(
            (rotation.length() - 1.0).abs() < 1.0e-5,
            "rapier holds a non-unit rotation {rotation:?}"
        );
        assert!(
            physics.non_finite_inputs >= 1,
            "the zero rotation was not counted"
        );
    }
}

#[cfg(test)]
mod non_finite_tests_rest {
    use super::*;
    use rapier3d::dynamics::RigidBodyAdditionalMassProps;

    #[test]
    fn origin_pose_is_the_identity() {
        let (translation, rotation) = origin_pose();
        assert_eq!(translation, Vec3::ZERO);
        assert_eq!(rotation, Quat::IDENTITY);
    }

    #[test]
    fn build_rapier_body_substitutes_non_finite_fields_and_counts_them() {
        let body = BodyComp {
            linear_damping: f32::NAN,
            angular_damping: f32::INFINITY,
            gravity_scale: f32::NAN,
            ..BodyComp::dynamic()
        };
        let mass = Mass::Explicit {
            mass: 4.0,
            center_of_mass: Vec3::new(f32::NAN, 0.0, 0.0),
            principal_inertia: Vec3::ONE,
        };
        let (rb, substituted) = build_rapier_body(&body, Some(&mass), origin_pose());
        // Both dampings, the gravity scale and the explicit mass were substituted.
        assert_eq!(substituted, 4);
        assert_eq!(rb.linear_damping(), 0.0);
        assert_eq!(rb.angular_damping(), 0.0);
        assert_eq!(rb.gravity_scale(), 1.0);
        let t = rb.translation();
        assert!(
            t.x.is_finite() && t.y.is_finite() && t.z.is_finite(),
            "pose stays finite"
        );
    }

    #[test]
    fn spawn_pose_keeps_unit_rotations_and_repairs_bad_ones() {
        // Corr-physics audit: spawning copied the quaternion bits into rapier, which never
        // renormalizes, so a zero or non-unit spawn rotation stayed a zero or skewing matrix.
        let unit = Quat::from_rotation_y(0.7);
        assert_eq!(spawn_pose((Vec3::X, unit)), ((Vec3::X, unit), false));
        let ((t, q), dropped) = spawn_pose((Vec3::X, Quat::from_xyzw(0.0, 0.0, 0.0, 2.0)));
        assert!(!dropped);
        assert_eq!(t, Vec3::X);
        assert!((q.length() - 1.0).abs() < 1.0e-6, "{q:?}");
        let ((t, q), dropped) = spawn_pose((Vec3::Y, Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)));
        assert!(dropped, "a zero rotation is a dropped input");
        assert_eq!((t, q), (Vec3::Y, Quat::IDENTITY));
        assert_eq!(spawn_pose((Vec3::NAN, Quat::IDENTITY)), (origin_pose(), true));
    }

    #[test]
    fn build_rapier_body_passes_finite_fields_through() {
        let (rb, substituted) = build_rapier_body(&BodyComp::dynamic(), None, origin_pose());
        assert_eq!(substituted, 0);
        assert_eq!(rb.gravity_scale(), 1.0);
    }

    #[test]
    fn build_rapier_body_drops_a_non_positive_explicit_mass() {
        let dropped = [
            (-10.0, Vec3::ONE),
            (0.0, Vec3::ONE),
            (10.0, Vec3::new(1.0, 0.0, 1.0)),
            (10.0, Vec3::new(1.0, -2.0, 1.0)),
            (f32::NAN, Vec3::ONE),
        ];
        for (mass, inertia) in dropped {
            let explicit = Mass::Explicit {
                mass,
                center_of_mass: Vec3::ZERO,
                principal_inertia: inertia,
            };
            let (rb, substituted) = build_rapier_body(&BodyComp::dynamic(), Some(&explicit), origin_pose());
            assert!(substituted >= 1, "mass {mass} inertia {inertia} was pushed");
            // `RigidBody::mass` is the collider contribution. It stays 0 until the body is
            // inserted and recomputed, so the explicit mass lives in `additional_local_mprops`.
            assert!(
                rb.mass_properties().additional_local_mprops.is_none(),
                "mass {mass} inertia {inertia} was stored on the body"
            );
        }
        let ok = Mass::Explicit {
            mass: 4.0,
            center_of_mass: Vec3::ZERO,
            principal_inertia: Vec3::ONE,
        };
        let (rb, substituted) = build_rapier_body(&BodyComp::dynamic(), Some(&ok), origin_pose());
        assert_eq!(substituted, 0);
        let extra = rb.mass_properties().additional_local_mprops.as_deref();
        let Some(RigidBodyAdditionalMassProps::MassProps(props)) = extra else {
            panic!("a usable explicit mass must be stored, got {extra:?}");
        };
        assert!((props.mass() - 4.0).abs() < 1e-5, "{}", props.mass());
        let inertia = props.principal_inertia();
        assert!(
            (inertia.x - 1.0).abs() < 1e-5
                && (inertia.y - 1.0).abs() < 1e-5
                && (inertia.z - 1.0).abs() < 1e-5,
            "{inertia:?}"
        );
    }

    /// Parry 0.31 `SharedShape::convex_hull` returns `Some` for a coplanar quad: the hull
    /// builder treats dimension 2 as a double-sided flat mesh (`try_get_initial_mesh`), and
    /// `from_convex_mesh` accepts that mesh. A collinear set is the case that returns `None`
    /// (repeated triangle indices). This pins both, and that the flat quad still contacts.
    #[test]
    fn coplanar_quad_builds_a_shared_shape_and_contacts_a_box() {
        let quad = shape_to_shared_shape(&ColliderShape::ConvexHull {
            points: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(0.0, 1.0, 0.0),
                Vec3::new(1.0, 1.0, 0.0),
            ],
        })
        .expect("parry builds a flat convex for a coplanar quad");
        let line = shape_to_shared_shape(&ColliderShape::ConvexHull {
            points: vec![
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(2.0, 0.0, 0.0),
                Vec3::new(3.0, 0.0, 0.0),
            ],
        });
        assert!(line.is_none(), "collinear points are not a convex mesh");

        let cuboid = rapier3d::parry::shape::Cuboid::new(rapier3d::math::Vector::new(0.2, 0.2, 0.2));
        let id = rapier3d::math::Pose::identity();
        let through = rapier3d::math::Pose::translation(0.5, 0.5, 0.0);
        let hit = rapier3d::parry::query::contact(&id, &*quad, &through, &cuboid, 0.0)
            .expect("cuboid against a convex is a supported query");
        let dist = hit.as_ref().map(|c| c.dist);
        assert!(
            dist.is_some_and(|d| d < 0.0),
            "a box centered on the quad must penetrate it, got {dist:?}"
        );

        let above = rapier3d::math::Pose::translation(0.5, 0.5, 0.5);
        let gap = rapier3d::parry::query::contact(&id, &*quad, &above, &cuboid, 0.0)
            .expect("cuboid against a convex is a supported query");
        assert!(
            gap.as_ref().is_none_or(|c| c.dist > 0.0),
            "a box clear of the quad must not penetrate, got {gap:?}"
        );
    }
}
