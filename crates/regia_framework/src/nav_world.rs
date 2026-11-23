// SPDX-License-Identifier: MPL-2.0
//! Keeps the navmesh in step with the world (ADR-1934): [`NavWorldPlugin`] turns destruction and
//! terrain messages into [`regia_nav::NavRegionQueue`] boxes and serves level geometry for their
//! rebuild.
//!
//! `regia_nav` sees neither physics nor terrain, so the bridge lives here, where both are in reach.
//! The plugin is opt-in and inert until the app inserts a [`regia_nav::NavTileCache`].
//!
//! # Schedule order (ADR-2420)
//!
//! In `FixedUpdate`, an app with this plugin runs, in this order:
//!
//! 1. `PhysicsStep` (bodies move, chunks break off);
//! 2. `write_back_destruction`, which writes `NavmeshInvalidated`;
//! 3. this plugin's forwarders (destruction boxes, [`NavAreaZone`]s, the [`NavAreaCostTable`]);
//! 4. `NavRegionStep` (`NavAuthoredLinksPlugin` puts authored links on the mesh just before it);
//! 5. `NavCrowdStep`, then `NavLaneStep`.
//!
//! So the crowd always walks the mesh of the step's own physics, and a replay rebuilds on the same
//! tick. Each step is ordered by an edge (`after(PhysicsStep)`, `after(write_back_destruction)`,
//! `before(NavRegionStep)`, the region system `before(NavCrowdStep)`, the lane system
//! `after(NavRegionStep)` and `after(NavCrowdStep)`). The edges sit on the engine's systems, not on
//! the sets: a game system placed in `NavRegionStep` or `NavLaneStep` adds the same edges to get
//! the same place. Ordering a crowd system before `PhysicsStep` closes a cycle and the schedule
//! refuses to build. A game
//! that feeds crowd velocities into physics (character bodies following agents) reads them after
//! `NavCrowdStep` and applies them on the next step, which is one fixed step of latency and keeps
//! the order acyclic. `the_fixed_step_runs_physics_then_the_navmesh_then_the_crowds` pins this.

use regia_app::prelude::*;
use regia_core::math::{Affine3A, Vec3, det_atan2};
use regia_nav::{
    NavAreaCosts, NavAreaError, NavAreaVolume, NavAreaVolumeId, NavDirtySource, NavMesh, NavObstacleShape,
    NavRegionPlugin, NavRegionQueue, NavRegionStep, NavTileCache,
};
use regia_physics::destruction::{ChunkState, Destruction, NavmeshInvalidated, write_back_destruction};
use regia_physics::{BodyKind, Collider, ColliderShape, PhysicsWorld, RigidBody};
#[cfg(feature = "client")]
use regia_scene::streaming::OriginOffset;
use regia_scene::{GlobalTransform, Transform};
#[cfg(feature = "client")]
use regia_terrain::{HolesMask, Terrain, TerrainHeightsChanged, TerrainHoles, TerrainMap};

/// Paints a nav area over a box around the entity (ADR-1934): mud, a road, a shallow ford. It follows
/// the entity's position and its turn about +Y; [`NavWorldPlugin`] keeps the [`NavTileCache`] in
/// step, each fixed step. A zone that moves paints last where it overlaps another. An invalid zone
/// (an area past 63, a size that is not positive) stays out of the cache with a warning.
///
/// Zones are reflected, so a level stores them, and the streaming cook paints them into the
/// cooked nav tiles (ADR-2133); with a [`NavAreaCostTable`] they are the level's nav bake settings
/// for ground costs.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct NavAreaZone {
    /// Half the box size along its own axes, metres.
    pub half_extents: Vec3,
    /// The area id.
    pub area: u8,
}

impl Default for NavAreaZone {
    fn default() -> Self {
        Self {
            half_extents: Vec3::ONE,
            area: 0,
        }
    }
}

/// A nav area id for every triangle of the collider on the same entity (ADR-2788). The streaming
/// cook (`regia cook`) paints it per triangle through `NavTileCache::build_with_triangle_areas`, so
/// a road mesh or a mud patch authored as a collider carries its area into the cooked tiles without
/// a zone box. A [`NavAreaZone`] still paints over it. Only the streaming cook reads it: a navmesh
/// built at run time from colliders (not from cooked tiles) ignores it.
#[derive(Component, Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct NavSurface {
    /// The area id, `0..=63` (0 is plain ground). A cook with a larger id fails.
    pub area: u8,
}

/// One area of a [`NavAreaCostTable`].
#[derive(Reflect, Clone, Debug, PartialEq)]
#[reflect(Default)]
pub struct NavAreaCostEntry {
    /// The area id, `0..=63`.
    pub area: u8,
    /// Its cost per metre, finite and positive (1 is plain ground).
    pub cost: f32,
    /// A label for tools ("mud", "road"); not used by pathfinding.
    pub name: String,
}

impl Default for NavAreaCostEntry {
    fn default() -> Self {
        Self {
            area: 0,
            cost: 1.0,
            name: String::new(),
        }
    }
}

/// The level's area costs (ADR-2133), stored in the level's `resources` map next to
/// `LevelStreaming`: what each area id costs per metre for every agent. [`NavWorldPlugin`] applies
/// it to the [`NavTileCache`] (and a [`NavMesh`] resource) when it changes or the cache is
/// replaced; an agent's `NavQueryFilter` still scales it. Areas not listed cost 1; a later row for
/// the same area wins. An invalid table is left unapplied with a warning.
#[derive(Resource, Reflect, Clone, Debug, PartialEq, Default)]
#[reflect(Resource, Default)]
pub struct NavAreaCostTable {
    /// The rows.
    pub areas: Vec<NavAreaCostEntry>,
}

impl NavAreaCostTable {
    /// The cost table the navmesh takes. Errors: an area past 63 or a cost that is not finite and
    /// positive (`nav_invalid_area`, `nav_invalid_area_cost`).
    pub fn costs(&self) -> Result<NavAreaCosts, NavAreaError> {
        let mut costs = NavAreaCosts::default();
        for row in &self.areas {
            costs.set(row.area, row.cost)?;
        }
        Ok(costs)
    }
}

/// A zone's volume id, when the cache took it, and what it asked for.
type ZoneEntry = (Option<NavAreaVolumeId>, NavAreaVolume);

/// What [`sync_nav_area_zones`] reads of a zone's entity.
type ZoneItem = (
    Entity,
    &'static NavAreaZone,
    &'static Transform,
    Option<&'static GlobalTransform>,
    Has<ChildOf>,
);

/// What each [`NavAreaZone`] put in the cache.
#[derive(Resource, Debug, Default)]
struct NavZoneVolumes(std::collections::BTreeMap<Entity, ZoneEntry>);

/// Rebuilds the navmesh under destruction and terrain edits, and keeps [`NavAreaZone`]s in it. Adds
/// [`NavRegionPlugin`] and, unless the app set its own, the [`level_triangles_in`] geometry source.
///
/// [`NavmeshInvalidated`] is read in the fixed step that wrote it, so a replay rebuilds on the same
/// tick; that orders `NavRegionStep`, and with it `NavCrowdStep`, after `PhysicsStep`.
/// `TerrainHeightsChanged` is read in `PostUpdate`, once per frame (client builds).
pub struct NavWorldPlugin;

impl Plugin for NavWorldPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(NavRegionPlugin);
        app.add_message::<NavmeshInvalidated>()
            .init_resource::<NavZoneVolumes>()
            .register_type::<NavAreaZone>()
            .register_type::<NavSurface>()
            .register_type::<NavAreaCostEntry>()
            .register_type::<NavAreaCostTable>()
            .add_systems(
                FixedUpdate,
                (forward_destruction, sync_nav_area_zones, apply_nav_area_costs)
                    .after(write_back_destruction)
                    .before(NavRegionStep),
            );
        // A dedicated server has no terrain crate (spec 76 § 2.1, ADR-1964).
        #[cfg(feature = "client")]
        app.add_message::<TerrainHeightsChanged>()
            .add_systems(PostUpdate, forward_terrain);
        let mut queue = app.world_mut().resource_mut::<NavRegionQueue>();
        if !queue.has_source() {
            queue.set_source(std::sync::Arc::new(level_triangles_in));
        }
    }
}

/// The volume `zone` paints at the world pose `pose`: centred on its translation, turned about +Y
/// with its local +X (the streaming cook uses this too, so cooked and live paint agree).
pub fn nav_area_zone_volume(zone: &NavAreaZone, pose: Affine3A) -> NavAreaVolume {
    zone_volume(
        zone,
        Vec3::from(pose.translation),
        pose.transform_vector3(Vec3::X),
    )
}

