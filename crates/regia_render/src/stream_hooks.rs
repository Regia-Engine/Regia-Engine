// SPDX-License-Identifier: MPL-2.0
//! The renderer's side of world streaming (`docs/specs/78-world-streaming.md` §§ 7.1, 10.3, 12.2).
//! Rendering depends on `regia_scene`'s streaming types, never on the runtime crate.
//!
//! - The active camera is published as [`PrimaryStreamView`], the implicit source of a streamed
//!   level with no enabled source.
//! - The `render` gate marks a cell once its members' meshes resolved. A cell whose mesh references
//!   nothing resolves stays `sim_ready`, and `stream.cells` names this gate. With
//!   [`StreamRenderResidency`] on (ADR-2317, off by default) it also waits until the renderer drew
//!   the members once: their meshes are in its cache with the writes submitted and their materials
//!   (with their textures) are bound, so `visible` never comes before the content is on the GPU.
//!   Additive levels share world cell coordinates. An unresolved mesh and a residency wait are
//!   keyed by level and cell, so one level cannot hold or spend the wait of another.
//! - A rebase shifts the camera and draw history, so temporal passes see no movement.
//! - HLOD proxies load their cooked meshes off the main thread and draw with their cooked flat color,
//!   or with their baked atlas material when the cook made one (ADR-2483); an HLOD cross-fade
//!   (`HlodFade`) draws as the LOD dither.
//! - HLOD voxel proxies of the terrain scatter (`HlodProxyVoxels`, ADR-3253) load their cooked
//!   `VoxelFarProxy` the same way and draw its finest level as voxel boxes, shaded by vertex color.
//! - Proxies cast no shadow unless [`HlodProxyShadows`] is on (off by default); on, each proxy casts
//!   into the far sun cascades its own distance picks, per tier.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use regia_app::prelude::*;
use regia_app::{PostUpdate, PostUpdateSet};
use regia_asset::{AssetId, AssetServer};
use regia_scene::GlobalTransform;
use regia_scene::level::{LevelId, LevelMember};
use regia_scene::stream_grid::CellCoord;
use regia_scene::streaming::{
    CellMember, GateId, HlodFade, HlodProxyMesh, HlodProxyResidency, HlodProxyVoxels, HlodVoxelResidency,
    OriginOffset, OriginShifted, PrimaryStreamView, Readiness, StreamGates, StreamSet,
};

use crate::cooked_model::{load_cooked_material, load_cooked_mesh};
use crate::primitives::Primitive;
use crate::runtime::{
    CpuMaterial, CpuMesh, MeshLodFade, MeshRenderer, MeshSource, NotShadowCaster, RuntimeRenderer,
};

/// The `render` gate's id.
#[derive(Resource, Debug, Clone, Copy)]
pub(crate) struct RenderStreamGate(pub(crate) GateId);

/// Whether the streaming `render` gate waits for GPU residency (spec 78 § 10.3, ADR-2317).
///
/// Off (the default), a cell is `visible` once its members' meshes resolved on the CPU. On, the
/// gate also waits until the runtime renderer has every shown member's mesh in its cache with the
/// writes submitted, and its material bound (which uploads the material's textures). Members an
/// HLOD proxy hides are not waited for: they are not drawn, so they cannot upload, and the cell's
/// proxy covers them. Without a render device, or while no camera view draws, there is nothing to
/// wait for and the gate behaves as when off.
///
/// ```
/// use regia_render::StreamRenderResidency;
/// let residency = StreamRenderResidency { enabled: true, ..Default::default() };
/// assert!(residency.enabled);
/// assert_eq!(residency.waits, 0);
/// ```
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StreamRenderResidency {
    /// Wait for residency.
    pub enabled: bool,
    /// Gate runs that held at least one cell back for residency (a counter, never reset).
    pub waits: u64,
    /// Cells held back by the last gate run.
    pub waiting_cells: u32,
    /// Cells marked after waiting `RESIDENCY_WAIT_CAP` gate runs without becoming resident (a
    /// counter, never reset; above 0 it names a member the renderer never draws).
    pub timeouts: u64,
}

/// Adds the hooks (part of [`crate::RenderPlugin`]).
pub(crate) fn add_to_app(app: &mut App) {
    regia_scene::streaming::add_to_app(app);
    let gate = app
        .world_mut()
        .get_resource_or_init::<StreamGates>()
        .register("render", Readiness::Visible);
    app.insert_resource(RenderStreamGate(gate));
    app.init_resource::<ProxyLoads>();
    app.init_resource::<VoxelProxyLoads>();
    app.init_resource::<HlodProxyShadows>();
    app.init_resource::<StreamRenderResidency>();
    app.add_systems(
        PostUpdate,
        (
            publish_primary_view,
            mark_render_gate,
            shift_camera_history,
            load_proxies,
            load_voxel_proxies,
            apply_proxy_shadows.after(load_proxies).after(load_voxel_proxies),
        )
            .after(PostUpdateSet::TransformPropagate),
    );
    app.add_systems(PostUpdate, apply_hlod_fade.after(StreamSet::Hlod));
}

