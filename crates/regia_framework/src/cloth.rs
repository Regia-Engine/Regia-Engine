// SPDX-License-Identifier: MPL-2.0
// Original code (spec 55 § 2.2, § 2.4, § 2.9, § 3.2). Linear blend skinning is
// `regia_animation::cpu_skin` (glTF 2.0 § 3.7.3 joint matrices); nothing is ported.

//! Cloth and soft bodies in the game world (spec 55 § 2.1, § 3.1): the [`ClothSim`],
//! [`SoftSim`], [`ClothBodyProxy`] and [`ClothOutput`] components and the glue systems that
//! connect a cooked [`ClothAsset`] / [`SoftAsset`] to the solvers of
//! `regia_physics::cloth` and to the renderer.
//!
//! [`ClothGluePlugin`] (it adds `regia_physics::cloth::ClothPlugin` when missing) runs, in
//! `FixedUpdate` after the rigid step and before the cloth step, every edge explicit:
//!
//! 1. `spawn_cloth_solvers`: builds a solver per [`ClothSim`] / [`SoftSim`] into `ClothSet` /
//!    `SoftSet` (and rebuilds it when the asset `Arc` changes: hot reload enters `Settling`);
//!    removes the solvers of despawned instances;
//! 2. [`switch_cloth_sim_lods`]: moves a cloth onto the coarser sim mesh of its asset at LOD
//!    level 2 and back, keeping its motion;
//! 3. [`sync_cloth_colliders`]: body proxies to solver colliders (previous and current shape);
//! 4. [`build_cloth_snapshots`]: a world snapshot per cloth from the rigid world (colliders
//!    around the particle bounds, filtered by the cloth's layer, sensors excluded);
//! 5. [`sync_cloth_pose`]: skins the sim mesh with the owner's joint palette (or places the
//!    rest mesh by the entity transform) and sets the pose targets and root motion;
//! 6. [`sync_cloth_visibility`]: pause when hidden (`InheritedVisibility`), distance LOD from the nearest-order
//!    camera, the particle budget;
//!
//! then, after the cloth and soft step, [`collect_cloth_output`] swaps [`ClothOutput`],
//! [`emit_cloth_messages`] reports state changes, teleports and tears, and
//! [`apply_cloth_reaction`] turns two-way contact impulses into `ExternalImpulse`s for the next
//! rigid step. In `PostUpdate`, [`publish_cloth_mesh`] interpolates the output by the fixed-step
//! overstep and publishes the render section as a new `Arc<CpuMesh>` on the section entity's
//! `MeshRenderer` (ADR-1585 v1; the renderer frees the previous upload when its last `Arc`
//! drops, ADR-2083).
//!
//! The last system of that chain, [`feed_scene_wind_to_cloth`], adds the scene wind
//! ([`SceneWind`], spec 99 § 3.3) to each cloth solver's global wind term, scaled by the cloth's
//! `wind_scale` (ADR-2884).
//!
//! [`ClothSimSource`] and [`SoftSimSource`] are what scene files store: `resolve_cloth_sources`
//! loads the cooked asset ([`load_cloth_asset`], [`load_soft_asset`]) into a [`ClothSim`] /
//! [`SoftSim`] and follows hot reloads.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

use regia_animation::cpu_skin::{SkinInfluence, skin_points};
use regia_animation::{SkinPalette, SkinnedMesh};
use regia_app::prelude::*;
use regia_app::{PostUpdate, PostUpdateSet};
use regia_asset::{AssetId, AssetServer};
use regia_core::math::{Affine3A, Isometry, Mat4, Quat, Vec3};
use regia_ecs::spawn_order::{SharedSpawnOrder, SpawnKey, SpawnOrder};
use regia_physics::cloth::asset::{ClothLod, CookedInfluence};
use regia_physics::cloth::asset::{bind_point, vertex_normals};
use regia_physics::cloth::{
    ClothAsset, ClothCollider, ClothError, ClothParams, ClothSet, ClothShape, ClothSolver, ClothState,
    ClothTuning, LOD_OFF, PoseTarget, SnapshotKind, SnapshotShape, SoftAsset, SoftSet, SoftSolver,
    WorldSnapshot, lod,
};
use regia_physics::{
    Collider, ColliderShape, CollisionLayer, ExternalImpulse, PhysicsLayers, PhysicsPose, PhysicsWorld,
    PreviousPhysicsPose, QueryFilter, RigidBody, ShapeAxis, WorldQueries,
};
use regia_render::{Camera, CpuMesh, LabVertex, MeshRenderer};
use regia_scene::GlobalTransform;
use regia_scene::components::InheritedVisibility;
use regia_scene::weather::{SceneWind, SceneWindMode};
use tracing::{debug_span, warn};

/// Default margin around a cloth's particle bounds for its world snapshot (meters).
pub const SNAPSHOT_MARGIN: f32 = 0.5;
/// Default per-body cap of two-way reaction impulses per fixed step (N·s).
pub const MAX_REACTION_IMPULSE: f32 = 5.0;

/// `Arc<CpuMesh>` values `publish_cloth_mesh` created in one app (tests insert it; app-local, so
/// parallel tests cannot add to it). A back buffer rewritten in place does not increment it.
#[cfg(test)]
#[derive(Resource, Default)]
pub struct ClothRenderArcs {
    /// New arcs so far.
    pub arcs: u64,
}

#[cfg(test)]
fn charge_solver_probe(state: &mut ClothGlueState, hit: bool, len: usize) {
    let n = u64::try_from(if hit { 1 } else { len.max(1) }).unwrap_or(u64::MAX);
    state.solver_probes = state.solver_probes.wrapping_add(n);
}

#[cfg(not(test))]
fn charge_solver_probe(_state: &mut ClothGlueState, hit: bool, len: usize) {
    let _ = (hit, len);
}

/// A slot hit compares the cached index. A miss copies the solver name once, then records the slot.
fn resolve_slot(
    len: usize,
    state: &mut ClothGlueState,
    slot_matches: impl Fn(&str, usize) -> bool,
    find: impl FnOnce(&str) -> Option<usize>,
) -> Option<usize> {
    if let Some(index) = state.solver_slot
        && slot_matches(state.solver.as_str(), index)
    {
        charge_solver_probe(state, true, len);
        return Some(index);
    }
    let name = state.solver.clone();
    charge_solver_probe(state, false, len);
    let index = find(name.as_str())?;
    state.solver_slot = Some(index);
    Some(index)
}

fn cloth_solver<'a>(set: &'a ClothSet, state: &mut ClothGlueState) -> Option<&'a ClothSolver> {
    let index = resolve_slot(
        set.len(),
        state,
        |name, index| set.slot_name(index) == Some(name),
        |name| set.slot_of(name),
    )?;
    set.get_slot(index)
}

fn cloth_solver_mut<'a>(set: &'a mut ClothSet, state: &mut ClothGlueState) -> Option<&'a mut ClothSolver> {
    let index = resolve_slot(
        set.len(),
        state,
        |name, index| set.slot_name(index) == Some(name),
        |name| set.slot_of(name),
    )?;
    set.get_slot_mut(index)
}

fn soft_solver<'a>(set: &'a SoftSet, state: &mut ClothGlueState) -> Option<&'a SoftSolver> {
    let index = resolve_slot(
        set.len(),
        state,
        |name, index| set.slot_name(index) == Some(name),
        |name| set.slot_of(name),
    )?;
    set.get_slot(index)
}

fn soft_solver_mut<'a>(set: &'a mut SoftSet, state: &mut ClothGlueState) -> Option<&'a mut SoftSolver> {
    let index = resolve_slot(
        set.len(),
        state,
        |name, index| set.slot_name(index) == Some(name),
        |name| set.slot_of(name),
    )?;
    set.get_slot_mut(index)
}
/// Default distance LOD thresholds `[d1, d2, d_off]` (meters).
pub const LOD_DISTANCES: [f32; 3] = [15.0, 30.0, 60.0];

/// A cloth instance (spec 55 § 2.1): the cooked asset, the skinned mesh it follows, parameter
/// overrides, the collision layer, the solver name and the render section.
#[derive(Component, Clone, Debug)]
pub struct ClothSim {
    /// The cooked asset. Replacing the `Arc` (hot reload, a map edit) rebuilds the solver.
    pub asset: Arc<ClothAsset>,
    /// The [`SkinnedMesh`] entity the cloth is skinned to (`None`: the rest mesh follows this
    /// entity's transform).
    pub skinned_mesh: Option<Entity>,
    /// Parameter overrides (`None`: the asset's LOD parameters).
    pub params: Option<ClothParams>,
    /// Collision layer name for world snapshots (`config/physics.ron`; default `"default"`).
    pub layer: String,
    /// Solver name in `ClothSet`; empty gives `cloth:<entity bits>`.
    pub solver: String,
    /// Blend between pose (0) and simulation (1).
    pub blend: f32,
    /// Multiplies the `pose_pull` map at runtime (spec 55 § 2.2): 0 lets the cloth go, 1 is as
    /// authored.
    pub pull_scale: f32,
    /// Distance LOD thresholds `[d1, d2, d_off]` in meters (ascending).
    pub lod_distances: [f32; 3],
    /// World-snapshot margin around the particle bounds (meters).
    pub snapshot_margin: f32,
    /// Two-way reaction: contact impulses push dynamic bodies (off by default).
    pub reaction: bool,
    /// Per-body cap of reaction impulses per fixed step (N·s).
    pub max_reaction_impulse: f32,
    /// The entity whose `MeshRenderer` shows the section (`None`: nothing is drawn).
    pub section: Option<Entity>,
}

impl ClothSim {
    /// An instance of `asset` with the spec defaults: no skinned owner, layer `"default"`,
    /// blend 1, LOD at 15 / 30 / 60 m, reaction off, no section.
    #[must_use]
    pub fn new(asset: Arc<ClothAsset>) -> Self {
        Self {
            asset,
            skinned_mesh: None,
            params: None,
            layer: "default".to_string(),
            solver: String::new(),
            blend: 1.0,
            pull_scale: 1.0,
            lod_distances: LOD_DISTANCES,
            snapshot_margin: SNAPSHOT_MARGIN,
            reaction: false,
            max_reaction_impulse: MAX_REACTION_IMPULSE,
            section: None,
        }
    }
}

/// A soft-body instance (spec 55 § 2.8): the cooked asset, solver name and render section.
#[derive(Component, Clone, Debug)]
pub struct SoftSim {
    /// The cooked asset. Replacing the `Arc` rebuilds the solver.
    pub asset: Arc<SoftAsset>,
    /// Solver name in `SoftSet`; empty gives `soft:<entity bits>`.
    pub solver: String,
    /// Collision layer name for world snapshots.
    pub layer: String,
    /// World-snapshot margin (meters).
    pub snapshot_margin: f32,
    /// The entity whose `MeshRenderer` shows the surface.
    pub section: Option<Entity>,
    /// The `SkinnedMesh` entity a posed soft body (flesh) is skinned to. `None`: its rest shape
    /// follows this entity's transform. Used only when the asset is posed
    /// ([`SoftAsset::is_posed`]).
    pub skinned_mesh: Option<Entity>,
    /// Distance LOD thresholds `[d1, d2, d_off]` in meters (ascending).
    pub lod_distances: [f32; 3],
}

impl SoftSim {
    /// An instance of `asset` on layer `"default"` with no section.
    #[must_use]
    pub fn new(asset: Arc<SoftAsset>) -> Self {
        Self {
            asset,
            solver: String::new(),
            layer: "default".to_string(),
            snapshot_margin: SNAPSHOT_MARGIN,
            section: None,
            skinned_mesh: None,
            lod_distances: LOD_DISTANCES,
        }
    }
}

/// A saved cloth instance: what a `.level.ron` / `.prefab.ron` stores (a [`ClothSim`] holds
/// the loaded `Arc` and is never saved, like `MeshSource` and `MeshRenderer`).
/// [`resolve_cloth_sources`] loads the cooked asset through the `AssetServer` and inserts the
/// [`ClothSim`]: the skinned owner is the nearest [`SkinnedMesh`] on the entity or its
/// ancestors, the section is this entity (give it a `MeshSource` or a `MeshRenderer` to draw).
/// A hot reload of the asset rebuilds the solver.
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct ClothSimSource {
    /// The `.cloth.ron` path, project-relative.
    #[reflect(@regia_ecs::attrs::AssetFilter("*.cloth.ron"))]
    pub asset: String,
    /// Collision layer name for world snapshots.
    pub layer: String,
    /// Solver name in `ClothSet`; empty gives `cloth:<entity bits>`.
    pub solver: String,
    /// Blend between pose (0) and simulation (1).
    pub blend: f32,
    /// Multiplies the `pose_pull` map at runtime.
    pub pull_scale: f32,
    /// Distance LOD thresholds `[d1, d2, d_off]` in meters (ascending).
    pub lod_distances: [f32; 3],
    /// Two-way reaction: contact impulses push dynamic bodies.
    pub reaction: bool,
}