/// Applies the [`NavAreaCostTable`] to the cache and the mesh when the table changed or either
/// was replaced (a cost set by hand in between stays until then). Inserting over an existing
/// resource does not count as added, so a cache whose revision went back (revisions only grow
/// within one cache) is taken as a new one too.
fn apply_nav_area_costs(
    table: Option<Res<NavAreaCostTable>>,
    cache: Option<ResMut<NavTileCache>>,
    mesh: Option<ResMut<NavMesh>>,
    mut last_revision: Local<Option<u64>>,
) {
    let seen = cache.as_ref().map(|c| c.revision());
    let replaced = matches!((seen, *last_revision), (Some(now), Some(before)) if now < before);
    *last_revision = seen;
    let Some(table) = table else {
        return;
    };
    let fresh = |added: bool| table.is_changed() || added;
    let cache = cache.filter(|c| fresh(c.is_added() || replaced));
    let mesh = mesh.filter(|m| fresh(m.is_added()));
    if cache.is_none() && mesh.is_none() {
        return;
    }
    let costs = match table.costs() {
        Ok(costs) => costs,
        Err(e) => {
            tracing::warn!(target: "regia::nav", error = %e, "nav area cost table left unapplied");
            return;
        }
    };
    if let Some(mut cache) = cache
        && cache.area_costs() != &costs
    {
        cache.set_area_costs(costs);
        *last_revision = Some(cache.revision());
    }
    if let Some(mut mesh) = mesh
        && mesh.area_costs() != &costs
    {
        mesh.set_area_costs(costs);
    }
}

/// The volume of a zone at `center`, whose local +X points along `forward`. The yaw comes from
/// the deterministic kernel (spec 15 § 5), not the platform libm: the streaming cook paints these
/// volumes into cooked tiles, and a Linux cook and a Windows runtime must rasterize the same yaw.
fn zone_volume(zone: &NavAreaZone, center: Vec3, forward: Vec3) -> NavAreaVolume {
    NavAreaVolume::new(
        NavObstacleShape::OrientedBox {
            center,
            half_extents: zone.half_extents,
            yaw: det_atan2(-forward.z, forward.x),
        },
        zone.area,
    )
}

/// Keeps the cache's volumes equal to the [`NavAreaZone`]s, each fixed step: tiles are queued on
/// the tick a zone moves, whatever the frame rate. The pose of an entity with no parent is its
/// [`Transform`], current on this tick; a child's is its [`GlobalTransform`], one propagation old.
/// Comparing every zone needs no removal events, which a frame with no fixed step would lose.
fn sync_nav_area_zones(
    zones: Query<ZoneItem>,
    cache: Option<ResMut<NavTileCache>>,
    mut volumes: ResMut<NavZoneVolumes>,
) {
    let Some(mut cache) = cache else {
        return;
    };
    // A volume counts only while the cache still holds it: a cache can be replaced under the zones.
    let held = |cache: &NavTileCache, entry: &ZoneEntry| {
        entry.0.filter(|&id| cache.area_volume(id) == Some(entry.1))
    };
    let gone: Vec<Entity> = volumes
        .0
        .keys()
        .copied()
        .filter(|e| !zones.contains(*e))
        .collect();
    for entity in gone {
        if let Some(entry) = volumes.0.remove(&entity)
            && let Some(id) = held(&cache, &entry)
        {
            let _ = cache.remove_area_volume(id);
        }
    }
    let mut ordered: Vec<_> = zones.iter().collect();
    ordered.sort_by_key(|(entity, ..)| *entity);
    for (entity, zone, local, global, is_child) in ordered {
        let volume = match global.filter(|_| is_child) {
            Some(pose) => zone_volume(zone, pose.translation(), pose.0.transform_vector3(Vec3::X)),
            None => zone_volume(zone, local.translation, local.rotation * Vec3::X),
        };
        if let Some(entry) = volumes.0.get(&entity) {
            let here = held(&cache, entry);
            // A zone the cache refused stays out until it changes.
            if entry.1 == volume && (here.is_some() || entry.0.is_none()) {
                continue;
            }
            if let Some(id) = here {
                let _ = cache.remove_area_volume(id);
            }
        }
        let id = match cache.add_area_volume(volume) {
            Ok(id) => Some(id),
            Err(e) => {
                tracing::warn!(target: "regia::nav", error = %e, "nav area zone left out");
                None
            }
        };
        volumes.0.insert(entity, (id, volume));
    }
}

fn forward_destruction(mut reader: MessageReader<NavmeshInvalidated>, mut queue: ResMut<NavRegionQueue>) {
    for NavmeshInvalidated(n) in reader.read() {
        queue.push(
            Vec3::from_array(n.min),
            Vec3::from_array(n.max),
            NavDirtySource::Destruction,
        );
    }
}

#[cfg(feature = "client")]
fn forward_terrain(
    mut reader: MessageReader<TerrainHeightsChanged>,
    mut queue: ResMut<NavRegionQueue>,
    origin: Option<Res<OriginOffset>>,
) {
    // The message is in the terrain's own space, the queue and the navmesh are local (spec 78 § 12.2).
    let offset = origin_offset(origin.as_deref());
    for m in reader.read() {
        queue.push(
            Vec3::from_array(m.world_min) - offset,
            Vec3::from_array(m.world_max) - offset,
            NavDirtySource::Terrain,
        );
    }
}

/// The local origin in the terrain's space; zero without a floating-origin rebase.
#[cfg(feature = "client")]
fn origin_offset(origin: Option<&OriginOffset>) -> Vec3 {
    origin.map_or(Vec3::ZERO, |o| Vec3::from_array(o.0.map(|axis| axis as f32)))
}

/// Whether a triangle's XZ bounds touch the box.
fn touches(tri: &[Vec3; 3], min: Vec3, max: Vec3) -> bool {
    let lo = tri[0].min(tri[1]).min(tri[2]);
    let hi = tri[0].max(tri[1]).max(tri[2]);
    lo.x <= max.x && hi.x >= min.x && lo.z <= max.z && hi.z >= min.z
}

/// The default [`regia_nav::NavGeometrySource`]: terrain surfaces (holes cut; client builds), static box,
/// triangle-mesh and heightfield colliders, and the attached chunks of destructibles (each chunk's
/// convex hulls) touching the XZ box. Spheres, capsules and compounds are not navmesh geometry. A
/// collider counts when a static body sits on its own entity, placed at [`world_pose`] (its parents'
/// current transforms composed), which is what the streaming cook takes too. Always `true`.
pub fn level_triangles_in(world: &World, min: Vec3, max: Vec3, out: &mut Vec<[Vec3; 3]>) -> bool {
    #[cfg(feature = "client")]
    {
        let runtime_holes = world
            .get_resource::<TerrainHoles>()
            .and_then(TerrainHoles::applied_mask);
        // The box and the triangles are local, the heightfield stays in its own space (spec 78 § 12.2).
        let offset = origin_offset(world.get_resource::<OriginOffset>());
        if let Some(mut terrains) = world.try_query::<&Terrain>() {
            for terrain in terrains.iter(world) {
                let holes = runtime_holes.as_deref().or(terrain.holes.as_ref());
                let first = out.len();
                terrain_triangles_in(&terrain.map, holes, min + offset, max + offset, out);
                if offset != Vec3::ZERO {
                    for vertex in out[first..].iter_mut().flatten() {
                        *vertex -= offset;
                    }
                }
            }
        }
    }
    if let Some(mut colliders) = world.try_query::<(Entity, &Collider, &RigidBody)>() {
        for (entity, collider, body) in colliders.iter(world) {
            if !collider.sensor && body.kind == BodyKind::Static {
                collider_triangles_at(collider, world_pose(world, entity), min, max, out);
            }
        }
    }
    destructible_triangles_in(world, min, max, out);
    true
}

/// Appends the triangles of the attached chunks of destructibles touching the XZ box: each chunk's
/// convex hulls as rapier holds them, or its bounding box for a shape that is not a hull.
fn destructible_triangles_in(world: &World, min: Vec3, max: Vec3, out: &mut Vec<[Vec3; 3]>) {
    if let (Some(destruction), Some(physics)) = (
        world.get_resource::<Destruction>(),
        world.get_resource::<PhysicsWorld>(),
    ) {
        for (id, instance) in destruction.world.instances() {
            for (chunk, state) in instance.chunk_states().iter().enumerate() {
                if *state != ChunkState::Attached {
                    continue;
                }
                let Some(handle) = destruction.physics.intact_collider(id, chunk as u32) else {
                    continue;
                };
                let Some(collider) = physics.colliders.get(handle) else {
                    continue;
                };
                // The chunk's own convex hulls, as rapier holds them (ADR-2305): a chunk turned on
                // its side no longer blocks the corners of its bounding box.
                let position = collider.position();
                let shape = collider.shape();
                let mut hulls = 0;
                if let Some(hull) = shape.as_convex_polyhedron() {
                    let (points, faces) = hull.to_trimesh();
                    let world: Vec<Vec3> = points
                        .iter()
                        .map(|p| {
                            let q = position.transform_point(*p);
                            Vec3::new(q.x, q.y, q.z)
                        })
                        .collect();
                    hull_triangles_in(&world, &faces, min, max, out);
                    hulls += 1;
                } else if let Some(compound) = shape.as_compound() {
                    for (part, sub) in compound.shapes() {
                        let Some(hull) = sub.as_convex_polyhedron() else {
                            continue;
                        };
                        let (points, faces) = hull.to_trimesh();
                        let world: Vec<Vec3> = points
                            .iter()
                            .map(|p| {
                                let q = position.transform_point(part.transform_point(*p));
                                Vec3::new(q.x, q.y, q.z)
                            })
                            .collect();
                        hull_triangles_in(&world, &faces, min, max, out);
                        hulls += 1;
                    }
                }
                if hulls == 0 {
                    // Not a hull (a game's own shape): its bounding box, as before.
                    let aabb = collider.compute_aabb();
                    let (lo, hi) = (
                        Vec3::new(aabb.mins.x, aabb.mins.y, aabb.mins.z),
                        Vec3::new(aabb.maxs.x, aabb.maxs.y, aabb.maxs.z),
                    );
                    let center = (lo + hi) * 0.5;
                    box_triangles_in(Affine3A::from_translation(center), (hi - lo) * 0.5, min, max, out);
                }
            }
        }
    }
}

