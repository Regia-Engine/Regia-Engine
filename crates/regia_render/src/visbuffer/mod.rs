// SPDX-License-Identifier: MPL-2.0
//! Visibility buffer + cluster (meshlet) LOD: the opt-in geometry path for dense scenes
//! (`docs/specs/27-renderer-excellence.md` § 4.2 stage 3, `specs/26` § 4 high-detail geometry).
//!
//! With [`crate::RenderProfile::visibility_buffer`] on, rigid opaque meshes that have a
//! [`MeshletMesh`] skip the forward depth prepass and color pass, and so do alpha-tested
//! (`ALPHA_MASK`) ones on a texture-table device (ADR-3915: their clusters draw with an
//! alpha-tested raster that samples the base color through the table). Instead, per frame:
//!
//! 1. **Cull** (compute): instances are frustum-culled; every cluster of a visible instance
//!    takes the LOD cut test (screen-space error of its simplification group, [`LodView`]),
//!    the frustum and a normal-cone test, then two-phase Hi-Z occlusion: clusters visible last
//!    frame draw first, the pyramid is built from that depth, the rest are tested against it.
//! 2. **Visibility pass**: one instanced draw per phase writes `(cluster, triangle)` IDs into
//!    an `R32Uint` target and depth into the scene depth buffer (so SSAO, decals, fog and the
//!    forward meshes see it).
//! 3. **Material resolve**: each material shades its own pixels with the forward shader's
//!    fragment body (`lab_forward.wgsl` composed with `VISBUFFER`), so lighting, shadows, GI,
//!    fog and the tonemap are the forward path's, term for term.
//!
//! Meshlets come from the cooker (`regia_asset_pipeline::meshlet`) either ahead of time
//! ([`crate::FrameRenderer::insert_meshlets`]) or through a [`MeshletBuilder`] that the
//! renderer calls once per new mesh ([`crate::FrameRenderer::set_meshlet_builder`]). Meshes
//! without meshlets stay on the forward path. A game's cooked `.fmlt` sub-assets load with
//! [`MeshletMesh::load`]; `regia_framework::meshlet_bridge` does that for every rigid opaque
//! mesh entity that names one and inserts them under its mesh key while the profile has the
//! path on (`book/agents/visbuffer.md`).
//!
//! **Cluster memory** (ADR-353): every resident mesh's cluster records and geometry words
//! (vertices, cluster vertex lists, triangles) are ranges of one buffer per stream, taken from a
//! best-fit coalescing free list. [`crate::FrameRenderer::remove_meshlets`],
//! [`crate::FrameRenderer::release_meshes`] and a changed mesh return their ranges, which the
//! next mesh reuses, so a streaming map creates no buffer once warm. A stream that runs out of
//! room doubles up to a cap (logged once when hit); a mesh past the cap draws forward.
//! [`crate::FrameRenderer::visbuffer_arena_stats`] reports the occupancy, and
//! [`crate::FrameRenderer::visbuffer_create_stats`] every GPU object the path created, in total
//! and in the last frame (none once warm, ADR-355).
//!
//! **Cluster residency** ([`ClusterResidency`], ADR-3118, opt-in): the cluster data of meshes
//! made resident while it is on stays within a byte budget per tier. Coarse bands of each mesh's
//! DAG are always resident, finer bands load as the view's screen error asks for them (bounded
//! upload bytes per frame, LRU eviction), and the finest resident band acts as the cut's leaves
//! through the cluster records, so the cull is unchanged.
//!
//! **Shadows** ([`VisBufferSettings::cluster_shadows`]): the same instances are culled per
//! shadow job — each sun cascade, each redrawn sun page, each local-light atlas tile — against
//! the job's light-space region, and each cluster takes the LOD cut for the job's texel size
//! (an orthographic cascade projects an error by its texels per metre at any distance). The
//! selected clusters are drawn with one indirect depth-only draw per job inside the renderer's
//! shadow passes, so a dense scatter's casters cost no CPU draws and no full-detail triangles.
//!
//! **Software raster** ([`VisBufferSettings::software_raster`], ADR-1887): clusters that project
//! to a few pixels rasterize in compute instead of through the hardware rasterizer, with two
//! 32-bit atomics per pixel (a depth pass, then an ID pass), and merge into the ID target and the
//! depth before the Hi-Z build of their phase. Everything downstream reads one visibility buffer.
//!
//! **Selection** ([`VisibilityPath`], ADR-2071): which frames take the path, and whether the
//! resolve shades all materials in one draw (`single_pass`). Both off by default.
//!
//! **Voxel far proxies** ([`VoxelFarProxy`], ADR-2445): past a mesh's last cluster LOD, a cooked
//! sparse-brick voxel stand-in takes over as further levels of the same DAG
//! ([`VoxelFarProxy::extend_meshlets`]), so the cut moves onto it with no swap pop, in the main
//! view and in every shadow job.
//!
//! **Persistent instance sets** ([`crate::FrameRenderer::set_instance_set`]): a scatter of
//! many instances of one mesh is uploaded once and drawn without a [`crate::FrameMesh`] per
//! instance. LOD switch telemetry mirrors the boundary comparisons on the CPU (ADR-4631).
//! Sets draw only through this path.

