// SPDX-License-Identifier: MPL-2.0
//! `world_pick`: the nearest surface along a ray over terrain and mesh entities, with the
//! [`MapLayer`] it belongs to.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use regia_core::math::{Mat4, Ray, Vec3};
use regia_ecs::ecs::entity::Entity;
use regia_ecs::prelude::World;
use regia_ecs::spawn_order::{SpawnOrder, track_spawn_order};
use regia_render::{
    CpuMesh, MeshMorphTargets, MeshRenderer, SkinInfluences8, SkinJoints, SkinningMethod,
    resolve_skinning_options,
};
use regia_scene::components::MorphWeights;
use regia_scene::layers::LayerParked;
use regia_scene::prelude::{InheritedVisibility, MapLayer, Visibility};

use crate::bvh::{bvh_for, sweep_dead};
use crate::mesh::{FaceCull, MeshHit, PoseSkin};
use crate::terrain::raycast_terrain_entity;

/// A set of [`MapLayer`] values (all 256 possible layers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LayerSet([u64; 4]);

impl LayerSet {
    /// Every layer.
    pub const ALL: Self = Self([u64::MAX; 4]);
    /// No layer.
    pub const NONE: Self = Self([0; 4]);

    /// Exactly `layers`.
    ///
    /// ```
    /// use regia_pick::LayerSet;
    ///
    /// let mut set = LayerSet::only(&[0, 4]);
    /// set.insert(7);
    /// assert_eq!(set.layers(), vec![0, 4, 7]);
    /// ```
    #[must_use]
    pub fn only(layers: &[u8]) -> Self {
        let mut set = Self::NONE;
        for &layer in layers {
            set.insert(layer);
        }
        set
    }

    /// Adds `layer`.
    pub fn insert(&mut self, layer: u8) {
        self.0[usize::from(layer) / 64] |= 1 << (layer % 64);
    }

    /// Whether `layer` is in the set.
    #[must_use]
    pub fn contains(&self, layer: u8) -> bool {
        self.0[usize::from(layer) / 64] & (1 << (layer % 64)) != 0
    }

    /// The layers in the set, ascending.
    #[must_use]
    pub fn layers(&self) -> Vec<u8> {
        (0..=255_u8).filter(|l| self.contains(*l)).collect()
    }
}

impl Default for LayerSet {
    fn default() -> Self {
        Self::ALL
    }
}

/// What a [`world_pick`] may hit.
#[derive(Debug, Clone)]
pub struct PickFilter {
    /// Farthest hit, metres (default: unbounded).
    pub max_distance: f32,
    /// Layers that can be hit (default: all). An entity without a [`MapLayer`] is layer 0.
    pub layers: LayerSet,
    /// Which mesh-triangle faces count (default: both, like the id pass). Terrain ignores this:
    /// a terrain hit is only a crossing from above.
    pub cull: FaceCull,
    /// Test terrain heightfields (default: yes).
    pub terrain: bool,
    /// Test mesh renderers (default: yes).
    pub meshes: bool,
    /// Test entities whose own or inherited [`Visibility`] is `Hidden` too (default: no).
    /// Members of a parked data layer stay out either way.
    pub include_hidden: bool,
    /// Mesh or terrain entities to ignore (each mesh entity of a model, not just its root), e.g.
    /// the shooter's own body.
    pub skip: Vec<Entity>,
}

impl Default for PickFilter {
    fn default() -> Self {
        Self {
            max_distance: f32::INFINITY,
            layers: LayerSet::ALL,
            cull: FaceCull::None,
            terrain: true,
            meshes: true,
            include_hidden: false,
            skip: Vec::new(),
        }
    }
}

impl PickFilter {
    /// A filter restricted to `layers`.
    ///
    /// ```
    /// use regia_pick::PickFilter;
    ///
    /// let filter = PickFilter::on_layers(&[1, 2]);
    /// assert!(filter.layers.contains(1));
    /// assert!(!filter.layers.contains(3));
    /// ```
    #[must_use]
    pub fn on_layers(layers: &[u8]) -> Self {
        Self {
            layers: LayerSet::only(layers),
            ..Self::default()
        }
    }
}