impl Default for ClothSimSource {
    fn default() -> Self {
        Self {
            asset: String::new(),
            layer: "default".to_string(),
            solver: String::new(),
            blend: 1.0,
            pull_scale: 1.0,
            lod_distances: LOD_DISTANCES,
            reaction: false,
        }
    }
}

/// A saved soft-body instance (see [`ClothSimSource`]).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct SoftSimSource {
    /// The `.soft.ron` path, project-relative.
    #[reflect(@regia_ecs::attrs::AssetFilter("*.soft.ron"))]
    pub asset: String,
    /// Collision layer name for world snapshots.
    pub layer: String,
    /// Solver name in `SoftSet`; empty gives `soft:<entity bits>`.
    pub solver: String,
    /// Distance LOD thresholds `[d1, d2, d_off]` in meters (ascending).
    pub lod_distances: [f32; 3],
}

impl Default for SoftSimSource {
    fn default() -> Self {
        Self {
            asset: String::new(),
            layer: "default".to_string(),
            solver: String::new(),
            lod_distances: LOD_DISTANCES,
        }
    }
}

/// The cooked asset a [`ClothSimSource`] or [`SoftSimSource`] resolved to (or failed on).
#[derive(Component, Clone, Debug, PartialEq, Eq)]
pub struct ClothSourceState {
    /// The path this state belongs to (a changed path resolves again).
    pub asset: String,
    /// The cooked id (`None` when the path has no manifest entry).
    pub id: Option<AssetId>,
    /// The asset's hot-reload generation last read.
    pub generation: u64,
    /// The stable code of the last load error.
    pub error: Option<String>,
}

/// Finds the cooked id of a project-relative path (with or without `assets/`).
fn cooked_id(server: &AssetServer, path: &str) -> Option<AssetId> {
    let rel = path.trim_start_matches("./");
    server
        .id_for_path(rel)
        .or_else(|| server.id_for_path(&format!("assets/{rel}")))
}

fn read_cooked_file(server: &mut AssetServer, id: AssetId) -> Result<Vec<u8>, ClothError> {
    let (bytes, _) = server.read_cooked(id).map_err(|e| ClothError::AssetInvalid {
        reason: e.to_string(),
    })?;
    Ok(bytes)
}

/// Loads the cooked cloth of a project-relative `.cloth.ron` path from the mounted paks or the
/// cooked cache. It is marked as decoded here, so its hot reloads bump
/// `AssetServer::generation`.
///
/// # Errors
/// [`ClothError::AssetInvalid`] when the path has no cooked entry or the file does not read;
/// else as [`ClothAsset::from_cooked_file`].
pub fn load_cloth_asset(server: &mut AssetServer, path: &str) -> Result<Arc<ClothAsset>, ClothError> {
    let _span = tracing::info_span!("cloth_asset_load").entered();
    let id = cooked_id(server, path).ok_or_else(|| ClothError::AssetInvalid {
        reason: format!("no cooked asset for {path}"),
    })?;
    server.track_external(id);
    Ok(Arc::new(ClothAsset::from_cooked_file(&read_cooked_file(
        server, id,
    )?)?))
}

/// Loads the cooked soft body of a project-relative `.soft.ron` path (see [`load_cloth_asset`]).
///
/// # Errors
/// As [`load_cloth_asset`], with [`SoftAsset::from_cooked_file`]'s errors.
pub fn load_soft_asset(server: &mut AssetServer, path: &str) -> Result<Arc<SoftAsset>, ClothError> {
    let _span = tracing::info_span!("cloth_asset_load").entered();
    let id = cooked_id(server, path).ok_or_else(|| ClothError::AssetInvalid {
        reason: format!("no cooked asset for {path}"),
    })?;
    server.track_external(id);
    Ok(Arc::new(SoftAsset::from_cooked_file(&read_cooked_file(
        server, id,
    )?)?))
}

/// The nearest entity from `entity` up its parents that has a [`SkinnedMesh`].
fn skinned_owner(
    entity: Entity,
    parents: &Query<&ChildOf>,
    owners: &Query<(), With<SkinnedMesh>>,
) -> Option<Entity> {
    let mut at = Some(entity);
    while let Some(e) = at {
        if owners.contains(e) {
            return Some(e);
        }
        at = parents.get(e).ok().map(ChildOf::parent);
    }
    None
}

/// What one saved source asks of the server this frame.
enum SourceLoad {
    /// Nothing to do.
    Current,
    /// Load `(id, generation)`.
    Load(Option<AssetId>, u64),
}

fn source_load(server: &AssetServer, path: &str, state: Option<&ClothSourceState>) -> SourceLoad {
    match state {
        Some(state) if state.asset == path => match state.id {
            Some(id) if server.generation(id) != state.generation => {
                SourceLoad::Load(Some(id), server.generation(id))
            }
            _ => SourceLoad::Current,
        },
        _ => {
            let id = cooked_id(server, path);
            SourceLoad::Load(id, id.map_or(0, |id| server.generation(id)))
        }
    }
}

/// Resolves every [`ClothSimSource`] and [`SoftSimSource`] into its [`ClothSim`] / [`SoftSim`]
/// and follows hot reloads (a new `Arc` rebuilds the solver and enters `Settling`). A failed
/// load is recorded in [`ClothSourceState`], logged once and retried only on a hot reload or a
/// changed path. Without an `AssetServer` nothing happens.
pub fn resolve_cloth_sources(
    mut commands: Commands,
    server: Option<ResMut<AssetServer>>,
    cloths: Query<(
        Entity,
        &ClothSimSource,
        Option<&ClothSourceState>,
        Option<&ClothSim>,
    )>,
    softs: Query<SoftSourceInstance, Without<ClothSimSource>>,
    parents: Query<&ChildOf>,
    owners: Query<(), With<SkinnedMesh>>,
) {
    let Some(mut server) = server else {
        return;
    };
    for (entity, source, state, existing) in &cloths {
        let SourceLoad::Load(id, generation) = source_load(&server, &source.asset, state) else {
            continue;
        };
        let loaded = load_cloth_asset(&mut server, &source.asset);
        let error = loaded.as_ref().err().map(|e| e.code().to_string());
        if let Err(err) = &loaded {
            warn!(target: "regia_framework", "cloth {}: not loaded: {err}", source.asset);
        }
        commands.entity(entity).insert(ClothSourceState {
            asset: source.asset.clone(),
            id,
            generation,
            error,
        });
        let Ok(asset) = loaded else {
            continue;
        };
        let mut sim = existing.cloned().unwrap_or_else(|| {
            let mut sim = ClothSim::new(Arc::clone(&asset));
            sim.skinned_mesh = skinned_owner(entity, &parents, &owners);
            sim.section = Some(entity);
            sim.layer.clone_from(&source.layer);
            sim.solver.clone_from(&source.solver);
            sim.blend = source.blend;
            sim.pull_scale = source.pull_scale;
            sim.lod_distances = source.lod_distances;
            sim.reaction = source.reaction;
            sim
        });
        sim.asset = asset;
        commands.entity(entity).insert(sim);
    }
    for (entity, source, state, existing) in &softs {
        let SourceLoad::Load(id, generation) = source_load(&server, &source.asset, state) else {
            continue;
        };
        let loaded = load_soft_asset(&mut server, &source.asset);
        let error = loaded.as_ref().err().map(|e| e.code().to_string());
        if let Err(err) = &loaded {
            warn!(target: "regia_framework", "soft body {}: not loaded: {err}", source.asset);
        }
        commands.entity(entity).insert(ClothSourceState {
            asset: source.asset.clone(),
            id,
            generation,
            error,
        });
        let Ok(asset) = loaded else {
            continue;
        };
        let mut sim = existing.cloned().unwrap_or_else(|| {
            let mut sim = SoftSim::new(Arc::clone(&asset));
            sim.skinned_mesh = skinned_owner(entity, &parents, &owners);
            sim.section = Some(entity);
            sim.layer.clone_from(&source.layer);
            sim.solver.clone_from(&source.solver);
            sim.lod_distances = source.lod_distances;
            sim
        });
        sim.asset = asset;
        commands.entity(entity).insert(sim);
    }
}

/// The shape of a [`ClothBodyProxy`], in the joint entity's frame.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Default, Clone)]
pub enum ProxyShape {
    /// A sphere at `center`.
    Sphere {
        /// Center in the joint frame.
        center: Vec3,
        /// Radius in meters.
        radius: f32,
    },
    /// A capsule between `a` and `b`.
    Capsule {
        /// First end in the joint frame.
        a: Vec3,
        /// Second end in the joint frame.
        b: Vec3,
        /// Radius in meters.
        radius: f32,
    },
}

impl Default for ProxyShape {
    fn default() -> Self {
        Self::Sphere {
            center: Vec3::ZERO,
            radius: 0.1,
        }
    }
}

/// A sphere or capsule collider that follows a joint entity (spec 55 § 2.4). A cloth collides
/// with the proxies its asset names, or every proxy when the asset names none.
#[derive(Component, Reflect, Clone, Debug, Default, PartialEq)]
#[reflect(Component, Default, Clone)]
pub struct ClothBodyProxy {
    /// Name matched against the asset's proxy list.
    pub name: String,
    /// Shape in the joint frame.
    pub shape: ProxyShape,
    /// Contact friction.
    pub friction: f32,
}

/// The output of a cloth or soft body (written by [`collect_cloth_output`]): blended positions
/// and normals per particle at the previous and the current fixed step, for interpolation.
#[derive(Component, Clone, Debug, Default)]
pub struct ClothOutput {
    /// Output at the previous fixed step.
    pub prev: Vec<PoseTarget>,
    /// Output at the current fixed step.
    pub cur: Vec<PoseTarget>,
    /// Solver steps at `cur`.
    pub steps: u64,
    /// The solver state at `cur`.
    pub state: Option<ClothState>,
    /// Render triangles dropped by tearing in the last publish.
    pub hidden_triangles: usize,
}

/// A cloth or soft instance changed lifecycle state (spec 55 § 3.2).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClothStateChanged {
    /// The instance.
    pub entity: Entity,
    /// The state it left.
    pub from: ClothState,
    /// The state it entered.
    pub to: ClothState,
}

/// A cloth instance snapped to its pose: its root jumped past `teleport_distance`, or it was
/// reset (spec 55 § 2.2).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClothTeleported {
    /// The instance.
    pub entity: Entity,
}

/// A distance link of a cloth instance was cut by tearing (spec 55 § 2.5).
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClothTear {
    /// The instance.
    pub entity: Entity,
    /// Index of the distance constraint.
    pub constraint: usize,
    /// The solver's fixed-step count when it was cut.
    pub step: u64,
}

/// Per-instance glue state (internal; never saved).
#[derive(Component, Clone, Debug, Default)]
pub struct ClothGlueState {
    /// Address of the asset the solver was built from.
    asset: usize,
    /// Solver name in use.
    solver: String,
    /// Rest normals of the sim mesh (LOD 0).
    rest_normals: Vec<Vec3>,
    /// Influences of the sim mesh (LOD 0).
    influences: Vec<SkinInfluence>,
    /// Pose targets of the last fixed step.
    last_targets: Vec<PoseTarget>,
    /// Root pose of the last fixed step.
    last_root: Option<Isometry>,
    /// Proxy entities in collider order and their last world shape.
    proxies: Vec<(Entity, Option<ClothShape>)>,
    /// How many proxies the world had at the last scan.
    proxies_seen: Option<usize>,
    /// Snapshot key → rigid body entity, for reaction.
    bodies: BTreeMap<SpawnKey, Entity>,
    /// Index of `solver` in `ClothSet` or `SoftSet` after the last lookup that found it.
    /// A removal shifts slots; a stale index fails the name check and rescans once.
    solver_slot: Option<usize>,
    /// Solver lookups this instance charged since the counter was zeroed (tests; `1` on a
    /// slot hit, the set length on a scan).
    #[cfg(test)]
    solver_probes: u64,
    /// Reaction sums processed for this instance since the counter was zeroed (tests). An
    /// empty reaction wrench does not increment it.
    #[cfg(test)]
    reaction_runs: u64,
    /// The two render-section meshes that alternate under one cache id. The extracted frame
    /// can still hold the front one while the next publish edits the back one in place.
    render_meshes: [Option<Arc<CpuMesh>>; 2],
    /// Slot last published. The other slot is the one extract is not holding.
    render_front: u8,
    /// Bounds and collider stamps of the last snapshot. An unchanged step reuses it.
    snapshot_reuse: Option<SnapshotReuse>,
    /// Current distance LOD level.
    lod: u8,
    /// The asset LOD (sim mesh) the solver runs.
    sim_lod: usize,
    /// The `ClothSim.blend` last pushed to the solver.
    applied_blend: Option<f32>,
    /// The `ClothSim.pull_scale` last pushed to the solver.
    applied_pull_scale: Option<f32>,
    /// The glue paused the solver because it is hidden (a pause by anyone else is not undone).
    hidden_pause: bool,
    /// The root has a negative scale: triangle normals and winding are reversed in the world.
    mirrored: bool,
    /// The solver state last reported by a [`ClothStateChanged`].
    reported_state: Option<ClothState>,
    /// The solver's teleport count last reported by a [`ClothTeleported`].
    reported_teleports: u64,
    /// The pose solver error of the last sync (`None` when fine).
    pub last_error: Option<&'static str>,
    /// The scene wind this cloth received at the last fixed step, after its `wind_scale`
    /// ([`feed_scene_wind_to_cloth`]; zero without a `SceneWind`).
    scene_wind: Vec3,
}