/// The camera that draws first is the primary view.
fn publish_primary_view(
    cameras: Query<(Entity, &crate::Camera, &GlobalTransform)>,
    mut primary: ResMut<PrimaryStreamView>,
) {
    let view = pick_primary_view(cameras.iter());
    if primary.view != view {
        primary.view = view;
    }
}

/// The primary view among `cameras`: the lowest [`crate::Camera::order`]; a tie keeps the
/// entity spawned first (`index_u32`: an entity keeps its index when it moves tables, and a
/// replay restores spawn order). Without the tie-break the pick followed query (archetype
/// and row) order: adding a component to the winning camera moved it to another table and
/// the stream origin jumped to the other camera (audit fix-render4 #5).
fn pick_primary_view<'a>(
    cameras: impl Iterator<Item = (Entity, &'a crate::Camera, &'a GlobalTransform)>,
) -> Option<([f32; 3], [f32; 3])> {
    cameras
        .min_by_key(|(entity, camera, _)| (camera.order, entity.index_u32()))
        .map(|(_, _, global)| {
            let forward = global.0.matrix3 * regia_core::math::Vec3A::NEG_Z;
            (
                regia_core::math::Vec3::from(global.0.translation).to_array(),
                regia_core::math::Vec3::from(forward).to_array(),
            )
        })
}

/// A mesh reference that has no [`MeshRenderer`] yet.
type UnresolvedMesh = (Or<(With<MeshSource>, With<Primitive>)>, Without<MeshRenderer>);

/// A cell member the residency check reads: its level, its cell, its mesh and material.
type ShownMember = (
    Entity,
    &'static CellMember,
    &'static LevelMember,
    &'static MeshRenderer,
    Has<regia_scene::streaming::CellRenderHidden>,
    Has<regia_scene::layers::LayerParked>,
);

/// Gate runs a cell may wait for residency before it is marked anyway (about two seconds at 60
/// frames per second): a member the renderer never draws must not hold a cell back forever.
pub(crate) const RESIDENCY_WAIT_CAP: u32 = 120;

/// What the residency check reads of the renderer: the last extracted frame, the runtime renderer's
/// caches and whether a device draws.
type RendererState<'w> = (
    Option<Res<'w, crate::runtime::ExtractedFrame>>,
    Option<Res<'w, RuntimeRenderer>>,
    (
        Option<Res<'w, crate::RenderDevice>>,
        Option<Res<'w, crate::GameFrameWarm>>,
    ),
);

/// Marks every waiting cell whose mesh references all resolved and, with [`StreamRenderResidency`]
/// on, whose shown members the renderer drew.
fn mark_render_gate(
    (gate, mut gates): (Res<RenderStreamGate>, ResMut<StreamGates>),
    pending: Query<(&CellMember, &LevelMember), UnresolvedMesh>,
    mut residency: ResMut<StreamRenderResidency>,
    members: Query<ShownMember>,
    (extracted, runtime, (device, warm)): RendererState,
    mut held_runs: Local<std::collections::BTreeMap<(LevelId, CellCoord), u32>>,
) {
    let waiting: Vec<_> = gates.awaiting(gate.0).collect();
    if waiting.is_empty() {
        if residency.waiting_cells != 0 {
            residency.waiting_cells = 0;
        }
        held_runs.clear();
        return;
    }
    let _span = tracing::trace_span!("stream_render_gate", cells = waiting.len()).entered();
    // Level and cell. A coordinate alone would let level A's unresolved mesh skip every
    // other level's cell at that address (audit fix-render4 finding 1).
    let unresolved: BTreeSet<(LevelId, CellCoord)> = pending
        .iter()
        .map(|(member, level)| (level.level(), member.0))
        .collect();
    // While a game-frame warm runs, the runtime renderer is a stand-in with empty caches: hold the
    // cells without counting the runs against the cap.
    if residency.enabled && warm.as_deref().is_some_and(crate::GameFrameWarm::is_warming) {
        return;
    }
    // Residency only means something while a device draws views.
    let drawn = extracted
        .as_deref()
        .filter(|frame| residency.enabled && device.is_some() && frame.is_renderable())
        .zip(runtime.as_deref());
    let not_resident: BTreeSet<(LevelId, CellCoord)> = match drawn {
        Some((frame, runtime)) => {
            let cells: BTreeSet<(LevelId, CellCoord)> = waiting
                .iter()
                .copied()
                .filter(|key| !unresolved.contains(key))
                .collect();
            cells_not_resident(&cells, &members, frame, &runtime.0)
        }
        None => BTreeSet::new(),
    };
    let mut held = 0u32;
    // Drop a level's counter on any run where that level's own members are resident.
    // Keeping it by coordinate alone charged one level for another's wait (finding 2).
    held_runs.retain(|key, _| not_resident.contains(key));
    for (level, cell) in waiting {
        if unresolved.contains(&(level, cell)) {
            continue;
        }
        if not_resident.contains(&(level, cell)) {
            let runs = held_runs.entry((level, cell)).or_insert(0);
            *runs += 1;
            if *runs <= RESIDENCY_WAIT_CAP {
                held += 1;
                continue;
            }
            held_runs.remove(&(level, cell));
            residency.timeouts += 1;
            tracing::warn!(
                target: "regia_render",
                "stream render gate: cell {} not resident after {RESIDENCY_WAIT_CAP} runs; marked visible",
                cell.label()
            );
        }
        gates.set_ready(gate.0, level, cell);
    }
    if held > 0 {
        residency.waits += 1;
    }
    if residency.waiting_cells != held {
        residency.waiting_cells = held;
    }
}

