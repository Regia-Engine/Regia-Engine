// SPDX-License-Identifier: MPL-2.0
//! GPU side of the visibility-buffer path: the cluster geometry arena, per-frame instances,
//! the cull / Hi-Z / visibility passes and the material-resolve pipelines.

use crate::frame_work::counted::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;

mod counted_vertices;
mod levels;
mod lightmap;
mod lod_views;
mod resident;
mod sway;
mod warm;

use super::arena::{ClusterArena, ClusterKey};
use super::creates::CreateLedger;
use super::dense::DenseGate;
use super::meshlet::{Meshlet, MeshletMesh};
use super::single_pass::{SinglePass, SinglePassTarget};
use super::{
    DenseGateReport, MeshletBuilder, SceneDensity, VisBufferArenaStats, VisBufferCreateStats,
    VisBufferSettings, VisBufferStats, VisInstanceSet, VisResolveReport, VisibilityPath, VisibilitySelect,
};
use crate::device::{TrackedDevice, TrackedQueue};
use crate::shader_lib::compose_source;
use crate::{LabMaterial, LabVertex, RenderError, ShadowCast};

/// Words per vertex in the arena: position, uv.x, normal, uv.y, tangent, colour.
const VERTEX_WORDS: u32 = 16;
/// `state` buffer words (see `vis_cull.wgsl`).
const STATE_WORDS: u64 = 72;
/// First word of the masked clusters' four DrawIndirects in `state` (`vis_cull.wgsl` `MASK_ARGS`):
/// phase 1 back-face-culled, phase 1 double-sided, then phase 2's (ADR-3915).
const MASK_ARGS_WORD: u64 = 52;
// The masked DrawIndirects fit `state`.
const _: () = assert!(MASK_ARGS_WORD + 16 <= STATE_WORDS);
/// First word of the work-item scan's dispatch in `state` (`vis_cull.wgsl` `SCAN_ARGS`).
const SCAN_ARGS_WORD: u64 = 32;
/// Byte offset of the work-item scan's dispatch in the `dispatch_args` copy.
const SCAN_ARGS_OFFSET: u64 = 32;
/// Elements per block of the cull's prefix sums (`vis_cull.wgsl` `SCAN_BLOCK`).
const SCAN_BLOCK: u64 = 256;
/// First word of the double-sided clusters' DrawIndirect pair in `state` (`vis_cull.wgsl`).
const DS_ARGS_WORD: u64 = 16;
/// The Hi-Z pyramid's bind groups.
#[derive(Default)]
struct HizGroups {
    /// Depth to the base level, with the depth view it binds.
    copy: Option<(wgpu::TextureView, wgpu::BindGroup)>,
    /// Each level from the one above, level 1 first; replaced with the targets.
    down: Vec<wgpu::BindGroup>,
}

/// [`GpuInstance::flags`]`[1]`: a double-sided instance (glTF `doubleSided`): no cone cull, and
/// its clusters draw with the cull-none raster pipelines from the end of the cluster lists, or in
/// the software rasterizer, which keeps their back faces (ADR-2689).
pub(super) const VIS_DOUBLE_SIDED: u32 = 1;
/// [`GpuInstance::flags`]`[1]`: an alpha-tested instance (ADR-3915): its clusters sit after the
/// opaque ones in each list and draw with the alpha-tested raster, never in the software range.
pub(super) const VIS_MASKED: u32 = 2;
/// Words of the masked lists' header after the cluster lists (one entry of two words per phase).
const MASK_HEADER_WORDS: u64 = 4;
/// Visibility IDs: `phase << 31 | slot << 7 | triangle`; the slot has 24 bits.
const MAX_CLUSTER_SLOTS: u32 = 1 << 24;
/// Material slots the 16-bit material depth can name (0 = no material).
const MAX_MATERIAL_SLOTS: usize = 65534;
/// Where [`VisBufferPass::begin_frame`] rebuilds the table, for the tests that reach it.
#[cfg(test)]
pub(crate) const MATERIAL_SLOTS_REBUILD_AT: usize = MAX_MATERIAL_SLOTS / 4 * 3;
/// Instance cap: `cull_instances` indexes 64 × 65535 × 65535 threads; this keeps buffers sane.
const MAX_INSTANCES: usize = 1 << 22;
// `scan_top` in `vis_cull.wgsl` scans the block sums in one workgroup, 256 per round: the
// instance cap keeps that at 64 rounds (ADR-2294).
const _: () = assert!(MAX_INSTANCES as u64 <= 64 * SCAN_BLOCK * SCAN_BLOCK);
/// Storage buffers the resolve fragment stage binds: the forward view group's seven (bindings 2, 3,
/// 4, 10, 19, 21 and 25 of `frame/pipelines.rs`) plus the resolve group's four. It was 10 while
/// the view group had seven, so a device with exactly 10 took the path and failed the resolve
/// layout (ADR-4906, `visbuffer::tests::close`).
pub(crate) const RESOLVE_STORAGE_BUFFERS: u32 = 11;

/// The visibility target format.
pub(crate) const VIS_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::R32Uint;
/// The material-ID depth format of the resolve pass.
pub(crate) const MATERIAL_DEPTH_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Depth16Unorm;

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct GpuInstance {
    model: [[f32; 4]; 4],
    sphere: [f32; 4],
    info: [u32; 4],
    /// x = the stretch upper bound (`max_stretch`), y = sway reach in metres; z and w = where the
    /// mesh's voxel far levels start, in world units: z = their error (0: none), w = what their LOD sphere adds (ADR-3916).
    scale: [f32; 4],
    /// Shadow scale, shadow lift, shadow reach (`vis_shadow.wgsl`).
    shadow: [f32; 4],
    /// x = shadow flags ([`SHADOW_CASTS`], [`SHADOW_GPU_MASK`], cascade mask, cascade cap),
    /// y = [`VIS_DOUBLE_SIDED`] | [`VIS_MASKED`], with the sway record + 1 from bit 8 (ADR-3916),
    /// z and w = the mesh's level runs (first record, count; `levels`).
    flags: [u32; 4],
}

/// The material is alpha-tested the way the masked raster tests it (ADR-3915): `ALPHA_MASK`, no
/// hair cards (their alpha has its own terms) and no parallax (the forward UV moves with it).
pub(crate) fn alpha_tests(material: &LabMaterial) -> bool {
    material.alpha_mode == crate::lab_frame::ALPHA_MASK
        && crate::hair::effective_alpha_mode(material) == crate::lab_frame::ALPHA_MASK
        && !material.hair.enabled
        && material.hard_surface.pom_scale <= 1e-5
}

/// What a frame binds for the masked raster ([`VisBufferPass::bind_masked`]).
struct MaskedBind {
    /// The single-pass resolve's group: the material arena and the frame's slot map.
    group: wgpu::BindGroup,
    /// The frame's texture table group.
    table: wgpu::BindGroup,
    /// The arena's record stride in words.
    stride_words: u32,
    /// The frame's texture mip bias.
    mip_bias: f32,
}

/// The instance is drawn into shadow maps.
const SHADOW_CASTS: u32 = 0x100;
/// The instance's sun cascades are picked on the GPU from its reach and cap (bits 12–15).
const SHADOW_GPU_MASK: u32 = 0x200;

/// Shadow fields of an instance ([`GpuInstance::shadow`], [`GpuInstance::flags`]). `gpu_mask`
/// lets the cull pick a [`ShadowCast::Local`] caster's cascades (persistent sets, which have no
/// CPU pass per frame); otherwise the cascade bits come from [`VisBufferPass::set_shadow_mask`].
fn shadow_fields(shadow: ShadowCast, gpu_mask: bool) -> ([f32; 4], u32) {
    match shadow {
        ShadowCast::Off => ([1.0, 0.0, 0.0, 0.0], 0),
        ShadowCast::All => (
            [1.0, 0.0, 0.0, 0.0],
            SHADOW_CASTS | if gpu_mask { 0xff } else { 0 },
        ),
        ShadowCast::Local {
            reach,
            max_cascades,
            shadow_scale,
            shadow_lift,
            ..
        } => {
            // As `frame.rs` `shadow_model`: a factor that is not finite and above 0 leaves the
            // caster as drawn (a negative one made every LOD error negative: only roots cast).
            let scale = if shadow_scale.is_finite() && shadow_scale > 0.0 {
                shadow_scale
            } else {
                1.0
            };
            let lift = if shadow_lift.is_finite() { shadow_lift } else { 0.0 };
            let flags = if gpu_mask {
                SHADOW_CASTS | SHADOW_GPU_MASK | (u32::from(max_cascades.min(15)) << 12)
            } else {
                SHADOW_CASTS
            };
            ([scale, lift, reach.max(0.0), 0.0], flags)
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct CullParams {
    view_proj: [[f32; 4]; 4],
    planes: [[f32; 4]; 6],
    eye: [f32; 4],
    screen: [f32; 4],
    forward: [f32; 4],
    counts: [u32; 4],
    /// Regions of the `scan` buffer in 16-byte elements: class words before `x`, the scanned
    /// values before `y`, the block sums from `y` (`vis_cull.wgsl`).
    scan: [u32; 4],
    /// LOD hysteresis (ADR-4631): x = band in pixels as f32 bits, y bit 0 = clear the stored
    /// level, z = instance slots in the history buffer, w unused.
    lod: [u32; 4],
}

/// `ROOT` in `vis_cull.wgsl`: a parent error at or above this is a root cluster.
const LOD_CUT_ROOT: f32 = 3.0e38;

/// What `VisBufferPass::settle_lod` hands the cull for this frame.
struct LodFrame {
    switches: u32,
    /// `CullParams.lod.x`: the band in pixels, as f32 bits.
    band_bits: u32,
    /// `CullParams.lod.y`: bit 0 clears the stored level.
    clear: u32,
    /// `CullParams.lod.z`: instance slots in the history buffer.
    slots: u32,
}

/// The LOD cut the CPU mirror shares with `vis_cull.wgsl`.
struct LodCut {
    threshold: f32,
    band: f32,
    pixels_per_unit: f32,
    near: f32,
    eye: [f32; 3],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct RasterParams {
    view_proj: [[f32; 4]; 4],
    info: [u32; 4],
    /// The masked raster's alpha test (`vis_raster.wgsl`).
    mask: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct VelocityParams {
    view_proj: [[f32; 4]; 4],
    unjittered: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    info: [u32; 4],
    screen: [f32; 4],
}

/// The TAAU velocity target's format (`taau.rs`).
const VELOCITY_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

#[repr(C)]
#[derive(Clone, Copy, Debug, bytemuck::Pod, bytemuck::Zeroable)]
struct ResolveParams {
    view_proj: [[f32; 4]; 4],
    info: [u32; 4],
    screen: [f32; 4],
}

/// The vertices a mesh's clusters are made from (ADR-7462).
#[derive(Clone, Copy)]
pub(super) enum VertexInput<'a> {
    /// CPU vertices and indices (a builder may cook meshlets from them).
    Cpu(&'a [LabVertex], &'a [u32]),
    /// A counted upload's vertex and index counts: cooked meshlets only, the base vertices
    /// repacked on the GPU from the upload.
    Gpu {
        /// Vertices in the upload.
        vertices: usize,
        /// Indices in the upload.
        indices: usize,
    },
}

impl VertexInput<'_> {
    /// Vertices and indices.
    fn counts(self) -> (usize, usize) {
        match self {
            Self::Cpu(vertices, indices) => (vertices.len(), indices.len()),
            Self::Gpu { vertices, indices } => (vertices, indices),
        }
    }
}

/// What [`VisBufferPass::admit_mesh`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Admit {
    /// The mesh draws forward.
    Forward,
    /// Its clusters are resident (were, or are now).
    Resident,
    /// Its clusters are resident now and its `vertices` base vertices are the GPU-owned range at
    /// geometry word `dst_word`, to be filled from the upload (ADR-7462).
    Repack {
        /// The mesh's arena key (the range is dropped with it).
        key: ClusterKey,
        /// First geometry word of the range.
        dst_word: u32,
        /// Vertices to repack.
        vertices: u32,
    },
}

#[derive(Clone, Copy, Debug)]
struct MeshSlot {
    meshlet_base: u32,
    meshlet_count: u32,
    vertex_count: usize,
    index_count: usize,
    bounds: [f32; 4],
    /// The mesh's ranges in the cluster arena.
    key: ClusterKey,
    /// The first geometry word of its vertices (a lightmap receiver's UV2 is found from it).
    vertex_base: u32,
    /// Its level runs in the table (first record, count; ADR-3915).
    runs: (u32, u32),
    /// Where its voxel far levels start, object units: their error and what their LOD sphere adds
    /// to the distance from the mesh's centre (ADR-3916). `[0, 0]`: no voxel levels.
    voxel: [f32; 2],
}

/// [`MeshSlot`]'s `voxel`: where `mesh`'s voxel far levels start, object units (their error, and
/// what their LOD sphere adds to the distance from the mesh's centre; ADR-3916). `[0, 0]`: none.
fn voxel_start(mesh: &MeshletMesh, switch: Option<super::sway::VoxelSwitch>) -> [f32; 2] {
    switch.map_or([0.0; 2], |s| {
        let off = (0..3)
            .map(|i| (s.sphere[i] - mesh.bounds[i]).powi(2))
            .sum::<f32>()
            .sqrt();
        [s.error, s.sphere[3] - off]
    })
}

/// A cluster record with the no-sway flag in `pad` that voxel far levels carry (ADR-3916): set
/// from `level` against `switch`, as the whole-mesh upload sets it.
fn sway_flagged(record: Meshlet, level: u32, switch: Option<super::sway::VoxelSwitch>) -> Meshlet {
    Meshlet {
        pad: u32::from(switch.is_some_and(|s| level >= s.level)),
        ..record
    }
}

/// One cluster as the whole-mesh upload writes it: arena addresses, the voxel-far sway flag in
/// `pad`, and finite LOD errors. A non-finite own error or a NaN parent would otherwise fail
/// `vis_cull.wgsl` `lod_decision` and leave a hole. Residency is off by default, so this path is
/// the one a cooked mesh takes.
/// The sway frame's `w`: world error times this is the distance from which a voxel far level can
/// first be selected on approach (`foliage_sway.wgsl` `cluster_offset`). With LOD hysteresis a
/// held voxel level stays until the projected error passes the threshold plus the band, so the
/// switch is nearer than the hard cut; the fade must reach rest before that nearer distance, or
/// the mesh levels replace the voxel level already swaying.
fn sway_switch_scale(pixels_per_unit: f32, settings: &super::VisBufferSettings) -> f32 {
    let held = settings.lod_threshold_px.max(0.0) + settings.resolved_lod_hysteresis_px();
    pixels_per_unit / held.max(1e-6)
}

fn whole_mesh_record(m: &Meshlet, vertex_offset: u32, triangle_offset: u32, pad: u32) -> Meshlet {
    let (lod_error, parent_error) = super::residency::finite_errors(m.lod_error, m.parent_error);
    Meshlet {
        vertex_offset,
        triangle_offset,
        lod_error,
        parent_error,
        pad,
        ..*m
    }
}

/// The camera a visibility frame is drawn for.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VisView {
    /// View-projection of the frame's raster (jittered under TAA).
    pub view_proj: [[f32; 4]; 4],
    /// Unjittered view-projection (frustum planes, as the forward cull).
    pub cull_view_proj: [[f32; 4]; 4],
    /// Last frame's view-projection (TAAU motion vectors).
    pub prev_view_proj: [[f32; 4]; 4],
    /// Eye position.
    pub eye: [f32; 3],
    /// Unit view direction.
    pub forward: [f32; 3],
    /// Near plane.
    pub near: f32,
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Depth / visibility target size.
    pub width: u32,
    /// Depth / visibility target size.
    pub height: u32,
    /// No back-face culling (the scene's glTF double-sided flag).
    pub double_sided: bool,
}

struct Targets {
    width: u32,
    height: u32,
    ids_view: wgpu::TextureView,
    material_depth: wgpu::TextureView,
    hiz_all: wgpu::TextureView,
    hiz_storage: Vec<wgpu::TextureView>,
    hiz_sampled: Vec<wgpu::TextureView>,
    hiz_size: (u32, u32),
}

struct Pipes {
    cull_bgl: wgpu::BindGroupLayout,
    reset: wgpu::ComputePipeline,
    cull_instances: wgpu::ComputePipeline,
    phase1: wgpu::ComputePipeline,
    phase2: wgpu::ComputePipeline,
    /// The cull's prefix sums and list placement (ADR-2294).
    scan_local: wgpu::ComputePipeline,
    /// The second level of the instance scan, then of each phase's (with the dispatch and the
    /// lists' counts in the same dispatch).
    scan_top: [wgpu::ComputePipeline; 3],
    place_instances: wgpu::ComputePipeline,
    place: [wgpu::ComputePipeline; 2],
    /// Counts changed instance cuts after both visibility phases (ADR-4631).
    lod_prepare: wgpu::ComputePipeline,
    lod_commit: wgpu::ComputePipeline,
    hiz_copy_bgl: wgpu::BindGroupLayout,
    hiz_down_bgl: wgpu::BindGroupLayout,
    hiz_copy: wgpu::ComputePipeline,
    hiz_down: wgpu::ComputePipeline,
    raster_bgl: wgpu::BindGroupLayout,
    raster_layout: wgpu::PipelineLayout,
    raster_module: wgpu::ShaderModule,
    /// Phase 1's empty-ID fill where the backend cannot clear the IDs to `u32::MAX`
    /// (`backend_policy::clears_integer_targets_exactly`, ADR-5807); `None` elsewhere.
    id_fill: Option<wgpu::RenderPipeline>,
    resolve_bgl: wgpu::BindGroupLayout,
    velocity_bgl: wgpu::BindGroupLayout,
    /// Group 1 of the velocity pass: the scene depth (occlusion test).
    velocity_depth_bgl: wgpu::BindGroupLayout,
    velocity: wgpu::RenderPipeline,
}

#[derive(Default)]
struct Buffers {
    instances: Option<(wgpu::Buffer, u64)>,
    work: Option<(wgpu::Buffer, u64)>,
    /// The cull's class words and prefix sums (`vis_cull.wgsl` `scan`).
    scan: Option<(wgpu::Buffer, u64)>,
    clusters: Option<(wgpu::Buffer, u64)>,
    bits: Option<(wgpu::Buffer, u64)>,
    geo: Option<(wgpu::Buffer, u64)>,
    meshlets: Option<(wgpu::Buffer, u64)>,
    state: Option<wgpu::Buffer>,
    /// Copy of the cluster-cull dispatch arguments: an indirect buffer may not also be bound
    /// as writable storage in the dispatch that reads it.
    dispatch_args: Option<wgpu::Buffer>,
    cull_params: Option<wgpu::Buffer>,
    raster_params: Option<[wgpu::Buffer; 2]>,
    resolve_params: Option<wgpu::Buffer>,
    prev_models: Option<(wgpu::Buffer, u64)>,
    velocity_params: Option<wgpu::Buffer>,
    /// Per-instance change flags, switch count, and cluster boundary history (ADR-4631).
    lod: Option<(wgpu::Buffer, u64)>,
}

/// Owner of every visibility-buffer resource; lives in [`crate::FrameRenderer`].
#[derive(Default)]
pub(crate) struct VisBufferPass {
    pub(crate) settings: VisBufferSettings,
    builder: Option<MeshletBuilder>,
    cooked: HashMap<u64, Arc<MeshletMesh>>,
    meshes: HashMap<u64, MeshSlot>,
    declined: HashSet<u64>,
    /// Meshes refused because the arena is at its cap; retried once a range is freed.
    arena_refused: HashSet<u64>,
    /// Suballocated cluster records and geometry words of every resident mesh (ADR-353).
    arena: ClusterArena,
    /// The UV2 ranges of lightmap receivers' meshes (ADR-2107, `lightmap`).
    uv2: HashMap<u64, lightmap::Uv2Slot>,
    /// This frame's lightmap receivers by material slot (ADR-2689, `lightmap`).
    receivers: lightmap::ReceiverSlots,
    /// Floors on the list capacities while the profile selects by density (ADR-2689, `warm`).
    reserve: warm::ListReserve,
    /// Every resident mesh's LOD level runs for the instance cull (ADR-3915, `levels`).
    levels: levels::LevelTable,
    /// The runs of each resident mesh, for a rebuild of the table.
    mesh_runs: HashMap<u64, Vec<levels::LevelRun>>,
    /// The sway records, the clock and the standard twins (ADR-3916, `sway`).
    sway: sway::SwayTable,
    /// World boxes of the persistent sets that sway.
    sway_boxes: Vec<[[f32; 3]; 2]>,
    /// The device carries the texture table, so masked sets draw alpha-tested (ADR-3915).
    masked_device: bool,
    /// Masked instances among the persistent ones, and among this frame's.
    persistent_masked: usize,
    frame_masked: usize,
    /// Material slots of this frame's masked instances (deduplicated by `masked_slot_used`).
    masked_slots: Vec<u32>,
    masked_slot_used: HashSet<u32>,
    /// This frame's masked-raster bindings ([`Self::bind_masked`]).
    masked_bind: Option<MaskedBind>,
    /// Masked raster pipelines keyed by (cull none, reversed list, alpha-tested).
    masked_pipes: HashMap<(bool, bool, bool), wgpu::RenderPipeline>,
    /// The alpha-tested raster's layout and module (built once, on a table device).
    masked_layout: Option<wgpu::PipelineLayout>,
    masked_module: Option<wgpu::ShaderModule>,
    /// This frame's instances from the renderer's mesh list (after the persistent ones).
    instances: Vec<GpuInstance>,
    /// Material key of every slot. Slots persist across frames (persistent instances keep
    /// theirs); the table is rebuilt only when it fills.
    material_slots: Vec<u64>,
    slot_of: HashMap<u64, u32>,
    /// Slots shaded this frame, and a per-slot flag for deduplication.
    frame_slots: Vec<u32>,
    slot_used: Vec<bool>,
    total_chunks: u64,
    total_bits: u64,
    candidates: u64,
    /// Persistent instance sets ([`crate::FrameRenderer::set_instance_set`]).
    sets: BTreeMap<u64, StoredSet>,
    /// Instances of every set, rebuilt when a set changes and uploaded once per change.
    persistent: Vec<GpuInstance>,
    persistent_bits: u64,
    persistent_chunks: u64,
    persistent_dirty: bool,
    persistent_upload: bool,
    /// A set changed since the renderer last asked ([`Self::take_sets_changed`]).
    sets_changed: bool,
    /// Bumped on every set change (frame signatures).
    sets_generation: u64,
    /// Some persistent instance picks its sun cascades on the GPU (`ShadowCast::Local`).
    gpu_masked: bool,
    /// Double-sided instances among the persistent ones, and among this frame's.
    persistent_double_sided: usize,
    frame_double_sided: usize,
    shadow: shadow::ShadowRes,
    /// The compute rasterizer of [`VisBufferSettings::software_raster`] (ADR-1887).
    software: software::SoftwareRaster,
    buffers: Buffers,
    /// Bumped whenever a bound buffer or target is replaced; bind groups key on it.
    generation: u64,
    targets: Option<Targets>,
    pipes: Option<Pipes>,
    /// Keyed by (cull none, reversed list: the double-sided clusters).
    raster_pipes: HashMap<(bool, bool), wgpu::RenderPipeline>,
    resolve_layout: Option<wgpu::PipelineLayout>,
    resolve_pipes: HashMap<(wgpu::TextureFormat, bool, bool), (wgpu::RenderPipeline, wgpu::RenderPipeline)>,
    cull_bg: Option<(u64, wgpu::BindGroup)>,
    raster_bgs: Option<(u64, [wgpu::BindGroup; 2])>,
    resolve_bg: Option<(u64, wgpu::BindGroup)>,
    velocity_bg: Option<(u64, wgpu::BindGroup)>,
    /// The velocity pass's scene-depth group, keyed by the depth view.
    velocity_depth_bg: Option<(wgpu::TextureView, wgpu::BindGroup)>,
    /// Previous-frame model matrix of every instance (parallel to `instances`).
    prev_models: Vec<[[f32; 4]; 4]>,
    /// The Hi-Z pyramid's groups (ADR-2689): they go with the targets they bind, not with the
    /// renderer's bind epoch, so a renewal elsewhere does not rebuild them in play.
    hiz_bgs: HizGroups,
    stats: VisBufferStats,
    /// [`Self::prepare`] ran this frame.
    prepared: bool,
    /// The last main-view frame drew through the path ([`Self::set_active`]); the mesh LOD
    /// selector hands meshes with clusters to their cluster LOD only while it is set.
    active: bool,
    /// Every GPU object the path's entry points created (ADR-355).
    creates: CreateLedger,
    /// The dense-scene selection's memory (ADR-2071).
    dense: DenseGate,
    /// The profile's switch and path choices at the last main-view frame (warms, the report).
    profile_path: (bool, VisibilityPath),
    /// The single-pass resolve (ADR-2071).
    single: SinglePass,
    /// Cluster residency (ADR-3118): the managed meshes, the budget's state and its counters.
    residency: super::residency::Residency,
    /// Test hook: GPU stamps around the cull passes (ADR-2294's cost check).
    #[cfg(test)]
    cull_timer: Option<CullTimer>,
    /// Test hook: meshes made resident from now on get no level runs (every cluster a candidate).
    #[cfg(test)]
    pub(crate) levels_off: bool,
    /// Test hook: fill the IDs by the draw DX12 uses even where the clear is exact (ADR-5807).
    /// Takes effect when the pipelines are built.
    #[cfg(test)]
    pub(crate) force_id_fill: bool,
    /// Boundary decisions per instance/cluster, indexed by its visibility-bit offset.
    /// Bits: own above = 1, parent above = 2, valid = 4, selected = 8 (ADR-4631).
    lod_history: Vec<u32>,
    /// Last frame's instance descriptors, for geometry and motion continuity checks.
    lod_previous: Vec<GpuInstance>,
    /// Stable transient object keys supplied by the draw list (external frame patch).
    lod_keys: Vec<Option<u64>>,
    lod_previous_keys: Vec<Option<u64>>,
    /// Changes to persistent set membership invalidate slot-based boundary history.
    lod_sets_generation: u64,
    /// The current main-view frame advanced boundary history; missing frames invalidate it.
    lod_updated: bool,
    /// Eye of the last frame that updated the LOD history.
    lod_eye: Option<[f32; 3]>,
    /// Instance slots in the LOD history buffer (`CullParams.lod.z`).
    lod_slots: u32,
    /// The next frame discards the stored LOD (a camera cut, or a new history buffer).
    lod_clear: bool,
    /// The window view the next main-view frame draws (`FrameRenderer::set_occlusion_view`,
    /// ADR-7329).
    occlusion_view: u32,
    /// The window view whose cluster visibility bits `buffers.bits` holds (`None` before the
    /// first cluster frame, or after that view went away).
    bits_view: Option<u32>,
    /// Per window view, its visibility bits (and the bytes in use) while another view draws
    /// (ADR-7329).
    view_bits: Vec<Option<(wgpu::Buffer, u64)>>,
    /// The live bits belong to a view that was retired (ADR-8620): the next view's switch clears
    /// them.
    bits_stale: bool,
    /// Per window view, its LOD hysteresis history while another view draws (ADR-8620).
    lod_views: Vec<lod_views::LodViewSlot>,
    /// Stored LOD histories discarded since the pass was made (`VisBufferStats::lod_history_discards`).
    lod_discards: u64,
    /// Buffer copies of LOD history words between views (`VisBufferStats::lod_history_copies`).
    lod_view_copies: u64,
    /// Counted meshes' clusters from their upload and their pending vertex repacks (ADR-7462).
    counted: counted_vertices::CountedVertices,
}

/// A persistent instance set as stored between frames.
struct StoredSet {
    mesh_key: u64,
    material_key: u64,
    material: Arc<LabMaterial>,
    models: Vec<[[f32; 4]; 4]>,
    shadow: ShadowCast,
    /// Per-instance shadow policies (empty: `shadow` for all).
    shadows: Vec<ShadowCast>,
    /// The adopted material whose sway the set draws (`material` is its standard twin, ADR-3916).
    sway: Option<Arc<LabMaterial>>,
    /// The mesh's positions and triangles for the traced sun's scene (ADR-2310); sets with one
    /// mesh share it.
    trace: crate::hwrt::TraceCasterMesh,
    /// The mesh's vertex normals, for the probes' distance brick (ADR-5457: a brick is signed by the
    /// normals where the winding disagrees, as a placed mesh's is); sets with one mesh share them.
    /// Empty for a mesh too large to bake.
    normals: Arc<[[f32; 3]]>,
    /// The smallest and largest instance scale (each instance's longest model axis).
    scale_range: (f32, f32),
}

/// The trace copy of a set's mesh: reuses another set's copy of the same mesh, else copies the
/// positions and indices. Its generation is a hash of the content.
fn trace_mesh_of<'a>(
    set: &VisInstanceSet<'_>,
    mut stored: impl Iterator<Item = &'a crate::hwrt::TraceCasterMesh>,
) -> crate::hwrt::TraceCasterMesh {
    let key = set.mesh_key ^ crate::hwrt::casters::SET_CASTER_TAG;
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |value: u32| {
        hash ^= u64::from(value);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for vertex in set.vertices {
        vertex.position.iter().for_each(|c| mix(c.to_bits()));
    }
    set.indices.iter().for_each(|&i| mix(i));
    if let Some(same) = stored.find(|mesh| mesh.key == key && mesh.generation == hash) {
        return same.clone();
    }
    crate::hwrt::TraceCasterMesh {
        key,
        generation: hash,
        positions: set.vertices.iter().map(|v| v.position).collect(),
        indices: std::sync::Arc::from(set.indices),
    }
}

/// The vertex normals a set's mesh gives the probes' brick bake: another set's copy of the same
/// mesh (the same trace copy), else a copy. Empty for a mesh over the bake's triangle limit.
fn probe_normals_of<'a>(
    set: &VisInstanceSet<'_>,
    trace: &crate::hwrt::TraceCasterMesh,
    mut stored: impl Iterator<Item = &'a StoredSet>,
) -> Arc<[[f32; 3]]> {
    if set.indices.len() / 3 > crate::ddgi_bake::MAX_BAKE_TRIANGLES {
        return Arc::from(Vec::new());
    }
    if let Some(same) = stored.find(|other| {
        other.trace.key == trace.key
            && other.trace.generation == trace.generation
            && other.normals.len() == set.vertices.len()
    }) {
        return Arc::clone(&same.normals);
    }
    set.vertices.iter().map(|v| v.normal).collect()
}