/// What a pick ray hit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickSurface {
    /// A terrain heightfield.
    Terrain,
    /// A mesh renderer's triangle.
    Mesh,
}

/// The nearest surface a ray hits ([`world_pick`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldPick {
    /// The layer of the entity hit.
    pub layer: MapLayer,
    /// The terrain or mesh entity hit (a model's part entity, not its owner).
    pub entity: Entity,
    /// Hit point in local coordinates (the `GlobalTransform` frame). A terrain answer has
    /// `OriginOffset` added for the march and subtracted again (spec 78 § 12.2).
    pub point: Vec3,
    /// World-space unit normal (a mesh triangle's front side; the heightfield's slope normal).
    pub normal: Vec3,
    /// Distance from the ray origin, metres.
    pub distance: f32,
    /// Terrain or mesh.
    pub surface: PickSurface,
    /// The triangle index in the mesh's index list (`None` for terrain).
    pub triangle: Option<u32>,
}

/// The [`MapLayer`] an entity belongs to: its own, else the nearest ancestor's, else layer 0. A
/// terrain is always layer 0 (as [`world_pick`] reports it), whatever `MapLayer` it carries.
#[must_use]
pub fn map_layer_of(world: &World, entity: Entity) -> MapLayer {
    if world.get::<regia_terrain::Terrain>(entity).is_some() {
        return MapLayer(0);
    }
    let mut current = entity;
    // Hierarchies are shallow; the bound only guards a malformed cycle.
    for _ in 0..65_536 {
        if let Some(layer) = world.get::<MapLayer>(current) {
            return *layer;
        }
        match world.get::<regia_ecs::prelude::ChildOf>(current) {
            Some(parent) => current = parent.parent(),
            None => break,
        }
    }
    MapLayer(0)
}

/// Whether a pick skips the entity by its visibility: a member of a parked data layer always,
/// a hidden entity unless the filter includes those.
fn skipped_by_visibility(entity_ref: regia_ecs::prelude::EntityRef<'_>, filter: &PickFilter) -> bool {
    entity_ref.contains::<LayerParked>()
        || (!filter.include_hidden
            && is_hidden(
                entity_ref.get::<Visibility>(),
                entity_ref.get::<InheritedVisibility>(),
            ))
}

/// Whether the entity is hidden itself or by an ancestor (resolved by transform propagation).
fn is_hidden(visibility: Option<&Visibility>, inherited: Option<&InheritedVisibility>) -> bool {
    matches!(visibility, Some(Visibility::Hidden)) || inherited.is_some_and(|v| !v.0)
}

/// Object-space bounds of a mesh, cached by its `Arc` (the rigid early-out). Entries of freed
/// meshes are released at most once per query scope (or on every call outside one); the vertex scan runs outside the lock.
fn mesh_bounds(mesh: &Arc<CpuMesh>) -> Option<(Vec3, Vec3)> {
    type BoundsCache = HashMap<usize, (std::sync::Weak<CpuMesh>, (usize, Option<(Vec3, Vec3)>))>;
    static CACHE: Mutex<Option<BoundsCache>> = Mutex::new(None);
    let key = Arc::as_ptr(mesh) as usize;
    let graveyard = {
        let mut guard = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let cache = guard.get_or_insert_with(HashMap::new);
        // Freed entries release at most once per query scope (see `pick_cache_scope`).
        let graveyard = if crate::stats::sweep_due_bounds() {
            crate::stats::note_cache_sweep();
            sweep_dead(cache)
        } else {
            Vec::new()
        };
        // A vertex-count change invalidates the entry (a guard: the weak handle makes `Arc::get_mut`
        // fail, so an edited mesh normally arrives as a new `Arc`).
        if let Some((weak, (count, bounds))) = cache.get(&key)
            && *count == mesh.vertices.len()
            && weak.upgrade().is_some_and(|m| Arc::ptr_eq(&m, mesh))
        {
            return *bounds;
        }
        graveyard
    };
    drop(graveyard);
    let bounds = mesh.vertices.iter().fold(None, |acc: Option<(Vec3, Vec3)>, v| {
        let p = Vec3::from_array(v.position);
        Some(acc.map_or((p, p), |(lo, hi)| (lo.min(p), hi.max(p))))
    });
    let mut guard = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    guard
        .get_or_insert_with(HashMap::new)
        .insert(key, (Arc::downgrade(mesh), (mesh.vertices.len(), bounds)));
    bounds
}