/// The level-cells of `cells` with a shown member the renderer has not drawn yet: its mesh (as the
/// last extracted frame keyed it, LOD variant included) or its material is not in the renderer's
/// caches. A shown member missing from the last extracted frame spawned after it, and waits too.
/// The key is the member's level and cell, not the coordinate alone.
fn cells_not_resident(
    cells: &BTreeSet<(LevelId, CellCoord)>,
    members: &Query<ShownMember>,
    frame: &crate::runtime::ExtractedFrame,
    renderer: &crate::FrameRenderer,
) -> BTreeSet<(LevelId, CellCoord)> {
    let mut out = BTreeSet::new();
    let mut shown: std::collections::HashMap<Entity, (LevelId, CellCoord)> = std::collections::HashMap::new();
    for (entity, member, level, _, hidden, parked) in members.iter() {
        let key = (level.level(), member.0);
        if !hidden && !parked && cells.contains(&key) {
            shown.insert(entity, key);
        }
    }
    if shown.is_empty() {
        return out;
    }
    let mut seen: std::collections::HashSet<Entity> = std::collections::HashSet::new();
    for mesh in &frame.meshes {
        let Some(cell) = shown.get(&mesh.entity).copied() else {
            continue;
        };
        seen.insert(mesh.entity);
        let key = crate::mesh_lod::lod_variant_key(crate::runtime::mesh_key(&mesh.mesh), mesh.lod_key);
        let material = crate::runtime::material_key(&mesh.material);
        if !renderer.mesh_resident(key) || !renderer.material_resident(material) {
            out.insert(cell);
        }
    }
    for (entity, cell) in shown {
        if !seen.contains(&entity) {
            out.insert(cell);
        }
    }
    out
}

/// Finished proxy loads, delivered from the compute pool.
#[derive(Resource, Default)]
pub(crate) struct ProxyLoads(Arc<Mutex<Vec<LoadedProxy>>>);

/// A finished proxy load: the entity, the tree node, its flat color, the mesh, if it read, and its
/// baked atlas material, if it has one and it read (ADR-2483).
type LoadedProxy = (
    Entity,
    u32,
    Option<[f32; 4]>,
    Option<Arc<CpuMesh>>,
    Option<CpuMaterial>,
);

/// Orders finished proxy loads for insertion. IO completions arrive in finish order, which
/// timing chooses; archetype row order follows insertion order, and draw order follows the
/// extracted list, so the insertion order must not come out of the pool (audit
/// fix-render4 #6).
fn sort_loaded_proxies(loaded: &mut [LoadedProxy]) {
    loaded.sort_by_key(|(entity, ..)| entity.index_u32());
}

/// Same insertion order as [`sort_loaded_proxies`]: IO finish order must not choose the draw.
fn sort_loaded_voxels(loaded: &mut [LoadedVoxelProxy]) {
    loaded.sort_by_key(|(entity, ..)| entity.index_u32());
}

/// Loads each new proxy's cooked mesh off the main thread, draws it with the default material and
/// marks it resident. A proxy that does not read logs and stays non-resident: the cut goes around it.
fn load_proxies(
    added: Query<(Entity, &HlodProxyMesh), Added<HlodProxyMesh>>,
    live: Query<&HlodProxyMesh>,
    assets: Option<Res<AssetServer>>,
    loads: Res<ProxyLoads>,
    mut residency: ResMut<HlodProxyResidency>,
    mut commands: Commands,
) {
    if let Some(assets) = assets {
        for (entity, proxy) in &added {
            let reader = assets.payload_reader();
            let (done, id, node, color) =
                (Arc::clone(&loads.0), AssetId(proxy.mesh), proxy.node, proxy.color);
            let atlas = proxy.material.map(AssetId);
            // The IO pool: the load is a blocking file read, which on the 1-2 thread async pool of
            // a small CPU would hold DDGI bakes and scatter placements behind it
            // (audit-v035-conc L4).
            regia_core::tasks::spawn_io(async move {
                // The baked atlas material, if any; a proxy whose material does not read draws flat.
                let material = atlas.and_then(|mid| match load_cooked_material(&reader, mid) {
                    Ok(material) => Some(material),
                    Err(error) => {
                        tracing::warn!(target: "regia_render", "HLOD proxy {node} atlas: {error}; drawn flat");
                        None
                    }
                });
                let mesh = match load_cooked_mesh(&reader, id) {
                    Ok(mesh) => Some(Arc::new(CpuMesh {
                        vertices: mesh.vertices,
                        indices: mesh.indices,
                    })),
                    Err(error) => {
                        tracing::warn!(target: "regia_render", "HLOD proxy {node}: {error}");
                        None
                    }
                };
                done.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((entity, node, color, mesh, material));
            })
            .detach();
        }
    }
    let mut finished = std::mem::take(&mut *loads.0.lock().unwrap_or_else(PoisonError::into_inner));
    sort_loaded_proxies(&mut finished);
    for (entity, node, color, mesh, atlas) in finished {
        // The runtime may have evicted the node while its read was in flight (despawn plus an
        // unmark): a late load must not mark a proxy resident that nothing draws.
        if !live.get(entity).is_ok_and(|proxy| proxy.node == node) {
            continue;
        }
        if let Some(mesh) = mesh {
            let material = Arc::new(atlas.unwrap_or_else(|| proxy_material(color)));
            commands
                .entity(entity)
                .try_insert((MeshRenderer::new(mesh, material), NotShadowCaster));
            residency.resident.insert(node);
            proxy_shadow_pending(&mut commands, entity);
        }
    }
}