/// One source of [`LevelNavSources`].
#[derive(Debug, Clone)]
enum NavSource {
    /// A terrain's heightfield, the holes cut from it, and the floating-origin offset.
    #[cfg(feature = "client")]
    Terrain {
        map: TerrainMap,
        holes: Option<HolesMask>,
        offset: Vec3,
    },
    /// A static box, triangle-mesh or heightfield collider at its owner's pose.
    Collider { collider: Collider, owner: Affine3A },
    /// The destructibles' attached chunks, already as triangles (a few hulls).
    Triangles(Vec<[Vec3; 3]>),
}

/// The level's navmesh geometry sources as owned copies, so the triangles can be made on another
/// thread while the world goes on (ADR-8594: the editor's navigation preview reads the level here
/// on the editor thread, cheaply, and makes the triangles on its bake worker). The sources and the
/// triangles are those of [`level_triangles_in`]: the terrains (holes cut; client builds), the
/// static box, triangle-mesh and heightfield colliders at [`world_pose`], and the destructibles'
/// attached chunks. Capturing copies each terrain's heights and each mesh collider's vertices
/// (memory copies, far cheaper than the triangles: a 4097² terrain is 67 MB of heights and 2 GB of
/// triangles); the destructibles' hulls are made at capture.
#[derive(Debug, Clone, Default)]
pub struct LevelNavSources {
    sources: Vec<NavSource>,
}

impl LevelNavSources {
    /// Copies the level's navmesh sources out of `world`. A destructible chunk counts when it
    /// touches the XZ box `min`..`max` (pass the box [`Self::triangles_in`] takes).
    #[must_use]
    pub fn capture(world: &World, min: Vec3, max: Vec3) -> Self {
        let _span = tracing::info_span!(target: "regia_framework", "nav_sources_capture").entered();
        let mut sources = Vec::new();
        #[cfg(feature = "client")]
        {
            let runtime_holes = world
                .get_resource::<TerrainHoles>()
                .and_then(TerrainHoles::applied_mask);
            let offset = origin_offset(world.get_resource::<OriginOffset>());
            if let Some(mut terrains) = world.try_query::<&Terrain>() {
                for terrain in terrains.iter(world) {
                    let holes = runtime_holes.as_deref().or(terrain.holes.as_ref());
                    sources.push(NavSource::Terrain {
                        map: terrain.map.clone(),
                        holes: holes.cloned(),
                        offset,
                    });
                }
            }
        }
        if let Some(mut colliders) = world.try_query::<(Entity, &Collider, &RigidBody)>() {
            for (entity, collider, body) in colliders.iter(world) {
                let shaped = matches!(
                    collider.shape,
                    ColliderShape::Box { .. }
                        | ColliderShape::HeightField { .. }
                        | ColliderShape::TriMesh { .. }
                );
                if shaped && !collider.sensor && body.kind == BodyKind::Static {
                    sources.push(NavSource::Collider {
                        collider: collider.clone(),
                        owner: world_pose(world, entity),
                    });
                }
            }
        }
        let mut chunks = Vec::new();
        destructible_triangles_in(world, min, max, &mut chunks);
        if !chunks.is_empty() {
            sources.push(NavSource::Triangles(chunks));
        }
        Self { sources }
    }

    /// How many sources were captured (terrains, colliders, and one for all destructible chunks).
    #[must_use]
    pub fn len(&self) -> usize {
        self.sources.len()
    }

    /// Whether the level has no navmesh source at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }

    /// Appends the triangles of every source touching the XZ box, in [`level_triangles_in`]'s
    /// order. `go_on(done, total)` is asked before each source; `false` stops the read there and
    /// the call returns `false` (the triangles so far stay in `out`). `true` when every source was
    /// read.
    pub fn triangles_in(
        &self,
        min: Vec3,
        max: Vec3,
        out: &mut Vec<[Vec3; 3]>,
        mut go_on: impl FnMut(usize, usize) -> bool,
    ) -> bool {
        let _span = tracing::info_span!(target: "regia_framework", "nav_sources_triangles").entered();
        let total = self.sources.len();
        for (done, source) in self.sources.iter().enumerate() {
            if !go_on(done, total) {
                return false;
            }
            match source {
                #[cfg(feature = "client")]
                NavSource::Terrain { map, holes, offset } => {
                    let first = out.len();
                    terrain_triangles_in(map, holes.as_ref(), min + *offset, max + *offset, out);
                    if *offset != Vec3::ZERO {
                        for vertex in out[first..].iter_mut().flatten() {
                            *vertex -= *offset;
                        }
                    }
                }
                NavSource::Collider { collider, owner } => {
                    collider_triangles_at(collider, *owner, min, max, out)
                }
                NavSource::Triangles(triangles) => {
                    out.extend(triangles.iter().filter(|tri| touches(tri, min, max)));
                }
            }
        }
        true
    }
}

/// Appends the triangles of a hull (world-space points, index triples) touching the XZ box.
fn hull_triangles_in(points: &[Vec3], faces: &[[u32; 3]], min: Vec3, max: Vec3, out: &mut Vec<[Vec3; 3]>) {
    for &[a, b, c] in faces {
        let (Some(a), Some(b), Some(c)) = (
            points.get(a as usize),
            points.get(b as usize),
            points.get(c as usize),
        ) else {
            continue;
        };
        let tri = [*a, *b, *c];
        if touches(&tri, min, max) {
            out.push(tri);
        }
    }
}

/// The off-mesh links [`regia_nav::generate_links`] finds for `mesh`, keeping only those whose
/// move clears the level's colliders (ADR-2420): [`regia_nav::generate_clear_links`] with a
/// [`regia_nav::TriangleClearance`] over [`level_triangles_in`] across the mesh's bounds (raised
/// by the agent height and `max_jump_up`), so a thin wall or a railing the mesh cannot see stops
/// a jump or a drop. Returns the links and how many candidates the colliders rejected. Errors:
/// invalid parameters, or more than `MAX_NAV_LINKS` links.
pub fn generate_level_links(
    world: &World,
    mesh: &NavMesh,
    params: &regia_nav::NavLinkGenParams,
) -> Result<(Vec<regia_nav::OffMeshLink>, usize), regia_nav::NavLinkError> {
    let _span = tracing::info_span!("nav_generate_level_links").entered();
    params.validate()?;
    let lift = Vec3::Y * (mesh.config.agent.height + params.max_jump_up.max(0.0));
    let reach = Vec3::new(1.0, 0.0, 1.0) * (params.max_jump_gap + 2.0 * mesh.config.agent.radius);
    let (lo, hi) = (mesh.min - reach - lift, mesh.max + reach + lift);
    let mut triangles = Vec::new();
    if lo.is_finite() && hi.is_finite() {
        level_triangles_in(world, lo, hi, &mut triangles);
    }
    regia_nav::generate_clear_links(mesh, params, &triangles)
}

/// The deepest parent chain [`world_pose`] follows; a deeper (or cyclic) chain stops there.
const MAX_POSE_DEPTH: usize = 256;

/// An entity's world pose from the current [`Transform`]s of it and its parents, composed the way
/// the streaming cook composes a level's (a missing `Transform` counts as identity). It does not
/// read [`GlobalTransform`], which is one propagation old inside a fixed step: a static collider
/// under a parent lands where the cook put it, on the step it moved.
pub fn world_pose(world: &World, entity: Entity) -> Affine3A {
    let local = |e: Entity| {
        world
            .get::<Transform>(e)
            .map_or(Affine3A::IDENTITY, Transform::compute_affine)
    };
    let mut pose = local(entity);
    let mut at = entity;
    for _ in 0..MAX_POSE_DEPTH {
        let Some(parent) = world.get::<ChildOf>(at).map(ChildOf::parent) else {
            break;
        };
        pose = local(parent) * pose;
        at = parent;
    }
    pose
}

/// Appends the triangles of a box, triangle-mesh or heightfield collider whose owner sits at
/// `owner` (the collider's own offset is applied) touching the XZ box. Other shapes add none. The
/// streaming cook takes the same triangles, so a region rebuild over a cooked tile keeps what the
/// tile held.
pub fn collider_triangles_at(
    collider: &Collider,
    owner: Affine3A,
    min: Vec3,
    max: Vec3,
    out: &mut Vec<[Vec3; 3]>,
) {
    let pose = owner * collider.offset.compute_affine();
    match &collider.shape {
        ColliderShape::Box { half_extents } => box_triangles_in(pose, *half_extents, min, max, out),
        ColliderShape::HeightField {
            nx,
            nz,
            heights,
            scale,
        } => heightfield_triangles_in(pose, (*nx, *nz), heights, *scale, min, max, out),
        ColliderShape::TriMesh { vertices, indices } => {
            for &[a, b, c] in indices {
                let (Some(a), Some(b), Some(c)) = (
                    vertices.get(a as usize),
                    vertices.get(b as usize),
                    vertices.get(c as usize),
                ) else {
                    continue;
                };
                let tri = [
                    pose.transform_point3(*a),
                    pose.transform_point3(*b),
                    pose.transform_point3(*c),
                ];
                if touches(&tri, min, max) {
                    out.push(tri);
                }
            }
        }
        _ => {}
    }
}