/// Whether the ray meets the box at `t ≥ 0` (slab test, padded by a relative epsilon so edge hits
/// count). A direction component with magnitude below `1e-12` counts as parallel to that slab, so
/// pass a direction of ordinary length (normalized, as `raycast_mesh_entity` does).
#[must_use]
pub fn ray_meets_box(origin: Vec3, direction: Vec3, (lo, hi): (Vec3, Vec3)) -> bool {
    let pad = (hi - lo).max_element() * 1e-4 + 1e-6;
    let (lo, hi) = (lo - Vec3::splat(pad), hi + Vec3::splat(pad));
    let mut near = 0.0_f32;
    let mut far = f32::INFINITY;
    for axis in 0..3 {
        let (o, d) = (origin[axis], direction[axis]);
        if d.abs() < 1e-12 {
            if o < lo[axis] || o > hi[axis] {
                return false;
            }
            continue;
        }
        let (a, b) = ((lo[axis] - o) / d, (hi[axis] - o) / d);
        near = near.max(a.min(b));
        far = far.min(a.max(b));
        if near > far {
            return false;
        }
    }
    true
}

/// A copy of `mesh` deformed the way the renderer draws `entity`, and how far its object-space
/// bounds must grow.
///
/// `None` when the frame draws the bind pose: no [`MeshMorphTargets`], no [`MorphWeights`], a
/// vertex count that does not match, or every effective weight zero. Active weights are the
/// renderer's subset for the component's mode.
fn morphed_mesh(world: &World, entity: Entity, mesh: &CpuMesh) -> Option<(CpuMesh, f32)> {
    let (targets, mode) = {
        let morph = world.get::<MeshMorphTargets>(entity)?;
        if morph.targets.vertex_count() as usize != mesh.vertices.len() {
            return None;
        }
        (Arc::clone(&morph.targets), morph.mode)
    };
    let weights = world.get::<MorphWeights>(entity)?.0.clone();
    let effective = targets.effective_weights(&weights, mode);
    if effective.iter().all(|weight| *weight == 0.0) {
        return None;
    }
    let pad = targets.max_displacement(&effective);
    let mut vertices = mesh.vertices.clone();
    targets.apply(&effective, &mut vertices);
    Some((
        CpuMesh {
            vertices,
            indices: mesh.indices.clone(),
        },
        pad,
    ))
}

/// The blend the renderer draws for `entity`.
///
/// An empty palette stays four-weight linear. Otherwise the options are
/// [`resolve_skinning_options`]: the entity's own, else the nearest ancestor's without the
/// influence table. A table whose length is not `vertex_count` is ignored, as the compute path
/// ignores it.
fn drawn_skin(
    world: &World,
    entity: Entity,
    palette_empty: bool,
    vertex_count: usize,
) -> (SkinningMethod, Option<Arc<[SkinInfluences8]>>) {
    if palette_empty {
        return (SkinningMethod::Linear, None);
    }
    let Some(options) = resolve_skinning_options(world, entity) else {
        return (SkinningMethod::Linear, None);
    };
    let table = options.influences8.filter(|rows| rows.len() == vertex_count);
    (options.method, table)
}

/// Casts a ray against one mesh entity of `world`.
///
/// Reads [`MeshRenderer`] and `GlobalTransform`. When the entity has both morph targets and
/// morph weights, positions and normals are deformed with those active weights before the
/// triangle tests, then skinned. A [`SkinJoints`] palette is applied with four-weight linear
/// blend, or with the skinning options the renderer resolves when the entity has them (dual
/// quaternions, and an eight-influence table whose length matches the mesh). Rigid meshes with
/// many triangles go through a cached BVH. `None` when it has no mesh or the ray misses.
#[must_use]
pub fn raycast_mesh_entity(
    world: &World,
    entity: Entity,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
) -> Option<MeshHit> {
    raycast_mesh_entity_within(world, entity, origin, direction, cull, f32::INFINITY)
}

