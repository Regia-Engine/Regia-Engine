// SPDX-License-Identifier: MPL-2.0
//! 2D rigid-body physics with rapier2d (`docs/specs/22-tier2.md` § T2.10), behind the
//! `physics2d` cargo feature.
//!
//! The ECS surface mirrors the 3D wrapper on a smaller scale: [`RigidBody2d`] (reusing
//! [`BodyKind`]), [`Collider2d`] with [`ColliderShape2d`], [`Velocity2d`], the
//! [`PhysicsWorld2d`] resource with ray and point queries, and [`Collision2d`] messages.
//! [`Physics2dPlugin`] runs the chain in `FixedUpdate` (`Physics2dStep`): removals → additions
//! → kinematic targets and velocity edits → one rapier step at `FixedTime` dt → contact
//! messages → pose/velocity write-back into `Transform` (x, y and the rotation about Z; z is
//! kept as the draw order).
//!
//! After spawn only the transform of a kinematic body and a changed [`Velocity2d`] reach the
//! simulation. The remaining [`RigidBody2d`] fields (kind, damping, gravity scale, CCD, rotation
//! lock) and every [`Collider2d`] field are **creation-only**: to change one, remove the
//! component (or despawn) and add it again.
//!
//! rapier2d's math type *is* glam's `Vec2` (rapier 0.36 → glamx 0.3 → the workspace's glam
//! 0.33), so no conversion layer is needed. Units are whatever the game uses: set
//! [`PhysicsWorld2d::set_length_unit`] to the size of a typical object (e.g. 16 for pixel art in
//! pixels) so rapier's tolerances scale, and [`PhysicsWorld2d::gravity`] to match.
//!
//! Determinism: same build, platform and inputs → identical results (rapier is deterministic per
//! platform; bodies and colliders are added in their components' arrival order, never entity-id
//! order, which parallel `Commands::spawn` would make thread-timing dependent; ADR-1347). Contact
//! messages follow rapier collider-handle order for the same reason. Angles extracted from
//! `Transform` rotations use the deterministic transcendentals (spec 15 § 5).
//! `physics-cross-platform-determinism` enables rapier2d's `enhanced-determinism` too.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use rapier2d::dynamics::{
    RigidBodyBuilder, RigidBodyHandle, RigidBodySet, RigidBodyType, SoftBodySet, SoftBodyTearEvent,
};
use rapier2d::geometry::{ColliderBuilder, ColliderHandle, ColliderSet, CollisionEvent, ContactPair, Ray};
use rapier2d::pipeline::{ActiveEvents, EventHandler, PhysicsWorld as RapierWorld, QueryFilter};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec2};
use regia_scene::Transform;
use regia_scene::layers::LayerParked;

use crate::components::BodyKind;

/// Most points one 2D polyline collider may have.
const MAX_POLYLINE_POINTS: usize = 65_536;

/// A 2D rigid body. Its pose comes from the entity's `Transform` (x, y, rotation about Z).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
#[require(Transform, Velocity2d)]
pub struct RigidBody2d {
    /// Dynamic, kinematic (driven by its `Transform`) or static.
    pub kind: BodyKind,
    /// Linear damping.
    pub linear_damping: f32,
    /// Angular damping.
    pub angular_damping: f32,
    /// Gravity multiplier.
    pub gravity_scale: f32,
    /// Continuous collision detection (fast bodies).
    pub ccd: bool,
    /// Never rotates (characters).
    pub lock_rotation: bool,
}

impl Default for RigidBody2d {
    fn default() -> Self {
        Self {
            kind: BodyKind::Dynamic,
            linear_damping: 0.0,
            angular_damping: 0.0,
            gravity_scale: 1.0,
            ccd: false,
            lock_rotation: false,
        }
    }
}

/// A 2D collider shape (local to the entity, before [`Collider2d::offset`]).
#[derive(Reflect, Clone, Debug, PartialEq)]
pub enum ColliderShape2d {
    /// Circle.
    Ball {
        /// Radius.
        radius: f32,
    },
    /// Box.
    Cuboid {
        /// Half width and half height.
        half_extents: Vec2,
    },
    /// Vertical capsule.
    Capsule {
        /// Half the distance between the cap centres.
        half_height: f32,
        /// Cap radius.
        radius: f32,
    },
    /// A line segment.
    Segment {
        /// First end.
        a: Vec2,
        /// Second end.
        b: Vec2,
    },
    /// The convex hull of the points (a degenerate set, such as collinear points, is skipped with a warning).
    ConvexPolygon {
        /// Points.
        points: Vec<Vec2>,
    },
    /// An open chain of segments (static terrain outlines).
    Polyline {
        /// Points.
        points: Vec<Vec2>,
    },
}

impl Default for ColliderShape2d {
    fn default() -> Self {
        Self::Cuboid {
            half_extents: Vec2::splat(0.5),
        }
    }
}

/// A 2D collider. On an entity with a [`RigidBody2d`] it is attached to that body; otherwise it is
/// static at the entity's `Transform`.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default)]
#[require(Transform)]
pub struct Collider2d {
    /// Shape.
    pub shape: ColliderShape2d,
    /// Offset from the entity origin.
    pub offset: Vec2,
    /// Friction coefficient.
    pub friction: f32,
    /// Restitution (bounciness).
    pub restitution: f32,
    /// Density (mass = density × area).
    pub density: f32,
    /// Reports overlaps without a physical response.
    pub sensor: bool,
}

impl Default for Collider2d {
    fn default() -> Self {
        Self {
            shape: ColliderShape2d::default(),
            offset: Vec2::ZERO,
            friction: 0.5,
            restitution: 0.0,
            density: 1.0,
            sensor: false,
        }
    }
}

impl Collider2d {
    /// A box collider of `width`×`height`.
    #[must_use]
    pub fn cuboid(width: f32, height: f32) -> Self {
        Self {
            shape: ColliderShape2d::Cuboid {
                half_extents: Vec2::new(width, height) * 0.5,
            },
            ..Self::default()
        }
    }

    /// A circle collider.
    #[must_use]
    pub fn ball(radius: f32) -> Self {
        Self {
            shape: ColliderShape2d::Ball { radius },
            ..Self::default()
        }
    }
}

/// Linear (units/s) and angular (rad/s) velocity of a 2D body. Written back after every step;
/// a change made by gameplay is pushed into rapier before the next step.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct Velocity2d {
    /// Linear velocity.
    pub linear: Vec2,
    /// Angular velocity, counter-clockwise.
    pub angular: f32,
}

/// The rapier handle of a 2D body (inserted by the plugin).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Body2dHandle(pub RigidBodyHandle);

/// The rapier handle of a 2D collider (inserted by the plugin).
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Collider2dHandle(pub ColliderHandle);

/// Two 2D colliders started or stopped touching (or overlapping, for sensors).
///
/// A pair whose collider went away (component removed, body removed or parked, entity
/// despawned) reports its end on the next step; that entity may already be despawned. Within
/// one step every end is written before every begin.
///
/// `a` and `b` follow the rapier collider handles (the lower handle's entity first), not the
/// entity ids: ids from parallel `Commands::spawn` are thread-timing order (ADR-1347), and
/// messages in a different order replay differently. A listener that must be id-order
/// independent should compare the entities itself.
#[derive(Message, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Collision2d {
    /// Entity owning the lower collider handle of the pair.
    pub a: Entity,
    /// Entity owning the higher collider handle of the pair.
    pub b: Entity,
    /// True when the contact began this step, false when it ended.
    pub started: bool,
}

/// A ray hit.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RaycastHit2d {
    /// The entity owning the hit collider.
    pub entity: Entity,
    /// World hit point.
    pub point: Vec2,
    /// Surface normal at the hit.
    pub normal: Vec2,
    /// Distance along the (normalised) ray.
    pub distance: f32,
}