/// The quads of a heightfield, as half-open index ranges `[i0, i1) × [j0, j1)` (`i` along the
/// collider's Z with `nx` samples, `j` along its X with `nz`), that can touch the XZ box. Conservative:
/// the caller still tests each triangle. A triangle counts when its XZ bounds touch the box, so the
/// box first grows by one quad's world XZ bounds. The whole field when the pose tilts the field's up
/// axis or the box does not map to finite indices (the cook's unbounded query).
fn heightfield_quad_range(
    pose: Affine3A,
    nx: usize,
    nz: usize,
    scale: Vec3,
    min: Vec3,
    max: Vec3,
) -> [usize; 4] {
    let whole = [0, nx - 1, 0, nz - 1];
    let up = pose.transform_vector3(Vec3::Y);
    let level = up.y.abs() > 0.0 && (up.x.abs() + up.z.abs()) <= 1.0e-6 * up.y.abs();
    if !level || scale.x == 0.0 || scale.z == 0.0 {
        return whole;
    }
    let along_x = pose.transform_vector3(Vec3::new(scale.x / (nz - 1) as f32, 0.0, 0.0));
    let along_z = pose.transform_vector3(Vec3::new(0.0, 0.0, scale.z / (nx - 1) as f32));
    let grow = Vec3::new(
        along_x.x.abs() + along_z.x.abs(),
        0.0,
        along_x.z.abs() + along_z.z.abs(),
    );
    let (min, max) = (min - grow, max + grow);
    let inverse = pose.inverse();
    let (mut lo, mut hi) = ([f32::INFINITY; 2], [f32::NEG_INFINITY; 2]);
    for (x, z) in [(min.x, min.z), (min.x, max.z), (max.x, min.z), (max.x, max.z)] {
        let p = inverse.transform_point3(Vec3::new(x, pose.translation.y, z));
        let i = (p.z / scale.z + 0.5) * (nx - 1) as f32;
        let j = (p.x / scale.x + 0.5) * (nz - 1) as f32;
        lo = [lo[0].min(i), lo[1].min(j)];
        hi = [hi[0].max(i), hi[1].max(j)];
    }
    if !lo.iter().chain(&hi).all(|v| v.is_finite()) {
        return whole;
    }
    // One quad of slack on each side for rounding; the triangle test decides.
    let start = |v: f32, n: usize| ((v.floor() - 1.0).max(0.0) as usize).min(n - 1);
    let end = |v: f32, n: usize| ((v.ceil() + 1.0).max(0.0) as usize).min(n - 1);
    [start(lo[0], nx), end(hi[0], nx), start(lo[1], nz), end(hi[1], nz)]
}

/// Appends the two triangles of each heightfield quad touching the XZ box, with the layout of
/// `ColliderShape::HeightField` (`nx` samples along Z, `nz` along X, spanning `[-0.5, 0.5] × scale`
/// around `pose`; a NaN height is a hole) and the collider's own diagonal, so a saddle reads the
/// same to nav as to physics. Quads come in row order (`j` outer, `i` inner).
fn heightfield_triangles_in(
    pose: Affine3A,
    (nx, nz): (usize, usize),
    heights: &[f32],
    scale: Vec3,
    min: Vec3,
    max: Vec3,
    out: &mut Vec<[Vec3; 3]>,
) {
    // A checked product: a wrapped `nx * nz` (shipping builds have no overflow checks) would pass the
    // length test and index past the end.
    let Some(samples) = nx.checked_mul(nz) else {
        return;
    };
    if nx < 2 || nz < 2 || heights.len() < samples {
        return;
    }
    let vertex = |i: usize, j: usize| {
        let at = Vec3::new(
            (j as f32 / (nz - 1) as f32 - 0.5) * scale.x,
            heights[i + j * nx] * scale.y,
            (i as f32 / (nx - 1) as f32 - 0.5) * scale.z,
        );
        pose.transform_point3(at)
    };
    let [i0, i1, j0, j1] = heightfield_quad_range(pose, nx, nz, scale, min, max);
    for j in j0..j1 {
        for i in i0..i1 {
            let (a, b, c, d) = (
                vertex(i, j),
                vertex(i, j + 1),
                vertex(i + 1, j + 1),
                vertex(i + 1, j),
            );
            if a.is_finite() && b.is_finite() && c.is_finite() && d.is_finite() {
                // The collider's diagonal (parry's default cell status): p10-p01, with
                // a = p00, b = p01, c = p11, d = p10.
                for tri in [[a, d, b], [d, c, b]] {
                    if touches(&tri, min, max) {
                        out.push(tri);
                    }
                }
            }
        }
    }
}

/// Appends the 12 triangles of a box at `pose` touching the XZ box.
fn box_triangles_in(pose: Affine3A, half: Vec3, min: Vec3, max: Vec3, out: &mut Vec<[Vec3; 3]>) {
    let corner = |bits: usize| {
        let sign = |bit: usize| if bits & bit == 0 { -1.0 } else { 1.0 };
        pose.transform_point3(Vec3::new(sign(1) * half.x, sign(2) * half.y, sign(4) * half.z))
    };
    const FACES: [[usize; 4]; 6] = [
        [0, 1, 5, 4],
        [2, 3, 7, 6],
        [0, 1, 3, 2],
        [4, 5, 7, 6],
        [0, 2, 6, 4],
        [1, 3, 7, 5],
    ];
    for [a, b, c, d] in FACES {
        for tri in [
            [corner(a), corner(b), corner(c)],
            [corner(a), corner(c), corner(d)],
        ] {
            if touches(&tri, min, max) {
                out.push(tri);
            }
        }
    }
}

/// Appends the two triangles of each heightfield quad touching the XZ box, leaving out quads whose
/// center a hole of `holes` contains.
#[cfg(feature = "client")]
pub fn terrain_triangles_in(
    map: &TerrainMap,
    holes: Option<&HolesMask>,
    min: Vec3,
    max: Vec3,
    out: &mut Vec<[Vec3; 3]>,
) {
    let (w, h) = (i64::from(map.width()), i64::from(map.height()));
    if w < 2 || h < 2 {
        return;
    }
    let [c0, r0] = map.world_to_grid(min.x, min.z);
    let [c1, r1] = map.world_to_grid(max.x, max.z);
    let (c0, c1) = ((c0.floor() as i64).max(0), (c1.ceil() as i64).min(w - 1));
    let (r0, r1) = ((r0.floor() as i64).max(0), (r1.ceil() as i64).min(h - 1));
    let vertex = |col: i64, row: i64| {
        let [x, z] = map.sample_to_world(col as u32, row as u32);
        Vec3::new(x, map.height_at_sample(col, row), z)
    };
    for row in r0..r1 {
        for col in c0..c1 {
            let (a, b, c, d) = (
                vertex(col, row),
                vertex(col + 1, row),
                vertex(col + 1, row + 1),
                vertex(col, row + 1),
            );
            if holes.is_some_and(|mask| mask.contains((a.x + c.x) * 0.5, (a.z + c.z) * 0.5)) {
                continue;
            }
            out.push([a, b, c]);
            out.push([a, c, d]);
        }
    }
}

#[cfg(all(test, feature = "client"))]
mod tests {
    use regia_nav::{NavMeshBuildParams, NavTileCache, NavTileCacheParams};
    use regia_physics::destruction::{DestructibleId, NavInvalidation};

    use super::*;

    fn map(heights: Vec<f32>, n: u32, world_width: f32) -> TerrainMap {
        TerrainMap::from_heights(n, n, heights, world_width, 1.0, 0.0).expect("a map")
    }

    #[test]
    fn heightfield_triangles_refuses_a_wrapping_sample_count() {
        // 2^62 * 4 wraps to 0 on a 64-bit target without overflow checks (shipping profile); the checked
        // product must refuse it instead of indexing an empty height list.
        let (min, max) = (Vec3::splat(-1e9), Vec3::splat(1e9));
        let mut out = Vec::new();
        heightfield_triangles_in(
            Affine3A::IDENTITY,
            (1usize << 62, 4),
            &[],
            Vec3::ONE,
            min,
            max,
            &mut out,
        );
        assert!(out.is_empty());
        heightfield_triangles_in(
            Affine3A::IDENTITY,
            (usize::MAX, 2),
            &[0.0],
            Vec3::ONE,
            min,
            max,
            &mut out,
        );
        assert!(out.is_empty());
        // A well-formed 2x2 field still yields two triangles per cell.
        heightfield_triangles_in(
            Affine3A::IDENTITY,
            (2, 2),
            &[0.0; 4],
            Vec3::ONE,
            min,
            max,
            &mut out,
        );
        assert_eq!(out.len(), 2);
    }

