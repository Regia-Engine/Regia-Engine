// SPDX-License-Identifier: MPL-2.0
//! Spatial queries (specs/15 § 3): raycast, shape cast, overlap, closest point — as a
//! [`SystemParam`] so systems (and `ScriptCtx`-style helpers) can run them against the live
//! world.
//!
//! Every query takes a [`QueryFilter`] with the spec's fields: layers (a layer mask plus the
//! "belongs to" layer that selects the query-matrix row), excluded entities, sensor exclusion
//! and an entity predicate. Results carry the owning `Entity`, the hit point, normal and
//! distance/toi.
//!
//! Provenance: rapier `QueryPipeline` public API (Apache-2.0) — this module is a mapping layer.

use rapier3d::geometry::{ColliderHandle, Ray};
use rapier3d::parry::query::ShapeCastOptions;
use rapier3d::pipeline::QueryFilterFlags;
use regia_core::math::Vec3;
use regia_ecs::ecs::system::SystemParam;
use regia_ecs::prelude::*;
use regia_scene::Transform;

use crate::components::{ColliderShape, CollisionLayer};
use crate::layers::PhysicsLayers;
use crate::step::{shape_axis_rotation, shape_to_shared_shape};
use crate::world::PhysicsWorld;

/// One ray hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RaycastHit {
    /// The entity whose collider was hit.
    pub entity: Entity,
    /// World-space hit point.
    pub point: Vec3,
    /// World-space surface normal at the hit.
    pub normal: Vec3,
    /// Distance along the (normalized) ray.
    pub distance: f32,
}

/// One shape-cast hit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeCastHitInfo {
    /// The entity whose collider was hit.
    pub entity: Entity,
    /// World-space hit point on the collider.
    pub point: Vec3,
    /// World-space surface normal on the collider.
    pub normal: Vec3,
    /// Time of impact along the cast. The call normalizes `direction`, so this is always a
    /// distance in metres.
    pub time_of_impact: f32,
}

/// A `closest_point` result.
///
/// `normal` and `distance` describe the collider's surface nearest to the query point `q`
/// (ADR-7233): the nearest surface point is always `q - normal * distance`, whether `q` is
/// outside (distance > 0) or inside (distance < 0, the penetration depth).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ClosestPointResult {
    /// The entity whose collider is closest.
    pub entity: Entity,
    /// The closest point of that collider's solid (world space): the nearest surface point when
    /// the query point is outside, the query point itself when it is inside.
    pub point: Vec3,
    /// Whether the query point was inside the shape (or on its boundary).
    pub is_inside: bool,
    /// The outward unit surface normal at the nearest surface point (world space). On the
    /// surface itself it is the normal of the feature there (a face's normal).
    pub normal: Vec3,
    /// The signed distance from the query point to the collider's surface, in meters: positive
    /// outside, negative inside, zero on the surface.
    pub distance: f32,
}

/// Below this distance (meters) the query point counts as on the surface, and the normal comes
/// from the surface feature instead of the point-to-surface direction.
const ON_SURFACE: f32 = 1e-6;

/// The outward normal and signed distance from `point` to `collider`'s surface (ADR-7233).
///
/// Off the surface the normal is the direction between the point and its projection onto the
/// boundary (the gradient of the distance; exact for every shape). On the surface it is the
/// normal of the feature the point lies on (on a triangle mesh, of the triangle the projection
/// landed on); shapes that report no feature normal fall back to
/// the direction from the collider's center, then +Y.
fn surface_normal_and_distance(collider: &rapier3d::geometry::Collider, point: Vec3) -> (Vec3, f32) {
    let q = crate::convert::to_rapier_vec3(point);
    let surface = collider.shape().project_point(collider.position(), q, false);
    let delta = point - crate::convert::from_rapier_vec3(surface.point);
    let length = delta.length();
    if length.is_finite() && length > ON_SURFACE {
        return if surface.is_inside {
            (-delta / length, -length)
        } else {
            (delta / length, length)
        };
    }
    let pose = collider.position();
    // The projection names the sub-shape the feature belongs to (a mesh's triangle); a mesh picks
    // its face normal by it, so passing 0 would answer triangle 0's normal anywhere on the mesh.
    let (projection, feature) = collider.shape().project_point_and_get_feature(pose, q);
    let local = pose.inverse_transform_point(q);
    let feature_normal = collider
        .shape()
        .feature_normal_at_point(projection.subshape, feature, local)
        .map(|n| crate::convert::from_rapier_vec3(pose.rotation * n))
        .filter(|n| n.is_finite() && n.length_squared() > 0.0);
    let center = crate::convert::from_rapier_vec3(pose.translation);
    let normal = feature_normal
        .or_else(|| (point - center).try_normalize())
        .unwrap_or(Vec3::Y)
        .normalize_or(Vec3::Y);
    let distance = if length.is_finite() { 0.0 } else { f32::NAN };
    (normal, distance)
}