/// The smallest and largest of the instances' scales (each the longest axis of its model matrix);
/// (1, 1) without instances.
fn scale_range_of(models: &[[[f32; 4]; 4]]) -> (f32, f32) {
    let mut range = (f32::MAX, 0.0f32);
    for model in models {
        let axis = |c: usize| (model[c][0].powi(2) + model[c][1].powi(2) + model[c][2].powi(2)).sqrt();
        let scale = axis(0).max(axis(1)).max(axis(2));
        range = (range.0.min(scale), range.1.max(scale));
    }
    if models.is_empty() { (1.0, 1.0) } else { range }
}

#[path = "shadow.rs"]
pub(crate) mod shadow;
#[path = "software.rs"]
pub(crate) mod software;
#[cfg(test)]
pub(crate) use shadow::ShadowLists;
pub(crate) use shadow::{VisShadowFrame, VisShadowJob};

/// Through the tracked device, so the frame-path counters (and `VisBufferPass::create_stats`)
/// see the buffer.
fn make_buffer(device: &TrackedDevice, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(16).next_multiple_of(16),
        usage,
        mapped_at_creation: false,
    })
}

/// Grows `slot` to hold `needed` bytes (power-of-two steps). Returns true when replaced.
/// The regions of the cull's `scan` buffer, in 16-byte elements (`vis_cull.wgsl`): the class
/// words of `work_items` items end at `[0]`, the values of the longer scan (instances or work
/// items) at `[1]`, its block sums at `[2]`.
fn scan_regions(work_items: u64, instances: u64) -> [u32; 3] {
    let elements = work_items.max(instances);
    let words = work_items;
    let values = words + elements;
    let blocks = values + elements.div_ceil(SCAN_BLOCK);
    [words, values, blocks].map(|n| u32::try_from(n).unwrap_or(u32::MAX))
}

fn ensure_capacity(
    device: &TrackedDevice,
    slot: &mut Option<(wgpu::Buffer, u64)>,
    label: &str,
    needed: u64,
    usage: wgpu::BufferUsages,
) -> bool {
    if slot.as_ref().is_some_and(|(_, cap)| *cap >= needed) {
        return false;
    }
    let cap = needed.max(256).next_power_of_two();
    *slot = Some((make_buffer(device, label, cap, usage), cap));
    true
}

fn buffer_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    ty: wgpu::BufferBindingType,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn storage(read_only: bool) -> wgpu::BufferBindingType {
    wgpu::BufferBindingType::Storage { read_only }
}

fn texture_entry(
    binding: u32,
    visibility: wgpu::ShaderStages,
    sample_type: wgpu::TextureSampleType,
) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Texture {
            sample_type,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn whole(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn view(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

fn module(device: &wgpu::Device, label: &str, source: &str) -> Result<wgpu::ShaderModule, RenderError> {
    let naga = compose_source(source, label, HashMap::new())?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(naga)),
    }))
}

/// Source of the resolve module: the forward shader with its `VISBUFFER` hooks plus the resolve.
pub(crate) fn resolve_source() -> &'static str {
    static SOURCE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!(
            "{}\n{}",
            include_str!("../../shaders/lab_forward.wgsl"),
            include_str!("vis_resolve.wgsl")
        )
    });
    SOURCE.as_str()
}

/// The visibility-buffer shaders with their compile defs, for `shader_check` (spirv-val and the
/// ADR-026 layout scan): cull, Hi-Z, raster, the software raster and its merge, motion, the
/// cluster-shadow cull and raster, and the resolve in both encode variants.
pub(crate) fn checked_sources() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    vec![
        ("vis_cull.wgsl", include_str!("vis_cull.wgsl"), Vec::new()),
        ("vis_hiz.wgsl", include_str!("vis_hiz.wgsl"), Vec::new()),
        ("vis_raster.wgsl", include_str!("vis_raster.wgsl"), Vec::new()),
        ("vis_software.wgsl", include_str!("vis_software.wgsl"), Vec::new()),
        (
            "vis_software_merge.wgsl",
            include_str!("vis_software_merge.wgsl"),
            Vec::new(),
        ),
        ("vis_velocity.wgsl", include_str!("vis_velocity.wgsl"), Vec::new()),
        (
            "vis_vertex_repack.wgsl",
            include_str!("vis_vertex_repack.wgsl"),
            Vec::new(),
        ),
        ("vis_shadow.wgsl", include_str!("vis_shadow.wgsl"), Vec::new()),
        (
            "vis_shadow_raster.wgsl",
            include_str!("vis_shadow_raster.wgsl"),
            Vec::new(),
        ),
        ("vis_resolve.wgsl", resolve_source(), vec!["VISBUFFER"]),
        (
            "vis_resolve.wgsl",
            resolve_source(),
            vec!["VISBUFFER", "skip_srgb_encode"],
        ),
        (
            "vis_resolve.wgsl",
            resolve_source(),
            vec!["VISBUFFER", "skip_srgb_encode", crate::hwrt::SUN_SHADOW_RT_DEF],
        ),
        // Lightmap receivers in the resolve (`frame/lightmap_paths.rs`, ADR-2107).
        (
            "vis_resolve.wgsl",
            resolve_source(),
            vec!["VISBUFFER", "LIGHTMAP"],
        ),
        (
            "vis_resolve.wgsl",
            resolve_source(),
            vec!["VISBUFFER", "LIGHTMAP", "skip_srgb_encode"],
        ),
        (
            "vis_resolve.wgsl",
            resolve_source(),
            vec![
                "VISBUFFER",
                "LIGHTMAP",
                "skip_srgb_encode",
                crate::hwrt::SUN_SHADOW_RT_DEF,
            ],
        ),
    ]
}

fn push_vertex(geo: &mut Vec<u32>, v: &LabVertex) {
    let words = [
        v.position[0],
        v.position[1],
        v.position[2],
        v.uv[0],
        v.normal[0],
        v.normal[1],
        v.normal[2],
        v.uv[1],
        v.tangent[0],
        v.tangent[1],
        v.tangent[2],
        v.tangent[3],
        v.color[0],
        v.color[1],
        v.color[2],
        v.color[3],
    ];
    geo.extend(words.iter().map(|w| w.to_bits()));
}

/// An upper bound on how far `model` lengthens any vector: what the cull's spheres, errors and
/// level reaches scale by (ADR-3915). Orthogonal columns (translation, rotation, scale in the
/// local axes) give the largest column length, which is exact. A non-uniform parent over a rotated
/// child shears, and a diagonal then stretches further than any column, so the spheres and the
/// level range would miss clusters: there the bound is Gershgorin's on `MᵀM`, the largest row sum of
/// the columns' dot products. Off-diagonals of rounding size keep the exact column length.
fn max_stretch(model: &[[f32; 4]; 4]) -> f32 {
    let dot = |a: [f32; 4], b: [f32; 4]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let [x, y, z] = [model[0], model[1], model[2]];
    let (xx, yy, zz) = (dot(x, x), dot(y, y), dot(z, z));
    let (xy, xz, yz) = (dot(x, y).abs(), dot(x, z).abs(), dot(y, z).abs());
    let axis = xx.max(yy).max(zz);
    if xy.max(xz).max(yz) <= axis * 1e-5 {
        return axis.sqrt();
    }
    (xx + xy + xz).max(xy + yy + yz).max(xz + yz + zz).sqrt()
}

impl VisBufferPass {
    /// Installs (or removes) the on-demand cooker.
    pub(crate) fn set_builder(&mut self, builder: Option<MeshletBuilder>) {
        self.builder = builder;
        self.declined.clear();
    }

    /// Supplies cooked meshlets for `mesh_key`, uploaded the first time the mesh draws.
    pub(crate) fn insert_cooked(&mut self, mesh_key: u64, mesh: Arc<MeshletMesh>) {
        self.declined.remove(&mesh_key);
        self.arena_refused.remove(&mesh_key);
        self.residency_forget(mesh_key);
        // A mesh a persistent set draws keeps its resident clusters until the set is replaced
        // (the set holds no vertices to rebuild from).
        if !self.set_draws(mesh_key) {
            self.drop_resident(mesh_key);
        }
        self.cooked.insert(mesh_key, mesh);
    }

    /// Forgets `mesh_key`'s meshlets: pending cooked data, its resident clusters (their arena
    /// ranges are reused by the next mesh) and a refusal. A persistent set that drew the mesh
    /// stops drawing it. True when there was something to remove.
    pub(crate) fn remove_meshlets(&mut self, mesh_key: u64) -> bool {
        let pending = self.cooked.remove(&mesh_key).is_some();
        self.counted.meshlets.remove(&mesh_key);
        self.declined.remove(&mesh_key);
        self.arena_refused.remove(&mesh_key);
        self.residency_forget(mesh_key);
        let resident = self.drop_resident(mesh_key);
        if resident && self.set_draws(mesh_key) {
            self.sets_changed();
        }
        pending || resident
    }

    /// True when `mesh_key` has cooked meshlets waiting for its first draw or resident clusters.
    pub(crate) fn has_meshlets(&self, mesh_key: u64) -> bool {
        self.cooked.contains_key(&mesh_key) || self.meshes.contains_key(&mesh_key)
    }

    /// Occupancy and counters of the cluster arena.
    pub(crate) fn arena_stats(&self) -> VisBufferArenaStats {
        VisBufferArenaStats {
            pending: self.cooked.len() as u32,
            residency: self.residency_stats(),
            ..self.arena.stats()
        }
    }

    /// GPU objects the path created, in total and in the last frame (ADR-355).
    pub(crate) fn create_stats(&self) -> VisBufferCreateStats {
        self.creates.stats()
    }

    /// True when a persistent set draws `mesh_key`.
    fn set_draws(&self, mesh_key: u64) -> bool {
        self.sets.values().any(|set| set.mesh_key == mesh_key)
    }

    /// Frees `mesh_key`'s resident clusters back to the arena. True when it had some.
    fn drop_resident(&mut self, mesh_key: u64) -> bool {
        self.lod_clear = true;
        self.clear_parked_lod_views();
        let Some(slot) = self.meshes.remove(&mesh_key) else {
            return false;
        };
        self.counted.forget(mesh_key);
        self.drop_uv2(mesh_key);
        self.residency_drop(mesh_key);
        self.levels.release(slot.runs.1);
        self.mesh_runs.remove(&mesh_key);
        if self.arena.free(slot.key) {
            // Room was made: meshes refused at the cap may fit now.
            self.arena_refused.clear();
        }
        true
    }

    /// Records whether this main-view frame draws through the path.
    pub(crate) fn set_active(&mut self, active: bool) {
        self.active = active;
    }

    /// The last main-view frame drew through the path.
    pub(crate) fn active(&self) -> bool {
        self.active
    }

    /// True when `mesh_key`'s clusters pick its level of detail: the last main-view frame drew
    /// through the path, and the mesh has meshlets (pending or resident) that were neither
    /// declined nor refused at the arena's cap. Such a mesh draws whole (its LOD 0 index list)
    /// and the cull cuts its cluster hierarchy per view.
    pub(crate) fn takes_cluster_lod(&self, mesh_key: u64) -> bool {
        self.active
            && self.has_meshlets(mesh_key)
            && !self.declined.contains(&mesh_key)
            && !self.arena_refused.contains(&mesh_key)
            && !self.residency_refused(mesh_key)
    }

    /// Lowers the cluster arena's caps to `device`'s largest storage binding. The renderer calls
    /// it when a visibility frame starts, before that frame's meshes take their ranges, so the
    /// first frame cannot outgrow the device either (`prepare` calls it again, a no-op then).
    pub(crate) fn clamp_to_device(&mut self, device: &wgpu::Device) {
        let limits = device.limits();
        self.clamp_arena(limits.max_storage_buffer_binding_size.min(limits.max_buffer_size));
        self.counted.max_binding = Some(limits.max_storage_buffer_binding_size);
        let masked = Self::masked_supported(device);
        if masked != self.masked_device {
            self.masked_device = masked;
            if !self.sets.is_empty() {
                // Masked sets change lists; the next frame start rebuilds them.
                self.persistent_dirty = true;
            }
        }
    }

    /// [`Self::clamp_to_device`] with the limit in bytes.
    fn clamp_arena(&mut self, max_binding_bytes: u64) {
        self.arena.clamp_to(max_binding_bytes);
    }

    /// True when some mesh could take the path (a builder, cooked data or resident meshes).
    pub(crate) fn has_sources(&self) -> bool {
        self.builder.is_some() || !self.cooked.is_empty() || !self.meshes.is_empty() || !self.sets.is_empty()
    }

    /// True when a persistent instance set is installed.
    pub(crate) fn has_persistent_sets(&self) -> bool {
        !self.sets.is_empty()
    }

    /// Game-frame warms: the other forward module (ADR-2035) of every resolve this pass has built.
    /// Returns how many it asked for.
    pub(crate) fn warm_resolve_twins(
        &mut self,
        device: &wgpu::Device,
        view_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
    ) -> Result<usize, RenderError> {
        let twins: Vec<_> = self.resolve_pipes.keys().copied().collect();
        let mut asked = 0;
        for (format, encode, sun_rt) in twins {
            self.resolve_pipelines(device, format, encode, !sun_rt, view_bgl, mat_bgl)?;
            asked += 1;
        }
        // The single pass's other forward module too (ADR-2071).
        if let Some(stride) = self.single.stride() {
            for (format, encode, sun_rt) in self.single.twins() {
                self.single_pass_pipeline(
                    device,
                    view_bgl,
                    SinglePassTarget {
                        stride,
                        format,
                        encode,
                        sun_rt,
                    },
                )?;
                asked += 1;
            }
        }
        Ok(asked)
    }

    /// Whether this main-view frame takes the path under `select` (ADR-2071). A profile that
    /// selects by density still draws through the path while persistent sets exist: they draw
    /// nowhere else. `count` runs only under [`VisibilitySelect::Dense`].
    pub(crate) fn select_frame(
        &mut self,
        select: VisibilitySelect,
        count: impl FnOnce() -> SceneDensity,
    ) -> bool {
        match select {
            VisibilitySelect::Always => {
                self.dense.reset();
                true
            }
            VisibilitySelect::Dense(select) => {
                let _span = tracing::info_span!("visbuffer_density").entered();
                let dense = self.dense.step(select, count());
                dense || self.has_persistent_sets()
            }
        }
    }

    /// The profile's switch and path choices (each main-view frame).
    pub(crate) fn note_path(&mut self, on: bool, path: VisibilityPath) {
        self.profile_path = (on, path);
        // A floor once set: the warm's window-size frame through the path notes `Always`.
        if on {
            self.reserve = self.reserve.max(warm::ListReserve::for_path(path.select));
        }
        // A path switched off forgets the scene: turned back on, a sparse level starts sparse.
        if !on {
            self.dense.reset();
        }
    }

    /// What [`Self::note_path`] last heard.
    pub(crate) fn profile_path(&self) -> (bool, VisibilityPath) {
        self.profile_path
    }

    /// What the dense-scene selection did.
    pub(crate) fn dense_report(&self) -> DenseGateReport {
        self.dense.report()
    }

    /// What the last resolve did.
    pub(crate) fn resolve_report(&self) -> VisResolveReport {
        self.single.report
    }

    /// Sets what the last resolve did (the frame's resolve pass).
    pub(crate) fn set_resolve_report(&mut self, report: VisResolveReport) {
        self.single.report = report;
    }