impl ClothGlueState {
    /// The solver name in `ClothSet` (cloth) or `SoftSet` (soft body).
    #[must_use]
    pub fn solver(&self) -> &str {
        &self.solver
    }

    /// The distance LOD level the glue chose last.
    #[must_use]
    pub fn lod(&self) -> u8 {
        self.lod
    }

    /// The scene wind (m/s) this cloth received at the last fixed step, already scaled by its
    /// `wind_scale` (zero without a `SceneWind`, in `SceneWindMode::Off`, or for a soft body).
    #[must_use]
    pub fn scene_wind(&self) -> Vec3 {
        self.scene_wind
    }
}

/// Wires cooked cloth and soft assets into the solvers and the renderer (see the module docs).
#[derive(Default)]
pub struct ClothGluePlugin;

impl Plugin for ClothGluePlugin {
    fn build(&self, app: &mut App) {
        if !app.world().contains_resource::<ClothSet>() {
            app.add_plugins(regia_physics::cloth::ClothPlugin);
        }
        // Order cloth, soft, proxy and camera work by spawn arrival, never by entity id or a
        // session GUID (ADR-1347, framework3 determinism audit): parallel `Commands` reserve ids
        // in thread-timing order, and a runtime-spawned prefab draws a random version-4 UUID.
        // Cloth and soft bodies share ONE arrival counter so a mixed budget tie resolves by
        // true spawn arrival (framework3 review high 1); a failure (component already in use)
        // logs and falls back to entity ids, as in `regia_physics`.
        regia_ecs::spawn_order::track_shared_spawn_order::<ClothSim>(app.world_mut());
        regia_ecs::spawn_order::track_shared_spawn_order::<SoftSim>(app.world_mut());
        regia_ecs::spawn_order::track_spawn_order::<ClothBodyProxy>(app.world_mut());
        regia_ecs::spawn_order::track_spawn_order::<Camera>(app.world_mut());
        register_cloth_types(app);
        app.init_resource::<ClothBudget>()
            .add_message::<ClothStateChanged>()
            .add_message::<ClothTeleported>()
            .add_message::<ClothTear>()
            .add_systems(Update, resolve_cloth_sources)
            .add_systems(
                FixedUpdate,
                (
                    spawn_cloth_solvers,
                    switch_cloth_sim_lods,
                    sync_cloth_colliders,
                    build_cloth_snapshots,
                    sync_cloth_pose,
                    sync_cloth_visibility,
                    feed_scene_wind_to_cloth,
                )
                    .chain()
                    .after(regia_physics::PhysicsStep)
                    .before(regia_physics::cloth::gather_cloth_wind),
            )
            .add_systems(
                FixedUpdate,
                (collect_cloth_output, emit_cloth_messages, apply_cloth_reaction)
                    .chain()
                    .after(regia_physics::cloth::soft::step_soft_set)
                    .after(regia_physics::cloth::step_cloth_set),
            )
            .add_systems(
                PostUpdate,
                publish_cloth_mesh.after(PostUpdateSet::TransformPropagate),
            );
    }
}

/// Registers the components levels and prefabs store ([`ClothSimSource`], [`SoftSimSource`],
/// [`ClothBodyProxy`]) with the app's type registry. [`ClothGluePlugin`] calls it; hosts that
/// load those files without running the glue call it directly (the editor registers the same
/// types with its level types).
pub fn register_cloth_types(app: &mut App) {
    app.register_type::<ClothSimSource>()
        .register_type::<SoftSimSource>()
        .register_type::<ClothBodyProxy>()
        .register_type::<ProxyShape>();
}

/// `ClothBudget { max_particles_per_step }` (spec 55 § 2.9): 8,192 on every target. The budget
/// shapes the authoritative fixed step, which must step identically on desktop and Android
/// (ADR-4764); `lod::BUDGET_ANDROID` stays as the smaller cap a host may set explicitly for a
/// session that is not part of a cross-platform replay.
#[derive(Resource, Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClothBudget {
    /// Most LOD-weighted particles stepped per fixed step.
    pub max_particles_per_step: usize,
}

impl Default for ClothBudget {
    fn default() -> Self {
        Self {
            max_particles_per_step: lod::BUDGET_DESKTOP,
        }
    }
}

/// The wind a cloth feels from `scene` before its `wind_scale`: the gusting velocity
/// (`SceneWind::velocity3`), or `None` in `SceneWindMode::Off` or for a non-finite value.
#[must_use]
pub fn cloth_scene_wind(scene: &SceneWind) -> Option<Vec3> {
    if scene.mode == SceneWindMode::Off {
        return None;
    }
    Some(Vec3::from_array(scene.velocity3())).filter(|v| v.is_finite())
}

/// The scene-wind-to-cloth bridge (ADR-2884; a spec 99 § 3.3 consumer). Every fixed step, after
/// the glue and before `regia_physics::cloth::gather_cloth_wind`, each [`ClothSim`]'s solver
/// gets the global wind term `ClothSet::global_wind() + w · wind_scale`, with `w` from
/// [`cloth_scene_wind`] (gusts included) and `wind_scale` from the instance's parameter
/// override, else the asset's LOD 0 parameters. Without a [`SceneWind`], or in
/// `SceneWindMode::Off`, or with a zero product, the term is the set's value alone, bit for bit
/// what it was before this bridge. Solvers no [`ClothSim`] owns (a host's own, an agent's) and
/// soft bodies are left alone. [`ClothGlueState::scene_wind`] records what each cloth received.
pub fn feed_scene_wind_to_cloth(
    scene: Option<Res<SceneWind>>,
    set: Option<ResMut<ClothSet>>,
    mut sims: Query<(&ClothSim, &mut ClothGlueState)>,
) {
    let Some(mut set) = set else {
        return;
    };
    if set.is_empty() {
        return;
    }
    let _span = debug_span!("cloth_scene_wind").entered();
    let host = set.global_wind();
    let wind = scene.as_deref().and_then(cloth_scene_wind);
    for (sim, mut state) in &mut sims {
        let scale = sim
            .params
            .as_ref()
            .map_or(sim.asset.lod0().params.wind_scale, |p| p.wind_scale);
        let received = match wind {
            Some(w) if scale > 0.0 && scale.is_finite() => w * scale,
            _ => Vec3::ZERO,
        };
        let Some(solver) = set.get_mut(&state.solver) else {
            continue;
        };
        // `host + 0` would turn a `-0.0` host component into `+0.0`: skip the add for no wind.
        let total = if received == Vec3::ZERO {
            host
        } else {
            host + received
        };
        if solver.global_wind().to_array().map(f32::to_bits) != total.to_array().map(f32::to_bits) {
            // Finite by construction (finite host, finite clamped inputs); a failure keeps the old term.
            let _ = solver.set_global_wind(total);
        }
        if state.scene_wind != received {
            state.scene_wind = received;
        }
    }
}

/// Sort key of one entity for cloth ordering: the order `C` arrived in the world
/// ([`SpawnOrder`], ADR-1347). Never the entity id (parallel `Commands` reserve ids in
/// thread-timing order, and bevy stores the index inverted in `to_bits`) and never an
/// `EntityGuid` (a runtime-spawned prefab draws a random version-4 UUID per session). Falls back
/// to the entity id when `C`'s spawn order is not tracked.
fn spawn_key<C: Component>(order: Option<&SpawnOrder<C>>, entity: Entity) -> SpawnKey {
    order.map_or((u64::MAX, entity.index().index(), entity.to_bits()), |o| {
        o.key(entity)
    })
}

/// Sort key of one entity against a [`SharedSpawnOrder`]: the shared arrival number first, the
/// entity id as the fallback (same shape as [`spawn_key`]).
fn shared_spawn_key(order: Option<&SharedSpawnOrder>, entity: Entity) -> SpawnKey {
    order.map_or((u64::MAX, entity.index().index(), entity.to_bits()), |o| {
        o.key(entity)
    })
}

fn solver_name(configured: &str, prefix: &str, entity: Entity) -> String {
    if configured.is_empty() {
        format!("{prefix}:{}", entity.to_bits())
    } else {
        configured.to_string()
    }
}

fn fixed_step(fixed: Option<&FixedTime>) -> f32 {
    fixed.map_or(1.0 / 60.0, |f| f.step().as_secs_f32())
}

fn world_of(transform: Option<&GlobalTransform>) -> Affine3A {
    transform.map_or(Affine3A::IDENTITY, |t| t.0)
}

fn iso_of(world: Affine3A) -> Isometry {
    let (_, rotation, translation) = world.to_scale_rotation_translation();
    Isometry::new(rotation, translation)
}

/// What the solver builder reads from a cloth instance.
type ClothInstance = (
    Entity,
    &'static ClothSim,
    Option<&'static GlobalTransform>,
    Option<&'static mut ClothGlueState>,
);

/// What the solver builder reads from a soft instance.
type SoftInstance = (
    Entity,
    &'static SoftSim,
    Option<&'static GlobalTransform>,
    Option<&'static mut ClothGlueState>,
);

/// The proxy queries `sync_cloth_colliders` scans, with the proxy arrival order.
type ProxyQueries<'w, 's> = (
    Query<'w, 's, ProxyInstance>,
    Query<'w, 's, &'static SkinnedMesh>,
    Query<'w, 's, &'static ChildOf>,
    Option<Res<'w, SpawnOrder<ClothBodyProxy>>>,
);

/// Arrival orders `sync_cloth_visibility` breaks ties by ([`SpawnOrder`], ADR-1347): cameras
/// break the LOD eye tie; cloth AND soft instances share ONE counter ([`SharedSpawnOrder`]) for
/// the budget tie, so a mixed cloth-vs-soft tie resolves by true spawn arrival — two independent
/// per-type counters would both start at 0 and fall through to the entity id, which parallel
/// `Commands` reserve in thread-timing order.
type ClothSpawnOrders<'w> = (
    Option<Res<'w, SpawnOrder<Camera>>>,
    Option<Res<'w, SharedSpawnOrder>>,
);

/// One cloth's part of the proxy scan: which proxies it takes and how many the world had.
struct ProxyScan<'a> {
    /// Names the asset names (`empty` takes every proxy of the body).
    names: &'a [String],
    /// The [`SkinnedMesh`] entity the cloth is skinned to (`None`: take from the whole world).
    owner: Option<Entity>,
    /// How many proxies the world had at this scan.
    seen: usize,
}

/// The rigid-world queries the snapshot builder reads, with the collider arrival order.
type SnapshotQueries<'w, 's> = (
    Query<'w, 's, SnapshotSource>,
    Query<'w, 's, SnapshotBody, With<RigidBody>>,
    Query<'w, 's, &'static ChildOf>,
    Option<Res<'w, SpawnOrder<Collider>>>,
);

/// What the visibility and LOD system reads from a cloth instance.
type LodInstance = (
    Entity,
    &'static ClothSim,
    &'static mut ClothGlueState,
    Option<&'static InheritedVisibility>,
);

/// What the LOD pass reads from a soft body.
type SoftLodInstance = (
    Entity,
    &'static SoftSim,
    &'static mut ClothGlueState,
    Option<&'static InheritedVisibility>,
);

/// What the collider sync reads from a body proxy.
type ProxyInstance = (Entity, &'static ClothBodyProxy, &'static GlobalTransform);

/// What the pose sync reads from a soft body.
type SoftPoseInstance = (
    &'static SoftSim,
    Option<&'static GlobalTransform>,
    &'static mut ClothGlueState,
);

/// Reused buffers of `sync_cloth_pose`.
type PoseScratch = (Vec<(Vec3, Vec3)>, Vec<PoseTarget>, Vec<SkinInfluence>);

/// What `resolve_cloth_sources` reads from a saved soft body.
type SoftSourceInstance = (
    Entity,
    &'static SoftSimSource,
    Option<&'static ClothSourceState>,
    Option<&'static SoftSim>,
);