/// A ray origin deeper than this inside a shape started inside it (meters). An origin on the
/// surface (within this) keeps its time-of-impact-0 hit: a ray cast from a contact point still
/// touches that surface.
const INSIDE_START: f32 = 1.0e-3;

/// Whether `origin` lies deeper than [`INSIDE_START`] inside `collider`.
fn starts_inside(collider: &rapier3d::geometry::Collider, origin: Vec3) -> bool {
    let (_, distance) = surface_normal_and_distance(collider, origin);
    distance.is_finite() && distance < -INSIDE_START
}

/// The outward normal of a face a ray leaves a shape by: the side of `normal` that points along
/// `dir`. Parry's hollow cast from inside reports that face with the normal turned back into the
/// shape (against the ray), which a ground probe would read as a floor under it (ADR-8801).
fn exit_normal(normal: Vec3, dir: Vec3) -> Vec3 {
    if normal.dot(dir) < 0.0 { -normal } else { normal }
}

/// A world-space position in rapier's vector type.
fn to_rapier_point(v: Vec3) -> rapier3d::math::Vector {
    crate::convert::to_rapier_vec3(v)
}

/// Whether a (normalised) ray is castable: finite origin, a non-zero direction and a distance that
/// is neither NaN nor negative. Anything else is a miss, not a rapier panic or a bogus hit.
fn valid_ray(origin: Vec3, dir: Vec3, max_distance: f32) -> bool {
    origin.is_finite() && dir != Vec3::ZERO && !max_distance.is_nan() && max_distance >= 0.0
}

/// Whether a query pose is finite.
fn valid_pose(pose: &Transform) -> bool {
    pose.translation.is_finite() && pose.rotation.is_finite()
}

/// Which colliders a query may hit (specs/15 § 3 `QueryFilter`).
///
/// `layers` is a bitmask of target layer indices; `from_layer` additionally intersects it with
/// the query matrix row of that layer (a query "belonging to" a layer only sees what the
/// config allows).
#[derive(Default)]
pub struct QueryFilter<'a> {
    from_layer: Option<CollisionLayer>,
    layer_mask: Option<u32>,
    exclude_sensors: bool,
    exclude_entities: &'a [Entity],
    #[allow(clippy::type_complexity)]
    predicate: Option<&'a dyn Fn(Entity) -> bool>,
}

impl<'a> QueryFilter<'a> {
    /// Everything is hitable.
    pub const fn new() -> Self {
        Self {
            from_layer: None,
            layer_mask: None,
            exclude_sensors: false,
            exclude_entities: &[],
            predicate: None,
        }
    }

    /// The query belongs to this layer (selects a query-matrix row).
    pub fn from_layer(mut self, layer: CollisionLayer) -> Self {
        self.from_layer = Some(layer);
        self
    }

    /// Only hit colliders on these layers (bitmask `1 << layer_index`).
    pub fn layers(mut self, mask: u32) -> Self {
        self.layer_mask = Some(mask);
        self
    }

    /// Skip sensor colliders.
    pub fn exclude_sensors(mut self) -> Self {
        self.exclude_sensors = true;
        self
    }

    /// Never hit colliders owned by these entities.
    pub fn exclude_entities(mut self, entities: &'a [Entity]) -> Self {
        self.exclude_entities = entities;
        self
    }

    /// Only hit entities for which the predicate returns `true`.
    pub fn predicate(mut self, predicate: &'a dyn Fn(Entity) -> bool) -> Self {
        self.predicate = Some(predicate);
        self
    }
}