    /// Game-frame warms of a profile that selects by density (ADR-2071): what the first dense
    /// frame of play would otherwise compile. The cull, raster and shadow pipelines, and the
    /// resolve for each `(format, encode, sun_rt)`. The first dense frame still creates its lists
    /// and targets. Returns how many it asked for.
    pub(crate) fn warm_path(
        &mut self,
        device: &wgpu::Device,
        view_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
        arena_stride: Option<u64>,
        targets: &[(wgpu::TextureFormat, bool, bool)],
    ) -> Result<usize, RenderError> {
        self.prewarm_double_sided(device)?;
        let before = CreateLedger::mark();
        let d = &TrackedDevice::observe(device);
        for double_sided in [false, true] {
            self.raster_pipeline(d, double_sided, false);
            self.shadow_raster_pipeline(d, double_sided, false);
        }
        if self.settings.software_raster {
            self.software.ensure_pipes(d)?;
        }
        self.creates.charge(before);
        let mut asked = 1;
        let single =
            self.profile_path.1.single_pass_resolve && super::single_pass::fallback_on(device).is_none();
        for &(format, encode, sun_rt) in targets {
            crate::pipeline_warm::warm_checkpoint()?;
            self.resolve_pipelines(device, format, encode, sun_rt, view_bgl, mat_bgl)?;
            asked += 1;
            if let (true, Some(stride)) = (single, arena_stride) {
                self.single_pass_pipeline(
                    device,
                    view_bgl,
                    SinglePassTarget {
                        stride,
                        format,
                        encode,
                        sun_rt,
                    },
                )?;
                asked += 1;
            }
        }
        Ok(asked)
    }

    /// Takes the pipelines `warmed` built on a warm worker or an abandoned game-frame warm
    /// (`FrameRenderer::adopt_warm_pipelines`), where this pass has none yet. Frame state (targets,
    /// lists, the arena) never moves. Layouts with equal entries are one layout on a device, so
    /// they fit this renderer's frame groups. Returns whether anything moved.
    pub(crate) fn adopt_pipelines(&mut self, warmed: &mut Self) -> bool {
        if self.pipes.is_some() || warmed.pipes.is_none() {
            return false;
        }
        self.pipes = warmed.pipes.take();
        self.raster_pipes = std::mem::take(&mut warmed.raster_pipes);
        self.resolve_layout = warmed.resolve_layout.take();
        self.resolve_pipes = std::mem::take(&mut warmed.resolve_pipes);
        self.shadow.adopt_pipelines(&mut warmed.shadow);
        self.software.adopt_pipelines(&mut warmed.software);
        self.single.adopt_pipelines(&mut warmed.single);
        true
    }

    /// The single-pass shading pipeline for a colour target and forward module.
    pub(crate) fn single_pass_pipeline(
        &mut self,
        device: &wgpu::Device,
        view_bgl: &wgpu::BindGroupLayout,
        target: SinglePassTarget,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        let before = CreateLedger::mark();
        let out = self.build_single_pass_pipeline(device, view_bgl, target);
        self.creates.charge(before);
        out
    }

    fn build_single_pass_pipeline(
        &mut self,
        device: &wgpu::Device,
        view_bgl: &wgpu::BindGroupLayout,
        target: SinglePassTarget,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        let d = &TrackedDevice::observe(device);
        self.ensure_pipes(d)?;
        let pipes = self
            .pipes
            .as_ref()
            .ok_or_else(|| RenderError::Shader("visibility pipelines missing".into()))?;
        self.single.pipeline(d, &pipes.resolve_bgl, view_bgl, target)
    }

    /// Writes the frame's material slot map and returns group 2 of the single-pass resolve, with
    /// how many materials it names. `record_of` gives a material key's arena record. `None` when
    /// no material of the frame has one.
    pub(crate) fn single_pass_group(
        &mut self,
        d: &TrackedDevice,
        q: &TrackedQueue,
        arena: &crate::bindless::MaterialArena,
        record_of: impl Fn(u64) -> Option<u32>,
    ) -> Option<(wgpu::BindGroup, u32)> {
        let before = CreateLedger::mark();
        let mut named = 0;
        let slots = self.material_slots.len();
        let (frame_slots, material_slots) = (&self.frame_slots, &self.material_slots);
        self.single.write_map(
            d,
            q,
            slots,
            frame_slots.iter().filter_map(|&slot| {
                let record = record_of(*material_slots.get(slot as usize)?)?;
                named += 1;
                Some((slot, record))
            }),
        );
        let group = (named > 0).then(|| self.single.bind_group(d, arena)).flatten();
        self.creates.charge(before);
        group.map(|group| (group, named))
    }

    /// True when `device` can run the path (storage-buffer count of the resolve stage).
    pub(crate) fn supported(device: &wgpu::Device) -> bool {
        device.limits().max_storage_buffers_per_shader_stage >= RESOLVE_STORAGE_BUFFERS
    }

    /// Last frame's counts.
    pub(crate) fn stats(&self) -> VisBufferStats {
        self.stats
    }

    /// Forgets meshes whose last `Arc` dropped ([`crate::FrameRenderer::release_meshes`]): their
    /// resident clusters (unless a persistent set draws them; their arena ranges are reused),
    /// refusal and cooked data.
    pub(crate) fn release_meshes(&mut self, keys: &[u64]) {
        for &key in keys {
            if !self.set_draws(key) {
                self.drop_resident(key);
            }
            self.declined.remove(&key);
            self.arena_refused.remove(&key);
            self.residency_forget(key);
            self.cooked.remove(&key);
            self.counted.meshlets.remove(&key);
        }
    }

    /// Makes `mesh_key`'s clusters resident; false keeps the mesh on the forward path.
    pub(crate) fn ensure_mesh(&mut self, mesh_key: u64, vertices: &[LabVertex], indices: &[u32]) -> bool {
        !matches!(
            self.admit_mesh(mesh_key, VertexInput::Cpu(vertices, indices)),
            Admit::Forward
        )
    }

    /// [`Self::ensure_mesh`] for CPU vertices or a counted upload's counts (ADR-7462). A counted
    /// mesh needs cooked meshlets (no builder runs) and residency off; its base vertices are a
    /// GPU-owned range the caller fills with a repack ([`Admit::Repack`]).
    pub(super) fn admit_mesh(&mut self, mesh_key: u64, input: VertexInput<'_>) -> Admit {
        let (vertex_count, index_count) = input.counts();
        // A dynamic mesh (a cloth section) rewrites its vertices under a stable key. Meshlets
        // baked from one frame would stay stale the next, so it draws on the forward path,
        // which rewrites the resident buffers in place.
        if crate::runtime::mesh_content_revision(mesh_key) != 0 {
            if self.meshes.contains_key(&mesh_key) {
                self.drop_resident(mesh_key);
            }
            self.declined.insert(mesh_key);
            return Admit::Forward;
        }
        if let Some(slot) = self.meshes.get(&mesh_key) {
            if slot.vertex_count == vertex_count && slot.index_count == index_count {
                return Admit::Resident;
            }
            // Same key, new content: rebuild (the old ranges return to the arena).
            self.drop_resident(mesh_key);
        }
        if self.declined.contains(&mesh_key)
            || self.arena_refused.contains(&mesh_key)
            || self.residency_refused(mesh_key)
        {
            return Admit::Forward;
        }
        // Cluster residency packs each band's vertices on the CPU: a counted mesh draws forward
        // while it is on (ADR-7462).
        if matches!(input, VertexInput::Gpu { .. }) && self.residency_config().enabled {
            return Admit::Forward;
        }
        let mesh = match self.cooked.remove(&mesh_key) {
            Some(mesh) => mesh,
            None => {
                // No CPU vertices to cook from: a counted mesh waits for cooked meshlets.
                let VertexInput::Cpu(vertices, indices) = input else {
                    return Admit::Forward;
                };
                let Some(builder) = self.builder.as_ref() else {
                    return Admit::Forward;
                };
                let _span =
                    tracing::info_span!("visbuffer_build_meshlets", triangles = indices.len() / 3).entered();
                match builder(vertices, indices) {
                    Some(mesh) => Arc::new(mesh),
                    None => {
                        self.declined.insert(mesh_key);
                        return Admit::Forward;
                    }
                }
            }
        };
        // A stream-shaped mesh (ADR-3708) carries its cluster lists on the pak; the whole path
        // needs them in RAM, so materialize it when residency is off (the admission below reads
        // the base band itself).
        let mut mesh = mesh;
        let streamed = mesh.disk.is_some();
        if streamed
            && !self.residency_config().enabled
            && let Err(err) = Arc::make_mut(&mut mesh).materialize()
        {
            tracing::warn!(
                target: "regia_render",
                mesh_key,
                "streamed meshlets not materialized ({}); the mesh draws forward",
                err
            );
            self.declined.insert(mesh_key);
            return Admit::Forward;
        }
        // The draw supplies the mesh's own vertices; a voxel far DAG carries the ones it appends
        // (ADR-2655), so the check is against the base range.
        if mesh.meshlets.is_empty()
            || mesh.base_vertex_count() != vertex_count
            || mesh.source_index_count as usize != index_count
            || (!streamed && mesh.validate().is_err())
        {
            tracing::warn!(
                target: "regia_render",
                mesh_key,
                "meshlets do not match the mesh; it stays on the forward path"
            );
            self.declined.insert(mesh_key);
            return Admit::Forward;
        }
        // Cluster residency (ADR-3118/3708): the base now, finer bands as the view asks for
        // them (streamed from the pak when the mesh carries a disk).
        if self.residency_config().enabled
            && let VertexInput::Cpu(vertices, _) = input
        {
            match self.residency_admit(mesh_key, &mesh, vertices, index_count) {
                resident::Admission::Managed => return Admit::Resident,
                resident::Admission::Refused => {
                    self.cooked.insert(mesh_key, mesh);
                    return Admit::Forward;
                }
                resident::Admission::Whole if streamed => {
                    // A stream-shaped mesh that stays whole needs its sections in RAM.
                    if let Err(err) = Arc::make_mut(&mut mesh).materialize() {
                        tracing::warn!(
                            target: "regia_render",
                            mesh_key,
                            "streamed meshlets not materialized ({}); the mesh draws forward",
                            err
                        );
                        self.declined.insert(mesh_key);
                        return Admit::Forward;
                    }
                }
                resident::Admission::Whole => {}
            }
        }
        // One geometry range: the vertices, then the cluster vertex lists, then the triangles.
        let appended = &mesh.appended_vertices.0;
        let words = (vertex_count + appended.len()) as u64 * u64::from(VERTEX_WORDS)
            + mesh.vertices.len() as u64
            + mesh.triangles.len() as u64;
        let (Ok(words), Ok(clusters)) = (u32::try_from(words), u32::try_from(mesh.meshlets.len())) else {
            self.declined.insert(mesh_key);
            return Admit::Forward;
        };
        let Some((key, ranges)) = self.arena.alloc(words, clusters) else {
            // The arena is at its cap: the mesh draws forward until a range frees up (the
            // meshlets are kept, so a builder does not run again).
            self.arena_refused.insert(mesh_key);
            self.cooked.insert(mesh_key, mesh);
            return Admit::Forward;
        };
        let vertex_base = ranges.geo.0;
        // A counted mesh's base vertices are GPU-owned (the repack writes them); everything after
        // them goes through the mirror, as on the CPU path.
        let (gpu_words, mut geo) = match input {
            VertexInput::Cpu(vertices, _) => {
                let mut geo = Vec::with_capacity(words as usize);
                for v in vertices {
                    push_vertex(&mut geo, v);
                }
                (0, geo)
            }
            VertexInput::Gpu { .. } => {
                let gpu_words = vertex_count as u32 * VERTEX_WORDS;
                if !self.arena.own_on_gpu(vertex_base, gpu_words) {
                    self.arena.free(key);
                    self.declined.insert(mesh_key);
                    return Admit::Forward;
                }
                // Kept for a re-upload under the same key (a hot reload, ADR-7462): there are no
                // CPU vertices to cook them again from.
                self.counted.meshlets.insert(mesh_key, Arc::clone(&mesh));
                (gpu_words, Vec::with_capacity(words as usize - gpu_words as usize))
            }
        };
        for v in appended {
            push_vertex(&mut geo, v);
        }
        let vref_base = vertex_base + gpu_words + geo.len() as u32;
        geo.extend(mesh.vertices.iter().map(|&v| vertex_base + v * VERTEX_WORDS));
        let tri_base = vertex_base + gpu_words + geo.len() as u32;
        geo.extend_from_slice(&mesh.triangles);
        self.arena.write_geo(vertex_base + gpu_words, &geo);
        // Voxel far levels do not sway: their records say so in `pad` (ADR-3916), and instances fade
        // their sway before those levels can be selected.
        let switch = super::sway::voxel_switch(&mesh);
        let records: Vec<Meshlet> = mesh
            .meshlets
            .iter()
            .map(|m| {
                whole_mesh_record(
                    m,
                    vref_base + m.vertex_offset,
                    tri_base + m.triangle_offset,
                    u32::from(switch.is_some_and(|s| m.level >= s.level)),
                )
            })
            .collect();
        let voxel = voxel_start(&mesh, switch);
        self.arena.write_clusters(ranges.clusters.0, &records);
        let runs = levels::runs_of(&mesh.meshlets, mesh.bounds);
        #[cfg(test)]
        let runs = if self.levels_off { Vec::new() } else { runs };
        let run_range = self.levels.insert(&runs);
        self.mesh_runs.insert(mesh_key, runs);
        if self.set_draws(mesh_key) {
            self.persistent_dirty = true;
        }
        self.meshes.insert(
            mesh_key,
            MeshSlot {
                meshlet_base: ranges.clusters.0,
                meshlet_count: clusters,
                vertex_count,
                index_count,
                bounds: mesh.bounds,
                key,
                vertex_base,
                runs: run_range,
                voxel,
            },
        );
        match input {
            VertexInput::Cpu(..) => Admit::Resident,
            VertexInput::Gpu { .. } => Admit::Repack {
                key,
                dst_word: vertex_base,
                vertices: vertex_count as u32,
            },
        }
    }

    /// Starts a frame's instance list: the persistent sets' materials are shaded every frame.
    pub(crate) fn begin_frame(&mut self) {
        self.lod_clear |= !std::mem::take(&mut self.lod_updated);
        self.creates.begin_frame();
        self.counted.begin_frame();
        self.residency_begin_frame();
        self.prepared = false;
        self.instances.clear();
        self.lod_keys.clear();
        self.prev_models.clear();
        for &slot in &self.frame_slots {
            if let Some(used) = self.slot_used.get_mut(slot as usize) {
                *used = false;
            }
        }
        self.frame_slots.clear();
        self.receivers.begin_frame();
        self.frame_double_sided = 0;
        self.sway.begin_frame();
        self.frame_masked = 0;
        self.masked_slots.clear();
        self.masked_slot_used.clear();
        self.masked_bind = None;
        self.total_chunks = 0;
        self.total_bits = 0;
        self.candidates = 0;
        // Slots of materials no longer drawn are reclaimed when the table runs three quarters
        // full: it is rebuilt from the sets, and this frame's instances take new slots.
        if self.material_slots.len() >= MAX_MATERIAL_SLOTS / 4 * 3 {
            self.material_slots.clear();
            self.slot_of.clear();
            self.slot_used.clear();
            self.persistent_dirty = true;
        }
        if self.levels.wants_compaction() {
            self.compact_levels();
        }
        if self.persistent_dirty {
            self.rebuild_persistent();
        }
        let set_slots: Vec<u32> = self
            .sets
            .values()
            .filter(|set| !set.models.is_empty())
            .filter_map(|set| self.slot_of.get(&set.material_key).copied())
            .collect();
        for slot in set_slots {
            self.mark_used(slot);
        }
        if self.persistent_masked > 0 {
            let masked: Vec<u32> = self
                .sets
                .values()
                .filter(|set| !set.models.is_empty() && alpha_tests(&set.material))
                .filter_map(|set| self.slot_of.get(&set.material_key).copied())
                .collect();
            for slot in masked {
                self.note_masked(slot);
            }
        }
    }

    /// Rebuilds the level table from the resident meshes (its holes passed half): every mesh's
    /// runs move, so the persistent instances are rebuilt too.
    fn compact_levels(&mut self) {
        self.levels.rebuild();
        let mut keys: Vec<u64> = self.meshes.keys().copied().collect();
        keys.sort_unstable();
        for key in keys {
            let runs = self.mesh_runs.get(&key).map_or(&[][..], Vec::as_slice);
            let range = self.levels.insert(runs);
            if let Some(slot) = self.meshes.get_mut(&key) {
                slot.runs = range;
            }
        }
        self.persistent_dirty = true;
    }

    /// Notes a masked instance's material slot for the stats.
    fn note_masked(&mut self, slot: u32) {
        if self.masked_slot_used.insert(slot) {
            self.masked_slots.push(slot);
        }
    }

    fn mark_used(&mut self, slot: u32) {
        let i = slot as usize;
        if self.slot_used.len() <= i {
            self.slot_used.resize(i + 1, false);
        }
        if !self.slot_used[i] {
            self.slot_used[i] = true;
            self.frame_slots.push(slot);
        }
    }

    /// Test hook: fills the material slot table with unused keys up to `len` entries, so a test
    /// reaches the high slots and the rebuild without creating tens of thousands of materials.
    #[cfg(test)]
    pub(crate) fn fill_material_slots(&mut self, len: usize) {
        while self.material_slots.len() < len {
            let key = u64::MAX - self.material_slots.len() as u64;
            self.slot_for(key);
        }
    }

    /// Test hook: whether phase 1 fills the empty ID by a draw (ADR-5807).
    #[cfg(test)]
    pub(crate) fn fills_ids_by_draw(&self) -> bool {
        self.pipes.as_ref().is_some_and(|p| p.id_fill.is_some())
    }

    /// Test hook: records in the level-run table (holes included).
    #[cfg(test)]
    pub(crate) fn level_records(&self) -> usize {
        self.levels.len()
    }

    /// Test hook: entries in the material slot table, and the slot `material_key` holds.
    #[cfg(test)]
    pub(crate) fn material_slot_state(&self, material_key: u64) -> (usize, Option<u32>) {
        (
            self.material_slots.len(),
            self.slot_of.get(&material_key).copied(),
        )
    }

    /// The material slot of `material_key`, allocated on first use. `None` when the table is full.
    fn slot_for(&mut self, material_key: u64) -> Option<u32> {
        if let Some(&slot) = self.slot_of.get(&material_key) {
            return Some(slot);
        }
        if self.material_slots.len() >= MAX_MATERIAL_SLOTS {
            return None;
        }
        let slot = self.material_slots.len() as u32;
        self.material_slots.push(material_key);
        self.slot_of.insert(material_key, slot);
        Some(slot)
    }

    fn make_instance(
        slot: MeshSlot,
        model: [[f32; 4]; 4],
        bits: u64,
        material: u32,
        shadow: ([f32; 4], u32),
        double_sided: bool,
        masked: bool,
    ) -> GpuInstance {
        let scale = max_stretch(&model);
        let m = regia_core::glam::Mat4::from_cols_array_2d(&model);
        let center = m.transform_point3(regia_core::glam::Vec3::new(
            slot.bounds[0],
            slot.bounds[1],
            slot.bounds[2],
        ));
        GpuInstance {
            model,
            sphere: [center.x, center.y, center.z, slot.bounds[3] * scale],
            info: [slot.meshlet_base, slot.meshlet_count, bits as u32, material],
            scale: [scale, 0.0, slot.voxel[0] * scale, slot.voxel[1] * scale],
            shadow: shadow.0,
            flags: [
                shadow.1,
                if double_sided { VIS_DOUBLE_SIDED } else { 0 } | if masked { VIS_MASKED } else { 0 },
                slot.runs.0,
                slot.runs.1,
            ],
        }
    }

    /// Some instance this frame (persistent or not) is double-sided: the cull-none raster
    /// draws run.
    pub(crate) fn any_double_sided(&self) -> bool {
        self.persistent_double_sided + self.frame_double_sided > 0
    }

    /// Adds one instance of a resident mesh (`prev_model`: its model matrix last frame, for
    /// motion vectors; `double_sided`: its material draws back faces). Returns its index for
    /// [`Self::set_shadow_mask`], or `None` (nothing added) when the mesh is not resident or a
    /// limit is reached; the caller then draws it forward.
    pub(crate) fn push_instance(
        &mut self,
        mesh_key: u64,
        material_key: u64,
        model: [[f32; 4]; 4],
        prev_model: [[f32; 4]; 4],
        shadow: ShadowCast,
        double_sided: bool,
    ) -> Option<usize> {
        self.push(
            mesh_key,
            material_key,
            model,
            prev_model,
            shadow,
            double_sided,
            false,
        )
    }

    /// True when `device` can draw masked clusters alpha-tested: it carries the texture table,
    /// whose index the alpha test samples through (ADR-3915). Elsewhere masked meshes stay on the
    /// forward path.
    pub(crate) fn masked_supported(device: &wgpu::Device) -> bool {
        crate::texture_table::device_support(device).capacity().is_some()
    }