mod arena;
mod creates;
mod dense;
pub(crate) mod gpu;
pub(crate) mod meshlet;
mod residency;
pub(crate) mod single_pass;
pub mod sway;
mod voxel_far;

use std::sync::Arc;

pub use dense::{
    DENSE_CROSSOVER_MARGIN, DENSITY_TILE_PX, DenseBenchRow, DenseCrossover, DenseGateReport,
    DenseSceneSelect, DensityMetrics, SceneDensity, SinglePassFallback, VisibilityDefault,
    VisibilityDefaultState, VisibilityPath, VisibilitySelect,
};
pub(crate) use gpu::{RESOLVE_STORAGE_BUFFERS, VisBufferPass, VisShadowFrame, VisShadowJob, VisView};
pub use single_pass::VisResolveReport;

pub use meshlet::{
    AppendedVertices, LodView, MAX_MESHLET_TRIANGLES, MAX_MESHLET_VERTICES, MESHLET_FORMAT_VERSION,
    MESHLET_TYPE_PATH, Meshlet, MeshletFormatError, MeshletLoadError, MeshletMesh, MeshletStreamHead,
    ROOT_ERROR, cooked_mesh_lod0, meshlet_sub_path, sphere_of_points,
};
pub use residency::{
    BandDelivery, BandDisk, BandDiskError, BandPayload, BandSections, ClusterResidency, RESIDENCY_BANDS,
    VisBufferResidencyStats,
};
pub use voxel_far::MAX_VOXEL_COVERAGE;
pub use voxel_far::{
    MAX_VOXEL_FAR_RESOLUTION, VOXEL_BRICK_EDGE, VOXEL_FAR_FORMAT_VERSION, VOXEL_FAR_TYPE_PATH, VoxelBrick,
    VoxelFarFormatError, VoxelFarLevelUse, VoxelFarLoadError, VoxelFarMesh, VoxelFarProxy, VoxelFarSettings,
    VoxelLevel, VoxelSample, VoxelSurface, cooked_voxel_far_file_version, oct_decode, oct_encode,
    voxel_dither, voxel_far_migration_sources, voxel_far_schema_fingerprint, voxel_far_sub_path,
};
pub use voxel_far::{
    VOXEL_EXTENT_FULL, VOXEL_EXTENT_INEXACT, VOXEL_EXTENT_STEPS, VoxelShape, migrate_cooked_voxel_far_file,
    pack_voxel_extent, unpack_voxel_extent, voxel_cube_error, voxel_extent_error,
};

/// Builds the cluster hierarchy of a mesh from its vertices and triangle indices, or `None`
/// to keep that mesh on the forward path. Tools pass `regia_asset_pipeline`'s cooker.
pub type MeshletBuilder = Arc<dyn Fn(&[crate::LabVertex], &[u32]) -> Option<MeshletMesh> + Send + Sync>;

/// Many instances of one mesh kept on the GPU between frames
/// ([`crate::FrameRenderer::set_instance_set`]): a static scatter (rocks, debris) with no
/// per-instance CPU work per frame.
#[derive(Clone, Copy, Debug)]
pub struct VisInstanceSet<'a> {
    /// The mesh's key (as a [`crate::FrameMesh::mesh_key`]); its clusters come from
    /// [`crate::FrameRenderer::insert_meshlets`] or the builder.
    pub mesh_key: u64,
    /// The mesh's vertices (made resident once).
    pub vertices: &'a [crate::LabVertex],
    /// The mesh's triangle indices.
    pub indices: &'a [u32],
    /// The material's key (as a [`crate::FrameMesh::material_key`]).
    pub material_key: u64,
    /// The material: rigid (no surface graph, or the sway of [`crate::FoliageSway::adopt`], ADR-3916),
    /// opaque or alpha-tested (`ALPHA_MASK`: alpha-tested clusters on a texture-table device,
    /// ADR-3915); it is not checked again. A masked set casts no cluster shadows whatever
    /// [`Self::shadow`] says (the shadow raster has no alpha test), and where its test cannot run
    /// (no table, hair cards, parallax) it draws nothing: a set has no forward path to stay on.
    pub material: &'a crate::LabMaterial,
    /// One model matrix per instance.
    pub models: &'a [[[f32; 4]; 4]],
    /// Shadow policy of every instance (unless [`Self::instance_shadows`] is given).
    /// [`crate::ShadowCast::Local`] picks the cascades on the GPU from the instance's own
    /// centre (its `anchor` is not used).
    pub shadow: crate::ShadowCast,
    /// One shadow policy per instance (same length as [`Self::models`]), or empty.
    pub instance_shadows: &'a [crate::ShadowCast],
}