/// Physics scene queries against the live [`PhysicsWorld`] (specs/15 § 3).
///
/// A thin [`SystemParam`] wrapper over [`WorldQueries`], which holds the same two
/// resources directly — tooling that already owns a `&World` (the agent protocol's
/// `physics.*` handlers) builds [`WorldQueries`] without a system.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use regia_app::prelude::*;
/// use regia_core::math::Vec3;
/// use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsQuery, PhysicsStep, QueryFilter, RigidBody};
///
/// let mut app = App::new();
/// app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(1.0 / 60.0)));
/// let mut fixed = FixedTime::default();
/// fixed.hz = 60.0;
/// app.insert_resource(fixed);
/// app.add_plugins(PhysicsPlugin);
/// // A static ground plate whose top face sits at y = 0.
/// app.world_mut().spawn((
///     regia_scene::Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
///     RigidBody::static_body(),
///     Collider {
///         shape: ColliderShape::Box {
///             half_extents: Vec3::new(5.0, 0.5, 5.0),
///         },
///         ..Collider::default()
///     },
/// ));
///
/// // `PhysicsQuery` is a `SystemParam`: run the query from inside a system.
/// let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
/// let out = slot.clone();
/// // Order the query `.after(PhysicsStep)` so it never sees a half-synced world.
/// app.add_systems(
///     FixedUpdate,
///     (move |queries: PhysicsQuery| {
///         *out.lock().unwrap_or_else(|p| p.into_inner()) = queries
///             .raycast(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 20.0, QueryFilter::new())
///             .map(|hit| hit.distance);
///     })
///     .after(PhysicsStep),
/// );
/// for _ in 0..6 {
///     app.update(); // sync the collider into the rapier world, then step
/// }
///
/// let distance = slot.lock().unwrap_or_else(|p| p.into_inner()).expect("the ray hit");
/// assert!((distance - 10.0).abs() < 1e-3, "top face is 10 m below: {distance}");
/// ```
#[derive(SystemParam)]
pub struct PhysicsQuery<'w> {
    /// The rapier world.
    world: Res<'w, PhysicsWorld>,
    /// The layer table (absent → every layer interacts).
    layers: Option<Res<'w, PhysicsLayers>>,
}

impl PhysicsQuery<'_> {
    /// Casts a ray; returns the closest hit (or `None`).
    ///
    /// `direction` is normalized by the call. A ray that starts inside a shape reports the
    /// first surface it crosses, never its own origin ([`WorldQueries::raycast`]).
    pub fn raycast(
        &self,
        origin: Vec3,
        direction: Vec3,
        max_distance: f32,
        filter: QueryFilter<'_>,
    ) -> Option<RaycastHit> {
        self.queries().raycast(origin, direction, max_distance, filter)
    }

    /// The velocity of a hit entity's collider at `point` ([`WorldQueries::point_velocity`]).
    pub fn point_velocity(&self, entity: Entity, point: Vec3) -> Vec3 {
        self.queries().point_velocity(entity, point)
    }

    /// Casts a ray; returns every hit, sorted by distance.
    pub fn raycast_all(
        &self,
        origin: Vec3,
        direction: Vec3,
        max_distance: f32,
        filter: QueryFilter<'_>,
    ) -> Vec<RaycastHit> {
        self.queries()
            .raycast_all(origin, direction, max_distance, filter)
    }

    /// Casts `shape` from `from` along `direction` (up to `max_distance` metres).
    pub fn shapecast(
        &self,
        shape: &ColliderShape,
        from: Transform,
        direction: Vec3,
        max_distance: f32,
        filter: QueryFilter<'_>,
    ) -> Option<ShapeCastHitInfo> {
        self.queries()
            .shapecast(shape, from, direction, max_distance, filter)
    }

    /// Every entity whose collider intersects `shape` at `pose`, in collider-handle order
    /// (the same order every run, whatever the thread count; ADR-1347).
    pub fn overlap(&self, shape: &ColliderShape, pose: Transform, filter: QueryFilter<'_>) -> Vec<Entity> {
        self.queries().overlap(shape, pose, filter)
    }

    /// The closest point on any collider within `max_distance` of `point`.
    pub fn closest_point(
        &self,
        point: Vec3,
        max_distance: f32,
        filter: QueryFilter<'_>,
    ) -> Option<ClosestPointResult> {
        self.queries().closest_point(point, max_distance, filter)
    }

    /// The same queries over borrowed resources.
    fn queries(&self) -> WorldQueries<'_> {
        WorldQueries {
            world: &self.world,
            layers: self.layers.as_deref(),
        }
    }
}