    /// [`Self::push_instance`] for an alpha-tested material ([`alpha_tests`], ADR-3915). The
    /// instance casts no cluster shadow: its caller keeps it in the forward shadow passes, which
    /// alpha-test. The frame must call [`Self::bind_masked`] before [`Self::record_geometry`].
    pub(crate) fn push_masked_instance(
        &mut self,
        mesh_key: u64,
        material_key: u64,
        model: [[f32; 4]; 4],
        prev_model: [[f32; 4]; 4],
        double_sided: bool,
    ) -> Option<usize> {
        self.push(
            mesh_key,
            material_key,
            model,
            prev_model,
            ShadowCast::Off,
            double_sided,
            true,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        mesh_key: u64,
        material_key: u64,
        model: [[f32; 4]; 4],
        prev_model: [[f32; 4]; 4],
        shadow: ShadowCast,
        double_sided: bool,
        masked: bool,
    ) -> Option<usize> {
        // A receiver's UV2 was made resident just before this push (`Self::ensure_uv2`).
        let receiver = self.receivers.take_armed(mesh_key);
        let slot = self.meshes.get(&mesh_key).copied()?;
        if self.persistent.len() + self.instances.len() >= MAX_INSTANCES {
            return None;
        }
        let material = self.slot_for(material_key)?;
        self.mark_used(material);
        let bits = self.persistent_bits + self.total_bits;
        if bits + u64::from(slot.meshlet_count) > u64::from(u32::MAX) {
            return None;
        }
        let index = self.instances.len();
        self.instances.push(Self::make_instance(
            slot,
            model,
            bits,
            material,
            shadow_fields(shadow, false),
            double_sided,
            masked,
        ));
        self.lod_keys.push(None);
        self.frame_double_sided += usize::from(double_sided);
        if masked {
            self.frame_masked += 1;
            self.note_masked(material);
        }
        if receiver {
            self.receivers.note(material);
        }
        self.prev_models.push(prev_model);
        self.total_bits += u64::from(slot.meshlet_count);
        self.total_chunks += u64::from(slot.meshlet_count.div_ceil(64));
        self.candidates += u64::from(slot.meshlet_count);
        Some(index)
    }

    /// Sun cascades (bit `i` = cascade `i`) of this frame's instance `index` (from
    /// [`Self::push_instance`]); the renderer's per-caster cascade choice.
    pub(crate) fn set_shadow_mask(&mut self, index: usize, mask: u8) {
        if let Some(inst) = self.instances.get_mut(index)
            && inst.flags[0] & SHADOW_CASTS != 0
        {
            inst.flags[0] = (inst.flags[0] & !0xff) | u32::from(mask);
        }
    }

    /// Whether the next rebuild emits instances for `set`. A masked set on a device without
    /// the texture table is stored and draws nothing, so it takes no instance budget.
    fn set_draws_instances(&self, set: &StoredSet) -> bool {
        let masked = self.masked_device && alpha_tests(&set.material);
        if set.material.alpha_mode == crate::lab_frame::ALPHA_MASK && !masked {
            return false;
        }
        self.meshes.contains_key(&set.mesh_key)
    }

    /// Instances and cluster-bit words the next rebuild will emit for every stored set except
    /// `except_key` (the set about to be replaced).
    fn persistent_budget_except(&self, except_key: u64) -> (usize, u64) {
        let mut instances = 0usize;
        let mut bits = 0u64;
        for (&key, set) in &self.sets {
            if key == except_key || !self.set_draws_instances(set) {
                continue;
            }
            let Some(slot) = self.meshes.get(&set.mesh_key) else {
                continue;
            };
            let n = set.models.len();
            instances = instances.saturating_add(n);
            let n_bits = u64::try_from(n).unwrap_or(u64::MAX);
            bits = bits.saturating_add(u64::from(slot.meshlet_count).saturating_mul(n_bits));
        }
        (instances, bits)
    }

    /// True when `models` instances of a mesh with `meshlet_count` clusters fit beside the sets
    /// other than `except_key`: the instance cap and the `u32` cluster-bit cursor.
    fn models_fit_budget(&self, except_key: u64, meshlet_count: u32, models: usize) -> bool {
        if models == 0 {
            return true;
        }
        let (used_instances, used_bits) = self.persistent_budget_except(except_key);
        let Some(instances) = used_instances.checked_add(models) else {
            return false;
        };
        if instances > MAX_INSTANCES {
            return false;
        }
        let Ok(models_u64) = u64::try_from(models) else {
            return false;
        };
        let Some(add) = u64::from(meshlet_count).checked_mul(models_u64) else {
            return false;
        };
        used_bits
            .checked_add(add)
            .is_some_and(|bits| bits <= u64::from(u32::MAX))
    }

    /// Stores (or replaces) a persistent instance set. False when the mesh has no clusters
    /// (that set is removed) or when its models do not fit the remaining instance and
    /// cluster-bit budget (the previous set stays, and this call stores nothing).
    pub(crate) fn set_instance_set(&mut self, set_key: u64, set: &VisInstanceSet<'_>) -> bool {
        if !self.ensure_mesh(set.mesh_key, set.vertices, set.indices) {
            self.remove_instance_set(set_key);
            return false;
        }
        let Some(slot) = self.meshes.get(&set.mesh_key).copied() else {
            self.remove_instance_set(set_key);
            return false;
        };
        // A masked set that cannot alpha-test is stored and draws nothing (no budget).
        let masked = self.masked_device && alpha_tests(set.material);
        let draws = set.material.alpha_mode != crate::lab_frame::ALPHA_MASK || masked;
        if draws && !self.models_fit_budget(set_key, slot.meshlet_count, set.models.len()) {
            return false;
        }
        let trace = trace_mesh_of(set, self.sets.values().map(|stored| &stored.trace));
        let normals = probe_normals_of(set, &trace, self.sets.values());
        // An adopted swaying material draws under its standard twin; the record comes at the rebuild.
        let (sway, twin) = if super::sway::sway_gate(set.material) == super::sway::SwayGate::Clusters {
            match crate::FoliageSway::plain(set.material) {
                Some(plain) => (Some(Arc::new(set.material.clone())), Some(plain)),
                None => (None, None),
            }
        } else {
            (None, None)
        };
        self.sets.insert(
            set_key,
            StoredSet {
                mesh_key: set.mesh_key,
                material_key: if twin.is_some() {
                    set.material_key ^ sway::SWAY_TWIN_TAG
                } else {
                    set.material_key
                },
                material: Arc::new(twin.unwrap_or_else(|| set.material.clone())),
                sway,
                models: set.models.to_vec(),
                shadow: set.shadow,
                shadows: if set.instance_shadows.len() == set.models.len() {
                    set.instance_shadows.to_vec()
                } else {
                    Vec::new()
                },
                trace,
                normals,
                scale_range: scale_range_of(set.models),
            },
        );
        self.sets_changed();
        true
    }

    /// Every persistent set that draws, for the probes' scene (ADR-5457): its mesh, material and
    /// models, in set-key order. A masked set where its alpha test cannot run draws nothing
    /// (`rebuild_persistent`), so the probes do not trace it either.
    pub(crate) fn probe_sets(&self) -> impl Iterator<Item = crate::ddgi::ProbeSet<'_>> {
        self.sets
            .iter()
            .filter(|(_, set)| {
                set.material.alpha_mode != crate::lab_frame::ALPHA_MASK
                    || (self.masked_device && alpha_tests(&set.material))
            })
            .map(|(&set_key, set)| crate::ddgi::ProbeSet {
                set_key,
                generation: self.sets_generation,
                mesh_key: set.mesh_key,
                material_key: set.material_key,
                material: &set.material,
                positions: &set.trace.positions,
                normals: &set.normals,
                indices: &set.trace.indices,
                models: &set.models,
                shadow: set.shadow,
                shadows: &set.shadows,
                scale_range: set.scale_range,
            })
    }

    /// Every persistent set for the traced sun's scene (ADR-2310): its mesh, its models, and
    /// each instance's shadow policy (`shadow` for every instance when the slice is empty).
    pub(crate) fn trace_sets(
        &self,
    ) -> impl Iterator<
        Item = (
            &crate::hwrt::TraceCasterMesh,
            &[[[f32; 4]; 4]],
            ShadowCast,
            &[ShadowCast],
        ),
    > {
        self.sets.values().map(|set| {
            (
                &set.trace,
                set.models.as_slice(),
                set.shadow,
                set.shadows.as_slice(),
            )
        })
    }

    /// Drops a persistent instance set. False when there was none.
    pub(crate) fn remove_instance_set(&mut self, set_key: u64) -> bool {
        let removed = self.sets.remove(&set_key).is_some();
        if removed {
            self.sets_changed();
        }
        removed
    }

    fn sets_changed(&mut self) {
        self.persistent_dirty = true;
        self.sets_changed = true;
        self.sets_generation = self.sets_generation.wrapping_add(1);
    }

    /// True once after any set change (the renderer then redraws its cached shadow maps).
    pub(crate) fn take_sets_changed(&mut self) -> bool {
        std::mem::take(&mut self.sets_changed)
    }

    /// True when a persistent instance picks its sun cascades on the GPU (from the snapped eye).
    pub(crate) fn gpu_masked(&self) -> bool {
        self.gpu_masked
    }

    /// Bumped on every set change.
    pub(crate) fn sets_generation(&self) -> u64 {
        self.sets_generation
    }

    /// Material key and material of every persistent set (the renderer keeps them resident).
    pub(crate) fn set_materials(&self) -> Vec<(u64, Arc<LabMaterial>)> {
        self.sets
            .values()
            .map(|set| (set.material_key, Arc::clone(&set.material)))
            .collect()
    }

    /// Rebuilds the persistent instances from the sets (a set changed). Persistent instances
    /// come first in the instance buffer, so their visibility bits keep their place.
    fn rebuild_persistent(&mut self) {
        let _span = tracing::info_span!("visbuffer_rebuild_sets").entered();
        self.persistent_dirty = false;
        self.persistent_upload = true;
        self.persistent.clear();
        self.gpu_masked = false;
        self.persistent_bits = 0;
        self.persistent_chunks = 0;
        self.persistent_double_sided = 0;
        self.persistent_masked = 0;
        self.sway.begin_persistent();
        self.sway_boxes.clear();
        // A full table is rebuilt from what is still referenced.
        let needed: HashSet<u64> = self.sets.values().map(|set| set.material_key).collect();
        if self.material_slots.len() + needed.len() > MAX_MATERIAL_SLOTS {
            self.material_slots.clear();
            self.slot_of.clear();
            self.slot_used.clear();
            self.frame_slots.clear();
        }
        let sets = std::mem::take(&mut self.sets);
        for set in sets.values() {
            // A masked set draws alpha-tested where the device has the table (ADR-3915). It has no
            // forward path to stay on, so where its test cannot run it draws nothing rather than
            // solid cards, and it casts no cluster shadows: the shadow raster has no alpha test,
            // and a set has no forward shadow draw.
            let masked = self.masked_device && alpha_tests(&set.material);
            if set.material.alpha_mode == crate::lab_frame::ALPHA_MASK && !masked {
                continue;
            }
            let (Some(slot), Some(material)) = (
                self.meshes.get(&set.mesh_key).copied(),
                self.slot_for(set.material_key),
            ) else {
                continue;
            };
            let shared = shadow_fields(if masked { ShadowCast::Off } else { set.shadow }, true);
            // The whole set or none of it. A prefix would leave instances that
            // `set_instance_set` already reported as stored. A skipped set claims no sway record.
            let model_count = set.models.len();
            let Some(need) = u64::try_from(model_count)
                .ok()
                .and_then(|n| u64::from(slot.meshlet_count).checked_mul(n))
            else {
                continue;
            };
            if self.persistent.len().saturating_add(model_count) > MAX_INSTANCES
                || self.persistent_bits.saturating_add(need) > u64::from(u32::MAX)
            {
                continue;
            }
            let set_sway = set.sway.as_deref().and_then(|material| {
                let sway = self.sway.sway_of(material);
                if sway.is_none() {
                    self.note_persistent_full();
                }
                sway
            });
            let first_instance = self.persistent.len();
            for (i, model) in set.models.iter().enumerate() {
                let shadow = match set.shadows.get(i) {
                    Some(&own) if !masked => shadow_fields(own, true),
                    _ => shared,
                };
                self.gpu_masked |= shadow.1 & SHADOW_GPU_MASK != 0;
                self.persistent.push(Self::make_instance(
                    slot,
                    *model,
                    self.persistent_bits,
                    material,
                    shadow,
                    set.material.double_sided,
                    masked,
                ));
                if let Some(sway) = set_sway {
                    self.apply_persistent_sway(self.persistent.len() - 1, sway);
                }
                self.persistent_double_sided += usize::from(set.material.double_sided);
                self.persistent_masked += usize::from(masked);
                self.persistent_bits += u64::from(slot.meshlet_count);
                self.persistent_chunks += u64::from(slot.meshlet_count.div_ceil(64));
            }
            if let Some(bounds) = Self::sway_box_of(&self.persistent[first_instance..]) {
                self.sway_boxes.push(bounds);
            }
        }
        self.sets = sets;
    }

    /// Instances this frame (persistent sets plus the mesh list's).
    pub(crate) fn instance_count(&self) -> usize {
        self.persistent.len() + self.instances.len()
    }

    /// This frame's mesh-list instances with an object identity, as `(object key, first
    /// visibility bit, bit count)` into `out` (its capacity reused): what the occlusion readback
    /// reduces the cluster bits by (ADR-7286). Persistent sets have no per-entity identity.
    pub(crate) fn occlusion_ranges(&self, out: &mut Vec<(u64, u32, u32)>) {
        out.clear();
        out.extend(
            self.instances
                .iter()
                .zip(&self.lod_keys)
                .filter_map(|(instance, key)| key.map(|key| (key, instance.info[2], instance.info[1]))),
        );
    }

    /// The cluster visibility bits (`vis_cull.wgsl` `bits`: phase 2 leaves a cluster's bit set
    /// when it drew) and how many 32-bit words this frame's instances use (ADR-7286).
    pub(crate) fn visibility_bits(&self) -> Option<(&wgpu::Buffer, u64)> {
        let words = (self.persistent_bits + self.total_bits).div_ceil(32);
        self.buffers.bits.as_ref().map(|(buffer, _)| (buffer, words))
    }