/// Finished voxel proxy loads (ADR-3253): the entity, the tree node and the drawn surface, if the
/// proxy read.
#[derive(Resource, Default)]
pub(crate) struct VoxelProxyLoads(Arc<Mutex<Vec<LoadedVoxelProxy>>>);

/// A finished voxel proxy load: the entity, the tree node and the drawn surface, if it read.
type LoadedVoxelProxy = (Entity, u32, Option<Arc<CpuMesh>>);

/// Loads each new voxel proxy off the main thread and draws its finest level as voxel boxes with
/// the default material (the voxels carry their color). One that does not read logs and stays
/// non-resident: the cut goes around its node.
fn load_voxel_proxies(
    added: Query<(Entity, &HlodProxyVoxels), Added<HlodProxyVoxels>>,
    live: Query<&HlodProxyVoxels>,
    assets: Option<Res<AssetServer>>,
    loads: Res<VoxelProxyLoads>,
    mut residency: ResMut<HlodVoxelResidency>,
    mut commands: Commands,
) {
    if let Some(assets) = assets {
        for (entity, proxy) in &added {
            let reader = assets.payload_reader();
            let (done, id, node) = (Arc::clone(&loads.0), AssetId(proxy.proxy), proxy.node);
            regia_core::tasks::spawn_io(async move {
                let mesh = match reader.read(id) {
                    Ok((bytes, _)) => match crate::visbuffer::VoxelFarProxy::from_cooked_file(&bytes) {
                        Ok(voxels) => hlod_voxel_mesh(&voxels).map(Arc::new),
                        Err(error) => {
                            tracing::warn!(target: "regia_render", "HLOD voxel proxy {node}: {error}");
                            None
                        }
                    },
                    Err(error) => {
                        tracing::warn!(target: "regia_render", "HLOD voxel proxy {node}: {error}");
                        None
                    }
                };
                done.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push((entity, node, mesh));
            })
            .detach();
        }
    }
    let mut finished = std::mem::take(&mut *loads.0.lock().unwrap_or_else(PoisonError::into_inner));
    sort_loaded_voxels(&mut finished);
    for (entity, node, mesh) in finished {
        // As for mesh proxies: an evicted node's late load marks nothing.
        if !live.get(entity).is_ok_and(|proxy| proxy.node == node) {
            continue;
        }
        if let Some(mesh) = mesh {
            commands.entity(entity).try_insert((
                MeshRenderer::new(mesh, Arc::new(CpuMaterial::default())),
                NotShadowCaster,
            ));
            residency.resident.insert(node);
            proxy_shadow_pending(&mut commands, entity);
        }
    }
}

/// The drawn surface of an HLOD voxel proxy (ADR-3253): its finest level as sub-voxel boxes (the
/// smallest silhouette error, `VoxelShape::Boxes`), every voxel kept, colored per voxel. `None` for
/// a proxy with no voxels.
#[must_use]
pub fn hlod_voxel_mesh(voxels: &crate::visbuffer::VoxelFarProxy) -> Option<CpuMesh> {
    let surface = voxels.surface_drawn(0, false, crate::visbuffer::VoxelShape::Boxes);
    (!surface.indices.is_empty()).then_some(CpuMesh {
        vertices: surface.vertices,
        indices: surface.indices,
    })
}

/// Whether HLOD proxies cast sun shadows, per render tier (ADR-3253, spec 78 § 11.7). Off by
/// default: proxy shadows change how the far field looks, so a game turns them on.
///
/// On, a proxy casts with [`crate::ShadowCast::Local`]: it is drawn only into the sun cascades
/// whose receivers its bounding sphere (grown by its own radius, the reach of its shadow) can
/// touch, at most the tier's cascade count, and the near cascades, where the full-detail cells
/// cast their own shadows, never see a far proxy. The tier is the active profile's preset;
/// a custom profile uses [`Self::custom_cascades`].
///
/// ```
/// use regia_render::{HlodProxyShadows, RenderProfilePreset};
/// let shadows = HlodProxyShadows { enabled: true, ..Default::default() };
/// assert_eq!(shadows.cascades(Some(RenderProfilePreset::DesktopUltra)), 2);
/// assert_eq!(shadows.cascades(Some(RenderProfilePreset::MobileLow)), 0);
/// assert_eq!(HlodProxyShadows::default().cascades(Some(RenderProfilePreset::DesktopUltra)), 0);
/// ```
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct HlodProxyShadows {
    /// The master switch (default off).
    pub enabled: bool,
    /// Most sun cascades a proxy draws into on a profile that is no preset.
    pub custom_cascades: u8,
}