/// The query entry points over borrowed resources (specs/15 § 3): the system parameter
/// delegates here, and tooling that holds a `&World` (the agent protocol) builds this
/// directly.
///
/// # Example
///
/// ```
/// use std::time::Duration;
/// use regia_app::prelude::*;
/// use regia_core::math::Vec3;
/// use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, QueryFilter, RigidBody, WorldQueries};
///
/// let mut app = App::new();
/// app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(1.0 / 60.0)));
/// let mut fixed = FixedTime::default();
/// fixed.hz = 60.0;
/// app.insert_resource(fixed);
/// app.add_plugins(PhysicsPlugin);
/// app.world_mut().spawn((
///     regia_scene::Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
///     RigidBody::static_body(),
///     Collider {
///         shape: ColliderShape::Box {
///             half_extents: Vec3::new(5.0, 0.5, 5.0),
///         },
///         ..Collider::default()
///     },
/// ));
/// for _ in 0..6 {
///     app.update(); // sync the collider into the rapier world
/// }
///
/// let world = app.world().resource::<PhysicsWorld>();
/// let queries = WorldQueries { world, layers: None };
/// let hit = queries
///     .raycast(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 20.0, QueryFilter::new())
///     .expect("the ray hits the ground plate");
/// assert!((hit.distance - 10.0).abs() < 1e-3, "top face is 10 m below: {}", hit.distance);
/// ```
pub struct WorldQueries<'a> {
    /// The rapier world.
    pub world: &'a PhysicsWorld,
    /// The layer table (absent → every layer interacts).
    pub layers: Option<&'a PhysicsLayers>,
}