    /// Material slots shaded this frame and their material keys.
    pub(crate) fn frame_materials(&self) -> impl Iterator<Item = (u32, u64)> + '_ {
        self.frame_slots
            .iter()
            .filter_map(|&slot| self.material_slots.get(slot as usize).map(|&key| (slot, key)))
    }

    /// Material depth target of the resolve pass (after [`Self::record_geometry`]).
    pub(crate) fn material_depth_view(&self) -> Option<&wgpu::TextureView> {
        self.targets.as_ref().map(|t| &t.material_depth)
    }

    /// Group 1 of the resolve pipelines (after [`Self::record_geometry`]).
    pub(crate) fn resolve_bind_group(&self) -> Option<&wgpu::BindGroup> {
        self.resolve_bg.as_ref().map(|(_, bg)| bg)
    }

    fn ensure_pipes(&mut self, d: &TrackedDevice) -> Result<(), RenderError> {
        if self.pipes.is_some() {
            return Ok(());
        }
        let compute = wgpu::ShaderStages::COMPUTE;
        let cull_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis cull"),
            entries: &[
                buffer_entry(0, compute, wgpu::BufferBindingType::Uniform),
                buffer_entry(1, compute, storage(true)),
                buffer_entry(2, compute, storage(true)),
                buffer_entry(3, compute, storage(false)),
                buffer_entry(4, compute, storage(false)),
                buffer_entry(5, compute, storage(false)),
                buffer_entry(6, compute, storage(false)),
                texture_entry(7, compute, wgpu::TextureSampleType::Float { filterable: false }),
                buffer_entry(8, compute, storage(false)),
                buffer_entry(9, compute, storage(true)),
                buffer_entry(10, compute, storage(false)),
            ],
        });
        let cull_module = module(d, "vis_cull.wgsl", include_str!("vis_cull.wgsl"))?;
        let cull_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vis cull"),
            bind_group_layouts: &[Some(&cull_bgl)],
            immediate_size: 0,
        });
        let compute_pipe = |layout: &wgpu::PipelineLayout, module: &wgpu::ShaderModule, entry: &str| {
            d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(layout),
                module,
                entry_point: Some(entry),
                compilation_options: Default::default(),
                cache: crate::pipeline_cache::cache_for(&d).as_deref(),
            })
        };
        let hiz_copy_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis hiz copy"),
            entries: &[
                texture_entry(0, compute, wgpu::TextureSampleType::Depth),
                storage_texture_entry(2),
            ],
        });
        let hiz_down_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis hiz down"),
            entries: &[
                texture_entry(1, compute, wgpu::TextureSampleType::Float { filterable: false }),
                storage_texture_entry(2),
            ],
        });
        let hiz_module = module(d, "vis_hiz.wgsl", include_str!("vis_hiz.wgsl"))?;
        let layout_of = |bgl: &wgpu::BindGroupLayout, label: &str| {
            d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some(label),
                bind_group_layouts: &[Some(bgl)],
                immediate_size: 0,
            })
        };
        let hiz_copy_layout = layout_of(&hiz_copy_bgl, "vis hiz copy");
        let hiz_down_layout = layout_of(&hiz_down_bgl, "vis hiz down");
        let vertex = wgpu::ShaderStages::VERTEX;
        let fragment = wgpu::ShaderStages::FRAGMENT;
        let raster_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis raster"),
            entries: &[
                // The masked fragment stage reads the arena stride and the mip bias (ADR-3915).
                buffer_entry(0, vertex | fragment, wgpu::BufferBindingType::Uniform),
                buffer_entry(1, vertex, storage(true)),
                buffer_entry(2, vertex, storage(true)),
                buffer_entry(3, vertex, storage(true)),
                buffer_entry(4, vertex, storage(true)),
                // The sway records (ADR-3916): uniform, so the storage-buffer count stays.
                buffer_entry(5, vertex, wgpu::BufferBindingType::Uniform),
            ],
        });
        let raster_layout = layout_of(&raster_bgl, "vis raster");
        let raster_module = module(d, "vis_raster.wgsl", include_str!("vis_raster.wgsl"))?;
        let fill_ids = !crate::backend_policy::clears_integer_targets_exactly(d.adapter_info().backend);
        #[cfg(test)]
        let fill_ids = fill_ids || self.force_id_fill;
        let id_fill = fill_ids.then(|| id_fill_pipeline(d, &raster_module));
        let resolve_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis resolve"),
            entries: &[
                buffer_entry(8, fragment, wgpu::BufferBindingType::Uniform),
                buffer_entry(9, fragment, storage(true)),
                buffer_entry(10, fragment, storage(true)),
                buffer_entry(11, fragment, storage(true)),
                buffer_entry(12, fragment, storage(true)),
                texture_entry(13, fragment, wgpu::TextureSampleType::Uint),
                buffer_entry(14, fragment, wgpu::BufferBindingType::Uniform),
            ],
        });
        let velocity_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis velocity"),
            entries: &[
                buffer_entry(0, fragment, wgpu::BufferBindingType::Uniform),
                buffer_entry(1, fragment, storage(true)),
                buffer_entry(2, fragment, storage(true)),
                buffer_entry(3, fragment, storage(true)),
                buffer_entry(4, fragment, storage(true)),
                buffer_entry(5, fragment, storage(true)),
                texture_entry(6, fragment, wgpu::TextureSampleType::Uint),
                buffer_entry(7, fragment, wgpu::BufferBindingType::Uniform),
            ],
        });
        let velocity_depth_bgl = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("vis velocity depth"),
            entries: &[texture_entry(0, fragment, wgpu::TextureSampleType::Depth)],
        });
        let velocity_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("vis velocity"),
            bind_group_layouts: &[Some(&velocity_bgl), Some(&velocity_depth_bgl)],
            immediate_size: 0,
        });
        let velocity_module = module(d, "vis_velocity.wgsl", include_str!("vis_velocity.wgsl"))?;
        let velocity = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("vis velocity"),
            layout: Some(&velocity_layout),
            vertex: wgpu::VertexState {
                module: &velocity_module,
                entry_point: Some("vs_fullscreen"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &velocity_module,
                entry_point: Some("fs_velocity"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: VELOCITY_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        self.pipes = Some(Pipes {
            velocity_bgl,
            velocity_depth_bgl,
            velocity,
            reset: compute_pipe(&cull_layout, &cull_module, "reset"),
            cull_instances: compute_pipe(&cull_layout, &cull_module, "cull_instances"),
            phase1: compute_pipe(&cull_layout, &cull_module, "cull_phase1"),
            phase2: compute_pipe(&cull_layout, &cull_module, "cull_phase2"),
            scan_local: compute_pipe(&cull_layout, &cull_module, "scan_local"),
            scan_top: [
                compute_pipe(&cull_layout, &cull_module, "scan_top_instances"),
                compute_pipe(&cull_layout, &cull_module, "scan_top_phase1"),
                compute_pipe(&cull_layout, &cull_module, "scan_top_phase2"),
            ],
            place_instances: compute_pipe(&cull_layout, &cull_module, "place_instances"),
            place: [
                compute_pipe(&cull_layout, &cull_module, "place_phase1"),
                compute_pipe(&cull_layout, &cull_module, "place_phase2"),
            ],
            lod_prepare: compute_pipe(&cull_layout, &cull_module, "lod_prepare"),
            lod_commit: compute_pipe(&cull_layout, &cull_module, "lod_commit"),
            hiz_copy: compute_pipe(&hiz_copy_layout, &hiz_module, "copy_depth"),
            hiz_down: compute_pipe(&hiz_down_layout, &hiz_module, "downsample"),
            cull_bgl,
            hiz_copy_bgl,
            hiz_down_bgl,
            raster_bgl,
            raster_layout,
            raster_module,
            id_fill,
            resolve_bgl,
        });
        Ok(())
    }

    /// `reversed`: the double-sided clusters at the end of each list (`vs_vis_reversed`).
    fn raster_pipeline(
        &mut self,
        d: &TrackedDevice,
        double_sided: bool,
        reversed: bool,
    ) -> Option<wgpu::RenderPipeline> {
        if let Some(pipe) = self.raster_pipes.get(&(double_sided, reversed)) {
            return Some(pipe.clone());
        }
        let pipes = self.pipes.as_ref()?;
        let pipe = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(if reversed {
                "vis raster (double-sided)"
            } else {
                "vis raster"
            }),
            layout: Some(&pipes.raster_layout),
            vertex: wgpu::VertexState {
                module: &pipes.raster_module,
                entry_point: Some(if reversed { "vs_vis_reversed" } else { "vs_vis" }),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &pipes.raster_module,
                entry_point: Some("fs_vis"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: VIS_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: if double_sided {
                    None
                } else {
                    Some(wgpu::Face::Back)
                },
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        self.raster_pipes.insert((double_sided, reversed), pipe.clone());
        Some(pipe)
    }

    /// The masked clusters' raster pipeline (ADR-3915): `tested` takes the alpha test of
    /// `vis_raster_masked.wgsl` (texture table devices, [`Self::bind_masked`] this frame), else the
    /// unbound fragment discards them, preserving the depth through their holes. `reversed`: the double-sided list.
    fn masked_pipeline(
        &mut self,
        d: &TrackedDevice,
        cull_none: bool,
        reversed: bool,
        tested: bool,
    ) -> Result<Option<wgpu::RenderPipeline>, RenderError> {
        if let Some(pipe) = self.masked_pipes.get(&(cull_none, reversed, tested)) {
            return Ok(Some(pipe.clone()));
        }
        if tested && self.masked_layout.is_none() {
            let Some(table) = crate::texture_table::table_layout(d) else {
                return Ok(None);
            };
            let capacity = crate::texture_table::device_support(d).capacity().unwrap_or(1);
            let group = self.single.group_layout(d);
            let Some(pipes) = self.pipes.as_ref() else {
                return Ok(None);
            };
            self.masked_layout = Some(d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("vis raster masked"),
                bind_group_layouts: &crate::texture_table::with_table(
                    &[Some(&pipes.raster_bgl), Some(&group)],
                    Some(&table),
                ),
                immediate_size: 0,
            }));
            let naga = compose_source(
                masked_raster_source(),
                "vis_raster_masked.wgsl",
                masked_defs(capacity),
            )?;
            self.masked_module = Some(d.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("vis raster masked"),
                source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(naga)),
            }));
        }
        let Some(pipes) = self.pipes.as_ref() else {
            return Ok(None);
        };
        let (layout, module, fragment) = if tested {
            let (Some(layout), Some(module)) = (self.masked_layout.as_ref(), self.masked_module.as_ref())
            else {
                return Ok(None);
            };
            (layout, module, "fs_vis_masked")
        } else {
            (&pipes.raster_layout, &pipes.raster_module, "fs_vis_untested")
        };
        let pipe = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(if tested {
                "vis raster masked"
            } else {
                "vis raster masked (untested)"
            }),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module,
                entry_point: Some(if reversed {
                    "vs_vis_masked_reversed"
                } else {
                    "vs_vis_masked"
                }),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module,
                entry_point: Some(fragment),
                targets: &[Some(wgpu::ColorTargetState {
                    format: VIS_FORMAT,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                cull_mode: if cull_none { None } else { Some(wgpu::Face::Back) },
                ..Default::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&d).as_deref(),
        });
        self.masked_pipes
            .insert((cull_none, reversed, tested), pipe.clone());
        Ok(Some(pipe))
    }

    /// Masked instances this frame (persistent and the mesh list's).
    pub(crate) fn masked_instance_count(&self) -> usize {
        self.persistent_masked + self.frame_masked
    }

    /// Distinct materials of this frame's masked instances.
    fn masked_material_count(&self) -> u32 {
        self.masked_slots.len() as u32
    }

    /// Binds this frame's masked raster (ADR-3915): `arena` (the material arena, whole) and the
    /// frame's map from material slots to its records (`record_of`, as the single-pass resolve
    /// writes it), the texture table group, and the frame's texture mip bias. Call it after the
    /// frame's instances are pushed and before [`Self::record_geometry`]; without it the masked
    /// clusters discard every fragment. Returns whether the alpha test is bound.
    pub(crate) fn bind_masked(
        &mut self,
        d: &TrackedDevice,
        q: &TrackedQueue,
        arena: &crate::bindless::MaterialArena,
        table: &wgpu::BindGroup,
        mip_bias: f32,
        record_of: impl Fn(u64) -> Option<u32>,
    ) -> bool {
        self.masked_bind = None;
        if self.masked_instance_count() == 0 || !Self::masked_supported(d) {
            return false;
        }
        let _span = tracing::info_span!("visbuffer_bind_masked").entered();
        let stride = arena.stats().stride.max(4);
        let Some((group, _)) = self.single_pass_group(d, q, arena, record_of) else {
            return false;
        };
        self.masked_bind = Some(MaskedBind {
            group,
            table: table.clone(),
            stride_words: u32::try_from(stride / 4).unwrap_or(u32::MAX),
            mip_bias,
        });
        true
    }

    /// Builds the cull-none raster pipelines of double-sided instances (camera and shadow)
    /// off the frame path ([`crate::FrameRenderer::prewarm_double_sided`]).
    ///
    /// # Errors
    ///
    /// A visibility-buffer shader failed to build.
    pub(crate) fn prewarm_double_sided(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        let before = CreateLedger::mark();
        let out = self.prewarm_pipelines(device);
        self.creates.charge(before);
        out
    }

    fn prewarm_pipelines(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        let d = &TrackedDevice::observe(device);
        self.ensure_pipes(d)?;
        self.ensure_shadow_pipes(d)?;
        self.raster_pipeline(d, true, true);
        self.shadow_raster_pipeline(d, true, true);
        Ok(())
    }

    /// The resolve pipelines for a colour target and forward module (`sun_rt`, ADR-2035):
    /// (material depth, shade).
    pub(crate) fn resolve_pipelines(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        encode: bool,
        sun_rt: bool,
        view_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
    ) -> Result<(wgpu::RenderPipeline, wgpu::RenderPipeline), RenderError> {
        if let Some(pair) = self.resolve_pipes.get(&(format, encode, sun_rt)) {
            return Ok(pair.clone());
        }
        let before = CreateLedger::mark();
        let out = self.build_resolve_pipelines(device, format, encode, sun_rt, view_bgl, mat_bgl);
        self.creates.charge(before);
        out
    }

    fn build_resolve_pipelines(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        encode: bool,
        sun_rt: bool,
        view_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
    ) -> Result<(wgpu::RenderPipeline, wgpu::RenderPipeline), RenderError> {
        let d = &TrackedDevice::observe(device);
        self.ensure_pipes(d)?;
        let pipes = self
            .pipes
            .as_ref()
            .ok_or_else(|| RenderError::Shader("visibility pipelines missing".into()))?;
        let layout = self
            .resolve_layout
            .get_or_insert_with(|| {
                // The material texture table (group 4) on the table path (ADR-863).
                let table = crate::texture_table::table_layout(d);
                d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("vis resolve"),
                    bind_group_layouts: &crate::texture_table::with_table(
                        &[Some(view_bgl), Some(&pipes.resolve_bgl), Some(mat_bgl)],
                        table.as_ref(),
                    ),
                    immediate_size: 0,
                })
            })
            .clone();
        let mut defs = HashMap::new();
        defs.insert(
            "VISBUFFER".to_string(),
            naga_oil::compose::ShaderDefValue::Bool(true),
        );
        if !encode {
            defs.insert(
                "skip_srgb_encode".to_string(),
                naga_oil::compose::ShaderDefValue::Bool(true),
            );
        }
        if sun_rt {
            crate::hwrt::SunShadowVariant::RayQuery.add_forward_defs(&mut defs);
        }
        // The material binding matches this device's material layout (`bindless`, ADR-316).
        crate::bindless::add_material_defs(d, &mut defs);
        let naga = compose_source(resolve_source(), "vis_resolve.wgsl", defs)?;
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("vis resolve"),
            source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(naga)),
        });
        let pipeline = |label: &str, entry: &str, write: wgpu::ColorWrites, depth_write: bool, compare| {
            d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: wgpu::VertexState {
                    module: &module,
                    entry_point: Some("vs_vis_fullscreen"),
                    buffers: &[],
                    compilation_options: Default::default(),
                },
                fragment: Some(wgpu::FragmentState {
                    module: &module,
                    entry_point: Some(entry),
                    targets: &[Some(wgpu::ColorTargetState {
                        format,
                        blend: None,
                        write_mask: write,
                    })],
                    compilation_options: Default::default(),
                }),
                primitive: wgpu::PrimitiveState::default(),
                depth_stencil: Some(wgpu::DepthStencilState {
                    format: MATERIAL_DEPTH_FORMAT,
                    depth_write_enabled: Some(depth_write),
                    depth_compare: Some(compare),
                    stencil: wgpu::StencilState::default(),
                    bias: wgpu::DepthBiasState::default(),
                }),
                multisample: wgpu::MultisampleState::default(),
                multiview_mask: None,
                cache: crate::pipeline_cache::cache_for(&d).as_deref(),
            })
        };
        let pair = (
            pipeline(
                "vis material depth",
                "fs_vis_material_depth",
                wgpu::ColorWrites::empty(),
                true,
                wgpu::CompareFunction::Always,
            ),
            pipeline(
                "vis resolve",
                "fs_vis_resolve",
                wgpu::ColorWrites::ALL,
                false,
                wgpu::CompareFunction::Equal,
            ),
        );
        self.resolve_pipes.insert((format, encode, sun_rt), pair.clone());
        Ok(pair)
    }

    fn ensure_targets(&mut self, d: &TrackedDevice, width: u32, height: u32) {
        if self
            .targets
            .as_ref()
            .is_some_and(|t| t.width == width && t.height == height)
        {
            return;
        }
        let texture = |label: &str, format, w: u32, h: u32, mips: u32, usage| {
            d.create_texture(&wgpu::TextureDescriptor {
                label: Some(label),
                size: wgpu::Extent3d {
                    width: w,
                    height: h,
                    depth_or_array_layers: 1,
                },
                mip_level_count: mips,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format,
                usage,
                view_formats: &[],
            })
        };
        let ids = texture(
            "vis ids",
            VIS_FORMAT,
            width,
            height,
            1,
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let material = texture(
            "vis material depth",
            MATERIAL_DEPTH_FORMAT,
            width,
            height,
            1,
            wgpu::TextureUsages::RENDER_ATTACHMENT,
        );
        let hiz_size = (width.next_power_of_two(), height.next_power_of_two());
        let mips = 32 - hiz_size.0.max(hiz_size.1).leading_zeros();
        let hiz = texture(
            "vis hiz",
            wgpu::TextureFormat::R32Float,
            hiz_size.0,
            hiz_size.1,
            mips,
            wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
        );
        let mip_view = |mip: u32| {
            hiz.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: mip,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        self.targets = Some(Targets {
            width,
            height,
            ids_view: ids.create_view(&wgpu::TextureViewDescriptor::default()),
            material_depth: material.create_view(&wgpu::TextureViewDescriptor::default()),
            hiz_all: hiz.create_view(&wgpu::TextureViewDescriptor::default()),
            hiz_storage: (0..mips).map(mip_view).collect(),
            hiz_sampled: (0..mips).map(mip_view).collect(),
            hiz_size,
        });
        self.hiz_bgs = HizGroups::default();
        self.generation += 1;
    }

    fn ensure_buffers(&mut self, d: &TrackedDevice, queue: &TrackedQueue) {
        let storage_dst = wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST;
        // COPY_SRC: a growing stream copies its old buffer into the new one (ADR-7462).
        let arena_usage = storage_dst | wgpu::BufferUsages::COPY_SRC;
        let cap = self.cluster_capacity();
        let software_room = if self.settings.software_raster {
            u64::from(software::software_capacity(cap))
        } else {
            0
        };
        let b = &mut self.buffers;
        // Geometry and clusters: a buffer the size of the arena, replaced only when the arena
        // doubles (the old one copied into it on the GPU); then the ranges written since the last
        // frame (ADR-7462).
        let mut replaced = self
            .arena
            .sync(d, queue, &mut b.geo, &mut b.meshlets, arena_usage);
        // Capacities take the warm's reserve as a floor (ADR-2689); what the cull reads does not.
        let reserve = self.reserve;
        let count = (self.persistent.len() + self.instances.len()) as u64;
        let room = count.max(reserve.instances);
        let chunks = (self.persistent_chunks + self.total_chunks).max(reserve.chunks);
        let instance_bytes = room * std::mem::size_of::<GpuInstance>() as u64;
        let mut persistent_lost =
            ensure_capacity(d, &mut b.instances, "vis instances", instance_bytes, storage_dst);
        persistent_lost |= ensure_capacity(
            d,
            &mut b.prev_models,
            "vis previous models",
            room * 64,
            storage_dst,
        );
        replaced |= persistent_lost;
        if persistent_lost {
            self.persistent_upload = true;
        }
        replaced |= ensure_capacity(
            d,
            &mut b.work,
            "vis work",
            chunks * 8,
            wgpu::BufferUsages::STORAGE,
        );
        let scan = scan_regions(chunks, room);
        replaced |= ensure_capacity(
            d,
            &mut b.scan,
            "vis cull scan",
            u64::from(scan[2]) * 16,
            wgpu::BufferUsages::STORAGE,
        );
        // Visibility bits survive frames; a regrown buffer starts at zero (phase 2 draws all).
        replaced |= ensure_capacity(
            d,
            &mut b.bits,
            "vis bits",
            (self.persistent_bits + self.total_bits)
                .max(reserve.clusters)
                .div_ceil(32)
                * 4,
            // COPY_SRC: the occlusion readback copies the bits (ADR-7286). COPY_DST: a
            // split-screen view brings its own bits back in (ADR-7329).
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        );
        replaced |= ensure_capacity(
            d,
            &mut b.clusters,
            "vis clusters",
            (u64::from(cap) * 2 + software_room) * 8 + MASK_HEADER_WORDS * 4,
            // COPY_SRC: the list readback of the determinism tests (ADR-2294).
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        );
        if b.state.is_none() {
            b.state = Some(make_buffer(
                d,
                "vis state",
                STATE_WORDS * 4,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_SRC,
            ));
            b.dispatch_args = Some(make_buffer(
                d,
                "vis dispatch args",
                SCAN_ARGS_OFFSET + 16,
                wgpu::BufferUsages::INDIRECT | wgpu::BufferUsages::COPY_DST,
            ));
            let uniform = wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST;
            b.cull_params = Some(make_buffer(
                d,
                "vis cull params",
                std::mem::size_of::<CullParams>() as u64,
                uniform,
            ));
            let raster_bytes = std::mem::size_of::<RasterParams>() as u64;
            b.raster_params = Some([
                make_buffer(d, "vis raster params 1", raster_bytes, uniform),
                make_buffer(d, "vis raster params 2", raster_bytes, uniform),
            ]);
            b.resolve_params = Some(make_buffer(d, "vis resolve params", 96, uniform));
            b.velocity_params = Some(make_buffer(d, "vis velocity params", 224, uniform));
            replaced = true;
        }
        let inst_cap = b.instances.as_ref().map(|(_, cap)| *cap).unwrap_or(0);
        let slots = u32::try_from(inst_cap / std::mem::size_of::<GpuInstance>() as u64).unwrap_or(u32::MAX);
        let lod_bytes = u64::from(slots)
            .saturating_add(1)
            .saturating_add((self.persistent_bits + self.total_bits).max(reserve.clusters))
            .saturating_mul(4);
        let lod_replaced = ensure_capacity(
            d,
            &mut b.lod,
            "vis lod history",
            lod_bytes.max(4),
            wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
        );
        if lod_replaced || self.lod_slots != slots {
            self.lod_clear = true;
        }
        replaced |= lod_replaced;
        self.lod_slots = slots;
        replaced |= self.levels.sync(d, queue);
        replaced |= self.sway.ensure_buffer(d);
        if replaced {
            self.generation += 1;
        }
        self.sway.write_records(queue);
        // Persistent instances are uploaded once per change; this frame's after them.
        let size = std::mem::size_of::<GpuInstance>() as u64;
        if let (Some((instances, _)), Some((prev, _))) = (&b.instances, &b.prev_models) {
            if self.persistent_upload && !self.persistent.is_empty() {
                let _span =
                    tracing::info_span!("visbuffer_upload_sets", instances = self.persistent.len()).entered();
                queue.write_buffer(instances, 0, bytemuck::cast_slice(&self.persistent));
                let models: Vec<[[f32; 4]; 4]> = self.persistent.iter().map(|inst| inst.model).collect();
                queue.write_buffer(prev, 0, bytemuck::cast_slice(&models));
            }
            self.persistent_upload = false;
            if !self.instances.is_empty() {
                let base = self.persistent.len() as u64;
                queue.write_buffer(instances, base * size, bytemuck::cast_slice(&self.instances));
                queue.write_buffer(prev, base * 64, bytemuck::cast_slice(&self.prev_models));
            }
        }
    }

    fn cluster_capacity(&self) -> u32 {
        self.settings.max_clusters.clamp(1024, MAX_CLUSTER_SLOTS)
    }

    /// The entry of the masked lists' header in the cluster buffer: after both phases' lists and
    /// the software range (ADR-3915).
    fn masked_header(&self) -> u32 {
        let cap = self.cluster_capacity();
        let software = if self.settings.software_raster {
            software::software_capacity(cap)
        } else {
            0
        };
        cap * 2 + software
    }

    fn ensure_bind_groups(&mut self, d: &TrackedDevice) {
        let generation = self.generation;
        let (Some(pipes), Some(t), b, Some(levels), Some(sway)) = (
            self.pipes.as_ref(),
            self.targets.as_ref(),
            &self.buffers,
            self.levels.buffer(),
            self.sway.buffer(),
        ) else {
            return;
        };
        let (
            Some((instances, _)),
            Some((meshlets, _)),
            Some((work, _)),
            Some((scan, _)),
            Some((clusters, _)),
            Some((bits, _)),
            Some((geo, _)),
            Some(state),
            Some(cull_params),
            Some(raster_params),
            Some(resolve_params),
            Some((lod_hist, _)),
        ) = (
            &b.instances,
            &b.meshlets,
            &b.work,
            &b.scan,
            &b.clusters,
            &b.bits,
            &b.geo,
            &b.state,
            &b.cull_params,
            &b.raster_params,
            &b.resolve_params,
            &b.lod,
        )
        else {
            return;
        };
        if self.cull_bg.as_ref().is_none_or(|(g, _)| *g != generation) {
            let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vis cull"),
                layout: &pipes.cull_bgl,
                entries: &[
                    whole(0, cull_params),
                    whole(1, instances),
                    whole(2, meshlets),
                    whole(3, work),
                    whole(4, state),
                    whole(5, clusters),
                    whole(6, bits),
                    view(7, &t.hiz_all),
                    whole(8, scan),
                    whole(9, levels),
                    whole(10, lod_hist),
                ],
            });
            self.cull_bg = Some((generation, bg));
        }
        if self.raster_bgs.as_ref().is_none_or(|(g, _)| *g != generation) {
            let raster = |params: &wgpu::Buffer| {
                d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("vis raster"),
                    layout: &pipes.raster_bgl,
                    entries: &[
                        whole(0, params),
                        whole(1, instances),
                        whole(2, meshlets),
                        whole(3, geo),
                        whole(4, clusters),
                        whole(5, sway),
                    ],
                })
            };
            self.raster_bgs = Some((generation, [raster(&raster_params[0]), raster(&raster_params[1])]));
        }
        if self.resolve_bg.as_ref().is_none_or(|(g, _)| *g != generation) {
            let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vis resolve"),
                layout: &pipes.resolve_bgl,
                entries: &[
                    whole(8, resolve_params),
                    whole(9, instances),
                    whole(10, meshlets),
                    whole(11, geo),
                    whole(12, clusters),
                    view(13, &t.ids_view),
                    whole(14, sway),
                ],
            });
            self.resolve_bg = Some((generation, bg));
        }
        if let (Some((prev, _)), Some(params)) = (&b.prev_models, &b.velocity_params)
            && self.velocity_bg.as_ref().is_none_or(|(g, _)| *g != generation)
        {
            let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vis velocity"),
                layout: &pipes.velocity_bgl,
                entries: &[
                    whole(0, params),
                    whole(1, instances),
                    whole(2, meshlets),
                    whole(3, geo),
                    whole(4, clusters),
                    whole(5, prev),
                    view(6, &t.ids_view),
                    whole(7, sway),
                ],
            });
            self.velocity_bg = Some((generation, bg));
        }
    }

    /// Readies the software rasterizer for this frame. False when a buffer it binds is missing.
    fn prepare_software(
        &mut self,
        d: &TrackedDevice,
        queue: &TrackedQueue,
        camera: &VisView,
    ) -> Result<bool, RenderError> {
        let capacity = self.cluster_capacity();
        let b = &self.buffers;
        let (
            Some((instances, _)),
            Some((meshlets, _)),
            Some((geo, _)),
            Some((clusters, _)),
            Some(state),
            Some(sway),
        ) = (
            &b.instances,
            &b.meshlets,
            &b.geo,
            &b.clusters,
            &b.state,
            self.sway.buffer(),
        )
        else {
            return Ok(false);
        };
        self.software.prepare(
            d,
            queue,
            &software::SoftwareBindings {
                generation: self.generation,
                instances,
                meshlets,
                geo,
                clusters,
                state,
                sway,
            },
            &software::SoftwareView {
                view_proj: camera.view_proj,
                width: camera.width,
                height: camera.height,
                capacity,
                cull_back: !camera.double_sided,
            },
        )?;
        Ok(true)
    }

    /// Makes pipelines, targets and buffers ready for this frame and uploads the instances
    /// (once per frame; the shadow cull and the geometry pass share it). False when there is
    /// nothing to draw.
    pub(crate) fn prepare(
        &mut self,
        device: &wgpu::Device,
        queue: &TrackedQueue,
    ) -> Result<bool, RenderError> {
        let before = CreateLedger::mark();
        let out = self.prepare_frame(device, queue);
        self.creates.charge(before);
        out
    }

    /// [`Self::prepare`] without the charge (its callers charge).
    fn prepare_frame(&mut self, device: &wgpu::Device, queue: &TrackedQueue) -> Result<bool, RenderError> {
        if self.instance_count() == 0 {
            return Ok(false);
        }
        if self.prepared {
            return Ok(true);
        }
        let d = &TrackedDevice::observe(device);
        self.ensure_pipes(d)?;
        self.clamp_to_device(d);
        self.residency_frame();
        self.ensure_buffers(d, queue);
        // Counted meshes admitted since: their vertices, into the buffer just synced (ADR-7462).
        self.run_repacks(d, queue);
        self.prepared = true;
        Ok(true)
    }

    /// Writes the motion vectors of the visibility pixels into the TAAU velocity target (after
    /// the forward meshes' velocity draws, which may have drawn hidden meshes there). Needs a
    /// [`Self::record_geometry`] this frame. `depth` is the frame's scene depth: a visibility
    /// pixel that a forward mesh drew over later keeps that mesh's motion.
    pub(crate) fn record_velocity(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        depth: &wgpu::TextureView,
    ) {
        let before = CreateLedger::mark();
        self.velocity_pass(device, encoder, target, depth);
        self.creates.charge(before);
    }

    fn velocity_pass(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        depth: &wgpu::TextureView,
    ) {
        let Some(pipes) = self.pipes.as_ref() else {
            return;
        };
        if self.velocity_depth_bg.as_ref().is_none_or(|(v, _)| v != depth) {
            let d = TrackedDevice::observe(device);
            let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("vis velocity depth"),
                layout: &pipes.velocity_depth_bgl,
                entries: &[view(0, depth)],
            });
            self.velocity_depth_bg = Some((depth.clone(), bg));
        }
        let (Some((_, bg)), Some((_, depth_bg))) =
            (self.velocity_bg.as_ref(), self.velocity_depth_bg.as_ref())
        else {
            return;
        };
        let _span = tracing::info_span!("visbuffer_velocity").entered();
        let _crumb = crate::breadcrumbs::frame_pass("visbuffer_velocity");
        let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("vis velocity"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            ..Default::default()
        });
        pass.counted_set_pipeline(&pipes.velocity);
        pass.counted_set_bind_group(0, bg, &[]);
        pass.counted_set_bind_group(1, depth_bg, &[]);
        pass.counted_draw(0..3, 0..1);
    }

    /// Culls and rasterizes this frame's instances into the visibility target and `depth`
    /// (loaded, so the forward prepass occludes clusters). Returns false when nothing was
    /// recorded (no instances); the caller then skips the resolve.
    #[tracing::instrument(skip_all, name = "visbuffer_geometry")]
    pub(crate) fn record_geometry(
        &mut self,
        device: &wgpu::Device,
        queue: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
        camera: &VisView,
    ) -> Result<bool, RenderError> {
        let before = CreateLedger::mark();
        let out = self.geometry_passes(device, queue, encoder, depth, camera);
        self.creates.charge(before);
        out
    }

    /// Names the window view the next main-view frames draw (ADR-7329): each keeps its own
    /// cluster visibility bits, so its phase 1 draws what it saw last frame.
    pub(crate) fn set_occlusion_view(&mut self, view: u32) {
        self.occlusion_view = view;
    }

    /// Brings the next window view's cluster visibility bits into the cull's bit buffer and keeps
    /// the outgoing view's aside (ADR-7329): two buffer copies per view change, no bind group
    /// change. Phase 1 of a split-screen view then draws the clusters *that view* saw last frame,
    /// so an occluder only it sees is in the pyramid its phase 2 tests. A view without bits of
    /// its own starts at zero (phase 1 draws nothing, phase 2 tests every cluster: what a new
    /// buffer does). One view never copies. The bits are only phase 1's hint: phase 2 retests
    /// every selected cluster and rewrites its bit, so a stale or shifted bit costs work, never
    /// a wrong result.
    fn switch_bits_view(&mut self, d: &TrackedDevice, encoder: &mut wgpu::CommandEncoder) {
        let view = self.occlusion_view;
        if self.bits_view == Some(view) || view as usize >= crate::prepass_occlusion::MAX_OCCLUSION_VIEWS {
            return;
        }
        let Some((bits, _)) = self.buffers.bits.as_ref() else {
            return;
        };
        let used = ((self.persistent_bits + self.total_bits).div_ceil(32) * 4).min(bits.size());
        if self.bits_view.is_none() && std::mem::take(&mut self.bits_stale) && used > 0 {
            // A retired view's bits (ADR-8620): the next view starts from zero, as a new view.
            encoder.clear_buffer(bits, 0, Some(used));
        }
        if let Some(out) = self.bits_view.map(|v| v as usize) {
            let _span = tracing::info_span!("visbuffer_bits_view").entered();
            if self.view_bits.len() <= out {
                self.view_bits.resize_with(out + 1, || None);
            }
            let slot = &mut self.view_bits[out];
            if slot.as_ref().is_none_or(|(saved, _)| saved.size() < used) {
                *slot = Some((
                    make_buffer(
                        d,
                        "vis bits view",
                        used,
                        wgpu::BufferUsages::COPY_SRC | wgpu::BufferUsages::COPY_DST,
                    ),
                    0,
                ));
            }
            if let Some((saved, saved_used)) = slot.as_mut() {
                if used > 0 {
                    encoder.copy_buffer_to_buffer(bits, 0, saved, 0, used);
                }
                *saved_used = used;
            }
            let restored = match self.view_bits.get(view as usize).and_then(Option::as_ref) {
                Some((saved, saved_used)) => {
                    let n = (*saved_used).min(used);
                    if n > 0 {
                        encoder.copy_buffer_to_buffer(saved, 0, bits, 0, n);
                    }
                    n
                }
                None => 0,
            };
            if restored < used {
                encoder.clear_buffer(bits, restored, Some(used - restored));
            }
        }
        // Each window view's LOD hysteresis history goes with its bits (ADR-8620).
        self.switch_lod_view(d, encoder, self.bits_view, view);
        self.bits_view = Some(view);
    }

    /// Frees the visibility bits kept for window views at index `views` and above (ADR-7329).
    pub(crate) fn retain_bits_views(&mut self, views: usize) {
        self.view_bits.truncate(views);
        self.retain_lod_views(views);
        if let Some(view) = self.bits_view.filter(|&view| view as usize >= views) {
            self.retire_view(view);
        }
        self.drop_lone_view_buffers();
    }

    /// Test hook: discards the live view's stored per-instance cluster boundaries. The next
    /// visibility frame uses the hard cut (ADR-4631). The frame's camera cut calls
    /// `clear_lod_history_of` for the view it draws (ADR-8620).
    #[cfg(test)]
    pub(crate) fn clear_lod_history(&mut self) {
        self.lod_clear = true;
    }

    /// Advances every instance boundary and counts changed cuts, including off-screen ones. Mirrors
    /// `lod_prepare`, `lod_selected` and `lod_commit` so [`VisBufferStats::lod_switches`] matches
    /// the GPU word.
    #[tracing::instrument(skip_all, name = "visbuffer_lod_hysteresis")]
    fn settle_lod(&mut self, camera: &VisView) -> LodFrame {
        self.lod_updated = true;
        let count = self.instance_count();
        let boundary_count = (self.persistent_bits + self.total_bits) as usize;
        let mut clear = std::mem::take(&mut self.lod_clear)
            || self.lod_previous.len() != count
            || self.lod_history.len() != boundary_count
            || self.lod_sets_generation != self.sets_generation
            || self.lod_previous_keys != self.lod_keys;
        for index in 0..count {
            let (Some(inst), Some(prev)) = (self.instance_at(index), self.lod_previous.get(index)) else {
                clear = true;
                break;
            };
            let previous_model = if index < self.persistent.len() {
                inst.model
            } else {
                self.prev_models[index - self.persistent.len()]
            };
            if inst.info[..2] != prev.info[..2]
                || inst.info[3] != prev.info[3]
                || previous_model != prev.model
            {
                clear = true;
                break;
            }
        }
        if let Some(prev) = self.lod_eye {
            let jump = lod_distance(prev, camera.eye);
            if !jump.is_finite() || jump > crate::camera_cut::CameraCutDetection::default().max_eye_jump_m {
                clear = true;
            }
        }
        if clear {
            // A stored history thrown away (a new or emptied one is not a discard).
            if self.lod_eye.is_some() {
                self.lod_discards += 1;
            }
            self.lod_history.fill(0);
        }
        self.lod_history.resize(boundary_count, 0);
        let cut = LodCut {
            threshold: self.settings.lod_threshold_px,
            band: self.settings.resolved_lod_hysteresis_px(),
            pixels_per_unit: camera.height as f32 / (2.0 * (camera.fov_y * 0.5).tan()).max(1e-6),
            near: camera.near.max(1e-4),
            eye: camera.eye,
        };
        let mut switches = 0u32;
        for index in 0..count {
            let Some(inst) = self.instance_at(index) else {
                continue;
            };
            // One matrix per instance: rebuilding it per meshlet (twice: the lod and parent
            // spheres) was a measurable share of the mirror walk.
            let model = regia_core::glam::Mat4::from_cols_array_2d(&inst.model);
            let mut changed = false;
            for (local, meshlet) in self.arena.meshlets(inst.info[0], inst.info[1]).iter().enumerate() {
                let slot = &mut self.lod_history[inst.info[2] as usize + local];
                let chosen = lod_decision(&model, inst.scale[0], meshlet, *slot, &cut);
                changed |= *slot & 4 != 0 && (*slot ^ chosen) & 8 != 0;
                *slot = chosen;
            }
            switches += u32::from(changed);
        }
        self.lod_previous.clear();
        self.lod_previous.extend_from_slice(&self.persistent);
        self.lod_previous.extend_from_slice(&self.instances);
        self.lod_previous_keys.clone_from(&self.lod_keys);
        self.lod_sets_generation = self.sets_generation;
        self.lod_eye = Some(camera.eye);
        LodFrame {
            switches,
            band_bits: cut.band.to_bits(),
            clear: u32::from(clear),
            slots: self.lod_slots,
        }
    }

    /// Assigns the draw list's stable object identity to a transient instance (ADR-4631).
    /// Replacing or reordering objects invalidates the stored cut even at the same count.
    /// Anonymous draws use the mesh/material and previous-model continuity checks.
    pub(crate) fn set_lod_identity(&mut self, index: usize, object_key: u64) {
        if let Some(key) = self.lod_keys.get_mut(index) {
            *key = Some(object_key);
        }
    }

    fn instance_at(&self, index: usize) -> Option<GpuInstance> {
        if index < self.persistent.len() {
            self.persistent.get(index).copied()
        } else {
            self.instances.get(index - self.persistent.len()).copied()
        }
    }

    fn geometry_passes(
        &mut self,
        device: &wgpu::Device,
        queue: &TrackedQueue,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
        camera: &VisView,
    ) -> Result<bool, RenderError> {
        if !self.prepare_frame(device, queue)? {
            return Ok(false);
        }
        let d = &TrackedDevice::observe(device);
        self.ensure_targets(d, camera.width.max(1), camera.height.max(1));
        self.ensure_bind_groups(d);
        let software_raster = self.settings.software_raster && self.prepare_software(d, queue, camera)?;
        let Some(raster_pipe) = self.raster_pipeline(d, camera.double_sided, false) else {
            return Ok(false);
        };
        // Double-sided clusters: cull none, from the end of each phase's list.
        let raster_ds = if self.any_double_sided() {
            self.raster_pipeline(d, true, true)
        } else {
            None
        };
        // Masked clusters (ADR-3915): after the opaque ones in each list, alpha-tested when the
        // frame bound the table (`bind_masked`).
        let masked = self.masked_instance_count() > 0;
        let tested = self.masked_bind.is_some();
        let (masked_single, masked_double) = if masked {
            (
                self.masked_pipeline(d, camera.double_sided, false, tested)?,
                if self.any_double_sided() {
                    self.masked_pipeline(d, true, true, tested)?
                } else {
                    None
                },
            )
        } else {
            (None, None)
        };
        let header = self.masked_header();
        let cap = self.cluster_capacity();
        if self.pipes.is_none()
            || self.targets.is_none()
            || self.cull_bg.is_none()
            || self.raster_bgs.is_none()
            || self.buffers.state.is_none()
            || self.buffers.cull_params.is_none()
            || self.buffers.lod.is_none()
        {
            return Ok(false);
        }
        self.switch_bits_view(d, encoder);
        let lod_frame = self.settle_lod(camera);
        let (Some(pipes), Some(t)) = (self.pipes.as_ref(), self.targets.as_ref()) else {
            return Ok(false);
        };
        let b = &self.buffers;
        let (Some(state), Some(args), Some(cull_params), Some(raster_params), Some(resolve_params)) = (
            &b.state,
            &b.dispatch_args,
            &b.cull_params,
            &b.raster_params,
            &b.resolve_params,
        ) else {
            return Ok(false);
        };
        let (Some((_, cull_bg)), Some((_, raster_bgs))) = (&self.cull_bg, &self.raster_bgs) else {
            return Ok(false);
        };

        let moving = self.sway.moving();
        let instance_count = self.instance_count() as u32;
        let scan = scan_regions(
            self.persistent_chunks + self.total_chunks,
            u64::from(instance_count),
        );
        let mips = t.hiz_storage.len() as f32;
        let pixels_per_unit = camera.height as f32 / (2.0 * (camera.fov_y * 0.5).tan()).max(1e-6);
        let sway_frame = [
            camera.eye[0],
            camera.eye[1],
            camera.eye[2],
            sway_switch_scale(pixels_per_unit, &self.settings),
        ];
        let planes = crate::Frustum::from_view_proj(&regia_core::glam::Mat4::from_cols_array_2d(
            &camera.cull_view_proj,
        ))
        .planes();
        // The next frame's residency demand reads this view (ADR-3118); a field write, as the
        // buffers above are borrowed.
        self.residency.view = Some(super::residency::ResidencyView {
            eye: camera.eye,
            planes,
            pixels_per_unit,
            near: camera.near.max(1e-4),
            threshold_px: self.settings.lod_threshold_px,
        });
        let params = CullParams {
            view_proj: camera.view_proj,
            planes,
            eye: [
                camera.eye[0],
                camera.eye[1],
                camera.eye[2],
                self.settings.lod_threshold_px,
            ],
            screen: [
                camera.width as f32,
                camera.height as f32,
                if self.settings.occlusion { mips } else { 0.0 },
                pixels_per_unit,
            ],
            forward: [
                camera.forward[0],
                camera.forward[1],
                camera.forward[2],
                camera.near.max(1e-4),
            ],
            counts: [
                instance_count,
                cap,
                u32::from(!camera.double_sided),
                if software_raster {
                    self.settings
                        .software_raster_max_px
                        .min(software::MAX_SOFTWARE_PX)
                } else {
                    0
                },
            ],
            scan: [scan[0], scan[1], header, u32::from(moving)],
            lod: [lod_frame.band_bits, lod_frame.clear, lod_frame.slots, 0],
        };
        queue.write_buffer(cull_params, 0, bytemuck::bytes_of(&params));
        self.sway.write_frame(queue, sway_frame);
        let (stride_words, mip_bias) = self
            .masked_bind
            .as_ref()
            .map_or((0, 0.0), |bind| (bind.stride_words, bind.mip_bias));
        // The texture-index words follow the standard constants (`frame/gpu_types.rs`).
        let index_word = (crate::surface_graph::GRAPH_STANDARD_VEC4S * 4) as u32;
        for (phase, buf) in raster_params.iter().enumerate() {
            let raster = RasterParams {
                view_proj: camera.view_proj,
                info: [cap, phase as u32, stride_words, header],
                mask: [index_word, mip_bias.to_bits(), 0, 0],
            };
            queue.write_buffer(buf, 0, bytemuck::bytes_of(&raster));
        }
        let resolve = ResolveParams {
            view_proj: camera.view_proj,
            info: [cap, 0, 0, 0],
            screen: [camera.width as f32, camera.height as f32, 0.0, 0.0],
        };
        queue.write_buffer(resolve_params, 0, bytemuck::bytes_of(&resolve));
        if let Some(params) = &b.velocity_params {
            let velocity = VelocityParams {
                view_proj: camera.view_proj,
                unjittered: camera.cull_view_proj,
                prev_view_proj: camera.prev_view_proj,
                info: [cap, 0, 0, 0],
                screen: [camera.width as f32, camera.height as f32, 0.0, 0.0],
            };
            queue.write_buffer(params, 0, bytemuck::bytes_of(&velocity));
        }

        #[cfg(test)]
        self.stamp_cull(encoder, 0);
        let groups_2d = |groups: u32| (groups.min(65535), groups.div_ceil(65535).max(1));
        let (gx, gy) = groups_2d(instance_count.div_ceil(64));
        let (sx, sy) = groups_2d(instance_count.div_ceil(SCAN_BLOCK as u32));
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vis cull phase 1"),
                timestamp_writes: None,
            });
            pass.counted_set_bind_group(0, cull_bg, &[]);
            pass.counted_set_pipeline(&pipes.lod_prepare);
            let (lx, ly) = groups_2d(instance_count);
            pass.counted_dispatch(lx, ly, 1);
            pass.counted_set_pipeline(&pipes.reset);
            pass.counted_dispatch(1, 1, 1);
            // Work items in instance order: counts, prefix sums, placement (ADR-2294).
            pass.counted_set_pipeline(&pipes.cull_instances);
            pass.counted_dispatch(gx, gy, 1);
            pass.counted_set_pipeline(&pipes.scan_local);
            pass.counted_dispatch(sx, sy, 1);
            pass.counted_set_pipeline(&pipes.scan_top[0]);
            pass.counted_dispatch(1, 1, 1);
            pass.counted_set_pipeline(&pipes.place_instances);
            pass.counted_dispatch(gx, gy, 1);
        }
        encoder.copy_buffer_to_buffer(state, 32, args, 0, 12);
        encoder.copy_buffer_to_buffer(state, SCAN_ARGS_WORD * 4, args, SCAN_ARGS_OFFSET, 12);
        record_cull_phase(encoder, pipes, cull_bg, args, 0);
        #[cfg(test)]
        self.stamp_cull(encoder, 1);
        let ds_args = |phase: u64| {
            raster_ds
                .as_ref()
                .map(|pipe| (pipe, (DS_ARGS_WORD + phase * 4) * 4))
        };
        let masked_draws = |phase: u64| MaskedDraws {
            single: masked_single
                .as_ref()
                .map(|pipe| (pipe, (MASK_ARGS_WORD + phase * 8) * 4)),
            double: masked_double
                .as_ref()
                .map(|pipe| (pipe, (MASK_ARGS_WORD + phase * 8 + 4) * 4)),
            groups: self.masked_bind.as_ref().map(|bind| (&bind.group, &bind.table)),
        };
        raster_phase(
            encoder,
            (&raster_pipe, 0),
            ds_args(0),
            &masked_draws(0),
            &raster_bgs[0],
            state,
            &t.ids_view,
            depth,
            pipes.id_fill.as_ref().map_or(IdsLoad::Clear, IdsLoad::Fill),
        );
        let software_round = software::SoftwareRound {
            state,
            args,
            ids: &t.ids_view,
            depth,
        };
        if software_raster {
            self.software.record_clear(encoder);
            self.software.record_round(encoder, &software_round);
        }
        if self.settings.occlusion {
            let _span = tracing::info_span!("visbuffer_hiz").entered();
            let _crumb = crate::breadcrumbs::frame_pass("visbuffer_hiz");
            if self.hiz_bgs.copy.as_ref().is_none_or(|(v, _)| v != depth) {
                let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("vis hiz copy"),
                    layout: &pipes.hiz_copy_bgl,
                    entries: &[view(0, depth), view(2, &t.hiz_storage[0])],
                });
                self.hiz_bgs.copy = Some((depth.clone(), bg));
            }
            if self.hiz_bgs.down.is_empty() {
                self.hiz_bgs.down = (1..t.hiz_storage.len())
                    .map(|mip| {
                        d.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("vis hiz down"),
                            layout: &pipes.hiz_down_bgl,
                            entries: &[view(1, &t.hiz_sampled[mip - 1]), view(2, &t.hiz_storage[mip])],
                        })
                    })
                    .collect();
            }
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vis hiz"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&pipes.hiz_copy);
            if let Some((_, copy_bg)) = self.hiz_bgs.copy.as_ref() {
                pass.counted_set_bind_group(0, copy_bg, &[]);
            }
            pass.counted_dispatch(t.hiz_size.0.div_ceil(8), t.hiz_size.1.div_ceil(8), 1);
            pass.counted_set_pipeline(&pipes.hiz_down);
            for (mip, bg) in (1..t.hiz_storage.len()).zip(&self.hiz_bgs.down) {
                pass.counted_set_bind_group(0, bg, &[]);
                let w = (t.hiz_size.0 >> mip).max(1);
                let h = (t.hiz_size.1 >> mip).max(1);
                pass.counted_dispatch(w.div_ceil(8), h.div_ceil(8), 1);
            }
        }
        #[cfg(test)]
        self.stamp_cull(encoder, 2);
        record_cull_phase(encoder, pipes, cull_bg, args, 1);
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("vis lod commit"),
                timestamp_writes: None,
            });
            pass.counted_set_bind_group(0, cull_bg, &[]);
            pass.counted_set_pipeline(&pipes.lod_commit);
            pass.counted_dispatch(gx, gy, 1);
        }
        #[cfg(test)]
        self.stamp_cull(encoder, 3);
        raster_phase(
            encoder,
            (&raster_pipe, 16),
            ds_args(1),
            &masked_draws(1),
            &raster_bgs[1],
            state,
            &t.ids_view,
            depth,
            IdsLoad::Keep,
        );
        if software_raster {
            self.software.record_round(encoder, &software_round);
        }

        let (sway_instances, sway_materials, sway_forward) = self.sway.report();
        let (view_histories, view_history_bytes) = self.view_history_report();
        self.stats = VisBufferStats {
            frames: self.stats.frames + 1,
            instances: instance_count,
            candidate_clusters: self.candidates + self.persistent_bits,
            materials: self.frame_slots.len() as u32,
            resident_meshes: self.meshes.len() as u32,
            geometry_bytes: self.arena.used_bytes(),
            declined_meshes: self.declined.len() as u32,
            receiver_instances: self.receivers.instances(),
            receiver_materials: self.receivers.materials(),
            masked_instances: self.masked_instance_count() as u32,
            masked_materials: self.masked_material_count(),
            masked_tested: masked && tested,
            sway_instances,
            sway_materials,
            sway_moving: moving,
            sway_forward,
            lod_switches: lod_frame.switches,
            lod_history_discards: self.lod_discards,
            lod_history_copies: self.lod_view_copies,
            view_histories,
            view_history_bytes,
        };
        Ok(true)
    }

    /// Reads the last frame's cull counters back. Blocks on the GPU: a diagnostic for tests and
    /// benches, never called by the frame path. `None` before the first visibility frame.
    ///
    /// # Errors
    ///
    /// The poll or the map failed.
    pub(crate) fn read_counters(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<super::VisBufferCounters>, RenderError> {
        let Some(state) = self.buffers.state.as_ref() else {
            return Ok(None);
        };
        let words = read_words(device, state, STATE_WORDS)?;
        let word = |i: usize| words.get(i).copied().unwrap_or(0);
        let mask = MASK_ARGS_WORD as usize;
        let masked = [word(mask + 1) + word(mask + 5), word(mask + 9) + word(mask + 13)];
        Ok(Some(super::VisBufferCounters {
            phase1_clusters: word(1) + word(DS_ARGS_WORD as usize + 1) + masked[0],
            phase2_clusters: word(5) + word(DS_ARGS_WORD as usize + 5) + masked[1],
            work_items: word(11),
            dropped_clusters: word(12),
            software_clusters: word(software::SOFT_COUNT_WORD as usize),
            masked_clusters: masked[0] + masked[1],
        }))
    }
}