    /// The bridge reads the world box, not the sample rectangle.
    fn no_rect() -> regia_terrain::DirtyRect {
        regia_terrain::DirtyRect {
            col_start: 0,
            col_end: 0,
            row_start: 0,
            row_end: 0,
        }
    }

    fn cache_params() -> NavTileCacheParams {
        NavTileCacheParams {
            build: NavMeshBuildParams {
                cell_size: 0.25,
                ..Default::default()
            },
            tile_size: 32,
        }
    }

    #[test]
    fn terrain_triangles_are_clipped_to_the_box_and_follow_the_heights() {
        let n = 9;
        let heights: Vec<f32> = (0..n * n).map(|i| (i % n) as f32).collect();
        let m = map(heights, n, 8.0);
        let mut all = Vec::new();
        terrain_triangles_in(&m, None, Vec3::splat(-100.0), Vec3::splat(100.0), &mut all);
        assert_eq!(all.len(), 2 * 8 * 8, "two per quad");

        let mut some = Vec::new();
        terrain_triangles_in(
            &m,
            None,
            Vec3::new(-1.0, 0.0, -1.0),
            Vec3::new(1.0, 0.0, 1.0),
            &mut some,
        );
        assert!(!some.is_empty() && some.len() < all.len());
        for v in some.iter().flatten() {
            assert!(v.x.abs() <= 2.0 && v.z.abs() <= 2.0, "{v}");
            assert!((v.y - (v.x + 4.0)).abs() < 1e-3, "height follows the column: {v}");
        }

        let mut none = Vec::new();
        terrain_triangles_in(&m, None, Vec3::splat(50.0), Vec3::splat(51.0), &mut none);
        assert!(none.is_empty());
    }

    #[test]
    fn a_box_collider_gives_twelve_triangles_at_its_pose() {
        let mut out = Vec::new();
        let pose = Affine3A::from_translation(Vec3::new(10.0, 1.0, 0.0));
        box_triangles_in(
            pose,
            Vec3::new(1.0, 2.0, 3.0),
            Vec3::splat(-100.0),
            Vec3::splat(100.0),
            &mut out,
        );
        assert_eq!(out.len(), 12);
        let top = out.iter().flatten().map(|v| v.y).fold(f32::MIN, f32::max);
        assert_eq!(top, 3.0);
        out.clear();
        box_triangles_in(pose, Vec3::ONE, Vec3::splat(-1.0), Vec3::ONE, &mut out);
        assert!(out.is_empty(), "the box is far from the query");
    }