impl WorldQueries<'_> {
    /// The group mask for a query (query matrix row of `from` ∩ requested mask).
    fn query_groups(&self, filter: &QueryFilter<'_>) -> rapier3d::geometry::InteractionGroups {
        match self.layers {
            Some(layers) => layers.query_groups(filter.from_layer.as_ref(), filter.layer_mask),
            None => rapier3d::geometry::InteractionGroups::all(),
        }
    }

    /// Runs `f` with a query-pipeline view restricted by `filter`.
    ///
    /// A collider with no entity (a raw rapier collider inserted outside the ECS sync, a
    /// `destruction::add_fixed_box` wall, a destruction collider before its entity spawns) is
    /// never a candidate: every query reports entities, so such a collider closest to the query
    /// used to turn a single-hit query into a miss instead of letting the next collider answer
    /// (ADR-7417). The multi-hit queries already skipped it.
    fn with_pipeline<T>(
        &self,
        filter: &QueryFilter<'_>,
        f: impl FnOnce(&rapier3d::pipeline::QueryPipeline<'_>) -> T,
    ) -> T {
        let entity_allowed = |handle: ColliderHandle| -> bool {
            let Some(entity) = self.world.entity_of_collider(handle) else {
                return false;
            };
            !filter.exclude_entities.contains(&entity) && filter.predicate.map(|p| p(entity)).unwrap_or(true)
        };
        let pred =
            move |handle: ColliderHandle, _collider: &rapier3d::geometry::Collider| entity_allowed(handle);
        let predicate: &dyn Fn(rapier3d::geometry::ColliderHandle, &rapier3d::geometry::Collider) -> bool =
            &pred;
        let rapier_filter = rapier3d::pipeline::QueryFilter {
            flags: if filter.exclude_sensors {
                QueryFilterFlags::EXCLUDE_SENSORS
            } else {
                QueryFilterFlags::empty()
            },
            groups: Some(self.query_groups(filter)),
            exclude_collider: None,
            exclude_rigid_body: None,
            predicate: Some(predicate),
        };
        let pipeline = self.world.query_pipeline(rapier_filter);
        f(&pipeline)
    }

    /// Casts a ray; returns the closest hit (or `None`).
    ///
    /// `direction` is normalized by the call. A ray that starts on a surface reports it at
    /// distance 0. A ray that starts inside a shape (deeper than 1 mm) reports the first surface
    /// it crosses: the face where it leaves that shape, or a nearer surface of another shape. It
    /// never reports its own origin as a hit there (ADR-8721). The normal of a face the ray leaves
    /// by points out of that shape, along the ray (ADR-8801), so it never reads as a surface the
    /// ray came down onto.
    pub fn raycast(
        &self,
        origin: Vec3,
        direction: Vec3,
        max_distance: f32,
        filter: QueryFilter<'_>,
    ) -> Option<RaycastHit> {
        let dir = direction.normalize_or_zero();
        if !valid_ray(origin, dir, max_distance) {
            return None;
        }
        let ray = Ray::new(to_rapier_point(origin), crate::convert::to_rapier_vec3(dir));
        self.with_pipeline(&filter, |pipeline| {
            let inside = |handle: ColliderHandle| {
                pipeline
                    .colliders
                    .get(handle)
                    .is_some_and(|collider| starts_inside(collider, origin))
            };
            let mut best = pipeline.cast_ray_and_get_normal(&ray, max_distance, true);
            // Whether `best` is the face the ray leaves its starting shape by.
            let mut exit = false;
            // A solid cast that starts inside a shape stops at once (time of impact 0 at the
            // origin). Cast on, hollow, to the first surface the ray crosses.
            if best
                .as_ref()
                .is_some_and(|(handle, hit)| hit.time_of_impact <= INSIDE_START && inside(*handle))
            {
                best = pipeline.cast_ray_and_get_normal(&ray, max_distance, false);
                // The hollow cast reports the face the ray leaves by with a normal that points
                // back into the shape. A nearer hit on another shape is an entry and keeps its
                // outward normal.
                exit = best.as_ref().is_some_and(|(handle, _)| inside(*handle));
            }
            // ADR-8715: a vertical ray exactly on a heightfield cell line can miss in parry;
            // a nearer rescued heightfield hit (an entry) replaces the pipeline's answer.
            for (handle, toi, normal) in heightfield_line_hits(pipeline, &ray, max_distance) {
                if best.as_ref().is_none_or(|(_, hit)| toi < hit.time_of_impact) {
                    best = Some((
                        handle,
                        rapier3d::parry::query::RayIntersection::new(
                            toi,
                            normal,
                            rapier3d::parry::shape::FeatureId::Unknown,
                        ),
                    ));
                    exit = false;
                }
            }
            let (handle, hit) = best?;
            let mut normal = crate::convert::from_rapier_vec3(hit.normal);
            if exit {
                normal = exit_normal(normal, dir);
            }
            let entity = self.world.entity_of_collider(handle)?;
            Some(RaycastHit {
                entity,
                point: origin + dir * hit.time_of_impact,
                normal,
                distance: hit.time_of_impact,
            })
        })
    }

    /// The velocity of `entity`'s collider at the world point `point`, meters per second: its
    /// body's linear velocity plus the body's spin about its center of mass (the formula the
    /// character controller uses for moving ground). `entity` is a query hit's entity. Zero for
    /// an entity without a collider, a collider without a body, or a body that does not move.
    pub fn point_velocity(&self, entity: Entity, point: Vec3) -> Vec3 {
        if !point.is_finite() {
            return Vec3::ZERO;
        }
        let Some(handle) = self.world.collider_handle_of(entity) else {
            return Vec3::ZERO;
        };
        let velocity = crate::character::ground_point_velocity(self.world, handle, point);
        if velocity.is_finite() {
            velocity
        } else {
            Vec3::ZERO
        }
    }

    /// Casts a ray; returns every hit, sorted by distance. A shape the ray starts inside (deeper
    /// than 1 mm) is reported where the ray leaves it, with that face's outward normal, as
    /// [`Self::raycast`] does; one it does
    /// not leave within `max_distance` is not reported. A total order: distance ties
    /// (two colliders the ray meets at the same distance, e.g. coincident boxes) break by
    /// collider handle (rapier's index and generation: the index is reused as colliders are
    /// removed, so the order is deterministic for one scene history, not insertion order),
    /// and hits with a non-finite time of impact,
    /// normal or point are dropped — `partial_cmp` would compare a NaN equal to everything
    /// and the sort could return an arbitrary permutation (Rust's sort may even panic on a
    /// non-total comparator).
    pub fn raycast_all(
        &self,
        origin: Vec3,
        direction: Vec3,
        max_distance: f32,
        filter: QueryFilter<'_>,
    ) -> Vec<RaycastHit> {
        let dir = direction.normalize_or_zero();
        if !valid_ray(origin, dir, max_distance) {
            return Vec::new();
        }
        let ray = Ray::new(to_rapier_point(origin), crate::convert::to_rapier_vec3(dir));
        self.with_pipeline(&filter, |pipeline| {
            let rescued = heightfield_line_hits(pipeline, &ray, max_distance)
                .into_iter()
                .map(|(handle, toi, normal)| {
                    (
                        handle,
                        rapier3d::parry::query::RayIntersection::new(
                            toi,
                            normal,
                            rapier3d::parry::shape::FeatureId::Unknown,
                        ),
                    )
                });
            let mut hits: Vec<(RaycastHit, (u32, u32))> = pipeline
                .intersect_ray(ray, max_distance, true)
                .map(|(handle, _collider, hit)| (handle, hit))
                .chain(rescued)
                .filter_map(|(handle, hit)| {
                    let entity = self.world.entity_of_collider(handle)?;
                    let (hit, normal) = if hit.time_of_impact <= INSIDE_START
                        && let Some(collider) = pipeline.colliders.get(handle)
                        && starts_inside(collider, origin)
                    {
                        // The face this shape is left by; its normal points out, along the ray.
                        let exit = collider.shape().cast_ray_and_get_normal(
                            collider.position(),
                            &ray,
                            max_distance,
                            false,
                        )?;
                        (
                            exit,
                            exit_normal(crate::convert::from_rapier_vec3(exit.normal), dir),
                        )
                    } else {
                        (hit, crate::convert::from_rapier_vec3(hit.normal))
                    };
                    let point = origin + dir * hit.time_of_impact;
                    if !hit.time_of_impact.is_finite() || !normal.is_finite() || !point.is_finite() {
                        return None;
                    }
                    Some((
                        RaycastHit {
                            entity,
                            point,
                            normal,
                            distance: hit.time_of_impact,
                        },
                        handle.into_raw_parts(),
                    ))
                })
                .collect();
            hits.sort_by(|a, b| a.0.distance.total_cmp(&b.0.distance).then(a.1.cmp(&b.1)));
            hits.into_iter().map(|(hit, _)| hit).collect()
        })
    }

    /// Casts `shape` from `from` along `direction` (up to `max_distance` metres).
    pub fn shapecast(
        &self,
        shape: &ColliderShape,
        from: Transform,
        direction: Vec3,
        max_distance: f32,
        filter: QueryFilter<'_>,
    ) -> Option<ShapeCastHitInfo> {
        if !valid_pose(&from) || !direction.is_finite() || max_distance.is_nan() || max_distance < 0.0 {
            return None;
        }
        let Some(shared) = shape_to_shared_shape(shape) else {
            tracing::warn!(target: "regia_physics", "shapecast with an invalid shape; reporting no hit");
            return None;
        };
        let axis_rot = shape_axis_rotation(shape);
        let pose = crate::convert::to_rapier_pose(from.translation, from.rotation * axis_rot);
        let velocity = crate::convert::to_rapier_vec3(direction.normalize_or_zero());
        let options = ShapeCastOptions {
            max_time_of_impact: max_distance,
            target_distance: 0.0,
            stop_at_penetration: true,
            compute_impact_geometry_on_penetration: true,
        };
        self.with_pipeline(&filter, |pipeline| {
            let (handle, hit) = pipeline.cast_shape(&pose, velocity, &*shared, options)?;
            let entity = self.world.entity_of_collider(handle)?;
            Some(ShapeCastHitInfo {
                entity,
                point: crate::convert::from_rapier_vec3(hit.witness1),
                normal: crate::convert::from_rapier_vec3(hit.normal1),
                time_of_impact: hit.time_of_impact,
            })
        })
    }

    /// Every entity whose collider's bounding box touches the box `min..=max`, in
    /// collider-handle (rapier insertion) order — not the broad phase's, which follows
    /// rapier handle history, and not entity-id order, which parallel `Commands::spawn`
    /// reserves in thread-timing order (ADR-1347) — so effects applied in this order are
    /// the same every run. Conservative: a collider that only comes near is reported too.
    /// Unlike [`WorldQueries::overlap`] it sees heightfields, which have no exact shape test.
    pub fn overlap_aabb(&self, min: Vec3, max: Vec3, filter: QueryFilter<'_>) -> Vec<Entity> {
        if !min.is_finite() || !max.is_finite() {
            return Vec::new();
        }
        let aabb = rapier3d::parry::bounding_volume::Aabb::new(
            crate::convert::to_rapier_vec3(min),
            crate::convert::to_rapier_vec3(max),
        );
        let mut hits: Vec<(ColliderHandle, Entity)> = self.with_pipeline(&filter, |pipeline| {
            pipeline
                .intersect_aabb_conservative(aabb)
                .filter_map(|(handle, _collider)| self.world.entity_of_collider(handle).map(|e| (handle, e)))
                .collect()
        });
        hits.sort_by_key(|(handle, _)| handle.into_raw_parts());
        hits.into_iter().map(|(_, entity)| entity).collect()
    }

    /// Every entity whose collider intersects `shape` at `pose`, in collider-handle (rapier
    /// insertion) order — not the broad phase's, which follows rapier handle history, and
    /// not entity-id order, which parallel `Commands::spawn` reserves in thread-timing order
    /// (ADR-1347) — so effects applied in this order are the same every run.
    pub fn overlap(&self, shape: &ColliderShape, pose: Transform, filter: QueryFilter<'_>) -> Vec<Entity> {
        if !valid_pose(&pose) {
            return Vec::new();
        }
        let Some(shared) = shape_to_shared_shape(shape) else {
            tracing::warn!(target: "regia_physics", "overlap with an invalid shape; reporting no overlaps");
            return Vec::new();
        };
        let axis_rot = shape_axis_rotation(shape);
        let rapier_pose = crate::convert::to_rapier_pose(pose.translation, pose.rotation * axis_rot);
        let mut hits: Vec<(ColliderHandle, Entity)> = self.with_pipeline(&filter, |pipeline| {
            pipeline
                .intersect_shape(rapier_pose, &*shared)
                .filter_map(|(handle, _collider)| self.world.entity_of_collider(handle).map(|e| (handle, e)))
                .collect()
        });
        hits.sort_by_key(|(handle, _)| handle.into_raw_parts());
        hits.into_iter().map(|(_, entity)| entity).collect()
    }

    /// The closest point on any collider within `max_distance` of `point`, with the outward
    /// surface normal and signed distance there ([`ClosestPointResult`]).
    pub fn closest_point(
        &self,
        point: Vec3,
        max_distance: f32,
        filter: QueryFilter<'_>,
    ) -> Option<ClosestPointResult> {
        if !point.is_finite() || max_distance.is_nan() || max_distance < 0.0 {
            return None;
        }
        self.with_pipeline(&filter, |pipeline| {
            let (handle, projection) =
                pipeline.project_point(crate::convert::to_rapier_vec3(point), max_distance, true)?;
            let entity = self.world.entity_of_collider(handle)?;
            let collider = self.world.colliders.get(handle)?;
            let (normal, distance) = surface_normal_and_distance(collider, point);
            if !distance.is_finite() {
                return None;
            }
            Some(ClosestPointResult {
                entity,
                point: crate::convert::from_rapier_vec3(projection.point),
                is_inside: projection.is_inside,
                normal,
                distance,
            })
        })
    }
}