impl Default for HlodProxyShadows {
    fn default() -> Self {
        Self {
            enabled: false,
            custom_cascades: 1,
        }
    }
}

impl HlodProxyShadows {
    /// The per-tier table: most sun cascades a proxy draws into (0: no proxy shadow). Desktop High,
    /// Desktop Ultra and Cinematic 2, Desktop Low and the Deck 1, the mobile tiers 0 (their one or
    /// two cascades end before the proxies begin).
    #[must_use]
    pub fn tier_cascades(preset: crate::RenderProfilePreset) -> u8 {
        use crate::RenderProfilePreset as P;
        match preset {
            P::DesktopHigh | P::DesktopUltra | P::Cinematic => 2,
            P::DesktopLow | P::Deck => 1,
            P::MobileLow | P::MobileHigh => 0,
        }
    }

    /// Cascades a proxy draws into on `preset` (`None`: a custom profile); 0 when off.
    #[must_use]
    pub fn cascades(&self, preset: Option<crate::RenderProfilePreset>) -> u8 {
        if !self.enabled {
            return 0;
        }
        preset.map_or(self.custom_cascades, Self::tier_cascades)
    }
}

/// How a proxy of bounding radius `radius_m` casts with at most `cascades` sun cascades
/// (ADR-3253): [`crate::ShadowCast::Off`] at 0, else [`crate::ShadowCast::Local`] with its own
/// radius as the reach of its shadow, drawn as is.
#[must_use]
pub fn hlod_proxy_shadow_cast(radius_m: f32, cascades: u8) -> crate::ShadowCast {
    if cascades == 0 || !radius_m.is_finite() {
        return crate::ShadowCast::Off;
    }
    crate::ShadowCast::Local {
        reach: radius_m.max(0.0),
        max_cascades: cascades,
        shadow_scale: 1.0,
        shadow_lift: 0.0,
        anchor: None,
        lod_switch_keeps_pages: false,
    }
}

/// Marks a proxy whose mesh just arrived for [`apply_proxy_shadows`].
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct ProxyShadowPending;

fn proxy_shadow_pending(commands: &mut Commands, entity: Entity) {
    commands.entity(entity).try_insert(ProxyShadowPending);
}

