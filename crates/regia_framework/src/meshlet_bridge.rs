// SPDX-License-Identifier: MPL-2.0
//! Cooked meshlets reach the frame from the ECS (ADR-354, `book/agents/visbuffer.md`,
//! `docs/specs/27` § 4.2 stage 3).
//!
//! `regia cook` writes a `.fmlt` meshlet sub-asset next to every mesh primitive of a glTF whose
//! `.meta` says `meshlets: true` (`meshlets/<node>/primitive_<i>` beside `mesh/<node>/primitive_<i>`).
//! A host marks the entity that draws such a primitive with [`MeshletSource`] (the sibling's
//! asset id). While the active profile has [`regia_render::RenderProfile::visibility_buffer`]
//! on, [`MeshletBridgePlugin`] loads the payload through the [`AssetServer`] ([`MeshletAssets`])
//! and inserts it under the entity's mesh key (`FrameRenderer::insert_meshlets`), so the mesh
//! draws through the visibility buffer instead of the forward passes.
//!
//! - A changed mesh or source, a despawn, a removed component and a hot reload of the `.fmlt`
//!   remove the meshlets and insert them again (or not). Meshes shared by several entities stay
//!   inserted until the last one goes.
//! - A profile that turns the path off removes them all.
//! - Skinned (`SkinJoints`), morphing (`MeshMorphTargets`), translucent (blended, hair drawn
//!   blended or masked, transmissive) and material-graph meshes stay on the forward path, as the
//!   renderer's own gate does. Two kinds pass: an alpha-tested material the masked raster can test
//!   (ADR-3915), and a plain material adopted with [`regia_render::FoliageSway::adopt`] (ADR-3916):
//!   a sway on any other surface graph stays forward ([`ForwardReason::SwayGraphSurface`]). The
//!   renderer may still draw such a mesh forward for a frame (`render.get_visbuffer`
//!   `last_frame.sway.forward` says why).
//! - A lightmap receiver ([`regia_render::lightmap::AppliedLightmapReceiver`]) draws its entry's
//!   unwrap, not the mesh the payload was cooked for: its clusters come with the bake (ADR-2107,
//!   `apply_lightmap_receivers`). The bridge leaves it alone while it is one and comes back when it
//!   gets its own mesh again.
//! - A missing, damaged or old payload, or one cooked for other mesh data, logs one warning and
//!   the mesh stays on the forward path; [`MeshletBridgeState`] records why
//!   (`render.get_visbuffer`).
//!
//! **Voxel far levels** (ADR-2655, `book/agents/voxel-far.md`). An entity that also has a
//! [`VoxelFarSource`] (the `.fvox` cooked beside the meshlets, `voxels/<node>/primitive_<i>`)
//! gets the proxy's voxel levels on top of its cluster DAG
//! (`regia_render::visbuffer::VoxelFarProxy::extend_meshlets`). The voxel vertices travel inside
//! the inserted meshlets (`MeshletMesh::appended_vertices`), so the entity keeps its mesh, its
//! mesh key and its forward vertex buffer, and the payload is checked against the mesh's own
//! vertex count (`MeshletMesh::base_vertex_count`), not the extended one. A missing, damaged or
//! old `.fvox`, or one that does not extend the DAG, logs one warning and the mesh takes its plain
//! meshlets ([`MeshletBridgeState::voxel_far_fallbacks`]); a hot reload of the `.fvox` or a
//! changed or removed [`VoxelFarSource`] inserts again. [`MeshletBridgeState::voxel_far_stats`]
//! counts what the levels added (`render.voxel_far_stats`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use regia_app::prelude::*;
use regia_asset::{AssetId, AssetServer};
use regia_render::lightmap::AppliedLightmapReceiver;
use regia_render::visbuffer::{
    MeshletMesh, VoxelFarLoadError, VoxelFarProxy, VoxelFarSettings, meshlet_sub_path, sway,
    voxel_far_sub_path,
};
use regia_render::{MeshMorphTargets, MeshRenderer, RenderSettings, RuntimeRenderer, SkinJoints, mesh_key};

/// Names the cooked meshlet sub-asset (`.fmlt`) of the mesh this entity's [`MeshRenderer`]
/// draws. The meshlets index that mesh's vertices, so they must be cooked from the same mesh
/// data (LOD 0 of the cooked mesh, as `regia_render::visbuffer::cooked_mesh_lod0` decodes it);
/// a payload whose counts do not match stays unused.
///
/// # Example
///
/// ```
/// use regia_asset::AssetId;
/// use regia_framework::meshlet_bridge::MeshletSource;
///
/// let rock = AssetId::new_random();
/// let source = MeshletSource::for_mesh(rock, "mesh/Rock/primitive_0").expect("a mesh sub-asset");
/// assert_eq!(source.id, AssetId::sub_asset(rock, "meshlets/Rock/primitive_0"));
/// assert!(MeshletSource::for_mesh(rock, "model").is_none());
/// ```
#[derive(Component, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MeshletSource {
    /// The meshlet sub-asset's id.
    pub id: AssetId,
}

impl MeshletSource {
    /// The meshlet sub-asset `id`.
    #[must_use]
    pub fn new(id: AssetId) -> Self {
        Self { id }
    }

    /// The meshlets cooked for the mesh sub-asset `mesh_sub_path` (`mesh/<node>/primitive_<i>`) of
    /// the glTF `gltf`. `None` for a path that names no mesh primitive.
    #[must_use]
    pub fn for_mesh(gltf: AssetId, mesh_sub_path: &str) -> Option<Self> {
        meshlet_sub_path(mesh_sub_path).map(|path| Self::new(AssetId::sub_asset(gltf, &path)))
    }

    /// The meshlets cooked for the mesh at the manifest path `mesh_path`
    /// (`assets/rock.gltf#mesh/Rock/primitive_0`), looked up in `server`'s manifest. `None` when
    /// the path names no mesh primitive or nothing was cooked for it.
    #[must_use]
    pub fn for_mesh_path(server: &AssetServer, mesh_path: &str) -> Option<Self> {
        let (file, sub) = mesh_path.split_once('#')?;
        let sub = meshlet_sub_path(sub)?;
        server.id_for_path(&format!("{file}#{sub}")).map(Self::new)
    }
}

/// Names the cooked voxel far proxy (`.fvox`, ADR-2445 and ADR-2655) of the mesh this entity's
/// [`MeshRenderer`] draws, beside its [`MeshletSource`]: the bridge stacks the proxy's voxel
/// levels on the meshlets' DAG, so the placed mesh draws voxels past its last cluster LOD.
/// Without a [`MeshletSource`] it does nothing.
///
/// # Example
///
/// ```
/// use regia_asset::AssetId;
/// use regia_framework::meshlet_bridge::VoxelFarSource;
///
/// let tree = AssetId::new_random();
/// let source = VoxelFarSource::for_mesh(tree, "mesh/Oak/primitive_0").expect("a mesh sub-asset");
/// assert_eq!(source.id, AssetId::sub_asset(tree, "voxels/Oak/primitive_0"));
/// assert!(source.settings.opacity_dropout);
/// ```
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct VoxelFarSource {
    /// The voxel far sub-asset's id.
    pub id: AssetId,
    /// How the levels are stacked (cost ratio, error scale, opacity dropout).
    pub settings: VoxelFarSettings,
}

impl VoxelFarSource {
    /// The voxel far sub-asset `id` with the default settings.
    #[must_use]
    pub fn new(id: AssetId) -> Self {
        Self {
            id,
            settings: VoxelFarSettings::default(),
        }
    }

    /// The proxy cooked for the mesh sub-asset `mesh_sub_path` (`mesh/<node>/primitive_<i>`) of
    /// the glTF `gltf`. `None` for a path that names no mesh primitive.
    #[must_use]
    pub fn for_mesh(gltf: AssetId, mesh_sub_path: &str) -> Option<Self> {
        voxel_far_sub_path(mesh_sub_path).map(|path| Self::new(AssetId::sub_asset(gltf, &path)))
    }

    /// The proxy cooked for the mesh at the manifest path `mesh_path`
    /// (`assets/oak.gltf#mesh/Oak/primitive_0`), looked up in `server`'s manifest. `None` when
    /// the path names no mesh primitive or nothing was cooked for it.
    #[must_use]
    pub fn for_mesh_path(server: &AssetServer, mesh_path: &str) -> Option<Self> {
        let (file, sub) = mesh_path.split_once('#')?;
        let sub = voxel_far_sub_path(sub)?;
        server.id_for_path(&format!("{file}#{sub}")).map(Self::new)
    }
}

/// What the voxel far levels added to the meshes the bridge inserted
/// ([`MeshletBridgeState::voxel_far_stats`], `render.voxel_far_stats`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VoxelFarBridgeStats {
    /// Entities with a [`VoxelFarSource`] whose mesh is on the visibility buffer.
    pub entities: u32,
    /// Distinct meshes inserted with at least one voxel level.
    pub meshes: u32,
    /// Meshes whose proxy had no level cheaper than the DAG's roots (inserted plain).
    pub meshes_without_levels: u32,
    /// Voxel levels over every extended mesh.
    pub levels: u32,
    /// Clusters the voxel levels added.
    pub clusters: u64,
    /// Triangles the voxel levels added.
    pub triangles: u64,
    /// Vertices the voxel levels appended.
    pub vertices: u64,
    /// Bytes the voxel levels add to the visibility buffer's arenas, once per mesh: vertices
    /// (64 B each), cluster vertex lists and triangles (4 B per entry) and cluster records.
    pub gpu_bytes: u64,
    /// Meshes that fell back to plain meshlets ([`MeshletBridgeState::voxel_far_fallbacks`]).
    pub fallbacks: u32,
}