/// Two colliders in contact (or sensor overlap), keyed and ordered by the rapier collider
/// handles — never by entity id, which parallel `Commands::spawn` reserves in thread-timing
/// order (ADR-1347). Begin/end messages are written in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Touching2d {
    /// Lower collider handle of the pair (raw parts: rapier's `ColliderHandle` has no `Ord`).
    collider_a: (u32, u32),
    /// Higher collider handle of the pair.
    collider_b: (u32, u32),
    /// Entity owning `collider_a`'s handle.
    entity_a: Entity,
    /// Entity owning `collider_b`'s handle.
    entity_b: Entity,
}

/// The pair for two collider handles in handle (insertion) order, or `None` when either
/// has no entity.
fn touching2d(
    collider_entities: &HashMap<ColliderHandle, Entity>,
    h1: ColliderHandle,
    h2: ColliderHandle,
) -> Option<Touching2d> {
    let (ha, hb) = (h1.into_raw_parts(), h2.into_raw_parts());
    let (collider_a, collider_b) = if ha <= hb { (ha, hb) } else { (hb, ha) };
    let handle_a = if ha <= hb { h1 } else { h2 };
    let handle_b = if ha <= hb { h2 } else { h1 };
    Some(Touching2d {
        collider_a,
        collider_b,
        entity_a: *collider_entities.get(&handle_a)?,
        entity_b: *collider_entities.get(&handle_b)?,
    })
}

/// The rapier2d world plus the entity ↔ handle maps.
#[derive(Resource)]
pub struct PhysicsWorld2d {
    /// The rapier world (bodies, colliders, pipeline). Tools may read it; gameplay goes through
    /// components.
    pub world: RapierWorld,
    body_entities: HashMap<RigidBodyHandle, Entity>,
    collider_entities: HashMap<ColliderHandle, Entity>,
    bodies_of: HashMap<Entity, RigidBodyHandle>,
    colliders_of: HashMap<Entity, ColliderHandle>,
    touching: BTreeSet<Touching2d>,
    /// Inserts into a contact-pair set since the world was created. A settled step must add none:
    /// the pair set is kept and updated from begin/end, not rebuilt (the allocation test reads this).
    touching_inserts: u64,
    /// Collision begin/end events from the step in progress. Kept so a quiet step reuses the buffer.
    pending_events: Vec<(ColliderHandle, ColliderHandle, bool)>,
    steps: u64,
    /// Non-finite poses, body fields and velocities dropped at the rapier seam since the
    /// world was created (ADR-1507; the 2D twin of `PhysicsStats::non_finite_inputs`).
    non_finite_inputs: u64,
    /// Entities whose `RigidBody2d` left since the last fixed step (despawn or removal),
    /// recorded by a `Remove` observer: removal messages are cleared every frame and would be
    /// lost on frames without a fixed step (ADR-1660, as the 3D path does).
    removed_bodies: Vec<Entity>,
    /// The same for `Collider2d`.
    removed_colliders: Vec<Entity>,
}

impl Default for PhysicsWorld2d {
    fn default() -> Self {
        let mut world = RapierWorld::new();
        world.gravity = Vec2::new(0.0, -9.81);
        Self {
            world,
            body_entities: HashMap::new(),
            collider_entities: HashMap::new(),
            bodies_of: HashMap::new(),
            colliders_of: HashMap::new(),
            touching: BTreeSet::new(),
            touching_inserts: 0,
            pending_events: Vec::new(),
            steps: 0,
            non_finite_inputs: 0,
            removed_bodies: Vec::new(),
            removed_colliders: Vec::new(),
        }
    }
}

impl PhysicsWorld2d {
    /// Gravity (units/s²).
    #[must_use]
    pub fn gravity(&self) -> Vec2 {
        self.world.gravity
    }

    /// Sets gravity (units/s²).
    pub fn set_gravity(&mut self, gravity: Vec2) {
        self.world.gravity = gravity;
    }

    /// Sets the size of a typical object in world units (rapier scales its tolerances by it).
    pub fn set_length_unit(&mut self, unit: f32) {
        self.world.integration_parameters.length_unit = unit.max(f32::MIN_POSITIVE);
    }

    /// Number of rigid bodies.
    #[must_use]
    pub fn body_count(&self) -> usize {
        self.world.bodies.len()
    }

    /// Number of steps taken.
    #[must_use]
    pub fn steps(&self) -> u64 {
        self.steps
    }

    /// Non-finite poses, body fields and velocities dropped at the rapier seam since the
    /// world was created (ADR-1507). Zero in a healthy app.
    #[must_use]
    pub fn non_finite_inputs(&self) -> u64 {
        self.non_finite_inputs
    }

    /// The entity owning a collider.
    #[must_use]
    pub fn entity_of_collider(&self, handle: ColliderHandle) -> Option<Entity> {
        self.collider_entities.get(&handle).copied()
    }

    /// True when every dynamic body is asleep.
    #[must_use]
    pub fn all_asleep(&self) -> bool {
        self.world
            .bodies
            .iter()
            .all(|(_, b)| !b.is_dynamic() || b.is_sleeping())
    }

    /// The closest hit along a ray (`direction` is normalised; a ray starting inside a shape hits
    /// it at distance 0).
    #[must_use]
    pub fn raycast(&self, origin: Vec2, direction: Vec2, max_distance: f32) -> Option<RaycastHit2d> {
        let dir = direction.normalize_or_zero();
        if dir == Vec2::ZERO {
            return None;
        }
        let ray = Ray::new(origin, dir);
        let (handle, hit) =
            self.world
                .cast_ray_and_get_normal(&ray, max_distance, true, QueryFilter::default())?;
        Some(RaycastHit2d {
            entity: self.entity_of_collider(handle)?,
            point: origin + dir * hit.time_of_impact,
            normal: hit.normal,
            distance: hit.time_of_impact,
        })
    }

    /// Entities whose colliders contain `point`, in rapier collider-handle order (never
    /// entity-id order, which parallel `Commands::spawn` makes thread-timing dependent;
    /// ADR-1347), so the sequence is the same on every run.
    #[must_use]
    pub fn entities_at(&self, point: Vec2) -> Vec<Entity> {
        let mut out: Vec<(ColliderHandle, Entity)> = self
            .world
            .intersect_point(point, QueryFilter::default())
            .filter_map(|(handle, _)| self.entity_of_collider(handle).map(|e| (handle, e)))
            .collect();
        out.sort_by_key(|(handle, _)| handle.into_raw_parts());
        out.into_iter().map(|(_, entity)| entity).collect()
    }

    fn add_body(&mut self, entity: Entity, body: &RigidBody2d, transform: &Transform, velocity: Velocity2d) {
        let kind = match body.kind {
            BodyKind::Dynamic => RigidBodyType::Dynamic,
            BodyKind::Kinematic => RigidBodyType::KinematicPositionBased,
            BodyKind::Static => RigidBodyType::Fixed,
        };
        // Non-finite inputs are substituted, not handed to rapier (ADR-1507): a NaN pose
        // parks the body at the origin (it must exist — colliders reference its handle),
        // non-finite damping, gravity scale and velocities fall back to the defaults, and
        // every substitution is counted.
        let pose_finite = transform.translation.is_finite() && transform.rotation.is_finite();
        if !pose_finite {
            self.non_finite_inputs += 1;
        }
        let translation = if pose_finite {
            transform.translation.truncate()
        } else {
            Vec2::ZERO
        };
        let rotation = if pose_finite {
            angle_of(transform.rotation)
        } else {
            0.0
        };
        let linear_damping = if body.linear_damping.is_finite() {
            body.linear_damping
        } else {
            self.non_finite_inputs += 1;
            0.0
        };
        let angular_damping = if body.angular_damping.is_finite() {
            body.angular_damping
        } else {
            self.non_finite_inputs += 1;
            0.0
        };
        let gravity_scale = if body.gravity_scale.is_finite() {
            body.gravity_scale
        } else {
            self.non_finite_inputs += 1;
            1.0
        };
        let velocity_finite = velocity.linear.is_finite() && velocity.angular.is_finite();
        if !velocity_finite {
            self.non_finite_inputs += 1;
        }
        let (linvel, angvel) = if velocity_finite {
            (velocity.linear, velocity.angular)
        } else {
            (Vec2::ZERO, 0.0)
        };
        let mut builder = RigidBodyBuilder::new(kind)
            .pose(rapier2d::math::Pose::from_parts(
                translation,
                rotation_2d(rotation),
            ))
            .linear_damping(linear_damping)
            .angular_damping(angular_damping)
            .gravity_scale(gravity_scale)
            .ccd_enabled(body.ccd)
            .linvel(linvel)
            .angvel(angvel);
        if body.lock_rotation {
            builder = builder.lock_rotations();
        }
        let handle = self.world.insert_body(builder.build());
        self.body_entities.insert(handle, entity);
        self.bodies_of.insert(entity, handle);
    }