/// [`raycast_mesh_entity`] that ignores hits farther than `max_distance` metres along the unit
/// ray (a hit at exactly `max_distance` still counts). The mesh is rejected before any
/// triangle test when its world bounds — posed, for a linearly skinned mesh — are missed or
/// start beyond `max_distance`.
#[must_use]
pub fn raycast_mesh_entity_within(
    world: &World,
    entity: Entity,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
    max_distance: f32,
) -> Option<MeshHit> {
    let unit = direction.normalize_or_zero();
    if unit == Vec3::ZERO
        || !origin.is_finite()
        || !max_distance.is_finite() && max_distance != f32::INFINITY
        || max_distance <= 0.0
    {
        return None;
    }
    let mesh = {
        let renderer = world.get::<MeshRenderer>(entity)?;
        Arc::clone(&renderer.mesh)
    };
    // No `GlobalTransform` means the renderer's extract skips the entity: it is not pickable.
    let model = Mat4::from(world.get::<regia_scene::GlobalTransform>(entity)?.0);
    if !model.is_finite() {
        return None;
    }
    let palette_owned = world.get::<SkinJoints>(entity).map(|skin| skin.palette.clone());
    let palette = palette_owned.as_deref().unwrap_or(&[]);
    let deformed = morphed_mesh(world, entity, &mesh);
    let cast_mesh = deformed.as_ref().map(|(deformed, _)| deformed).unwrap_or(&mesh);
    let (method, table) = drawn_skin(world, entity, palette.is_empty(), cast_mesh.vertices.len());
    let ray = Ray::new(origin, unit);
    if let Some(entry) = mesh_entry_of(
        &mesh,
        palette,
        method == SkinningMethod::Linear,
        table.as_deref(),
        model,
        &deformed,
        ray,
    ) {
        match entry {
            None => return None,
            Some(entry) if entry > max_distance => return None,
            Some(_) => {}
        }
    }
    crate::stats::note_inverse();
    let inverse = model.inverse();
    if !inverse.is_finite() {
        return None;
    }
    let pose = PoseSkin {
        joints: palette,
        method,
        influences8: table.as_deref(),
    };
    if palette.is_empty()
        && deformed.is_none()
        && let Some(bvh) = bvh_for(&mesh)
    {
        return crate::mesh::cast_mesh_bvh(
            &mesh,
            &bvh,
            model,
            Some(inverse),
            origin,
            direction,
            cull,
            max_distance,
        );
    }
    crate::mesh::cast_mesh(
        cast_mesh,
        &cast_mesh.indices,
        model,
        pose,
        Some(inverse),
        origin,
        direction,
        cull,
        max_distance,
    )
}

/// World-axis bounds of an object-space box under `model` (the eight corners).
fn transform_aabb(model: Mat4, lo: Vec3, hi: Vec3) -> (Vec3, Vec3) {
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for index in 0..8 {
        let point = model.transform_point3(Vec3::new(
            if index & 1 == 0 { lo.x } else { hi.x },
            if index & 2 == 0 { lo.y } else { hi.y },
            if index & 4 == 0 { lo.z } else { hi.z },
        ));
        min = min.min(point);
        max = max.max(point);
    }
    (min, max)
}

/// Entry distance of a unit ray against a padded box, or `None` on a miss. Zero when the
/// origin is inside the box.
fn ray_aabb_entry(origin: Vec3, direction: Vec3, (lo, hi): (Vec3, Vec3)) -> Option<f32> {
    let pad = (hi - lo).max_element() * 1e-4 + 1e-6;
    let (lo, hi) = (lo - Vec3::splat(pad), hi + Vec3::splat(pad));
    let mut near = 0.0_f32;
    let mut far = f32::INFINITY;
    for axis in 0..3 {
        let (o, d) = (origin[axis], direction[axis]);
        if d.abs() < 1e-12 {
            if o < lo[axis] || o > hi[axis] {
                return None;
            }
            continue;
        }
        let (a, b) = ((lo[axis] - o) / d, (hi[axis] - o) / d);
        near = near.max(a.min(b));
        far = far.min(a.max(b));
        if near > far || far < 0.0 {
            return None;
        }
    }
    Some(near.max(0.0))
}