/// Inclusive barycentric slack of [`heightfield_vertical_hit`] (fraction of a triangle): a ray on a
/// shared edge belongs to both triangles, whatever way their coordinates round.
const HEIGHTFIELD_EDGE_SLACK: f32 = 1.0e-5;

/// Heightfield hits parry's own ray cast missed for a vertical ray (ADR-8715): for every
/// heightfield collider the ray's vertical line passes through (the pipeline's filter applies)
/// whose `cast_ray` reports nothing, [`heightfield_vertical_hit`]'s answer, as
/// `(collider, time of impact, world normal)`. Empty for a ray that is not exactly vertical, so
/// every other query is the pipeline's, bit for bit.
fn heightfield_line_hits(
    pipeline: &rapier3d::pipeline::QueryPipeline<'_>,
    ray: &Ray,
    max_toi: f32,
) -> Vec<(ColliderHandle, f32, rapier3d::parry::math::Vector)> {
    use rapier3d::parry::bounding_volume::Aabb;
    let (o, d) = (ray.origin, ray.dir);
    if d.x != 0.0 || d.z != 0.0 || d.y == 0.0 {
        return Vec::new();
    }
    // The vertical segment's box; `max_toi` may be infinite (an infinite box edge is fine, a
    // `0 * inf` would not be, so the box is built per axis).
    let (lo, hi) = if d.y < 0.0 {
        (o.y - max_toi, o.y)
    } else {
        (o.y, o.y + max_toi)
    };
    let aabb = Aabb::new(
        rapier3d::parry::math::Vector::new(o.x, lo, o.z),
        rapier3d::parry::math::Vector::new(o.x, hi, o.z),
    );
    let mut out = Vec::new();
    for (handle, collider) in pipeline.intersect_aabb_conservative(aabb) {
        let Some(field) = collider.shape().as_heightfield() else {
            continue;
        };
        if collider
            .shape()
            .cast_ray(collider.position(), ray, max_toi, true)
            .is_some()
        {
            continue;
        }
        if let Some((toi, normal)) = heightfield_vertical_hit(field, collider.position(), ray, max_toi) {
            out.push((handle, toi, normal));
        }
    }
    out
}