/// What one extended mesh carries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct VoxelUse {
    levels: u32,
    clusters: u64,
    triangles: u64,
    vertices: u64,
    gpu_bytes: u64,
}

/// Why an entity's mesh draws on the forward path although it names meshlets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ForwardReason {
    /// It has a joint palette ([`SkinJoints`]).
    Skinned,
    /// It has morph targets ([`MeshMorphTargets`]).
    Morphing,
    /// Its material blends, has a masked hair or parallax alpha test, or transmits.
    Translucent,
    /// Its material is a surface graph (the resolve shades the standard surface only).
    GraphMaterial,
    /// Its material has a thin film (the `THIN_FILM` forward variant, ADR-1977).
    ThinFilm,
    /// The payload did not load ([`MeshletAssets::errors`] has the code).
    LoadError,
    /// The payload was cooked for other mesh data (vertex or index count differs).
    Mismatch,
    /// Its material sways, but on a surface graph of its own: the resolve shades the standard
    /// surface only (ADR-3916).
    SwayGraphSurface,
}

impl ForwardReason {
    /// Every reason, in report order.
    pub const ALL: [Self; 8] = [
        Self::Skinned,
        Self::Morphing,
        Self::Translucent,
        Self::GraphMaterial,
        Self::ThinFilm,
        Self::LoadError,
        Self::Mismatch,
        Self::SwayGraphSurface,
    ];

    /// The stable code the agent protocol reports.
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::Skinned => "skinned",
            Self::Morphing => "morphing",
            Self::Translucent => "translucent",
            Self::GraphMaterial => "graph_material",
            Self::ThinFilm => "thin_film",
            Self::LoadError => "load_error",
            Self::Mismatch => "meshlets_mismatch",
            Self::SwayGraphSurface => "sway_graph_surface",
        }
    }
}

/// A meshlet payload that did not load.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MeshletPayloadError {
    /// The meshlet sub-asset.
    pub id: AssetId,
    /// `meshlets_not_cooked`, `meshlets_unreadable`, `meshlets_old_version`, `meshlets_invalid`
    /// (`regia_render::visbuffer::MeshletLoadError::code`) or `meshlets_mismatch`.
    pub code: &'static str,
    /// What went wrong.
    pub message: String,
}

struct Payload {
    generation: u64,
    result: Result<Arc<MeshletMesh>, MeshletPayloadError>,
}

/// The cooked meshlet loader: payloads by asset id, read once per asset-server generation (a
/// hot reload of the `.fmlt` loads it again). A failed load warns once per generation.
#[derive(Resource, Default)]
pub struct MeshletAssets {
    payloads: HashMap<AssetId, Payload>,
    /// Loads that failed since start.
    failures: u64,
}

impl MeshletAssets {
    /// The payload of `id` at `server`'s current generation of it, loading it when needed.
    ///
    /// # Errors
    ///
    /// The asset is not cooked, or its payload is unreadable, of another version or invalid
    /// (remembered until the asset changes).
    pub fn load(
        &mut self,
        server: Option<&mut AssetServer>,
        id: AssetId,
    ) -> Result<Arc<MeshletMesh>, MeshletPayloadError> {
        let generation = server.as_deref().map_or(0, |s| s.generation(id));
        if let Some(payload) = self.payloads.get(&id)
            && payload.generation == generation
        {
            return payload.result.clone();
        }
        let result = match server {
            Some(server) => {
                // The id is marked as decoded outside the asset server, so a hot reload of it
                // bumps the generation the payload is cached under (MeshletMesh::load did this).
                server.track_external(id);
                // The streaming load (ADR-3708): the payload's bytes stay a mapped view of the
                // pak behind a `BandDisk`; the cluster lists and triangles are read on demand.
                // The head parse validates the same way `MeshletMesh::load` does.
                let reader = server.payload_reader();
                crate::meshlet_stream::open_stream(&reader, id)
                    .map(|open| Arc::new(open.mesh))
                    .map_err(|err| MeshletPayloadError {
                        id,
                        code: err.code(),
                        message: err.to_string(),
                    })
            }
            None => Err(MeshletPayloadError {
                id,
                code: "meshlets_not_cooked",
                message: "no asset server".to_string(),
            }),
        };
        if let Err(err) = &result {
            self.failures += 1;
            tracing::warn!(
                target: "regia_framework",
                %id,
                code = err.code,
                "cooked meshlets not loaded ({}); the mesh draws forward",
                err.message
            );
        }
        self.payloads.insert(
            id,
            Payload {
                generation,
                result: result.clone(),
            },
        );
        result
    }

    /// Loads that failed since start.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures
    }

    /// The payloads whose last load failed.
    pub fn errors(&self) -> impl Iterator<Item = &MeshletPayloadError> + '_ {
        self.payloads.values().filter_map(|p| p.result.as_ref().err())
    }

    /// Drops payloads no entity names any more.
    fn retain(&mut self, keep: &HashSet<AssetId>) {
        self.payloads.retain(|id, _| keep.contains(id));
    }
}

struct VoxelPayload {
    generation: u64,
    result: Result<Arc<VoxelFarProxy>, MeshletPayloadError>,
}

/// The cooked voxel far loader: proxies by asset id, read once per asset-server generation (a
/// hot reload of the `.fvox` loads it again). A failed load warns once per generation.
#[derive(Resource, Default)]
pub struct VoxelFarAssets {
    payloads: HashMap<AssetId, VoxelPayload>,
    /// Loads that failed since start.
    failures: u64,
}

impl VoxelFarAssets {
    /// The proxy `id` at `server`'s current generation of it, loading it when needed.
    ///
    /// # Errors
    ///
    /// The proxy is not cooked, or unreadable, of another version or invalid
    /// (`regia_render::visbuffer::VoxelFarLoadError::code`), remembered until it changes.
    pub fn load(
        &mut self,
        server: Option<&mut AssetServer>,
        id: AssetId,
    ) -> Result<Arc<VoxelFarProxy>, MeshletPayloadError> {
        let generation = server.as_deref().map_or(0, |s| s.generation(id));
        if let Some(payload) = self.payloads.get(&id)
            && payload.generation == generation
        {
            return payload.result.clone();
        }
        let result = match server {
            Some(server) => {
                VoxelFarProxy::load(server, id)
                    .map(Arc::new)
                    .map_err(|err| MeshletPayloadError {
                        id,
                        code: err.code(),
                        message: err.to_string(),
                    })
            }
            None => Err(MeshletPayloadError {
                id,
                code: VoxelFarLoadError::NotCooked(String::new()).code(),
                message: "no asset server".to_string(),
            }),
        };
        if let Err(err) = &result {
            self.failures += 1;
            tracing::warn!(
                target: "regia_framework",
                %id,
                code = err.code,
                "voxel far proxy not loaded ({}); the mesh takes its plain meshlets",
                err.message
            );
        }
        self.payloads.insert(
            id,
            VoxelPayload {
                generation,
                result: result.clone(),
            },
        );
        result
    }

    /// Loads that failed since start.
    #[must_use]
    pub fn failures(&self) -> u64 {
        self.failures
    }

    /// Drops proxies no entity names any more.
    fn retain(&mut self, keep: &HashSet<AssetId>) {
        self.payloads.retain(|id, _| keep.contains(id));
    }
}

/// The voxel far proxy an entity names, as last looked at.
#[derive(Clone, Copy, Debug, PartialEq)]
struct VoxelKey {
    id: AssetId,
    generation: u64,
    settings: VoxelFarSettings,
}

/// An entity the bridge has seen.
#[derive(Clone, Copy, Debug)]
struct Tracked {
    mesh_key: u64,
    source: AssetId,
    generation: u64,
    voxel: Option<VoxelKey>,
    /// A counted stand-in's upload serial when it was looked at (`None`: not counted, or not
    /// resident yet): a new upload looks at it again (ADR-7542).
    upload: Option<u64>,
    /// `None`: its meshlets are inserted.
    reason: Option<ForwardReason>,
}

/// Meshlets the bridge inserted under one mesh key.
#[derive(Clone, Copy, Debug)]
struct Installed {
    source: AssetId,
    generation: u64,
    /// The proxy asked for when it was inserted (`None`: plain meshlets were asked for).
    voxel: Option<VoxelKey>,
    /// What the voxel levels added (`None`: inserted plain).
    voxel_use: Option<VoxelUse>,
    /// The counted upload the meshlets were checked against (`None`: not counted, or its counts
    /// were not known yet): a new upload inserts them again (ADR-7542).
    upload: Option<u64>,
    /// Entities on the visibility buffer with this key.
    users: u32,
}

/// What the bridge did, for the agent protocol (`render.get_visbuffer`) and host UIs.
#[derive(Resource, Default)]
pub struct MeshletBridgeState {
    enabled: bool,
    entities: HashMap<Entity, Tracked>,
    installed: HashMap<u64, Installed>,
    /// Mismatched (payload, mesh key) pairs already warned about.
    mismatch_warned: HashSet<(AssetId, u64)>,
    mismatches: Vec<MeshletPayloadError>,
    /// (proxy, mesh key) pairs already warned about.
    voxel_warned: HashSet<(AssetId, u64)>,
    voxel_fallbacks: Vec<MeshletPayloadError>,
    inserts: u64,
    removals: u64,
}