/// Fraction of [`VisBufferSettings::lod_threshold_px`] used as the LOD hysteresis band when
/// the pass does not set one (ADR-4631).
///
/// A projected error that stays inside the band keeps the level stored last frame, so a camera
/// move of a fraction of a pixel does not swap the cluster. Luebke, "A Developer's Survey of
/// Polygonal Simplification Algorithms", IEEE Computer Graphics and Applications 21(3), 2001,
/// describes popping when a view-dependent cut sits on the switch between levels. The band is
/// this project's rule.
/// Transition note: Luebke (2001), <https://doi.org/10.1109/38.920624>; the band fraction is original.
pub const LOD_HYSTERESIS_FRACTION: f32 = 0.125;

/// Visibility-buffer knobs.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VisBufferSettings {
    /// Largest projected simplification error a drawn cluster may have, pixels.
    pub lod_threshold_px: f32,
    /// Band around [`Self::lod_threshold_px`], in pixels (ADR-4631). `None` uses
    /// [`LOD_HYSTERESIS_FRACTION`] of the threshold. `Some(0.0)` is today's hard cut.
    pub lod_hysteresis_px: Option<f32>,
    /// Clusters the caller asks each cull phase to draw.
    ///
    /// The cull uses this value clamped to 1024..=16_777_216 (`1 << 24` slots) and does not
    /// rewrite the stored field. A setting below 1024 still draws up to 1024 clusters; a setting
    /// above the cap draws at most the cap. Clusters past that clamped capacity are counted in
    /// [`VisBufferCounters::dropped_clusters`] and not drawn.
    pub max_clusters: u32,
    /// Two-phase Hi-Z occlusion culling of clusters (off = frustum, cone and LOD only).
    pub occlusion: bool,
    /// Visibility-buffer instances cast their shadows as culled, LOD-cut clusters (off: the
    /// mesh-list instances go through the forward shadow passes, and persistent sets cast none).
    pub cluster_shadows: bool,
    /// Largest projected simplification error a shadow cluster may have, shadow-map texels.
    pub shadow_lod_threshold_px: f32,
    /// Clusters at most [`Self::software_raster_max_px`] across rasterize in compute (ADR-1887).
    /// Off by default: it needs a GPU measurement before a preset turns it on.
    pub software_raster: bool,
    /// Largest projected cluster diameter the software rasterizer takes, pixels (at most 64).
    pub software_raster_max_px: u32,
    /// Cluster residency override (ADR-3118): `None` follows the profile
    /// ([`crate::RenderProfile::cluster_residency`]), `Some` replaces it (`Some(ClusterResidency::OFF)`
    /// turns it off).
    pub residency: Option<ClusterResidency>,
}

impl Default for VisBufferSettings {
    fn default() -> Self {
        Self {
            lod_threshold_px: 1.0,
            lod_hysteresis_px: None,
            max_clusters: 1 << 20,
            occlusion: true,
            cluster_shadows: true,
            shadow_lod_threshold_px: 1.0,
            software_raster: false,
            software_raster_max_px: 16,
            residency: None,
        }
    }
}

impl VisBufferSettings {
    /// The band in pixels this frame: the set value, or [`LOD_HYSTERESIS_FRACTION`] of the
    /// threshold. A non-positive or non-finite set value, or a non-finite threshold, is the
    /// hard cut (a band of 0).
    #[must_use]
    pub fn resolved_lod_hysteresis_px(&self) -> f32 {
        match self.lod_hysteresis_px {
            Some(px) if px.is_finite() && px > 0.0 => px,
            Some(_) => 0.0,
            None => {
                let band = LOD_HYSTERESIS_FRACTION * self.lod_threshold_px.max(0.0);
                if band.is_finite() { band } else { 0.0 }
            }
        }
    }
}