/// The nearest hit of a ray that is vertical in `field`'s frame, from the triangles of the 3 x 3
/// cells around it, with an inclusive barycentric test (ADR-8715). parry 0.31 tests only the one
/// cell its `cell_at_point` picks for such a ray, and that pick and the triangles' own edge
/// coordinates round apart: a ray exactly on a cell line (every simulation sample of a
/// grid-origin map, ADR-8179) missed about one time in four. `(time of impact, world normal
/// facing the ray)`; `None` off the field, over a removed cell, or outside `0..=max_toi`.
fn heightfield_vertical_hit(
    field: &rapier3d::parry::shape::HeightField,
    pose: &rapier3d::parry::math::Pose,
    ray: &Ray,
    max_toi: f32,
) -> Option<(f32, rapier3d::parry::math::Vector)> {
    let local = ray.inverse_transform_by(pose);
    let (o, d) = (local.origin, local.dir);
    if d.x != 0.0 || d.z != 0.0 || d.y == 0.0 {
        return None;
    }
    let half = field.scale() * 0.5;
    if o.x.abs() > half.x || o.z.abs() > half.z {
        return None;
    }
    let (i, j) = field.closest_cell_at_point(o);
    let mut best: Option<(f32, rapier3d::parry::math::Vector)> = None;
    for ci in i.saturating_sub(1)..=i + 1 {
        for cj in j.saturating_sub(1)..=j + 1 {
            let (first, second) = field.triangles_at(ci, cj);
            for tri in [first, second].into_iter().flatten() {
                let Some(y) = vertical_height(&tri, o.x, o.z) else {
                    continue;
                };
                let toi = (y - o.y) / d.y;
                if !(0.0..=max_toi).contains(&toi) || best.is_some_and(|(b, _)| b <= toi) {
                    continue;
                }
                let Some(n) = tri.normal() else {
                    continue;
                };
                let n = if n.dot(d) > 0.0 { -n } else { n };
                best = Some((toi, pose.transform_vector(n)));
            }
        }
    }
    best
}