impl MeshletBridgeState {
    /// The profile had the visibility buffer on at the last update.
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Distinct meshes (mesh keys) whose meshlets the bridge inserted.
    #[must_use]
    pub fn meshes_on_visbuffer(&self) -> usize {
        self.installed.len()
    }

    /// Entities naming meshlets whose mesh is inserted.
    #[must_use]
    pub fn entities_on_visbuffer(&self) -> usize {
        self.entities.values().filter(|t| t.reason.is_none()).count()
    }

    /// Entities naming meshlets (on the visibility buffer or not).
    #[must_use]
    pub fn entities_with_meshlets(&self) -> usize {
        self.entities.len()
    }

    /// Entities naming meshlets that draw forward, per reason (every reason listed).
    #[must_use]
    pub fn forward_counts(&self) -> Vec<(ForwardReason, usize)> {
        ForwardReason::ALL
            .iter()
            .map(|&reason| {
                let n = self
                    .entities
                    .values()
                    .filter(|t| t.reason == Some(reason))
                    .count();
                (reason, n)
            })
            .collect()
    }

    /// Why `entity` draws forward; `None` when its meshlets are inserted or the bridge has not
    /// seen it (no [`MeshletSource`], or the path is off).
    #[must_use]
    pub fn forward_reason(&self, entity: Entity) -> Option<ForwardReason> {
        self.entities.get(&entity).and_then(|t| t.reason)
    }

    /// True when the bridge inserted meshlets for `entity`'s mesh.
    #[must_use]
    pub fn on_visbuffer(&self, entity: Entity) -> bool {
        self.entities.get(&entity).is_some_and(|t| t.reason.is_none())
    }

    /// Payloads cooked for other mesh data (code `meshlets_mismatch`).
    #[must_use]
    pub fn mismatches(&self) -> &[MeshletPayloadError] {
        &self.mismatches
    }

    /// Voxel far proxies that did not load or did not extend their mesh's DAG, so the mesh took
    /// its plain meshlets (one entry per proxy and mesh; codes `voxel_far_not_cooked`,
    /// `voxel_far_unreadable`, `voxel_far_old_version`, `voxel_far_invalid`).
    #[must_use]
    pub fn voxel_far_fallbacks(&self) -> &[MeshletPayloadError] {
        &self.voxel_fallbacks
    }

    /// What the voxel far levels added to the inserted meshes.
    #[must_use]
    pub fn voxel_far_stats(&self) -> VoxelFarBridgeStats {
        let mut stats = VoxelFarBridgeStats {
            entities: self
                .entities
                .values()
                .filter(|t| t.reason.is_none() && t.voxel.is_some())
                .count() as u32,
            fallbacks: self.voxel_fallbacks.len() as u32,
            ..VoxelFarBridgeStats::default()
        };
        for installed in self.installed.values() {
            match installed.voxel_use {
                Some(used) if used.levels > 0 => {
                    stats.meshes += 1;
                    stats.levels += used.levels;
                    stats.clusters += used.clusters;
                    stats.triangles += used.triangles;
                    stats.vertices += used.vertices;
                    stats.gpu_bytes += used.gpu_bytes;
                }
                Some(_) => stats.meshes_without_levels += 1,
                None => {}
            }
        }
        stats
    }

    /// Meshlet inserts and removals the bridge made since start.
    #[must_use]
    pub fn churn(&self) -> (u64, u64) {
        (self.inserts, self.removals)
    }

    /// Removes `entity` from the visibility buffer's users; the last user of a mesh key
    /// removes its meshlets.
    fn detach(&mut self, entity: Entity, renderer: &mut RuntimeRenderer) {
        let Some(tracked) = self.entities.remove(&entity) else {
            return;
        };
        if tracked.reason.is_some() {
            return;
        }
        if let Some(installed) = self.installed.get_mut(&tracked.mesh_key) {
            installed.users = installed.users.saturating_sub(1);
            if installed.users == 0 {
                self.installed.remove(&tracked.mesh_key);
                renderer.0.remove_meshlets(tracked.mesh_key);
                self.removals += 1;
            }
        }
    }

    /// Removes every meshlet the bridge inserted (the path turned off).
    fn remove_all(&mut self, renderer: &mut RuntimeRenderer) {
        self.removals += self.installed.len() as u64;
        for key in self.installed.keys() {
            renderer.0.remove_meshlets(*key);
        }
        self.installed.clear();
        self.entities.clear();
        self.mismatches.clear();
        self.voxel_fallbacks.clear();
        self.voxel_warned.clear();
    }
}

/// Loads cooked meshlets and keeps the renderer's visibility-buffer meshes in step with the
/// ECS ([`sync_meshlets`]); see the module docs.
#[derive(Default)]
pub struct MeshletBridgePlugin;

impl Plugin for MeshletBridgePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<MeshletAssets>()
            .init_resource::<VoxelFarAssets>()
            .init_resource::<MeshletBridgeState>()
            .add_systems(PostUpdate, sync_meshlets);
    }
}

/// What [`sync_meshlets`] reads from a meshlet entity.
type MeshletEntity = (
    Entity,
    Ref<'static, MeshRenderer>,
    Ref<'static, MeshletSource>,
    Has<SkinJoints>,
    Has<MeshMorphTargets>,
    Has<AppliedLightmapReceiver>,
    Option<Ref<'static, VoxelFarSource>>,
);

/// Why `renderer` with these components cannot take the visibility buffer, or `None`.
fn ineligible(renderer: &MeshRenderer, skinned: bool, morphing: bool) -> Option<ForwardReason> {
    let material = &renderer.material;
    if skinned {
        Some(ForwardReason::Skinned)
    } else if morphing {
        Some(ForwardReason::Morphing)
    } else if (regia_render::hair::effective_alpha_mode(material) != regia_render::ALPHA_OPAQUE
        && !sway::takes_alpha_test(material))
        || material.transmission > 0.0
    {
        Some(ForwardReason::Translucent)
    } else if material.surface_graph.is_some()
        && let Some(reason) = match sway::sway_gate(material) {
            sway::SwayGate::Clusters => None,
            sway::SwayGate::NotSwaying => Some(ForwardReason::GraphMaterial),
            sway::SwayGate::Forward(sway::SwaySkip::GraphSurface) => Some(ForwardReason::SwayGraphSurface),
            sway::SwayGate::Forward(sway::SwaySkip::Translucent) => Some(ForwardReason::Translucent),
            sway::SwayGate::Forward(_) => Some(ForwardReason::GraphMaterial),
        }
    {
        Some(reason)
    } else if material.thin_film.is_on() {
        Some(ForwardReason::ThinFilm)
    } else {
        None
    }
}

/// The removals [`sync_meshlets`] reacts to: a mesh renderer or a meshlet source taken off (or
/// its entity despawned).
type Removed<'w, 's> = (
    RemovedComponents<'w, 's, MeshRenderer>,
    RemovedComponents<'w, 's, MeshletSource>,
);

/// `PostUpdate`: inserts the cooked meshlets of every eligible [`MeshletSource`] entity while the
/// profile has the visibility buffer on, and removes them when the mesh changes, the entity
/// goes, the payload is hot-reloaded or the path turns off. Does nothing without a
/// [`RuntimeRenderer`].
#[tracing::instrument(skip_all, name = "meshlet_bridge")]
pub fn sync_meshlets(
    settings: Option<Res<RenderSettings>>,
    runtime: Option<ResMut<RuntimeRenderer>>,
    mut server: Option<ResMut<AssetServer>>,
    (mut assets, mut voxel_assets): (ResMut<MeshletAssets>, ResMut<VoxelFarAssets>),
    mut state: ResMut<MeshletBridgeState>,
    entities: Query<MeshletEntity>,
    (mut removed_renderers, mut removed_sources): Removed<'_, '_>,
) {
    let Some(mut runtime) = runtime else {
        return;
    };
    let state = &mut *state;
    let enabled = settings.is_some_and(|s| s.profile.visibility_buffer);
    if !enabled {
        if state.enabled || !state.installed.is_empty() {
            state.remove_all(&mut runtime);
        }
        state.enabled = false;
        removed_renderers.clear();
        removed_sources.clear();
        return;
    }
    // Switched on (or first run): every entity is looked at again.
    let rescan = !state.enabled;
    state.enabled = true;
    let gone: Vec<Entity> = removed_renderers.read().chain(removed_sources.read()).collect();
    for entity in gone {
        if !entities.contains(entity) {
            state.detach(entity, &mut runtime);
        }
    }
    let mut named = HashSet::new();
    let mut named_voxels = HashSet::new();
    for (entity, renderer, source, skinned, morphing, receiver, voxel_source) in &entities {
        named.insert(source.id);
        let voxel = voxel_source.as_ref().map(|v| {
            named_voxels.insert(v.id);
            VoxelKey {
                id: v.id,
                generation: server.as_deref().map_or(0, |s| s.generation(v.id)),
                settings: v.settings,
            }
        });
        let voxel_changed = voxel_source.as_ref().is_some_and(|v| v.is_changed());
        if receiver {
            state.detach(entity, &mut runtime);
            continue;
        }
        let reason = ineligible(&renderer, skinned, morphing);
        let generation = server.as_deref().map_or(0, |s| s.generation(source.id));
        let key = mesh_key(&renderer.mesh);
        // A counted stand-in (ADR-7284) is looked at again when its mesh is uploaded again (a hot
        // reload of the `.fmesh`): its meshlets are checked against the new upload's counts and
        // inserted again, so the renderer never keeps clusters cooked for the old one (ADR-7542).
        let upload = if renderer.mesh.vertices.is_empty() && regia_render::is_asset_mesh_key(key) {
            runtime.0.asset_mesh_upload_serial(key)
        } else {
            None
        };
        // Unchanged: same payload version and the same eligibility as when it was last looked at
        // (a load error or mismatch is retried only when the payload or the mesh changes).
        let unchanged = state.entities.get(&entity).is_some_and(|t| {
            let eligibility = match t.reason {
                None | Some(ForwardReason::LoadError | ForwardReason::Mismatch) => None,
                other => other,
            };
            t.source == source.id
                && t.generation == generation
                && t.voxel == voxel
                && t.upload == upload
                && eligibility == reason
        });
        if unchanged && !rescan && !renderer.is_changed() && !source.is_changed() && !voxel_changed {
            continue;
        }
        state.detach(entity, &mut runtime);
        let mut tracked = Tracked {
            mesh_key: key,
            source: source.id,
            generation,
            voxel,
            upload,
            reason,
        };
        if reason.is_none() {
            tracked.reason = attach(
                state,
                (&mut assets, &mut voxel_assets),
                server.as_deref_mut(),
                &mut runtime,
                &renderer,
                &tracked,
            );
        }
        state.entities.insert(entity, tracked);
    }
    assets.retain(&named);
    voxel_assets.retain(&named_voxels);
}