/// One cull phase over the work items (ADR-2294): each item's lanes record their classes and
/// counts, the counts are prefix-summed (the second level also sets what each list keeps), and
/// every selected cluster is written at its scanned slot.
fn record_cull_phase(
    encoder: &mut wgpu::CommandEncoder,
    pipes: &Pipes,
    cull_bg: &wgpu::BindGroup,
    args: &wgpu::Buffer,
    phase: usize,
) {
    let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
        label: Some(if phase == 0 {
            "vis cull clusters 1"
        } else {
            "vis cull phase 2"
        }),
        timestamp_writes: None,
    });
    pass.counted_set_bind_group(0, cull_bg, &[]);
    pass.counted_set_pipeline(if phase == 0 { &pipes.phase1 } else { &pipes.phase2 });
    pass.counted_dispatch_indirect(args, 0);
    pass.counted_set_pipeline(&pipes.scan_local);
    pass.counted_dispatch_indirect(args, SCAN_ARGS_OFFSET);
    pass.counted_set_pipeline(&pipes.scan_top[phase + 1]);
    pass.counted_dispatch(1, 1, 1);
    pass.counted_set_pipeline(&pipes.place[phase]);
    pass.counted_dispatch_indirect(args, 0);
}

/// The last frame's cull output (test hook, [`VisBufferPass::read_cull_lists`]).
#[cfg(test)]
pub(crate) struct CullLists {
    /// Every `state` word (`vis_cull.wgsl` lists the layout).
    pub(crate) state: Vec<u32>,
    /// Clusters per phase list.
    pub(crate) capacity: u32,
    /// The whole cluster buffer, two words per entry (instance, meshlet).
    pub(crate) clusters: Vec<u32>,
}