    fn add_collider(
        &mut self,
        entity: Entity,
        collider: &Collider2d,
        transform: &Transform,
    ) -> Option<ColliderHandle> {
        // `max` also turns NaN into the floor.
        let extent = |v: f32| v.max(f32::MIN_POSITIVE);
        let builder = match &collider.shape {
            ColliderShape2d::Ball { radius } => ColliderBuilder::ball(extent(*radius)),
            ColliderShape2d::Cuboid { half_extents } => {
                ColliderBuilder::cuboid(extent(half_extents.x), extent(half_extents.y))
            }
            ColliderShape2d::Capsule { half_height, radius } => {
                ColliderBuilder::capsule_y(half_height.max(0.0), extent(*radius))
            }
            ColliderShape2d::Segment { a, b } => {
                if !(a.is_finite() && b.is_finite()) {
                    tracing::warn!(?entity, "non-finite 2D segment collider; skipped");
                    return None;
                }
                ColliderBuilder::segment(*a, *b)
            }
            ColliderShape2d::ConvexPolygon { points } => match convex_polygon_builder(points) {
                Some(builder) => builder,
                None => {
                    tracing::warn!(?entity, "degenerate 2D convex polygon collider; skipped");
                    return None;
                }
            },
            ColliderShape2d::Polyline { points } => {
                if points.len() < 2
                    || points.len() > MAX_POLYLINE_POINTS
                    || !points.iter().all(|p| p.is_finite())
                {
                    tracing::warn!(
                        ?entity,
                        "empty, non-finite or oversized 2D polyline collider; skipped"
                    );
                    return None;
                }
                ColliderBuilder::polyline(points.clone(), None)
            }
        };
        // Non-finite or negative material numbers, a non-finite offset and a non-finite free
        // pose are substituted and counted, as `add_body` does (ADR-1507): friction, restitution
        // and density take the `Collider2d` defaults, the offset zero, and a free collider at a
        // non-finite pose is parked at the origin.
        let defaults = Collider2d::default();
        let mut material = |value: f32, default: f32| {
            if value.is_finite() && value >= 0.0 {
                value
            } else {
                self.non_finite_inputs += 1;
                default
            }
        };
        let friction = material(collider.friction, defaults.friction);
        let restitution = material(collider.restitution, defaults.restitution);
        let density = material(collider.density, defaults.density);
        let offset = if collider.offset.is_finite() {
            collider.offset
        } else {
            self.non_finite_inputs += 1;
            Vec2::ZERO
        };
        let parent = self.bodies_of.get(&entity).copied();
        let mut builder = builder
            .friction(friction)
            .restitution(restitution)
            .density(density)
            .sensor(collider.sensor)
            .active_events(ActiveEvents::COLLISION_EVENTS);
        builder = match parent {
            // Body-local: the body's rotation turns the offset.
            Some(_) => builder.translation(offset),
            None => {
                let pose_finite = transform.translation.is_finite() && transform.rotation.is_finite();
                let (origin, angle) = if pose_finite {
                    (transform.translation.truncate(), angle_of(transform.rotation))
                } else {
                    self.non_finite_inputs += 1;
                    (Vec2::ZERO, 0.0)
                };
                // `offset` is from the entity origin in the entity's frame, as on a body: the
                // shape's own position is the rotated offset, the shape turns about it.
                // `Rotation::new` would send that angle back through the platform libm.
                let rotation = rotation_2d(angle);
                builder.position(rapier2d::math::Pose::from_parts(
                    origin + rotation * offset,
                    rotation,
                ))
            }
        };
        let handle = self.world.insert_collider(builder.build(), parent);
        self.collider_entities.insert(handle, entity);
        self.colliders_of.insert(entity, handle);
        Some(handle)
    }

    /// Removes only the collider of `entity`; returns whether one existed.
    fn remove_collider_of(&mut self, entity: Entity) -> bool {
        let Some(handle) = self.colliders_of.remove(&entity) else {
            return false;
        };
        self.collider_entities.remove(&handle);
        self.world.remove_collider(handle);
        true
    }

    /// Removes the body and collider of `entity`; returns every entity that lost a collider
    /// (its own, and colliders of other entities attached to the body).
    fn remove_entity(&mut self, entity: Entity) -> Vec<Entity> {
        let mut orphaned = Vec::new();
        if self.remove_collider_of(entity) {
            orphaned.push(entity);
        }
        if let Some(handle) = self.bodies_of.remove(&entity) {
            self.body_entities.remove(&handle);
            // Colliders of other entities attached to this body go with it.
            let attached: Vec<(Entity, ColliderHandle)> = self
                .colliders_of
                .iter()
                .filter(|(_, ch)| {
                    self.world
                        .colliders
                        .get(**ch)
                        .is_some_and(|c| c.parent() == Some(handle))
                })
                .map(|(e, ch)| (*e, *ch))
                .collect();
            for (e, ch) in attached {
                self.colliders_of.remove(&e);
                self.collider_entities.remove(&ch);
                orphaned.push(e);
            }
            self.world.remove_body(handle);
        }
        // Pairs of the removed colliders stay in `touching`: the next step no longer finds them
        // and reports their end, as it does for a collider removed on its own.
        orphaned
    }
}

/// The rotation about Z of a quaternion (2D bodies ignore the other axes).
///
/// `atan2(2(xy + wz), 1 − 2(y² + z²))` — the ZYX euler yaw — with [`regia_core`]'s
/// deterministic `atan2`: glam's `to_euler` goes through the platform libm, and this runs for
/// every new 2D body and every kinematic body's step, so its last digit would split lockstep
/// peers (spec 15 § 5).
fn angle_of(q: Quat) -> f32 {
    regia_core::math::det_atan2(2.0 * (q.x * q.y + q.w * q.z), 1.0 - 2.0 * (q.y * q.y + q.z * q.z))
}

/// A rapier 2D rotation from `angle` radians. `Rotation::new` calls the platform `sin`/`cos`;
/// the stored cosine and sine come from `det_sin_cos` and are renormalized with IEEE `sqrt`
/// so the unit-length invariant holds (spec 15 § 5).
fn rotation_2d(angle: f32) -> rapier2d::math::Rotation {
    let (sin, cos) = regia_core::math::det_sin_cos(angle);
    let n = (sin * sin + cos * cos).sqrt();
    if n > 0.0 {
        rapier2d::math::Rotation::from_cos_sin_unchecked(cos / n, sin / n)
    } else {
        rapier2d::math::Rotation::identity()
    }
}

/// A Z rotation quaternion from `angle` radians, via the deterministic half-angle sine and cosine.
/// `Quat::from_rotation_z` uses the platform `sin_cos`.
fn quat_from_z(angle: f32) -> Quat {
    let (sin, cos) = regia_core::math::det_sin_cos(angle * 0.5);
    Quat::from_xyzw(0.0, 0.0, sin, cos)
}

/// The 2D physics chain in `FixedUpdate`.
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Physics2dStep;

/// Steps [`PhysicsWorld2d`] in `FixedUpdate` and keeps it in sync with the 2D components.
#[derive(Default)]
pub struct Physics2dPlugin;