/// `meshlets` with the voxel levels of `voxel` on top for the mesh `renderer` draws, and what they
/// added; the plain meshlets (and a recorded, warned fallback) when the proxy does not load or
/// does not extend them.
fn extend_with_voxels(
    state: &mut MeshletBridgeState,
    voxel_assets: &mut VoxelFarAssets,
    server: Option<&mut AssetServer>,
    renderer: &MeshRenderer,
    meshlets: Arc<MeshletMesh>,
    voxel: &VoxelKey,
    key: u64,
) -> (Arc<MeshletMesh>, VoxelUse) {
    let _span = tracing::info_span!("meshlet_bridge_voxel_far").entered();
    // A stream-shaped mesh (ADR-3708) carries its lists on the pak; the extension reads them.
    let mut meshlets = meshlets;
    if meshlets.disk.is_some()
        && let Err(err) = Arc::make_mut(&mut meshlets).materialize()
    {
        let err = MeshletPayloadError {
            id: voxel.id,
            code: err.code(),
            message: format!("streamed meshlets did not materialize: {err}"),
        };
        if state.voxel_warned.insert((voxel.id, key)) {
            tracing::warn!(
                target: "regia_framework",
                id = %voxel.id,
                code = err.code,
                "voxel far proxy {}; the mesh takes its plain meshlets",
                err.message
            );
            state.voxel_fallbacks.push(err);
        }
        return (meshlets, VoxelUse::default());
    }
    let extended = voxel_assets.load(server, voxel.id).and_then(|proxy| {
        proxy
            .extend_meshlets(&meshlets, &renderer.mesh.vertices, &voxel.settings)
            .map_err(|err| {
                let err = VoxelFarLoadError::Format(err);
                MeshletPayloadError {
                    id: voxel.id,
                    code: err.code(),
                    message: format!("does not extend the mesh's meshlets: {err}"),
                }
            })
    });
    match extended {
        Ok(far) => {
            // A proxy that failed before and extends now is no fallback any more.
            state.voxel_warned.remove(&(voxel.id, key));
            state.voxel_fallbacks.retain(|e| e.id != voxel.id);
            let lists = (far.meshlets.vertices.len() - meshlets.vertices.len()) as u64
                + (far.meshlets.triangles.len() - meshlets.triangles.len()) as u64;
            let clusters = far.voxel_clusters();
            let vertices = (far.vertices.len() - renderer.mesh.vertices.len()) as u64;
            let used = VoxelUse {
                levels: far.levels.len() as u32,
                clusters,
                triangles: far.voxel_triangles(),
                vertices,
                gpu_bytes: vertices * 64
                    + lists * 4
                    + clusters * std::mem::size_of::<regia_render::visbuffer::Meshlet>() as u64,
            };
            if far.levels.is_empty() {
                return (meshlets, used);
            }
            (Arc::new(far.bridged()), used)
        }
        Err(err) => {
            if state.voxel_warned.insert((voxel.id, key)) {
                // A load failure already warned in the loader; an extension failure warns here.
                if err.message.starts_with("does not extend") {
                    tracing::warn!(
                        target: "regia_framework",
                        id = %voxel.id,
                        code = err.code,
                        "voxel far proxy {}; the mesh takes its plain meshlets",
                        err.message
                    );
                }
                state.voxel_fallbacks.push(err);
            }
            (meshlets, VoxelUse::default())
        }
    }
}