/// What the visibility-buffer path did in the last frame (CPU-side counts).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VisBufferStats {
    /// Frames that drew through the path.
    pub frames: u64,
    /// Instances submitted last frame.
    pub instances: u32,
    /// Clusters of those instances (the cull's candidates, every LOD level).
    pub candidate_clusters: u64,
    /// Distinct materials resolved last frame.
    pub materials: u32,
    /// Meshes with resident cluster geometry.
    pub resident_meshes: u32,
    /// Bytes of resident cluster geometry (vertices, cluster lists, triangles, cluster records).
    pub geometry_bytes: u64,
    /// Meshes the builder declined (they draw forward).
    pub declined_meshes: u32,
    /// Lightmap receiver instances on the path (ADR-2107).
    pub receiver_instances: u32,
    /// Materials with a receiver instance: the ones the per-material resolve shades through its
    /// `LIGHTMAP` variant (ADR-2689); the others keep the plain shade pipeline.
    pub receiver_materials: u32,
    /// Masked (alpha-tested) instances on the path (ADR-3915).
    pub masked_instances: u32,
    /// Distinct materials of those instances.
    pub masked_materials: u32,
    /// The masked clusters were alpha-tested (the frame bound the texture table); false with
    /// masked instances means their fragments were discarded without writing IDs or depth.
    pub masked_tested: bool,
    /// Swaying instances on the path (ADR-3916).
    pub sway_instances: u32,
    /// Distinct materials of those instances.
    pub sway_materials: u32,
    /// Some sway moves this frame (not calm), so the cull grew those instances' bounds.
    pub sway_moving: bool,
    /// Swaying meshes that stayed on the forward path, per [`sway::SwaySkip::index`].
    pub sway_forward: [u32; sway::SwaySkip::ALL.len()],
    /// Instances whose selected cluster cut changed this frame (including off-screen ones).
    /// A cut, or the first stored boundary decisions, is not
    /// a change (ADR-4631).
    pub lod_switches: u32,
    /// Stored cluster LOD histories discarded since the renderer was built: camera cuts, a
    /// changed instance list, a replaced history buffer. Each window view keeps its own history,
    /// so a split-screen view change is not a discard (ADR-8620); a fresh view's first frame is
    /// not one either.
    pub lod_history_discards: u64,
    /// Buffer copies of LOD history words between window views since the renderer was built
    /// (one out and one in per view change in split screen; one view never copies, ADR-8620).
    pub lod_history_copies: u64,
    /// Window views that own buffers the pass parks their cluster visibility bits or LOD history
    /// in while another view draws (ADR-7329, ADR-8620): 0 with one view, one per view in split
    /// screen. A view not drawn for [`crate::OCCLUSION_VIEW_RETIRE_ROUNDS`] rounds is retired and
    /// no longer counts.
    pub view_histories: u32,
    /// GPU bytes of those buffers.
    pub view_history_bytes: u64,
}

/// Occupancy of the visibility buffer's cluster arena
/// ([`crate::FrameRenderer::visbuffer_arena_stats`], ADR-353). Bytes count both streams: the
/// cluster records (96 bytes each) and the geometry words (vertices, cluster vertex lists and
/// triangles, 4 bytes each).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisBufferArenaStats {
    /// Meshes with resident clusters.
    pub meshes: u32,
    /// Cooked meshlets inserted but not drawn yet (made resident at the mesh's first draw).
    pub pending: u32,
    /// Cluster records resident.
    pub clusters: u64,
    /// Cluster records the cluster buffer holds.
    pub cluster_capacity: u64,
    /// Geometry words resident.
    pub geometry_words: u64,
    /// Geometry words the geometry buffer holds.
    pub geometry_capacity_words: u64,
    /// Bytes allocated to resident meshes.
    pub used_bytes: u64,
    /// Bytes of the buffers not allocated to a mesh.
    pub free_bytes: u64,
    /// Bytes of both buffers (`used_bytes + free_bytes`).
    pub capacity_bytes: u64,
    /// Free ranges over both streams (2 with no fragmentation).
    pub free_ranges: u64,
    /// Times a stream doubled since the renderer was made.
    pub growths: u64,
    /// GPU buffers created for the arena since the renderer was made (one per growth that
    /// reached a frame, plus the first of each stream).
    pub buffer_creates: u64,
    /// A stream hit its cap; meshes that did not fit draw forward.
    pub capped: bool,
    /// What cluster residency did (ADR-3118); zeros while it is off.
    pub residency: VisBufferResidencyStats,
}