impl Plugin for Physics2dPlugin {
    fn build(&self, app: &mut App) {
        // One entity can carry both 3D and 2D body components; run the 2D chain after the 3D
        // one and before the platformer so the `Transform` writer that wins is always the same.
        app.configure_sets(
            FixedUpdate,
            Physics2dStep
                .after(crate::PhysicsStep)
                .before(crate::platformer2d::PlatformerStep),
        );
        app.init_resource::<PhysicsWorld2d>()
            .add_observer(record_2d_body_removal)
            .add_observer(record_2d_collider_removal)
            .add_message::<Collision2d>()
            .register_type::<RigidBody2d>()
            .register_type::<Collider2d>()
            .register_type::<ColliderShape2d>()
            .register_type::<Velocity2d>()
            .add_systems(
                FixedUpdate,
                (
                    sync_2d_parked,
                    sync_2d_removed,
                    sync_2d_added,
                    sync_2d_inputs,
                    step_physics_2d,
                    writeback_2d,
                )
                    .chain()
                    .in_set(Physics2dStep),
            )
            .add_message::<regia_scene::streaming::OriginShifted>()
            .add_systems(
                FixedUpdate,
                shift_physics2d_origin
                    .after(regia_scene::streaming::StreamSet::Rebase)
                    .before(regia_scene::streaming::StreamSet::Plan),
            );
        // Insertion follows the order bodies arrive, not entity ids (ADR-1347).
        crate::plugin::must_track_spawn_order::<RigidBody2d>(app.world_mut());
        crate::plugin::must_track_spawn_order::<Collider2d>(app.world_mut());
    }
}

/// Observer: a `RigidBody2d` is leaving its entity (removal or despawn).
fn record_2d_body_removal(on: On<Remove<RigidBody2d>>, world: Option<ResMut<PhysicsWorld2d>>) {
    if let Some(mut world) = world {
        world.removed_bodies.push(on.event().entity);
    }
}

/// Observer: a `Collider2d` is leaving its entity (removal or despawn).
fn record_2d_collider_removal(on: On<Remove<Collider2d>>, world: Option<ResMut<PhysicsWorld2d>>) {
    if let Some(mut world) = world {
        world.removed_colliders.push(on.event().entity);
    }
}

/// Queues the rapier objects of parked data-layer members for [`sync_2d_removed`] (spec 37 § 13).
/// Their components stay; [`sync_2d_added`] rebuilds them, velocity included, once they are live.
#[allow(clippy::type_complexity)]
pub fn sync_2d_parked(
    mut world: ResMut<PhysicsWorld2d>,
    bodies: Query<Entity, (With<LayerParked>, With<Body2dHandle>)>,
    colliders: Query<Entity, (With<LayerParked>, With<Collider2dHandle>)>,
) {
    world.removed_bodies.extend(bodies.iter());
    world.removed_colliders.extend(colliders.iter());
}

/// Releases rapier bodies/colliders of despawned entities or removed components.
///
/// Removing `RigidBody2d` drops the body and the colliders attached to it, so their handle
/// components are cleared too and `sync_2d_added` rebuilds whatever components remain.
/// Removing only `Collider2d` leaves the body alone. Works from the records the plugin's
/// `Remove` observers keep, not from `RemovedComponents`, so a removal on a frame without a
/// fixed step is not lost.
pub fn sync_2d_removed(mut commands: Commands, mut world: ResMut<PhysicsWorld2d>) {
    let _span = tracing::debug_span!("sync_2d_removed").entered();
    // Removal order decides which slots rapier reuses next. Not entity ids (thread timing under
    // parallel `Commands::spawn`): the body handle, which follows insertion (spawn) order;
    // entities without a body here sort first, by id (ADR-2396).
    let mut gone_bodies: Vec<Entity> = std::mem::take(&mut world.removed_bodies);
    gone_bodies.sort_by_cached_key(|e| (world.bodies_of.get(e).map(|h| h.into_raw_parts()), *e));
    gone_bodies.dedup();
    for entity in &gone_bodies {
        for owner in world.remove_entity(*entity) {
            commands.entity(owner).try_remove::<Collider2dHandle>();
        }
        commands.entity(*entity).try_remove::<Body2dHandle>();
    }
    let mut gone_colliders: Vec<Entity> = std::mem::take(&mut world.removed_colliders);
    gone_colliders.sort_by_cached_key(|e| (world.colliders_of.get(e).map(|h| h.into_raw_parts()), *e));
    gone_colliders.dedup();
    for entity in gone_colliders {
        if world.remove_collider_of(entity) {
            commands.entity(entity).try_remove::<Collider2dHandle>();
        }
    }
}

/// Adds new bodies, then new colliders (attached to their entity's body when it has one), in
/// the order the components arrived (`regia_ecs::spawn_order`; entity ids are the fallback)
/// so the rapier world is built the same way every run, on any thread count (ADR-1347).
#[allow(clippy::type_complexity)]
#[tracing::instrument(skip_all, target = "regia_physics")]
pub fn sync_2d_added(
    mut commands: Commands,
    mut world: ResMut<PhysicsWorld2d>,
    bodies: Query<
        (Entity, &RigidBody2d, &Transform, &Velocity2d),
        (Without<Body2dHandle>, Without<LayerParked>),
    >,
    colliders: Query<(Entity, &Collider2d, &Transform), (Without<Collider2dHandle>, Without<LayerParked>)>,
    (body_order, collider_order): (
        Option<Res<regia_ecs::spawn_order::SpawnOrder<RigidBody2d>>>,
        Option<Res<regia_ecs::spawn_order::SpawnOrder<Collider2d>>>,
    ),
) {
    let _span = tracing::debug_span!("sync_2d_added").entered();
    let mut new_bodies: Vec<_> = bodies.iter().collect();
    new_bodies.sort_by_cached_key(|(e, ..)| crate::step::spawn_key(body_order.as_deref(), *e));
    for (entity, body, transform, velocity) in new_bodies {
        world.add_body(entity, body, transform, *velocity);
        if let Some(handle) = world.bodies_of.get(&entity).copied() {
            commands.entity(entity).insert(Body2dHandle(handle));
        }
    }
    let mut new_colliders: Vec<_> = colliders.iter().collect();
    new_colliders.sort_by_cached_key(|(e, ..)| crate::step::spawn_key(collider_order.as_deref(), *e));
    for (entity, collider, transform) in new_colliders {
        if let Some(handle) = world.add_collider(entity, collider, transform) {
            commands.entity(entity).insert(Collider2dHandle(handle));
        }
    }
}

/// Kinematic bodies follow their `Transform`; velocities changed by gameplay go into rapier.
#[allow(clippy::type_complexity)]
#[tracing::instrument(skip_all, target = "regia_physics")]
pub fn sync_2d_inputs(
    mut world: ResMut<PhysicsWorld2d>,
    kinematic: Query<(&Body2dHandle, &RigidBody2d, &Transform)>,
    velocities: Query<(&Body2dHandle, &Velocity2d), Changed<Velocity2d>>,
) {
    let _span = tracing::debug_span!("sync_2d_inputs").entered();
    for (handle, body, transform) in &kinematic {
        if body.kind != BodyKind::Kinematic {
            continue;
        }
        // A non-finite `Transform` never reaches the solver: the body keeps its last valid
        // pose (ADR-1507).
        if !transform.translation.is_finite() || !transform.rotation.is_finite() {
            world.non_finite_inputs += 1;
            continue;
        }
        if let Some(rb) = world.world.bodies.get_mut(handle.0) {
            rb.set_next_kinematic_translation(transform.translation.truncate());
            rb.set_next_kinematic_rotation(rotation_2d(angle_of(transform.rotation)));
        }
    }
    for (handle, velocity) in &velocities {
        if !velocity.linear.is_finite() || !velocity.angular.is_finite() {
            // The body keeps integrating its last valid velocity (ADR-1507).
            world.non_finite_inputs += 1;
            continue;
        }
        if let Some(rb) = world.world.bodies.get_mut(handle.0)
            && rb.is_dynamic()
            && (rb.linvel() != velocity.linear || rb.angvel() != velocity.angular)
        {
            rb.set_linvel(velocity.linear, true);
            rb.set_angvel(velocity.angular, true);
        }
    }
}