/// Inserts the meshlets `entity` names under its mesh key (or counts one more user of the ones
/// there). Returns why the mesh stays forward, or `None`.
fn attach(
    state: &mut MeshletBridgeState,
    (assets, voxel_assets): (&mut MeshletAssets, &mut VoxelFarAssets),
    mut server: Option<&mut AssetServer>,
    runtime: &mut RuntimeRenderer,
    renderer: &MeshRenderer,
    entity: &Tracked,
) -> Option<ForwardReason> {
    let (key, source, generation) = (entity.mesh_key, entity.source, entity.generation);
    let mesh = match assets.load(server.as_deref_mut(), source) {
        Ok(mesh) => mesh,
        Err(_) => return Some(ForwardReason::LoadError),
    };
    // The payload's base range is the mesh's own vertices (all of a cooked `.fmlt`). A counted
    // stand-in (ADR-7284) has none on the CPU: its upload's counts are the mesh's (ADR-7462); while
    // they are unknown (no uploader, not resident yet) the renderer checks them at the first draw.
    let (vertex_count, index_count) =
        if renderer.mesh.vertices.is_empty() && regia_render::is_asset_mesh_key(key) {
            runtime
                .0
                .asset_mesh_counts(key)
                .unwrap_or((mesh.base_vertex_count(), mesh.source_index_count as usize))
        } else {
            (renderer.mesh.vertices.len(), renderer.mesh.indices.len())
        };
    if mesh.base_vertex_count() != vertex_count || mesh.source_index_count as usize != index_count {
        if state.mismatch_warned.insert((source, key)) {
            let message = format!(
                "meshlets for {} vertices / {} indices, the mesh has {} / {}",
                mesh.source_vertex_count, mesh.source_index_count, vertex_count, index_count
            );
            tracing::warn!(
                target: "regia_framework",
                id = %source,
                "cooked meshlets do not match the mesh ({message}); it draws forward"
            );
            state.mismatches.push(MeshletPayloadError {
                id: source,
                code: ForwardReason::Mismatch.code(),
                message,
            });
        }
        return Some(ForwardReason::Mismatch);
    }
    let current = state.installed.get(&key).copied();
    // The proxy the mesh was inserted with was hot-reloaded or its settings changed.
    let proxy_moved = |installed: &Installed| match (installed.voxel, entity.voxel) {
        (Some(a), Some(b)) => a.id == b.id && a != b,
        _ => false,
    };
    match current {
        // Another entity with this mesh inserted it: one more user. A different source or
        // proxy (or none) for the same mesh keeps the first one's meshlets and levels.
        Some(installed)
            if installed.source != source
                || (installed.generation == generation
                    && installed.upload == entity.upload
                    && !proxy_moved(&installed)) =>
        {
            if let Some(installed) = state.installed.get_mut(&key) {
                installed.users += 1;
            }
        }
        // The same source at a new generation (a hot reload), its proxy moved, or a counted
        // mesh's new upload: replace.
        Some(installed) => {
            let (mesh, voxel_use) = match &entity.voxel {
                Some(voxel) => {
                    let (mesh, used) =
                        extend_with_voxels(state, voxel_assets, server, renderer, mesh, voxel, key);
                    (mesh, Some(used))
                }
                None => (mesh, None),
            };
            runtime.0.remove_meshlets(key);
            runtime.0.insert_meshlets(key, mesh);
            state.installed.insert(
                key,
                Installed {
                    source,
                    generation,
                    voxel: entity.voxel,
                    voxel_use,
                    upload: entity.upload,
                    users: installed.users + 1,
                },
            );
            state.removals += 1;
            state.inserts += 1;
        }
        None => {
            let (mesh, voxel_use) = match &entity.voxel {
                Some(voxel) => {
                    let (mesh, used) =
                        extend_with_voxels(state, voxel_assets, server, renderer, mesh, voxel, key);
                    (mesh, Some(used))
                }
                None => (mesh, None),
            };
            runtime.0.insert_meshlets(key, mesh);
            state.installed.insert(
                key,
                Installed {
                    source,
                    generation,
                    voxel: entity.voxel,
                    voxel_use,
                    upload: entity.upload,
                    users: 1,
                },
            );
            state.inserts += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::{
        AssetServerSettings, COOKED_FORMAT_VERSION, CookedHeader, cooked_file_name, encode_cooked_file,
    };
    use regia_core::id::TypePathHash;
    use regia_render::visbuffer::MESHLET_TYPE_PATH;
    use regia_render::{CpuMaterial, CpuMesh, LabVertex};
    use regia_test_utils::unique_temp_dir;

    /// An `n` × `n` quad grid on XZ.
    fn grid(n: u32) -> Arc<CpuMesh> {
        let mut vertices = Vec::new();
        for z in 0..=n {
            for x in 0..=n {
                vertices.push(LabVertex::from_pos_normal_uv(
                    [x as f32, 0.0, z as f32],
                    [0.0, 1.0, 0.0],
                    [x as f32 / n as f32, z as f32 / n as f32],
                ));
            }
        }
        let mut indices = Vec::new();
        for z in 0..n {
            for x in 0..n {
                let a = z * (n + 1) + x;
                indices.extend_from_slice(&[a, a + n + 1, a + 1, a + 1, a + n + 1, a + n + 2]);
            }
        }
        Arc::new(CpuMesh { vertices, indices })
    }

    fn meshlets_for(mesh: &CpuMesh) -> MeshletMesh {
        let positions: Vec<[f32; 3]> = mesh.vertices.iter().map(|v| v.position).collect();
        MeshletMesh::single_level(&positions, &mesh.indices)
    }

    /// Writes `payload` as the cooked file of `id` (a `MESHLET_TYPE_PATH` sub-asset).
    fn cook(server: &AssetServer, id: AssetId, payload: &[u8]) {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of(MESHLET_TYPE_PATH),
            asset_id: id,
            importer_version: 1,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, payload).expect("cooked file");
        std::fs::create_dir_all(server.cache_dir()).expect("cache dir");
        regia_test_utils::write_atomic(&server.cache_dir().join(cooked_file_name(id)), &file).expect("write");
    }

    fn server(tag: &str) -> AssetServer {
        AssetServer::open(AssetServerSettings {
            project_root: unique_temp_dir(tag),
            platform: "linux".to_string(),
            content_dir: None,
        })
        .expect("open asset server")
    }

    fn settings(on: bool) -> RenderSettings {
        let mut settings = RenderSettings::default();
        settings.profile.visibility_buffer = on;
        settings
    }

    fn app(server: AssetServer, on: bool) -> App {
        let mut app = App::new();
        app.add_plugins(MeshletBridgePlugin);
        app.insert_resource(server);
        app.insert_resource(RuntimeRenderer::default());
        app.insert_resource(settings(on));
        app
    }

    /// A rigid opaque mesh entity and the cooked meshlets of its mesh.
    fn rock(app: &mut App, mesh: &Arc<CpuMesh>) -> (AssetId, Entity) {
        let id = AssetId::new_random();
        cook(
            app.world().resource::<AssetServer>(),
            id,
            &meshlets_for(mesh).to_bytes(),
        );
        let entity = app
            .world_mut()
            .spawn((
                MeshRenderer::new(Arc::clone(mesh), Arc::new(CpuMaterial::default())),
                MeshletSource::new(id),
            ))
            .id();
        (id, entity)
    }

    fn has(app: &App, mesh: &Arc<CpuMesh>) -> bool {
        app.world()
            .resource::<RuntimeRenderer>()
            .0
            .has_meshlets(mesh_key(mesh))
    }

    fn state(app: &App) -> &MeshletBridgeState {
        app.world().resource::<MeshletBridgeState>()
    }

    /// Profile on: the cooked mesh's meshlets are inserted under its mesh key; a second entity
    /// with the same mesh shares them, and they go when the last entity is despawned.
    #[test]
    fn a_cooked_mesh_takes_the_visibility_buffer_until_despawned() {
        let mut app = app(server("meshlet-bridge-insert"), true);
        let mesh = grid(12);
        let (id, first) = rock(&mut app, &mesh);
        app.update();
        assert!(has(&app, &mesh), "inserted under the mesh key");
        assert!(state(&app).on_visbuffer(first));
        assert_eq!(state(&app).meshes_on_visbuffer(), 1);
        assert!(state(&app).enabled());

        let second = app
            .world_mut()
            .spawn((
                MeshRenderer::new(Arc::clone(&mesh), Arc::new(CpuMaterial::default())),
                MeshletSource::new(id),
            ))
            .id();
        app.update();
        assert_eq!(state(&app).entities_on_visbuffer(), 2);
        assert_eq!(state(&app).meshes_on_visbuffer(), 1, "one mesh, two users");

        assert!(app.world_mut().despawn(first));
        app.update();
        assert!(has(&app, &mesh), "the other entity still draws it");
        assert!(app.world_mut().despawn(second));
        app.update();
        assert!(!has(&app, &mesh), "despawn removed the meshlets");
        assert_eq!(state(&app).meshes_on_visbuffer(), 0);
        assert_eq!(state(&app).churn(), (1, 1));
    }

    /// `mesh`'s unwrap with a seam: a vertex split, so the receiver mesh has one vertex more.
    fn seamed_lightmap(mesh: &CpuMesh) -> regia_asset::lightmap::Lightmap {
        use regia_asset::lightmap::{
            Coverage, Lightmap, LightmapBakeInfo, LightmapEntry, LightmapLayout, MeshUnwrap,
        };
        let split = mesh.indices[0];
        let vertex_count = mesh.vertices.len() as u32;
        let mut xref: Vec<u32> = (0..vertex_count).collect();
        xref.push(split);
        let mut indices = mesh.indices.clone();
        indices[0] = vertex_count;
        Lightmap::from_irradiance(
            LightmapLayout {
                width: 2,
                height: 2,
                density: 1.0,
                padding: 1,
                entries: vec![LightmapEntry {
                    name: "rock".into(),
                    source_vertices: vertex_count,
                    rect: [0, 0, 2, 2],
                    scale_offset: [1.0, 1.0, 0.0, 0.0],
                    vertex_count: 0,
                    index_count: 0,
                    first_vertex: 0,
                    first_index: 0,
                    charts: 1,
                    area_m2: 1.0,
                    texels_per_metre: 1.0,
                    key: None,
                    seams: None,
                    page: 0,
                }],
                unwraps: vec![MeshUnwrap {
                    uv: vec![[0.5, 0.5]; xref.len()],
                    xref,
                    indices,
                }],
            },
            &[[1.0; 3]; 4],
            vec![Coverage::BAKED; 4],
            LightmapBakeInfo::default(),
        )
        .expect("valid")
    }

    /// A lightmap receiver draws its unwrap: the source's meshlets do not fit it, so the bridge
    /// gives them back and reports no mismatch (ADR-2107). When the entity gets its own mesh back
    /// the bridge inserts them again.
    #[test]
    fn a_lightmap_receiver_gives_its_source_meshlets_back_without_a_mismatch() {
        use regia_render::lightmap::{InstalledLightmap, LightmapReceiver, LightmapReceiverReport};
        let mut app = app(server("meshlet-bridge-receiver"), true);
        let mesh = grid(8);
        let (_, entity) = rock(&mut app, &mesh);
        let mut installed = InstalledLightmap::default();
        installed.install("test", seamed_lightmap(&mesh));
        app.insert_resource(installed);
        app.init_resource::<LightmapReceiverReport>();
        app.add_systems(Update, regia_render::lightmap::apply_lightmap_receivers);
        app.update();
        assert!(state(&app).on_visbuffer(entity), "not a receiver yet");
        assert!(has(&app, &mesh));

        app.world_mut()
            .entity_mut(entity)
            .insert(LightmapReceiver { entry: "rock".into() });
        for _ in 0..3 {
            app.update();
        }
        let report = app.world().resource::<LightmapReceiverReport>();
        assert_eq!((report.applied, report.clusters, report.no_clusters), (1, 0, 1));
        let drawn = app
            .world()
            .get::<MeshRenderer>(entity)
            .expect("mesh")
            .mesh
            .clone();
        assert_eq!(
            drawn.vertices.len(),
            mesh.vertices.len() + 1,
            "the seam split one"
        );
        assert!(!has(&app, &mesh), "the source's meshlets went back");
        assert!(!has(&app, &drawn), "they do not fit the receiver mesh");
        assert_eq!(state(&app).forward_reason(entity), None);
        assert!(
            state(&app).mismatches().is_empty(),
            "{:?}",
            state(&app).mismatches()
        );
        assert_eq!(state(&app).entities_with_meshlets(), 0);

        app.world_mut().entity_mut(entity).remove::<LightmapReceiver>();
        for _ in 0..2 {
            app.update();
        }
        assert!(Arc::ptr_eq(
            &app.world().get::<MeshRenderer>(entity).expect("mesh").mesh,
            &mesh
        ));
        assert!(has(&app, &mesh), "its own mesh, its own meshlets");
        assert!(state(&app).on_visbuffer(entity));
    }

    /// Turning the path off removes every inserted mesh; turning it on inserts them again.
    #[test]
    fn a_profile_without_the_path_removes_them_all() {
        let mut app = app(server("meshlet-bridge-profile"), true);
        let (a, b) = (grid(8), grid(9));
        rock(&mut app, &a);
        rock(&mut app, &b);
        app.update();
        assert!(has(&app, &a) && has(&app, &b));

        app.insert_resource(settings(false));
        app.update();
        assert!(!has(&app, &a) && !has(&app, &b), "profile off removed all");
        assert!(!state(&app).enabled());
        assert_eq!(state(&app).meshes_on_visbuffer(), 0);

        app.insert_resource(settings(true));
        app.update();
        assert!(has(&app, &a) && has(&app, &b), "profile on inserted them again");
    }

    /// Skinned, morphing, translucent and graph meshes are never inserted, and say why.
    #[test]
    fn skinned_and_translucent_meshes_stay_forward() {
        let mut app = app(server("meshlet-bridge-forward"), true);
        let skinned = grid(6);
        let (_, skin) = rock(&mut app, &skinned);
        app.world_mut()
            .entity_mut(skin)
            .insert(regia_render::SkinJoints::default());
        let glass = grid(7);
        let (_, pane) = rock(&mut app, &glass);
        let blended = CpuMaterial {
            alpha_mode: regia_render::ALPHA_BLEND,
            ..CpuMaterial::default()
        };
        app.world_mut()
            .entity_mut(pane)
            .insert(MeshRenderer::new(Arc::clone(&glass), Arc::new(blended)));
        for _ in 0..3 {
            app.update();
        }
        assert!(!has(&app, &skinned), "a skinned mesh is never inserted");
        assert!(!has(&app, &glass), "a translucent mesh is never inserted");
        assert_eq!(state(&app).forward_reason(skin), Some(ForwardReason::Skinned));
        assert_eq!(state(&app).forward_reason(pane), Some(ForwardReason::Translucent));
        assert_eq!(state(&app).entities_on_visbuffer(), 0);

        // The skin goes: the mesh is rigid now and takes the path.
        app.world_mut()
            .entity_mut(skin)
            .remove::<regia_render::SkinJoints>();
        app.update();
        assert!(has(&app, &skinned));
        assert!(!has(&app, &glass));
    }

    /// A cooked quad (four vertices, two triangles, 16-bit indices) as the pipeline packs it.
    fn cooked_quad() -> regia_asset::CookedMesh {
        let positions = [
            [-0.5f32, 0.0, -0.5],
            [0.5, 0.0, -0.5],
            [0.5, 0.0, 0.5],
            [-0.5, 0.0, 0.5],
        ];
        let mut mesh = regia_asset::CookedMesh {
            aabb_min: [-0.5, 0.0, -0.5],
            aabb_max: [0.5, 0.0, 0.5],
            bounding_sphere: [0.0, 0.0, 0.0, 0.75],
            stream_positions: Vec::new(),
            stream_normals_tangents: Vec::new(),
            stream_uv0: Vec::new(),
            indices: Vec::new(),
            index_width: regia_asset::CookedIndexWidth::U16,
            vertex_count: 4,
            lods: vec![regia_asset::CookedMeshLod {
                screen_relative_size: 1.0,
                index_byte_offset: 0,
                index_byte_length: 12,
            }],
            submeshes: Vec::new(),
            lod_metric: regia_asset::LOD_METRIC_ERROR,
            format_version: regia_asset::COOKED_MESH_FORMAT_VERSION,
            skin: None,
            stream_color0: Vec::new(),
            morph_targets: Vec::new(),
            skin_influences: regia_asset::DEFAULT_SKIN_INFLUENCES,
        };
        for p in positions {
            for c in p {
                mesh.stream_positions.extend_from_slice(&c.to_le_bytes());
            }
            let (x, y) = regia_asset::encode_octahedral_normal([0.0, 1.0, 0.0]);
            for w in [x, y, 0, -1] {
                mesh.stream_normals_tangents.extend_from_slice(&w.to_le_bytes());
            }
            mesh.stream_uv0.extend_from_slice(&[0; 4]);
        }
        for i in [0u16, 2, 1, 0, 3, 2] {
            mesh.indices.extend_from_slice(&i.to_le_bytes());
        }
        mesh
    }

    /// ADR-7462: a counted stand-in (`MeshRenderer::counted`, no CPU vertices) with a
    /// `MeshletSource` takes the visibility buffer: its `.fmlt` is inserted under the asset's frame
    /// key (`asset_mesh_key`) instead of being refused as a mismatch against the stand-in's empty
    /// vertex list. Without an uploader the counts are unknown here, so the renderer checks them
    /// at the first draw.
    #[test]
    fn a_counted_stand_in_inserts_its_meshlets_under_the_asset_key() {
        let mut app = app(server("meshlet-bridge-counted"), true);
        let mesh_id = AssetId::new_random();
        let payload = cooked_quad().to_postcard().expect("mesh payload");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: regia_asset::AssetTypeTag::mesh().0,
            asset_id: mesh_id,
            importer_version: 1,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("cooked file");
        let cache = app.world().resource::<AssetServer>().cache_dir().to_path_buf();
        std::fs::create_dir_all(&cache).expect("cache dir");
        regia_test_utils::write_atomic(&cache.join(cooked_file_name(mesh_id)), &file).expect("write");
        let handle = {
            let mut server = app.world_mut().resource_mut::<AssetServer>();
            let handle = server.load::<regia_asset::CpuMesh>(mesh_id).expect("load");
            server.wait_for_loads();
            handle
        };
        let quad: Vec<[f32; 3]> = vec![
            [-0.5, 0.0, -0.5],
            [0.5, 0.0, -0.5],
            [0.5, 0.0, 0.5],
            [-0.5, 0.0, 0.5],
        ];
        let meshlets = MeshletMesh::single_level(&quad, &[0, 2, 1, 0, 3, 2]);
        let source = AssetId::new_random();
        cook(
            app.world().resource::<AssetServer>(),
            source,
            &meshlets.to_bytes(),
        );
        let entity = app
            .world_mut()
            .spawn((
                MeshRenderer::counted(handle, Arc::new(CpuMaterial::default())),
                MeshletSource::new(source),
            ))
            .id();
        app.update();
        let key = regia_render::asset_mesh_key(mesh_id);
        assert_eq!(state(&app).forward_reason(entity), None, "not a mismatch");
        assert!(state(&app).on_visbuffer(entity));
        assert!(
            app.world().resource::<RuntimeRenderer>().0.has_meshlets(key),
            "inserted under the asset's frame key"
        );
        assert!(state(&app).mismatches().is_empty());
    }

    /// `n` copies of [`cooked_quad`] side by side along X (4·`n` vertices, 6·`n` indices).
    fn cooked_quads(n: u16) -> regia_asset::CookedMesh {
        let quad = cooked_quad();
        let mut mesh = cooked_quad();
        mesh.stream_positions.clear();
        mesh.stream_normals_tangents.clear();
        mesh.stream_uv0.clear();
        mesh.indices.clear();
        for q in 0..n {
            for (v, p) in quad.stream_positions.as_chunks::<12>().0.iter().enumerate() {
                let x = f32::from_le_bytes([p[0], p[1], p[2], p[3]]) + f32::from(q) * 1.5;
                mesh.stream_positions.extend_from_slice(&x.to_le_bytes());
                mesh.stream_positions.extend_from_slice(&p[4..]);
                mesh.stream_normals_tangents
                    .extend_from_slice(&quad.stream_normals_tangents[v * 8..v * 8 + 8]);
                mesh.stream_uv0
                    .extend_from_slice(&quad.stream_uv0[v * 4..v * 4 + 4]);
            }
            for i in [0u16, 2, 1, 0, 3, 2] {
                mesh.indices.extend_from_slice(&(i + 4 * q).to_le_bytes());
            }
        }
        mesh.vertex_count = 4 * u32::from(n);
        mesh.lods[0].index_byte_length = 12 * u32::from(n);
        mesh.aabb_max[0] += 1.5 * f32::from(n - 1);
        mesh
    }

    /// The positions and indices of [`cooked_quads`]`(n)`.
    fn quads_geometry(n: u16) -> (Vec<[f32; 3]>, Vec<u32>) {
        let mut positions = Vec::new();
        let mut indices = Vec::new();
        for q in 0..n {
            let x = f32::from(q) * 1.5;
            positions.extend([
                [x - 0.5, 0.0, -0.5],
                [x + 0.5, 0.0, -0.5],
                [x + 0.5, 0.0, 0.5],
                [x - 0.5, 0.0, 0.5],
            ]);
            indices.extend([0u32, 2, 1, 0, 3, 2].map(|i| i + 4 * u32::from(q)));
        }
        (positions, indices)
    }

    /// Writes `mesh` as the cooked `.fmesh` of `id` into `app`'s asset cache.
    fn cook_mesh(app: &App, id: AssetId, mesh: &regia_asset::CookedMesh) {
        let payload = mesh.to_postcard().expect("mesh payload");
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: regia_asset::AssetTypeTag::mesh().0,
            asset_id: id,
            importer_version: 1,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, &payload).expect("cooked file");
        let cache = app.world().resource::<AssetServer>().cache_dir().to_path_buf();
        std::fs::create_dir_all(&cache).expect("cache dir");
        regia_test_utils::write_atomic(&cache.join(cooked_file_name(id)), &file).expect("write");
    }

    /// rev-claude-60 finding 1 (ADR-7542): the bridge re-validates a counted stand-in's meshlets
    /// whenever its upload changes (the renderer's per-key upload serial), not only when the
    /// `.fmlt` does. A re-import whose `.fmlt` reloads one update before its `.fmesh` is first a
    /// mismatch against the old upload; when the new upload lands, the next update inserts the
    /// meshlets (before the fix the entity stayed a mismatch until the next `.fmlt` reload). A
    /// re-upload with the same counts inserts the payload again, so the renderer never draws the
    /// new upload with clusters it only kept from the old one.
    #[test]
    fn a_counted_reupload_revalidates_its_meshlets() {
        let device = match regia_render::RenderDevice::shared_headless() {
            Ok(device) => device,
            Err(err) => {
                assert!(
                    std::env::var("REGIA_GPU_STRICT").as_deref() != Ok("1"),
                    "REGIA_GPU_STRICT=1 and no adapter: {err}"
                );
                eprintln!("SKIP: no adapter ({err})");
                return;
            }
        };
        let mut app = app(server("meshlet-bridge-counted-reupload"), true);
        let uploads = regia_render::AssetMeshUploads::new(&device);
        let mesh_id = AssetId::new_random();
        cook_mesh(&app, mesh_id, &cooked_quads(1));
        let handle = {
            let mut server = app.world_mut().resource_mut::<AssetServer>();
            server.register_prepare(uploads.clone());
            let handle = server.load::<regia_asset::CpuMesh>(mesh_id).expect("load");
            server.wait_for_loads();
            handle
        };
        app.world_mut()
            .resource_mut::<RuntimeRenderer>()
            .0
            .set_asset_meshes(Some(uploads.clone()));
        let key = regia_render::asset_mesh_key(mesh_id);
        let counts = |app: &App| app.world().resource::<RuntimeRenderer>().0.asset_mesh_counts(key);
        assert_eq!(counts(&app), Some((4, 6)));

        // The re-import's `.fmlt` lands first: two quads against the one-quad upload.
        let (two, two_indices) = quads_geometry(2);
        let source = AssetId::new_random();
        cook(
            app.world().resource::<AssetServer>(),
            source,
            &MeshletMesh::single_level(&two, &two_indices).to_bytes(),
        );
        let entity = app
            .world_mut()
            .spawn((
                MeshRenderer::counted(handle, Arc::new(CpuMaterial::default())),
                MeshletSource::new(source),
            ))
            .id();
        app.update();
        assert_eq!(state(&app).forward_reason(entity), Some(ForwardReason::Mismatch));
        app.update();
        assert_eq!(state(&app).churn(), (0, 0));

        // Its `.fmesh` lands one update later: the upload now has the meshlets' counts.
        cook_mesh(&app, mesh_id, &cooked_quads(2));
        app.world_mut()
            .resource_mut::<AssetServer>()
            .reload_id(mesh_id)
            .expect("reload");
        assert_eq!(counts(&app), Some((8, 12)));
        app.update();
        assert_eq!(
            state(&app).forward_reason(entity),
            None,
            "the new upload matches the meshlets"
        );
        assert!(state(&app).on_visbuffer(entity));
        assert!(app.world().resource::<RuntimeRenderer>().0.has_meshlets(key));
        assert_eq!(state(&app).churn(), (1, 0));
        app.update();
        assert_eq!(state(&app).churn(), (1, 0), "steady: nothing inserted again");

        // A re-upload with the same counts: the payload is inserted again.
        app.world_mut()
            .resource_mut::<AssetServer>()
            .reload_id(mesh_id)
            .expect("reload");
        app.update();
        assert_eq!(state(&app).churn(), (2, 1), "replaced for the new upload");
        assert!(state(&app).on_visbuffer(entity));
        assert!(app.world().resource::<RuntimeRenderer>().0.has_meshlets(key));
    }

    /// A damaged, old, missing or mismatched payload is a status and one warning, never a
    /// panic, and the mesh stays forward.
    #[test]
    fn a_bad_payload_is_a_status_not_a_panic() {
        let mut app = app(server("meshlet-bridge-bad"), true);
        let old = grid(5);
        let (old_id, old_entity) = rock(&mut app, &old);
        let mut bytes = meshlets_for(&old).to_bytes();
        bytes[4] = 99; // format version 99
        cook(app.world().resource::<AssetServer>(), old_id, &bytes);
        let missing = grid(4);
        let missing_entity = app
            .world_mut()
            .spawn((
                MeshRenderer::new(Arc::clone(&missing), Arc::new(CpuMaterial::default())),
                MeshletSource::new(AssetId::new_random()),
            ))
            .id();
        let other = grid(3);
        let (other_id, other_entity) = rock(&mut app, &other);
        // Cooked for another mesh: the counts do not match.
        cook(
            app.world().resource::<AssetServer>(),
            other_id,
            &meshlets_for(&grid(10)).to_bytes(),
        );
        app.update();
        app.update();

        for (mesh, entity) in [(&old, old_entity), (&missing, missing_entity)] {
            assert!(!has(&app, mesh));
            assert_eq!(state(&app).forward_reason(entity), Some(ForwardReason::LoadError));
        }
        assert!(!has(&app, &other));
        assert_eq!(
            state(&app).forward_reason(other_entity),
            Some(ForwardReason::Mismatch)
        );
        assert_eq!(state(&app).mismatches().len(), 1);
        let assets = app.world().resource::<MeshletAssets>();
        let mut codes: Vec<&str> = assets.errors().map(|e| e.code).collect();
        codes.sort_unstable();
        assert_eq!(codes, ["meshlets_not_cooked", "meshlets_old_version"]);
        assert_eq!(assets.failures(), 2, "each bad payload failed (and warned) once");

        // Recooked and hot-reloaded: the mesh takes the path.
        cook(
            app.world().resource::<AssetServer>(),
            old_id,
            &meshlets_for(&old).to_bytes(),
        );
        let reloaded = app
            .world_mut()
            .resource_mut::<AssetServer>()
            .reload_id(old_id)
            .expect("reload");
        assert_eq!(reloaded, Some(regia_asset::LoadedKind::External));
        app.update();
        assert!(has(&app, &old));
        assert!(state(&app).on_visbuffer(old_entity));
    }

    /// A hot reload of the `.fmlt` and a changed mesh remove and insert again.
    #[test]
    fn hot_reload_and_mesh_change_insert_again() {
        let mut app = app(server("meshlet-bridge-reload"), true);
        let mesh = grid(8);
        let (id, entity) = rock(&mut app, &mesh);
        app.update();
        assert_eq!(state(&app).churn(), (1, 0));

        app.world_mut()
            .resource_mut::<AssetServer>()
            .reload_id(id)
            .expect("reload");
        app.update();
        assert!(has(&app, &mesh));
        assert_eq!(state(&app).churn(), (2, 1), "the reload replaced the meshlets");

        // The host swaps the mesh for an equal copy (a new `Arc`, a new key) with its meshlets.
        let copy = Arc::new(CpuMesh {
            vertices: mesh.vertices.clone(),
            indices: mesh.indices.clone(),
        });
        app.world_mut().entity_mut(entity).insert(MeshRenderer::new(
            Arc::clone(&copy),
            Arc::new(CpuMaterial::default()),
        ));
        app.update();
        assert!(!has(&app, &mesh), "the old key's meshlets were removed");
        assert!(has(&app, &copy), "the new key's were inserted");
        assert_eq!(state(&app).churn(), (3, 2));
    }

    /// The gate takes an adopted plant, opaque or masked, and says why it leaves other swaying and
    /// masked materials forward (ADR-3916).
    #[test]
    fn adopted_sway_and_masked_materials_pass_the_gate() {
        use regia_render::{ALPHA_BLEND, ALPHA_MASK, ALPHA_OPAQUE, FoliageSway, LabMaterial};
        let mesh = grid(4);
        let gate = |material: LabMaterial| {
            ineligible(
                &MeshRenderer::new(Arc::clone(&mesh), Arc::new(material)),
                false,
                false,
            )
        };
        let plain = |alpha_mode| LabMaterial {
            alpha_mode,
            alpha_cutoff: 0.5,
            ..LabMaterial::default()
        };
        let adopt = |material: &LabMaterial| FoliageSway::default().adopt(material).expect("adopt");
        assert_eq!(gate(plain(ALPHA_OPAQUE)), None);
        assert_eq!(gate(plain(ALPHA_MASK)), None, "a leaf card");
        assert_eq!(gate(adopt(&plain(ALPHA_OPAQUE))), None, "a swaying trunk");
        assert_eq!(gate(adopt(&plain(ALPHA_MASK))), None, "a swaying leaf card");
        assert_eq!(gate(adopt(&plain(ALPHA_BLEND))), Some(ForwardReason::Translucent));
        assert_eq!(gate(plain(ALPHA_BLEND)), Some(ForwardReason::Translucent));
        let custom = LabMaterial {
            surface_graph: Some(Arc::new(
                FoliageSway::default().graph("fn a_surface_of_its_own() {}"),
            )),
            ..LabMaterial::default()
        };
        assert_eq!(gate(custom), Some(ForwardReason::SwayGraphSurface));
        let graph = LabMaterial {
            surface_graph: Some(Arc::new(regia_render::surface_graph::SurfaceGraph::new(
                "fn a_surface_of_its_own() {}",
            ))),
            ..LabMaterial::default()
        };
        assert_eq!(gate(graph), Some(ForwardReason::GraphMaterial));
        let still = FoliageSway::still(&adopt(&plain(ALPHA_OPAQUE)));
        assert_eq!(
            gate(still),
            Some(ForwardReason::GraphMaterial),
            "a still copy keeps its graph surface, which the resolve cannot shade"
        );
    }

    /// A full layer of `resolution`² voxels around y = 0 over the `extent` × `extent` square of
    /// [`grid`], with its mip chain (whole cubes, as format v1 stores them).
    fn layer_proxy(resolution: u32, extent: f32) -> regia_render::visbuffer::VoxelFarProxy {
        use regia_render::visbuffer::{VoxelBrick, VoxelFarProxy, VoxelLevel, VoxelSample};
        let size0 = extent / resolution as f32;
        let mut proxy = VoxelFarProxy {
            origin: [0.0, -size0 * 0.5, 0.0],
            extent,
            bounds: [extent * 0.5, 0.0, extent * 0.5, extent * 0.71],
            source_triangles: 288,
            ..VoxelFarProxy::default()
        };
        let mut res = resolution;
        loop {
            let size = extent / res as f32;
            let mut bricks: std::collections::BTreeMap<[u32; 3], u64> = std::collections::BTreeMap::new();
            for z in 0..res {
                for x in 0..res {
                    *bricks.entry([x / 4, 0, z / 4]).or_default() |= 1u64 << (x % 4 + 16 * (z % 4));
                }
            }
            let brick_offset = proxy.bricks.len() as u32;
            for (b, mask) in &bricks {
                let voxel_offset = proxy.voxels.len() as u32;
                for _ in 0..mask.count_ones() {
                    proxy.voxels.push(VoxelSample::new(
                        [0.0, 1.0, 0.0],
                        [0.3, 0.5, 0.2],
                        1.0,
                        [0.5, 0.5],
                    ));
                }
                proxy.bricks.push(VoxelBrick {
                    coord: VoxelBrick::pack_coord(*b),
                    voxel_offset,
                    mask_lo: *mask as u32,
                    mask_hi: (*mask >> 32) as u32,
                });
            }
            proxy.levels.push(VoxelLevel {
                resolution: res,
                voxel_size: size,
                error: size * 1.732_050_8,
                brick_offset,
                brick_count: proxy.bricks.len() as u32 - brick_offset,
            });
            if res == 1 {
                break;
            }
            res /= 2;
        }
        proxy.validate().expect("valid layer proxy");
        proxy
    }

    /// Writes `payload` as the cooked voxel far file of `id`.
    fn cook_voxels(server: &AssetServer, id: AssetId, payload: &[u8]) {
        let header = CookedHeader {
            format_version: COOKED_FORMAT_VERSION,
            type_tag: TypePathHash::of(regia_render::visbuffer::VOXEL_FAR_TYPE_PATH),
            asset_id: id,
            importer_version: 1,
            dependencies: vec![],
            payload_len: payload.len() as u64,
        };
        let file = encode_cooked_file(&header, payload).expect("cooked file");
        std::fs::create_dir_all(server.cache_dir()).expect("cache dir");
        regia_test_utils::write_atomic(&server.cache_dir().join(cooked_file_name(id)), &file).expect("write");
    }

    /// A rock with its meshlets and a voxel far proxy cooked as `payload`.
    fn tree(app: &mut App, mesh: &Arc<CpuMesh>, payload: &[u8]) -> (AssetId, Entity) {
        let (_, entity) = rock(app, mesh);
        let id = AssetId::new_random();
        cook_voxels(app.world().resource::<AssetServer>(), id, payload);
        app.world_mut().entity_mut(entity).insert(VoxelFarSource::new(id));
        (id, entity)
    }

    /// A placed mesh entity with a `.fvox` takes the voxel levels under its own mesh key: the
    /// payload is checked against the mesh's vertex count, not the extended one (no mismatch),
    /// and the stats count what the levels added. A second entity with the same mesh shares it.
    #[test]
    fn a_voxel_far_source_gives_a_placed_mesh_the_far_levels() {
        let mut app = app(server("meshlet-bridge-voxel"), true);
        let mesh = grid(12);
        let (_, entity) = tree(&mut app, &mesh, &layer_proxy(4, 12.0).to_bytes());
        app.update();
        assert!(has(&app, &mesh), "inserted under the mesh's own key");
        assert!(state(&app).on_visbuffer(entity));
        assert!(
            state(&app).mismatches().is_empty(),
            "{:?}",
            state(&app).mismatches()
        );
        let stats = state(&app).voxel_far_stats();
        assert_eq!(
            (stats.entities, stats.meshes, stats.fallbacks),
            (1, 1, 0),
            "{stats:?}"
        );
        assert!(
            stats.levels >= 1 && stats.triangles > 0 && stats.vertices > 0,
            "{stats:?}"
        );
        assert!(stats.gpu_bytes >= stats.vertices * 64);
        assert_eq!(
            app.world()
                .resource::<RuntimeRenderer>()
                .0
                .visbuffer_arena_stats()
                .pending,
            1,
            "waiting for the first draw"
        );
        // Same mesh, same proxy: one more user of the same insert.
        let voxel = *app.world().get::<VoxelFarSource>(entity).expect("source");
        let meshlets = *app.world().get::<MeshletSource>(entity).expect("source");
        app.world_mut().spawn((
            MeshRenderer::new(Arc::clone(&mesh), Arc::new(CpuMaterial::default())),
            meshlets,
            voxel,
        ));
        app.update();
        assert_eq!(state(&app).voxel_far_stats().entities, 2);
        assert_eq!(state(&app).meshes_on_visbuffer(), 1);
        assert_eq!(state(&app).churn(), (1, 0));
    }

    /// A missing, damaged or old `.fvox` is a logged fallback to the plain meshlets, never a
    /// panic: the mesh stays on the visibility buffer and the fallback names the code. A recook
    /// and hot reload of the proxy, or removing the source, insert again.
    #[test]
    fn a_missing_or_damaged_voxel_proxy_falls_back_to_plain_meshlets() {
        let mut app = app(server("meshlet-bridge-voxel-bad"), true);
        let good = layer_proxy(4, 12.0).to_bytes();
        let truncated = &good[..good.len() - 7];
        let mut old = good.clone();
        old[4..8].copy_from_slice(&7u32.to_le_bytes());
        let meshes = [grid(12), grid(11), grid(10)];
        let (damaged_id, damaged) = tree(&mut app, &meshes[0], truncated);
        let (_, versioned) = tree(&mut app, &meshes[1], &old);
        let (_, missing) = rock(&mut app, &meshes[2]);
        app.world_mut()
            .entity_mut(missing)
            .insert(VoxelFarSource::new(AssetId::new_random()));
        for _ in 0..3 {
            app.update();
        }
        for (mesh, entity) in meshes.iter().zip([damaged, versioned, missing]) {
            assert!(has(&app, mesh), "plain meshlets still inserted");
            assert!(state(&app).on_visbuffer(entity));
        }
        let mut codes: Vec<&str> = state(&app).voxel_far_fallbacks().iter().map(|e| e.code).collect();
        codes.sort_unstable();
        assert_eq!(
            codes,
            [
                "voxel_far_invalid",
                "voxel_far_not_cooked",
                "voxel_far_old_version"
            ]
        );
        let stats = state(&app).voxel_far_stats();
        assert_eq!((stats.meshes, stats.fallbacks), (0, 3), "{stats:?}");
        assert_eq!(
            app.world().resource::<VoxelFarAssets>().failures(),
            3,
            "each bad proxy failed (and warned) once"
        );

        // Recooked and hot-reloaded: the levels arrive.
        cook_voxels(app.world().resource::<AssetServer>(), damaged_id, &good);
        app.world_mut()
            .resource_mut::<AssetServer>()
            .reload_id(damaged_id)
            .expect("reload");
        app.update();
        assert_eq!(state(&app).voxel_far_stats().meshes, 1);
        assert!(has(&app, &meshes[0]));
        let (inserts, removals) = state(&app).churn();

        // The source removed: the plain meshlets go back in.
        app.world_mut().entity_mut(damaged).remove::<VoxelFarSource>();
        app.update();
        assert_eq!(state(&app).voxel_far_stats().meshes, 0);
        assert!(has(&app, &meshes[0]));
        assert_eq!(state(&app).churn(), (inserts + 1, removals + 1));
    }

    #[test]
    fn forward_codes_are_protocol_safe() {
        for reason in ForwardReason::ALL {
            assert!(reason.code().bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }

    /// The bridge registers masked geometry; the frame decides whether the device can alpha-test it.
    #[test]
    fn masked_leaf_meshlets_are_inserted_and_follow_material_changes() {
        let mut app = app(server("meshlet-bridge-masked"), true);
        let mesh = grid(8);
        let (_, entity) = rock(&mut app, &mesh);
        let leaf = CpuMaterial {
            alpha_mode: regia_render::ALPHA_MASK,
            double_sided: true,
            ..CpuMaterial::default()
        };
        app.world_mut()
            .entity_mut(entity)
            .insert(MeshRenderer::new(Arc::clone(&mesh), Arc::new(leaf.clone())));
        app.update();
        assert!(has(&app, &mesh), "masked foliage needs its cooked meshlets");
        assert!(state(&app).on_visbuffer(entity));
        assert_eq!(state(&app).forward_reason(entity), None);

        let blended = CpuMaterial {
            alpha_mode: regia_render::ALPHA_BLEND,
            ..leaf.clone()
        };
        app.world_mut()
            .entity_mut(entity)
            .insert(MeshRenderer::new(Arc::clone(&mesh), Arc::new(blended)));
        app.update();
        assert!(!has(&app, &mesh));
        assert_eq!(
            state(&app).forward_reason(entity),
            Some(ForwardReason::Translucent)
        );

        app.world_mut()
            .entity_mut(entity)
            .insert(MeshRenderer::new(Arc::clone(&mesh), Arc::new(leaf)));
        app.update();
        assert!(
            has(&app, &mesh),
            "returning to a cutoff material registers it again"
        );
        assert_eq!(state(&app).churn(), (2, 1));
        app.insert_resource(settings(false));
        app.update();
        assert!(!has(&app, &mesh), "the opt-in profile still controls the bridge");
    }

    #[test]
    fn masked_leaf_gate_keeps_hair_parallax_and_transmission_forward() {
        let leaf = CpuMaterial {
            alpha_mode: regia_render::ALPHA_MASK,
            ..CpuMaterial::default()
        };
        let renderer = |material| MeshRenderer::new(grid(2), Arc::new(material));
        assert_eq!(ineligible(&renderer(leaf.clone()), false, false), None);
        let mut hair = leaf.clone();
        hair.hair = regia_render::HairFactors::strands();
        let mut parallax = leaf.clone();
        parallax.hard_surface.pom_scale = 0.1;
        let transmitting = CpuMaterial {
            transmission: 0.5,
            ..leaf
        };
        for material in [hair, parallax, transmitting] {
            assert_eq!(
                ineligible(&renderer(material), false, false),
                Some(ForwardReason::Translucent)
            );
        }
    }
}