/// A proxy entity the shadow switch reads: its mesh and whether it is new.
type ProxyShadowItem = (Entity, &'static MeshRenderer, Has<ProxyShadowPending>);

/// An HLOD proxy entity of either kind.
type AnyHlodProxy = Or<(With<HlodProxyMesh>, With<HlodProxyVoxels>)>;

/// Gives every loaded proxy its shadow mode: [`NotShadowCaster`] while proxy shadows are off for the
/// tier, [`crate::MeshShadowCast`] with [`hlod_proxy_shadow_cast`] when on. Runs for new proxies, and
/// for all of them when the switch or the profile changes.
fn apply_proxy_shadows(
    shadows: Res<HlodProxyShadows>,
    settings: Option<Res<crate::RenderSettings>>,
    proxies: Query<ProxyShadowItem, AnyHlodProxy>,
    mut last: Local<Option<u8>>,
    mut commands: Commands,
) {
    let cascades = shadows.cascades(settings.as_deref().and_then(|s| s.preset()));
    let all = *last != Some(cascades);
    *last = Some(cascades);
    for (entity, renderer, pending) in &proxies {
        if !(all || pending) {
            continue;
        }
        let mut entity = commands.entity(entity);
        entity.try_remove::<ProxyShadowPending>();
        // Zero cascades is `ShadowCast::Off` for every radius, including a non-finite one.
        // The default (and every preset while the switch is off) must not walk the vertices.
        if cascades == 0 {
            entity.try_remove::<crate::MeshShadowCast>();
            entity.try_insert(NotShadowCaster);
            continue;
        }
        let cast = hlod_proxy_shadow_cast(mesh_radius(&renderer.mesh), cascades);
        if cast == crate::ShadowCast::Off {
            entity.try_remove::<crate::MeshShadowCast>();
            entity.try_insert(NotShadowCaster);
        } else {
            entity.try_remove::<NotShadowCaster>();
            entity.try_insert(crate::MeshShadowCast(cast));
        }
    }
}

#[cfg(test)]
thread_local! {
    /// Vertices [`mesh_radius`] walked on this thread. A test resets it before a system run.
    static MESH_RADIUS_VERTEX_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Radius of the sphere about the mesh's box center that holds every vertex, meters.
fn mesh_radius(mesh: &CpuMesh) -> f32 {
    #[cfg(test)]
    MESH_RADIUS_VERTEX_READS.with(|reads| {
        reads.set(reads.get().saturating_add(mesh.vertices.len() as u64));
    });
    let Some(first) = mesh.vertices.first() else {
        return 0.0;
    };
    let (mut lo, mut hi) = (first.position, first.position);
    for v in &mesh.vertices {
        for k in 0..3 {
            lo[k] = lo[k].min(v.position[k]);
            hi[k] = hi[k].max(v.position[k]);
        }
    }
    let c: [f32; 3] = std::array::from_fn(|k| (lo[k] + hi[k]) * 0.5);
    mesh.vertices
        .iter()
        .map(|v| {
            let d: [f32; 3] = std::array::from_fn(|k| v.position[k] - c[k]);
            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
        })
        .fold(0.0, f32::max)
}

/// A proxy's material: its cooked flat color on the default material, or the default material
/// when the cook knew no color (spec 78 § 11.1, ADR-2022).
pub(crate) fn proxy_material(color: Option<[f32; 4]>) -> CpuMaterial {
    let mut material = CpuMaterial::default();
    if let Some(color) = color.filter(|c| c.iter().all(|v| v.is_finite())) {
        material.base_color = color.map(|v| v.clamp(0.0, 1.0));
    }
    material
}

/// Draws an HLOD cross-fade as the LOD dither (ADR-2022): the runtime's `HlodFade` on a proxy or a
/// cell member becomes its `MeshLodFade`, and goes with it.
fn apply_hlod_fade(
    changed: Query<(Entity, &HlodFade), Changed<HlodFade>>,
    mut removed: RemovedComponents<HlodFade>,
    mut commands: Commands,
) {
    for entity in removed.read() {
        commands.entity(entity).try_remove::<MeshLodFade>();
    }
    for (entity, fade) in &changed {
        commands.entity(entity).try_insert(MeshLodFade(fade.0));
    }
}

/// Keeps the renderer's history in the shifted space.
fn shift_camera_history(
    mut shifted: MessageReader<OriginShifted>,
    mut runtime: ResMut<RuntimeRenderer>,
    clouds: Option<ResMut<crate::CloudSettings>>,
    origin: Option<Res<OriginOffset>>,
) {
    let mut clouds = clouds;
    for message in shifted.read() {
        runtime.0.shift_history(message.rebase.shift);
        if let Some(clouds) = clouds.as_deref_mut() {
            shift_cloud_drift(clouds, message.rebase.shift);
        }
    }
    // The world's origin is the authority (a renderer can be newer than the last rebase).
    if let Some(origin) = origin {
        let offset = origin.0.map(|axis| axis as f32);
        if runtime.0.origin_offset() != offset {
            runtime.0.set_origin_offset(offset);
        }
    }
}

/// Keeps the clouds over the same ground: they sample the weather at `xz - drift`, and every
/// local `xz` went down by the shift, so the drift goes down by it too (spec 78 § 12.2).
pub(crate) fn shift_cloud_drift(clouds: &mut crate::CloudSettings, shift: [f32; 3]) {
    clouds.drift_offset_m[0] -= shift[0];
    clouds.drift_offset_m[1] -= shift[2];
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_ecs::ecs::system::RunSystemOnce;

    /// Audit fix-render4 finding 5 (medium): two cameras at the default order 0 keep the
    /// lower entity id as the stream origin, whichever order the query visits them in; a
    /// lower `order` still wins over the entity id.
    #[test]
    fn primary_view_breaks_order_ties_by_entity_in_either_visit_order() {
        let at = |x: f32| {
            GlobalTransform(regia_core::math::Affine3A::from_translation(
                regia_core::math::Vec3::new(x, 0.0, 0.0),
            ))
        };
        let mut world = World::new();
        let first = world.spawn((crate::Camera::default(), at(1.0))).id();
        let second = world.spawn((crate::Camera::default(), at(2.0))).id();
        let want = Some(([1.0, 0.0, 0.0], [0.0, 0.0, -1.0]));
        let mut query = world.query::<(Entity, &crate::Camera, &GlobalTransform)>();
        assert_eq!(pick_primary_view(query.iter(&world)), want);
        // The same rows as a query visiting the other table first (a component move or a
        // replay restoring the cameras in the other order): same origin.
        let rows: Vec<(Entity, &crate::Camera, &GlobalTransform)> = query.iter(&world).collect();
        assert_eq!(
            pick_primary_view(rows.into_iter().rev()),
            want,
            "visit order does not choose the stream origin"
        );
        let _ = (first, second);
        // A lower `order` still wins over the entity id.
        world.entity_mut(second).insert(crate::Camera {
            order: -1,
            ..crate::Camera::default()
        });
        let mut query = world.query::<(Entity, &crate::Camera, &GlobalTransform)>();
        assert_eq!(
            pick_primary_view(query.iter(&world)),
            Some(([2.0, 0.0, 0.0], [0.0, 0.0, -1.0]))
        );
    }

    /// Audit fix-render4 finding 6 (low): proxy loads insert in entity order, whichever
    /// order the IO pool finished them in.
    /// Audit audfix-render-3-g: voxel proxy loads are the same insertion as mesh proxies
    /// (archetype row order follows command order, and draw order follows the extracted
    /// list). IO finish order must not choose it. Mesh proxies sort; this path did not.
    #[test]
    fn voxel_proxy_loads_insert_in_entity_order_either_completion_order() {
        let mut app = App::new();
        app.init_resource::<VoxelProxyLoads>();
        app.init_resource::<HlodVoxelResidency>();
        // Proxy entities as the runtime spawns them: each carries its node.
        let e1 = app.world_mut().spawn(voxel_proxy(1)).id();
        let e2 = app.world_mut().spawn(voxel_proxy(2)).id();
        let e3 = app.world_mut().spawn(voxel_proxy(3)).id();
        let mesh = Arc::new(CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        {
            let loads = app.world().resource::<VoxelProxyLoads>();
            let mut queue = loads.0.lock().unwrap_or_else(PoisonError::into_inner);
            queue.extend([
                (e3, 3, Some(Arc::clone(&mesh))),
                (e1, 1, Some(Arc::clone(&mesh))),
                (e2, 2, Some(Arc::clone(&mesh))),
            ]);
        }
        app.add_systems(Update, load_voxel_proxies);
        app.update();
        let mut query = app.world_mut().query::<(Entity, &MeshRenderer)>();
        let got: Vec<Entity> = query.iter(app.world()).map(|(entity, _)| entity).collect();
        assert_eq!(
            got,
            vec![e1, e2, e3],
            "voxel proxies inserted in IO order {got:?}, want entity order {e1:?} {e2:?} {e3:?}"
        );
        let resident = app.world().resource::<HlodVoxelResidency>();
        assert_eq!(
            resident.resident.iter().copied().collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    fn voxel_proxy(node: u32) -> HlodProxyVoxels {
        HlodProxyVoxels {
            node,
            proxy: regia_core::id::Guid::from_u128(u128::from(node) + 1),
            bytes: 0,
            voxel_size_m: 1.0,
        }
    }

    fn mesh_proxy(node: u32) -> HlodProxyMesh {
        HlodProxyMesh {
            node,
            mesh: regia_core::id::Guid::from_u128(u128::from(node) + 1),
            bytes: 0,
            color: None,
            material: None,
        }
    }

    /// deep-zai-terrain-stream #8: a proxy evicted while its read was in flight (entity despawned,
    /// node unmarked) is not marked resident when the read completes; a live one still is.
    #[test]
    fn a_late_load_of_an_evicted_proxy_marks_nothing_resident() {
        let mut app = App::new();
        app.init_resource::<ProxyLoads>();
        app.init_resource::<HlodProxyResidency>();
        app.init_resource::<VoxelProxyLoads>();
        app.init_resource::<HlodVoxelResidency>();
        let live_mesh = app.world_mut().spawn(mesh_proxy(1)).id();
        let gone_mesh = app.world_mut().spawn(mesh_proxy(2)).id();
        let live_voxels = app.world_mut().spawn(voxel_proxy(3)).id();
        let gone_voxels = app.world_mut().spawn(voxel_proxy(4)).id();
        app.world_mut().despawn(gone_mesh);
        app.world_mut().despawn(gone_voxels);
        let mesh = Arc::new(CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        {
            let loads = app.world().resource::<ProxyLoads>();
            let mut queue = loads.0.lock().unwrap_or_else(PoisonError::into_inner);
            queue.extend([
                (live_mesh, 1, None, Some(Arc::clone(&mesh)), None),
                (gone_mesh, 2, None, Some(Arc::clone(&mesh)), None),
            ]);
        }
        {
            let loads = app.world().resource::<VoxelProxyLoads>();
            let mut queue = loads.0.lock().unwrap_or_else(PoisonError::into_inner);
            queue.extend([
                (live_voxels, 3, Some(Arc::clone(&mesh))),
                (gone_voxels, 4, Some(Arc::clone(&mesh))),
            ]);
        }
        app.add_systems(Update, (load_proxies, load_voxel_proxies));
        app.update();
        let meshes = app.world().resource::<HlodProxyResidency>();
        assert_eq!(meshes.resident.iter().copied().collect::<Vec<_>>(), vec![1]);
        let voxels = app.world().resource::<HlodVoxelResidency>();
        assert_eq!(voxels.resident.iter().copied().collect::<Vec<_>>(), vec![3]);
    }

    #[test]
    fn loaded_proxies_insert_in_entity_order_either_completion_order() {
        let mut world = World::new();
        let e1 = world.spawn_empty().id();
        let e2 = world.spawn_empty().id();
        let e3 = world.spawn_empty().id();
        let mut loaded: Vec<LoadedProxy> = vec![
            (e3, 3, None, None, None),
            (e1, 1, None, None, None),
            (e2, 2, None, None, None),
        ];
        sort_loaded_proxies(&mut loaded);
        assert_eq!(
            loaded.iter().map(|(entity, ..)| *entity).collect::<Vec<_>>(),
            vec![e1, e2, e3]
        );
        // The reverse completion order lands the same insertion order.
        let mut loaded: Vec<LoadedProxy> = vec![
            (e2, 2, None, None, None),
            (e1, 1, None, None, None),
            (e3, 3, None, None, None),
        ];
        sort_loaded_proxies(&mut loaded);
        assert_eq!(
            loaded.iter().map(|(entity, ..)| *entity).collect::<Vec<_>>(),
            vec![e1, e2, e3]
        );
    }

    #[test]
    fn clouds_stay_over_the_same_ground_across_a_rebase() {
        let mut clouds = crate::CloudSettings {
            time_s: 37.0,
            ..crate::CloudSettings::default()
        };
        let world = [4100.0f32, 3050.0];
        let before = clouds.drift_m();
        let sample_before = [world[0] - before[0], world[1] - before[1]];
        let shift = [4096.0, 0.0, 3072.0];
        shift_cloud_drift(&mut clouds, shift);
        let after = clouds.drift_m();
        let local = [world[0] - shift[0], world[1] - shift[2]];
        let sample_after = [local[0] - after[0], local[1] - after[1]];
        for (a, b) in sample_before.iter().zip(sample_after) {
            assert!((a - b).abs() < 1.0e-3, "{sample_before:?} vs {sample_after:?}");
        }
    }

    #[test]
    fn proxy_shadows_are_off_by_default_and_follow_the_tier_when_on() {
        use crate::RenderProfilePreset as P;
        let off = HlodProxyShadows::default();
        for preset in P::WITH_OFFLINE {
            assert_eq!(off.cascades(Some(preset)), 0, "{preset:?}");
        }
        assert_eq!(off.cascades(None), 0);
        let on = HlodProxyShadows {
            enabled: true,
            custom_cascades: 3,
        };
        assert_eq!(on.cascades(Some(P::DesktopHigh)), 2);
        assert_eq!(on.cascades(Some(P::Deck)), 1);
        assert_eq!(on.cascades(Some(P::MobileHigh)), 0);
        assert_eq!(on.cascades(None), 3);
        // Off is `ShadowCast::Off`; on is a bounded caster whose reach is the proxy's radius.
        assert_eq!(hlod_proxy_shadow_cast(50.0, 0), crate::ShadowCast::Off);
        assert_eq!(hlod_proxy_shadow_cast(f32::NAN, 2), crate::ShadowCast::Off);
        match hlod_proxy_shadow_cast(50.0, 2) {
            crate::ShadowCast::Local {
                reach,
                max_cascades,
                shadow_scale,
                anchor,
                lod_switch_keeps_pages,
                ..
            } => {
                assert_eq!((reach, max_cascades, shadow_scale), (50.0, 2, 1.0));
                assert!(anchor.is_none() && !lod_switch_keeps_pages);
            }
            other => panic!("{other:?}"),
        }
    }

    /// Proxy shadows are off on every preset. Applying that must not walk proxy vertices.
    #[test]
    fn proxy_shadows_off_read_no_vertices() {
        let (vertices, indices) = crate::uv_sphere(3.0, 12, 8);
        let vertex_count = vertices.len() as u64;
        let mesh = std::sync::Arc::new(super::CpuMesh { vertices, indices });
        let mut world = super::World::new();
        world.insert_resource(HlodProxyShadows::default());
        world.spawn((
            super::HlodProxyMesh {
                node: 1,
                mesh: regia_core::id::Guid(0),
                bytes: 0,
                color: None,
                material: None,
            },
            super::MeshRenderer::new(mesh, std::sync::Arc::new(super::CpuMaterial::default())),
            super::ProxyShadowPending,
        ));
        MESH_RADIUS_VERTEX_READS.with(|reads| reads.set(0));
        world
            .run_system_once(apply_proxy_shadows)
            .expect("apply_proxy_shadows runs");
        let reads = MESH_RADIUS_VERTEX_READS.with(|reads| reads.get());
        assert_eq!(
            reads, 0,
            "proxy shadows off walked {reads} vertices of a {vertex_count}-vertex proxy"
        );
        assert_eq!(
            world.query::<&super::ProxyShadowPending>().iter(&world).count(),
            0
        );
        assert_eq!(world.query::<&super::NotShadowCaster>().iter(&world).count(), 1);
        assert_eq!(world.query::<&crate::MeshShadowCast>().iter(&world).count(), 0);
    }

    #[test]
    fn a_proxy_mesh_radius_holds_every_vertex() {
        let (vertices, indices) = crate::uv_sphere(3.0, 8, 6);
        let mesh = CpuMesh { vertices, indices };
        let r = mesh_radius(&mesh);
        assert!((r - 3.0).abs() < 0.05, "{r}");
        let empty = CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        };
        assert_eq!(mesh_radius(&empty), 0.0);
    }

    #[test]
    fn a_proxy_draws_its_cooked_color_or_the_default_material() {
        let default = CpuMaterial::default();
        assert_eq!(proxy_material(None).base_color, default.base_color);
        let tinted = proxy_material(Some([0.25, 0.5, 0.75, 1.0]));
        assert_eq!(tinted.base_color, [0.25, 0.5, 0.75, 1.0]);
        assert_eq!(tinted.roughness, default.roughness);
        assert_eq!(tinted.metallic, default.metallic);
        // A bad color never reaches the GPU.
        assert_eq!(
            proxy_material(Some([f32::NAN, 0.0, 0.0, 1.0])).base_color,
            default.base_color
        );
        assert_eq!(
            proxy_material(Some([2.0, -1.0, 0.5, 1.0])).base_color,
            [1.0, 0.0, 0.5, 1.0]
        );
    }
}