/// What the publisher reads from an instance.
type PublishInstance = (
    Option<&'static ClothSim>,
    Option<&'static SoftSim>,
    &'static mut ClothGlueState,
    &'static mut ClothOutput,
);

/// A solver of the instance's asset at LOD `lod`, with its parameter overrides.
fn build_instance_solver(
    sim: &ClothSim,
    lod: usize,
    dt: f32,
    world: Affine3A,
) -> Result<ClothSolver, ClothError> {
    match sim.params {
        Some(params) => sim.asset.build_solver_with(lod, dt, world, params),
        None => sim.asset.build_solver(lod, dt, world),
    }
}

/// The rest normals and skin influences a sim mesh is skinned with.
fn sim_lod_rest(lod: &ClothLod) -> (Vec<Vec3>, Vec<SkinInfluence>) {
    let mut rest_normals = Vec::new();
    vertex_normals(&lod.rest_positions, &lod.triangles, &mut rest_normals);
    (rest_normals, skin_influences(&lod.skin))
}

/// Cooked influences as the skinning code takes them.
fn skin_influences(skin: &[CookedInfluence]) -> Vec<SkinInfluence> {
    skin.iter()
        .map(|s| SkinInfluence {
            joints: s.joints.map(u16::from),
            weights: s.weights.map(|w| f32::from(w) / 255.0),
        })
        .collect()
}

/// The asset LOD a distance LOD level runs on (spec 55 § 2.9): the authored mesh, and at level 2
/// and beyond the asset's first LOD entry when it has one.
fn sim_lod_for(level: u8, lods: usize) -> usize {
    if level >= 2 && lods > 1 { 1 } else { 0 }
}

/// Builds (or rebuilds) the solver of every [`ClothSim`] and [`SoftSim`], and removes the
/// solvers of instances that are gone.
pub fn spawn_cloth_solvers(
    mut commands: Commands,
    fixed: Option<Res<FixedTime>>,
    mut cloths: ResMut<ClothSet>,
    mut softs: ResMut<SoftSet>,
    mut cloth_q: Query<ClothInstance>,
    mut soft_q: Query<SoftInstance, Without<ClothSim>>,
    mut owned: Local<BTreeMap<Entity, (bool, String)>>,
) {
    let dt = fixed_step(fixed.as_deref());
    // Instances that lost their component (or were despawned) give their solver back.
    owned.retain(|entity, (soft, name)| {
        let alive = if *soft {
            soft_q.contains(*entity)
        } else {
            cloth_q.contains(*entity)
        };
        if !alive {
            if *soft {
                softs.remove(name);
            } else {
                cloths.remove(name);
            }
        }
        alive
    });
    for (entity, sim, transform, mut state) in &mut cloth_q {
        let address = Arc::as_ptr(&sim.asset) as usize;
        let name = solver_name(&sim.solver, "cloth", entity);
        if let Some(s) = state.as_mut() {
            // Copy the failure flag before the solver lookup borrows the glue state.
            let failed = s.last_error.is_some();
            let same = s.asset == address && s.solver == name;
            if same && (cloth_solver(&cloths, s).is_some() || failed) {
                continue;
            }
        }
        let _span = debug_span!(
            "cloth_build_solver",
            particles = sim.asset.lod0().rest_positions.len()
        )
        .entered();
        let built = build_instance_solver(sim, 0, dt, world_of(transform)).and_then(|mut solver| {
            solver.set_blend(sim.blend.clamp(0.0, 1.0))?;
            if sim.reaction {
                solver.set_reaction(Some(sim.max_reaction_impulse))?;
            }
            Ok(solver)
        });
        let solver = match built {
            Ok(solver) => solver,
            Err(err) => {
                // Recorded so the same asset is not retried (and logged) every fixed step.
                warn!(target: "regia_framework", "cloth {name}: solver not built: {err}");
                let failed = ClothGlueState {
                    asset: address,
                    solver: name,
                    last_error: Some(err.code()),
                    ..ClothGlueState::default()
                };
                match state {
                    Some(mut state) => *state = failed,
                    None => {
                        commands.entity(entity).insert((failed, ClothOutput::default()));
                    }
                }
                continue;
            }
        };
        if let Some((_, old)) = owned.insert(entity, (false, name.clone()))
            && old != name
        {
            cloths.remove(&old);
        }
        cloths.insert(name.clone(), solver);
        let solver_slot = cloths.slot_of(&name);
        let (rest_normals, influences) = sim_lod_rest(sim.asset.lod0());
        let glue = ClothGlueState {
            asset: address,
            solver: name,
            rest_normals,
            influences,
            solver_slot,
            ..ClothGlueState::default()
        };
        match state {
            Some(mut state) => {
                // A rebuild (hot reload) reports its `Settling` to whoever watched the old solver.
                let reported_state = state.reported_state;
                *state = ClothGlueState {
                    reported_state,
                    ..glue
                };
            }
            None => {
                commands.entity(entity).insert((glue, ClothOutput::default()));
            }
        }
    }
    for (entity, sim, transform, mut state) in &mut soft_q {
        let address = Arc::as_ptr(&sim.asset) as usize;
        let name = solver_name(&sim.solver, "soft", entity);
        if let Some(s) = state.as_mut() {
            let failed = s.last_error.is_some();
            let same = s.asset == address && s.solver == name;
            if same && (soft_solver(&softs, s).is_some() || failed) {
                continue;
            }
        }
        let solver = match sim.asset.build_solver(dt, world_of(transform)) {
            Ok(solver) => solver,
            Err(err) => {
                warn!(target: "regia_framework", "soft body {name}: solver not built: {err}");
                let failed = ClothGlueState {
                    asset: address,
                    solver: name,
                    last_error: Some(err.code()),
                    ..ClothGlueState::default()
                };
                match state {
                    Some(mut state) => *state = failed,
                    None => {
                        commands.entity(entity).insert((failed, ClothOutput::default()));
                    }
                }
                continue;
            }
        };
        if let Some((_, old)) = owned.insert(entity, (true, name.clone()))
            && old != name
        {
            softs.remove(&old);
        }
        softs.insert(name.clone(), solver);
        let solver_slot = softs.slot_of(&name);
        let glue = ClothGlueState {
            asset: address,
            solver: name,
            solver_slot,
            // Soft pose targets only carry positions (`reach`, `pose_pull`).
            rest_normals: vec![Vec3::ZERO; sim.asset.rest_positions.len()],
            influences: skin_influences(&sim.asset.skin),
            mirrored: is_mirrored(world_of(transform)),
            ..ClothGlueState::default()
        };
        match state {
            Some(mut state) => {
                let reported_state = state.reported_state;
                *state = ClothGlueState {
                    reported_state,
                    ..glue
                };
            }
            None => {
                commands.entity(entity).insert((glue, ClothOutput::default()));
            }
        }
    }
}

fn proxy_shape(proxy: &ClothBodyProxy, world: Affine3A) -> ClothShape {
    match proxy.shape {
        ProxyShape::Sphere { center, radius } => ClothShape::Sphere {
            center: world.transform_point3(center),
            radius,
        },
        ProxyShape::Capsule { a, b, radius } => ClothShape::Capsule {
            a: world.transform_point3(a),
            b: world.transform_point3(b),
            radius,
        },
    }
}

/// A proxy that left the world: a tiny sphere far away, so collider indices stay stable.
const PARKED: ClothShape = ClothShape::Sphere {
    center: Vec3::new(0.0, -1.0e6, 0.0),
    radius: 1e-3,
};

/// Moves each cloth onto the sim mesh its LOD level asks for (spec 55 § 2.9): a solver of that
/// asset LOD takes over the old one's motion by nearest rest vertex, under the same name, so
/// the switch neither settles nor shows as a teleport. Body proxies are added to the new solver
/// by the next step of [`sync_cloth_colliders`]; agent `physics.cloth_set_params` changes stay
/// on the old solver (the LOD's own parameters apply).
pub fn switch_cloth_sim_lods(
    fixed: Option<Res<FixedTime>>,
    mut cloths: ResMut<ClothSet>,
    mut sims: Query<(&ClothSim, Option<&GlobalTransform>, &mut ClothGlueState)>,
) {
    let dt = fixed_step(fixed.as_deref());
    for (sim, transform, mut state) in &mut sims {
        let Some(old) = cloth_solver(&cloths, &mut state) else {
            continue;
        };
        let want = sim_lod_for(old.lod_level(), sim.asset.lods.len());
        if want == state.sim_lod {
            continue;
        }
        let _span = debug_span!("cloth_lod_switch", to = want).entered();
        let map = sim.asset.lod_transfer(state.sim_lod, want);
        let built = build_instance_solver(sim, want, dt, world_of(transform)).and_then(|mut solver| {
            solver.adopt_state(old, &map)?;
            Ok(solver)
        });
        let solver = match built {
            Ok(solver) => solver,
            Err(err) => {
                warn!(target: "regia_framework", "cloth {}: sim LOD {want} not built: {err}", state.solver);
                state.last_error = Some(err.code());
                continue;
            }
        };
        cloths.insert(state.solver.clone(), solver);
        let (rest_normals, influences) = sim_lod_rest(&sim.asset.lods[want]);
        state.rest_normals = rest_normals;
        state.influences = influences;
        state.sim_lod = want;
        state.last_targets.clear();
        // The other sim mesh has its own tuning.
        state.applied_pull_scale = None;
        // The new solver has no colliders yet.
        state.proxies.clear();
        state.proxies_seen = None;
        // Nor a world snapshot: the next snapshot pass must build one. The reuse key describes
        // the old solver's snapshot; with equal particle bounds (a resting cloth whose extreme
        // vertices map exactly) it would hand the new solver none (corr-framework-a F6).
        state.snapshot_reuse = None;
    }
}

/// The top of `entity`'s hierarchy.
fn hierarchy_root(entity: Entity, parents: &Query<&ChildOf>) -> Entity {
    let mut at = entity;
    while let Ok(parent) = parents.get(at) {
        at = parent.parent();
    }
    at
}

/// Updates each cloth's body-proxy colliders (spec 55 § 2.4). Whenever the number of proxies in
/// the world changes, the ones this cloth should use and has not listed yet are added in
/// spawn-arrival order (ADR-1347): those the asset names (all when it names none), on the owner's
/// joints or under the owner's hierarchy root (a skinned cloth never collides with another
/// character's body; an unskinned cloth has no root and takes them from the whole world). Every
/// fixed step then sets the current shape and the previous one (moving colliders). A proxy that
/// leaves the world is parked, so collider indices stay stable.
pub fn sync_cloth_colliders(
    mut cloths: ResMut<ClothSet>,
    mut softs: ResMut<SoftSet>,
    mut sims: Query<(&ClothSim, &mut ClothGlueState)>,
    mut soft_sims: Query<(&SoftSim, &mut ClothGlueState), Without<ClothSim>>,
    proxies: ProxyQueries,
) {
    let (proxy_q, owners, parents, proxy_order) = proxies;
    let seen = proxy_q.iter().len();
    for (sim, mut state) in &mut sims {
        let Some(solver) = cloth_solver_mut(&mut cloths, &mut state) else {
            continue;
        };
        let owner = sim.skinned_mesh;
        let names = sim.asset.proxies.as_slice();
        sync_proxies(
            solver,
            &mut state,
            ProxyScan { names, owner, seen },
            &proxy_q,
            &owners,
            &parents,
            proxy_order.as_deref(),
        );
    }
    for (sim, mut state) in &mut soft_sims {
        let Some(solver) = soft_solver_mut(&mut softs, &mut state) else {
            continue;
        };
        sync_proxies(
            solver.cloth_mut(),
            &mut state,
            ProxyScan {
                names: &[],
                owner: sim.skinned_mesh,
                seen,
            },
            &proxy_q,
            &owners,
            &parents,
            proxy_order.as_deref(),
        );
    }
}