/// The unit ray's entry into the entity's world bounds: `Some(entry)` on a meet (the entry
/// distance, 0 from inside), `Some(None)` on a miss. `None` when bounds cannot reject the ray
/// (an empty mesh, or skin weights that are not a convex linear blend) — the cast then decides.
fn mesh_entry_of(
    mesh: &Arc<CpuMesh>,
    palette: &[regia_render::JointMatrix],
    linear: bool,
    table: Option<&[SkinInfluences8]>,
    model: Mat4,
    deformed: &Option<(CpuMesh, f32)>,
    ray: Ray,
) -> Option<Option<f32>> {
    let bind = mesh_bounds(mesh)?;
    let bind = match deformed {
        Some((_, pad)) => {
            let pad = Vec3::splat(*pad);
            (bind.0 - pad, bind.1 + pad)
        }
        None => bind,
    };
    let object = if palette.is_empty() {
        Some(bind)
    } else if linear {
        // Dual-quaternion blends normalize and can leave the convex hull: only a linear blend
        // (four weights, or the matching eight-influence table) gets an early-out.
        crate::mesh::posed_object_bounds(mesh, palette, table, bind)
    } else {
        None
    };
    let (lo, hi) = object?;
    let (world_lo, world_hi) = transform_aabb(model, lo, hi);
    Some(ray_aabb_entry(ray.origin, ray.direction, (world_lo, world_hi)))
}

/// The unit ray's entry into the entity's world bounds, for ranking a query's candidates.
/// `Some(entry)` is the box-meet distance (0 when the bounds cannot reject the ray cheaply);
/// `None` when the box is missed (or the entity is not pickable) and the cast cannot hit either.
fn mesh_entry(world: &World, entity: Entity, origin: Vec3, unit: Vec3) -> Option<f32> {
    let renderer = world.get::<MeshRenderer>(entity)?;
    if world.get::<regia_render::MeshMorphTargets>(entity).is_some()
        && world
            .get::<regia_scene::components::MorphWeights>(entity)
            .is_some()
    {
        // Morphs deform the bounds; the cast tests the padded box itself.
        return Some(0.0);
    }
    let model = Mat4::from(world.get::<regia_scene::GlobalTransform>(entity)?.0);
    if !model.is_finite() {
        return None;
    }
    let palette_owned = world.get::<SkinJoints>(entity).map(|skin| skin.palette.clone());
    let palette = palette_owned.as_deref().unwrap_or(&[]);
    let (method, table) = drawn_skin(world, entity, palette.is_empty(), renderer.mesh.vertices.len());
    let ray = Ray::new(origin, unit);
    match mesh_entry_of(
        &renderer.mesh,
        palette,
        method == SkinningMethod::Linear,
        table.as_deref(),
        model,
        &None,
        ray,
    ) {
        Some(Some(entry)) => Some(entry),
        Some(None) => None,
        None => Some(0.0),
    }
}