/// Begin/end events from one rapier step.
///
/// The buffer lives on the world so a quiet step does not allocate. Rapier's handler is
/// `Sync`, so the buffer sits in a thread-local for the step instead of inside the handler.
struct CollisionRecorder;

thread_local! {
    static COLLISION_EVENTS_2D: RefCell<Vec<(ColliderHandle, ColliderHandle, bool)>> =
        const { RefCell::new(Vec::new()) };
}

impl EventHandler for CollisionRecorder {
    fn handle_collision_event(
        &self,
        _bodies: &RigidBodySet,
        _colliders: &ColliderSet,
        event: CollisionEvent,
        _contact_pair: Option<&ContactPair>,
    ) {
        // `started()` and the handle getters take `self` by value. `CollisionEvent` is `Copy`.
        COLLISION_EVENTS_2D.with(|slot| {
            slot.borrow_mut()
                .push((event.collider1(), event.collider2(), event.started()));
        });
    }

    fn handle_contact_force_event(
        &self,
        _dt: rapier2d::math::Real,
        _bodies: &RigidBodySet,
        _colliders: &ColliderSet,
        _contact_pair: &ContactPair,
        _total_force_magnitude: rapier2d::math::Real,
    ) {
    }

    fn handle_soft_body_tear_event(&self, _soft_bodies: &SoftBodySet, _event: &SoftBodyTearEvent) {}
}

/// One rapier step at the fixed dt, then contact begin/end messages (sorted pairs).
///
/// The touching set is updated from the step's begin/end events. A step with no event
/// leaves the set alone. Ends are written before starts, both in collider-handle order: a
/// collider rebuilt this step (its body removed, the collider kept) ends its old pair and begins
/// the new one between the same entities, and a listener that tracks entity pairs must see the
/// begin last.
#[tracing::instrument(skip_all)]
pub fn step_physics_2d(
    fixed: Res<FixedTime>,
    mut world: ResMut<PhysicsWorld2d>,
    mut events: MessageWriter<Collision2d>,
) {
    let _span = tracing::debug_span!("step_physics_2d").entered();
    let world = &mut *world;
    world.world.integration_parameters.dt = fixed.step().as_secs_f32();
    let mut pending = std::mem::take(&mut world.pending_events);
    pending.clear();
    COLLISION_EVENTS_2D.with(|slot| {
        *slot.borrow_mut() = std::mem::take(&mut pending);
    });
    world.world.step_with_events(&(), &CollisionRecorder);
    COLLISION_EVENTS_2D.with(|slot| {
        pending = std::mem::take(&mut *slot.borrow_mut());
    });
    world.steps += 1;

    if !pending.is_empty() {
        // Last event of the step wins for a pair, matching the old end-of-step set diff.
        type HandleParts = (u32, u32);
        type ContactNet = BTreeMap<(HandleParts, HandleParts), bool>;
        let mut net: ContactNet = BTreeMap::new();
        for &(h1, h2, started) in &pending {
            let (ha, hb) = (h1.into_raw_parts(), h2.into_raw_parts());
            let key = if ha <= hb { (ha, hb) } else { (hb, ha) };
            net.insert(key, started);
        }
        // Ends first: a collider rebuilt this step (its body removed, the collider kept) ends its
        // old pair and begins the new one between the same entities; the begin must come last.
        for (&(collider_a, collider_b), &started) in &net {
            if started {
                continue;
            }
            let Some(t) = world
                .touching
                .iter()
                .find(|p| p.collider_a == collider_a && p.collider_b == collider_b)
                .copied()
            else {
                continue;
            };
            world.touching.remove(&t);
            events.write(Collision2d {
                a: t.entity_a,
                b: t.entity_b,
                started: false,
            });
        }
        for (&(collider_a, collider_b), &started) in &net {
            if !started {
                continue;
            }
            let h1 = ColliderHandle::from_raw_parts(collider_a.0, collider_a.1);
            let h2 = ColliderHandle::from_raw_parts(collider_b.0, collider_b.1);
            if let Some(t) = touching2d(&world.collider_entities, h1, h2)
                && world.touching.insert(t)
            {
                world.touching_inserts += 1;
                events.write(Collision2d {
                    a: t.entity_a,
                    b: t.entity_b,
                    started: true,
                });
            }
        }
    }
    pending.clear();
    world.pending_events = pending;
}

/// Moves every 2D body and free collider by a floating-origin rebase (the 2D twin of the 3D
/// hook): dynamic bodies are never read back from `Transform`, so without this the next
/// write-back would copy the pre-rebase position over the shifted `Transform`. Velocities stay.
#[tracing::instrument(skip_all, target = "regia_physics")]
pub(crate) fn shift_physics2d_origin(
    mut shifted: MessageReader<regia_scene::streaming::OriginShifted>,
    mut world: ResMut<PhysicsWorld2d>,
) {
    for message in shifted.read() {
        let shift = Vec2::new(message.rebase.shift[0], message.rebase.shift[1]);
        let world = &mut world.world;
        for (_, body) in world.bodies.iter_mut() {
            let moved = body.translation() - shift;
            body.set_translation(moved, false);
            if body.is_kinematic() {
                body.set_next_kinematic_translation(moved);
            }
        }
        for (_, collider) in world.colliders.iter_mut() {
            if collider.parent().is_none() {
                collider.set_translation(collider.translation() - shift);
            }
        }
        world
            .bodies
            .propagate_modified_body_positions_to_colliders(&mut world.colliders);
        let params = rapier2d::prelude::IntegrationParameters::default();
        for (handle, collider) in world.colliders.iter() {
            let aabb = collider.compute_broad_phase_aabb(&params, &world.bodies);
            world.broad_phase.set_aabb(&params, handle, aabb);
        }
    }
}

/// Dynamic bodies write their pose into `Transform` (z kept) and their velocity into
/// [`Velocity2d`] (without marking it changed, so it is not pushed back).
#[tracing::instrument(skip_all, target = "regia_physics")]
pub fn writeback_2d(
    mut world: ResMut<PhysicsWorld2d>,
    mut bodies: Query<(&Body2dHandle, &mut Transform, &mut Velocity2d)>,
) {
    let _span = tracing::debug_span!("writeback_2d").entered();
    for (handle, mut transform, mut velocity) in &mut bodies {
        // Copy everything out of the rapier borrow before touching the world's counter.
        let Some(rb) = world.world.bodies.get(handle.0) else {
            continue;
        };
        let dynamic = rb.is_dynamic();
        let translation = rb.translation();
        // `Rotation::angle` is the platform `atan2`. The stored sine and cosine are exact.
        let rotation = rb.rotation();
        let angle = regia_core::math::det_atan2(rotation.sin(), rotation.cos());
        let linvel = rb.linvel();
        let angvel = rb.angvel();
        if !dynamic {
            continue;
        }
        if !translation.is_finite() || !angle.is_finite() || !linvel.is_finite() || !angvel.is_finite() {
            // A non-finite simulated pose or velocity stays in the simulation; it is never
            // written into the rendered `Transform` or `Velocity2d` (ADR-1507).
            world.non_finite_inputs += 1;
            continue;
        }
        transform.translation.x = translation.x;
        transform.translation.y = translation.y;
        transform.rotation = quat_from_z(angle);
        let v = velocity.bypass_change_detection();
        v.linear = linvel;
        v.angular = angvel;
    }
}