/// One instance's part of [`sync_cloth_colliders`]: `names` empty takes every proxy of the body.
fn sync_proxies(
    solver: &mut ClothSolver,
    state: &mut ClothGlueState,
    scan: ProxyScan<'_>,
    proxies: &Query<ProxyInstance>,
    owners: &Query<&SkinnedMesh>,
    parents: &Query<&ChildOf>,
    proxy_order: Option<&SpawnOrder<ClothBodyProxy>>,
) {
    let _span = debug_span!("cloth_sync_colliders", proxies = state.proxies.len()).entered();
    if state.proxies_seen != Some(scan.seen) {
        state.proxies_seen = Some(scan.seen);
        let joints = scan
            .owner
            .and_then(|e| owners.get(e).ok())
            .map(|mesh| mesh.joints.as_slice());
        let root = scan.owner.map(|e| hierarchy_root(e, parents));
        let mut found: Vec<(SpawnKey, Entity)> = proxies
            .iter()
            .filter(|(e, p, _)| {
                let named = scan.names.is_empty() || scan.names.contains(&p.name);
                let same_body = root.is_none_or(|root| {
                    joints.is_some_and(|j| j.contains(e)) || hierarchy_root(*e, parents) == root
                });
                named && same_body && !state.proxies.iter().any(|(listed, _)| listed == e)
            })
            .map(|(e, _, _)| (spawn_key(proxy_order, e), e))
            .collect();
        found.sort_unstable();
        for (_, entity) in found {
            let Ok((_, proxy, transform)) = proxies.get(entity) else {
                continue;
            };
            let shape = proxy_shape(proxy, transform.0);
            let collider = ClothCollider {
                shape,
                friction: proxy.friction,
            };
            if solver.add_collider(collider).is_ok() {
                state.proxies.push((entity, Some(shape)));
            }
        }
    }
    for index in 0..state.proxies.len() {
        let (entity, last) = state.proxies[index];
        let (shape, friction) = match proxies.get(entity) {
            Ok((_, proxy, transform)) => (proxy_shape(proxy, transform.0), proxy.friction),
            Err(_) => (PARKED, 0.0),
        };
        let previous = last.unwrap_or(shape);
        if solver
            .set_collider(index, ClothCollider { shape, friction })
            .is_ok()
        {
            let _ = solver.set_collider_previous(index, previous);
        }
        state.proxies[index].1 = Some(shape);
    }
}

fn axis_rotation(axis: ShapeAxis) -> Quat {
    axis.from_y_rotation()
}

/// Outward half-spaces `n · x <= d` of a convex hull with at most
/// [`regia_physics::cloth::snapshot::SNAPSHOT_HULL_PLANES_MAX`] vertices and unique planes.
/// `None` when the cloud is larger, flat, or does not yield a hull in that budget.
fn convex_hull_planes(points: &[Vec3]) -> Option<Vec<(Vec3, f32)>> {
    const MAX: usize = regia_physics::cloth::snapshot::SNAPSHOT_HULL_PLANES_MAX;
    if points.is_empty() || points.len() > MAX {
        return None;
    }
    let hull = regia_physics::convex_decomposition::convex_hull_of_points(points, MAX as u32)?;
    let mut planes = Vec::new();
    for tri in &hull.triangles {
        let (Some(a), Some(b), Some(c)) = (
            hull.vertices.get(tri[0] as usize),
            hull.vertices.get(tri[1] as usize),
            hull.vertices.get(tri[2] as usize),
        ) else {
            continue;
        };
        let n = (*b - *a).cross(*c - *a);
        let len = n.length();
        if len <= 1.0e-6 {
            continue;
        }
        let n = n.normalize();
        let d = n.dot(*a);
        let same = planes
            .iter()
            .any(|(pn, pd): &(Vec3, f32)| pn.dot(n) > 0.999 && (pd - d).abs() < 1.0e-3);
        if !same {
            planes.push((n, d));
        }
    }
    if planes.is_empty() || planes.len() > MAX {
        None
    } else {
        Some(planes)
    }
}

#[cfg(test)]
thread_local! {
    static HEIGHTFIELD_CELLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one heightfield cell the snapshot walk inspected. The counter exists only in tests.
#[inline]
fn note_heightfield_cell() {
    #[cfg(test)]
    HEIGHTFIELD_CELLS.with(|cells| cells.set(cells.get().saturating_add(1)));
}

/// Heightfield cells `snapshot_kinds` has inspected on this thread since the last take.
#[cfg(test)]
fn take_heightfield_cells() -> u64 {
    HEIGHTFIELD_CELLS.with(|cells| cells.replace(0))
}

/// Quad indices on one heightfield axis that can overlap `[lo, hi]`.
///
/// Sample `k` of `samples` sits at `(k / (samples - 1) - 0.5) * scale`. Quad `j` covers samples
/// `j` and `j + 1`. A scale that is not positive and finite visits every quad.
fn heightfield_quad_span(lo: f32, hi: f32, samples: usize, scale: f32) -> std::ops::Range<usize> {
    let quads = samples.saturating_sub(1);
    if quads == 0 || !(scale.is_finite() && scale > 0.0) || !lo.is_finite() || !hi.is_finite() {
        return 0..quads;
    }
    let denom = (samples - 1) as f32;
    // A few ulps of slack so a query that lands on a sample still includes that quad.
    let slack = 1.0e-4;
    let t0 = lo / scale + 0.5 - slack;
    let t1 = hi / scale + 0.5 + slack;
    let first = (t0 * denom - 1.0).ceil();
    let last = (t1 * denom).floor();
    if !first.is_finite() || !last.is_finite() {
        return 0..quads;
    }
    let first = (first as isize).max(0) as usize;
    let end = (last as isize).saturating_add(1).clamp(0, quads as isize) as usize;
    if first >= end {
        return 0..0;
    }
    first..end
}

/// Converts a rigid collider shape to snapshot shapes in the collider frame (each with its local
/// offset). Cylinders and cones use their bounding box. A convex hull of at most
/// [`regia_physics::cloth::snapshot::SNAPSHOT_HULL_PLANES_MAX`] vertices becomes outward planes
/// when that hull has at most that many unique planes; a larger hull, a failed hull, or an empty
/// point list uses the bounding box (an empty list adds nothing). Heightfields and trimeshes
/// become triangles (inside the query sphere `near`).
/// `near` is the query sphere `(center, radius)` in the collider frame.
fn snapshot_kinds(
    shape: &ColliderShape,
    local: Affine3A,
    near: (Vec3, f32),
    out: &mut Vec<(Affine3A, SnapshotKind)>,
) {
    match shape {
        ColliderShape::Sphere { radius } => out.push((local, SnapshotKind::Sphere { radius: *radius })),
        ColliderShape::Capsule {
            half_height,
            radius,
            axis,
        } => out.push((
            local * Affine3A::from_quat(axis_rotation(*axis)),
            SnapshotKind::Capsule {
                half_height: *half_height,
                radius: *radius,
            },
        )),
        ColliderShape::Box { half_extents } => out.push((
            local,
            SnapshotKind::Box {
                half_extents: *half_extents,
            },
        )),
        ColliderShape::Cylinder {
            half_height,
            radius,
            axis,
        }
        | ColliderShape::Cone {
            half_height,
            radius,
            axis,
        } => out.push((
            local * Affine3A::from_quat(axis_rotation(*axis)),
            SnapshotKind::Box {
                half_extents: Vec3::new(*radius, *half_height, *radius),
            },
        )),
        ColliderShape::ConvexHull { points } => {
            if points.is_empty() {
                return;
            }
            if let Some(planes) = convex_hull_planes(points) {
                // Planes are in the collider's local frame, so the shape stays on `local`.
                out.push((local, SnapshotKind::Hull { planes }));
                return;
            }
            let (lo, hi) = points
                .iter()
                .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(lo, hi), p| {
                    (lo.min(*p), hi.max(*p))
                });
            let half = ((hi - lo) * 0.5).max(Vec3::splat(1e-4));
            out.push((
                local * Affine3A::from_translation((lo + hi) * 0.5),
                SnapshotKind::Box { half_extents: half },
            ));
        }
        ColliderShape::TriMesh { vertices, indices } => {
            let c = local.inverse().transform_point3(near.0);
            let (lo, hi) = (c - Vec3::splat(near.1), c + Vec3::splat(near.1));
            let triangles: Vec<[Vec3; 3]> = indices
                .iter()
                .filter_map(|t| {
                    let tri = [
                        *vertices.get(t[0] as usize)?,
                        *vertices.get(t[1] as usize)?,
                        *vertices.get(t[2] as usize)?,
                    ];
                    let (a, b) = (tri[0].min(tri[1]).min(tri[2]), tri[0].max(tri[1]).max(tri[2]));
                    (a.cmple(hi).all() && b.cmpge(lo).all()).then_some(tri)
                })
                .collect();
            if !triangles.is_empty() {
                out.push((local, SnapshotKind::Triangles { triangles }));
            }
        }
        ColliderShape::HeightField {
            nx,
            nz,
            heights,
            scale,
        } => {
            let (nx, nz) = (*nx, *nz);
            if nx < 2 || nz < 2 || heights.len() < nx * nz {
                return;
            }
            let c = local.inverse().transform_point3(near.0);
            let (lo, hi) = (c - Vec3::splat(near.1), c + Vec3::splat(near.1));
            // Sample (i along Z, j along X) at x = (j / (nz - 1) - 0.5) * scale.x, likewise z.
            let point = |i: usize, j: usize| {
                Vec3::new(
                    (j as f32 / (nz - 1) as f32 - 0.5) * scale.x,
                    heights[i + j * nx] * scale.y,
                    (i as f32 / (nx - 1) as f32 - 0.5) * scale.z,
                )
            };
            let mut triangles = Vec::new();
            // Quads outside the query's XZ box cannot pass the reject below. Y stays inside the
            // loop: a quad below the query is kept (only one entirely above it is dropped).
            let i_span = heightfield_quad_span(lo.z, hi.z, nx, scale.z);
            let j_span = heightfield_quad_span(lo.x, hi.x, nz, scale.x);
            // `Range` is not `Copy` on this toolchain, so each row builds its own span.
            let (j0, j1) = (j_span.start, j_span.end);
            for i in i_span {
                for j in j0..j1 {
                    note_heightfield_cell();
                    let quad = [point(i, j), point(i + 1, j), point(i, j + 1), point(i + 1, j + 1)];
                    if quad.iter().any(|p| !p.is_finite()) {
                        continue;
                    }
                    let (a, b) = quad
                        .iter()
                        .fold((Vec3::splat(f32::MAX), Vec3::splat(f32::MIN)), |(a, b), p| {
                            (a.min(*p), b.max(*p))
                        });
                    if a.x > hi.x || b.x < lo.x || a.z > hi.z || b.z < lo.z || a.y > hi.y {
                        continue;
                    }
                    // Counter-clockwise seen from above (+Y).
                    triangles.push([quad[0], quad[1], quad[2]]);
                    triangles.push([quad[2], quad[1], quad[3]]);
                }
            }
            if !triangles.is_empty() {
                out.push((local, SnapshotKind::Triangles { triangles }));
            }
        }
        ColliderShape::Compound { shapes } => {
            for (offset, child) in shapes {
                let child_local = local
                    * Affine3A::from_scale_rotation_translation(
                        offset.scale,
                        offset.rotation,
                        offset.translation,
                    );
                snapshot_kinds(child, child_local, near, out);
            }
        }
    }
}

fn particle_bounds(positions: impl Iterator<Item = Vec3>) -> Option<(Vec3, Vec3)> {
    let mut bounds: Option<(Vec3, Vec3)> = None;
    for p in positions {
        bounds = Some(bounds.map_or((p, p), |(lo, hi)| (lo.min(p), hi.max(p))));
    }
    bounds
}

/// What one overlapping collider contributed to the last snapshot. Compared exactly, so a pose
/// or a shape edit misses the cache and rebuilds.
#[derive(Clone, Debug, PartialEq)]
struct SnapshotHitStamp {
    entity: u64,
    prev: Isometry,
    cur: Isometry,
    friction: f32,
    offset_translation: Vec3,
    offset_rotation: Quat,
    offset_scale: Vec3,
    /// Identity of the shape storage (pointer, size, endpoints), not a walk of every sample.
    cookie: u64,
}

/// Bounds and hit stamps of a snapshot the glue can reuse on the next unchanged step.
#[derive(Clone, Debug, PartialEq)]
struct SnapshotReuse {
    lo: Vec3,
    hi: Vec3,
    hits: Vec<SnapshotHitStamp>,
}

/// A fresh snapshot, or the one already on the solver when the rigid world did not change.
enum SnapshotBuild {
    Fresh(WorldSnapshot),
    Reuse,
}

/// What the snapshot builder reads from a collider entity.
///
/// `Ref` so an edit of the collider is visible. `GlobalTransform` is not consulted for change
/// detection: transform propagation writes it every frame even when the value is unchanged, and
/// the poses below come from the rigid step.
type SnapshotSource = (Ref<'static, Collider>, Ref<'static, GlobalTransform>);

/// What the snapshot builder reads from the rigid body of a collider.
type SnapshotBody = (
    &'static GlobalTransform,
    Option<&'static PhysicsPose>,
    Option<&'static PreviousPhysicsPose>,
);

/// The body a collider belongs to: its own entity, else the nearest ancestor with a [`RigidBody`]
/// (a collider on a child entity attaches to its parent's body).
fn collider_body(
    entity: Entity,
    parents: &Query<&ChildOf>,
    bodies: &Query<SnapshotBody, With<RigidBody>>,
) -> Option<Entity> {
    let mut at = Some(entity);
    while let Some(e) = at {
        if bodies.contains(e) {
            return Some(e);
        }
        at = parents.get(e).ok().map(ChildOf::parent);
    }
    None
}