/// Runs `visit` over every entity whose archetype contains `C`, without visiting entities that
/// cannot carry the component.
fn for_each_with<C: regia_ecs::prelude::Component>(
    world: &World,
    mut visit: impl FnMut(regia_ecs::prelude::EntityRef<'_>),
) {
    let Some(id) = world.component_id::<C>() else {
        return;
    };
    for archetype in world.archetypes().iter() {
        if !archetype.contains(id) {
            continue;
        }
        for slot in archetype.entities() {
            visit(world.entity(slot.id()));
        }
    }
}
/// The nearest hit over every mesh entity `filter` accepts, without the layer logic.
#[must_use]
pub fn raycast_meshes(
    world: &World,
    origin: Vec3,
    direction: Vec3,
    cull: FaceCull,
    accept: &dyn Fn(Entity) -> bool,
) -> Option<(Entity, MeshHit)> {
    // One sweep of the pick caches for this query, and a nearest-first walk: the running
    // distance caps every later cast.
    let _scope = crate::stats::pick_cache_scope();
    let unit = direction.normalize_or_zero();
    if unit == Vec3::ZERO || !origin.is_finite() {
        return None;
    }
    let mut ranked: Vec<(f32, u64, Entity)> = Vec::new();
    for_each_with::<MeshRenderer>(world, |entity_ref| {
        crate::stats::note_entity_visit();
        let entity = entity_ref.id();
        if !accept(entity) {
            return;
        }
        let Some(entry) = mesh_entry(world, entity, origin, unit) else {
            return;
        };
        ranked.push((entry, entity.to_bits(), entity));
    });
    ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let mut best: Option<(Entity, MeshHit)> = None;
    for (entry, _, entity) in ranked {
        let limit = best.as_ref().map_or(f32::INFINITY, |(_, hit)| hit.distance);
        if entry > limit {
            continue;
        }
        let Some(hit) = raycast_mesh_entity_within(world, entity, origin, direction, cull, limit) else {
            continue;
        };
        let wins = best.as_ref().is_none_or(|(kept_entity, kept)| {
            hit.distance < kept.distance
                || (hit.distance == kept.distance && mesh_key(world, entity) < mesh_key(world, *kept_entity))
        });
        if wins {
            best = Some((entity, hit));
        }
    }
    best
}

/// Installs the spawn-order tracking the picks use to break equal distances (coplanar surfaces)
/// by the order the surface arrived in the world instead of by entity id, which depends on thread
/// timing (ADR-1347, ADR-2273). Call it before the first mesh or terrain spawns; returns whether
/// `(MeshRenderer, Terrain)` are tracked. Without it ties fall back to the entity ordering.
pub fn track_pick_order(world: &mut World) -> (bool, bool) {
    (
        track_spawn_order::<MeshRenderer>(world),
        track_spawn_order::<regia_terrain::Terrain>(world),
    )
}

/// The tie-break key of a mesh entity: its arrival order when tracked, else the entity ordering.
pub(crate) fn mesh_key(world: &World, entity: Entity) -> PickKey {
    spawn_key::<MeshRenderer>(world, entity)
}

/// The tie-break key of a terrain entity.
pub(crate) fn terrain_key(world: &World, entity: Entity) -> PickKey {
    spawn_key::<regia_terrain::Terrain>(world, entity)
}

/// Arrival number (untracked entities last), then the entity's own ordering.
type PickKey = (u64, Entity);

fn spawn_key<C: regia_ecs::prelude::Component>(world: &World, entity: Entity) -> PickKey {
    let arrival = world
        .get_resource::<SpawnOrder<C>>()
        .and_then(|order| order.get(entity));
    (arrival.unwrap_or(u64::MAX), entity)
}

/// Every candidate's nearest hit, keyed by layer when `per_layer`, else under layer 0 only the
/// overall nearest.
fn cast(world: &World, ray: Ray, filter: &PickFilter, per_layer: bool) -> BTreeMap<u8, WorldPick> {
    let _span = tracing::info_span!(target: "regia_pick", "world_pick").entered();
    // One sweep of the pick caches for this query.
    let _scope = crate::stats::pick_cache_scope();
    let mut best: BTreeMap<u8, WorldPick> = BTreeMap::new();
    let unit = ray.direction.normalize_or_zero();
    if unit == Vec3::ZERO || !ray.origin.is_finite() {
        return best;
    }
    // The key under which a hit competes: its layer, or one shared slot for the nearest overall.
    let slot = |layer: u8| if per_layer { layer } else { 0 };
    let limit = |best: &BTreeMap<u8, WorldPick>, layer: u8| {
        best.get(&slot(layer))
            .map_or(filter.max_distance, |p| p.distance.min(filter.max_distance))
    };
    let offer = |best: &mut BTreeMap<u8, WorldPick>, pick: WorldPick| {
        let key = slot(pick.layer.0);
        // Equal distances (coplanar surfaces) go to the lower entity, not the ECS visit order.
        let closer = |b: &WorldPick| {
            pick.distance < b.distance
                || (pick.distance == b.distance && pick_key(world, &pick) < pick_key(world, b))
        };
        if pick.distance <= filter.max_distance && best.get(&key).is_none_or(closer) {
            best.insert(key, pick);
        }
    };
    if filter.meshes {
        // Rank the candidates by their bounds' entry distance and cast nearest first: the
        // running distance caps every later cast (a hit at exactly the limit still counts,
        // so equal-distance ties keep the lower entity through `offer`).
        let skip_set: Option<std::collections::HashSet<Entity>> =
            (filter.skip.len() > 8).then(|| filter.skip.iter().copied().collect());
        fn skipped(
            filter: &PickFilter,
            skip_set: &Option<std::collections::HashSet<Entity>>,
            entity: Entity,
            entity_ref: &regia_ecs::prelude::EntityRef<'_>,
        ) -> bool {
            (match skip_set {
                Some(set) => set.contains(&entity),
                None => filter.skip.contains(&entity),
            }) || skipped_by_visibility(*entity_ref, filter)
        }
        let mut ranked: Vec<(f32, u64, Entity, MapLayer)> = Vec::new();
        for_each_with::<MeshRenderer>(world, |entity_ref| {
            crate::stats::note_entity_visit();
            let entity = entity_ref.id();
            if skipped(filter, &skip_set, entity, &entity_ref) {
                return;
            }
            let layer = map_layer_of(world, entity);
            if !filter.layers.contains(layer.0) {
                return;
            }
            let Some(entry) = mesh_entry(world, entity, ray.origin, unit) else {
                return;
            };
            ranked.push((entry, entity.to_bits(), entity, layer));
        });
        ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (entry, _, entity, layer) in ranked {
            let limit = limit(&best, layer.0);
            if entry > limit {
                continue;
            }
            if let Some(hit) =
                raycast_mesh_entity_within(world, entity, ray.origin, ray.direction, filter.cull, limit)
                && hit.distance <= limit
            {
                offer(
                    &mut best,
                    WorldPick {
                        layer,
                        entity,
                        point: hit.position,
                        normal: hit.normal,
                        distance: hit.distance,
                        surface: PickSurface::Mesh,
                        triangle: Some(hit.triangle),
                    },
                );
            }
        }
    }
    if filter.terrain {
        for_each_with::<regia_terrain::Terrain>(world, |entity_ref| {
            crate::stats::note_entity_visit();
            let entity = entity_ref.id();
            if filter.skip.contains(&entity) || skipped_by_visibility(entity_ref, filter) {
                return;
            }
            // Terrain is always layer 0, whatever `MapLayer` it or an ancestor carries.
            let layer = MapLayer(0);
            if !filter.layers.contains(layer.0) {
                return;
            }
            if let Some(hit) =
                raycast_terrain_entity(world, entity, ray.origin, ray.direction, limit(&best, layer.0))
            {
                offer(
                    &mut best,
                    WorldPick {
                        layer,
                        entity,
                        point: hit.point,
                        normal: hit.normal,
                        distance: hit.distance,
                        surface: PickSurface::Terrain,
                        triangle: None,
                    },
                );
            }
        });
    }
    best
}

/// Tie-break key of a pick: mesh and terrain entities each use their own arrival order.
fn pick_key(world: &World, pick: &WorldPick) -> (u8, PickKey) {
    match pick.surface {
        PickSurface::Terrain => (0, terrain_key(world, pick.entity)),
        PickSurface::Mesh => (1, mesh_key(world, pick.entity)),
    }
}

/// The nearest surface along `ray` that `filter` accepts, with its [`MapLayer`]: terrain
/// heightfields (passing through holes; only a crossing from above) and mesh renderers. `ray` and
/// the hit are in local coordinates. Skinned meshes use a four-influence linear blend of
/// [`SkinJoints`]. `None` on a miss.
#[must_use]
pub fn world_pick(world: &World, ray: Ray, filter: &PickFilter) -> Option<WorldPick> {
    cast(world, ray, filter, false).into_values().next()
}

/// The nearest hit of every layer `filter` accepts, nearest first: what a ray crosses through a
/// stack of floors.
#[must_use]
pub fn world_pick_layers(world: &World, ray: Ray, filter: &PickFilter) -> Vec<WorldPick> {
    let mut hits: Vec<WorldPick> = cast(world, ray, filter, true).into_values().collect();
    hits.sort_by(|a, b| {
        a.distance
            .total_cmp(&b.distance)
            .then_with(|| pick_key(world, a).cmp(&pick_key(world, b)))
    });
    hits
}