/// A convex-hull builder for `points`, or `None` when parry would assert: fewer than three
/// points, a non-finite point, or all points equal (parry's 2D hull asserts on those).
fn convex_polygon_builder(points: &[Vec2]) -> Option<ColliderBuilder> {
    let first = *points.first()?;
    if points.len() < 3 || points.iter().any(|p| !p.is_finite()) || points.iter().all(|p| *p == first) {
        return None;
    }
    ColliderBuilder::convex_hull(points)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, Physics2dPlugin));
        enable_fixed_step(&mut app, 60.0);
        app
    }

    /// A pair that stays touching must not allocate a new contact-set node every step.
    #[test]
    fn a_resting_contact_does_not_grow_the_contact_set() {
        let mut app = app();
        app.world_mut().spawn((
            Collider2d::cuboid(4.0, 1.0),
            Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
        ));
        // Ground top is y = 0.5. The ball starts on that surface. Sleep stays off so the
        // resting pair remains an active contact (a sleeping body drops out of the set).
        app.world_mut().spawn((
            RigidBody2d {
                lock_rotation: true,
                ..RigidBody2d::default()
            },
            Collider2d {
                restitution: 0.0,
                ..Collider2d::ball(0.5)
            },
            Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
        ));
        let mut began = false;
        let mut note = |app: &App| {
            began |= app
                .world()
                .resource::<Messages<Collision2d>>()
                .iter_current_update_messages()
                .any(|c| c.started);
        };
        app.update();
        note(&app);
        {
            let mut world = app.world_mut().resource_mut::<PhysicsWorld2d>();
            for (_, body) in world.world.bodies.iter_mut() {
                if body.is_dynamic() {
                    // rapier 0.36 has no `set_can_sleep`. A negative threshold is what
                    // `RigidBodyBuilder::can_sleep(false)` writes.
                    let activation = body.activation_mut();
                    activation.normalized_linear_threshold = -1.0;
                    activation.angular_threshold = -1.0;
                }
            }
        }
        for _ in 0..40 {
            app.update();
            note(&app);
        }
        assert!(began, "the ball never came to rest on the ground");
        assert!(
            !app.world().resource::<PhysicsWorld2d>().touching.is_empty(),
            "the resting pair left the contact set before the measurement"
        );
        // This world's contact-set inserts and its event buffer as (address, capacity). A step
        // that inserted a pair node or grew the buffer changes the triple. Read per world, not
        // through a process-wide counter: other 2D tests step their own worlds on other threads
        // at the same time.
        let buffers = |app: &App| {
            let world = app.world().resource::<PhysicsWorld2d>();
            (
                world.touching_inserts,
                world.pending_events.as_ptr() as usize,
                world.pending_events.capacity(),
            )
        };
        let warm = buffers(&app);
        let mut messages = 0usize;
        let mut grown = 0usize;
        for _ in 0..32 {
            app.update();
            messages += app
                .world()
                .resource::<Messages<Collision2d>>()
                .iter_current_update_messages()
                .count();
            if buffers(&app) != warm {
                grown += 1;
            }
        }
        assert!(
            !app.world().resource::<PhysicsWorld2d>().touching.is_empty(),
            "the resting pair left the contact set during the measurement"
        );
        assert_eq!(messages, 0, "a resting pair emitted begin or end");
        assert_eq!(
            grown, 0,
            "the contact set or its event buffer grew on {grown} of 32 resting steps"
        );
    }

    #[test]
    fn a_ball_falls_onto_the_ground_and_reports_the_contact() {
        let mut app = app();
        app.world_mut().spawn((
            Collider2d::cuboid(20.0, 1.0),
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        ));
        let ball = app
            .world_mut()
            .spawn((
                RigidBody2d::default(),
                Collider2d::ball(0.5),
                Transform::from_translation(Vec3::new(0.0, 3.0, 2.0)),
            ))
            .id();
        let mut started = 0;
        for _ in 0..240 {
            app.update();
            let messages = app.world().resource::<Messages<Collision2d>>();
            started += messages
                .iter_current_update_messages()
                .filter(|c| c.started)
                .count();
        }
        let t = app
            .world()
            .get::<Transform>(ball)
            .map(|t| t.translation)
            .unwrap_or_default();
        assert!((t.y - 0.5).abs() < 0.02, "resting on the ground: {t}");
        assert_eq!(t.z, 2.0, "z is the draw order and is kept");
        assert!(started >= 1, "a contact began");
        let hit = app
            .world()
            .resource::<PhysicsWorld2d>()
            .raycast(Vec2::new(0.0, 5.0), -Vec2::Y, 10.0);
        assert_eq!(hit.map(|h| h.entity), Some(ball));
        assert!(hit.is_some_and(|h| (h.point.y - 1.0).abs() < 0.03 && h.normal.y > 0.99));
        assert_eq!(
            app.world()
                .resource::<PhysicsWorld2d>()
                .entities_at(Vec2::new(5.0, -0.5))
                .len(),
            1
        );
    }

    /// Resting contacts keep their begin/end messages without rebuilding the pair set.
    /// The second settled step inserts nothing into a contact-pair `BTreeSet`.
    #[test]
    fn contact_events_match_without_rebuilding_the_pair_set() {
        let mut app = app();
        // Eight separated boxes fall onto a floor and rest. The step after they have settled
        // must not rebuild the contact set.
        app.world_mut().spawn((
            Collider2d::cuboid(40.0, 1.0),
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        ));
        for i in 0..8 {
            app.world_mut().spawn((
                RigidBody2d::default(),
                Collider2d::cuboid(0.4, 0.4),
                Transform::from_translation(Vec3::new(i as f32 * 2.0 - 7.0, 1.5, 0.0)),
            ));
        }
        for _ in 0..300 {
            app.update();
        }
        let world = app.world().resource::<PhysicsWorld2d>();
        assert!(
            world.touching_inserts >= 8,
            "the boxes reached the floor, inserts {}",
            world.touching_inserts
        );
        assert!(world.all_asleep(), "the stack is at rest");
        let inserts = world.touching_inserts;
        app.update();
        let again: Vec<Collision2d> = app
            .world()
            .resource::<Messages<Collision2d>>()
            .iter_current_update_messages()
            .copied()
            .collect();
        let more = app.world().resource::<PhysicsWorld2d>().touching_inserts - inserts;
        assert!(
            again.is_empty(),
            "a settled step starts and ends nothing: {again:?}"
        );
        assert_eq!(
            more, 0,
            "a settled step inserted {more} contact-set nodes (the pair set was rebuilt)"
        );
    }

    #[test]
    fn kinematic_bodies_follow_their_transform_and_despawn_releases_bodies() {
        let mut app = app();
        let platform = app
            .world_mut()
            .spawn((
                RigidBody2d {
                    kind: BodyKind::Kinematic,
                    ..Default::default()
                },
                Collider2d::cuboid(2.0, 0.2),
                Transform::default(),
            ))
            .id();
        app.update();
        if let Some(mut t) = app.world_mut().get_mut::<Transform>(platform) {
            t.translation.x = 3.0;
        }
        app.update();
        app.update();
        let pw = app.world().resource::<PhysicsWorld2d>();
        let x = app
            .world()
            .get::<Body2dHandle>(platform)
            .and_then(|h| pw.world.bodies.get(h.0))
            .map(|b| b.translation().x);
        assert_eq!(x, Some(3.0));
        app.world_mut().despawn(platform);
        app.update();
        assert_eq!(app.world().resource::<PhysicsWorld2d>().body_count(), 0);
    }

    #[test]
    fn a_removal_on_a_frame_without_a_fixed_step_still_releases_the_body() {
        // 1 ms frames at a 60 Hz step: most frames run no step, and removal messages are cleared
        // before the next one would read them (ADR-1660).
        let mut app = app();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(1),
        ));
        let spawn = |app: &mut App, x: f32| {
            app.world_mut()
                .spawn((
                    RigidBody2d::default(),
                    Collider2d::ball(0.5),
                    Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
                ))
                .id()
        };
        let (a, b) = (spawn(&mut app, 0.0), spawn(&mut app, 5.0));
        let c = app
            .world_mut()
            .spawn((
                Collider2d::ball(0.5),
                Transform::from_translation(Vec3::new(9.0, 0.0, 0.0)),
            ))
            .id();
        for _ in 0..40 {
            app.update();
        }
        let counts = |app: &App| {
            let w = app.world().resource::<PhysicsWorld2d>();
            (w.body_count(), w.world.colliders.len())
        };
        assert_eq!(counts(&app), (2, 3));
        app.world_mut().despawn(a);
        if let Ok(mut b) = app.world_mut().get_entity_mut(b) {
            b.remove::<RigidBody2d>();
        }
        if let Ok(mut c) = app.world_mut().get_entity_mut(c) {
            c.remove::<Collider2d>();
        }
        for _ in 0..40 {
            app.update();
        }
        // `b` keeps its collider as a fixed (body-less) one; `a`'s and `c`'s are gone.
        assert_eq!(counts(&app), (0, 1));
    }

    #[test]
    fn an_origin_shift_moves_2d_bodies_and_the_write_back_keeps_the_new_frame() {
        use regia_scene::floating_origin::Rebase;
        use regia_scene::streaming::OriginShifted;
        let mut app = app();
        app.world_mut().resource_mut::<PhysicsWorld2d>().world.gravity = Vec2::ZERO;
        let ball = app
            .world_mut()
            .spawn((
                RigidBody2d::default(),
                Collider2d::ball(0.5),
                Transform::from_translation(Vec3::new(100.0, 7.0, 0.0)),
            ))
            .id();
        for _ in 0..3 {
            app.update();
        }
        // What the streaming rebase does: the entity's `Transform` and the message.
        let shift = [1024.0, 0.0, 0.0];
        app.world_mut()
            .get_mut::<Transform>(ball)
            .expect("transform")
            .translation
            .x -= shift[0];
        app.world_mut()
            .resource_mut::<Messages<OriginShifted>>()
            .write(OriginShifted {
                rebase: Rebase {
                    cell_delta: [1, 0, 0],
                    shift,
                },
                origin_cell: [1, 0, 0],
            });
        for _ in 0..3 {
            app.update();
        }
        let x = app
            .world()
            .get::<Transform>(ball)
            .expect("transform")
            .translation
            .x;
        assert!((x - (100.0 - 1024.0)).abs() < 1e-3, "x = {x}");
    }

    /// glibc `sin_cos` of half-angle `0x3c7ee806` recovers to `0x3cfee807`, and
    /// `Rotation::new` then stores cosine `0x3f7fe046`. Renormalized `det_sin_cos` of that
    /// angle is `0x3f7fe047`. A free collider's offset is that cosine (spec 15 § 5).
    #[test]
    fn a_free_collider_offset_uses_the_deterministic_rotation() {
        let half = f32::from_bits(0x3c7e_e806);
        let (half_sin, half_cos) = half.sin_cos();
        let rotation = Quat::from_xyzw(0.0, 0.0, half_sin, half_cos);
        let recovered = angle_of(rotation);
        let (sin, cos) = regia_core::math::det_sin_cos(recovered);
        let n = (sin * sin + cos * cos).sqrt();
        let expect_x = cos / n;
        let mut world = PhysicsWorld2d::default();
        let entity = Entity::from_raw_u32(11).expect("an index");
        let mut collider = Collider2d::ball(0.1);
        collider.offset = Vec2::new(1.0, 0.0);
        let handle = world
            .add_collider(
                entity,
                &collider,
                &Transform {
                    rotation,
                    ..Transform::default()
                },
            )
            .expect("collider");
        let pos = world
            .world
            .colliders
            .get(handle)
            .expect("inserted")
            .position()
            .translation;
        assert_eq!(
            pos.x.to_bits(),
            expect_x.to_bits(),
            "offset x {:08x} vs det {:08x} (angle {:08x})",
            pos.x.to_bits(),
            expect_x.to_bits(),
            recovered.to_bits()
        );
    }

    /// The Z angle of a spawn/kinematic rotation must come from the deterministic
    /// transcendental, not the platform libm (spec 15 § 5): a kinematic platform feeds this
    /// angle to rapier2d every fixed step, and a rider's lockstep replay would split on the
    /// last digit.
    #[test]
    fn angle_of_uses_the_deterministic_atan2() {
        for &turn in &[0.7f32, -1.3, 2.9, 0.1023, 0.35] {
            let q = Quat::from_rotation_z(turn);
            let expected = regia_core::math::det_atan2(
                2.0 * (q.x * q.y + q.w * q.z),
                1.0 - 2.0 * (q.y * q.y + q.z * q.z),
            );
            assert_eq!(angle_of(q).to_bits(), expected.to_bits(), "turn {turn}");
        }
        assert_eq!(angle_of(Quat::IDENTITY), 0.0);
    }

    /// A kinematic platform at a fixed rotation about Z drives a rider; repeating the scene
    /// in-process must give bit-identical rider poses (the angle path is pinned above).
    /// The dynamic write-back derives the angle with `det_atan2(im, re)` of the rapier2d
    /// rotation and rebuilds the quaternion with `det_sin_cos` of the half angle, so the
    /// gameplay `Transform` bits are the same on every platform (the rapier-internal bits
    /// already are). The pin is structural: it compares against the deterministic kernels
    /// applied to the rapier body's own rotation. On a host whose libm round-trips these
    /// values exactly the pre-fix code would also pass — the pin binds where they differ.
    #[test]
    fn writeback_2d_uses_the_deterministic_kernels() {
        let mut app = app();
        let body = app
            .world_mut()
            .spawn((
                RigidBody2d::default(),
                Collider2d::cuboid(0.5, 0.5),
                Transform {
                    rotation: Quat::from_rotation_z(0.7),
                    ..Transform::from_translation(Vec3::new(0.0, 3.0, 0.0))
                },
            ))
            .id();
        app.update();
        let rb_rot = {
            let handle = app.world().get::<Body2dHandle>(body).expect("the body handle").0;
            let world2d = app.world().resource::<PhysicsWorld2d>();
            let rb = world2d.world.bodies.get(handle).expect("the rapier body");
            *rb.rotation()
        };
        let angle = regia_core::math::det_atan2(rb_rot.im, rb_rot.re);
        let (sin, cos) = regia_core::math::det_sin_cos(angle * 0.5);
        let expected = Quat::from_xyzw(0.0, 0.0, sin, cos);
        let written = app
            .world()
            .get::<Transform>(body)
            .expect("the transform")
            .rotation;
        assert_eq!(
            written.to_array().map(f32::to_bits),
            expected.to_array().map(f32::to_bits),
            "write-back must use the deterministic kernels, not platform libm"
        );
    }

    #[test]
    fn a_rotated_kinematic_platform_replays_bit_identically() {
        let run = || {
            let mut app = app();
            app.world_mut().spawn((
                RigidBody2d {
                    kind: BodyKind::Kinematic,
                    ..Default::default()
                },
                Collider2d::cuboid(10.0, 0.5),
                Transform {
                    rotation: Quat::from_rotation_z(0.7),
                    ..Transform::default()
                },
            ));
            let rider = app
                .world_mut()
                .spawn((
                    RigidBody2d::default(),
                    Collider2d::cuboid(0.5, 0.5),
                    Transform::from_translation(Vec3::new(0.1, 1.0, 0.0)),
                ))
                .id();
            let mut bits = Vec::new();
            for _ in 0..120 {
                app.update();
                let t = app.world().get::<Transform>(rider).copied().unwrap_or_default();
                bits.push((
                    t.translation.x.to_bits(),
                    t.translation.y.to_bits(),
                    t.rotation.to_array().map(f32::to_bits),
                ));
            }
            bits
        };
        assert_eq!(run(), run(), "the rotated platform scene must replay exactly");
    }

    #[test]
    fn collider_shapes_are_sanitized_before_they_reach_the_solver() {
        let mut world = PhysicsWorld2d::default();
        let at = Transform::default();
        let collider = |shape: ColliderShape2d| Collider2d {
            shape,
            ..Collider2d::default()
        };
        let entity = Entity::from_raw_u32(7).expect("an index");
        let cuboid = collider(ColliderShape2d::Cuboid {
            half_extents: Vec2::new(-1.0, f32::NAN),
        });
        let handle = world.add_collider(entity, &cuboid, &at).expect("kept");
        let aabb = world.world.colliders[handle].compute_aabb();
        assert!(aabb.mins.x <= aabb.maxs.x && aabb.mins.y <= aabb.maxs.y);
        let segment = collider(ColliderShape2d::Segment {
            a: Vec2::ZERO,
            b: Vec2::new(f32::NAN, 0.0),
        });
        assert!(world.add_collider(entity, &segment, &at).is_none());
    }

    #[test]
    fn degenerate_polylines_build_no_collider() {
        let mut world = PhysicsWorld2d::default();
        let at = Transform::default();
        let collider = |shape: ColliderShape2d| Collider2d {
            shape,
            ..Collider2d::default()
        };
        let entity = Entity::from_raw_u32(9).expect("an index");
        let one_point = collider(ColliderShape2d::Polyline {
            points: vec![Vec2::ZERO],
        });
        assert!(world.add_collider(entity, &one_point, &at).is_none());
        let non_finite = collider(ColliderShape2d::Polyline {
            points: vec![Vec2::ZERO, Vec2::new(f32::INFINITY, 0.0)],
        });
        assert!(world.add_collider(entity, &non_finite, &at).is_none());
        let oversized = collider(ColliderShape2d::Polyline {
            points: vec![Vec2::ZERO; MAX_POLYLINE_POINTS + 1],
        });
        assert!(world.add_collider(entity, &oversized, &at).is_none());
        let ok = collider(ColliderShape2d::Polyline {
            points: vec![Vec2::ZERO, Vec2::X],
        });
        assert!(world.add_collider(entity, &ok, &at).is_some());
    }

    #[test]
    fn degenerate_convex_polygons_build_no_hull() {
        let v = Vec2::new;
        assert!(convex_polygon_builder(&[]).is_none());
        assert!(convex_polygon_builder(&[v(1.0, 1.0)]).is_none());
        assert!(convex_polygon_builder(&[v(1.0, 1.0), v(1.0, 1.0), v(1.0, 1.0)]).is_none());
        assert!(convex_polygon_builder(&[v(f32::NAN, 0.0), v(1.0, 0.0), v(0.0, 1.0)]).is_none());
        assert!(convex_polygon_builder(&[v(0.0, 0.0), v(1.0, 0.0), v(0.0, 1.0)]).is_some());
    }

    /// Every `Collision2d` written so far, in order (the test reads the message buffer after
    /// each update, so nothing expires unread).
    fn collect_collisions(app: &mut App, updates: usize, out: &mut Vec<Collision2d>) {
        for _ in 0..updates {
            app.update();
            let messages = app.world().resource::<Messages<Collision2d>>();
            out.extend(messages.iter_current_update_messages().copied());
        }
    }

    /// Audit g-physics2 finding 4: despawning (or parking) a body in contact dropped the pair
    /// from `touching` before the step that would have reported its end, so a listener that
    /// saw `started: true` never saw `started: false`.
    #[test]
    fn despawning_a_touching_body_reports_the_contact_end() {
        for park in [false, true] {
            let mut app = app();
            let ground = app
                .world_mut()
                .spawn((
                    Collider2d::cuboid(4.0, 1.0),
                    Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
                ))
                .id();
            // Resting on the ground's top (y = 0.5) under gravity: touching from its first steps.
            let body = app
                .world_mut()
                .spawn((
                    RigidBody2d::default(),
                    Collider2d::ball(0.5),
                    Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
                ))
                .id();
            let mut seen = Vec::new();
            collect_collisions(&mut app, 20, &mut seen);
            let involves = |c: &Collision2d| (c.a, c.b) == (ground, body) || (c.a, c.b) == (body, ground);
            assert_eq!(
                seen.iter().rev().find(|c| involves(c)).map(|c| c.started),
                Some(true),
                "the ball rests on the ground: {seen:?}"
            );
            if park {
                app.world_mut().entity_mut(body).insert(LayerParked {
                    restore: regia_scene::prelude::Visibility::default(),
                    inherited: false,
                });
            } else {
                app.world_mut().despawn(body);
            }
            seen.clear();
            collect_collisions(&mut app, 4, &mut seen);
            assert_eq!(
                seen.iter().filter(|c| involves(c) && !c.started).count(),
                1,
                "park = {park}: the end is reported once: {seen:?}"
            );
            assert_eq!(
                seen.iter().filter(|c| involves(c) && c.started).count(),
                0,
                "park = {park}: {seen:?}"
            );
        }
    }

    /// Audit g-physics2 finding 7: a free collider's `offset` turns with the entity's Z rotation,
    /// as it does on a body (where it is body-local).
    #[test]
    fn a_free_collider_offset_turns_with_the_entity_rotation() {
        let mut world = PhysicsWorld2d::default();
        let entity = Entity::from_raw_u32(3).expect("an index");
        let collider = Collider2d {
            offset: Vec2::new(2.0, 0.0),
            ..Collider2d::ball(0.25)
        };
        let at = Transform {
            translation: Vec3::new(1.0, 1.0, 0.0),
            rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
            ..Transform::default()
        };
        let handle = world.add_collider(entity, &collider, &at).expect("kept");
        let center = world.world.colliders[handle].translation();
        assert!(
            (center - Vec2::new(1.0, 3.0)).length() < 1e-5,
            "a quarter turn moves the (2, 0) offset to (0, 2): {center}"
        );

        // The same entity as a body puts the collider at the same place.
        let mut world = PhysicsWorld2d::default();
        world.add_body(entity, &RigidBody2d::default(), &at, Velocity2d::default());
        let handle = world.add_collider(entity, &collider, &at).expect("kept");
        let c = &world.world.colliders[handle];
        let on_body = c.translation();
        assert!((on_body - Vec2::new(1.0, 3.0)).length() < 1e-5, "{on_body}");
    }

    /// Audit g-physics2 finding 8: non-finite collider material, free pose and offset are
    /// substituted and counted; a polyline with a non-finite point is skipped like a segment.
    #[test]
    fn non_finite_collider_inputs_are_substituted_and_counted() {
        let entity = Entity::from_raw_u32(5).expect("an index");
        let at = Transform::default();
        for (collider, what) in [
            (
                Collider2d {
                    friction: f32::NAN,
                    ..Collider2d::default()
                },
                "friction",
            ),
            (
                Collider2d {
                    restitution: f32::INFINITY,
                    ..Collider2d::default()
                },
                "restitution",
            ),
            (
                Collider2d {
                    density: f32::NAN,
                    ..Collider2d::default()
                },
                "density",
            ),
            (
                Collider2d {
                    offset: Vec2::new(f32::NAN, 0.0),
                    ..Collider2d::default()
                },
                "offset",
            ),
        ] {
            let mut world = PhysicsWorld2d::default();
            let handle = world.add_collider(entity, &collider, &at).expect("kept");
            let c = &world.world.colliders[handle];
            assert!(
                c.friction().is_finite()
                    && c.restitution().is_finite()
                    && c.density().is_finite()
                    && c.translation().is_finite(),
                "{what}: friction {} restitution {} density {} at {}",
                c.friction(),
                c.restitution(),
                c.density(),
                c.translation()
            );
            assert_eq!(world.non_finite_inputs(), 1, "{what} counted once");
        }

        // A free collider at a non-finite pose is parked at the origin and counted, as a body is.
        for at in [
            Transform::from_translation(Vec3::new(f32::NAN, 0.0, 0.0)),
            Transform {
                rotation: Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0),
                ..Transform::default()
            },
        ] {
            let mut world = PhysicsWorld2d::default();
            let handle = world
                .add_collider(entity, &Collider2d::default(), &at)
                .expect("kept");
            let c = &world.world.colliders[handle];
            assert!(
                c.translation().is_finite() && c.rotation().angle().is_finite(),
                "{at:?} -> {} {}",
                c.translation(),
                c.rotation().angle()
            );
            assert_eq!(world.non_finite_inputs(), 1);
        }

        // A polyline with a non-finite point never reaches parry.
        let mut world = PhysicsWorld2d::default();
        let polyline = Collider2d {
            shape: ColliderShape2d::Polyline {
                points: vec![Vec2::ZERO, Vec2::new(f32::NAN, 0.0), Vec2::new(2.0, 0.0)],
            },
            ..Collider2d::default()
        };
        assert!(world.add_collider(entity, &polyline, &at).is_none());
        assert_eq!(world.world.colliders.len(), 0);
    }
}