    /// A world with every kind of navmesh source and some that are not: a terrain with a hole
    /// under a floating-origin offset, a static box under a moved parent, a static triangle mesh,
    /// a dynamic box, a sensor and a sphere.
    fn mixed_sources_world() -> World {
        let mut world = World::new();
        let n = 17u32;
        let heights: Vec<f32> = (0..n * n).map(|i| (i % 7) as f32 * 0.25).collect();
        let mut terrain = regia_terrain::Terrain::new(
            &map(heights, n, 16.0).to_heightfield(),
            16.0,
            1.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            1,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        terrain.holes = Some(HolesMask {
            holes: vec![regia_terrain::HoleEllipse {
                center: [2.0, 2.0],
                radius: [1.5, 1.5],
                rotation_radians: 0.0,
            }],
            ..HolesMask::default()
        });
        world.spawn(terrain);
        world.insert_resource(OriginOffset([3.0, 0.0, -1.0]));
        let parent = world
            .spawn(Transform::from_translation(Vec3::new(30.0, 0.0, 0.0)))
            .id();
        world.spawn((
            Transform::from_translation(Vec3::new(0.0, 1.0, 0.0)),
            ChildOf(parent),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(2.0, 0.5, 2.0),
                },
                ..Collider::default()
            },
        ));
        world.spawn((
            Transform::from_translation(Vec3::new(-20.0, 0.0, 5.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::TriMesh {
                    vertices: vec![
                        Vec3::new(0.0, 0.0, 0.0),
                        Vec3::new(4.0, 0.0, 0.0),
                        Vec3::new(0.0, 0.0, 4.0),
                        Vec3::new(4.0, 0.0, 4.0),
                    ],
                    indices: vec![[0, 2, 1], [1, 2, 3]],
                },
                ..Collider::default()
            },
        ));
        let not_ground = [
            (RigidBody::default(), Collider::default()),
            (
                RigidBody::static_body(),
                Collider {
                    sensor: true,
                    ..Collider::default()
                },
            ),
            (
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Sphere { radius: 1.0 },
                    ..Collider::default()
                },
            ),
        ];
        for (body, collider) in not_ground {
            world.spawn((Transform::IDENTITY, body, collider));
        }
        world
    }

    /// ADR-8594: the captured sources make exactly the triangles [`level_triangles_in`] makes
    /// from the world, in the same order, for the whole level and for a box over part of it.
    #[test]
    fn captured_sources_make_the_triangles_the_world_makes() {
        let world = mixed_sources_world();
        for (min, max) in [
            (Vec3::splat(-1.0e9), Vec3::splat(1.0e9)),
            (Vec3::new(-6.0, -10.0, -6.0), Vec3::new(1.0, 10.0, 2.0)),
            (Vec3::new(25.0, -10.0, -5.0), Vec3::new(35.0, 10.0, 5.0)),
        ] {
            let mut from_world = Vec::new();
            level_triangles_in(&world, min, max, &mut from_world);
            let sources = LevelNavSources::capture(&world, min, max);
            assert_eq!(
                sources.len(),
                3,
                "the terrain, the box and the mesh; nothing else"
            );
            let mut from_sources = Vec::new();
            assert!(sources.triangles_in(min, max, &mut from_sources, |_, _| true));
            assert!(!from_world.is_empty());
            assert_eq!(from_sources, from_world, "box {min} .. {max}");
        }
    }

    /// ADR-8594: the read asks before each source and stops where it is told, and the sources are
    /// copies: the world changing after the capture does not change what they make.
    #[test]
    fn a_read_stops_between_sources_and_ignores_later_world_changes() {
        let mut world = mixed_sources_world();
        let (min, max) = (Vec3::splat(-1.0e9), Vec3::splat(1.0e9));
        let sources = LevelNavSources::capture(&world, min, max);
        let mut asked = Vec::new();
        let mut partial = Vec::new();
        let finished = sources.triangles_in(min, max, &mut partial, |done, total| {
            asked.push((done, total));
            done < 1
        });
        assert!(!finished, "told to stop before the second source");
        assert_eq!(asked, vec![(0, 3), (1, 3)]);
        let mut terrain_only = Vec::new();
        LevelNavSources {
            sources: sources.sources[..1].to_vec(),
        }
        .triangles_in(min, max, &mut terrain_only, |_, _| true);
        assert_eq!(partial, terrain_only, "only the first source was read");

        let mut before = Vec::new();
        sources.triangles_in(min, max, &mut before, |_, _| true);
        let colliders: Vec<Entity> = world
            .query_filtered::<Entity, With<Collider>>()
            .iter(&world)
            .collect();
        for entity in colliders {
            world.despawn(entity);
        }
        let mut after = Vec::new();
        assert!(sources.triangles_in(min, max, &mut after, |_, _| true));
        assert_eq!(after, before, "the capture owns its geometry");
        assert!(LevelNavSources::capture(&World::new(), min, max).is_empty());
    }

    fn zone_app() -> App {
        let mut app = App::new();
        app.add_plugins(NavWorldPlugin);
        let floor = vec![
            [
                Vec3::new(-16.0, 0.0, -16.0),
                Vec3::new(16.0, 0.0, -16.0),
                Vec3::new(16.0, 0.0, 16.0),
            ],
            [
                Vec3::new(-16.0, 0.0, -16.0),
                Vec3::new(16.0, 0.0, 16.0),
                Vec3::new(-16.0, 0.0, 16.0),
            ],
        ];
        app.insert_resource(NavTileCache::build(cache_params(), &floor).expect("builds"));
        app
    }

    fn pose(x: f32, z: f32) -> Transform {
        Transform {
            translation: Vec3::new(x, 0.0, z),
            ..Default::default()
        }
    }

    fn area_at(app: &mut App, x: f32, z: f32) -> u8 {
        app.world_mut().run_schedule(FixedUpdate);
        let mut cache = app.world_mut().resource_mut::<NavTileCache>();
        cache.update();
        let (poly, _) = cache
            .query()
            .find_nearest(Vec3::new(x, 0.0, z), Vec3::splat(2.0))
            .expect("on the floor");
        cache.mesh().polys[poly as usize].area
    }

    const MUD: NavAreaZone = NavAreaZone {
        half_extents: Vec3::new(2.0, 1.0, 2.0),
        area: 3,
    };

    #[test]
    fn an_area_zone_paints_follows_its_entity_and_goes_with_it() {
        let mut app = zone_app();
        let e = app.world_mut().spawn((MUD, pose(-6.0, 0.0))).id();
        assert_eq!(area_at(&mut app, -6.0, 0.0), 3);
        assert_eq!(area_at(&mut app, 6.0, 0.0), 0);

        app.world_mut().entity_mut(e).insert(pose(6.0, 0.0));
        assert_eq!(area_at(&mut app, 6.0, 0.0), 3, "it moved");
        assert_eq!(area_at(&mut app, -6.0, 0.0), 0, "and left nothing behind");
        assert_eq!(app.world().resource::<NavTileCache>().area_volumes().count(), 1);

        app.world_mut().entity_mut(e).remove::<NavAreaZone>();
        assert_eq!(area_at(&mut app, 6.0, 0.0), 0);
        assert_eq!(app.world().resource::<NavTileCache>().area_volumes().count(), 0);
    }

    #[test]
    fn a_turned_zone_paints_its_own_box() {
        let mut app = zone_app();
        let long = NavAreaZone {
            half_extents: Vec3::new(8.0, 1.0, 1.0),
            area: 3,
        };
        let mut turned = pose(0.0, 0.0);
        turned.rotation = regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        app.world_mut().spawn((long, turned));
        assert_eq!(area_at(&mut app, 0.0, 6.0), 3, "long along z now");
        assert_eq!(area_at(&mut app, 6.0, 0.0), 0);
    }

    #[test]
    fn a_still_zone_queues_nothing_and_a_bad_zone_stays_out() {
        let mut app = zone_app();
        let e = app.world_mut().spawn((MUD, pose(-6.0, 0.0))).id();
        area_at(&mut app, -6.0, 0.0);
        app.world_mut().entity_mut(e).insert(pose(-6.0, 0.0));
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world().resource::<NavTileCache>().pending_tile_count(),
            0,
            "the same pose written again changes nothing"
        );
        let flat = NavAreaZone {
            half_extents: Vec3::ZERO,
            area: 3,
        };
        let past = NavAreaZone {
            half_extents: Vec3::ONE,
            area: 64,
        };
        app.world_mut().spawn((flat, pose(6.0, 0.0)));
        app.world_mut().spawn((past, pose(6.0, 6.0)));
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(app.world().resource::<NavTileCache>().area_volumes().count(), 1);
    }

    #[test]
    fn a_new_cache_gets_the_zones_painted_again() {
        let mut app = zone_app();
        app.world_mut().spawn((MUD, pose(-6.0, 0.0)));
        assert_eq!(area_at(&mut app, -6.0, 0.0), 3);
        let fresh = zone_app()
            .world_mut()
            .remove_resource::<NavTileCache>()
            .expect("a cache");
        app.world_mut().insert_resource(fresh);
        assert_eq!(area_at(&mut app, -6.0, 0.0), 3);
        assert_eq!(app.world().resource::<NavTileCache>().area_volumes().count(), 1);
    }

    #[test]
    fn destruction_and_terrain_messages_reach_the_queue_in_their_schedules() {
        let mut app = App::new();
        app.add_plugins(NavWorldPlugin);
        assert!(app.world().resource::<NavRegionQueue>().has_source());
        app.world_mut().write_message(NavmeshInvalidated(NavInvalidation {
            destructible: DestructibleId(1),
            min: [1.0, 0.0, 1.0],
            max: [2.0, 3.0, 2.0],
        }));
        // No cache yet: the fixed step reads the message and the box waits.
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world().resource::<NavRegionQueue>().boxes().next(),
            Some((Vec3::new(1.0, 0.0, 1.0), Vec3::new(2.0, 3.0, 2.0), "destruction"))
        );
        app.world_mut().write_message(TerrainHeightsChanged {
            entity: None,
            patch: 7,
            rect: no_rect(),
            world_min: [-3.0, 0.0, -3.0],
            world_max: [-2.0, 1.0, -2.0],
            generation: 1,
        });
        app.world_mut().run_schedule(PostUpdate);
        let boxes: Vec<_> = app.world().resource::<NavRegionQueue>().boxes().collect();
        assert_eq!(boxes.len(), 2);
        assert_eq!(boxes[1].2, "terrain");
    }

    /// Spec 78 § 12.2: nav stays in local coordinates after a rebase though the map does not move.
    #[test]
    fn a_rebase_keeps_terrain_geometry_and_edits_in_local_coordinates() {
        let n = 33u32;
        let terrain = regia_terrain::Terrain::new(
            &map(vec![3.0; (n * n) as usize], n, 16.0).to_heightfield(),
            16.0,
            1.0,
            0.0,
            regia_terrain::CdlodConfig::default(),
            1,
            regia_terrain::TerrainLayers::default(),
        )
        .expect("terrain");
        let mut app = App::new();
        app.add_plugins(NavWorldPlugin);
        app.world_mut().spawn(terrain);
        let offset = Vec3::new(1024.0, 0.0, -2048.0);
        app.insert_resource(OriginOffset([1024.0, 0.0, -2048.0]));

        // A local box over the map: triangles come back around it, in local coordinates.
        let (lo, hi) = (
            Vec3::new(-2.0, -10.0, -2.0) - offset,
            Vec3::new(2.0, 10.0, 2.0) - offset,
        );
        let mut tris = Vec::new();
        assert!(level_triangles_in(app.world(), lo, hi, &mut tris));
        assert!(!tris.is_empty());
        for vertex in tris.iter().flatten() {
            assert!(
                (vertex.x + 1024.0).abs() < 4.0 && (vertex.z - 2048.0).abs() < 4.0,
                "{vertex}"
            );
        }
        // The same box without the offset is off the map.
        let mut none = Vec::new();
        level_triangles_in(app.world(), lo + offset * 8.0, hi + offset * 8.0, &mut none);
        assert!(none.is_empty());

        // An edit in the terrain's space queues a local region.
        app.world_mut().write_message(TerrainHeightsChanged {
            entity: None,
            patch: 1,
            rect: no_rect(),
            world_min: [-1.0, 0.0, -1.0],
            world_max: [1.0, 1.0, 1.0],
            generation: 1,
        });
        app.world_mut().run_schedule(PostUpdate);
        let queue = app.world().resource::<NavRegionQueue>();
        assert_eq!(queue.len(), 1);
        let (queued_lo, queued_hi, _) = queue.boxes().next().expect("queued");
        assert_eq!(queued_lo, Vec3::new(-1.0, 0.0, -1.0) - offset);
        assert_eq!(queued_hi, Vec3::new(1.0, 1.0, 1.0) - offset);
    }

    #[test]
    fn a_terrain_edit_rebuilds_the_navmesh_from_the_live_terrain() {
        // A flat 16 m terrain is raised by 2 m over one strip, the way a height patch leaves it.
        let n = 33u32;
        let spacing = 0.5;
        let flat = vec![0.0f32; (n * n) as usize];
        let map_flat = map(flat.clone(), n, 16.0);
        let mut tris = Vec::new();
        terrain_triangles_in(
            &map_flat,
            None,
            Vec3::splat(-100.0),
            Vec3::splat(100.0),
            &mut tris,
        );

        let mut raised = flat;
        for row in 8..=10usize {
            for col in 0..n as usize {
                raised[row * n as usize + col] = 2.0;
            }
        }
        let map_raised = map(raised, n, 16.0);
        let (z0, z1) = (-8.0 + 8.0 * spacing - spacing, -8.0 + 10.0 * spacing + spacing);

        let mut app = App::new();
        app.add_plugins(NavWorldPlugin);
        app.insert_resource(NavTileCache::build(cache_params(), &tris).expect("builds"));
        app.world_mut()
            .resource_mut::<NavRegionQueue>()
            .set_source(std::sync::Arc::new(move |_, lo, hi, out| {
                terrain_triangles_in(&map_raised, None, lo, hi, out);
                true
            }));
        let before = app.world().resource::<NavTileCache>().revision();
        app.world_mut().write_message(TerrainHeightsChanged {
            entity: None,
            patch: 1,
            rect: no_rect(),
            world_min: [-8.0, 0.0, z0],
            world_max: [8.0, 2.0, z1],
            generation: 1,
        });
        app.world_mut().run_schedule(PostUpdate);
        for _ in 0..20 {
            app.world_mut().run_schedule(FixedUpdate);
        }
        let cache = app.world().resource::<NavTileCache>();
        assert!(cache.revision() > before);
        let (_, p) = cache
            .query()
            .find_nearest(Vec3::new(0.0, 2.0, -3.5), Vec3::splat(1.0))
            .expect("on the raised strip");
        assert!((p.y - 2.0).abs() < 1e-3, "height {}", p.y);
        let end = Vec3::new(0.0, 0.0, 4.0);
        let start = Vec3::new(0.0, 0.0, -7.0);
        assert!(
            cache.query().find_path(start, end, Vec3::splat(1.0)).is_none(),
            "the 2 m step cuts the map"
        );
    }

    /// Nothing is staged here: physics, destruction and the bridge run in one app, the wall is the
    /// destruction sample's, and the navmesh comes from the default geometry source.
    #[test]
    fn a_wall_brought_down_opens_the_navmesh_it_closed() {
        use regia_physics::PhysicsPlugin;
        use regia_physics::destruction::sample::{SAMPLE_SEEDS, WALL_POSE, sample_assets};
        use regia_physics::destruction::{Damage, DamageKind, DestructiblePose, DestructionPlugin};

        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        app.insert_resource(FixedTime::default());
        app.add_plugins((PhysicsPlugin, DestructionPlugin::default(), NavWorldPlugin));
        // A walkway narrower than the 4 m wall that stands across it at z = -5.
        app.world_mut().spawn((
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(1.5, 0.5, 8.0),
                },
                ..Default::default()
            },
            RigidBody {
                kind: BodyKind::Static,
                ..Default::default()
            },
            Transform {
                translation: Vec3::new(0.0, -0.5, -5.0),
                ..Default::default()
            },
        ));
        let (wall, _) = sample_assets().expect("assets");
        let id = app.world_mut().resource_scope(|world, mut d: Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            d.define(
                &mut physics,
                wall,
                DestructiblePose::at(WALL_POSE),
                SAMPLE_SEEDS[0],
            )
            .expect("the wall")
        });
        // The wall's colliders exist from its first tick on.
        let tick = |app: &App| app.world().resource::<Destruction>().world.tick();
        let next_tick = |app: &mut App| {
            let at = tick(app);
            for _ in 0..16 {
                app.update();
                if tick(app) > at {
                    return;
                }
            }
            panic!("the fixed step stopped at tick {at}");
        };
        next_tick(&mut app);

        let mut tris = Vec::new();
        let far = Vec3::splat(100.0);
        assert!(level_triangles_in(app.world(), -far, far, &mut tris));
        app.insert_resource(NavTileCache::build(cache_params(), &tris).expect("builds"));
        let (start, end) = (Vec3::new(0.0, 0.0, -9.0), Vec3::new(0.0, 0.0, -1.0));
        let path = |app: &App| {
            app.world()
                .resource::<NavTileCache>()
                .query()
                .find_path(start, end, Vec3::splat(1.0))
        };
        assert!(path(&app).is_none(), "the wall closes the walkway");
        let revision = app.world().resource::<NavTileCache>().revision();

        app.world_mut()
            .resource_mut::<Destruction>()
            .world
            .damage(Damage {
                target: Some(id),
                kind: DamageKind::Explosion {
                    center: [0.0, 1.0, -4.6],
                    radius: 5.0,
                    impulse: 50_000.0,
                },
            })
            .expect("queue");
        for _ in 0..30 {
            next_tick(&mut app);
        }

        let totals = app.world().resource::<Destruction>().message_totals();
        assert!(totals.nav > 0, "the break reported a region: {totals:?}");
        let queue = app.world().resource::<NavRegionQueue>();
        assert!(queue.is_empty() && queue.stats().slices > 0, "{queue:?}");
        assert_eq!(queue.stats().failed, 0);
        assert!(app.world().resource::<NavTileCache>().revision() > revision);
        let opened = path(&app).expect("the charge opened the walkway");
        assert!(opened.length >= 8.0, "a route through: {opened:?}");
    }

    /// A chunk turned 45 degrees about +Y is a diamond from above: the corners of its bounding box
    /// are open ground, and the navmesh source gives them no triangle (ADR-2305).
    #[test]
    fn a_turned_chunk_gives_its_hull_not_its_bounding_box() {
        use regia_physics::PhysicsPlugin;
        use regia_physics::destruction::{
            BoxGrid, DestructibleAsset, DestructiblePose, DestructionMaterial, DestructionPlugin, GridSupport,
        };

        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        app.insert_resource(FixedTime::default());
        app.add_plugins((PhysicsPlugin, DestructionPlugin::default()));
        // One 2 m cube, its centre 1 m up, turned 45 degrees about +Y.
        let cube = DestructibleAsset::box_grid(&BoxGrid {
            name: "cube".into(),
            counts: [1, 1, 1],
            cell: [2.0, 2.0, 2.0],
            density: 2000.0,
            strength_per_area: 1000.0,
            support: GridSupport::Bottom,
            material: DestructionMaterial::default(),
        })
        .expect("cube");
        let half = std::f32::consts::FRAC_PI_8;
        let pose = DestructiblePose {
            translation: [0.0, 0.0, 0.0],
            rotation: [0.0, half.sin(), 0.0, half.cos()],
        };
        app.world_mut().resource_scope(|world, mut d: Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            d.define(&mut physics, std::sync::Arc::new(cube), pose, 1)
                .expect("the cube")
        });
        for _ in 0..4 {
            app.update();
        }
        let chunk_tris = |min: Vec3, max: Vec3| {
            let mut tris = Vec::new();
            assert!(level_triangles_in(app.world(), min, max, &mut tris));
            tris
        };
        // The whole cube: 12 triangles, every corner within sqrt(2) m of the axis.
        let all = chunk_tris(Vec3::splat(-10.0), Vec3::splat(10.0));
        assert!(all.len() >= 12, "{}", all.len());
        let reach = all
            .iter()
            .flatten()
            .map(|v| Vec3::new(v.x, 0.0, v.z).length())
            .fold(0.0f32, f32::max);
        assert!((reach - std::f32::consts::SQRT_2).abs() < 1e-3, "{reach}");
        // Every vertex is on the diamond |x| + |z| <= sqrt(2); the bounding box's corners would be
        // at 2 sqrt(2).
        let l1 = all
            .iter()
            .flatten()
            .map(|v| v.x.abs() + v.z.abs())
            .fold(0.0f32, f32::max);
        assert!(l1 <= std::f32::consts::SQRT_2 + 1e-3, "{l1}");
        // A box away from the cube gets nothing.
        assert!(chunk_tris(Vec3::new(3.0, -1.0, 3.0), Vec3::new(4.0, 3.0, 4.0)).is_empty());
    }

    #[test]
    fn the_level_area_table_applies_on_change_and_on_a_new_cache() {
        let mut app = zone_app();
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world().resource::<NavTileCache>().area_costs().get(3),
            1.0,
            "no table, no change"
        );
        app.insert_resource(NavAreaCostTable {
            areas: vec![NavAreaCostEntry {
                area: 3,
                cost: 4.0,
                name: "mud".into(),
            }],
        });
        app.world_mut().run_schedule(FixedUpdate);
        let revision = app.world().resource::<NavTileCache>().revision();
        assert_eq!(app.world().resource::<NavTileCache>().area_costs().get(3), 4.0);
        // A cost set by hand holds until the table changes.
        let mut by_hand = *app.world().resource::<NavTileCache>().area_costs();
        by_hand.set(3, 2.0).expect("valid");
        app.world_mut()
            .resource_mut::<NavTileCache>()
            .set_area_costs(by_hand);
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(app.world().resource::<NavTileCache>().area_costs().get(3), 2.0);
        assert!(app.world().resource::<NavTileCache>().revision() > revision);
        app.world_mut().resource_mut::<NavAreaCostTable>().areas[0].cost = 5.0;
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(app.world().resource::<NavTileCache>().area_costs().get(3), 5.0);
        // A replaced cache takes the table.
        let fresh = NavTileCache::build(cache_params(), &[[Vec3::ZERO, Vec3::X, Vec3::Z]]).expect("builds");
        app.insert_resource(fresh);
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(app.world().resource::<NavTileCache>().area_costs().get(3), 5.0);
        // An invalid table is left out.
        app.world_mut().resource_mut::<NavAreaCostTable>().areas[0].cost = -1.0;
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(app.world().resource::<NavTileCache>().area_costs().get(3), 5.0);
        assert_eq!(
            app.world()
                .resource::<NavAreaCostTable>()
                .costs()
                .map_err(|e| e.code()),
            Err("nav_invalid_area_cost")
        );
    }

    /// The zone yaw is a deterministic-kernel value (spec 15 § 5), not the platform libm: a Linux
    /// cook and a Windows runtime must paint the same yaw bits into their tiles. (glibc's atan2f
    /// happens to agree with the kernel on this host; the pin guards the platforms where the
    /// libm's last digit differs.)
    #[test]
    fn zone_yaw_bits_are_pinned() {
        let zone = NavAreaZone {
            half_extents: Vec3::new(3.0, 1.0, 1.0),
            area: 9,
        };
        let center = Vec3::new(4.0, 0.5, -2.0);
        let forward = Vec3::new(0.6, 0.0, 0.8);
        let live = zone_volume(&zone, center, forward);
        // A pose whose local +X is exactly `forward` (X picks the first matrix column).
        let basis = regia_core::math::Mat3::from_cols(forward, Vec3::Y, forward.cross(Vec3::Y));
        let cooked = nav_area_zone_volume(&zone, Affine3A::from_mat3(basis));
        for volume in [&live, &cooked] {
            match volume.shape {
                NavObstacleShape::OrientedBox { yaw, .. } => {
                    assert_eq!(
                        yaw.to_bits(),
                        det_atan2(-0.8, 0.6).to_bits(),
                        "the zone yaw is not the deterministic kernel's bits"
                    );
                }
                shape => panic!("expected an oriented box, got {shape:?}"),
            }
        }
        match (live.shape, cooked.shape) {
            (NavObstacleShape::OrientedBox { yaw: a, .. }, NavObstacleShape::OrientedBox { yaw: b, .. }) => {
                assert_eq!(a.to_bits(), b.to_bits(), "the cook and the live path disagree")
            }
            pair => panic!("expected oriented boxes, got {pair:?}"),
        }
    }

    #[test]
    fn the_cook_and_the_runtime_paint_a_zone_alike() {
        let zone = NavAreaZone {
            half_extents: Vec3::new(3.0, 1.0, 1.0),
            area: 9,
        };
        let turned = Transform {
            translation: Vec3::new(4.0, 0.5, -2.0),
            rotation: regia_core::math::Quat::from_rotation_y(0.7),
            ..Default::default()
        };
        let cooked = nav_area_zone_volume(&zone, turned.compute_affine());
        let live = zone_volume(&zone, turned.translation, turned.rotation * Vec3::X);
        assert_eq!(cooked.area, live.area);
        match (cooked.shape, live.shape) {
            (
                NavObstacleShape::OrientedBox {
                    center: a,
                    half_extents: ha,
                    yaw: ya,
                },
                NavObstacleShape::OrientedBox {
                    center: b,
                    half_extents: hb,
                    yaw: yb,
                },
            ) => {
                assert!((a - b).length() < 1e-5);
                assert_eq!(ha, hb);
                assert!((ya - yb).abs() < 1e-5);
            }
            other => panic!("not oriented boxes: {other:?}"),
        }
        assert_eq!(NavAreaZone::default().half_extents, Vec3::ONE);
    }

    /// A heightfield collider with `nx` × `nz` samples of varying height.
    fn heightfield(nx: usize, nz: usize, scale: Vec3) -> Collider {
        Collider {
            shape: ColliderShape::HeightField {
                nx,
                nz,
                heights: (0..nx * nz).map(|k| ((k * 7) % 5) as f32 * 0.25).collect(),
                scale,
            },
            ..Collider::default()
        }
    }

    /// The quad range only speeds the query up: a bounded query is the unbounded one filtered by
    /// the box, in the same order, for a level, a turned and a tilted field.
    #[test]
    fn a_bounded_heightfield_query_is_the_full_field_filtered() {
        let collider = heightfield(17, 13, Vec3::new(48.0, 3.0, 64.0));
        let poses = [
            Affine3A::from_translation(Vec3::new(5.0, 1.0, -3.0)),
            Affine3A::from_rotation_translation(
                regia_core::math::Quat::from_rotation_y(0.6),
                Vec3::new(-7.0, 0.0, 2.0),
            ),
            Affine3A::from_rotation_translation(regia_core::math::Quat::from_rotation_x(0.3), Vec3::ZERO),
        ];
        let far = Vec3::splat(f32::MAX);
        for pose in poses {
            let mut all = Vec::new();
            collider_triangles_at(&collider, pose, -far, far, &mut all);
            assert_eq!(all.len(), 2 * 16 * 12, "every quad, unbounded");
            let (min, max) = (Vec3::new(-4.0, -50.0, -9.0), Vec3::new(6.0, 50.0, 3.0));
            let mut some = Vec::new();
            collider_triangles_at(&collider, pose, min, max, &mut some);
            let expected: Vec<_> = all.iter().filter(|t| touches(t, min, max)).copied().collect();
            assert!(!expected.is_empty() && expected.len() < all.len());
            assert_eq!(some, expected, "pose {pose:?}");
        }
    }

    /// The level link generator (ADR-2420) sees colliders: two floors with a gap get a jump, and a
    /// wall standing in the gap, which the navmesh cannot see, takes it away.
    #[test]
    fn the_level_link_generator_respects_colliders() {
        let floor = |x: f32| {
            (
                Transform::from_translation(Vec3::new(x, -0.5, 0.0)),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::new(2.5, 0.5, 4.0),
                    },
                    ..Collider::default()
                },
            )
        };
        let mut world = World::new();
        world.spawn(floor(-3.5));
        world.spawn(floor(3.5));
        let mut tris = Vec::new();
        level_triangles_in(&world, Vec3::splat(-100.0), Vec3::splat(100.0), &mut tris);
        let build = NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        };
        let mesh = regia_nav::NavMeshBuilder::new(build)
            .build(&tris)
            .expect("builds");
        let params = regia_nav::NavLinkGenParams {
            max_jump_gap: 3.0,
            ..Default::default()
        };
        let (open, rejected) = generate_level_links(&world, &mesh, &params).expect("generates");
        assert!(
            open.iter()
                .any(|l| l.kind == regia_nav::NavLinkKind::Jump && l.from.x * l.to.x < 0.0),
            "a jump over the gap: {open:?}"
        );
        assert_eq!(rejected, 0);
        // A tall thin wall in the gap: the mesh is the same, the jumps are gone.
        world.spawn((
            Transform::from_translation(Vec3::new(0.0, 1.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.05, 2.0, 5.0),
                },
                ..Collider::default()
            },
        ));
        let blind = regia_nav::generate_links(&mesh, &params, |_| true).expect("generates");
        assert_eq!(blind.len(), open.len(), "the mesh alone cannot see the wall");
        let (walled, rejected) = generate_level_links(&world, &mesh, &params).expect("generates");
        assert!(
            walled.iter().all(|l| l.from.x * l.to.x >= 0.0),
            "no jump through the wall: {walled:?}"
        );
        assert!(rejected > 0);
    }

    /// The documented fixed-step order (ADR-2420, module docs): physics, destruction write-back, the
    /// region rebuilds, the crowd, the lanes; and a crowd ordered before physics is a cycle.
    #[test]
    fn the_fixed_step_runs_physics_then_the_navmesh_then_the_crowds() {
        use regia_nav::{NavCrowdPlugin, NavCrowdStep, NavLanePlugin, NavLaneStep, NavRegionStep};
        use regia_physics::destruction::DestructionPlugin;
        use regia_physics::{PhysicsPlugin, PhysicsStep};

        #[derive(Resource, Default)]
        struct Order(Vec<&'static str>);

        let app_with = |extra: fn(&mut App)| {
            let mut app = App::new();
            app.insert_resource(FixedTime::default());
            app.add_plugins((
                PhysicsPlugin,
                DestructionPlugin::default(),
                NavWorldPlugin,
                NavCrowdPlugin,
                NavLanePlugin,
            ));
            app.init_resource::<Order>();
            app.add_systems(
                FixedUpdate,
                (
                    (|mut o: ResMut<Order>| o.0.push("physics")).in_set(PhysicsStep),
                    (|mut o: ResMut<Order>| o.0.push("write_back"))
                        .after(write_back_destruction)
                        .before(NavRegionStep),
                    // Each probe carries the edges of the engine system it stands for: the sets
                    // themselves are not ordered.
                    (|mut o: ResMut<Order>| o.0.push("regions"))
                        .in_set(NavRegionStep)
                        .before(NavCrowdStep),
                    (|mut o: ResMut<Order>| o.0.push("crowd")).in_set(NavCrowdStep),
                    (|mut o: ResMut<Order>| o.0.push("lanes"))
                        .in_set(NavLaneStep)
                        .after(NavRegionStep)
                        .after(NavCrowdStep),
                ),
            );
            extra(&mut app);
            app
        };
        let mut app = app_with(|_| {});
        app.world_mut().run_schedule(FixedUpdate);
        assert_eq!(
            app.world().resource::<Order>().0,
            ["physics", "write_back", "regions", "crowd", "lanes"]
        );
        // A crowd system before physics closes a cycle: the schedule does not build.
        let mut cyclic = app_with(|app| {
            app.add_systems(FixedUpdate, (|| {}).in_set(NavCrowdStep).before(PhysicsStep));
        });
        let built = cyclic
            .world_mut()
            .schedule_scope(FixedUpdate, |world, schedule| schedule.initialize(world));
        assert!(built.is_err(), "crowd before physics must be refused");
    }

    /// Gap row "Streaming runtime remainder" (5): the run-time nav geometry places a static collider
    /// under a parent at the composed pose of the current transforms, not its local `Transform`,
    /// and does not wait for the transform propagation.
    #[test]
    fn a_nested_static_collider_is_placed_at_its_composed_pose() {
        let mut world = World::new();
        let root = world
            .spawn(Transform::from_translation(Vec3::new(100.0, 0.0, 0.0)))
            .id();
        let group = world
            .spawn((
                Transform {
                    translation: Vec3::new(0.0, 0.0, 20.0),
                    rotation: regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                    ..Default::default()
                },
                ChildOf(root),
            ))
            .id();
        let wall = Transform::from_translation(Vec3::new(10.0, 0.0, 0.0));
        let block = Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::ONE,
            },
            ..Collider::default()
        };
        let child = world
            .spawn((wall, RigidBody::static_body(), block, ChildOf(group)))
            .id();
        let expected = Affine3A::from_translation(Vec3::new(100.0, 0.0, 0.0))
            * Affine3A::from_rotation_translation(
                regia_core::math::Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
                Vec3::new(0.0, 0.0, 20.0),
            )
            * wall.compute_affine();
        assert!(world_pose(&world, child).abs_diff_eq(expected, 1.0e-4));
        let mut out = Vec::new();
        assert!(level_triangles_in(
            &world,
            Vec3::splat(-1.0e4),
            Vec3::splat(1.0e4),
            &mut out
        ));
        assert_eq!(out.len(), 12);
        let lo = out.iter().flatten().fold(Vec3::splat(f32::MAX), |a, v| a.min(*v));
        let hi = out.iter().flatten().fold(Vec3::splat(f32::MIN), |a, v| a.max(*v));
        let center = (lo + hi) * 0.5;
        // The group turns +X to -Z: the wall's 10 m along X go to -10 m along Z.
        assert!(
            (center - Vec3::new(100.0, 0.0, 10.0)).length() < 1.0e-3,
            "{center}"
        );
    }
}