/// A collider's pose at the previous and the current fixed step. A body's poses come from the
/// rigid step: `GlobalTransform` is only propagated in `PostUpdate` and would put the shape one
/// frame behind the rigid world, so it gives just the collider's place on its body.
fn collider_poses(
    world: Isometry,
    body: Option<(
        &GlobalTransform,
        Option<&PhysicsPose>,
        Option<&PreviousPhysicsPose>,
    )>,
) -> (Isometry, Isometry) {
    let Some((body_world, current, previous)) = body else {
        return (world, world);
    };
    let on_body = iso_of(body_world.0).inverse() * world;
    let pose = |p: Isometry| p * on_body;
    let cur = current.map_or(world, |p| pose(Isometry::new(p.rotation, p.translation)));
    let prev = previous.map_or(cur, |p| pose(Isometry::new(p.rotation, p.translation)));
    (prev, cur)
}

fn mix(hash: u64, value: u64) -> u64 {
    hash.wrapping_mul(0x9E37_79B1_85EB_CA87).wrapping_add(value)
}

fn mix_f(hash: u64, value: f32) -> u64 {
    mix(hash, value.to_bits() as u64)
}

fn mix_vec(hash: u64, value: Vec3) -> u64 {
    mix_f(mix_f(mix_f(hash, value.x), value.y), value.z)
}

fn mix_quat(hash: u64, value: Quat) -> u64 {
    mix_f(mix_f(mix_f(mix_f(hash, value.x), value.y), value.z), value.w)
}

fn ptr_bits<T>(ptr: *const T) -> u64 {
    ptr as usize as u64
}

/// A cheap identity of a collider shape: numbers for the small shapes, and the storage pointer,
/// length and endpoints for meshes and heightfields. It does not walk every sample.
fn shape_cookie(shape: &ColliderShape) -> u64 {
    match shape {
        ColliderShape::Box { half_extents } => mix_vec(1, *half_extents),
        ColliderShape::Sphere { radius } => mix_f(2, *radius),
        ColliderShape::Capsule {
            half_height,
            radius,
            axis,
        } => mix(mix_f(mix_f(3, *half_height), *radius), *axis as u64),
        ColliderShape::Cylinder {
            half_height,
            radius,
            axis,
        } => mix(mix_f(mix_f(4, *half_height), *radius), *axis as u64),
        ColliderShape::Cone {
            half_height,
            radius,
            axis,
        } => mix(mix_f(mix_f(5, *half_height), *radius), *axis as u64),
        ColliderShape::ConvexHull { points } => {
            let mut hash = mix(mix(6, points.len() as u64), ptr_bits(points.as_ptr()));
            if let Some(point) = points.first() {
                hash = mix_vec(hash, *point);
            }
            if let Some(point) = points.last() {
                hash = mix_vec(hash, *point);
            }
            hash
        }
        ColliderShape::TriMesh { vertices, indices } => {
            let mut hash = mix(mix(7, vertices.len() as u64), indices.len() as u64);
            hash = mix(hash, ptr_bits(vertices.as_ptr()));
            hash = mix(hash, ptr_bits(indices.as_ptr()));
            if let Some(vertex) = vertices.first() {
                hash = mix_vec(hash, *vertex);
            }
            if let Some(vertex) = vertices.last() {
                hash = mix_vec(hash, *vertex);
            }
            hash
        }
        ColliderShape::HeightField {
            nx,
            nz,
            heights,
            scale,
        } => {
            let mut hash = mix(mix(mix(8, *nx as u64), *nz as u64), heights.len() as u64);
            hash = mix(hash, ptr_bits(heights.as_ptr()));
            hash = mix_vec(hash, *scale);
            if let Some(sample) = heights.first() {
                hash = mix_f(hash, *sample);
            }
            if let Some(sample) = heights.last() {
                hash = mix_f(hash, *sample);
            }
            hash
        }
        ColliderShape::Compound { shapes } => {
            let mut hash = mix(9, shapes.len() as u64);
            hash = mix(hash, ptr_bits(shapes.as_ptr()));
            for (offset, child) in shapes {
                hash = mix_vec(hash, offset.translation);
                hash = mix_quat(hash, offset.rotation);
                hash = mix_vec(hash, offset.scale);
                hash = mix(hash, shape_cookie(child));
            }
            hash
        }
    }
}

/// Builds a world snapshot per cloth and soft body (spec 55 § 2.4): a bounding-box query around
/// the particle bounds grown by the margin (the exact `overlap` misses heightfields, so terrain
/// would not stop the cloth), filtered by the instance's layer and sensors excluded; shapes keyed
/// by the collider's spawn-arrival key (ADR-1347, never entity bits or a session `EntityGuid`);
/// triangles capped nearest to the cloth. Without a `PhysicsWorld` nothing happens.
pub fn build_cloth_snapshots(
    world: Option<Res<PhysicsWorld>>,
    layers: Option<Res<PhysicsLayers>>,
    mut cloths: ResMut<ClothSet>,
    mut softs: ResMut<SoftSet>,
    mut cloth_q: Query<(&ClothSim, &mut ClothGlueState)>,
    mut soft_q: Query<(&SoftSim, &mut ClothGlueState), Without<ClothSim>>,
    rigid: SnapshotQueries,
) {
    let Some(world) = world else {
        return;
    };
    let (colliders, bodies, parents, collider_order) = rigid;
    let queries = WorldQueries {
        world: &world,
        layers: layers.as_deref(),
    };
    let build = |layer: &str,
                 margin: f32,
                 bounds: Option<(Vec3, Vec3)>,
                 state: &mut ClothGlueState|
     -> Option<SnapshotBuild> {
        let (lo, hi) = bounds?;
        let _span = debug_span!("cloth_snapshot").entered();
        let (lo, hi) = (lo - Vec3::splat(margin), hi + Vec3::splat(margin));
        let center = (lo + hi) * 0.5;
        let half = ((hi - lo) * 0.5).max(Vec3::splat(1e-3));
        let mut filter = QueryFilter::new().exclude_sensors();
        if let Some(index) = layers.as_deref().and_then(|l| l.index_of(layer)) {
            filter = filter.from_layer(CollisionLayer::new(index));
        }
        let hits = queries.overlap_aabb(lo, hi, filter);
        let mut stamps = Vec::with_capacity(hits.len());
        let mut seen: Vec<(Entity, Option<Entity>)> = Vec::with_capacity(hits.len());
        let mut changed = false;
        for entity in hits {
            let Ok((collider, transform)) = colliders.get(entity) else {
                continue;
            };
            // The collider component, not the transform: propagation rewrites `GlobalTransform`
            // every frame even when the pose is the same.
            changed |= collider.is_changed();
            let body = collider_body(entity, &parents, &bodies);
            let (prev, cur) = collider_poses(iso_of(transform.0), body.and_then(|b| bodies.get(b).ok()));
            stamps.push(SnapshotHitStamp {
                entity: entity.to_bits(),
                prev,
                cur,
                friction: collider.friction.max(0.0),
                offset_translation: collider.offset.translation,
                offset_rotation: collider.offset.rotation,
                offset_scale: collider.offset.scale,
                cookie: shape_cookie(&collider.shape),
            });
            seen.push((entity, body));
        }
        let same = !changed
            && state
                .snapshot_reuse
                .as_ref()
                .is_some_and(|previous| previous.lo == lo && previous.hi == hi && previous.hits == stamps);
        if same {
            return Some(SnapshotBuild::Reuse);
        }

        let mut shapes = Vec::new();
        state.bodies.clear();
        let mut kinds = Vec::new();
        for ((entity, body), stamp) in seen.into_iter().zip(&stamps) {
            let Ok((collider, _)) = colliders.get(entity) else {
                continue;
            };
            let key = spawn_key(collider_order.as_deref(), entity);
            let offset = Affine3A::from_scale_rotation_translation(
                Vec3::ONE,
                collider.offset.rotation,
                collider.offset.translation,
            );
            kinds.clear();
            let cur_frame = Affine3A::from_rotation_translation(stamp.cur.rotation, stamp.cur.translation);
            let inv = cur_frame.inverse();
            snapshot_kinds(
                &collider.shape,
                offset,
                (inv.transform_point3(center), half.length()),
                &mut kinds,
            );
            for (local, kind) in kinds.drain(..) {
                let (_, r, t) = local.to_scale_rotation_translation();
                let at =
                    |body: Isometry| Isometry::new(body.rotation * r, body.translation + body.rotation * t);
                shapes.push(SnapshotShape {
                    key,
                    kind,
                    prev: at(stamp.prev),
                    cur: at(stamp.cur),
                    friction: stamp.friction,
                });
            }
            if let Some(body) = body {
                state.bodies.insert(key, body);
            }
        }
        WorldSnapshot::cap_triangles(&mut shapes, center);
        let snapshot = match WorldSnapshot::new(shapes) {
            Ok(snapshot) => snapshot,
            Err(err) => {
                warn!(target: "regia_framework", "cloth snapshot dropped: {err}");
                state.snapshot_reuse = None;
                return None;
            }
        };
        state.snapshot_reuse = Some(SnapshotReuse { lo, hi, hits: stamps });
        Some(SnapshotBuild::Fresh(snapshot))
    };
    for (sim, mut state) in &mut cloth_q {
        let Some(solver) = cloth_solver_mut(&mut cloths, &mut state) else {
            continue;
        };
        let bounds = particle_bounds(solver.particles().iter().map(|p| p.position));
        match build(&sim.layer, sim.snapshot_margin, bounds, &mut state) {
            Some(SnapshotBuild::Fresh(snapshot)) => solver.set_snapshot(snapshot),
            Some(SnapshotBuild::Reuse) => solver.reuse_snapshot(),
            None => {}
        }
    }
    for (sim, mut state) in &mut soft_q {
        let Some(solver) = soft_solver_mut(&mut softs, &mut state) else {
            continue;
        };
        let bounds = particle_bounds(solver.cloth().particles().iter().map(|p| p.position));
        match build(&sim.layer, sim.snapshot_margin, bounds, &mut state) {
            Some(SnapshotBuild::Fresh(snapshot)) => solver.cloth_mut().set_snapshot(snapshot),
            Some(SnapshotBuild::Reuse) => solver.cloth_mut().reuse_snapshot(),
            None => {}
        }
    }
}

/// The palette of a cloth that is not skinned: its rest mesh follows one transform.
static IDENTITY_PALETTE: [Mat4; 1] = [Mat4::IDENTITY];

/// What the pose sync reads from a skinned owner.
type SkinOwner = (
    &'static SkinnedMesh,
    &'static GlobalTransform,
    Option<&'static SkinPalette>,
);

/// Whether a root flips handedness (a negative scale, spec 55 § 2.9).
fn is_mirrored(world: Affine3A) -> bool {
    world.matrix3.determinant() < 0.0
}

/// Skins `rest` into the scratch targets (spec 55 § 2.2): with the owner's joint palette (its
/// `SkinPalette`, else the joint entities' world transforms), or placed by the instance's
/// transform when it has no skin or no owner. Returns the root pose, or `None` when there is no
/// palette yet or skinning failed (the code is in `last_error`).
fn pose_targets(
    rest: &[Vec3],
    skinned_mesh: Option<Entity>,
    transform: Option<&GlobalTransform>,
    state: &mut ClothGlueState,
    owners: &Query<SkinOwner>,
    joints: &Query<&GlobalTransform>,
    scratch: &mut PoseScratch,
) -> Option<Isometry> {
    let (skinned, targets, rigid) = scratch;
    let owner = if state.influences.is_empty() {
        None
    } else {
        skinned_mesh.and_then(|e| owners.get(e).ok())
    };
    let (palette, world, influences): (Cow<'_, [Mat4]>, Affine3A, &[SkinInfluence]) = match owner {
        Some((mesh, owner_world, palette)) => {
            let mesh_world = Mat4::from(owner_world.0);
            let matrices = match palette {
                Some(p) if !p.matrices.is_empty() => Some(Cow::Borrowed(p.matrices.as_slice())),
                _ => mesh
                    .palette_from_joint_worlds(mesh_world, |j| joints.get(j).ok().map(|w| Mat4::from(w.0)))
                    .map(Cow::Owned),
            };
            (matrices?, owner_world.0, state.influences.as_slice())
        }
        None => {
            rigid.clear();
            rigid.resize(
                rest.len(),
                SkinInfluence {
                    joints: [0; 4],
                    weights: [1.0, 0.0, 0.0, 0.0],
                },
            );
            (
                Cow::Borrowed(IDENTITY_PALETTE.as_slice()),
                world_of(transform),
                &rigid[..],
            )
        }
    };
    let skinned_ok = skin_points(
        palette.as_ref(),
        Mat4::from(world),
        rest,
        &state.rest_normals,
        influences,
        skinned,
    );
    if let Err(err) = skinned_ok {
        state.last_error = Some(err.code());
        return None;
    }
    targets.clear();
    targets.extend(
        skinned
            .iter()
            .map(|&(position, normal)| PoseTarget { position, normal }),
    );
    if state.last_targets.len() != targets.len() {
        state.last_targets.clone_from(targets);
    }
    state.mirrored = is_mirrored(world);
    Some(iso_of(world))
}