/// The height of `tri` over `(x, z)`, if the point is inside its XZ projection (edges
/// included, with [`HEIGHTFIELD_EDGE_SLACK`]); `None` for a triangle seen edge-on from above.
fn vertical_height(tri: &rapier3d::parry::shape::Triangle, x: f32, z: f32) -> Option<f32> {
    let (a, b, c) = (tri.a, tri.b, tri.c);
    let area = (b.x - a.x) * (c.z - a.z) - (c.x - a.x) * (b.z - a.z);
    if area.abs() <= f32::EPSILON * (b.x - a.x).abs().max((c.z - a.z).abs()).max(1.0) {
        return None;
    }
    let wa = ((b.x - x) * (c.z - z) - (c.x - x) * (b.z - z)) / area;
    let wb = ((c.x - x) * (a.z - z) - (a.x - x) * (c.z - z)) / area;
    let wc = 1.0 - wa - wb;
    if wa < -HEIGHTFIELD_EDGE_SLACK || wb < -HEIGHTFIELD_EDGE_SLACK || wc < -HEIGHTFIELD_EDGE_SLACK {
        return None;
    }
    Some(wa * a.y + wb * b.y + wc * c.y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filter_defaults_allow_everything() {
        let f = QueryFilter::new();
        assert!(f.from_layer.is_none());
        assert!(f.layer_mask.is_none());
        assert!(!f.exclude_sensors);
        assert!(f.exclude_entities.is_empty());
        assert!(f.predicate.is_none());
    }

    #[test]
    fn filter_builders_stack() {
        let excluded = [Entity::PLACEHOLDER];
        let keep = |_e: Entity| true;
        let f = QueryFilter::new()
            .layers(0b011)
            .from_layer(CollisionLayer::new(1))
            .exclude_sensors()
            .exclude_entities(&excluded)
            .predicate(&keep);
        assert_eq!(f.layer_mask, Some(0b011));
        assert!(f.exclude_sensors);
        assert_eq!(f.exclude_entities.len(), 1);
    }
}