#[cfg(test)]
impl VisBufferPass {
    /// Test hook: reads the cull's state and cluster lists back. Blocks on the GPU. `None`
    /// before the first visibility frame.
    pub(crate) fn read_cull_lists(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<CullLists>, RenderError> {
        let (Some(state), Some((clusters, size))) =
            (self.buffers.state.as_ref(), self.buffers.clusters.as_ref())
        else {
            return Ok(None);
        };
        Ok(Some(CullLists {
            state: read_words(device, state, STATE_WORDS)?,
            capacity: self.cluster_capacity(),
            clusters: read_words(device, clusters, size / 4)?,
        }))
    }

    /// The GPU switch count of the last visibility frame (one word). Blocks on the GPU.
    /// `None` before the first visibility frame.
    pub(crate) fn read_lod_switches(&self, device: &crate::RenderDevice) -> Result<Option<u32>, RenderError> {
        let Some((buffer, _)) = self.buffers.lod.as_ref() else {
            return Ok(None);
        };
        if self.lod_slots == 0 {
            return Ok(None);
        }
        let offset = u64::from(self.lod_slots) * 4;
        read_word_at(device, buffer, offset).map(Some)
    }
}

/// Test hook: four GPU stamps per frame, before the instance cull, after phase 1's cull,
/// before and after phase 2's (the raster and the Hi-Z build sit between 1 and 2).
#[cfg(test)]
pub(crate) struct CullTimer {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
}

#[cfg(test)]
impl VisBufferPass {
    /// Test hook: turns the cull stamps on. False when the device cannot stamp in an encoder.
    pub(crate) fn enable_cull_timer(&mut self, device: &crate::RenderDevice) -> bool {
        let needed = wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS;
        if !device.device.features().contains(needed) {
            return false;
        }
        self.cull_timer = Some(CullTimer {
            set: device.device.create_query_set(&wgpu::QuerySetDescriptor {
                label: Some("vis cull timer"),
                ty: wgpu::QueryType::Timestamp,
                count: 4,
            }),
            resolve: device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("vis cull timer"),
                size: 32,
                usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            }),
        });
        true
    }

    fn stamp_cull(&self, encoder: &mut wgpu::CommandEncoder, index: u32) {
        if let Some(timer) = &self.cull_timer {
            encoder.write_timestamp(&timer.set, index);
            if index == 3 {
                encoder.resolve_query_set(&timer.set, 0..4, &timer.resolve, 0);
            }
        }
    }

    /// Test hook: the last frame's cull time in ms, phase 1 (with the instance cull) and
    /// phase 2. Blocks on the GPU.
    pub(crate) fn read_cull_ms(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<(f64, f64)>, RenderError> {
        let Some(timer) = &self.cull_timer else {
            return Ok(None);
        };
        let words = read_words(device, &timer.resolve, 8)?;
        let stamp = |i: usize| u64::from(words[2 * i]) | (u64::from(words[2 * i + 1]) << 32);
        let period = f64::from(device.queue.get_timestamp_period()) / 1_000_000.0;
        let ms = |a: usize, b: usize| stamp(b).saturating_sub(stamp(a)) as f64 * period;
        Ok(Some((ms(0, 1), ms(2, 3))))
    }
}

fn lod_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
}

/// `vis_cull.wgsl` `world_sphere`: the LOD test uses the rest sphere, not the sway-grown one.
fn lod_world_sphere(model: &regia_core::glam::Mat4, scale: f32, sphere: [f32; 4]) -> [f32; 4] {
    let center = model.transform_point3(regia_core::glam::Vec3::new(sphere[0], sphere[1], sphere[2]));
    [center.x, center.y, center.z, sphere[3] * scale]
}

/// `vis_cull.wgsl` `projected_error`.
fn lod_projected(sphere: [f32; 4], error: f32, cut: &LodCut) -> f32 {
    let d = lod_distance([sphere[0], sphere[1], sphere[2]], cut.eye) - sphere[3];
    error * cut.pixels_per_unit / d.max(cut.near)
}

/// A boundary holds its previous comparison inside the band. Equality uses the hard cut
/// on a first frame; a root's parent is always above the threshold.
fn lod_above(error: f32, previous: bool, valid: bool, cut: &LodCut) -> bool {
    if valid && cut.band > 0.0 && (error - cut.threshold).abs() <= cut.band {
        previous
    } else {
        error > cut.threshold
    }
}

/// `vis_cull.wgsl` `lod_decision`: both boundary comparisons and the resulting cut membership.
/// `model` is the instance's matrix (`lod_world_sphere` reads it twice) and `scale` its
/// uniform scale; both are per instance, hoisted out of the per-meshlet walk.
fn lod_decision(
    model: &regia_core::glam::Mat4,
    scale: f32,
    meshlet: &Meshlet,
    prev: u32,
    cut: &LodCut,
) -> u32 {
    let valid = prev & 4 != 0;
    let own = lod_above(
        lod_projected(
            lod_world_sphere(model, scale, meshlet.lod),
            meshlet.lod_error * scale,
            cut,
        ),
        prev & 1 != 0,
        valid,
        cut,
    );
    let parent = meshlet.parent_error >= LOD_CUT_ROOT
        || lod_above(
            lod_projected(
                lod_world_sphere(model, scale, meshlet.parent),
                meshlet.parent_error * scale,
                cut,
            ),
            prev & 2 != 0,
            valid,
            cut,
        );
    4 | u32::from(own) | (u32::from(parent) << 1) | (u32::from(!own && parent) << 3)
}

/// One word of `buffer` at `offset` bytes. Blocks on the GPU (tests).
#[cfg(test)]
fn read_word_at(
    device: &crate::RenderDevice,
    buffer: &wgpu::Buffer,
    offset: u64,
) -> Result<u32, RenderError> {
    let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vis lod readback"),
        size: 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vis lod readback"),
        });
    encoder.copy_buffer_to_buffer(buffer, offset, &staging, 0, 4);
    device.tracked_queue().submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|err| RenderError::Readback(err.to_string()))?
        .map_err(|err| RenderError::Readback(err.to_string()))?;
    let mapped = staging
        .slice(..)
        .get_mapped_range()
        .map_err(|err| RenderError::Readback(err.to_string()))?;
    let word = mapped
        .get(..4)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0);
    drop(mapped);
    staging.unmap();
    Ok(word)
}

/// Reads the first `words` words of `buffer` back. Blocks on the GPU (tests and benches).
fn read_words(
    device: &crate::RenderDevice,
    buffer: &wgpu::Buffer,
    words: u64,
) -> Result<Vec<u32>, RenderError> {
    let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("vis counters readback"),
        size: words * 4,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("vis counters readback"),
        });
    encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, words * 4);
    device.tracked_queue().submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
        let _ = tx.send(result);
    });
    device
        .tracked()
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(RenderError::Poll)?;
    rx.recv()
        .map_err(|err| RenderError::Readback(err.to_string()))?
        .map_err(|err| RenderError::Readback(err.to_string()))?;
    let mapped = staging
        .slice(..)
        .get_mapped_range()
        .map_err(|err| RenderError::Readback(err.to_string()))?;
    let out: Vec<u32> = mapped
        .as_chunks::<4>()
        .0
        .iter()
        .map(|c| u32::from_le_bytes(*c))
        .collect();
    drop(mapped);
    staging.unmap();
    Ok(out)
}

fn storage_texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format: wgpu::TextureFormat::R32Float,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
        count: None,
    }
}

/// One phase's masked draws (ADR-3915): each list's pipeline and `state` byte offset, and the
/// alpha test's groups 1 and 4 when the frame bound them.
struct MaskedDraws<'a> {
    single: Option<(&'a wgpu::RenderPipeline, u64)>,
    double: Option<(&'a wgpu::RenderPipeline, u64)>,
    groups: Option<(&'a wgpu::BindGroup, &'a wgpu::BindGroup)>,
}

/// How a raster phase starts its visibility IDs.
#[derive(Clone, Copy)]
enum IdsLoad<'a> {
    /// Cleared to the empty ID (`u32::MAX`) by the pass's clear.
    Clear,
    /// Cleared to zero, then filled with the empty ID by this full-screen draw (ADR-5807).
    Fill(&'a wgpu::RenderPipeline),
    /// Phase 2: kept from phase 1.
    Keep,
}

/// The full-screen draw that writes the empty ID (`vis_raster.wgsl` `vs_vis_fill`), for backends
/// whose clear cannot hold `u32::MAX` in an integer target (ADR-5807). Depth test always, no depth
/// write: the phase's depth stays the scene's.
fn id_fill_pipeline(d: &TrackedDevice, module: &wgpu::ShaderModule) -> wgpu::RenderPipeline {
    let layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("vis id fill"),
        bind_group_layouts: &[],
        immediate_size: 0,
    });
    d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some("vis id fill"),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_vis_fill"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_vis_fill"),
            targets: &[Some(wgpu::ColorTargetState {
                format: VIS_FORMAT,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(false),
            depth_compare: Some(wgpu::CompareFunction::Always),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&d).as_deref(),
    })
}

/// One phase's visibility draws: `culled` (pipeline, `state` byte offset of its DrawIndirect),
/// then the double-sided clusters' draw when `double_sided` is given, then the masked ones.
#[allow(clippy::too_many_arguments)]
fn raster_phase(
    encoder: &mut wgpu::CommandEncoder,
    culled: (&wgpu::RenderPipeline, u64),
    double_sided: Option<(&wgpu::RenderPipeline, u64)>,
    masked: &MaskedDraws<'_>,
    bind_group: &wgpu::BindGroup,
    state: &wgpu::Buffer,
    ids: &wgpu::TextureView,
    depth: &wgpu::TextureView,
    clear: IdsLoad<'_>,
) {
    let fill = match clear {
        IdsLoad::Fill(pipe) => Some(pipe),
        IdsLoad::Clear | IdsLoad::Keep => None,
    };
    let clear = !matches!(clear, IdsLoad::Keep);
    let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
        label: Some(if clear {
            "vis raster phase 1"
        } else {
            "vis raster phase 2"
        }),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: ids,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: match fill {
                    // Zero clears exactly everywhere; the fill below writes the empty ID.
                    Some(_) => wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    None if clear => wgpu::LoadOp::Clear(wgpu::Color {
                        r: f64::from(u32::MAX),
                        g: 0.0,
                        b: 0.0,
                        a: 0.0,
                    }),
                    None => wgpu::LoadOp::Load,
                },
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
            view: depth,
            depth_ops: Some(wgpu::Operations {
                load: wgpu::LoadOp::Load,
                store: wgpu::StoreOp::Store,
            }),
            stencil_ops: None,
        }),
        timestamp_writes: None,
        ..Default::default()
    });
    if let Some(fill) = fill {
        pass.counted_set_pipeline(fill);
        pass.counted_draw(0..3, 0..1);
    }
    pass.counted_set_bind_group(0, bind_group, &[]);
    pass.counted_set_pipeline(culled.0);
    pass.counted_draw_indirect(state, culled.1);
    if let Some((pipe, offset)) = double_sided {
        pass.counted_set_pipeline(pipe);
        pass.counted_draw_indirect(state, offset);
    }
    if masked.single.is_none() && masked.double.is_none() {
        return;
    }
    if let Some((group, table)) = masked.groups {
        pass.set_bind_group(1, group, &[]);
        pass.set_bind_group(crate::texture_table::TEXTURE_TABLE_GROUP, table, &[]);
    }
    for (pipe, offset) in [masked.single, masked.double].into_iter().flatten() {
        pass.set_pipeline(pipe);
        pass.draw_indirect(state, offset);
    }
}

/// Source of the alpha-tested raster: the raster module with the masked fragment appended. The
/// table's `binding_array` needs the `wgpu_binding_array` extension, whose directive must open the
/// module (`lab_forward.wgsl` says why).
pub(crate) fn masked_raster_source() -> &'static str {
    static SOURCE: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        format!(
            "enable wgpu_binding_array;\n{}\n{}",
            include_str!("vis_raster.wgsl"),
            include_str!("vis_raster_masked.wgsl")
        )
    });
    SOURCE.as_str()
}