/// Hands the solver the last and the new pose targets and root.
fn push_pose(
    solver: &mut ClothSolver,
    state: &ClothGlueState,
    targets: &[PoseTarget],
    root: Isometry,
) -> Result<(), ClothError> {
    solver
        .set_pose_targets(&state.last_targets, targets)
        .and_then(|()| solver.set_root_motion(state.last_root.unwrap_or(root), root))
}

/// Sets each cloth's pose targets (spec 55 § 2.2): the sim mesh skinned with the owner's joint
/// palette, or the rest mesh placed by the cloth entity's transform; the root motion is the
/// entity's (or owner's) world transform. A posed soft body ([`SoftAsset::is_posed`]) gets the
/// same from its rest shape (spec 55 § 2.8); a free one has no pose.
pub fn sync_cloth_pose(
    mut cloths: ResMut<ClothSet>,
    mut softs: ResMut<SoftSet>,
    mut sims: Query<(&ClothSim, Option<&GlobalTransform>, &mut ClothGlueState)>,
    mut soft_sims: Query<SoftPoseInstance, Without<ClothSim>>,
    owners: Query<SkinOwner>,
    joints: Query<&GlobalTransform>,
    mut scratch: Local<PoseScratch>,
) {
    let scratch = &mut *scratch;
    for (sim, transform, mut state) in &mut sims {
        let Some(solver) = cloth_solver_mut(&mut cloths, &mut state) else {
            continue;
        };
        let lod = &sim.asset.lods[state.sim_lod];
        let _span = debug_span!("cloth_pose_sync", vertices = lod.rest_positions.len()).entered();
        let Some(root) = pose_targets(
            &lod.rest_positions,
            sim.skinned_mesh,
            transform,
            &mut state,
            &owners,
            &joints,
            scratch,
        ) else {
            continue;
        };
        let result = push_pose(solver, &state, &scratch.1, root)
            .and_then(|()| {
                // Only a change of `ClothSim.blend` is pushed, so an agent's
                // `physics.cloth_set_params { blend }` holds until the game changes it.
                if state.applied_blend != Some(sim.blend) {
                    solver.set_blend(sim.blend.clamp(0.0, 1.0))
                } else {
                    Ok(())
                }
            })
            .and_then(|()| {
                if state.applied_pull_scale != Some(sim.pull_scale) {
                    solver.set_tuning(ClothTuning {
                        pull_scale: sim.pull_scale,
                        ..*solver.tuning()
                    })
                } else {
                    Ok(())
                }
            });
        state.applied_blend = Some(sim.blend);
        state.applied_pull_scale = Some(sim.pull_scale);
        state.last_error = result.err().map(|e| e.code());
        std::mem::swap(&mut state.last_targets, &mut scratch.1);
        state.last_root = Some(root);
    }
    for (sim, transform, mut state) in &mut soft_sims {
        if !sim.asset.is_posed() {
            continue;
        }
        let Some(solver) = soft_solver_mut(&mut softs, &mut state) else {
            continue;
        };
        let _span = debug_span!("cloth_pose_sync", vertices = sim.asset.rest_positions.len()).entered();
        let Some(root) = pose_targets(
            &sim.asset.rest_positions,
            sim.skinned_mesh,
            transform,
            &mut state,
            &owners,
            &joints,
            scratch,
        ) else {
            continue;
        };
        let result = push_pose(solver.cloth_mut(), &state, &scratch.1, root);
        state.last_error = result.err().map(|e| e.code());
        std::mem::swap(&mut state.last_targets, &mut scratch.1);
        state.last_root = Some(root);
    }
}

/// Pauses a hidden solver and resumes one the glue paused. Every frame while hidden: a sim-mesh
/// LOD switch may have replaced the solver. A pause made by anyone else stays.
fn sync_hidden(solver: &mut ClothSolver, state: &mut ClothGlueState, visible: bool) {
    if !visible {
        solver.set_paused(true);
    } else if state.hidden_pause {
        solver.set_paused(false);
    }
    state.hidden_pause = !visible;
}

/// Pauses cloth and soft bodies that are not visible (the instance's or its section's
/// `InheritedVisibility` is false: hidden in the hierarchy; the renderer's `ViewVisibility`, written
/// since ADR-7200, is not read here, so an off-screen cloth keeps stepping), picks each one's distance LOD from the camera with the lowest
/// `order` (10 % hysteresis; an `order` tie resolves to the earliest-spawned camera, ADR-1347,
/// never the lowest entity id) and moves the farthest down until the [`ClothBudget`] fits. Cloth
/// and soft bodies share the budget.
pub fn sync_cloth_visibility(
    budget: Res<ClothBudget>,
    mut sets: (ResMut<ClothSet>, ResMut<SoftSet>),
    mut sims: Query<LodInstance>,
    mut soft_sims: Query<SoftLodInstance, Without<ClothSim>>,
    (visibility, cameras): (
        Query<&InheritedVisibility>,
        Query<(Entity, &Camera, &GlobalTransform)>,
    ),
    (camera_order, shared_order): ClothSpawnOrders,
    mut entries: Local<Vec<lod::BudgetEntry>>,
) {
    let (cloths, softs) = &mut sets;
    let (cloths, softs) = (&mut **cloths, &mut **softs);
    let _span = debug_span!("cloth_lod").entered();
    let eye = cameras
        .iter()
        .min_by_key(|(e, c, _)| (c.order, spawn_key(camera_order.as_deref(), *e)))
        .map(|(_, _, t)| Vec3::from(t.0.translation));
    let shown = |view: Option<&InheritedVisibility>, section: Option<Entity>| {
        view.or_else(|| section.and_then(|s| visibility.get(s).ok()))
            .is_none_or(|v| v.0)
    };
    entries.clear();
    for (entity, sim, mut state, view) in &mut sims {
        let Some(solver) = cloth_solver_mut(cloths, &mut state) else {
            continue;
        };
        sync_hidden(solver, &mut state, shown(view, sim.section));
        let Some(eye) = eye else {
            continue;
        };
        let distance = solver.centroid().distance(eye);
        state.lod = lod::lod_level(distance, state.lod, sim.lod_distances);
        entries.push(lod::BudgetEntry {
            distance,
            key: shared_spawn_key(shared_order.as_deref(), entity),
            // LOD 0's count, as `BudgetEntry` says: a coarser sim mesh must not free budget
            // that would move the cloth straight back.
            particles: sim.asset.lod0().rest_positions.len(),
            level: state.lod,
        });
    }
    for (entity, sim, mut state, view) in &mut soft_sims {
        let Some(solver) = soft_solver_mut(softs, &mut state) else {
            continue;
        };
        let solver = solver.cloth_mut();
        sync_hidden(solver, &mut state, shown(view, sim.section));
        let Some(eye) = eye else {
            continue;
        };
        let distance = solver.centroid().distance(eye);
        state.lod = lod::lod_level(distance, state.lod, sim.lod_distances);
        entries.push(lod::BudgetEntry {
            distance,
            key: shared_spawn_key(shared_order.as_deref(), entity),
            particles: sim.asset.rest_positions.len(),
            level: state.lod,
        });
    }
    if entries.is_empty() {
        return;
    }
    lod::apply_budget(&mut entries, budget.max_particles_per_step);
    entries.sort_unstable_by_key(|e| e.key);
    let levels = entries.as_slice();
    let set_level = |solver: &mut ClothSolver, key: SpawnKey| {
        if let Ok(at) = levels.binary_search_by_key(&key, |e| e.key)
            && solver.lod_level() != levels[at].level
        {
            solver.set_lod_level(levels[at].level.min(LOD_OFF));
        }
    };
    for (entity, _, mut state, _) in &mut sims {
        if let Some(solver) = cloth_solver_mut(cloths, &mut state) {
            set_level(solver, shared_spawn_key(shared_order.as_deref(), entity));
        }
    }
    for (entity, _, mut state, _) in &mut soft_sims {
        if let Some(solver) = soft_solver_mut(softs, &mut state) {
            set_level(
                solver.cloth_mut(),
                shared_spawn_key(shared_order.as_deref(), entity),
            );
        }
    }
}

/// Normals taken from triangle winding point inward under a mirrored root.
fn flip_normals(mirrored: bool, output: &mut [PoseTarget]) {
    if mirrored {
        for t in output {
            t.normal = -t.normal;
        }
    }
}

/// A triangle in draw order. The renderer culls by world winding and a mirrored root reverses
/// it, whatever the section's own transform is (spec 55 § 2.9).
fn drawn_triangle(t: [u32; 3], mirrored: bool) -> [u32; 3] {
    if mirrored { [t[0], t[2], t[1]] } else { t }
}

/// Swaps each instance's [`ClothOutput`] after the step: `prev` takes `cur`, `cur` takes the
/// solver output (cloth: blended positions and normals; soft: particle positions with
/// area-weighted surface normals).
pub fn collect_cloth_output(
    cloths: Res<ClothSet>,
    softs: Res<SoftSet>,
    mut q: Query<(&mut ClothGlueState, &mut ClothOutput, Option<&SoftSim>)>,
    mut scratch: Local<(Vec<Vec3>, Vec<Vec3>)>,
) {
    let _span = debug_span!("cloth_collect_output").entered();
    let (normals, points) = &mut *scratch;
    for (mut state, mut output, soft) in &mut q {
        let output = &mut *output;
        if let Some(sim) = soft {
            let Some(solver) = soft_solver(&softs, &mut state) else {
                continue;
            };
            let steps = solver.cloth().stats().steps;
            if steps == output.steps && !output.cur.is_empty() {
                continue;
            }
            std::mem::swap(&mut output.prev, &mut output.cur);
            points.clear();
            points.extend(solver.cloth().particles().iter().map(|p| p.position));
            let asset = &sim.asset;
            if asset.surface_binding.is_empty() {
                vertex_normals(points, &asset.surface_triangles, normals);
                output.cur.clear();
                output.cur.extend(
                    points
                        .iter()
                        .zip(normals.iter())
                        .map(|(&position, &normal)| PoseTarget { position, normal }),
                );
            } else {
                output.cur.clear();
                output.cur.extend(asset.surface_binding.iter().map(|b| {
                    let position = match &asset.model {
                        regia_physics::cloth::asset::SoftModelData::Tetra { tets, .. } => {
                            tets.get(b.tet as usize).map_or(Vec3::ZERO, |t| {
                                (0..4).map(|k| points[t[k] as usize] * b.weights[k]).sum()
                            })
                        }
                        _ => Vec3::ZERO,
                    };
                    PoseTarget {
                        position,
                        normal: Vec3::ZERO,
                    }
                }));
                points.clear();
                points.extend(output.cur.iter().map(|t| t.position));
                vertex_normals(points, &asset.surface_triangles, normals);
                for (t, n) in output.cur.iter_mut().zip(normals.iter()) {
                    t.normal = *n;
                }
            }
            flip_normals(state.mirrored, &mut output.cur);
            if output.prev.len() != output.cur.len() {
                output.prev.clone_from(&output.cur);
            }
            output.steps = steps;
            output.state = Some(solver.cloth().state());
            continue;
        }
        let Some(solver) = cloth_solver(&cloths, &mut state) else {
            continue;
        };
        let steps = solver.stats().steps;
        if steps == output.steps && !output.cur.is_empty() {
            continue;
        }
        std::mem::swap(&mut output.prev, &mut output.cur);
        solver.output(&mut output.cur);
        flip_normals(state.mirrored, &mut output.cur);
        if output.prev.len() != output.cur.len() {
            output.prev.clone_from(&output.cur);
        }
        output.steps = steps;
        output.state = Some(solver.state());
    }
}