/// GPU objects the visibility-buffer path created, and the bytes it uploaded, through the
/// renderer's tracked device (ADR-355). Cluster buffers, instance and cull lists, parameter
/// blocks, the shadow cull's lists, the ID, material-depth and Hi-Z targets, their bind groups
/// and the path's pipelines all count. The blocking counter readbacks
/// ([`crate::FrameRenderer::visbuffer_counters`]) are diagnostics and do not.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisBufferCreates {
    /// Render and compute pipelines.
    pub pipelines: u64,
    /// Bind groups.
    pub bind_groups: u64,
    /// GPU buffers.
    pub buffers: u64,
    /// Bytes of those buffers.
    pub buffer_bytes: u64,
    /// Textures.
    pub textures: u64,
    /// Bytes of those textures.
    pub texture_bytes: u64,
    /// Bytes written with `write_buffer` (instances, parameters, arena ranges).
    pub upload_bytes: u64,
}

/// What the visibility-buffer path created ([`crate::FrameRenderer::visbuffer_create_stats`],
/// ADR-355). Once warm, a frame creates nothing: buffers are replaced only when a count passes
/// their capacity (and the cluster arena reuses freed ranges, ADR-353), bind groups only when a
/// buffer or target they bind is replaced, targets only on a size change, and pipelines once per
/// variant. A frame runs from the renderer's frame start to the next one, so between two frames
/// [`Self::last_frame`] is the frame just drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisBufferCreateStats {
    /// Since the renderer was made.
    pub total: VisBufferCreates,
    /// During the last frame.
    pub last_frame: VisBufferCreates,
    /// Frames counted (every frame of the renderer's main view, with or without visibility
    /// instances).
    pub frames: u64,
    /// Frames in a row, ending with the last one, that created no pipeline, bind group, buffer
    /// or texture through the path (uploads do not count).
    pub quiet_frames: u64,
}

/// The GPU cull's counters for one frame ([`crate::FrameRenderer::visbuffer_counters`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VisBufferCounters {
    /// Clusters drawn in phase 1 (visible last frame).
    pub phase1_clusters: u32,
    /// Clusters drawn in phase 2 (newly visible after the Hi-Z test).
    pub phase2_clusters: u32,
    /// 64-cluster work items of the frustum-visible instances.
    pub work_items: u32,
    /// Clusters past the cull capacity (not drawn).
    ///
    /// The capacity is [`VisBufferSettings::max_clusters`] clamped to 1024..=16_777_216, not the
    /// raw stored value. A setting of 8 drops nothing until more than 1024 clusters pass the
    /// tests; a setting above 16_777_216 drops the rest.
    pub dropped_clusters: u32,
    /// Clusters rasterized in compute, in both phases ([`VisBufferSettings::software_raster`]);
    /// not counted in [`Self::phase1_clusters`] and [`Self::phase2_clusters`].
    pub software_clusters: u32,
    /// Masked clusters submitted to the raster in both phases (ADR-3915); counted in
    /// [`Self::phase1_clusters`] and [`Self::phase2_clusters`] too. Without the alpha-test binding,
    /// their fragments are discarded ([`VisBufferStats::masked_tested`]).
    pub masked_clusters: u32,
}

/// Counted meshes (drawn from their GPU upload, ADR-7284) on the cluster path
/// ([`crate::FrameRenderer::counted_vis_stats`], ADR-7462).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CountedVisStats {
    /// Counted meshes whose clusters are resident, their vertices repacked from the upload.
    pub meshes: u32,
    /// Their cluster records.
    pub clusters: u64,
    /// Vertex repacks dispatched since the renderer was made (one per admitted mesh).
    pub repacks: u64,
    /// Counted whole draws of the last visibility-buffer frame that drew forward for want of
    /// clusters (no cooked meshlets under the key, counts that do not match them, a vertex
    /// buffer without `STORAGE`, cluster residency on, the arena at its cap).
    pub forward_fallbacks: u32,
    /// CPU bytes of the cooked meshlets the renderer keeps for the counted meshes it admitted
    /// (cluster records, cluster vertex lists, triangles): what a re-upload under the same key (a
    /// hot reload) is admitted with again, as no CPU vertices are left to cook them from (ADR-7542).
    /// Shared with the host when it holds the same `Arc` (the meshlet bridge's loaded payload).
    pub kept_meshlet_bytes: u64,
}

#[cfg(test)]
mod tests;