/// The defs of the alpha-tested raster for a table of `capacity` views.
pub(crate) fn masked_defs(capacity: u32) -> HashMap<String, naga_oil::compose::ShaderDefValue> {
    let mut defs = HashMap::new();
    defs.insert(
        crate::texture_table::TEXTURE_TABLE_SIZE_DEF.to_string(),
        naga_oil::compose::ShaderDefValue::UInt(capacity.max(1)),
    );
    defs
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "vis_cull.wgsl,vis_raster.wgsl,vis_raster_masked.wgsl,vis_shadow.wgsl,vis_shadow_raster.wgsl,vis_software.wgsl,vis_velocity.wgsl",
            "Instance",
            GpuInstance {
                model,
                sphere,
                info,
                scale,
                shadow,
                flags
            }
        ),
        crate::contract_row!(
            "vis_cull.wgsl",
            "CullParams",
            CullParams {
                view_proj,
                planes,
                eye,
                screen,
                forward,
                counts,
                scan,
                lod
            }
        ),
        crate::contract_row!(
            "vis_raster.wgsl,vis_raster_masked.wgsl",
            "RasterParams",
            RasterParams {
                view_proj,
                info,
                mask
            }
        ),
        crate::contract_row!(
            "vis_vertex_repack.wgsl",
            "RepackParams",
            counted_vertices::RepackParams {
                count,
                dst_word,
                row_groups,
                source_words
            }
        ),
        crate::contract_row!(
            "vis_velocity.wgsl",
            "VelocityParams",
            VelocityParams {
                view_proj,
                unjittered,
                prev_view_proj,
                info,
                screen
            }
        ),
        crate::contract_row!(
            "vis_resolve.wgsl",
            "VisResolveParams",
            ResolveParams {
                view_proj,
                info,
                screen
            }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::visbuffer::MAX_MESHLET_TRIANGLES;

    /// A negative or zero shadow scale draws the shadow as drawn, as the forward path's
    /// `shadow_model` does (corr-low-render, corr-render-visbuffer finding K).
    #[test]
    fn a_shadow_scale_that_is_not_positive_casts_as_drawn() {
        for shadow_scale in [-2.0, 0.0, f32::NAN] {
            let cast = ShadowCast::Local {
                reach: 1.0,
                max_cascades: 2,
                shadow_scale,
                shadow_lift: 0.0,
                anchor: None,
                lod_switch_keeps_pages: false,
            };
            let (fields, _) = shadow_fields(cast, true);
            assert_eq!(fields[0], 1.0, "scale {shadow_scale}");
        }
    }

    const IDENTITY: [[f32; 4]; 4] = [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ];

    /// CPU-only fixture with an analytic root boundary: projected error is exactly controlled.
    fn lod_fixture() -> VisBufferPass {
        let (vertices, indices, mesh) = triangle();
        let mut mesh = (*mesh).clone();
        mesh.meshlets[0].lod = [0.0; 4];
        mesh.meshlets[0].lod_error = 0.05;
        let mut pass = VisBufferPass::default();
        pass.settings.lod_hysteresis_px = Some(0.25);
        pass.insert_cooked(1, Arc::new(mesh));
        assert!(pass.ensure_mesh(1, &vertices, &indices));
        pass
    }

    fn lod_view(projected: f32) -> VisView {
        let fov_y: f32 = 1.0;
        let ppu = 96.0 / (2.0 * (fov_y * 0.5).tan());
        VisView {
            view_proj: IDENTITY,
            cull_view_proj: IDENTITY,
            prev_view_proj: IDENTITY,
            eye: [0.0, 0.0, 0.05 * ppu / projected],
            forward: [0.0, 0.0, -1.0],
            near: 0.1,
            fov_y,
            width: 96,
            height: 96,
            double_sided: true,
        }
    }

    fn lod_objects(pass: &mut VisBufferPass, keys: &[u64]) {
        pass.begin_frame();
        for &key in keys {
            let index = pass
                .push_instance(1, 10, IDENTITY, IDENTITY, ShadowCast::Off, true)
                .expect("resident fixture");
            pass.set_lod_identity(index, key);
        }
    }

    #[test]
    fn lod_hysteresis_same_count_replacement_and_reorder_use_the_hard_cut() {
        for replacement in [&[3, 2][..], &[2, 1][..]] {
            let mut pass = lod_fixture();
            lod_objects(&mut pass, &[1, 2]);
            assert_eq!(pass.settle_lod(&lod_view(0.9)).switches, 0);
            lod_objects(&mut pass, &[1, 2]);
            assert_eq!(pass.settle_lod(&lod_view(1.1)).clear, 0);
            assert!(pass.lod_history.iter().all(|word| word & 8 != 0), "old cut held");
            lod_objects(&mut pass, replacement);
            let frame = pass.settle_lod(&lod_view(1.1));
            assert_eq!(frame.clear, 1);
            assert_eq!(frame.switches, 0, "new history is not a switch");
            assert!(
                pass.lod_history.iter().all(|word| word & 8 == 0),
                "replacement uses hard cut"
            );
        }
    }

    #[test]
    fn lod_hysteresis_anonymous_draws_keep_history_when_geometry_and_pose_match() {
        let mut pass = lod_fixture();
        lod_objects(&mut pass, &[0]);
        assert_eq!(pass.settle_lod(&lod_view(0.9)).clear, 1);
        assert_ne!(pass.lod_history[0] & 8, 0);
        lod_objects(&mut pass, &[0]);
        let held = pass.settle_lod(&lod_view(1.1));
        assert_eq!(held.clear, 0);
        assert_eq!(held.switches, 0);
        assert_ne!(
            pass.lod_history[0] & 8,
            0,
            "anonymous camera motion retains the cut"
        );
        // Without an object key, an unmatched pose is missing history rather than continuity.
        lod_objects(&mut pass, &[0]);
        pass.instances[0].model[3][0] = 0.2;
        pass.prev_models[0] = pass.instances[0].model;
        let replaced = pass.settle_lod(&lod_view(1.1));
        assert_eq!(replaced.clear, 1);
        assert_eq!(replaced.switches, 0);
        assert_eq!(pass.lod_history[0] & 8, 0);
    }

    #[test]
    fn lod_hysteresis_a_missing_visibility_frame_discards_history() {
        let mut pass = lod_fixture();
        lod_objects(&mut pass, &[1]);
        pass.settle_lod(&lod_view(0.4));
        assert_ne!(pass.lod_history[0] & 8, 0);
        pass.begin_frame(); // No visibility geometry was recorded in this main-view frame.
        lod_objects(&mut pass, &[1]);
        let frame = pass.settle_lod(&lod_view(1.1));
        assert_eq!(frame.clear, 1);
        assert_eq!(frame.switches, 0);
        assert_eq!(pass.lod_history[0] & 8, 0);
    }

    #[test]
    fn lod_hysteresis_instance_capacity_growth_invalidates_the_shifted_history_base() {
        let device = crate::test_device();
        let mut pass = lod_fixture();
        pass.reserve = warm::ListReserve {
            instances: 1,
            clusters: 128,
            ..Default::default()
        };
        lod_objects(&mut pass, &[1]);
        pass.ensure_buffers(&device.tracked(), &device.tracked_queue());
        pass.settle_lod(&lod_view(0.9));
        assert!(!pass.lod_clear);
        let old_slots = pass.lod_slots;
        let old_history_capacity = pass.buffers.lod.as_ref().expect("history buffer").1;
        pass.reserve.instances = 2;
        pass.ensure_buffers(&device.tracked(), &device.tracked_queue());
        assert!(pass.lod_slots > old_slots, "instance header grew");
        assert_eq!(
            pass.buffers.lod.as_ref().expect("history buffer").1,
            old_history_capacity,
            "the history buffer itself did not grow"
        );
        assert!(
            pass.lod_clear,
            "a shifted boundary base cannot reuse old history words"
        );
    }

    #[test]
    fn lod_hysteresis_advances_boundaries_outside_the_candidate_range() {
        let mut pass = lod_fixture();
        let mut switches = Vec::new();
        for projected in [0.4, 2.0, 0.9] {
            lod_objects(&mut pass, &[1]);
            switches.push(pass.settle_lod(&lod_view(projected)).switches);
        }
        assert_eq!(switches, [0, 1, 0]);
        assert_eq!(
            pass.lod_history[0] & 8,
            0,
            "returning inside the band holds the fine cut"
        );
    }

    #[test]
    fn shader_cluster_vertex_count_matches_the_format() {
        // One cluster draw instance spans every triangle slot (`124 × 3` vertices).
        let line = format!(
            "const VERTICES_PER_CLUSTER: u32 = {}u;",
            MAX_MESHLET_TRIANGLES * 3
        );
        assert!(include_str!("vis_cull.wgsl").contains(&line));
    }

    /// One triangle: 3 vertices × 16 words + 3 cluster vertices + 1 triangle = 52 geometry words.
    fn triangle() -> (Vec<LabVertex>, Vec<u32>, Arc<MeshletMesh>) {
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let vertices: Vec<LabVertex> = positions
            .iter()
            .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]))
            .collect();
        let indices = vec![0, 1, 2];
        let meshlets = Arc::new(MeshletMesh::single_level(&positions, &indices));
        (vertices, indices, meshlets)
    }

    /// A missing alpha-test binding must leave the depth that phase one's Hi-Z reads untouched.
    #[test]
    fn unbound_masked_clusters_write_no_occluding_depth() {
        use regia_core::glam::Vec3;
        use regia_core::glam::camera::rh::{proj::directx::perspective, view::look_at_mat4};
        // The shared device: a fresh one late in the lib run failed to open on NVIDIA, where every
        // device a unit test opens is kept until exit (ADR-1584, ADR-5554).
        let device = crate::test_device();
        let size = 64;
        let extent = wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        };
        let view_proj = (perspective(1.0, 1.0, 0.1, 10.0)
            * look_at_mat4(Vec3::new(0.0, 0.0, 2.0), Vec3::ZERO, Vec3::Y))
        .to_cols_array_2d();
        let camera = VisView {
            view_proj,
            cull_view_proj: view_proj,
            prev_view_proj: view_proj,
            eye: [0.0, 0.0, 2.0],
            forward: [0.0, 0.0, -1.0],
            near: 0.1,
            fov_y: 1.0,
            width: size,
            height: size,
            double_sided: true,
        };
        for masked in [false, true] {
            let (vertices, indices, meshlets) = triangle();
            let mut pass = VisBufferPass::default();
            pass.insert_cooked(1, meshlets);
            assert!(pass.ensure_mesh(1, &vertices, &indices));
            pass.begin_frame();
            let instance = if masked {
                pass.push_masked_instance(1, 1, IDENTITY, IDENTITY, true)
            } else {
                pass.push_instance(1, 1, IDENTITY, IDENTITY, ShadowCast::Off, true)
            };
            assert!(instance.is_some());
            let depth = device.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("unbound masked depth"),
                size: extent,
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Depth32Float,
                usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                    | wgpu::TextureUsages::TEXTURE_BINDING
                    | wgpu::TextureUsages::COPY_SRC,
                view_formats: &[],
            });
            let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
            let readback = device.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("unbound masked depth readback"),
                size: u64::from(size * size * 4),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
                mapped_at_creation: false,
            });
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
            drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear unbound masked depth"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                ..Default::default()
            }));
            assert!(
                pass.record_geometry(
                    &device.device,
                    &device.tracked_queue(),
                    &mut encoder,
                    &depth_view,
                    &camera
                )
                .expect("geometry")
            );
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: &depth,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::DepthOnly,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(size * 4),
                        rows_per_image: Some(size),
                    },
                },
                extent,
            );
            device.tracked_queue().submit([encoder.finish()]);
            let depths = read_words(&device, &readback, u64::from(size * size)).expect("depth readback");
            let covered = depths.iter().filter(|&&word| word != 1.0f32.to_bits()).count();
            let counters = pass.read_counters(&device).expect("counters").expect("recorded");
            assert!(
                counters.phase1_clusters + counters.phase2_clusters > 0,
                "the triangle reaches a raster list"
            );
            if masked {
                assert!(counters.masked_clusters > 0);
                assert!(!pass.stats.masked_tested);
                assert_eq!(
                    covered, 0,
                    "untested masked fragments must not occlude through holes"
                );
            } else {
                assert!(covered > 0, "positive control: the opaque triangle writes depth");
            }
        }
    }

    #[test]
    fn the_masked_shader_constants_match() {
        let cull = include_str!("vis_cull.wgsl");
        assert!(cull.contains(&format!("const MASK_ARGS: u32 = {MASK_ARGS_WORD}u;")));
        assert!(cull.contains(&format!("const MASKED: u32 = {VIS_MASKED}u;")));
        assert!(cull.contains(&format!("const DOUBLE_SIDED: u32 = {VIS_DOUBLE_SIDED}u;")));
        // The masked raster reads the arena record's alpha words of `frame/gpu_types.rs`
        // `GpuMaterial`: base color (vec4 0), alpha block (vec4 3), detail (vec4 4).
        let masked = include_str!("vis_raster_masked.wgsl");
        for line in [
            "const WORD_BASE_ALPHA: u32 = 3u;",
            "const WORD_ALPHA_CUTOFF: u32 = 13u;",
            "const WORD_ALPHA_FLAGS: u32 = 15u;",
            "const WORD_MIN_LOD: u32 = 19u;",
        ] {
            assert!(masked.contains(line), "{line}");
        }
    }

    /// The stretch bound is exact on orthogonal columns and never below what any direction gets.
    #[test]
    fn the_stretch_bound_holds_for_shear_and_stays_exact_without_it() {
        use regia_core::glam::{Mat4, Quat, Vec3};
        let rigid = Mat4::from_rotation_translation(
            Quat::from_euler(regia_core::glam::EulerRot::XYZ, 0.3, 1.1, -0.7),
            Vec3::new(5.0, -2.0, 9.0),
        ) * Mat4::from_scale(Vec3::new(2.5, 1.0, 0.5));
        let longest = rigid
            .x_axis
            .truncate()
            .length()
            .max(rigid.y_axis.truncate().length());
        let longest = longest.max(rigid.z_axis.truncate().length());
        let exact = max_stretch(&rigid.to_cols_array_2d());
        assert!(
            (exact - longest).abs() <= longest * 1e-6,
            "orthogonal columns keep the largest column length: {exact} against {longest}"
        );
        // A non-uniform parent over a rotated child: the columns are 2.92 long, the stretch is 4.
        let sheared =
            Mat4::from_scale(Vec3::new(4.0, 1.0, 1.0)) * Mat4::from_rotation_z(std::f32::consts::FRAC_PI_4);
        let bound = max_stretch(&sheared.to_cols_array_2d());
        assert!((bound - 4.0).abs() < 1e-3, "bound {bound}");
        for i in 0..4096 {
            let t = i as f32 * 0.37;
            let v = Vec3::new(
                t.sin() * (t * 0.13).cos(),
                (t * 0.13).sin(),
                t.cos() * (t * 0.13).cos(),
            );
            let stretched = sheared.transform_vector3(v.normalize()).length();
            assert!(
                stretched <= bound * 1.0001,
                "{v} stretches {stretched} past {bound}"
            );
        }
    }

    /// A stored masked set: with the table it is masked and casts no cluster shadow, whatever its
    /// policy says (the shadow raster has no alpha test); without it, it draws nothing (no solid
    /// cards, and no forward path to stay on). An opaque set keeps its shadows.
    #[test]
    fn a_masked_set_needs_the_table_and_casts_no_cluster_shadow() {
        let (vertices, indices, meshlets) = triangle();
        let leaf = LabMaterial {
            alpha_mode: crate::lab_frame::ALPHA_MASK,
            ..LabMaterial::default()
        };
        let windy_leaf = crate::FoliageSway::default().adopt(&leaf).expect("adopt");
        let solid = LabMaterial::default();
        let models = [IDENTITY];
        let shadows = [ShadowCast::All];
        let mut pass = VisBufferPass::default();
        pass.insert_cooked(1, meshlets);
        for (key, material_key, material, instance_shadows) in [
            (1, 10, &leaf, &shadows[..]),
            (2, 11, &leaf, &[][..]),
            (3, 12, &solid, &[][..]),
            (4, 13, &windy_leaf, &shadows[..]),
            (5, 14, &windy_leaf, &[][..]),
        ] {
            assert!(pass.set_instance_set(
                key,
                &VisInstanceSet {
                    mesh_key: 1,
                    vertices: &vertices,
                    indices: &indices,
                    material_key,
                    material,
                    models: &models,
                    shadow: ShadowCast::All,
                    instance_shadows,
                },
            ));
        }
        pass.begin_frame();
        assert_eq!(
            pass.persistent.len(),
            1,
            "only the opaque set draws without the table"
        );
        assert_ne!(
            pass.persistent[0].flags[0] & SHADOW_CASTS,
            0,
            "the opaque set casts"
        );
        assert_eq!(pass.persistent[0].flags[1] & VIS_MASKED, 0);
        pass.masked_device = true;
        pass.persistent_dirty = true;
        pass.begin_frame();
        assert_eq!(pass.persistent.len(), 5, "the masked sets draw with the table");
        let masked: Vec<_> = pass
            .persistent
            .iter()
            .filter(|i| i.flags[1] & VIS_MASKED != 0)
            .collect();
        assert_eq!(masked.len(), 4);
        for instance in masked {
            assert_eq!(instance.flags[0] & SHADOW_CASTS, 0, "no solid cluster shadow");
        }
        let swaying: Vec<_> = pass
            .persistent
            .iter()
            .filter(|instance| instance.flags[1] >> 8 != 0)
            .collect();
        assert_eq!(swaying.len(), 2, "the adopted sets keep their sway records");
        for instance in swaying {
            assert_eq!(instance.flags[1] & VIS_MASKED, VIS_MASKED);
            assert_eq!(instance.scale[1], crate::FoliageSway::default().max_offset());
        }
    }

    /// Compaction moves the surviving meshes' runs: each slot then names its own records, and a
    /// set's instances are rebuilt with the new range.
    #[test]
    fn a_level_compaction_moves_the_runs_with_their_meshes() {
        let (vertices, indices, single) = triangle();
        let mut two_levels = (*single).clone();
        let coarse = Meshlet {
            level: 1,
            lod_error: 0.5,
            ..two_levels.meshlets[0]
        };
        two_levels.meshlets[0].parent_error = 0.5;
        two_levels.meshlets.push(coarse);
        let mut pass = VisBufferPass::default();
        for key in 0..70 {
            // Each mesh's runs differ, so a wrong range reads another mesh's records.
            let mut mesh = two_levels.clone();
            mesh.meshlets[0].parent_error = 0.5 + key as f32;
            pass.insert_cooked(key, Arc::new(mesh));
            assert!(pass.ensure_mesh(key, &vertices, &indices));
        }
        let kept = 69;
        let material = LabMaterial::default();
        let models = [IDENTITY];
        assert!(pass.set_instance_set(
            1,
            &VisInstanceSet {
                mesh_key: kept,
                vertices: &vertices,
                indices: &indices,
                material_key: 10,
                material: &material,
                models: &models,
                shadow: ShadowCast::Off,
                instance_shadows: &[],
            },
        ));
        pass.begin_frame();
        let before = pass.meshes[&kept].runs;
        assert_eq!(pass.persistent[0].flags[2], before.0);
        for key in 0..40 {
            assert!(pass.drop_resident(key));
        }
        assert!(pass.levels.wants_compaction());
        pass.begin_frame();
        assert!(!pass.levels.wants_compaction());
        assert_eq!(
            pass.levels.len(),
            30 * before.1 as usize,
            "only the survivors' runs remain"
        );
        let after = pass.meshes[&kept].runs;
        assert!(after.0 < before.0, "the runs moved: {before:?} -> {after:?}");
        assert_eq!(pass.persistent[0].flags[2], after.0, "the set follows its mesh");
        for key in 40..70 {
            let (first, count) = pass.meshes[&key].runs;
            let records = &pass.levels.records()[first as usize..(first + count) as usize];
            assert_eq!(records, pass.mesh_runs[&key].as_slice(), "mesh {key}");
        }
    }

    /// A masked push is a masked instance with no cluster shadow, counted with its material, and
    /// its mesh's level runs ride in the instance (ADR-3915).
    #[test]
    fn a_masked_push_is_flagged_and_counted() {
        let (vertices, indices, meshlets) = triangle();
        let mut pass = VisBufferPass::default();
        pass.insert_cooked(1, meshlets);
        assert!(pass.ensure_mesh(1, &vertices, &indices));
        pass.begin_frame();
        let opaque = pass.push_instance(1, 10, IDENTITY, IDENTITY, ShadowCast::All, false);
        let masked = pass.push_masked_instance(1, 11, IDENTITY, IDENTITY, true);
        let again = pass.push_masked_instance(1, 11, IDENTITY, IDENTITY, false);
        assert_eq!((opaque, masked, again), (Some(0), Some(1), Some(2)));
        assert_eq!(pass.masked_instance_count(), 2);
        assert_eq!(pass.masked_material_count(), 1, "one masked material");
        let flags: Vec<[u32; 4]> = pass.instances.iter().map(|inst| inst.flags).collect();
        assert_eq!(flags[0][1], 0);
        assert_eq!(flags[1][1], VIS_MASKED | VIS_DOUBLE_SIDED);
        assert_eq!(flags[2][1], VIS_MASKED);
        assert_ne!(
            flags[0][0] & SHADOW_CASTS,
            0,
            "the opaque instance casts cluster shadows"
        );
        assert_eq!(
            flags[1][0] & SHADOW_CASTS,
            0,
            "a masked one casts in the forward passes"
        );
        assert_eq!(
            flags[1][3], 0,
            "a one-level mesh has no runs: every cluster a candidate"
        );
        pass.begin_frame();
        assert_eq!(pass.masked_instance_count(), 0, "counts are per frame");
        assert_eq!(pass.masked_material_count(), 0);
    }

    #[test]
    fn masked_materials_are_alpha_mask_without_hair_or_parallax() {
        let mask = LabMaterial {
            alpha_mode: crate::lab_frame::ALPHA_MASK,
            ..LabMaterial::default()
        };
        assert!(alpha_tests(&mask));
        assert!(!alpha_tests(&LabMaterial::default()), "opaque");
        let mut parallax = mask.clone();
        parallax.hard_surface.pom_scale = 0.05;
        assert!(!alpha_tests(&parallax), "the forward UV moves with parallax");
        let blend = LabMaterial {
            alpha_mode: crate::lab_frame::ALPHA_BLEND,
            ..LabMaterial::default()
        };
        assert!(!alpha_tests(&blend));
    }

    #[test]
    fn cluster_lod_needs_an_active_path_and_usable_meshlets() {
        let (vertices, indices, meshlets) = triangle();
        let mut pass = VisBufferPass::default();
        pass.insert_cooked(1, Arc::clone(&meshlets));
        assert!(!pass.takes_cluster_lod(1), "the path has not drawn a frame");
        pass.set_active(true);
        assert!(pass.active());
        assert!(
            pass.takes_cluster_lod(1),
            "pending meshlets: resident at LOD 0's first draw"
        );
        assert!(pass.ensure_mesh(1, &vertices, &indices));
        assert!(pass.takes_cluster_lod(1), "resident");
        assert!(
            !pass.takes_cluster_lod(2),
            "no meshlets: the mesh LOD chain keeps it"
        );

        // Meshlets cooked for other mesh data are declined at the first draw; the chain takes the
        // mesh back from the next frame.
        pass.insert_cooked(3, Arc::new(MeshletMesh::single_level(&[[0.0; 3]; 4], &indices)));
        assert!(pass.takes_cluster_lod(3), "not checked until it draws");
        assert!(!pass.ensure_mesh(3, &vertices, &indices));
        assert!(!pass.takes_cluster_lod(3), "declined");

        pass.set_active(false);
        assert!(!pass.takes_cluster_lod(1), "the path stopped drawing");
        pass.set_active(true);
        assert!(pass.remove_meshlets(1));
        assert!(!pass.takes_cluster_lod(1), "removed");
    }

    /// A DAG that appends vertices past the mesh's own (a voxel far DAG, ADR-2655) is checked
    /// against the draw's vertex count, not its own source count, and uploads both sets.
    #[test]
    fn appended_vertices_are_checked_against_the_base_range_and_uploaded() {
        let (vertices, indices, _) = triangle();
        let positions = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 1.0],
            [0.0, 1.0, 1.0],
        ];
        // One cluster over the mesh's triangle and a second one over three appended vertices.
        let mut mesh = MeshletMesh::single_level(&positions, &[0, 1, 2, 3, 4, 5]);
        mesh.source_index_count = indices.len() as u32;
        mesh.appended_vertices = super::super::meshlet::AppendedVertices(
            positions[3..]
                .iter()
                .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]))
                .collect(),
        );
        assert_eq!(mesh.base_vertex_count(), 3);
        let mesh = Arc::new(mesh);
        let mut pass = VisBufferPass::default();
        pass.set_active(true);
        pass.insert_cooked(1, Arc::clone(&mesh));
        assert!(pass.ensure_mesh(1, &vertices, &indices), "the base range matches");
        // Six vertices × 16 words, six cluster vertex entries, two triangles.
        assert_eq!(pass.arena_stats().geometry_words, 6 * 16 + 6 + 2);
        // The same DAG drawn with a six-vertex buffer is not the mesh it was built for.
        let six: Vec<LabVertex> = positions
            .iter()
            .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]))
            .collect();
        pass.insert_cooked(2, mesh);
        assert!(!pass.ensure_mesh(2, &six, &indices), "declined");
    }

    #[test]
    fn a_mesh_refused_at_the_arena_cap_stays_on_its_chain() {
        let (vertices, indices, meshlets) = triangle();
        let mut pass = VisBufferPass {
            // Room for one triangle mesh (52 words) and its cluster, no growth.
            arena: ClusterArena::with_limits(64, 64, 1, 1),
            ..VisBufferPass::default()
        };
        pass.set_active(true);
        pass.insert_cooked(1, Arc::clone(&meshlets));
        pass.insert_cooked(2, Arc::clone(&meshlets));
        assert!(pass.ensure_mesh(1, &vertices, &indices));
        assert!(!pass.ensure_mesh(2, &vertices, &indices), "the arena is full");
        assert!(pass.has_meshlets(2), "refused meshlets are kept for a retry");
        assert!(
            !pass.takes_cluster_lod(2),
            "a refused mesh draws forward: its chain picks the LOD"
        );
        assert!(pass.takes_cluster_lod(1));
        // Freeing room lifts the refusal: the next frame hands it over and draws LOD 0.
        assert!(pass.remove_meshlets(1));
        assert!(pass.takes_cluster_lod(2));
        assert!(pass.ensure_mesh(2, &vertices, &indices));
    }

    #[test]
    fn a_device_limit_set_before_the_first_mesh_caps_the_first_frame() {
        let (vertices, indices, meshlets) = triangle();
        let mut pass = VisBufferPass::default();
        // A device whose storage bindings hold 256 bytes: 64 geometry words, 2 cluster records.
        pass.clamp_arena(256);
        pass.insert_cooked(1, Arc::clone(&meshlets));
        pass.insert_cooked(2, Arc::clone(&meshlets));
        assert!(pass.ensure_mesh(1, &vertices, &indices), "52 words fit");
        assert!(
            !pass.ensure_mesh(2, &vertices, &indices),
            "104 words would pass the limit"
        );
        let stats = pass.arena_stats();
        assert!(stats.capped);
        assert_eq!(stats.geometry_capacity_words, 64);
        assert!(stats.cluster_capacity <= 2);
        // Without the clamp the default caps take both.
        let mut unclamped = VisBufferPass::default();
        unclamped.insert_cooked(1, Arc::clone(&meshlets));
        unclamped.insert_cooked(2, meshlets);
        assert!(unclamped.ensure_mesh(1, &vertices, &indices));
        assert!(unclamped.ensure_mesh(2, &vertices, &indices));
        assert!(!unclamped.arena_stats().capped);
    }

    /// With the default LOD hysteresis a plant approaching the camera keeps its voxel far level
    /// until the projected error passes the threshold plus the band. Its sway must be at rest by
    /// then: the mesh levels that replace the voxel level start from rest (ADR-3916). Before, the
    /// switch distance ignored the band and the mesh levels arrived at about a quarter of their
    /// sway.
    #[test]
    fn corr_visbuffer_sway_is_at_rest_where_a_held_voxel_level_hands_over() {
        let settings = super::super::VisBufferSettings::default();
        let threshold = settings.lod_threshold_px;
        let band = settings.resolved_lod_hysteresis_px();
        assert!(band > 0.0, "the default has a band");
        let pixels_per_unit = 1080.0 / (2.0 * 0.45f32.tan());
        // The first voxel level's world error; the sphere's own reach is left at 0.
        let error = 0.05;
        let switch = error * super::sway_switch_scale(pixels_per_unit, &settings);
        // On approach the held voxel level gives way where error * ppu / distance = T + band.
        let handover = error * pixels_per_unit / (threshold + band);
        let fade = crate::visbuffer::sway::sway_fade(handover, switch);
        assert_eq!(fade, 0.0, "the plant is still swaying at the handover: {fade}");
        // Without a band the handover is the hard cut, as before.
        let hard = super::super::VisBufferSettings {
            lod_hysteresis_px: Some(0.0),
            ..settings
        };
        let switch = error * super::sway_switch_scale(pixels_per_unit, &hard);
        assert!((switch - error * pixels_per_unit / threshold).abs() <= 1e-3 * switch);
    }

    #[test]
    fn whole_mesh_upload_folds_non_finite_errors() {
        let mut cooked: Meshlet = bytemuck::Zeroable::zeroed();
        cooked.lod_error = f32::INFINITY;
        cooked.parent_error = f32::NAN;
        cooked.vertex_offset = 4;
        cooked.triangle_offset = 7;
        cooked.vertex_count = 3;
        cooked.level = 2;
        let uploaded = whole_mesh_record(
            &cooked,
            100 + cooked.vertex_offset,
            200 + cooked.triangle_offset,
            1,
        );
        assert_eq!(uploaded.vertex_offset, 104);
        assert_eq!(uploaded.triangle_offset, 207);
        assert_eq!(uploaded.pad, 1, "the voxel-far flag still rides in pad");
        assert_eq!(uploaded.lod_error, 0.0, "a non-finite own error uploads as 0");
        assert!(
            uploaded.parent_error >= super::super::ROOT_ERROR,
            "uploaded parent {} is not a root",
            uploaded.parent_error
        );
        let mut negative: Meshlet = bytemuck::Zeroable::zeroed();
        negative.lod_error = -2.0;
        negative.parent_error = f32::NEG_INFINITY;
        let kept = whole_mesh_record(&negative, 0, 0, 0);
        assert_eq!(kept.lod_error, -2.0);
        assert_eq!(kept.parent_error, f32::NEG_INFINITY);
    }

    #[test]
    fn gpu_structs_have_wgsl_sizes() {
        assert_eq!(std::mem::size_of::<GpuInstance>(), 144);
        assert_eq!(std::mem::size_of::<CullParams>(), 256);
        assert_eq!(std::mem::size_of::<RasterParams>(), 96);
        assert_eq!(std::mem::size_of::<ResolveParams>(), 96);
        assert_eq!(std::mem::size_of::<VelocityParams>(), 224);
    }

    /// One root cluster per entry, all sharing vertex 0, so the cluster-bit cursor binds before
    /// the instance cap (`u32::MAX / meshlet_count` instances).
    fn cluster_grid(count: u32) -> (Vec<LabVertex>, Vec<u32>, Arc<MeshletMesh>) {
        let positions = [[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]];
        let vertices: Vec<LabVertex> = positions
            .iter()
            .map(|&p| LabVertex::from_pos_normal_uv(p, [0.0, 0.0, 1.0], [0.0, 0.0]))
            .collect();
        let indices = vec![0, 1, 2];
        let sphere = [0.5, 0.5, 0.0, 1.0];
        let mut mesh = MeshletMesh {
            source_vertex_count: vertices.len() as u32,
            source_index_count: indices.len() as u32,
            bounds: sphere,
            ..MeshletMesh::default()
        };
        mesh.vertices.resize(count as usize, 0);
        mesh.triangles.resize(count as usize, 0);
        mesh.meshlets.reserve(count as usize);
        for i in 0..count {
            mesh.meshlets.push(Meshlet {
                bounds: sphere,
                cone: [0.0, 0.0, 1.0, 1.0],
                lod: sphere,
                parent: sphere,
                lod_error: 0.0,
                parent_error: crate::visbuffer::ROOT_ERROR,
                vertex_offset: i,
                triangle_offset: i,
                vertex_count: 1,
                triangle_count: 1,
                level: 0,
                pad: 0,
            });
        }
        mesh.validate().expect("cluster grid");
        (vertices, indices, Arc::new(mesh))
    }

    fn grid_set<'a>(
        vertices: &'a [LabVertex],
        indices: &'a [u32],
        material: &'a LabMaterial,
        models: &'a [[[f32; 4]; 4]],
    ) -> VisInstanceSet<'a> {
        VisInstanceSet {
            mesh_key: 1,
            vertices,
            indices,
            material_key: 10,
            material,
            models,
            shadow: ShadowCast::Off,
            instance_shadows: &[],
        }
    }

    /// `set_instance_set` must not report success while `begin_frame` draws fewer instances than
    /// the model slice. The cluster-bit cursor (`u32` visibility-bit base) fills before the
    /// instance cap once a mesh has more than 1024 clusters.
    #[test]
    fn render2_correctness_oversized_instance_set_is_refused_and_a_fitting_one_stays_whole() {
        let clusters: u32 = 65_536;
        let (vertices, indices, meshlets) = cluster_grid(clusters);
        let mut pass = VisBufferPass::default();
        pass.insert_cooked(1, meshlets);
        let material = LabMaterial::default();
        let fit = (u32::MAX as u64 / u64::from(clusters)) as usize;
        let exact = vec![IDENTITY; fit];
        let accepted = pass.set_instance_set(1, &grid_set(&vertices, &indices, &material, &exact));
        pass.begin_frame();
        assert!(
            accepted && pass.instance_count() == exact.len(),
            "a set at the bit budget ({fit}) must be stored whole, drew {}",
            pass.instance_count()
        );

        let over = vec![IDENTITY; fit + 1];
        let replaced = pass.set_instance_set(1, &grid_set(&vertices, &indices, &material, &over));
        pass.begin_frame();
        assert!(
            !replaced || pass.instance_count() == over.len(),
            "set_instance_set returned {replaced} but drew {} of {} instances",
            pass.instance_count(),
            over.len()
        );
        assert!(!replaced, "a set past the cluster-bit budget must be refused");
        assert_eq!(
            pass.instance_count(),
            exact.len(),
            "refusing the replacement leaves the previous set in place"
        );

        let extra = vec![IDENTITY; 1];
        let second = pass.set_instance_set(2, &grid_set(&vertices, &indices, &material, &extra));
        pass.begin_frame();
        assert!(!second, "a second set that crosses the cursor is refused");
        assert_eq!(pass.instance_count(), exact.len());
    }
}