/// Writes [`ClothStateChanged`], [`ClothTeleported`] and [`ClothTear`] for what each instance's
/// solver did this step. A new instance reports from its first change on.
pub fn emit_cloth_messages(
    mut cloths: ResMut<ClothSet>,
    softs: Res<SoftSet>,
    mut q: Query<(Entity, &mut ClothGlueState, Has<SoftSim>)>,
    mut changes: MessageWriter<ClothStateChanged>,
    mut teleports: MessageWriter<ClothTeleported>,
    mut tears: MessageWriter<ClothTear>,
    mut torn: Local<Vec<regia_physics::cloth::ClothTorn>>,
) {
    for (entity, mut state, soft) in &mut q {
        let reported = if soft {
            soft_solver(&softs, &mut state).map(|solver| {
                let cloth = solver.cloth();
                (cloth.state(), cloth.diagnostics().teleports)
            })
        } else {
            cloth_solver(&cloths, &mut state).map(|solver| (solver.state(), solver.diagnostics().teleports))
        };
        let Some((now, teleported)) = reported else {
            continue;
        };
        if let Some(from) = state.reported_state
            && from != now
        {
            changes.write(ClothStateChanged {
                entity,
                from,
                to: now,
            });
        }
        state.reported_state = Some(now);
        if teleported > state.reported_teleports {
            teleports.write(ClothTeleported { entity });
        }
        state.reported_teleports = teleported;
        if soft {
            continue;
        }
        torn.clear();
        if let Some(solver) = cloth_solver_mut(&mut cloths, &mut state) {
            solver.drain_torn(&mut torn);
        }
        for cut in torn.iter() {
            tears.write(ClothTear {
                entity,
                constraint: cut.constraint,
                step: cut.step,
            });
        }
    }
}

/// What `apply_cloth_reaction` reads of a body: its kind, pose, transform and mass.
type ReactionBody = (
    &'static RigidBody,
    Option<&'static PhysicsPose>,
    Option<&'static GlobalTransform>,
    Option<&'static regia_physics::Mass>,
);

/// A body's world center of mass for the reaction torque: its pose (`PhysicsPose`, else its
/// transform) and, with an explicit `Mass`, the local center of mass; with `Mass::Auto` the body
/// origin stands in for it (exact for colliders centered on the body).
fn reaction_center(
    pose: Option<&PhysicsPose>,
    transform: Option<&GlobalTransform>,
    mass: Option<&regia_physics::Mass>,
) -> Option<Vec3> {
    let (rotation, translation) = match (pose, transform) {
        (Some(p), _) => (p.rotation, p.translation),
        (None, Some(t)) => {
            let (_, r, tr) = t.0.to_scale_rotation_translation();
            (r, tr)
        }
        (None, None) => return None,
    };
    let local = match mass {
        Some(regia_physics::Mass::Explicit { center_of_mass, .. }) => *center_of_mass,
        _ => Vec3::ZERO,
    };
    Some(translation + rotation * local)
}

/// Per-body impulse sums [`apply_cloth_reaction`] reuses (clear + fill, `Local`-owned).
#[derive(Default)]
pub struct ReactionMaps {
    /// Running impulse/torque per rigid body across every reaction cloth this step.
    per_body: BTreeMap<Entity, (Vec3, Vec3)>,
    /// One cloth's contact sums before the cap is applied.
    sums: BTreeMap<Entity, (Vec3, Vec3)>,
}

/// Two-way reaction (spec 55 § 2.4): sums each cloth's contact impulses per rigid body,
/// caps the sum at the cloth's `max_reaction_impulse` and adds it as an `ExternalImpulse` for
/// the next rigid step, with the torque impulse of the contacts about the body's center of mass
/// (scaled by the same cap factor, ADR-2425). Only cloths with `reaction: true` take part.
pub fn apply_cloth_reaction(
    mut commands: Commands,
    mut cloths: ResMut<ClothSet>,
    mut sims: Query<(&ClothSim, &mut ClothGlueState)>,
    mut impulses: Query<&mut ExternalImpulse>,
    bodies: Query<ReactionBody>,
    mut drained: Local<Vec<regia_physics::cloth::ClothReaction>>,
    mut maps: Local<ReactionMaps>,
) {
    let ReactionMaps { per_body, sums } = &mut *maps;
    per_body.clear();
    for (sim, mut state) in &mut sims {
        if !sim.reaction {
            continue;
        }
        let Some(solver) = cloth_solver_mut(&mut cloths, &mut state) else {
            continue;
        };
        drained.clear();
        solver.take_reaction_wrench(&mut drained);
        if drained.is_empty() {
            continue;
        }
        #[cfg(test)]
        {
            state.reaction_runs = state.reaction_runs.wrapping_add(1);
        }
        sums.clear();
        for reaction in drained.iter() {
            let Some(&body) = state.bodies.get(&reaction.key) else {
                continue;
            };
            let center = bodies
                .get(body)
                .ok()
                .and_then(|(_, pose, transform, mass)| reaction_center(pose, transform, mass))
                .unwrap_or(reaction.about);
            let sum = sums.entry(body).or_default();
            sum.0 += reaction.impulse;
            sum.1 += reaction.angular_about(center);
        }
        for (body, (linear, angular)) in sums.iter() {
            let (linear, angular) = (*linear, *angular);
            let cap = sim.max_reaction_impulse.max(0.0);
            let capped = linear.clamp_length_max(cap);
            let length = linear.length();
            let scale = if length > 0.0 {
                capped.length() / length
            } else {
                1.0
            };
            let slot = per_body.entry(*body).or_default();
            slot.0 += capped;
            slot.1 += angular * scale;
        }
        sums.clear();
    }
    for (body, (impulse, torque_impulse)) in per_body.iter() {
        let (body, impulse, torque_impulse) = (*body, *impulse, *torque_impulse);
        let dynamic = bodies
            .get(body)
            .is_ok_and(|(b, ..)| b.kind == regia_physics::BodyKind::Dynamic);
        if !dynamic || !impulse.is_finite() || !torque_impulse.is_finite() {
            continue;
        }
        if let Ok(mut existing) = impulses.get_mut(body) {
            existing.impulse += impulse;
            existing.torque_impulse += torque_impulse;
        } else {
            commands.entity(body).insert(ExternalImpulse {
                impulse,
                torque_impulse,
            });
        }
    }
}

/// Writes `vertices` and `indices` into `slot`. A unique arc is rewritten in place (its
/// buffers keep their capacity and the cache id). Otherwise a new arc is stored and, when
/// `alias_key` is set, shares that cache id. [`regia_render::alias_mesh_key`] runs before any
/// [`regia_render::mesh_key`] on the new arc. Either way the content revision moves, so the
/// resident GPU buffers rewrite in place while the byte size holds.
fn install_render_mesh(
    slot: &mut Option<Arc<CpuMesh>>,
    alias_key: Option<u64>,
    vertices: &mut Vec<LabVertex>,
    indices: &mut Vec<u32>,
) -> Option<(Arc<CpuMesh>, bool)> {
    if let Some(existing) = slot.as_mut()
        && regia_render::with_unique_mesh_mut(existing, |cpu| {
            std::mem::swap(&mut cpu.vertices, vertices);
            std::mem::swap(&mut cpu.indices, indices);
        })
        .is_some()
    {
        vertices.clear();
        indices.clear();
        regia_render::note_mesh_content_changed(existing);
        return Some((Arc::clone(existing), false));
    }
    let mesh = Arc::new(CpuMesh {
        vertices: std::mem::take(vertices),
        indices: std::mem::take(indices),
    });
    if let Some(key) = alias_key {
        if !regia_render::alias_mesh_key(&mesh, key) {
            warn!(target: "regia_framework", "cloth mesh alias failed; a new cache key was assigned");
            let _ = regia_render::mesh_key(&mesh);
        }
    } else {
        let _ = regia_render::mesh_key(&mesh);
    }
    regia_render::note_mesh_content_changed(&mesh);
    *slot = Some(Arc::clone(&mesh));
    Some((mesh, true))
}

/// Publishes into the arc the renderer is not holding and flips `render_front`. The flag is set
/// when a new arc was made (no unique back buffer to rewrite in place).
fn publish_back_mesh(
    state: &mut ClothGlueState,
    vertices: &mut Vec<LabVertex>,
    indices: &mut Vec<u32>,
) -> Option<(Arc<CpuMesh>, bool)> {
    let empty = state.render_meshes.iter().all(|slot| slot.is_none());
    let front = usize::from(state.render_front) & 1;
    let slot = if empty { 0 } else { front ^ 1 };
    let alias_key = if empty {
        None
    } else {
        state.render_meshes[front].as_ref().map(regia_render::mesh_key)
    };
    let published = install_render_mesh(&mut state.render_meshes[slot], alias_key, vertices, indices)?;
    state.render_front = u8::try_from(slot).unwrap_or(0);
    Some(published)
}

/// Publishes the render section of every cloth and soft body (spec 55 § 2.2): the output
/// interpolated by the fixed-step overstep, render vertices from their sim-triangle bindings
/// (torn sim triangles drop their render triangles), in the section entity's local space.
///
/// Two `Arc<CpuMesh>` values alternate under one cache id. The extracted frame can hold the
/// front arc while the next publish rewrites the back arc in place, so a visible cloth costs
/// no new mesh arc, no new cache key and no full upload per frame; the cache rewrites the
/// resident buffers. `gi_proxy` stays off: a signed-distance brick is baked per key.
pub fn publish_cloth_mesh(
    fixed: Option<Res<FixedTime>>,
    cloths: Res<ClothSet>,
    mut q: Query<PublishInstance>,
    mut sections: Query<(&mut MeshRenderer, Option<&GlobalTransform>)>,
    mut scratch: Local<(Vec<Vec3>, Vec<Vec3>, Vec<Vec3>)>,
    mut mesh_scratch: Local<(Vec<LabVertex>, Vec<u32>)>,
    #[cfg(test)] mut arcs: Option<ResMut<ClothRenderArcs>>,
) {
    let alpha = fixed
        .as_deref()
        .map_or(1.0, FixedTime::overstep_fraction)
        .clamp(0.0, 1.0);
    let (positions, normals, sim_normals) = &mut *scratch;
    let (vertices, indices) = &mut *mesh_scratch;
    for (cloth, soft, mut state, mut output) in &mut q {
        let section = cloth
            .and_then(|c| c.section)
            .or_else(|| soft.and_then(|s| s.section));
        let Some(section) = section else {
            continue;
        };
        let Ok((mut renderer, section_world)) = sections.get_mut(section) else {
            continue;
        };
        if output.cur.is_empty() {
            continue;
        }
        let _span = debug_span!("cloth_publish_mesh", vertices = output.cur.len()).entered();
        positions.clear();
        normals.clear();
        for (a, b) in output.prev.iter().zip(&output.cur) {
            positions.push(a.position.lerp(b.position, alpha));
            normals.push(a.normal.lerp(b.normal, alpha).normalize_or_zero());
        }
        let to_local = world_of(section_world).inverse();
        vertices.clear();
        indices.clear();
        let mut hidden = 0usize;
        if let Some(sim) = cloth {
            let lod = &sim.asset.lods[state.sim_lod];
            if positions.len() != lod.rest_positions.len() {
                // A sim LOD switch this step: the output catches up at the next fixed step.
                continue;
            }
            vertex_normals(positions, &lod.triangles, sim_normals);
            if state.mirrored {
                sim_normals.iter_mut().for_each(|n| *n = -*n);
            }
            let solver = cloth_solver(&cloths, &mut state);
            vertices.reserve(lod.render.vertices.len());
            for b in &lod.render.vertices {
                let (p, n) = bind_point(b, &lod.triangles, positions, sim_normals);
                vertices.push(LabVertex::from_pos_normal_uv(
                    to_local.transform_point3(p).to_array(),
                    to_local.transform_vector3(n).normalize_or_zero().to_array(),
                    b.uv,
                ));
            }
            for t in &lod.render.indices {
                let torn = solver.is_some_and(|s| {
                    t.iter().any(|&v| {
                        lod.render
                            .vertices
                            .get(v as usize)
                            .is_some_and(|b| !s.triangle_visible(b.triangle as usize))
                    })
                });
                if torn {
                    hidden += 1;
                } else {
                    indices.extend_from_slice(&drawn_triangle(*t, state.mirrored));
                }
            }
        } else if let Some(sim) = soft {
            for (i, (p, n)) in positions.iter().zip(normals.iter()).enumerate() {
                vertices.push(LabVertex::from_pos_normal_uv(
                    to_local.transform_point3(*p).to_array(),
                    to_local.transform_vector3(*n).normalize_or_zero().to_array(),
                    sim.asset.uvs.get(i).copied().unwrap_or([0.0, 0.0]),
                ));
            }
            for t in &sim.asset.surface_triangles {
                indices.extend_from_slice(&drawn_triangle(*t, state.mirrored));
            }
        }
        output.hidden_triangles = hidden;
        if let Some((mesh, fresh)) = publish_back_mesh(&mut state, vertices, indices) {
            #[cfg(test)]
            if fresh && let Some(arcs) = arcs.as_mut() {
                arcs.arcs += 1;
            }
            #[cfg(not(test))]
            let _ = fresh;
            renderer.mesh = mesh;
            // A signed-distance brick is baked per mesh key: never for one that rewrites.
            renderer.gi_proxy = false;
        }
    }
}

#[cfg(test)]
mod tests;
