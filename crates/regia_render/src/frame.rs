// SPDX-License-Identifier: MPL-2.0
//! Persistent clustered-forward frame renderer (`specs/13` §§ 5–8).
//!
//! This is the ONE frame path: the golden harness ([`crate::render_lab_scene`]) and games
//! rendering through the ECS runtime path ([`crate::runtime`]) both submit a
//! [`FrameScene`] to a [`FrameRenderer`]. GPU resources persist across frames —
//! pipelines, shader modules, bind groups, shadow targets, IBL maps and per-mesh
//! uploads are created once and reused. Per-frame uniforms (frame, lights, object
//! matrices, skin palettes) are rewritten only when their bytes change.
//!
//! Pass sequence per view: shadow passes (sun cascades + local-light atlas) →
//! froxel cluster build (compute) → depth prepass → forward colour (opaque,
//! then sorted blended) → post stack (exposure, bloom, tonemap, FXAA).
//!
//! ## Provenance
//!
//! - Clustered forward light assignment: Olsson, Billeter, Assarsson,
//!   "Light Limits: Clustered Forward Shading", HPG 2012 (froxels, exponential Z).
//! - EV100 exposure: `1 / (1.2 · 2^EV100)` per Filament's public exposure notes.

use std::borrow::Cow;
use std::collections::HashMap;

use crate::lab_frame::{LabMaterial, LabVertex};
use crate::profile::FrameQuality;
use crate::shader_lib::compose_source;
use crate::skin::{JointMatrix, MAX_JOINTS_PER_SKIN};
use crate::{DirectionalLightUniform, ForwardResult, PostSettings, RenderDevice, RenderError};
use naga_oil::compose::ShaderDefValue;

mod canopy_view;
#[cfg(feature = "frame-gen")]
mod frame_gen_hook;
#[cfg(feature = "frame-gen")]
pub use frame_gen_hook::FrameGenInputs;
mod glass_gpu;
mod lifecycle;
mod mask_decals_gpu;
mod origin_shift;
pub(crate) use origin_shift::shifted_view_proj;
// `pub(crate)`: `crate::visibility` culls `ViewVisibility` with the same posed bound (ADR-7255).
pub(crate) mod skin_bounds;
mod skin_twin;
// Skinned instancing (ADR-1368): the switch and the skinned crowd counters.
mod skinned_instancing;
/// Two-eye frames for head-mounted displays (ADR-1918).
mod stereo;
mod teardown;
mod translucent_shadow;
mod view_state;
// Morph targets (Tincal B1): the compute pre-pass and the instances' deformed copies.
mod morph_gpu;
// Bindless step 2a (ADR-356): texture-set census of the material groups.
mod texture_set_census;
// Bindless step 5 (ADR-1110): GPU residency feedback into streaming.
mod residency_feedback;
// Reusable per-frame lists and their growth counter (ADR-385).
pub(crate) mod scratch;
// 16-bit offscreen readout for render-out (ADR-1404).
mod offscreen_deep;
// Pipeline construction for the forward, shadow and colour sets (ADR-1552 split).
mod pipelines;
// Texture and table uploads (ADR-1552 split).
mod gpu_upload;
// CPU readbacks of colour, depth, motion and HDR captures (ADR-1552 split).
mod readback;
// Bind group layout entry helpers (ADR-1552 split).
mod layouts;
// The pass extension point: host passes at the graph's named slots (ADR-1552).
mod extension;
// The frame's render graph: FRAME_STEPS plus registered passes, versioned (ADR-2018).
mod frame_graph;
// Passes of `render_into`, one module each (ADR-1552 split by pass).
mod cluster_probes;
mod display_passes;
mod forward_pass;
mod gpu_cull;
mod many_light;
mod oit_gpu;
mod outdoor_passes;
mod post_stack;
mod prepass;
mod prepass_two_phase;
mod screen_occlusion;
mod screen_space;
mod shadow_passes;
mod shadow_two_phase;
mod subsurface_gpu;
mod sun_trace;
mod temporal;
mod visbuffer_resolve;
use crate::GraphSlot;
pub use extension::{
    FRAME_STEPS, FrameGraphReport, FramePass, FramePassContext, FramePassInfo, FrameStep, PassRegisterError,
    SlotColorSpace, slot_color_space, slot_from_name, slot_name,
};
use frame_graph::{BuiltinNode, FrameSite};
pub use frame_graph::{
    FrameDispatchInfo, FrameDispatchViolation, FrameGraphError, FrameGraphInfo, FrameGraphNode,
};
pub use graph_buffers::FrameGraphBuffer;
pub use sun_trace::SunTraceReport;
// ReSTIR GI state, recorded inside the `screen_occlusion` step (ADR-3349): not a frame step of its own.
mod restir_gi;
// GPU-side frame, light, object and material layouts (ADR-1552 split).
mod gpu_types;
// Mesh bounds and the view frustum (ADR-1552 split).
mod culling;
// Shadow planning: cascades, the local-light atlas and caster lists (ADR-1552 split).
mod shadow_plan;
// GPU timestamp queries and the CPU and GPU timing reports (ADR-1552 split).
mod timing;
// Pipeline prewarm and material-graph warm-up (ADR-1552 split).
mod prewarm;
// GI, reflection-probe and baked-light settings (ADR-1552 split).
mod gi_api;
// Visibility-buffer and meshlet settings (ADR-1552 split).
mod visbuffer_api;
// Offscreen renders, render-to-texture views and their readbacks (ADR-1552 split).
mod offscreen;
// Mesh and object uploads, skin palettes and cache eviction (ADR-1552 split).
mod mesh_cache;
// Resident draws of meshes whose host dropped the CPU copy (ADR-7891).
mod host_geometry;
// Material uploads and texture streaming (ADR-1552 split).
mod materials;
// Frame targets, scratch textures and GPU object helpers (ADR-1552 split).
mod targets;
// FrameRenderer switches, settings and read-only reports (ADR-1552 split).
mod settings;
/// The studio environment's settings and reads (ADR-2000).
mod studio_state;
// The shared draw's per-frame engine inputs (ADR-1552 split).
mod shared_engine;
// Per-frame forward, deferred and mask-decal pipeline resolution (ADR-1552 split).
mod pipeline_sets;
// The frame's view bind group (ADR-1552 split).
mod view_bindings;
// Forward decals: the host API and the per-view staging (ADR-3255).
mod forward_decal_hook;
// The frame's draw list: entries, draws and index lists (ADR-1552 split, line m).
mod draw_list;
// CPU masked occlusion culling of the main view's draw list (ADR-10072).
mod masked_cull;
pub use masked_cull::{
    MASKED_CULL_HEIGHT, MASKED_CULL_WIDTH, MASKED_IDLE_FRAMES, MASKED_MAX_OCCLUDERS, MASKED_MIN_CULLABLE,
    MASKED_OCCLUDER_MIN_SCREEN, MASKED_OCCLUDER_TRIANGLE_BUDGET, MASKED_PROBE_INTERVAL, MaskedCullReport,
    MaskedCullSkip,
};
// Shadow targets, casters, jobs and caster lists after the CPU plan (ADR-1552 split, line m).
mod shadow_setup;
// The radiance cache's step in the SSGI pass (ADR-2085).
mod radiance_cache_pass;
// The mesh occluders and the shared draw's prepare step (ADR-1552 split, line m).
mod shared_prepare;
// Surface weather, the precipitation extinction and occlusion view (spec 99).
mod weather;
// The instanced shadow and depth lists (ADR-1552 split, line m).
mod instance_lists;
// GPU state, the frame uniform and per-frame buffer writes (ADR-1552 split, line m).
mod frame_uniforms;
// The end of the frame after the submit (ADR-1552 split, line m).
mod frame_end;
// The frame's lights and the CPU shadow plan (ADR-1552 split, line m).
mod frame_lights;
// The baked lightmap's receivers, `LIGHTMAP` variant and group 3 (ADR-1623).
mod lightmap_forward;
// Its GPU-driven, visibility-buffer and Mobile lite paths (ADR-2107).
mod lightmap_paths;
// The thin film's `THIN_FILM` variant and group 3 (ADR-1977).
mod thin_film_gpu;
// The Mobile tiers' lite forward pipelines (ADR-1874).
mod forward_lite_gpu;
// The setup and bookkeeping steps of `render_into`, one module each (ADR-2300).
mod steps;
// The forward shader's frame-level studio axis (ADR-2308).
mod forward_studio;
// Extra directional lights, ambient volumes and the shader debug views (ADR-7211).
mod lights_views;
// Counted asset meshes drawn from their GPU upload (ADR-7284).
mod asset_meshes;
pub(crate) use asset_meshes::{AssetMeshSkin, asset_mesh_skin};
// The two HDR scratch images as keyed transients of the view's pool (ADR-7877).
mod graph_buffers;
mod hdr_scratch;
pub(crate) use culling::mesh_bounds;
pub use culling::{Frustum, MeshBounds};
use draw_list::DrawList;
use frame_lights::FrameLights;
pub(crate) use gpu_types::LAB_FRAME_LIGHTS_PREFIX;
#[cfg(test)]
pub(crate) use gpu_types::{GPU_LIGHT_BYTES, SHADOW_TILE_BYTES, lab_frame_prefix_offsets};
use gpu_types::{
    GpuLight, GpuMaterial, LabFrame, ObjectData, ShadowTileGpu, ShadowVp, TABLE_RECORD_BYTES, pack_light,
    pack_material,
};
use gpu_upload::{build_ibl, create_probe_atlas, solid_rgba, upload_ltc_table, upload_rgba};
pub(crate) use gpu_upload::{
    sibling_view_format, upload_material_texture, upload_material_texture_views, upload_rgba8_chain,
    upload_rgba8_chain_views,
};
use layouts::{
    compare_samp_entry, depth_tex_array_entry, depth_tex_entry, point_samp_entry, samp_entry, storage_entry,
    storage_uniform, tex_array_entry, tex_entry,
};
pub use many_light::{ManyLightReport, ManyLightSkip};
use materials::material_bind_group;
#[cfg(test)]
use mesh_cache::{derived_cache_budget, derived_cache_budgets, fold_joint, rigid_to_joint_zero};
pub(crate) use pipelines::shared_pipes_key;
use pipelines::{
    MATERIAL_ANISOTROPY, add_sun_rt_def, build_pipeline_set, build_pipes, lab_color_pipeline_entry,
    shader_module,
};
pub use readback::{HdrCapture, MotionField, read_rgba8};
use readback::{HdrCopy, decode_hdr_texel, f16_to_f32, read_depth32, record_hdr_copy};
#[cfg(test)]
use shadow_plan::CSM_CASTER_REACH;
use shadow_plan::{
    CascadeSticky, CasterListCache, ShadowJob, blended_sort_depth, cascade_mask, fog_cascade_index,
    fog_light_shadows, plan_shadows, shadow_caster_id, shadow_model, world_caster_aabb,
};
pub(crate) use shadow_plan::{cascade_back, csm_far, ortho_light_vp};
use shadow_setup::{ShadowCasters, ShadowJobs};
pub use stereo::{
    EyeCamera, EyeFrame, EyeFrameReport, EyeHiddenArea, STEREO_EYE_VIEWS, hidden_area_clip_vertices,
    stereo_eye_of_view,
};
use targets::{
    DepthMatch, TargetPair, begin_load_pass, binding, depth_match_pipe, depth_target, extent3d, make_buffer,
    make_ldr, texture_binding,
};
use timing::{
    AddElapsedMs, CONTACT_QUERY, DDGI_QUERY_BASE, DDGI_STAGE_QUERY_BASE, FORWARD_GLASS_QUERY_BASE,
    FrameStamps, GpuTimer, OUTDOOR_QUERY_BASE, SHADOW_KIND_QUERY_BASE, SHARED_QUERY_BASE, SSSR_QUERY_BASE,
    TIMESTAMP_QUERIES, outdoor_query_slot,
};
pub use timing::{
    FrameCpuTimings, FrameExtent, GpuNodePass, GpuStamp, GpuStampNodes, GpuTimings, ScreenPassReport,
    ShadowCacheReport, ShadowCpuTimings,
};

pub use canopy_view::{CANOPY_EYE_MARGIN_M, CanopyDraws};
pub use lifecycle::DrawIntegrity;
use lifecycle::missing_draw_entry;
pub use scratch::FrameScratchStats;
pub use teardown::RendererDropReport;

use crate::view_math::look_at_view;
use regia_core::glam::camera::rh::proj::directx::{orthographic, perspective};

/// Opaque alpha mode ([`crate::ALPHA_OPAQUE`]).
const ALPHA_BLEND: u32 = 2;
/// Alpha blend ([`crate::ALPHA_BLEND`]).
const ALPHA_PREMULTIPLIED: u32 = 3;
/// The glass front layer's transmittance target (`fs_transmit` output, 0..1).
const FRONT_LAYER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// Additive blend ([`crate::ALPHA_ADDITIVE`]).
const ALPHA_ADDITIVE: u32 = 4;

const CLUSTER_COUNT: u32 = crate::CLUSTER_GRID_X * crate::CLUSTER_GRID_Y * crate::CLUSTER_GRID_Z;
/// Maximum shadow tiles in the local-light atlas plan (kept in one fixed buffer).
const MAX_SHADOW_TILES: usize = 64;
/// Staging-belt bytes a renderer reserves per instance: the per-frame uploads of one rigid
/// instance (motion-vector instance, instance-list entries for the prepass and each shadow
/// cascade, cull object, two-phase cull entry) with room. Measured at 492 to 546 B on DesktopHigh
/// with sun shadows, TAA and two-phase occlusion (ADR-3189); the dense-cut tests fail if a frame
/// outgrows it.
const BELT_BYTES_PER_INSTANCE: u64 = 640;
/// Most staging-belt bytes a renderer reserves up front.
const BELT_RESERVE_CAP: u64 = 32 * 1024 * 1024;
/// The staging-belt room `instances` reserved instances get ([`FrameRenderer::prewarm`],
/// [`FrameRenderer::reserve_instances`]).
fn belt_reserve_bytes(instances: u32) -> u64 {
    (u64::from(instances) * BELT_BYTES_PER_INSTANCE).min(BELT_RESERVE_CAP)
}
/// Most mesh bytes one frame copies into the mesh arenas through the staging belt; the rest
/// (a level load) goes through `queue.write_buffer`, so the belt never grows to a level's size.
const MESH_BELT_FRAME_BYTES: u64 = 4 << 20;
/// Belt room a mesh upload must leave free for the frame's later uploads (instance lists,
/// objects), so routing a mesh through the belt never makes it grow on the frame path.
const MESH_BELT_HEADROOM: u64 = 1 << 20;

/// Cached GPU entries unused for this many frames may be evicted: object slots always, meshes
/// and materials only while the caches are over their budget ([`CacheResidency`]).
const EVICT_AFTER_FRAMES: u64 = 600;
/// Default [`CacheResidency::max_idle_frames`]: ~10 minutes at 60 Hz.
const EVICT_MAX_IDLE_FRAMES: u64 = 36_000;
/// Cache budget base when texture streaming has no resident budget (2 GiB).
const CACHE_FALLBACK_BUDGET: u64 = 2 << 30;
/// Mesh buffers and textures uploaded whole, on top of the texture streaming budget (1 GiB).
const CACHE_MESH_ALLOWANCE: u64 = 1 << 30;
/// [`CACHE_FALLBACK_BUDGET`] on Android, whose GPU shares the app's memory (512 MiB).
const ANDROID_CACHE_FALLBACK_BUDGET: u64 = 512 << 20;
/// [`CACHE_MESH_ALLOWANCE`] on Android (256 MiB).
const ANDROID_CACHE_MESH_ALLOWANCE: u64 = 256 << 20;
/// Most a derived cache budget may be on Android (1 GiB).
const ANDROID_CACHE_CAP: u64 = 1 << 30;
/// A derived cache budget is at most this share of the adapter's device-local VRAM (1/2).
const CACHE_VRAM_SHARE_SHIFT: u32 = 1;
/// Derived budgets: the mesh cache's own budget is this share of device-local VRAM (1/8),
/// between [`CACHE_MESH_ALLOWANCE`] and [`CACHE_MESH_MAX`].
const CACHE_MESH_VRAM_DIVISOR: u64 = 8;
/// Most a derived mesh budget may be (4 GiB).
const CACHE_MESH_MAX: u64 = 4 << 30;
/// Least a derived mesh budget may be once the VRAM cap applies (256 MiB).
const CACHE_MESH_FLOOR: u64 = 256 << 20;
/// Derived budgets: meshes and materials together hold at most this many eighths of
/// device-local VRAM.
const CACHE_TOTAL_VRAM_EIGHTHS: u64 = 5;
/// Derived budgets: an entry drawn within this many seconds is never evicted by a budget
/// (only by memory pressure), whatever the frame rate.
const CACHE_MIN_IDLE_SECS: f32 = 20.0;
/// Derived budgets: a cache over its budget is swept down to this many eighths of it, so the
/// next uploads do not trigger the next sweep at once.
const CACHE_LOW_WATER_EIGHTHS: u64 = 7;
/// After a memory-pressure signal, the sweep evicts every unpinned entry idle this many frames
/// (~0.5 s), whatever the budget.
const PRESSURE_MIN_IDLE_FRAMES: u64 = 30;
/// Eviction sweep interval in frames.
const EVICT_SWEEP_EVERY: u64 = 256;
/// Frames between refreshes of the texture budget problems ([`FrameRenderer::texture_memory`]).
const TEXTURE_PROBLEMS_EVERY: u64 = 60;
/// Frames a per-size target set may go unused before it is released (window drag, dynamic
/// resolution). Views at several sizes in one frame (split screen) keep theirs.
const TARGET_IDLE_FRAMES: u64 = 4;
/// Held frames a temporally antialiased view keeps rendering after its static-screen key
/// stopped changing, before the cache serves it. Each renders with the next Halton jitter
/// and the resolve keeps a running mean (two 16-sample Halton cycles), so the image that
/// freezes is antialiased, not one jittered frame (motion-look lane).
const HELD_CONVERGE_FRAMES: u32 = 32;
/// Depth the CSM cascade fit substitutes for an infinite far plane (metres).
const CSM_MAX_FAR: f32 = 400.0;
/// Sun-shadow distance: the cascades never span past this many metres, whatever the far
/// plane. Splitting a 12–20 km far plane put the first of four cascades out to ~750 m
/// (~0.8 m per texel at 2048²), so a truss, a rail or a rock cast nothing near the camera.
/// Terrain past it keeps its heightfield sun occlusion.
const CSM_MAX_DISTANCE: f32 = 4_000.0;
/// Rigid casters before a full-resolution Hi-Z pyramid is worth building.
///
/// The pyramid is a full-frame reduction. On Sponza (a few hundred rigid draws)
/// it costs more than the draws it would reject, so the cull stays frustum-only.
/// See the one-submit ADR.
const HIZ_MIN_RIGID_CASTERS: usize = 2048;

/// One mesh submitted to a frame: borrowed vertex/index/material data plus a
/// stable cache identity.
///
/// `mesh_key`/`material_key`/`object_key` name GPU resources across frames:
/// the same key reuses the uploaded buffers and bind groups (contents are
/// re-checked: changed material parameters are rewritten, changed mesh data is
/// re-uploaded). Key `0` is ephemeral: each mesh submitted with a zero key gets its own
/// fresh upload (mesh, material or object slot) every frame and no identity across frames
/// — what a one-shot caller wants. Keys with the top byte [`EPHEMERAL_KEY`] are reserved
/// for that.
#[derive(Clone, Copy, Debug)]
pub struct FrameMesh<'a> {
    /// Stable mesh identity (GPU vertex/index cache).
    pub mesh_key: u64,
    /// Stable material identity (GPU material bind group cache).
    pub material_key: u64,
    /// Stable object identity (model matrix + skin palette span).
    ///
    /// Two draws may share one key. A draw the GPU cull uploads still gets its own object
    /// record and its own indirect command. On the per-object path each side of a
    /// [`Self::lod_dither`] cross-fade gets its own uniform, so the two copies do not share
    /// `skin.y`.
    pub object_key: u64,
    /// Vertices (object space).
    pub vertices: &'a [LabVertex],
    /// Triangle-list indices.
    pub indices: &'a [u32],
    /// Material parameters.
    pub material: &'a LabMaterial,
    /// Column-major object-to-world matrix.
    pub model: [[f32; 4]; 4],
    /// Joint palette in world space (empty = rigid).
    pub joints: &'a [JointMatrix],
    /// Decal receiver layer stamped for this mesh.
    ///
    /// `0` keeps the default: a rigid mesh is static (layer 1) and a skinned mesh is a
    /// character (layer 2). Any other value is the layer index, so a rigid prop can be
    /// masked without a joint palette.
    pub receiver_layer: u32,
    /// Which shadow views this mesh casts into ([`ShadowCast::All`] unless the caller knows
    /// the shadow is short).
    pub shadow: ShadowCast,
    /// Second UV set, one per vertex (empty = none: a material graph that reads it gets the
    /// first set).
    pub uv1: &'a [[f32; 2]],
    /// Screen-space dither of a LOD cross-fade ([`LOD_DITHER_NONE`] = drawn whole). `c` in
    /// `(0, 1)` keeps the share `c` of the pixels (the incoming LOD), `−c` keeps the other
    /// `1 − c` (the outgoing LOD): the two copies of an instance cover every pixel once.
    /// A dithered mesh draws through the alpha-test pipeline under its own batch.
    pub lod_dither: f32,
    /// The entity carries `regia_scene::components::Static`: its transform is not
    /// expected to change at runtime (`specs/14` A1). It does not change how the mesh draws
    /// or caches (ADR-5906): the GPU cull counts all-static batches
    /// ([`crate::static_batch_eligible`], `DrawStats::static_batches`) and the shadow-page
    /// caches count static casters, both as diagnostics. `false` for every mesh submitted
    /// without the marker.
    pub is_static: bool,
}

/// Top byte of the cache keys the renderer gives a [`FrameMesh`] submitted with key 0
/// (`EPHEMERAL_KEY | index in FrameScene::meshes`). Callers must not use keys in this range.
pub const EPHEMERAL_KEY: u64 = 0xFE00_0000_0000_0000;

/// [`FrameMesh::lod_dither`] of a mesh drawn whole.
pub const LOD_DITHER_NONE: f32 = 0.0;

/// Mixed into the mesh key of a dithered draw so it batches apart from the whole draws of
/// the same mesh (the batch picks the alpha-test pipeline).
///
/// Its top byte is not `0xFF` ([`SHADOW_LOD_MESH_TAG`]). Bits 1–2 are clear (a mesh LOD
/// index lives there).
pub(crate) const LOD_DITHER_MESH_TAG: u64 = 0xD17E_5C0F_ADE0_0001;

/// Top byte `0xFF` of a mesh key: this draw is the shadow LOD only. It is left out of the
/// colour pass and the visibility buffer; the shadow pass still draws it (an invisible caster).
///
/// Like [`EPHEMERAL_KEY`] (`0xFE`), callers must not use keys whose top byte is `0xFF`: the
/// check is `mesh_key >> 56 == 0xFF`, and an `Arc` address xor this tag always matches it.
/// A LOD 0 colour draw does not set it, so scenes without a LOD chain keep the `Arc`
/// address as the cache key.
pub const SHADOW_LOD_MESH_TAG: u64 = 0xFF00_0000_0000_0000;

/// Whether a submitted mesh key marks a shadow-LOD-only draw ([`SHADOW_LOD_MESH_TAG`]).
#[must_use]
pub const fn is_shadow_lod_key(mesh_key: u64) -> bool {
    mesh_key >> 56 == 0xFF
}

/// Which shadow views a [`FrameMesh`] is drawn into.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum ShadowCast {
    /// Every sun cascade and local-light tile it touches. The renderer cannot bound how far
    /// the shadow reaches, so this is the default.
    #[default]
    All,
    /// No shadow at all.
    Off,
    /// A short caster (a rock, a prop on the ground): its shadow ends within `reach` metres
    /// of its bounding sphere. It is drawn only into the sun cascades whose receivers it can
    /// shadow (receivers pick a cascade by distance to the eye, `split_distance` plus the
    /// blend band), at most `max_cascades` of them starting at the cascade that holds it,
    /// and only into the shadow pages its light-space footprint covers.
    Local {
        /// How far beyond its bounding sphere the shadow can fall, metres.
        reach: f32,
        /// Most sun cascades the mesh is drawn into (0 = no sun shadow).
        max_cascades: u8,
        /// The shadow passes draw the mesh scaled by this factor about its model origin, and
        /// the shadow-page cache tracks that scaled caster (1 = as drawn). A caster that
        /// shrinks every frame (a distance fade) can hold its shadow size in a few steps, so its
        /// pages are redrawn per step instead of per frame. Honoured on the GPU-driven
        /// (instanced) shadow path; other paths draw the shadow as drawn. A factor that is not
        /// finite and above 0 draws the shadow as drawn too.
        shadow_scale: f32,
        /// The shadow passes draw the mesh this many metres higher (world Y) than drawn, and
        /// the shadow-page cache tracks the lifted caster (0 = as drawn). A caster that bobs by
        /// centimetres every frame (a rock seated on morphing terrain) can cast from a fixed
        /// height, so its pages stay clean. Honoured where `shadow_scale` is.
        shadow_lift: f32,
        /// Pick the cascades as if the caster's centre were at this point. The caller keeps
        /// the choice conservative by adding the distance between the two to `reach`. A caster
        /// whose anchor holds its distance to the eye (scatter rocks anchor at a snapped eye's
        /// distance) keeps its cascades while the camera moves, so its pages are not redrawn.
        anchor: Option<[f32; 3]>,
        /// A mesh change under the same object key keeps the caster's cached shadow pages.
        ///
        /// `false` (what a caller should pass unless it has a page budget to hold): the paged
        /// cache treats a new mesh as a new caster and redraws its old and new pages, so a
        /// swapped mesh that fits the old bounds does not keep the old shadow. `true` is for
        /// LOD chains (scatter rocks) whose switches every few frames would otherwise redraw
        /// pages faster than the page budget allows; the cost is that a page keeps the
        /// previous LOD's shadow until something else redraws it (a moving caster, a cascade
        /// that re-snaps as the camera moves).
        lod_switch_keeps_pages: bool,
    },
}

impl ShadowCast {
    /// Whether the mesh can shadow the sun: anything but [`ShadowCast::Off`] and a
    /// [`ShadowCast::Local`] with `max_cascades: 0` (that one casts for local lights only, in no
    /// cascade, no near sun tile and no traced sun scene; ADR-8414).
    #[must_use]
    pub fn casts_sun(self) -> bool {
        !matches!(self, ShadowCast::Off | ShadowCast::Local { max_cascades: 0, .. })
    }
}

/// The model matrix the shadow passes draw `model` with ([`ShadowCast::Local`]'s
/// `shadow_scale` applied about the model origin, then `shadow_lift` along world Y).
/// Grid the eye is snapped to for persistent sets' GPU sun-cascade choice, metres (a crossing
/// redraws the cached cascades: every ~0.5 s at 15 m/s).
const VIS_SET_EYE_SNAP_M: f32 = 8.0;

/// A frame's render description, borrowed from either a [`crate::LabScene`]
/// (goldens) or the extracted ECS runtime state (games).
#[derive(Clone, Debug)]
pub struct FrameScene<'a> {
    /// Output width.
    pub width: u32,
    /// Output height.
    pub height: u32,
    /// Camera eye.
    pub eye: [f32; 3],
    /// Camera look-at point: any point ahead on the view ray (the frame rebuilds the direction as
    /// `normalize(target − eye)`). Far from the origin, place it with
    /// [`crate::view_math::look_target`]: one unit ahead, its `f32` rounding quantizes the view
    /// direction (ADR-1488).
    pub target: [f32; 3],
    /// Vertical field of view in radians.
    pub fov_y: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
    /// Run the depth prepass.
    pub depth_prepass: bool,
    /// Cascaded shadow map for the directional light.
    pub directional_shadows: bool,
    /// Image-based lighting from the procedural sky.
    pub ibl: bool,
    /// Fixed EV100 exposure (applied by the material shader when `post` is off;
    /// forwarded to the post stack when it is on).
    pub exposure_ev: f32,
    /// Flat ambient illuminance in lux (used when `ibl` is off).
    pub ambient_lux: f32,
    /// Render both triangle faces (glTF doubleSided).
    pub double_sided: bool,
    /// Post stack; `None` tonemaps in the material shader.
    pub post: Option<&'a PostSettings>,
    /// Distance + exponential height fog (off by default).
    pub fog: crate::FogSettings,
    /// Directional light.
    pub directional: DirectionalLightUniform,
    /// Point and spot lights.
    pub local_lights: &'a [crate::LabLocalLight],
    /// Quality knobs (CSM cascades/resolution, shadow atlas, MSAA) for this frame.
    pub quality: FrameQuality,
    /// Meshes to draw.
    pub meshes: Vec<FrameMesh<'a>>,
}

/// Where a frame's final colour lands.
#[derive(Clone, Copy, Debug)]
pub struct FrameTarget<'a> {
    /// Colour attachment to write the final (tonemapped) image into.
    pub view: &'a wgpu::TextureView,
    /// Format of that attachment.
    pub format: wgpu::TextureFormat,
    /// Width of the attachment in pixels.
    pub width: u32,
    /// Height of the attachment in pixels.
    pub height: u32,
    /// Pixel rectangle within the attachment this frame draws into
    /// (x, y, width, height) — the viewport of a split-screen view.
    /// Pass `[0, 0, width, height]` for a full-target frame.
    pub viewport: [u32; 4],
    /// Load the attachment's existing colour instead of clearing it
    /// (views after the first of a multi-view frame preserve earlier views).
    pub load_color: bool,
    /// Colour texture behind [`Self::view`], when the caller still holds it.
    ///
    /// The decal pass copies this into a scene-colour texture when it includes
    /// [`wgpu::TextureUsages::COPY_SRC`]. `None` keeps the tint composite; the
    /// receiver (layer mask and angle fade) is still written.
    pub color: Option<&'a wgpu::Texture>,
}

impl<'a> FrameTarget<'a> {
    /// A target covering its whole attachment.
    #[must_use]
    pub fn full(view: &'a wgpu::TextureView, format: wgpu::TextureFormat, width: u32, height: u32) -> Self {
        Self {
            view,
            format,
            width,
            height,
            viewport: [0, 0, width, height],
            load_color: false,
            color: None,
        }
    }

    /// A target drawing into `viewport` (pixel rect x, y, w, h inside a
    /// `width`×`height` attachment), optionally preserving existing colour.
    #[must_use]
    pub fn rect(
        view: &'a wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
        viewport: [u32; 4],
        load_color: bool,
    ) -> Self {
        Self {
            view,
            format,
            width,
            height,
            viewport,
            load_color,
            color: None,
        }
    }

    /// Attaches the colour texture the decal pass copies when it allows `COPY_SRC`.
    #[must_use]
    pub fn with_color(mut self, color: Option<&'a wgpu::Texture>) -> Self {
        self.color = color;
        self
    }
}

/// An internal offscreen colour target a frame was rendered into, ready for
/// overlay passes and readback.
#[derive(Clone)]
pub struct OffscreenTarget {
    /// The LDR (tonemapped, sRGB-encoded) texture.
    pub texture: wgpu::Texture,
    /// Its view (draw overlays here).
    pub view: wgpu::TextureView,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

// ---------------------------------------------------------------------------
// GPU uniform layouts (must match the WGSL structs exactly)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// CPU shadow plan (sun cascades + local-light atlas tiles)
// ---------------------------------------------------------------------------

/// `scene` with a camera every pass can build finite matrices from (ADR-869), or `None` when
/// its eye, target, field of view and clip planes are already usable.
///
/// A NaN eye (a physics blow-up), an eye on its target, `far: f32::INFINITY`, a zero near
/// plane or a field of view outside `(0, π)` would otherwise make the view-projection NaN or
/// infinite, and every pass of the frame (and the temporal history after it) would inherit
/// that. The first frame that needs a fix logs a warning.
fn usable_camera<'a>(scene: &FrameScene<'a>) -> Option<FrameScene<'a>> {
    let (fov_y, near, far) = crate::view_math::usable_projection(scene.fov_y, scene.near, scene.far);
    let finite3 = |v: [f32; 3]| v.iter().all(|c| c.is_finite());
    let eye = if finite3(scene.eye) { scene.eye } else { [0.0; 3] };
    let ahead = [eye[0], eye[1], eye[2] - 1.0];
    let target = if !finite3(scene.target) {
        ahead
    } else {
        let d = [
            scene.target[0] - eye[0],
            scene.target[1] - eye[1],
            scene.target[2] - eye[2],
        ];
        let len2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
        if len2 > 1.0e-20 && len2.is_finite() {
            scene.target
        } else {
            ahead
        }
    };
    if fov_y == scene.fov_y
        && near == scene.near
        && far == scene.far
        && eye == scene.eye
        && target == scene.target
    {
        return None;
    }
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            target: "regia_render",
            "unusable camera (eye {:?} target {:?} fov_y {} near {} far {}); rendering with eye {eye:?} \
             target {target:?} fov_y {fov_y} near {near} far {far} (logged once)",
            scene.eye,
            scene.target,
            scene.fov_y,
            scene.near,
            scene.far,
        );
    }
    let mut fixed = scene.clone();
    fixed.eye = eye;
    fixed.target = target;
    fixed.fov_y = fov_y;
    fixed.near = near;
    fixed.far = far;
    Some(fixed)
}

/// World up for a view. A zero vector is the ordinary camera (+Y).
fn camera_up_vector(up: [f32; 3]) -> regia_core::glam::Vec3 {
    let v = regia_core::glam::Vec3::from_array(up);
    if v.length_squared() > 1.0e-8 {
        v.normalize()
    } else {
        regia_core::glam::Vec3::Y
    }
}

/// One cubemap face: 90° square, capture quality, no post stack.
/// The target a probe face renders into: [`FrameRenderer::render_offscreen`]'s readback format.
pub(crate) const PROBE_CAPTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

fn probe_face_scene<'a>(
    scene: &FrameScene<'a>,
    eye: [f32; 3],
    target: [f32; 3],
    size: u32,
) -> FrameScene<'a> {
    let near = scene.near.max(0.02);
    FrameScene {
        width: size,
        height: size,
        eye,
        target,
        fov_y: std::f32::consts::FRAC_PI_2,
        near,
        far: scene.far.max(near + 1.0),
        depth_prepass: scene.depth_prepass,
        directional_shadows: scene.directional_shadows,
        ibl: scene.ibl,
        exposure_ev: scene.exposure_ev,
        ambient_lux: scene.ambient_lux,
        double_sided: scene.double_sided,
        post: None,
        fog: scene.fog,
        directional: scene.directional,
        local_lights: scene.local_lights,
        quality: crate::reflections::capture_quality(scene.quality),
        meshes: scene.meshes.clone(),
    }
}

// ---------------------------------------------------------------------------
// Frustum culling (specs/13 § 5 step 3)
// ---------------------------------------------------------------------------

/// How many meshes the last frame drew and frustum-culled (diagnostics + tests).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DrawStats {
    /// Meshes drawn in the colour passes.
    pub drawn: usize,
    /// Meshes rejected by per-view frustum culling.
    pub culled: usize,
    /// Visible material-graph meshes drawn in GPU-driven instanced batches.
    pub graph_instanced: usize,
    /// Visible material-graph meshes drawn one by one (skinned, blended, or a frame without the
    /// GPU-driven path).
    pub graph_per_object: usize,
    /// Visible opaque and masked meshes (any material, either cull mode) drawn in GPU-driven
    /// instanced batches by the forward colour pass.
    pub instanced: usize,
    /// Visible opaque and masked meshes the forward colour pass draws one by one (skinned
    /// without a joint-palette bound or with instancing off, compute-skinned twins, no bounds,
    /// or a frame without the GPU-driven path). Visibility-buffer meshes count in neither.
    pub per_object: usize,
    /// Of [`Self::per_object`], the double-sided materials drawn with the cull-none pipelines.
    pub double_sided_per_object: usize,
    /// Meshes submitted with a one-joint rigid skin whose joint was folded into the model
    /// matrix ([`FrameRenderer::set_rigid_skin_fold`]): they draw as rigid meshes.
    pub rigid_skin_folded: usize,
    /// Meshes submitted with `object_key == 0` whose model matrix changed since the previous
    /// frame at the same list index (0 on the first frame). Such meshes get index-paired motion
    /// vectors: a spawn, despawn or reorder smears under TAA/TSR. Give moving objects a stable
    /// non-zero `object_key` (see the host checklist, "Object keys").
    pub zero_key_moving_meshes: usize,
    /// Visible opaque and masked skinned meshes in the forward colour pass, either path
    /// (ADR-1368). Compute-skinned twins count; folded one-joint skins do not (they draw rigid).
    pub skinned_draws: usize,
    /// Of [`Self::skinned_draws`], those drawn in GPU-driven instanced batches
    /// ([`FrameRenderer::set_skinned_instancing`]). The rest count in [`Self::per_object`].
    pub skinned_instanced: usize,
    /// Distinct `(cull mode, mesh, material, pipeline)` groups among [`Self::skinned_draws`]:
    /// the colour draws they would take if every group were one instanced draw. A masked
    /// instance and a compute-skinned twin (a mesh of its own) are groups of one.
    pub skinned_batches_possible: usize,
    /// GPU-driven colour batches holding skinned instances this frame: one indirect draw each,
    /// however many units it holds.
    pub skinned_batches: usize,
    /// GPU-driven colour batches whose every instance is static-eligible
    /// ([`crate::static_batch_eligible`], `specs/14` A1). A diagnostic (ADR-5906): static and
    /// dynamic instances of one mesh and material share a batch, so this counts the batches
    /// that happen to be all-static, not extra draws.
    pub static_batches: usize,
}

/// Zero-key moving meshes in one TAA frame at which the renderer warns (once per run).
pub const ZERO_KEY_WARN_MIN: usize = 4;

/// When the renderer's mesh and material caches give GPU memory back
/// ([`FrameRenderer::set_cache_residency`]).
///
/// An entry no frame draws stays resident while the caches fit [`Self::budget_bytes`], so a unit
/// that leaves the view and comes back draws without re-uploading its buffers and textures.
/// Over the budget, the sweep (every 256 frames) evicts the least recently drawn entries first,
/// among those idle at least [`Self::min_idle_frames`], until the caches fit again. Entries idle
/// [`Self::max_idle_frames`] go whatever the budget. Memory pressure
/// ([`signal_memory_pressure`], [`FrameRenderer::trim_memory`]) evicts every entry idle ~0.5 s
/// at the end of the next frame, whatever the budget. Pinned keys ([`FrameRenderer::pin_keys`],
/// [`FrameRenderer::prewarm`]) are never evicted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CacheResidency {
    /// Bytes the mesh buffers and material textures may hold together, swept as one LRU over
    /// both caches. `0` (the default) derives two separate budgets instead
    /// ([`FrameRenderer::cache_budgets`]), so textures never evict meshes:
    ///
    /// - materials: the texture streaming resident budget (2 GiB when streaming is off or
    ///   unbudgeted; 512 MiB on Android; on a discrete card at least a quarter of its VRAM,
    ///   [`crate::TextureStreaming::vram_scaled_budget_mib`]) plus 1 GiB (256 MiB on Android)
    ///   for textures uploaded whole, at most half the adapter's device-local VRAM when known
    ///   (Vulkan) and at most 768 MiB on Android;
    /// - meshes: an eighth of the device-local VRAM, 1–4 GiB (1 GiB when unknown, 256 MiB on
    ///   Android), lowered so both stay within five eighths of the VRAM.
    ///
    /// Under the derived budgets an entry drawn in the last 20 s is never evicted by a budget
    /// (only by memory pressure), and a cache over its budget is swept to seven eighths of it.
    /// A memory-pressure step that lowers the streaming budget lowers the material budget too.
    pub budget_bytes: u64,
    /// Over the budget, entries drawn within this many frames are kept (default 600, ~10 s).
    pub min_idle_frames: u64,
    /// Entries undrawn this many frames are evicted under the budget too (default 36 000,
    /// ~10 minutes at 60 Hz).
    pub max_idle_frames: u64,
}

impl Default for CacheResidency {
    fn default() -> Self {
        Self {
            budget_bytes: 0,
            min_idle_frames: EVICT_AFTER_FRAMES,
            max_idle_frames: EVICT_MAX_IDLE_FRAMES,
        }
    }
}

/// Mesh and material cache totals ([`FrameRenderer::cache_stats`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheStats {
    /// Resident mesh entries.
    pub meshes: usize,
    /// Resident material entries.
    pub materials: usize,
    /// Mesh and material keys pinned resident.
    pub pinned: usize,
    /// Bytes the resident meshes (vertex, index and second-UV buffers) and material textures
    /// hold, as of the last eviction sweep.
    pub resident_bytes: u64,
    /// The budget the last sweep compared against ([`CacheResidency::budget_bytes`]).
    pub budget_bytes: u64,
    /// Mesh vertex/index uploads since the renderer was created (first use, a changed size,
    /// or a return after eviction).
    pub mesh_uploads: u64,
    /// Material entries created since the renderer was created (their textures upload with
    /// them; first use, a rebuild, or a return after eviction).
    pub material_uploads: u64,
    /// Mesh entries evicted since the renderer was created.
    pub meshes_evicted: u64,
    /// Material entries evicted since the renderer was created.
    pub materials_evicted: u64,
    /// Mesh entries dropped by [`FrameRenderer::release_meshes`] (their last `Arc` dropped)
    /// since the renderer was created.
    pub meshes_released: u64,
}

/// The mesh and material caches' bytes against their budgets as of the last eviction sweep
/// ([`FrameRenderer::cache_budgets`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CacheBudgets {
    /// `true` under the derived budgets ([`CacheResidency::budget_bytes`] `0`): meshes and
    /// materials are swept against their own budgets. `false`: one budget for both
    /// ([`Self::mesh_budget_bytes`] holds it, [`Self::material_budget_bytes`] is 0).
    pub split: bool,
    /// Bytes of the resident meshes' vertex, index and second-UV buffers.
    pub mesh_bytes: u64,
    /// The mesh budget (the combined budget when not [`Self::split`]).
    pub mesh_budget_bytes: u64,
    /// Bytes of the resident materials' textures (streamed levels and whole uploads).
    pub material_bytes: u64,
    /// The material budget (0 when not [`Self::split`]).
    pub material_budget_bytes: u64,
    /// Counted meshes drawn from their upload ([`FrameRenderer::set_asset_meshes`], ADR-7330):
    /// entries the mesh cache adopted. Not in [`Self::mesh_bytes`], never evicted (the asset
    /// server owns and frees their buffers).
    pub adopted_meshes: usize,
    /// Bytes of the adopted counted meshes' vertex, index and second-UV buffers.
    pub adopted_mesh_bytes: u64,
}

// ---------------------------------------------------------------------------
// Persistent GPU state
// ---------------------------------------------------------------------------

/// One opaque/blend pipeline set for a (target format, encode, double-sided) key.
#[derive(Clone)]
struct PipelineSet {
    depth: wgpu::RenderPipeline,
    color: wgpu::RenderPipeline,
    blend: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    premul: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    add: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// [`Self::blend`] with the depth test off: x-ray render overrides (`crate::render_override`).
    xray: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// Transmission, first pass: multiplies what is behind by the transmittance (`fs_transmit`).
    transmit_mul: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// Transmission, second pass: adds the surface's reflection and remaining diffuse.
    transmit_add: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// [`Self::transmit_add`] while the refraction copy binds the opaque depth (group 3):
    /// `fs_transmit_add` also traces the pane's reflection visibility against it.
    transmit_add_depth: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// Glass front layer (`fs_transmit` into an Rgba8 target with depth writes): the nearest
    /// transmissive layer's depth and transmittance, for [`Self::transmit_add_depth`].
    transmit_front: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// Transmission with screen-space refraction, between the two: adds the bent
    /// background minus the unbent one (`fs_refract`, group 3 = the opaque-frame copy).
    transmit_refract: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    shadow: wgpu::RenderPipeline,
    /// Sky background (full-screen, depth-tested at the far plane).
    sky: wgpu::RenderPipeline,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PipelineKey {
    format: wgpu::TextureFormat,
    /// Manually sRGB-encode in the shader (true for non-sRGB LDR targets).
    encode: bool,
    double_sided: bool,
    /// MSAA sample count of the colour/depth attachments (1 or 4).
    samples: u32,
    /// The ray-traced sun's forward module ([`crate::hwrt::SunShadowVariant::forward_key_bit`],
    /// ADR-2035). `false` unless the profile traces the sun, so every key keeps its value.
    sun_rt: bool,
    /// The studio-off forward module (`STUDIO_IBL_OFF`, ADR-2308): a frame without a studio
    /// feature. `false` composes the full module, what every key did before.
    studio_off: bool,
    /// The opaque colour pipeline shades the many-light reservoir (`RESTIR_DI`, ADR-2652); the
    /// set's blended and glass pipelines keep the light loop. `false` on every key unless the
    /// profile samples.
    restir_di: bool,
}

struct Pipes {
    forward_mod: wgpu::ShaderModule,
    shadow_mod: wgpu::ShaderModule,
    view_bgl: wgpu::BindGroupLayout,
    /// `view_bgl` carries the many-light reservoir binding 37 (ADR-2652).
    many_light_binding: bool,
    /// The 16-byte buffer binding 37 holds while no frame samples.
    many_light_stand_in: wgpu::Buffer,
    /// `view_bgl` carries the blue-noise table at binding 38 (ADR-2652).
    blue_noise_binding: bool,
    /// The forward decals' storage buffer, when `view_bgl` carries binding 39 (ADR-3255).
    forward_decals: Option<crate::forward_decals::ForwardDecalBindings>,
    /// Separate layout for the froxel compute pass (all entries COMPUTE-visible).
    cluster_bgl: wgpu::BindGroupLayout,
    object_bgl: wgpu::BindGroupLayout,
    mat_bgl: wgpu::BindGroupLayout,
    /// Material-graph groups (constants, seven textures, the sampler). The same layout as
    /// `mat_bgl` unless the device draws standard materials through the texture table
    /// (`texture_table`, ADR-863), where `mat_bgl` holds the record only.
    graph_mat_bgl: wgpu::BindGroupLayout,
    /// The texture table's group layout ([`crate::texture_table::TEXTURE_TABLE_GROUP`]) on the
    /// table path; `None` keeps every layout as in earlier releases.
    table_bgl: Option<wgpu::BindGroupLayout>,
    /// The forward layout over `graph_mat_bgl` (graph forward pipelines).
    graph_forward_layout: wgpu::PipelineLayout,
    /// Standard materials keep their constants in [`FrameRenderer::material_arena`] and
    /// `mat_bgl` binding 0 is a storage buffer (`bindless`, ADR-316); `false`: per-material
    /// uniform buffers.
    material_ssbo: bool,
    shadow_bgl: wgpu::BindGroupLayout,
    forward_layout: wgpu::PipelineLayout,
    /// Group 3 of [`Self::refract_layout`]: opaque-frame copy, scene depth, linear clamp.
    refract_bgl: wgpu::BindGroupLayout,
    /// The forward layout plus [`Self::refract_bgl`] (`fs_refract` only).
    refract_layout: wgpu::PipelineLayout,
    /// Linear clamp-to-edge sampler for the refraction copy.
    refract_sampler: wgpu::Sampler,
    /// `GLASS` shader variant pipelines. [`FrameRenderer::prewarm_glass`] builds the culled
    /// and cull-none sets; a frame that needs a set which was not prewarmed compiles it once.
    glass: glass_gpu::GlassPipes,
    /// `MASK_DECALS` variant pipelines and their group 3 (`mask_decals_gpu`).
    mask: mask_decals_gpu::MaskPipes,
    /// `LIGHTMAP` variant pipelines and their group 3 (`lightmap_forward`, ADR-1623).
    lightmap: lightmap_forward::LightmapPipes,
    /// `THIN_FILM` variant pipelines and their group 3 (`thin_film_gpu`, ADR-1977).
    film: thin_film_gpu::FilmPipes,
    /// `SUBSURFACE` variant pipelines and the blur (`subsurface_gpu`, ADR-2049).
    subsurface: subsurface_gpu::SubsurfacePipes,
    /// `OIT` variant pipelines and the composite (`oit_gpu`, ADR-2065).
    oit: oit_gpu::OitPipes,
    shadow_layout: wgpu::PipelineLayout,
    cluster_pipeline: wgpu::ComputePipeline,
    /// Morph target pre-pass (`morph_gpu`), built on first use.
    morph: morph_gpu::MorphPipes,
    sampler: wgpu::Sampler,
    /// Mips every material sample through `sampler` adds to the frame's texture bias: the
    /// device's anisotropic-blend calibration (ADR-5670, `material_aniso_calibration`); 0 where
    /// the anisotropic filter is never sharper than exact trilinear.
    material_lod_offset: f32,
    white_view: wgpu::TextureView,
    /// The same 1×1 white texel as a one-layer 2D array: the forward pass's AO texture (binding
    /// 17, both layers of the SSAO target) when SSAO is off.
    white_array_view: wgpu::TextureView,
    flat_view: wgpu::TextureView,
    /// Mid-grey detail albedo (128) so a missing map does not tint.
    mid_view: wgpu::TextureView,
    /// Height 255 / curvature 128, a no-op parallax and flat wear sample.
    aux_view: wgpu::TextureView,
    /// Depth-only pass that clears one shadow page to far.
    page_clear: wgpu::RenderPipeline,
    /// Translucent shadow casters' pipelines (colored shadow map, `translucent_shadow`).
    translucent: translucent_shadow::TranslucentPipes,
    sets: HashMap<PipelineKey, PipelineSet>,
    /// The lite forward path's modules and pipelines (ADR-1874).
    lite: forward_lite_gpu::LiteCache,
}

struct IblGpu {
    irradiance_tex: wgpu::Texture,
    prefilter_tex: wgpu::Texture,
    irradiance: wgpu::TextureView,
    prefilter: wgpu::TextureView,
    brdf: wgpu::TextureView,
    sampler: wgpu::Sampler,
    /// Highest prefilter mip level (sampled as the roughness-1 lookup).
    prefilter_mips: f32,
    /// [`crate::ibl::IblLower::plane_feed`], when this IBL takes a ground plane.
    plane_feed: Option<[f32; 3]>,
}

struct MeshEntry {
    /// Vertex buffer: a mesh-arena chunk shared with other meshes, or the mesh's own buffer.
    vb: wgpu::Buffer,
    /// Index buffer (same).
    ib: wgpu::Buffer,
    /// First vertex of the mesh in `vb` (the draw's `base_vertex`).
    base_vertex: i32,
    /// First index of the mesh in `ib`.
    first_index: u32,
    /// The arena ranges `vb`/`ib` hold, shared with a dithered twin entry; the last holder to
    /// drop returns them to the arenas (`None`: a dedicated buffer).
    leases: [Option<std::sync::Arc<crate::mesh_storage::ArenaLease>>; 2],
    /// Second UV stream (vertex buffer slot 1), uploaded when a graph that reads it draws the
    /// mesh: the mesh's [`FrameMesh::uv1`], or its first set when it has none.
    uv1: Option<wgpu::Buffer>,
    index_count: u32,
    /// Vertex/index byte counts, to re-upload when the source data changes under a stable key.
    vertex_bytes: usize,
    index_bytes: usize,
    /// Rest-pose object-space sphere. Stored for every mesh that has vertices. A static
    /// draw (rigid, or a folded rigid skin) culls with it; a skinned draw ignores it.
    bounds: Option<MeshBounds>,
    /// A static draw has visited this mesh. Set on upload when the visit is rigid or a
    /// folded one-joint skin, and set again when a later visit is. A skinned visit does
    /// not clear it. Sun tracing uses it to choose a rest-pose acceleration structure.
    static_bound: bool,
    /// Object-space metres per UV unit ([`crate::texture_stream::uv_span`]).
    uv_span: f32,
    last_used: u64,
    /// Registered for dithered draws ([`FrameMesh::lod_dither`]): batches of it use the
    /// alpha-test pipeline.
    dithered: bool,
    /// Which upload this entry's geometry is ([`FrameRenderer::mesh_generation`]): a new one
    /// rebuilds its acceleration structure.
    generation: u64,
    /// [`crate::mesh_content_revision`] last written into these buffers. A later revision with
    /// the same byte size is rewritten in place; it does not count as a new upload.
    content_rev: u64,
}

impl MeshEntry {
    /// The draw's index range in [`Self::ib`].
    fn indices(&self) -> std::ops::Range<u32> {
        self.first_index..self.first_index + self.index_count
    }
}

/// The renderer's vertex and index arenas ([`crate::mesh_storage::GpuArena`]): cached meshes
/// are ranges in shared chunk buffers, so a mesh replacing an evicted one creates no buffer.
struct MeshArenas {
    vertices: crate::mesh_storage::GpuArena,
    indices: crate::mesh_storage::GpuArena,
}

impl MeshArenas {
    fn new(d: &wgpu::Device) -> Self {
        let sizes = crate::mesh_storage::ArenaSizes::for_platform(cfg!(target_os = "android"))
            .clamped(d.limits().max_buffer_size);
        let blas = crate::mesh_storage::blas_input_usage(d.features());
        Self {
            vertices: crate::mesh_storage::GpuArena::new(
                "mesh vertex arena",
                wgpu::BufferUsages::VERTEX | blas,
                std::mem::size_of::<LabVertex>() as u64,
                sizes,
            ),
            indices: crate::mesh_storage::GpuArena::new(
                "mesh index arena",
                wgpu::BufferUsages::INDEX | blas,
                std::mem::size_of::<u32>() as u64,
                sizes,
            ),
        }
    }
}

/// Where a cached material's constants live (`bindless`, ADR-316).
enum MaterialConsts {
    /// Its own buffer: the uniform path, and every graph material (constants and graph block).
    Own(wgpu::Buffer),
    /// A slot of [`FrameRenderer::material_arena`]: `(arena id, slot, arena epoch the bind
    /// group was built against)`.
    Slot(u64, crate::bindless::MaterialSlot, u32),
}

impl MaterialConsts {
    /// The binding of the material group's binding 0 (`None`: a stale arena slot).
    fn binding<'a>(
        &'a self,
        arena: Option<&'a crate::bindless::MaterialArena>,
    ) -> Option<wgpu::BindingResource<'a>> {
        match self {
            Self::Own(buf) => Some(buf.as_entire_binding()),
            Self::Slot(id, slot, _) => arena
                .filter(|arena| arena.id() == *id)
                .and_then(|arena| arena.binding(*slot))
                .map(wgpu::BindingResource::Buffer),
        }
    }

    /// Writes the standard constant block (offset 0) where it lives.
    fn write(
        &self,
        q: &crate::device::TrackedQueue,
        arena: Option<&mut crate::bindless::MaterialArena>,
        bytes: &[u8],
    ) {
        match self {
            Self::Own(buf) => q.write_buffer(buf, 0, bytes),
            Self::Slot(id, slot, _) => {
                if let Some(arena) = arena.filter(|arena| arena.id() == *id) {
                    arena.write(q, *slot, bytes);
                }
            }
        }
    }

    /// Returns an arena slot to the arena it came from (reusable [`crate::bindless::SLOT_RECYCLE_FRAMES`]
    /// after `frame`).
    fn release(&self, arena: Option<&mut crate::bindless::MaterialArena>, frame: u64) {
        if let Self::Slot(id, slot, _) = self
            && let Some(arena) = arena.filter(|arena| arena.id() == *id)
        {
            arena.free(*slot, frame);
        }
    }

    /// A slot of an arena other than `arena` (the entry must be rebuilt).
    fn foreign(&self, arena: Option<&crate::bindless::MaterialArena>) -> bool {
        match self {
            Self::Own(_) => false,
            Self::Slot(id, ..) => arena.is_none_or(|arena| arena.id() != *id),
        }
    }

    /// A slot whose bind group was built before `arena` grew into a new buffer.
    fn stale(&self, arena: &crate::bindless::MaterialArena) -> bool {
        matches!(self, Self::Slot(id, _, epoch) if *id == arena.id() && *epoch != arena.epoch())
    }
}

/// Rebuilds the bind group of every arena-slot material built before `arena` grew (its views
/// are kept for this), so no draw binds the old buffer after a growth. On the texture table
/// path every such material takes the one new shared record group instead (ADR-863).
fn regroup_materials(
    materials: &mut crate::key_hash::KeyMap<u64, MaterialEntry>,
    arena: &crate::bindless::MaterialArena,
    d: &wgpu::Device,
    pipes: &Pipes,
    record_group: &mut RecordGroup,
) {
    let _span = tracing::info_span!("material_regroup").entered();
    for entry in materials.values_mut() {
        if !entry.consts.stale(arena) {
            continue;
        }
        if entry.table.is_some() {
            entry.bg = record_group.get(arena, d, pipes);
        } else if let Some(resource) = entry.consts.binding(Some(arena))
            && let Some(views) = entry.views.as_deref()
        {
            entry.bg = material_bind_group(d, pipes, resource, views);
        }
        if let MaterialConsts::Slot(_, _, epoch) = &mut entry.consts {
            *epoch = arena.epoch();
        }
    }
}

struct MaterialEntry {
    /// Parameter block (rewritten when the parameters change under a stable key).
    consts: MaterialConsts,
    bg: wgpu::BindGroup,
    params: GpuMaterial,
    /// [`crate::SurfaceGraph::key`] of a graph material (its pipelines come from
    /// `Pipes::graph_sets`); `None` for the standard surface.
    graph_key: Option<u64>,
    /// The graph parameter block last seen (it keys the material epoch's clock-only bump).
    graph_params: Option<[[f32; 4]; crate::surface_graph::GRAPH_PARAM_SLOTS]>,
    /// The graph parameter block last written after `params`: what the GPU holds. Under a live
    /// clock a slot-0-only change is seen but not written, so this can differ from
    /// `graph_params` in slot 0 (corr-render-frame-upscaler LOW 10).
    graph_uploaded: Option<[[f32; 4]; crate::surface_graph::GRAPH_PARAM_SLOTS]>,
    /// Mip residency of a streamed standard material ([`crate::texture_stream`]).
    stream: Option<Box<crate::texture_stream::MaterialStream>>,
    /// The bound views (bind order 1, 3..=8) of a streamed material or of an arena-slot one, to
    /// rebuild `bg` (a residency swap, the arena growing).
    views: Option<Box<[wgpu::TextureView; 7]>>,
    /// The table indices `views` hold on the texture table path (`texture_table`, ADR-863): the
    /// record names them and `bg` is the shared record group. `None` for graph materials and off
    /// the table path.
    table: Option<crate::texture_table::TableIndices>,
    /// The dynamic offset of this material's record in the shared record group (`bg`) on the
    /// texture table path (ADR-863); `None` everywhere else (the group binds its own record).
    /// Stable for the slot's life: the arena growing moves no slot.
    dyn_offset: Option<u32>,
    /// Textures uploaded whole (outside the stream), by slot name, for the texture report.
    /// Shared and dynamic textures are not listed here; the residency cache reports them once.
    fixed: Vec<(&'static str, wgpu::Texture)>,
    /// Image-identity keys this entry holds. Released with the entry.
    shares: Vec<crate::texture_residency::ShareKey>,
    /// Allocations mixed into this entry's key. Dropped with the entry, which is what keeps a
    /// recycled address from hashing to the same bind group (ADR-179).
    pinned: Vec<crate::texture_residency::Owner>,
    /// The material's asset name ([`crate::name_material_textures`]).
    name: Option<std::sync::Arc<str>>,
    last_used: u64,
    /// Key of the views a standard material's group was built with (`texture_sets`, ADR-356);
    /// `None` for a graph material. A streamed material's census reads `views` instead.
    texture_set: Option<u64>,
    /// The thin film's group 3 (`thin_film_gpu`), only on a material that draws the film.
    film: Option<thin_film_gpu::FilmBinding>,
    /// [`crate::texture_residency::counted_stamp`] of the material when the entry was built: the
    /// counted textures it binds and their upload serials (0: none). A new serial (a hot reload,
    /// the first upload after a load) rebuilds the entry under the same key (ADR-8610).
    counted: u64,
}

impl MaterialEntry {
    /// The dynamic offsets to bind `bg` with: this material's record offset on the texture table
    /// path, none elsewhere (ADR-863).
    fn offsets(&self) -> &[u32] {
        self.dyn_offset.as_slice()
    }
}

/// The one material record group of the texture table path (ADR-863): the arena's buffer at a
/// dynamic offset, built once per arena buffer and shared by every standard material, so a new
/// material creates no bind group.
#[derive(Default)]
struct RecordGroup {
    /// `(arena id, arena epoch, group)` of the last build.
    cached: Option<(u64, u32, wgpu::BindGroup)>,
    /// Groups built (the first one included): one per arena buffer.
    built: u64,
}

impl RecordGroup {
    /// The group over `arena`'s current buffer, built when the arena is new or has grown.
    fn get(
        &mut self,
        arena: &crate::bindless::MaterialArena,
        d: &wgpu::Device,
        pipes: &Pipes,
    ) -> wgpu::BindGroup {
        if let Some((id, epoch, group)) = self.cached.as_ref()
            && *id == arena.id()
            && *epoch == arena.epoch()
        {
            return group.clone();
        }
        // Counted as a frame-path create (`frame_stats`): it happens on a material upload.
        let group = crate::device::TrackedDevice::observe(d).create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("material record bg"),
            layout: &pipes.mat_bgl,
            entries: &[binding(0, wgpu::BindingResource::Buffer(arena.record_binding()))],
        });
        self.built += 1;
        self.cached = Some((arena.id(), arena.epoch(), group.clone()));
        group
    }
}

/// Slot names of a graph material's textures in the texture report.
const GRAPH_SLOT_NAMES: [&str; 8] = [
    "graph0", "graph1", "graph2", "graph3", "graph4", "graph5", "graph6", "graph7",
];

/// Slot names of the streamed slots ([`crate::texture_stream::STREAMED_SLOTS`] order).
const STREAM_SLOT_NAMES: [&str; crate::texture_stream::STREAMED_SLOTS] =
    ["base_color", "normal", "orm", "emissive"];

/// One draw of `FrameRenderer::render_into`'s frame: the mesh, material and object entries it
/// draws with and its per-frame state (hoisted out of `render_into` so pass modules can name it,
/// ADR-1552).
struct Draw {
    /// Drawn from its morphed copy (`morph_gpu`): its geometry follows the weights.
    morphed: bool,
    mesh: u64,
    material: u64,
    object: u64,
    sort_depth: f32,
    alpha_mode: u32,
    /// Transmissive (`LabMaterial::transmission` > 0): drawn in two blends.
    transmissive: bool,
    /// Transmissive with a volume thickness: bends the background when screen-space
    /// refraction is on (a third blend between the two).
    refracts: bool,
    model: [[f32; 4]; 4],
    joint_base: u32,
    /// Joints of this draw's palette from `joint_base` (0 = rigid or folded).
    joint_count: u32,
    visible: bool,
    /// Rigid mesh (safe bounding sphere from the mesh entry).
    rigid: bool,
    /// Vertex-shader-skinned draw that joins the GPU-driven batches (ADR-1368): its
    /// joint-palette world sphere ([`Self::skin_sphere`]) is the cull bound. `None` for
    /// rigid draws, twins, unbounded skins and frames without GPU objects.
    skin_sphere: Option<([f32; 3], f32)>,
    /// Vertex-shader-skinned draw with a joint-palette bound: that bound's model-space sphere,
    /// the shadow caches' caster bound (ADR-5418; a rigid draw's is its mesh entry's). `None`
    /// for rigid draws, compute twins and unbounded skins (those redraw whole cascades).
    skin_bound: Option<MeshBounds>,
    /// Explicit decal receiver layer. `0` infers static vs character from [`Self::rigid`].
    receiver_layer: u32,
    /// Shadow policy of the mesh.
    shadow: ShadowCast,
    /// Index in the GPU indirect buffer, when this draw was uploaded to the culler.
    indirect: Option<u32>,
    /// This draw still needs a per-object uniform (skinned, blended, or no bound).
    cpu_uniform: bool,
    /// Drawn by the visibility-buffer path instead of the depth prepass and forward pass.
    vis: bool,
    /// A masked (alpha-tested) draw on the visibility path (ADR-3915): its clusters draw
    /// alpha-tested, and it keeps its forward shadow draws, which alpha-test too.
    vis_masked: bool,
    /// Index of this draw in the visibility buffer's instance list.
    vis_instance: Option<usize>,
    /// Material-graph code key whose pipeline set is ready. The set is
    /// `(key, cull_off)` in `graph_sets`. `None` for the standard surface, and for a
    /// graph still building (drawn with the standard surface).
    graph: Option<u64>,
    /// The graph has a vertex stage: shadows and motion vectors use its pipelines.
    graph_vertex: bool,
    /// The vertex stage reads the material clock (wind sway), or a bounded skinned pose
    /// ([`Self::skin_bound`], ADR-5418): a dynamic shadow caster whose geometry moves without its
    /// bounds (ADR-2028).
    animated: bool,
    /// Eligible for a static batch ([`crate::static_batch_eligible`]): the entity carries
    /// `regia_scene::components::Static` and the draw is rigid and whole (`specs/14` A1).
    /// Counted per GPU batch, not part of its key (ADR-5906).
    is_static: bool,
    /// Bound growth for a vertex-stage graph's largest offset, metres.
    inflate: f32,
    /// The vertex-stage graph declares its largest offset (`max_vertex_offset` > 0, as a wind
    /// sway does), so `inflate` bounds its vertices: the two-phase culls test it on that grown
    /// bound, as the colour pass's cull does (ADR-5389). `false` without a vertex stage.
    vertex_offset_bounded: bool,
    /// [`FrameMesh::lod_dither`].
    lod_dither: f32,
    /// Index in [`FrameRenderer::glass`] of this draw's glass instance (standard
    /// transmissive surfaces only).
    glass: Option<usize>,
    /// Double-sided material in a frame that culls back faces: colour, depth, shadows and
    /// motion vectors use the cull-none pipelines (a material graph uses its cull-none set,
    /// `(key, true)` in `graph_sets`). On the GPU-driven path the cull mode is part of the
    /// instanced batch key, so these batch and instance like culled draws.
    cull_off: bool,
    /// Render override lanes for the object block's `skin.zw` (`[0, 0]` = none).
    over: [u32; 2],
    /// X-ray override: the blend pass draws it with the depth test off.
    xray: bool,
    /// Masked instance (`mask_decals`): drawn from the CPU colour loop with the
    /// `MASK_DECALS` pipeline and its group 3. Cleared when that variant is not built yet.
    mask: bool,
    /// Hair card material (`crate::hair`): it casts its shadow through the alpha test of
    /// the translucency maps, never as an opaque card outline.
    hair: bool,
    /// Lightmap receiver (`lightmap_forward`, ADR-1623): its entry in the installed lightmap. Drawn
    /// from the CPU colour loop with the `LIGHTMAP` pipeline, its group 3 and its UV2 stream.
    /// Cleared when that variant is not built yet.
    lightmap: Option<u32>,
    /// Thin film (`thin_film_gpu`, ADR-1977): drawn from the CPU colour loop with the `THIN_FILM`
    /// pipeline and its material's group 3. Cleared when that variant is not built yet.
    film: bool,
    /// Subsurface scattering (`subsurface_gpu`, ADR-2049): drawn from its own pass, ahead of the
    /// forward pass; the forward loop skips it. Cleared when the frame cannot scatter.
    subsurface: bool,
    /// Viewer-inside glass seen by a camera clearly outside it (`canopy_view`): its own
    /// light takes the full shader whatever the canopy setting.
    canopy_outside: bool,
    /// Index of the submitted mesh in `FrameScene::meshes` (the masked occlusion cull reads its
    /// CPU vertices, ADR-10072).
    src: usize,
}

/// Cache key of one per-object uniform: [`FrameMesh::object_key`] and the side of `lod_dither`
/// (whole, incoming or outgoing; `scratch::object_uniform_key`).
///
/// Built by `scratch::object_uniform_key`. A cross-fade pair shares the object key and
/// differs in the side, so the two copies do not share `skin.y`.
pub(super) type ObjectUniformKey = (u64, u32);

impl Draw {
    /// This draw's [`ObjectUniformKey`].
    fn uniform_key(&self) -> ObjectUniformKey {
        scratch::object_uniform_key(self.object, self.lod_dither.to_bits())
    }

    /// The per-object uniform this draw binds (model, joint base, dither bits, override lanes).
    fn object_data(&self) -> ObjectData {
        ObjectData {
            model: self.model,
            skin: [
                self.joint_base,
                self.lod_dither.to_bits(),
                self.over[0],
                self.over[1],
            ],
        }
    }
}

struct ObjectEntry {
    buf: wgpu::Buffer,
    bg: wgpu::BindGroup,
    /// The skin palette generation `bg` binds (a grown palette rebinds it on next use).
    skin_generation: u64,
    last_used: u64,
    /// Last uniform uploaded. A static object skips `write_buffer`.
    uploaded: Option<ObjectData>,
}

/// A held view may reuse its screen-space composite once the probes' scene has been
/// unchanged this many frames (every probe re-traced each frame for its first 8 updates).
const DDGI_REUSE_FRAMES: u32 = 8;

// ---------------------------------------------------------------------------
// FrameRenderer
// ---------------------------------------------------------------------------

/// History weight of the temporal resolve after `held` frames with an unchanged view:
/// 0.9 in motion, then a running mean (`(n + 9) / (n + 10)`, the moving history counted
/// as ten frames) so the converged still averages every jitter of the hold equally instead
/// of weighting the last ten (Yang, Liu & Salvi, JCGT 2020 § 3.3, accumulation).
fn held_history_weight(held: u32) -> f32 {
    let n = held.min(HELD_CONVERGE_FRAMES) as f32;
    ((n + 9.0) / (n + 10.0)).min(crate::taau::MAX_HISTORY_WEIGHT)
}

/// Identity of a static screen-space frame (SSGI / SSR / AO).
///
/// Floats are stored as bits so a NaN cannot fail the comparison.
#[derive(Clone, Copy, PartialEq, Eq)]
struct ScreenCacheKey {
    vp: [[u32; 4]; 4],
    sun_dir: [u32; 3],
    sun_lux: u32,
    exposure: u32,
    /// The HDR target's pre-exposure: a cached composite holds radiance × this scale.
    pre_exposure: u32,
    ambient: u32,
    draws: u64,
    width: u32,
    height: u32,
    flags: u32,
    /// Hash of the outdoor (sky, time of day, clouds, fog) parameters: the
    /// forward pass samples that sky's IBL, so a new sky must not reuse.
    outdoor: u64,
    /// The physical background is now part of the cached forward image: track installed
    /// tables, their served sun direction, and the star-flux switch as well as host inputs.
    sky: Option<(u64, [u32; 3], bool)>,
    /// [`crate::SharedPassDraw::content_key`] of the shared draw (terrain heights,
    /// contact map): content the draw list cannot see must not reuse either.
    shared: u64,
}

fn fbits(row: [f32; 4]) -> [u32; 4] {
    [
        row[0].to_bits(),
        row[1].to_bits(),
        row[2].to_bits(),
        row[3].to_bits(),
    ]
}

/// Installs made through a renderer's setters since it was built
/// ([`FrameRenderer::absorb_stand_in`]).
#[derive(Clone, Copy, Debug, Default)]
struct InstalledSince {
    gi: bool,
    probes: bool,
    interior: bool,
    ddgi_settings: bool,
    ddgi_enabled: bool,
    baked_gi: bool,
    /// Largest [`FrameRenderer::reserve_instances`] request.
    reserved: u32,
    /// [`FrameRenderer::set_level_material_graphs`].
    level_graphs: bool,
    /// [`FrameRenderer::set_warm_decals`].
    warm_decals: bool,
    /// A lightmap call ([`FrameRenderer::set_lightmap`] and its neighbors).
    lightmap: bool,
    /// [`FrameRenderer::set_warm_probe_capture`].
    warm_probe_capture: bool,
    /// [`FrameRenderer::set_permutation_guard`] (ADR-2026).
    permutation_guard: bool,
    /// [`FrameRenderer::set_aerial_interiors`].
    aerial_interiors: bool,
}

/// Persistent clustered-forward renderer. Owns every GPU resource a frame needs;
/// create one, keep it alive (a resource in the app), and render
/// [`FrameScene`]s into windows or offscreen targets.
///
/// Caches are keyed by the frame's stable keys ([`FrameMesh`]); meshes and materials whose keys
/// stop appearing stay resident while the caches fit their budget ([`CacheResidency`]).
#[derive(Default)]
pub struct FrameRenderer {
    weather: weather::FrameWeather,
    frame_index: u64,
    /// The [`signal_memory_pressure`] generation this renderer last trimmed for.
    memory_pressure_seen: u64,
    pipes: Option<Pipes>,
    /// Test-only override of the material sampler's anisotropy clamp (`None` = [`MATERIAL_ANISOTROPY`]).
    material_anisotropy: Option<u16>,
    ibl: Option<IblGpu>,
    /// Bumped every time `ibl` is replaced, so cached view groups that bind the old maps are
    /// dropped (ADR-1654).
    ibl_generation: u64,
    /// The split-sum DFG table (`crate::ibl::build_brdf_lut`) bound as `brdf_lut` in frames
    /// without the IBL. The table depends on the BRDF only, not on the environment: the
    /// multi-scatter energy compensation reads it for direct light too, and a white
    /// stand-in there zeroed the diffuse lobe of every multi-scatter material (ADR-359).
    brdf_lut: Option<wgpu::TextureView>,
    /// The IBL is a reference studio's environment, shown as the background too.
    studio_background: bool,
    /// Cache key of the gradient or equirect studio environment the IBL was built from
    /// (`StudioGradient::cache_key`, `StudioEquirect::cache_key`); `None` for every other IBL.
    studio_env_key: Option<u64>,
    /// The studio frame's settings (ADR-2000).
    studio: studio_state::StudioState,
    /// The extra directional lights, ambient volumes and shader debug view of the following frames,
    /// as the frame uniform's tail carries them (ADR-7211, [`Self::set_lights_views`]).
    lights_views: crate::LightsViewsUniform,
    /// Shadow targets, created at the size/count the first shadowed frame needs.
    csm: Option<(u32, u32, Vec<wgpu::TextureView>)>,
    atlas: Option<(u32, wgpu::TextureView)>,
    // Fixed-size per-frame buffers.
    frame_buf: Option<wgpu::Buffer>,
    dir_buf: Option<wgpu::Buffer>,
    lights_buf: Option<wgpu::Buffer>,
    counts_buf: Option<wgpu::Buffer>,
    indices_buf: Option<wgpu::Buffer>,
    /// Lights per cluster `indices_buf` has room for ([`crate::cluster_index_capacity`], ADR-3351).
    cluster_indices_cap: u32,
    /// Bumped each time `indices_buf` is replaced by a larger one.
    cluster_indices_gen: u64,
    csm_buf: Option<wgpu::Buffer>,
    tiles_buf: Option<wgpu::Buffer>,
    skin_buf: Option<wgpu::Buffer>,
    skin_capacity: usize,
    /// Generation of the skin buffer; bumped on growth (invalidates object bind groups).
    skin_generation: u64,
    tile_vp_bufs: Vec<wgpu::Buffer>,
    tile_vp_bgs: Vec<wgpu::BindGroup>,
    // Caches.
    meshes: crate::key_hash::KeyMap<u64, MeshEntry>,
    materials: crate::key_hash::KeyMap<u64, MaterialEntry>,
    /// Standard materials' constants on the storage-buffer path (`Pipes::material_ssbo`; created
    /// with the first such material, ADR-316).
    material_arena: Option<crate::bindless::MaterialArena>,
    /// Every standard material's textures on the texture table path (`Pipes::table_bgl`;
    /// created with the first such material, ADR-863).
    texture_table: Option<crate::texture_table::TextureTable>,
    /// The shared material record group of the texture table path (ADR-863).
    record_group: RecordGroup,
    /// Encoder calls of the last main-view frame's draw loops ([`Self::draw_counts`]).
    draw_counts: crate::frame_stats::FrameDrawCounts,
    /// GPU textures shared by image identity (graph slots, packed maps, dynamic displays).
    shared_textures: crate::texture_residency::ResidentTextures,
    /// Material-graph pipeline sets by (target, [`crate::SurfaceGraph::key`]), built inline or
    /// on workers (`graph_pipelines`).
    graph_cache: crate::graph_pipelines::GraphPipelineCache,
    /// Layouts graph pipelines are built against (created once with the pipelines).
    graph_layouts: Option<crate::graph_pipelines::GraphLayouts>,
    /// Material clock: (this frame's, last frame's) seconds. `None` keeps what each material
    /// carries in slot 0 (labs, goldens). When this or [`Self::foliage_wind`] is set, time-reading
    /// graphs read the frame clock instead, and a slot-0-only change does not upload (ADR-5160).
    material_time: Option<(f32, f32)>,
    /// Graph-block uploads (`ensure_material`). A live clock does not count a slot-0-only change.
    #[cfg(test)]
    pub(super) graph_block_writes: u64,
    /// Times `stream_materials` summed resident texture bytes. One scan a frame.
    #[cfg(test)]
    pub(super) resident_scans: u64,
    /// Scene wind (m/s on world X and Z) written into time-reading graphs' slot 0 `zw`
    /// ([`FrameRenderer::set_foliage_wind`]). `None` with no material clock keeps what the
    /// material carries; `None` while [`Self::material_time`] is set writes 0 wind (ADR-5160).
    foliage_wind: Option<[f32; 2]>,
    /// The foliage wind above came from a `SceneWind` ([`crate::OutdoorParams::scene_wind`]). The
    /// terrain's wind already follows that resource, so shared draws do not take it again
    /// (ADR-2043).
    foliage_wind_from_scene: bool,
    /// [`FrameRenderer::set_dynamic_caster_shadows`] (ADR-2028).
    dynamic_casters: crate::foliage_wind::DynamicCasterShadows,
    objects: crate::key_hash::KeyMap<ObjectUniformKey, ObjectEntry>,
    /// Mesh and material keys [`FrameRenderer::prewarm`] made resident or the host pinned
    /// ([`FrameRenderer::pin_keys`]): never evicted.
    pinned: std::collections::HashSet<u64>,
    /// When idle meshes and materials are evicted.
    residency: CacheResidency,
    /// Upload/eviction counters and the last sweep's totals ([`FrameRenderer::cache_stats`]).
    cache_stats: CacheStats,
    /// Per mesh key: whether its vertices are bound rigidly to joint 0 (the one-joint rigid
    /// skin fold), with the vertex count it was checked on.
    rigid_skin: crate::key_hash::KeyMap<u64, (usize, bool)>,
    /// The render-world entry lifecycle: the frame's draw window, removals deferred to its
    /// close, the skip and rebuild counters ([`Self::draw_integrity`], W2.2).
    entries: lifecycle::EntryLifecycle,
    /// Joint-palette bounds of skinned draws ([`Self::set_skinned_bounds`]).
    skin_bounds: skin_bounds::SkinBoundsCache,
    /// The CPU masked occlusion cull of the main view ([`Self::set_masked_occlusion`], ADR-10072).
    masked_cull: masked_cull::MaskedCull,
    /// Vertex and index arenas of [`Self::meshes`] (created with the first mesh).
    mesh_arenas: Option<MeshArenas>,
    /// Mesh bytes this frame copied through the staging belt ([`MESH_BELT_FRAME_BYTES`]).
    mesh_belt_bytes: u64,
    /// Mesh keys whose upload waits in the staging belt for this frame's submit. A frame that
    /// returns before it leaves them here; the next frame drops those entries so they upload
    /// again (the belt discards the copies of an unsubmitted frame).
    mesh_belt_keys: Vec<u64>,
    /// The next eviction sweep runs at the end of this frame and trims idle entries whatever
    /// the budget ([`Self::trim_memory`]).
    cache_trim_pending: bool,
    /// The adapter's device-local VRAM in MiB, read once at the first frame (`Some(None)`:
    /// unknown).
    adapter_vram_mib: Option<Option<u64>>,
    /// The adapter is a discrete GPU (read with [`Self::adapter_vram_mib`]).
    adapter_discrete: bool,
    /// The adapter gets the integrated-GPU streaming budget ([`crate::low_memory`]; read with
    /// [`Self::adapter_vram_mib`]).
    adapter_integrated: bool,
    /// The streaming budget stays the preset's whatever the VRAM
    /// ([`Self::set_vram_scaled_budgets`]).
    vram_budgets_off: bool,
    /// `(frame, time)` at recent eviction sweeps: the derived budgets keep entries drawn
    /// within [`CACHE_MIN_IDLE_SECS`] (the first sample at least that old is the cutoff).
    sweep_clock: std::collections::VecDeque<(u64, std::time::Instant)>,
    /// The last sweep's split of the cache bytes and budgets ([`Self::cache_budgets`]).
    cache_budgets: CacheBudgets,
    /// Test hook: derived `(mesh, material)` budgets and seconds floor instead of the
    /// adapter-derived ones and [`CACHE_MIN_IDLE_SECS`].
    #[cfg(test)]
    derived_override: Option<(u64, u64, f32)>,
    /// Some resident material may stream its textures (exact after each sweep; set when a
    /// streamed material is created): skips the per-frame streaming pass without a scan.
    may_stream: bool,
    /// One-joint rigid skins keep the per-object skinned path ([`Self::set_rigid_skin_fold`]).
    rigid_skin_fold_off: bool,
    /// The per-frame load budget, its plan and counters ([`Self::set_load_budget`], ADR-7884).
    load_budget: crate::load_budget::LoadBudgetState,
    /// Morphed instances and their deformed copies ([`Self::set_morph_instances`]).
    morph: morph_gpu::MorphState,
    targets: HashMap<(u32, u32), TargetPair>,
    post: Option<crate::post_pass::PostChain>,
    /// Motion blur and depth of field before the post stack ([`crate::lens`]).
    lens: crate::lens::LensPasses,
    /// HDR display stage: the target is linear in paper-white units and the post blit expands
    /// highlights up to this headroom (`None`: the target takes the display-encoded image).
    display_headroom: Option<f32>,
    view_bgs: crate::bind_cache::BindCache<(u64, u64)>,
    // Translucent effect layers (VFX; see `translucent`).
    translucent: Vec<std::sync::Arc<dyn crate::translucent::TranslucentLayer>>,
    /// Layer + world-normal receiver, and the scene-colour copy decals composite against.
    decal_receiver: crate::decal_receiver::DecalReceiver,
    /// Offscreen render-to-texture cameras, keyed by the camera's asset id.
    texture_targets: HashMap<u64, OffscreenTarget>,
    // Timing.
    gpu_timer: Option<GpuTimer>,
    /// Staging slot copied by the scene encoder, mapped after that submit.
    pending_timer_slot: Option<usize>,
    last_timings: GpuTimings,
    last_cpu: FrameCpuTimings,
    /// Atmosphere, froxel fog and volumetric clouds.
    outdoor: crate::outdoor::OutdoorState,
    /// The HDR targets this renderer's frames could composite the outdoor passes into, with
    /// the profile's froxel fog and clouds switches: what frame warms build the outdoor
    /// pipelines for (`OutdoorState::prewarm`), whatever the map shows.
    outdoor_targets: Vec<(wgpu::TextureFormat, bool, bool)>,
    /// The IBL textures currently hold the atmosphere bake.
    ibl_from_atmosphere: bool,
    /// Inside [`FrameRenderer::capture_reflection_probe`]: the frames are probe faces.
    capturing_probe: bool,
    // Culling.
    last_draw_stats: DrawStats,
    /// Shadow casters (opaque, shadow-casting meshes, culled ones included) in the last frame.
    last_shadow_casters: usize,
    /// GPU frustum + Hi-Z + indirect commands.
    culler: crate::gpu_driven::GpuCuller,
    /// The GPU cull skips its Hi-Z occlusion test ([`Self::set_hiz_occlusion`], ADR-1402).
    hiz_occlusion_off: bool,
    /// Two-phase occlusion of the depth prepass ([`Self::set_two_phase_occlusion`], ADR-1887).
    prepass_occlusion: crate::prepass_occlusion::PrepassOcclusion,
    /// Two-phase occlusion of the shadow casters ([`Self::set_shadow_occlusion`], ADR-2072).
    shadow_occlusion: crate::shadow_occlusion::ShadowOcclusion,
    /// The post stack waits for due exposure readbacks ([`Self::set_wait_for_exposure_readback`]).
    wait_exposure_readback: bool,
    /// Complete frames for an offline capture ([`Self::set_complete_frames`], ADR-2582): what the
    /// switch changed, to put back when it ends.
    complete_frames: Option<settings::CompleteSaved>,
    /// Masked materials take the hashed alpha test on temporal-AA frames too
    /// ([`Self::set_taa_hashed_alpha`]).
    taa_hashed_alpha: bool,
    /// Masked casters are alpha-tested in the instanced shadow passes
    /// ([`Self::set_alpha_tested_shadows`]).
    alpha_tested_shadows: bool,
    /// Off: the GPU-driven colour variants compile after the forward set, not alongside it
    /// (tests compare both orders; `REGIA_FORWARD_OVERLAP=0` does the same at run time).
    serial_forward_families: bool,
    /// [`Self::set_forward_lite`]: `None` follows the profile and the adapter.
    forward_lite_override: Option<crate::forward_lite::ForwardLite>,
    /// [`Self::set_output_dither`]: dither the 8-bit display image (off by default, ADR-2486).
    output_dither: bool,
    /// [`Self::set_sun_shadow_range`]: the farthest view distance the sun cascades reach (`None`: the tier's).
    sun_shadow_range: Option<f32>,
    /// GPU-driven colour variants the forward pass had to build itself (the overlap with the
    /// forward set did not predict them). Tests hold it at 0.
    late_gpu_color_builds: u64,
    /// Opt-in visibility-buffer + cluster-LOD path (`RenderProfile::visibility_buffer`).
    visbuffer: crate::visbuffer::VisBufferPass,
    /// Last frame drew visibility-buffer shadows as clusters (a change redraws cached maps).
    vis_shadow_last: bool,
    /// The snapped eye persistent sets last took their GPU cascade choice from.
    vis_set_eye: Option<[f32; 3]>,
    /// Screen-space occlusion target.
    ssao_pass: crate::ssao::SsaoPass,
    /// Screen-space contact shadows on the sun.
    contact_pass: crate::contact_shadows::ContactShadowPass,
    /// The sun-visibility trace (`RenderProfile::sun_shadow_trace`, ADR-2035): every part is
    /// empty until a profile asks for it.
    sun_trace: sun_trace::SunTraceState,
    /// The blue-noise table and whether this frame uses it (ADR-2075).
    blue_noise: crate::blue_noise::BlueNoiseState,
    /// Stochastic many-light sampling (`RenderProfile::many_light_sampling`, ADR-2652): empty
    /// until a profile asks.
    many_light: many_light::ManyLightState,
    /// The host's forward decals and atlas (ADR-3255); staged with each view's frame uniform.
    forward_decal_host: crate::forward_decals::ForwardDecalHost,
    /// ReSTIR GI (empty until `RenderProfile::restir_gi` asks on a capable device).
    restir_gi: restir_gi::RestirGiState,
    /// Mesh geometry uploads so far: each [`MeshEntry`] is stamped with its own.
    mesh_generation: u64,
    /// Temporal upscaler and motion vectors.
    taau_pass: crate::taau::TaauPass,
    /// Previous model matrix per object key (velocity).
    prev_models: crate::key_hash::KeyMap<u64, [[f32; 4]; 4]>,
    /// Previous joint palette per skinned object key (velocity): `(first joint, count)` in
    /// [`Self::prev_joints`]. Same lifetime as [`Self::prev_models`].
    prev_palettes: crate::key_hash::KeyMap<u64, (u32, u32)>,
    /// Last frame's packed skin palette (the joints `prev_palettes` index).
    prev_joints: Vec<JointMatrix>,
    prev_view_proj: [[f32; 4]; 4],
    have_prev_vp: bool,
    /// TAAU history is valid for `taau_size`.
    taau_ready: bool,
    taau_size: (u32, u32),
    /// TSR-grade upscaler (`specs/27` § 2.3). Used instead of `taau_pass`'s resolve when
    /// [`Self::set_temporal_upscaler`] selects it.
    tsr_pass: crate::tsr::TsrPass,
    /// FSR 3.1 upscaler (TSR's comparison baseline, ADR-113). Used instead of `taau_pass`'s
    /// resolve when [`Self::set_temporal_upscaler`] selects it.
    fsr3_pass: crate::fsr3::Fsr3Pass,
    /// What frame generation reads from the temporal seam (ADR-2067; off until it asks).
    #[cfg(feature = "frame-gen")]
    frame_gen_capture: frame_gen_hook::FrameGenCapture,
    /// Which temporal upscaler resolves a TAA frame. TAAU unless a profile opts in.
    temporal_upscaler: crate::TemporalUpscaler,
    /// A host-selected vendor upscaler (ADR-2002), tried before `temporal_upscaler`.
    vendor_upscaler: crate::upscaler::vendor::dlss::FrameVendor,
    /// Material/terrain texture mip bias the last frame rendered with (0 unless TSR upscaled).
    texture_lod_bias: f32,
    /// Nearest sampler for the occlusion texture (and the 1×1 white fallback).
    ao_sampler: Option<wgpu::Sampler>,
    /// Cached sun shadow pages. Used when [`FrameQuality::paged_sun_shadows`] is on.
    sun_pages: crate::paged_shadow::SunPageCache,
    atlas_tiles: crate::paged_shadow::AtlasTileCache,
    /// The CSM textures hold a complete draw. False forces a full clear.
    csm_valid: bool,
    atlas_valid: bool,
    /// Previous frame had a caster with no bound (a skinned mesh). Dropping it
    /// redraws every cascade once so its shadow does not stick.
    had_unbounded_caster: bool,
    /// View-projection the near sun tile (ADR-164) was last drawn with; `None` = its atlas
    /// square holds nothing valid (not drawn, or skipped with no mesh in range).
    near_tile_vp: Option<[[f32; 4]; 4]>,
    shadow_report: ShadowCacheReport,
    /// Shadow CPU times of the last frame ([`Self::shadow_cpu_timings`]).
    shadow_cpu: ShadowCpuTimings,
    /// Cached per-page caster lists, so a held camera does not re-project every caster every frame.
    caster_list_cache: CasterListCache,
    /// The frame's reusable lists ([`Self::frame_scratch_stats`], ADR-385).
    scratch: scratch::FrameScratch,
    /// Local lights the last frame uploaded, and whether a cap dropped any.
    light_upload: crate::light_budget::LightUploadReport,
    /// This renderer's once-per-change light-cap warnings.
    light_warnings: crate::light_budget::LightDropWarnings,
    /// Area lights fitted to emissive meshes, per mesh key (`emissive_lights`).
    emissive_cache: crate::emissive_lights::EmissiveCache,
    /// IES profiles and authored rect and tube lights for the next frames (`light_profiles`, ADR-2592).
    light_extras: crate::light_profiles::LightExtras,
    /// What the last frame did with them.
    emissive_report: crate::emissive_lights::EmissiveLightReport,
    /// The GGX LTC table the forward pass shades area lights with (`ltc`), created once.
    ltc_view: Option<wgpu::TextureView>,
    /// Distance probes of the uploaded emissive lights (`emissive_lights::PROBE_RES`² texels
    /// per light, tile k = uploaded emissive light k) and the bytes last written into it.
    probe_atlas: Option<(wgpu::Texture, wgpu::TextureView)>,
    probe_atlas_bytes: Vec<u8>,
    gi_field: Option<crate::gi::GiField>,
    /// Key of the daylit interior field the renderer baked into [`Self::gi_field`];
    /// `None` when the field (if any) is the game's own.
    gi_auto: Option<u64>,
    /// Fallback box used when no authored volume contains the eye. Held while the eye
    /// stays inside it, so a step does not move the probe grid.
    gi_fallback: Option<crate::InteriorVolume>,
    /// Interior geometry for the daylit bake ([`Self::set_interior_scene`]) and its hash.
    interior_scene: Option<(u64, crate::SdfScene)>,
    gi_buf: Option<wgpu::Buffer>,
    gi_grid_buf: Option<wgpu::Buffer>,
    /// Shared draw's ground bounce for meshes (`GroundBounce`): one header vec4 per level
    /// (origin XZ, cell, size; size 0 = none), a horizon header, each level's `size`² cells
    /// in order, then (when the header says so) each level's horizon cells.
    ground_bounce_buf: Option<wgpu::Buffer>,
    ground_bounce_bytes: Vec<u8>,
    /// Large meshes' occlusion and bounce proxies ([`Self::set_mesh_occluders`]).
    mesh_occluders: Vec<crate::MeshOccluder>,
    /// The last frame's packed proxies (`lab_forward.wgsl` binding 23, the shared draw's
    /// [`crate::SharedFrameInfo::mesh_occluders`]).
    mesh_occluder_block: Box<crate::MeshOccluderBlock>,
    mesh_occluder_buf: Option<wgpu::Buffer>,
    mesh_occluder_bytes: Vec<u8>,
    /// Per fog-light slot: the last frame's fog tapped its spot shadow tile (ADR-1365).
    fog_light_shadowed: [bool; 4],
    /// What broke the last frame ([`Self::frame_problems`]).
    frame_problems: Vec<crate::host_checklist::FrameProblem>,
    /// Texture budget problems, refreshed every [`TEXTURE_PROBLEMS_EVERY`] frames and added to
    /// [`Self::frame_problems`].
    texture_problems: Vec<crate::host_checklist::FrameProblem>,
    /// Capability fallbacks of the current device ([`crate::RenderCapabilities`]), computed
    /// once per device and added to every frame's problems so `render.problems` and the host
    /// checklist report them (min-spec auditing, `specs/13` § 2).
    capability_problems: Vec<crate::host_checklist::FrameProblem>,
    /// [`crate::RenderDevice::device_id`] the `capability_problems` were computed for.
    capability_device: Option<u64>,
    /// Output and internal size and upscaler of the last view drawn ([`Self::last_extent`]).
    last_extent: Option<FrameExtent>,
    /// Totals of the texture report built with [`Self::texture_problems`].
    texture_summary: crate::texture_budget::TextureMemorySummary,
    reflection_probes: Vec<crate::reflections::ReflectionProbe>,
    /// World up for the next view. `[0, 0, 0]` is +Y. Probe capture sets one
    /// cubemap face at a time; ±Y cannot use world up (it is parallel to the look).
    camera_up: [f32; 3],
    /// CPU milliseconds of the last probe trace, recorded by the caller.
    gi_cpu_ms: f32,
    /// Installs made through the setters since this renderer was built. Read only on a
    /// game-warm stand-in: [`Self::absorb_stand_in`] moves what the host installed on it while
    /// the real renderer was on a warm worker.
    installed_since: InstalledSince,
    /// GPU-traced irradiance probes (`crate::ddgi`).
    ddgi: crate::ddgi_gpu::DdgiRuntime,
    /// The map's baked GI volume (`crate::baked_gi`).
    baked_gi: crate::baked_gi::BakedGiRuntime,
    /// [`crate::baked_gi::BakedGiRuntime::generation`] the cached view groups bind.
    baked_view_gen: u64,
    /// Resolution of the probe GI screen lookup ([`Self::set_gi_lookup_resolution`]).
    gi_lookup: crate::ddgi::GiLookupResolution,
    /// SSGI gather resolution override ([`Self::set_ssgi_half_res`]).
    ssgi_half_res: Option<bool>,
    /// The SSGI upsample's relative-depth variant ([`Self::set_ssgi_upsample_relative_depth`]).
    ssgi_relative_depth: bool,
    /// The world-space radiance cache behind SSGI (ADR-2085; [`Self::set_radiance_cache`]).
    radiance_cache: crate::radiance_cache::RadianceCacheRuntime,
    highend: crate::highend::HighEndPass,
    /// Terrain (or any other heightfield) drawn into this frame's targets.
    shared_draw: Option<Box<dyn crate::SharedPassDraw + Send + Sync>>,
    /// World position of the local origin, the sum of the rebases so far (`shift_history`).
    origin_offset: [f32; 3],
    /// The level's material graphs ([`Self::set_level_material_graphs`]): the frame warms
    /// start their sets for the targets the warm frame drew.
    level_graphs: Vec<std::sync::Arc<crate::SurfaceGraph>>,
    /// [`Self::set_warm_decals`]: frame warms build the decal receiver before an effect layer
    /// is installed.
    warm_decals: bool,
    /// [`Self::set_warm_probe_capture`]: frame warms render a reflection-probe face.
    warm_probe_capture: bool,
    /// [`Self::capture_reflection_probe`] ran on this renderer: its frame warms cover the next
    /// capture ([`Self::warm_probe_face`]).
    probe_captured: bool,
    /// [`Self::set_shared_draw`] was called on this renderer: a stand-in that took over for an
    /// abandoned warm keeps the caller's choice over the late warm's draw
    /// ([`Self::salvage_shared_draw`]).
    shared_draw_set: bool,
    /// Previous SSGI/SSR composite, reused when [`ScreenCacheKey`] matches.
    screen_cache_key: Option<ScreenCacheKey>,
    screen_cache_view: Option<wgpu::TextureView>,
    /// Consecutive frames whose [`ScreenCacheKey`] matched the cached one.
    screen_hold: u32,
    /// Bumped whenever a material's uniform is created or rewritten: a scalar edit keeps the
    /// material key (the uniform is rewritten in place), so the held-view key mixes this in.
    material_epoch: u64,
    /// Screen-space passes the last frame recorded ([`Self::screen_pass_report`]).
    screen_report: ScreenPassReport,
    /// Scene depth at the output size, for a translucent pass on a scaled or MSAA frame.
    depth_match: Option<DepthMatch>,
    /// Gizmo lines drawn over the next frames ([`Self::set_gizmos`]).
    gizmos: Option<std::sync::Arc<crate::gizmos::GizmoFrame>>,
    /// The gizmo pass's pipelines and buffers.
    gizmo_renderer: crate::gizmos::render::GizmoRenderer,
    /// Per-vertex scalar overlays by object key ([`Self::set_scalar_overlays`]).
    scalar_overlays: HashMap<u64, crate::ScalarOverlay>,
    /// The scalar overlay pass's pipelines and buffers.
    scalar_pass: crate::scalar_overlay_pass::ScalarOverlayPass,
    /// The view's transient textures, keyed by [`crate::TextureDesc`] (ADR-2018): the frame's
    /// HDR target, the opaque-frame copy refracting glass samples
    /// ([`FrameQuality::screen_space_refraction`]) with the glass front layer drawn with it (the
    /// nearest transmissive layer's depth and transmittance), and the SSAO targets. One pool frame per
    /// `render_into`; the same requests get the same textures every frame.
    transients: crate::TransientPool,
    /// The view's slot-keyed images (ADR-7877): the two HDR scratch images, each under its own
    /// key (`frame::hdr_scratch`). A pool of their own, so the frame pool above and its counters
    /// (`FrameGraphInfo::transients`) hold the frame transients only.
    keyed_transients: crate::TransientPool,
    /// Per cascade: the view-projection and [`crate::SharedPassDraw::shadow_key`]
    /// of the last shared draw into it. A match on a retained cascade skips the draw.
    shared_shadow_keys: [Option<([[f32; 4]; 4], u64)>; 4],
    /// Sun cascade placements kept between frames ([`crate::cascade_anchor`]).
    cascade_anchors: [Option<crate::cascade_anchor::CascadeAnchor>; 4],
    /// Kept centre of the near sun tile (`CascadeSticky::near_anchor`).
    near_anchor: Option<[f32; 3]>,
    /// Cascade passes that drew the shared geometry, since the renderer was built.
    shared_shadow_draws: u64,
    /// Cascade passes that redrew the shared geometry only under moved mesh pages.
    shared_shadow_region_draws: u64,
    /// Writes and bytes the shared draw queued on the last frame.
    shared_upload_last: (u32, u64),
    /// Hash of the last [`Self::set_outdoor`] parameters (screen cache key).
    outdoor_key: u64,
    /// Cluster bind group. The four buffers are created once.
    cluster_bg: crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::Buffer, wgpu::Buffer, wgpu::Buffer)>,
    /// Last bytes written to the per-frame uniforms. A static camera skips the upload.
    frame_bytes: Vec<u8>,
    gi_grid_bytes: Vec<u8>,
    gi_probe_bytes: Vec<u8>,
    dir_bytes: Vec<u8>,
    lights_bytes: Vec<u8>,
    csm_bytes: Vec<u8>,
    tiles_bytes: Vec<u8>,
    skin_bytes: Vec<u8>,
    /// The probe buffer was filled with zeros because no GI field is installed.
    gi_probes_zeroed: bool,
    /// Last `queue.submit` from [`Self::render_into`]. Not `Clone`; taken by the bench.
    last_submission: Option<wgpu::SubmissionIndex>,
    /// The next frame copies its linear HDR (the post stack's input) for [`Self::take_hdr_capture`].
    hdr_capture_requested: bool,
    /// The copy recorded by the last frame: buffer, width, height, padded row bytes, format.
    hdr_capture: Option<(HdrCopy, f32)>,
    /// Pre-exposure of the last linear HDR frame (0 before the first), for history rescale.
    prev_pre_exposure: f32,
    /// What each frame drew, for offline tools (`None` = off; [`Self::set_scene_export`]).
    scene_export: Option<crate::scene_export::SceneExporter>,
    /// Packed uploads for this frame. One `write_buffer` per chunk, folded into the frame submit.
    belt: crate::staging::StagingBelt,
    /// Material texture streaming: this frame's policy and upload budget.
    stream_frame: crate::texture_stream::StreamFrame,
    /// Last shadow view-projection written to each tile slot. A skipped cascade is left alone.
    tile_vp_bytes: Vec<Vec<u8>>,
    /// Threads that finish the frame's command-buffer segments while the next is recorded.
    finish_workers: crate::parallel_finish::FinishWorkers,
    /// Render-submission trims switch and the last frame's counters (ADR-508).
    submission: crate::submission::SubmissionState,
    /// Camera-cut requests, detection and the temporal sequences' origin ([`Self::camera_cut`]).
    cut: crate::camera_cut::CutState,
    /// Per-instance glass state for subsequent frames ([`Self::set_glass`]).
    glass: Vec<crate::glass::GlassInstance>,
    /// Hash of [`Self::glass`] (screen cache key; 0 without glass).
    glass_key: u64,
    /// Per-instance render overrides by object key, packed for the object block's `skin.zw`
    /// ([`Self::set_render_overrides`]).
    render_overrides: HashMap<u64, [u32; 2]>,
    /// Material shading models by material key, packed for the material uniform
    /// ([`Self::set_material_shading`]).
    material_shading: HashMap<u64, [[f32; 4]; crate::shading_model::SHADING_VEC4S]>,
    /// Compute skinning twins and options ([`Self::set_compute_skinning`], Tincal B2).
    skin_compute: crate::skin_compute::SkinCompute,
    /// This renderer's viewer-inside glass setting ([`Self::set_canopy_glass`]; `None` follows
    /// [`crate::glass::set_canopy_glass`]).
    canopy_glass: Option<crate::glass::CanopyGlass>,
    /// The last main-view frame's viewer-inside glass draws ([`Self::canopy_draws`]).
    canopy_draws: CanopyDraws,
    /// Glass parameter buffers and crack pages.
    glass_gpu: glass_gpu::GlassGpu,
    /// Masked instances (material mask recolor, decal slots) and their GPU entries
    /// ([`Self::set_mask_decals`]).
    mask_decals: mask_decals_gpu::MaskDecalState,
    /// The baked lightmap, its receivers and its GPU copy ([`Self::set_lightmap`],
    /// [`Self::set_lightmap_receivers`]).
    lightmap_forward: lightmap_forward::LightmapForwardState,
    /// The thin film's per-frame counts (ADR-1977).
    thin_film: thin_film_gpu::ThinFilmState,
    /// Subsurface scattering's per-frame state (ADR-2049): empty until a frame scatters.
    subsurface: subsurface_gpu::SubsurfaceState,
    /// Order-independent transparency's per-frame state (ADR-2065): empty until a frame runs it.
    oit: oit_gpu::OitState,
    /// Translucent shadow maps (colored shadow map for blended casters).
    translucent_maps: translucent_shadow::TranslucentMaps,
    /// [`translucent_shadow::TranslucentMaps::generation`] the cached view groups bind.
    translucent_view_gen: u64,
    /// The outdoor cloud transmittance map's generation the cached view groups bind (ADR-320).
    cloud_map_view_gen: u64,
    /// The many-light buffers' generation the view groups were built for (ADR-2652).
    many_light_view_gen: u64,
    /// The `cluster_indices_gen` the cached view groups bind (ADR-3351).
    cluster_indices_view_gen: u64,
    /// The `ibl_generation` the cached view groups bind (ADR-1654).
    ibl_view_gen: u64,
    /// [`crate::shader_reload::shader_epoch`] the pipelines were built at.
    shader_epoch: u64,
    /// Camera- and size-dependent state of each render-to-texture view
    /// ([`Self::render_texture_view`], ADR-184), by asset id. While a view renders, its entry
    /// holds the main view's.
    texture_views: HashMap<u64, Box<view_state::ViewState>>,
    /// Epoch of the bind caches ([`crate::bind_cache::EpochScope`]), renewed when a view's
    /// extent changes or idle per-size targets are released, so the caches drop entries that
    /// hold replaced targets alive. 0 until the first frame.
    bind_epoch: u64,
    /// The material texture each texture view feeds ([`crate::TextureViewSettings::texture`]).
    texture_view_handles: HashMap<u64, crate::DynamicTexture>,
    /// The texture view whose frame is being recorded (`None`: the main view).
    view_pass: Option<u64>,
    /// The XR eye being rendered and its hidden-area meshes ([`Self::render_eye`]).
    stereo: stereo::StereoState,
    /// Copies recorded at the end of the next frame: a texture view's image into the GPU
    /// texture its [`crate::DynamicTexture`] handle is bound as.
    publish_copies: Vec<(wgpu::Texture, wgpu::Texture)>,
    /// Non-blocking readbacks of texture targets ([`Self::request_texture_readback`]).
    texture_readbacks: HashMap<u64, TextureReadback>,
    /// Bindless step-2 census ([`Self::set_texture_set_census`], ADR-356).
    texture_set_census: crate::texture_sets::TextureSetCensus,
    /// The main view's zone-driven eye and its log ([`Self::set_exposure_target`], ADR-322).
    exposure_zones: crate::exposure_zones::ZoneExposure,
    /// Passes a host registered at the graph's slots ([`Self::register_pass`], ADR-1552).
    extension_passes: extension::ExtensionPasses,
    /// Test hook: the scene depth as the depth passes left it, copied every frame while set
    /// ([`Self::set_prepass_depth_capture`], ADR-5953).
    #[cfg(feature = "test-hooks")]
    prepass_depth_capture: Option<readback::PrepassDepthCapture>,
    /// The uploader of counted meshes drawn without CPU vertices ([`Self::set_asset_meshes`],
    /// ADR-7284).
    asset_meshes: Option<crate::AssetMeshUploads>,
    /// Mesh-cache keys whose entries hold a counted mesh's uploaded buffers.
    asset_mesh_keys: std::collections::HashSet<u64>,
    /// Scratch of `sweep_asset_meshes` (kept so a steady frame allocates nothing).
    asset_mesh_gone: Vec<u64>,
    /// The second UV set an adopted counted mesh's `uv1` buffer was built from, by mesh key: the
    /// draw's slice address and the frame it was written (ADR-7330).
    asset_uv1_sources: std::collections::HashMap<u64, (usize, u64)>,
    /// What the frame needs of each adopted skinned counted mesh, by mesh key (ADR-8610).
    asset_mesh_skins: std::collections::HashMap<u64, std::sync::Arc<AssetMeshSkin>>,
    /// Material entries rebuilt because a counted texture they bind was uploaded again
    /// ([`Self::counted_material_rebinds`], ADR-8610).
    counted_rebinds: u64,
    /// LOD index lists uploaded for counted meshes' chain variants
    /// ([`Self::counted_lod_index_uploads`], ADR-7428).
    counted_lod_uploads: u64,
}

/// One non-blocking readback of a texture target ([`FrameRenderer::request_texture_readback`]).
struct TextureReadback {
    buffer: wgpu::Buffer,
    width: u32,
    height: u32,
    padded_row: u32,
    state: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// A copy is in flight or waiting to be taken.
    pending: bool,
}

/// What [`FrameRenderer::render_texture_view`] measured for one texture view's frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TextureViewFrame {
    /// CPU time of the view's frame (state swap, recording, submit), ms.
    pub cpu_ms: f32,
    /// GPU time of this view's newest timed frame, ms (at least two frames old; 0 without
    /// GPU timing or before the first sample).
    pub gpu_ms: f32,
    /// Shadow passes (cascades and atlas tiles) the view's frame recorded.
    pub shadow_passes: u32,
}

/// Colour-target configuration for [`FrameRenderer::prewarm_glass`].
///
/// Cull mode is not part of the target: prewarm builds both the back-face-culled set and the
/// cull-none set a double-sided glass material draws with. The two sets share one shader
/// module; only `cull_mode` differs (glTF `doubleSided`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct GlassPipelineTarget {
    /// Colour attachment format of the forward pass.
    pub format: wgpu::TextureFormat,
    /// Encode to sRGB in the shader (`true` for a non-sRGB LDR target, `false` for linear HDR).
    pub encode: bool,
    /// MSAA sample count of the colour and depth attachments (1 or 4).
    pub samples: u32,
}

impl GlassPipelineTarget {
    /// Linear HDR frames: `Rgba16Float`, no shader sRGB encode, one sample.
    ///
    /// What a post-processed or temporally upscaled frame draws into on Desktop, Deck and the
    /// lab harness (`FrameQuality::LAB` with the post stack on).
    pub const HDR: Self = Self {
        format: wgpu::TextureFormat::Rgba16Float,
        encode: false,
        samples: 1,
    };
}

/// Renders `scene` into `target` (final colour; `target.format` decides whether
/// the shader or the hardware sRGB-encodes).
#[tracing::instrument(skip_all)]
pub fn render_frame_into(
    renderer: &mut FrameRenderer,
    device: &RenderDevice,
    scene: &FrameScene<'_>,
    target: FrameTarget<'_>,
) -> Result<(), RenderError> {
    renderer.render_into(device, scene, target)
}

impl FrameRenderer {
    /// Whether the mesh cache holds `key` with its writes submitted (a mesh deferred to the staging
    /// belt is submitted with the next frame). The streaming `render` gate's residency check reads
    /// it (spec 78 § 10.3, ADR-2317).
    pub(crate) fn mesh_resident(&self, key: u64) -> bool {
        // A member drawn with an LOD or HLOD dither is cached under its dithered key.
        [key, key ^ LOD_DITHER_MESH_TAG]
            .iter()
            .any(|k| self.meshes.contains_key(k) && !self.mesh_belt_keys.contains(k))
    }

    /// Whether the material cache holds `key` (its bind group, and so its textures, exist).
    pub(crate) fn material_resident(&self, key: u64) -> bool {
        self.materials.contains_key(&key)
    }

    /// Whether the last frame drew its stochastic passes with the blue-noise table (ADR-2075,
    /// [`crate::RenderProfile::blue_noise`]).
    #[must_use]
    pub fn blue_noise_active(&self) -> bool {
        self.blue_noise.is_on()
    }

    /// An empty renderer; GPU state is created on the first frame.
    #[must_use]
    pub fn new() -> Self {
        Self {
            shader_epoch: crate::shader_reload::shader_epoch(),
            // Made here, not on the first eviction sweep (frame 256), so no steady frame
            // allocates it (ADR-2299).
            sweep_clock: std::collections::VecDeque::with_capacity(mesh_cache::SWEEP_CLOCK_SAMPLES),
            ..Self::default()
        }
    }

    /// `REGIA_FORWARD_OVERLAP=0`: the frame's GPU-driven colour variants compile after its
    /// forward set, not alongside it (A/B timing; see `serial_forward_families`).
    fn forward_overlap_env_off() -> bool {
        static OFF: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *OFF.get_or_init(|| regia_core::brand::env_var("REGIA_FORWARD_OVERLAP").is_ok_and(|v| v == "0"))
    }

    /// Rebuilds the GPU state from the current shader text when a dev shader edit was committed
    /// since it was built ([`crate::shader_reload`]). Returns true when it rebuilt.
    ///
    /// Pipelines, targets and caches are recreated on this frame; what callers installed stays
    /// (translucent layers, the shared draw, interior scene, GI field, reflection probes, GPU
    /// timing, encode threads, upscaler and TSR settings, display headroom, scene export,
    /// material clock, gizmos, glass, the outdoor state and the visibility-buffer pass with its
    /// meshlet builder and instance sets; those two keep their own pipelines; the registered
    /// frame passes, the occlusion switches with their graph nodes, the lightmap and its
    /// receivers, and the forward-lite, camera-cut, texture-set census and ReSTIR GI settings).
    /// Called at the start of every top-level render entry point; a no-op in shipping builds.
    pub fn sync_shader_epoch(&mut self) -> bool {
        let epoch = crate::shader_reload::shader_epoch();
        if epoch == self.shader_epoch {
            return false;
        }
        let _span = tracing::info_span!("shader_hot_swap", epoch).entered();
        let mut fresh = Self {
            shader_epoch: epoch,
            ..Self::default()
        };
        fresh.translucent = std::mem::take(&mut self.translucent);
        fresh.shared_draw = self.shared_draw.take();
        fresh.shared_draw_set = self.shared_draw_set;
        fresh.interior_scene = self.interior_scene.take();
        fresh.gi_field = self.gi_field.take();
        fresh.gi_auto = self.gi_auto.take();
        fresh.gi_fallback = self.gi_fallback;
        fresh.baked_gi = self.baked_gi.share();
        // ADR-1033 measurement settings survive a shader edit mid-run.
        fresh.gi_lookup = self.gi_lookup;
        fresh.ssgi_half_res = self.ssgi_half_res;
        fresh.ssgi_relative_depth = self.ssgi_relative_depth;
        fresh.radiance_cache.set_settings(self.radiance_cache.settings());
        fresh.radiance_cache.set_enabled(self.radiance_cache.enabled());
        fresh.reflection_probes = std::mem::take(&mut self.reflection_probes);
        fresh.camera_up = self.camera_up;
        fresh.gi_cpu_ms = self.gi_cpu_ms;
        fresh.adopt_timer(self.gpu_timer.take());
        fresh.finish_workers = std::mem::take(&mut self.finish_workers);
        fresh.submission.carry(&self.submission);
        fresh.frame_index = self.frame_index;
        fresh.temporal_upscaler = self.temporal_upscaler;
        fresh.tsr_pass.set_settings(self.tsr_pass.settings());
        fresh.display_headroom = self.display_headroom;
        fresh.scene_export = self.scene_export.take();
        fresh.material_time = self.material_time;
        fresh.foliage_wind = self.foliage_wind;
        fresh.foliage_wind_from_scene = self.foliage_wind_from_scene;
        fresh.dynamic_casters = self.dynamic_casters;
        fresh.graph_cache.set_async(self.graph_cache.is_async());
        // The permutation guard and its counters (ADR-2026); the edited graphs are judged again.
        fresh.graph_cache.set_guard(self.graph_cache.take_guard());
        fresh.gizmos = self.gizmos.take();
        fresh.scalar_overlays = std::mem::take(&mut self.scalar_overlays);
        fresh.glass = std::mem::take(&mut self.glass);
        fresh.glass_key = self.glass_key;
        fresh.render_overrides = std::mem::take(&mut self.render_overrides);
        fresh.mask_decals = self.mask_decals.carry_instances();
        fresh.material_shading = std::mem::take(&mut self.material_shading);
        fresh.skin_compute = std::mem::take(&mut self.skin_compute);
        fresh.skin_compute.forget_kernel();
        fresh.canopy_glass = self.canopy_glass;
        fresh.outdoor = std::mem::take(&mut self.outdoor);
        fresh.output_dither = self.output_dither;
        fresh.sun_shadow_range = self.sun_shadow_range;
        fresh.outdoor_key = self.outdoor_key;
        fresh.visbuffer = std::mem::take(&mut self.visbuffer);
        fresh.pinned = std::mem::take(&mut self.pinned);
        fresh.residency = self.residency;
        fresh.cache_stats = self.cache_stats;
        fresh.rigid_skin_fold_off = self.rigid_skin_fold_off;
        fresh.load_budget.absorb(&self.load_budget);
        fresh.morph = self.morph.carry();
        fresh.cache_trim_pending = self.cache_trim_pending;
        fresh.adapter_vram_mib = self.adapter_vram_mib;
        fresh.adapter_discrete = self.adapter_discrete;
        fresh.adapter_integrated = self.adapter_integrated;
        fresh.vram_budgets_off = self.vram_budgets_off;
        fresh.material_anisotropy = self.material_anisotropy;
        fresh.entries = std::mem::take(&mut self.entries);
        fresh.stream_frame = std::mem::take(&mut self.stream_frame);
        fresh.skin_bounds = std::mem::take(&mut self.skin_bounds);
        fresh.exposure_zones = std::mem::take(&mut self.exposure_zones);
        fresh.taa_hashed_alpha = self.taa_hashed_alpha;
        fresh.alpha_tested_shadows = self.alpha_tested_shadows;
        fresh.shadow_occlusion.min_casters = self.shadow_occlusion.min_casters;
        fresh.set_shadow_occlusion(self.shadow_occlusion.enabled);
        // The host's frame passes and the prepass cull switches (a fresh renderer had none of
        // the passes and had two-phase occlusion off and Hi-Z on). The passes keep their own
        // pipelines; both two-phase switches put their nodes back in the graph (ADR-5626 fixed
        // the same loss; this is the one carry both lanes' tests run against, ADR-5712).
        fresh.extension_passes.carry_passes(&mut self.extension_passes);
        fresh.set_two_phase_occlusion(self.prepass_occlusion.enabled);
        // The CPU masked cull's switch (ADR-10072); its culler is rebuilt on the next frame.
        fresh.set_masked_occlusion(self.masked_cull.enabled());
        fresh.hiz_occlusion_off = self.hiz_occlusion_off;
        // The installed lightmap (its receivers stay; the host sets them only on a change) and
        // the agent-set switches the runtime does not push every frame.
        fresh.lightmap_forward = std::mem::take(&mut self.lightmap_forward).carried();
        fresh.forward_lite_override = self.forward_lite_override;
        fresh.set_camera_cut_detection(self.camera_cut_detection());
        fresh.set_texture_set_census(self.texture_set_census());
        if let Err(err) = fresh.set_restir_gi_settings(self.restir_gi_settings()) {
            tracing::warn!(target: "regia_render", %err, "ReSTIR GI settings not carried over a shader edit");
        }
        fresh.culler.set_ordered(self.culler.ordered());
        fresh
            .culler
            .set_foliage_shadow_softness(self.culler.foliage_shadow_softness());
        // A rebuild during an offline job keeps its frames complete; the graph and sky switches
        // and the cull order are carried above (ADR-2582). The exposure wait is carried too:
        // the switch forces it while it is on, and a plain host wait survives the rebuild as
        // well — without it the fresh post stack is created with waits off, reopening the
        // map-callback metering nondeterminism the switch closes (audit fix-render6 #2).
        fresh.wait_exposure_readback = self.wait_exposure_readback;
        fresh.complete_frames = self.complete_frames;
        if fresh.complete_frames.is_some() {
            fresh.emissive_cache.set_complete(true);
            fresh.highend.set_never_sleep(true);
            fresh.ddgi.bake_sync = true;
        }
        *self = fresh;
        tracing::info!(target: "regia_render", epoch, "engine shaders changed; pipelines rebuilt");
        true
    }

    /// Gives `stand_in` copies of what was installed on this renderer before it leaves for a
    /// warm worker ([`crate::GameFrameWarm`]): the translucent layers (VFX, shared `Arc`s), the
    /// GI field (with its interior scene and bake key) and the reflection probes. A stand-in that
    /// takes over for good (an abandoned warm) then lights and draws the map as this one did.
    /// The shared pass draw (terrain) cannot be copied; it goes with the warm and an abandoned
    /// warm hands it back when it returns ([`Self::salvage_shared_draw`]).
    pub(crate) fn share_installs_with(&self, stand_in: &mut FrameRenderer) {
        stand_in.translucent.clone_from(&self.translucent);
        stand_in.gi_field.clone_from(&self.gi_field);
        stand_in.gi_auto = self.gi_auto;
        stand_in.gi_fallback = self.gi_fallback;
        stand_in.baked_gi = self.baked_gi.share();
        stand_in.interior_scene.clone_from(&self.interior_scene);
        stand_in.reflection_probes.clone_from(&self.reflection_probes);
        stand_in.outdoor.set_aerial_interiors(self.aerial_interiors());
        // An abandoned warm leaves the stand-in drawing the game (ADR-2026).
        stand_in.graph_cache.set_guard(self.graph_cache.guard());
        // An offline job's frames stay complete on the stand-in, and what it would put back when
        // it ends is this renderer's (ADR-2728).
        if let Some(saved) = self.complete_frames {
            stand_in.set_complete_frames(true);
            stand_in.complete_frames = Some(saved);
        }
    }

    /// Takes the shared pass draw (terrain) from `late`, the renderer an abandoned warm returned
    /// after the stand-in (this renderer) took over, unless this one has a draw or was given or
    /// cleared one itself ([`Self::set_shared_draw`]). Returns whether it moved.
    pub(crate) fn salvage_shared_draw(&mut self, late: &mut FrameRenderer) -> bool {
        if self.shared_draw.is_some() || self.shared_draw_set {
            return false;
        }
        let Some(draw) = late.shared_draw.take() else {
            return false;
        };
        self.set_shared_draw(Some(draw));
        true
    }

    /// What is installed (tests): a GI field, how many reflection probes, a shared draw.
    #[cfg(test)]
    pub(crate) fn installs(&self) -> (bool, usize, bool) {
        (
            self.gi_field.is_some(),
            self.reflection_probes.len(),
            self.shared_draw.is_some(),
        )
    }

    /// Takes the once-installed hooks a stand-in renderer received while this one was away on a
    /// warm worker ([`crate::GameFrameWarm`]): the translucent layers (VFX) and the shared pass
    /// draw (terrain), when this renderer has none. Per-frame state (outdoor, glass, gizmos,
    /// material time) is set again by the next frame.
    pub(crate) fn absorb_stand_in(&mut self, stand_in: &mut FrameRenderer) {
        // A load budget the host set on the stand-in during the warm (ADR-7884).
        self.load_budget.absorb(&stand_in.load_budget);
        if self.translucent.is_empty() && !stand_in.translucent.is_empty() {
            self.translucent = std::mem::take(&mut stand_in.translucent);
        }
        if self.shared_draw.is_none() && stand_in.shared_draw.is_some() {
            self.shared_draw = stand_in.shared_draw.take();
        }
        // What the host installed on the stand-in meanwhile (v0.1.8: a warm the engine starts by
        // itself, for a new scene, can begin while the host is still configuring the map): the
        // last call wins, as if it had been made on this renderer.
        let set = std::mem::take(&mut stand_in.installed_since);
        if set.interior {
            self.set_interior_scene(stand_in.interior_scene.take().map(|(_, scene)| scene));
        }
        if set.gi {
            self.gi_field = stand_in.gi_field.take();
            self.gi_auto = stand_in.gi_auto;
            self.gi_fallback = stand_in.gi_fallback;
        }
        if set.probes {
            self.reflection_probes = std::mem::take(&mut stand_in.reflection_probes);
        }
        if set.ddgi_settings {
            self.ddgi.settings = stand_in.ddgi.settings.clone();
            self.ddgi.tier = stand_in.ddgi.tier;
        }
        if set.ddgi_enabled {
            self.ddgi.enabled = stand_in.ddgi.enabled;
        }
        if set.baked_gi {
            self.baked_gi.install(stand_in.baked_gi.volume.take());
            self.baked_gi
                .set_layers(&std::mem::take(&mut stand_in.baked_gi.layers));
            self.baked_gi.settings = stand_in.baked_gi.settings;
            self.baked_gi.enabled = stand_in.baked_gi.enabled;
        }
        if set.reserved > 0 {
            self.reserve_instances(set.reserved);
        }
        if set.lightmap {
            self.take_stand_in_lightmap(stand_in);
        }
        if set.warm_decals {
            self.warm_decals = stand_in.warm_decals;
        }
        if set.warm_probe_capture {
            self.warm_probe_capture = stand_in.warm_probe_capture;
        }
        // A capture the host made on the stand-in: the next warm of this renderer covers it too.
        self.probe_captured |= stand_in.probe_captured;
        // An offline job that started or ended while this renderer was away (ADR-2728).
        self.set_complete_frames(stand_in.complete_frames());
        if set.level_graphs {
            self.level_graphs = std::mem::take(&mut stand_in.level_graphs);
        }
        if set.permutation_guard {
            self.graph_cache.set_guard(stand_in.graph_cache.take_guard());
        }
        if set.aerial_interiors {
            self.set_aerial_interiors(stand_in.aerial_interiors());
        }
    }

    /// Frames between the frame whose HDR the auto-exposure meter bins and the first frame
    /// exposed with that reading (the histogram is read back that many frames late). After a
    /// [`Self::camera_cut`] this many frames print at the initial exposure
    /// ([`crate::PostSettings::auto_exposure_initial_ev100`]); an offline render that wants its
    /// cut frame exposed by its own meter renders it this many times first and then cuts again
    /// with [`crate::CutExposure::Fade`] (`render.out`, ADR-1455).
    pub const EXPOSURE_READBACK_FRAMES: u64 = crate::readback::READBACK_LATENCY;

    /// Applies a pending or detected camera cut before the frame's first temporal read.
    fn begin_cut_frame(&mut self, scene: &FrameScene<'_>, target: &FrameTarget<'_>) {
        let view_key = [
            target.width,
            target.height,
            target.viewport[0],
            target.viewport[1],
            target.viewport[2],
            target.viewport[3],
        ];
        let view = crate::camera_cut::CutView {
            eye: scene.eye,
            target: scene.target,
        };
        let objects = scene.meshes.iter().map(|mesh| crate::camera_cut::CutObject {
            key: (mesh.object_key != 0 && mesh.object_key & EPHEMERAL_KEY != EPHEMERAL_KEY)
                .then_some(mesh.object_key),
            content: crate::camera_cut::content_signature(&mesh.model, mesh.vertices, mesh.indices.len()),
        });
        let Some((reason, exposure)) = self.cut.begin_frame(view_key, view, scene.meshes.len(), objects)
        else {
            return;
        };
        let _span = tracing::info_span!("camera_cut", reason = reason.name()).entered();
        tracing::debug!(target: "regia_render", reason = reason.name(), "camera cut");
        self.cut.note(self.frame_index, reason);
        // Temporal upscalers, motion vectors and the history's pre-exposure.
        self.taau_ready = false;
        self.taau_pass.invalidate();
        self.tsr_pass.invalidate();
        self.have_prev_vp = false;
        self.prev_models.clear();
        self.prev_palettes.clear();
        self.prev_pre_exposure = 0.0;
        // The many-light reservoirs (ADR-2652).
        self.many_light.reset_history();
        self.restir_gi.reset();
        // Static-screen SSGI/SSR composite and the held-view counter.
        self.screen_cache_key = None;
        self.screen_cache_view = None;
        self.screen_hold = 0;
        self.highend.reset_history();
        // Froxel fog history and the cloud noise phase.
        self.outdoor.reset_history();
        // Shadow caches: cascades, pages, atlas tiles, caster lists, cascade placement.
        self.csm_valid = false;
        self.atlas_valid = false;
        let cascades = self.csm.as_ref().map_or(1, |(_, count, _)| *count);
        self.sun_pages =
            crate::paged_shadow::SunPageCache::new(cascades, crate::paged_shadow::SUN_PAGES_PER_AXIS);
        self.atlas_tiles = crate::paged_shadow::AtlasTileCache::new();
        self.caster_list_cache = CasterListCache::default();
        self.had_unbounded_caster = false;
        self.near_tile_vp = None;
        self.shared_shadow_keys = [None; 4];
        self.cascade_anchors = [None; 4];
        self.near_anchor = None;
        self.vis_set_eye = None;
        // Cluster LOD history is temporal (ADR-4631); each window view keeps its own, and a cut
        // discards the history of the view it starts (ADR-8620).
        self.visbuffer
            .clear_lod_history_of(self.prepass_occlusion.feedback_view);
        // GPU irradiance probes: their hysteresis is temporal history too. They restart from
        // zero like a fresh renderer's; the mesh bricks (geometry, not history) stay.
        self.ddgi.camera_cut();
        self.radiance_cache.camera_cut();
        if exposure == crate::CutExposure::Snap
            && let Some(post) = self.post.as_mut()
        {
            post.reset_exposure();
        }
        if exposure == crate::CutExposure::Snap && self.view_pass.is_none() {
            self.exposure_zones.camera_cut();
        }
    }

    /// Bakes or drops the daylit interior field (see [`Self::set_interior_scene`]).
    fn update_daylit_interior(
        &mut self,
        scene: &FrameScene<'_>,
        sun: DirectionalLightUniform,
        lights: &[crate::LabLocalLight],
    ) {
        use std::hash::{Hash, Hasher};
        let wants = scene.quality.interior_ambient
            && matches!(scene.quality.gi, crate::profile::GiMode::Probes)
            && (self.gi_field.is_none() || self.gi_auto.is_some());
        let Some((scene_key, sdf)) = self.interior_scene.as_ref().filter(|_| wants) else {
            self.gi_fallback = None;
            if self.gi_auto.take().is_some() {
                self.gi_field = None;
            }
            return;
        };
        // An authored volume wins. Otherwise keep the last fallback box while the eye is
        // still inside it: centering a new box on every eye step changed the bake key and
        // retraced the whole grid.
        let authored = self
            .outdoor
            .params()
            .interiors
            .iter()
            .copied()
            .find(|volume| volume.contains(scene.eye));
        let (volume, fallback) = if let Some(volume) = authored {
            self.gi_fallback = None;
            (volume, false)
        } else if let Some(held) = self.gi_fallback.filter(|held| held.contains(scene.eye)) {
            (held, true)
        } else {
            (
                crate::InteriorVolume::with_floor(scene.eye, [6.0, 3.0, 6.0]),
                true,
            )
        };
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        scene_key.hash(&mut hasher);
        for component in volume.center.iter().chain(&volume.half_extent) {
            component.to_bits().hash(&mut hasher);
        }
        volume.ambient_lux.to_bits().hash(&mut hasher);
        lights.len().hash(&mut hasher);
        for c in sun.direction.iter().chain(&sun.color) {
            ((c * 200.0).round() as i32).hash(&mut hasher);
        }
        ((sun.illuminance / 100.0).round() as i32).hash(&mut hasher);
        let key = hasher.finish();
        if self.gi_auto == Some(key) {
            return;
        }
        let probe_sun = crate::ProbeSun {
            direction: sun.direction,
            color: sun.color,
            illuminance: sun.illuminance,
        };
        let procedural = |dir: [f32; 3]| crate::ibl::sky_radiance(dir);
        let env = self.outdoor.environment_fn();
        let sky: &dyn Fn([f32; 3]) -> [f32; 3] = match env.as_ref() {
            Some(env) => env,
            None => &procedural,
        };
        let baked =
            crate::interior::build_daylit_interior(&volume, sdf, lights, probe_sun, sky, 1.5, 6.0, 48);
        drop(env);
        match baked {
            Ok(lighting) => {
                self.gi_field = Some(lighting.field);
                self.gi_auto = Some(key);
                self.gi_fallback = fallback.then_some(volume);
            }
            Err(err) => {
                tracing::warn!(target: "regia_render", "daylit interior bake failed: {err}");
                self.gi_fallback = None;
                if self.gi_auto.take().is_some() {
                    self.gi_field = None;
                }
            }
        }
    }

    /// Renders `scene` into `target` (a window surface view or any colour attachment).
    ///
    /// On a warm worker whose [`crate::PipelineWarmQueue`] is shutting down it stops between
    /// passes with an error ([`crate::pipeline_warm::warm_cancelled`]); the frame path never
    /// sees that.
    #[tracing::instrument(skip_all)]
    pub fn render_into(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        target: FrameTarget<'_>,
    ) -> Result<(), RenderError> {
        let _mem = regia_core::memory::enter(regia_core::memory::Subsystem::Render);
        crate::pipeline_warm::warm_checkpoint()?;
        self.sync_shader_epoch();
        // Every matrix below is built from these; a degenerate camera is replaced first.
        let fixed_camera = usable_camera(scene);
        let scene = fixed_camera.as_ref().unwrap_or(scene);
        let tracked = device.tracked();
        let d = &tracked;
        // No deref to the raw queue: every submit on this path is counted.
        let queue = device.tracked_queue();
        let q = &queue;
        let width = scene.width.max(1);
        let height = scene.height.max(1);
        // A still tile projects a window of the full camera; camera-derived lights and
        // shader inputs must use that complete aspect, not the tile's shape.
        let aspect = self
            .stereo
            .capture_size
            .map_or(width as f32 / height as f32, |[w, h]| w as f32 / h as f32);
        // ---- Frame clock, pools and the frame-begin poll (`steps::begin`) ----
        let open = self.open_frame(device, d, scene, &target);
        let _encode = tracing::info_span!("encode").entered();
        self.begin_encode(d, scene);

        // ---- The frame's modes and matrices (`steps::modes`) ----
        let modes = self.frame_modes(
            device,
            d,
            scene,
            &target,
            steps::modes::ModeInputs {
                width,
                height,
                aspect,
                view_pass: open.view_pass,
                temporal_index: open.temporal_index,
            },
        );
        let mut cx = steps::recording::FrameRecording::new(open, modes);
        // The frame graph's built-in dispatch (ADR-4905): every built-in record site enters its node.
        let main_view = !cx.open.view_pass && !self.capturing_probe;
        self.extension_passes.dispatch.begin(self.frame_index, main_view);
        // CPU phases before recording, as spans (`render_into/encode/<phase>`).
        let phase = tracing::info_span!("frame_setup").entered();
        let FrameLights {
            uploaded,
            directional,
            fill_light,
            mut plan,
            emissive_records,
            emissive_probes,
        } = self.frame_lights(&frame_lights::FrameLightInputs {
            scene,
            view_pass: cx.open.view_pass,
            now: cx.open.now,
            fov_y: cx.modes.fov_y,
            aspect,
            planet_ibl: cx.modes.planet_ibl,
            unjittered_vp: cx.modes.unjittered_vp,
            forced_post: &cx.modes.forced_post,
            post_exposure: cx.modes.post_exposure,
            baked_on: cx.modes.baked_on,
        });
        // Records in the light buffer: the local lights, then the fitted emitters (ADR-2652).
        let many_light_count = (uploaded.len() + emissive_records.len()) as u32;
        crate::pipeline_warm::warm_checkpoint()?;
        // ---- GPU state (once) — before the frame uniform so the IBL LOD is known ----
        self.ensure_gpu_state(
            device,
            d,
            q,
            frame_uniforms::GpuStateInputs {
                scene,
                view_pass: cx.open.view_pass,
                planet_ibl: cx.modes.planet_ibl,
                linear_hdr: cx.modes.linear_hdr,
                directional,
                emissive_probes,
            },
        )?;
        // ---- Frame uniform and per-frame buffer writes (skipped when the bytes already match) ----
        self.write_frame_uniforms(
            d,
            frame_uniforms::FrameUniformInputs {
                scene,
                plan: &plan,
                uploaded: &uploaded,
                emissive_records,
                directional,
                fill_light,
                forward_fog: cx.modes.forward_fog,
                view: cx.modes.view,
                view_proj: cx.modes.view_proj,
                unjittered_vp: cx.modes.unjittered_vp,
                prev_vp: cx.modes.prev_vp,
                fov_y: cx.modes.fov_y,
                aspect,
                width,
                height,
                internal_w: cx.modes.internal_w,
                internal_h: cx.modes.internal_h,
                linear_hdr: cx.modes.linear_hdr,
                pre_exposure: cx.modes.pre_exposure,
                taau_on: cx.modes.taau_on,
                tsr_frame: cx.modes.tsr_frame,
                temporal_index: cx.open.temporal_index,
                texture_lod_bias: cx.modes.texture_lod_bias,
                sssr_on: cx.modes.sssr_on,
                bent_on: cx.modes.bent_on,
                planet_ldr: cx.modes.planet_ldr,
            },
        );

        drop(phase);
        let phase = tracing::info_span!("draw_list").entered();
        // ---- Skin palette (packed once per frame) ----
        let (packed_joints, spans, rigid_skin_folded) = self.pack_skin_palette(d, scene)?;

        let color_format = targets::scene_color_format(device, scene, cx.modes.linear_hdr, target.format);
        crate::pipeline_warm::warm_checkpoint()?;
        // The sun's variant this frame (ADR-2035, `sun_trace`).
        let sun_variant = self.select_sun_variant(d, scene, cx.open.view_pass, cx.modes.samples_early);
        let sun_rt = sun_variant.forward_key_bit();
        self.begin_blue_noise(d, &device.queue, scene.quality.blue_noise);
        self.culler.set_forward_sun_rt(sun_rt);
        // The studio axis this frame (ADR-2308, `forward_studio`).
        let studio_off = self.forward_studio_off();
        self.culler.set_forward_studio_off(studio_off);
        // ---- Material-graph pipeline sets, mesh/material/object entries, draw lists ----
        let DrawList {
            graph_sets,
            vis_frame,
            vis_shadow_state,
            vis_set_eye,
            mut draws,
            table_path,
            mask_unsupported,
            opaque_ix,
            mut blend_ix,
            shadow_ix,
            texture_table_group,
        } = self.build_draw_list(
            device,
            d,
            q,
            draw_list::DrawListInputs {
                scene,
                view_pass: cx.open.view_pass,
                linear_hdr: cx.modes.linear_hdr,
                encode_target: cx.modes.encode_target,
                color_format,
                gpu_objects: cx.modes.gpu_objects,
                samples_early: cx.modes.samples_early,
                shadow_debug: cx.modes.shadow_debug,
                taau_on: cx.modes.taau_on,
                height: self.stereo.capture_size.map_or(height, |size| size[1]),
                fov_y: cx.modes.fov_y,
                frustum: &cx.modes.frustum,
                view: cx.modes.view,
                unjittered_vp: cx.modes.unjittered_vp,
                spans,
                rigid_skin_folded,
                now: cx.open.now,
                sun_rt,
            },
        )?;

        drop(phase);
        crate::pipeline_warm::warm_checkpoint()?;
        let phase = tracing::info_span!("shadow_plan").entered();
        let plan_started = std::time::Instant::now();
        // ---- Shadow targets, casters and the caches' report ----
        let ShadowCasters {
            active_cascades,
            csm_size,
            cascade_count,
            paging,
            csm_splits,
            caster_info,
            trans_ix,
            trans_cascades,
            trans_local,
            trans_info,
            vis_shadows,
            cascade_uv_lists,
            cascade_vps,
            vp_count,
            casters,
            caster_slot,
            mut report,
            samples,
            mesh_full,
            shadow_regions,
        } = self.plan_shadow_casters(
            d,
            shadow_setup::ShadowCasterInputs {
                scene,
                plan: &mut plan,
                uploaded: &uploaded,
                draws: &draws,
                shadow_ix: &shadow_ix,
                gpu_objects: cx.modes.gpu_objects,
                vis_shadow_state,
            },
        );
        let cascade_uv = cascade_uv_lists.lists();
        let vps = &cascade_vps[..vp_count];

        // ---- Mesh occluders and the shared draw's prepare step ----
        let shared_info = self.prepare_shared_frame(
            d,
            shared_prepare::SharedPrepareInputs {
                scene,
                plan: &plan,
                directional: &directional,
                fill_light,
                forward_fog: cx.modes.forward_fog,
                planet_ibl: cx.modes.planet_ibl,
                view_proj: cx.modes.view_proj,
                unjittered_vp: cx.modes.unjittered_vp,
                prev_vp: cx.modes.prev_vp,
                fov_y: cx.modes.fov_y,
                internal_w: cx.modes.internal_w,
                internal_h: cx.modes.internal_h,
                color_format,
                linear_hdr: cx.modes.linear_hdr,
                pre_exposure: cx.modes.pre_exposure,
                taau_on: cx.modes.taau_on,
                texture_lod_bias: cx.modes.texture_lod_bias,
                terrain_texture_lod_bias: cx.modes.terrain_texture_lod_bias,
                sssr_on: cx.modes.sssr_on,
                samples,
                csm_size,
                cascade_count,
                shadow_regions,
                dynamic_shadow_cascades: if paging {
                    report.dynamic.policy.cascade_mask
                } else {
                    0
                },
            },
            &mut cx.shadow_cpu,
        )?;

        // ---- Shadow jobs and their caster lists ----
        let ShadowJobs {
            shadow_jobs,
            job_casters,
        } = self.build_shadow_jobs(
            shadow_setup::ShadowJobInputs {
                scene,
                plan: &plan,
                draws: &draws,
                shadow_ix: &shadow_ix,
                caster_info: &caster_info,
                cascade_uv,
                vps,
                csm_size,
                cascade_count,
                paging,
                samples,
                mesh_full: &mesh_full,
                vis_shadows,
            },
            &mut report,
            &mut cx.shadow_cpu,
        );
        cx.shadow_cpu.plan_ms = plan_started.elapsed().as_secs_f32() * 1000.0;

        drop(phase);
        crate::pipeline_warm::warm_checkpoint()?;
        let phase = tracing::info_span!("instance_lists").entered();
        let instance_lists_started = std::time::Instant::now();
        let instanced = self.build_instance_lists(
            d,
            &instance_lists::InstanceListInputs {
                scene,
                draws: &draws,
                job_casters: &job_casters,
                opaque_ix: &opaque_ix,
                gpu_objects: cx.modes.gpu_objects,
                samples_early: cx.modes.samples_early,
            },
        )?;

        cx.shadow_cpu.instance_lists_ms = instance_lists_started.elapsed().as_secs_f32() * 1000.0;
        drop(phase);
        crate::pipeline_warm::warm_checkpoint()?;
        let phase = tracing::info_span!("frame_bindings").entered();
        let commit_csm = paging && scene.directional_shadows;
        let commit_atlas = paging && !plan.tiles.is_empty();
        self.shadow_report = report;

        let csm_views = self.csm.as_ref().expect("csm targets").2.clone();
        let (_, atlas_view) = self.atlas.as_ref().expect("atlas").clone();

        // ---- Many-light sampling: decide and size, so the view group binds the reservoirs this
        // frame writes (`many_light`, ADR-2652) ----
        let many_light_bind = self.many_light.begin(
            d,
            &many_light::ManyLightBegin {
                requested: scene.quality.many_light_sampling,
                samples,
                linear_hdr: cx.modes.linear_hdr,
                view_pass: cx.open.view_pass,
                size: (cx.modes.internal_w, cx.modes.internal_h),
                binding: self.pipes.as_ref().is_some_and(|p| p.many_light_binding),
            },
            many_light::light_list_signature(&uploaded),
            uploaded.len() as u32,
        );
        let restir_di = many_light_bind.is_some();
        // The GPU-driven colour pipelines take the same axis (all of them are opaque).
        self.culler.set_forward_restir_di(restir_di);
        // ---- AO and visibility targets, probes and the view bind group (`steps::view_group`) ----
        let steps::view_group::ViewGroup {
            ssao_size,
            ssao_active,
            ao_bound,
            contact_active,
            contact_bound,
            visibility_active,
            forward_tier,
            baked_lit,
            ddgi_uniform,
            ddgi_data,
            ddgi_live,
            view_bg,
        } = self.frame_view_group(
            d,
            &steps::view_group::ViewGroupInputs {
                scene,
                target: &target,
                plan: &plan,
                directional,
                fill_light,
                csm_views: &csm_views,
                atlas_view: &atlas_view,
                view_proj: cx.modes.view_proj,
                internal_w: cx.modes.internal_w,
                internal_h: cx.modes.internal_h,
                samples,
                samples_early: cx.modes.samples_early,
                csm_size,
                gi_quality: cx.modes.gi_quality,
                sun_variant,
                baked_on: cx.modes.baked_on,
                view_pass: cx.open.view_pass,
                linear_hdr: cx.modes.linear_hdr,
                many_light: many_light_bind,
            },
        )?;

        crate::pipeline_warm::warm_checkpoint()?;
        // ---- Pipelines for this target (`steps::pipelines`) ----
        let steps::pipelines::FramePipelines {
            key,
            key_ds,
            any_cull_off,
            lite_colors,
            mask_sets,
            mut lightmap_frame,
            film_frame,
            subsurface_frame,
            mut oit_frame,
        } = self.frame_pipelines(
            device,
            d,
            &mut draws,
            &mut blend_ix,
            &steps::pipelines::PipelineInputs {
                scene,
                opaque_ix: &opaque_ix,
                graph_sets: &graph_sets,
                color_format,
                encode_target: cx.modes.encode_target,
                linear_hdr: cx.modes.linear_hdr,
                samples,
                sun_rt,
                studio_off,
                restir_di,
                gpu_objects: cx.modes.gpu_objects,
                mask_unsupported,
                forward_tier,
                view_pass: cx.open.view_pass,
            },
        )?;
        let pipes = self.pipes.as_mut().expect("pipelines");

        crate::pipeline_warm::warm_checkpoint()?;
        // ---- Colour/depth targets ----
        let targets::FrameTargets {
            attach_w,
            attach_h,
            depth_view,
            hdr_texture,
            hdr_view,
            msaa_color,
            msaa_depth,
        } = targets::frame_targets(
            targets::FrameTargetFields {
                targets: &mut self.targets,
                bind_epoch: &mut self.bind_epoch,
                transients: &mut self.transients,
            },
            d,
            &targets::FrameTargetInputs {
                target: &target,
                linear_hdr: cx.modes.linear_hdr,
                color_format,
                internal_w: cx.modes.internal_w,
                internal_h: cx.modes.internal_h,
                width,
                height,
                samples,
                now: cx.open.now,
            },
        );

        // ---- Shadow tile bind groups ----
        while self.tile_vp_bgs.len() < cascade_count + plan.tiles.len() {
            let buf = make_buffer(
                d,
                "shadow vp",
                size_of::<ShadowVp>() as u64,
                wgpu::BufferUsages::UNIFORM,
            );
            let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("shadow vp"),
                layout: &pipes.shadow_bgl,
                entries: &[binding(0, buf.as_entire_binding())],
            });
            self.tile_vp_bufs.push(buf);
            self.tile_vp_bgs.push(bg);
        }

        shared_engine::set_shared_engine_inputs(
            shared_engine::SharedEngineFields {
                shared_draw: &mut self.shared_draw,
                outdoor: &self.outdoor,
                ibl: &self.ibl,
                frame_buf: &self.frame_buf,
                lights_buf: &self.lights_buf,
                counts_buf: &self.counts_buf,
                indices_buf: &self.indices_buf,
                tiles_buf: &self.tiles_buf,
                belt: &mut self.belt,
                shared_upload_last: &mut self.shared_upload_last,
            },
            d,
            &shared_engine::SharedEngineInputs {
                scene,
                shared_info: &shared_info,
                csm_views: &csm_views,
                ao_bound: &ao_bound,
                contact_bound: &contact_bound,
                atlas_view: &atlas_view,
                ddgi_uniform: &ddgi_uniform,
                ddgi_data: &ddgi_data,
                samples,
                ssao_active,
                contact_active: visibility_active,
                planet_ibl: cx.modes.planet_ibl,
                ddgi_live,
            },
        );

        drop(phase);
        crate::pipeline_warm::warm_checkpoint()?;
        // ---- Record passes ----
        // Finished segments of this frame, in recording order (`parallel_finish`): the
        // encoder is cut at a few pass boundaries and each segment is finished on a
        // worker while the next is recorded. One submit at the end. Render-submission trims
        // (ADR-508) are published with the frame's submit (`steps::recording`).
        let mut rec =
            steps::recording::PassRecording::begin(d, q, self.gpu_timer.as_ref(), self.submission.enabled());
        // Keep rec.encoder in this local slot through finish (Android pool cuts, ADR-4657).
        let pass_cuts = crate::parallel_finish::pass_cuts::enabled().then(|| {
            crate::parallel_finish::pass_cuts::PassCuts::start(d, &rec.encoder, &mut self.finish_workers)
        });
        // Early defers (frame uniforms, skin, instance lists, mesh uploads) land before any pass.
        self.belt.flush(&mut rec.encoder);
        // Compute skinning: every pass below draws the skinned twins.
        self.skin_compute.encode(&mut rec.encoder);
        if let Some((set, _)) = &rec.timer {
            set.write(&mut rec.encoder, 0);
        }
        // Morph targets: deformed copies are filled before any pass reads vertices.
        morph_gpu::encode(
            &mut self.morph,
            &mut pipes.morph,
            &self.meshes,
            d,
            &mut rec.encoder,
        );
        // The shared draw's own compute (terrain virtual-texture page bakes) inside the frame's
        // timed span: it lands in the shadow bucket; the draw times it itself as well.
        if !cx.open.view_pass
            && let Some(draw) = self.shared_draw.as_ref()
        {
            draw.encode_before_passes(&mut rec.encoder);
        }
        // The cloud transmittance map's band for this frame (ADR-320), before any pass reads it.
        self.outdoor.encode_cloud_shadow_map(&mut rec.encoder);

        // ---- The passes, in the order the frame graph gives (ADR-4945) ----
        // The graph hands the frame its record sites in node order (`FrameDispatch::sites`); each
        // site enters its nodes through the dispatch. A site's products reach the later sites here.
        let (mut frame_pipes, mut screen, mut forward_prep, mut front_layers) = (None, None, None, None);
        let (mut vis_drawn, mut vis_resolved, mut hdr_particles) = (false, false, false);
        let (mut subsurface_counts, mut oit_reactive, mut shaded, mut upscaled) = (None, None, None, None);
        let mut output_depth: Option<wgpu::TextureView> = None;
        let mut prepared_sky = None;
        let mut stamps = Vec::new();
        let slot_camera = extension::SlotCamera::new(&msaa_depth, &depth_view, &cx.modes, scene.eye);
        let (sites, site_count) = self.extension_passes.dispatch.sites();
        for &site in &sites[..site_count] {
            match site {
                FrameSite::Shadows => {
                    let shadow_phase1 = self.enter_shadow_site(cx.open.view_pass, &shadow_jobs, &instanced);
                    let pipes = self.pipes.as_ref().expect("pipelines");
                    let (set, set_ds) = steps::forward_pipes::frame_sets(pipes, (key, key_ds), any_cull_off);
                    // Visibility-buffer casters: one cluster cull per shadow job (`vis_shadow.wgsl`), drawn
                    // inside the job's pass below.
                    let vis_shadow_drawn = shadow_passes::record_vis_shadow_cull(
                        &mut self.visbuffer,
                        d,
                        q,
                        &mut rec.encoder,
                        &shadow_passes::VisShadowInputs {
                            scene,
                            plan: &plan,
                            shadow_jobs: &shadow_jobs,
                            csm_splits: &csm_splits,
                            vis_set_eye,
                            csm_size,
                            cascade_count,
                            vis_shadows,
                        },
                        &mut cx.shadow_cpu,
                    )?;

                    crate::pipeline_warm::warm_checkpoint()?;
                    // Shadow passes: cascades first (buffer slots 0..cascade_count), then
                    // atlas tiles (slots cascade_count..). `kind` selects the depth target:
                    // 0..active_cascades = cascade i, 4 = an atlas tile.
                    shadow_passes::record_shadow_passes(
                        shadow_passes::ShadowFields {
                            tile_vp_bufs: &mut self.tile_vp_bufs,
                            shadow_report: &mut self.shadow_report,
                            tile_vp_bytes: &mut self.tile_vp_bytes,
                            shared_shadow_keys: &mut self.shared_shadow_keys,
                            visbuffer: &mut self.visbuffer,
                            tile_vp_bgs: &mut self.tile_vp_bgs,
                            shared_draw: &mut self.shared_draw,
                            meshes: &mut self.meshes,
                            materials: &mut self.materials,
                            entries: &mut self.entries,
                            shared_shadow_region_draws: &mut self.shared_shadow_region_draws,
                            shared_shadow_draws: &mut self.shared_shadow_draws,
                            objects: &mut self.objects,
                            csm_valid: &mut self.csm_valid,
                            belt: &mut self.belt,
                            occlusion: &mut self.shadow_occlusion,
                        },
                        d,
                        &mut rec.encoder,
                        &shadow_passes::ShadowInputs {
                            scene,
                            pipes,
                            plan: &plan,
                            shadow_jobs: &shadow_jobs,
                            job_casters: &job_casters,
                            draws: &draws,
                            graph_sets: &graph_sets,
                            instanced: &instanced,
                            set,
                            set_ds,
                            csm_views: &csm_views,
                            atlas_view: &atlas_view,
                            active_cascades,
                            cascade_count,
                            csm_size,
                            samples,
                            texture_table_group: &texture_table_group,
                            timer: &rec.timer,
                            trims: rec.trims,
                            vis_shadow_drawn,
                            view_pass: cx.open.view_pass,
                            alpha_tested_shadows: self.alpha_tested_shadows,
                            shadow_phase1,
                        },
                        shadow_passes::ShadowCounters {
                            binds: &mut rec.binds,
                            draw_counts: &mut rec.draw_counts,
                            shadow_cpu: &mut cx.shadow_cpu,
                        },
                    )?;
                    // Translucent casters into the translucency maps, inside the shadow bucket.
                    shadow_passes::record_translucent_shadows(
                        shadow_passes::TranslucentShadowFields {
                            glass_gpu: &mut self.glass_gpu,
                            glass: &self.glass,
                            meshes: &self.meshes,
                            objects: &self.objects,
                            materials: &self.materials,
                            entries: &self.entries,
                            belt: &mut self.belt,
                            translucent_maps: &mut self.translucent_maps,
                            shadow_report: &mut self.shadow_report,
                        },
                        d,
                        q,
                        &mut rec.encoder,
                        &shadow_passes::TranslucentShadowInputs {
                            scene,
                            pipes,
                            plan: &plan,
                            draws: &draws,
                            trans_ix: &trans_ix,
                            trans_info: &trans_info,
                            texture_table_group: &texture_table_group,
                            trans_cascades,
                            trans_local,
                            table_path,
                        },
                        &mut cx.shadow_cpu,
                    );
                    self.weather.record_readback(
                        d,
                        &mut rec.encoder,
                        &plan,
                        self.atlas.as_ref(),
                        self.frame_index,
                    );
                    if let Some((set, _)) = &rec.timer {
                        set.write(&mut rec.encoder, 1);
                    }
                    // Cuts sit where the next timestamp interval always holds work (here the cluster
                    // build): the queue's barrier between two command buffers is timed in that
                    // interval, so a skipped pass (a reused SSAO or SSGI) still reads ~0 ms.
                    self.finish_workers.cut(d, &mut rec.encoder, &mut rec.segments);
                }
                FrameSite::ClusterProbes => {
                    let pipes = self.pipes.as_mut().expect("pipelines");
                    crate::pipeline_warm::warm_checkpoint()?;
                    self.extension_passes
                        .dispatch
                        .enter_all(&[BuiltinNode::ClusterLights, BuiltinNode::GiProbes]);
                    cluster_probes::record_cluster_and_probes(
                        cluster_probes::ClusterProbeFields {
                            frame_buf: &self.frame_buf,
                            lights_buf: &self.lights_buf,
                            counts_buf: &self.counts_buf,
                            indices_buf: &self.indices_buf,
                            cluster_bg: &mut self.cluster_bg,
                            ddgi: &mut self.ddgi,
                            shared_draw: &self.shared_draw,
                            ibl: &self.ibl,
                        },
                        d,
                        &mut rec.encoder,
                        &cluster_probes::ClusterProbeInputs {
                            scene,
                            pipes: &*pipes,
                            timer: &rec.timer,
                            samples,
                            ibl_generation: self.ibl_generation,
                        },
                    )?;

                    // Owned pipeline clones and the glass sets (`steps::forward_pipes`).
                    frame_pipes = Some(steps::forward_pipes::FramePipeClones::take(
                        pipes,
                        d,
                        (key, key_ds),
                        any_cull_off,
                        &lite_colors,
                        &draws,
                    )?);
                }
                FrameSite::DepthPasses => {
                    let frame_pipes = steps::recording::site_input(&frame_pipes, "frame_pipes")?;
                    crate::pipeline_warm::warm_checkpoint()?;
                    // ---- Depth prepass, visibility geometry and the GPU cull (`steps::depth_passes`) ----
                    vis_drawn = self.record_depth_passes(
                        d,
                        q,
                        &mut rec.encoder,
                        &mut draws,
                        prepass::PrepassCounters {
                            binds: &mut rec.binds,
                            draw_counts: &mut rec.draw_counts,
                            shared_depth_stamped: &mut rec.shared_depth_stamped,
                        },
                        &steps::depth_passes::DepthPassInputs {
                            scene,
                            opaque_ix: &opaque_ix,
                            graph_sets: &graph_sets,
                            instanced: &instanced,
                            frame_pipes,
                            depth_view: &depth_view,
                            msaa_depth: &msaa_depth,
                            view_bg: &view_bg,
                            texture_table_group: &texture_table_group,
                            timer: &rec.timer,
                            frustum: &cx.modes.frustum,
                            strategy: cx.modes.strategy,
                            unjittered_vp: cx.modes.unjittered_vp,
                            view_proj: cx.modes.view_proj,
                            prev_vp: cx.modes.prev_vp,
                            fov_y: cx.modes.fov_y,
                            attach_w,
                            attach_h,
                            samples,
                            trims: rec.trims,
                            in_pass_stamps: rec.in_pass_stamps,
                            vis_frame,
                            view_pass: cx.open.view_pass,
                        },
                    )?;
                    // ---- Static-screen key and what the screen-space passes reuse (`steps::screen_reuse`) ----
                    let mut reuse = self.screen_reuse(&steps::screen_reuse::ScreenReuseInputs {
                        scene,
                        draws: &draws,
                        opaque_ix: &opaque_ix,
                        oit_frame: &oit_frame,
                        gi_quality: cx.modes.gi_quality,
                        unjittered_vp: cx.modes.unjittered_vp,
                        sun_variant,
                        color_format,
                        pre_exposure: cx.modes.pre_exposure,
                        width,
                        height,
                        internal_w: cx.modes.internal_w,
                        internal_h: cx.modes.internal_h,
                        samples,
                        linear_hdr: cx.modes.linear_hdr,
                        sky_pass: cx.modes.sky_pass,
                        shadow_debug: cx.modes.shadow_debug,
                        taau_on: cx.modes.taau_on,
                        visibility_active,
                        bent_on: cx.modes.bent_on,
                        sssr_chosen: cx.modes.sssr_chosen,
                        sssr_on: cx.modes.sssr_on,
                        view_pass: cx.open.view_pass,
                    });
                    // A capable ReSTIR GI frame must not serve a composite produced by another GI path. On
                    // unsupported devices the exact old reuse decision is retained for a bit-identical fallback.
                    if scene.quality.restir_gi
                        && d.features().contains(wgpu::Features::EXPERIMENTAL_RAY_QUERY)
                        && d.features()
                            .contains(wgpu::Features::EXPERIMENTAL_RAY_HIT_VERTEX_RETURN)
                    {
                        reuse.reuse_hit = false;
                    }
                    screen = Some(reuse);
                }
                FrameSite::ScreenOcclusion => {
                    let screen = steps::recording::site_input(&screen, "screen_reuse")?;
                    crate::pipeline_warm::warm_checkpoint()?;
                    self.dispatch_nodes(&[
                        BuiltinNode::Ssao,
                        BuiltinNode::ContactShadows,
                        BuiltinNode::SunShadowTrace,
                    ]);
                    screen_occlusion::record_screen_occlusion(
                        screen_occlusion::OcclusionFields {
                            ddgi: &mut self.ddgi,
                            baked_gi: &mut self.baked_gi,
                            ssao_pass: &mut self.ssao_pass,
                            blue_noise: self.blue_noise.active(),
                            contact_pass: &mut self.contact_pass,
                            sun_trace: &mut self.sun_trace,
                            restir_gi: &mut self.restir_gi,
                            materials: &self.materials,
                            ddgi_uniform: &ddgi_uniform,
                            ddgi_data: &ddgi_data,
                            gi_lookup: self.gi_lookup,
                            q,
                            belt: &mut self.belt,
                            screen_report: &mut self.screen_report,
                        },
                        d,
                        &mut rec.encoder,
                        &screen_occlusion::OcclusionInputs {
                            scene,
                            depth_view: &depth_view,
                            timer: &rec.timer,
                            ssao_size,
                            ssao_mode: cx.modes.gi_quality.ssao,
                            unjittered_proj: cx.modes.unjittered_proj,
                            view_proj: cx.modes.view_proj,
                            unjittered_vp: cx.modes.unjittered_vp,
                            view_pass: cx.open.view_pass,
                            viewport: cx.modes.viewport,
                            internal_w: cx.modes.internal_w,
                            internal_h: cx.modes.internal_h,
                            bent_to_world: cx.modes.bent_to_world,
                            temporal_index: cx.open.temporal_index,
                            taau_on: cx.modes.taau_on,
                            linear_hdr: cx.modes.linear_hdr,
                            ssao_active,
                            contact_active,
                            sun_variant,
                            meshes: &self.meshes,
                            draws: &draws,
                            shared_pipelines: device.shared_pipelines(),
                            baked_lit,
                            reuse_hit: screen.reuse_hit,
                            gather_reuse: screen.gather_reuse,
                        },
                    )?;
                }
                FrameSite::ManyLightSampling => {
                    // Stochastic many-light sampling (ADR-2652): the G-buffer from the prepass depth and the
                    // reservoir passes over this frame's lights and cluster lists, ahead of the forward pass
                    // that reads them. Nothing is recorded unless `begin` chose to sample.
                    let sampling = self.many_light.frame_active();
                    if self.dispatch_node(BuiltinNode::ManyLightSampling, sampling)
                        && let (Some(lights), Some(counts), Some(indices)) = (
                            self.lights_buf.as_ref(),
                            self.counts_buf.as_ref(),
                            self.indices_buf.as_ref(),
                        )
                    {
                        self.many_light.record(
                            d,
                            q,
                            &mut rec.encoder,
                            &many_light::ManyLightRecord {
                                depth: &depth_view,
                                lights,
                                light_count: many_light_count,
                                counts,
                                indices,
                                max_per_cluster: scene.quality.max_lights_per_cluster,
                                clip_from_world: cx.modes.unjittered_vp,
                                clip_from_view: cx.modes.unjittered_proj,
                                eye: scene.eye,
                                fov_y: cx.modes.fov_y,
                                near: scene.near,
                                far: scene.far,
                            },
                        );
                    }
                    // The interval after this cut is the forward pass.
                    self.finish_workers.cut(d, &mut rec.encoder, &mut rec.segments);
                }
                FrameSite::VisbufferResolve => {
                    let frame_pipes = steps::recording::site_input(&frame_pipes, "frame_pipes")?;
                    let mdi = cx.modes.strategy.uses_gpu_objects() && self.culler.instance_count() > 0;
                    let gpu_color = forward_pass::prepare_gpu_color(
                        forward_pass::GpuColorFields {
                            culler: &mut self.culler,
                            belt: &mut self.belt,
                            scratch: &mut self.scratch,
                            skin_buf: &self.skin_buf,
                            late_gpu_color_builds: &mut self.late_gpu_color_builds,
                        },
                        d,
                        &mut rec.encoder,
                        &forward_pass::GpuColorInputs {
                            draws: &draws,
                            view_bgl: &frame_pipes.view_bgl,
                            mat_bgl: &frame_pipes.mat_bgl,
                            color_format,
                            encode: key.encode,
                            samples_early: cx.modes.samples_early,
                            double_sided: scene.double_sided,
                        },
                        mdi,
                    )?;
                    // Lightmap receivers in the batches: their table and batches (`lightmap_paths`, ADR-2107).
                    self.lightmap_forward.prepare_instanced(
                        &mut lightmap_frame,
                        device,
                        &draws,
                        gpu_color.as_ref().map(|color| color.3.as_slice()),
                    );
                    forward_pass::count_forward_draws(&mut self.last_draw_stats, &draws, &gpu_color);
                    let single_indirect =
                        if cx.modes.strategy == crate::gpu_driven::IndirectStrategy::SingleIndirect {
                            self.culler.indirect().cloned()
                        } else {
                            None
                        };

                    // Visibility-buffer material resolve: clears (or keeps) the colour like the forward pass
                    // would, then shades the visibility pixels with the forward fragment body per material.
                    crate::pipeline_warm::warm_checkpoint()?;
                    self.dispatch_nodes(&[BuiltinNode::VisbufferResolve]);
                    vis_resolved = visbuffer_resolve::record_visbuffer_resolve(
                        visbuffer_resolve::ResolveFields {
                            visbuffer: &mut self.visbuffer,
                            materials: &self.materials,
                            arena: self.material_arena.as_ref(),
                        },
                        d,
                        q,
                        &mut rec.encoder,
                        &visbuffer_resolve::ResolveInputs {
                            sun_rt,
                            target: &target,
                            hdr_view: &hdr_view,
                            view_bg: &view_bg,
                            view_bgl: &frame_pipes.view_bgl,
                            mat_bgl: &frame_pipes.mat_bgl,
                            texture_table_group: &texture_table_group,
                            color_format,
                            encode: key.encode,
                            pre_exposure: cx.modes.pre_exposure,
                            single_pass: scene.quality.visibility_path.single_pass_resolve,
                            lightmap: lightmap_frame.paths.vis_resolve(),
                        },
                        vis_drawn,
                        &mut rec.draw_counts,
                    )?;
                    forward_prep = Some((gpu_color, single_indirect));
                }
                FrameSite::Subsurface => {
                    // Scattering materials' draws and their blur, before the forward pass loads the image.
                    crate::pipeline_warm::warm_checkpoint()?;
                    self.dispatch_nodes(&[BuiltinNode::SubsurfaceDraws, BuiltinNode::SubsurfaceBlur]);
                    subsurface_counts = subsurface_gpu::record_subsurface(
                        subsurface_gpu::SubsurfaceFields {
                            state: &mut self.subsurface,
                            transients: &mut self.transients,
                            entries: &mut self.entries,
                            meshes: &self.meshes,
                            materials: &self.materials,
                            objects: &self.objects,
                            belt: &mut self.belt,
                        },
                        d,
                        &mut rec.encoder,
                        &subsurface_frame,
                        &subsurface_gpu::SubsurfaceInputs {
                            scene,
                            load_color: target.load_color || vis_resolved,
                            draws: &draws,
                            opaque_ix: &opaque_ix,
                            view_bg: &view_bg,
                            depth_view: &depth_view,
                            hdr_view: &hdr_view,
                            texture_table_group: &texture_table_group,
                            unjittered_proj: cx.modes.unjittered_proj,
                            view_proj: cx.modes.view_proj,
                            internal_w: cx.modes.internal_w,
                            internal_h: cx.modes.internal_h,
                            pre_exposure: cx.modes.pre_exposure,
                            noise_index: temporal_noise_index(cx.modes.taau_on, cx.open.temporal_index),
                            trims: rec.trims,
                            main_view: !cx.open.view_pass,
                        },
                        &mut rec.binds,
                    )?;
                }
                FrameSite::Forward => {
                    let frame_pipes = steps::recording::site_input(&frame_pipes, "frame_pipes")?;
                    let (gpu_color, single_indirect) =
                        steps::recording::site_input(&forward_prep, "gpu_color")?;
                    // Screen-space refraction and the glass instances' groups (`forward_pass`).
                    let forward_pass::RefractionPrep {
                        front_views,
                        refract,
                        glass_bgs,
                        canopy_cheap,
                    } = self.prepare_refraction(
                        d,
                        q,
                        &forward_pass::RefractionInputs {
                            scene,
                            hdr_texture: &hdr_texture,
                            depth_view: &depth_view,
                            draws: &draws,
                            blend_ix: &blend_ix,
                            refract_bgl: &frame_pipes.refract_bgl,
                            refract_sampler: &frame_pipes.refract_sampler,
                            glass_pipes: &frame_pipes.glass_pipes,
                            glass_pipes_ds: &frame_pipes.glass_pipes_ds,
                            fov_y: cx.modes.fov_y,
                            internal_h: cx.modes.internal_h,
                            samples,
                        },
                    );

                    let oit_pass = oit_gpu::prepare_oit(
                        &mut self.oit,
                        &mut self.transients,
                        d,
                        std::mem::take(&mut oit_frame),
                        &oit_gpu::OitPrepare {
                            hdr_view: &hdr_view,
                            draws: &draws,
                            meshes: &self.meshes,
                            view_proj: cx.modes.view_proj,
                            internal: [cx.modes.internal_w, cx.modes.internal_h],
                            main_view: !cx.open.view_pass,
                        },
                    );
                    oit_reactive = oit_pass.as_ref().map(|pass| pass.reactive().clone());
                    prepared_sky =
                        self.prepare_frame_sky(device, q, scene, &cx.modes, &depth_view, color_format)?;
                    crate::pipeline_warm::warm_checkpoint()?;
                    // Forward colour (the transparency accumulation records in its middle).
                    self.dispatch_nodes(&[BuiltinNode::Forward]);
                    self.dispatch_node(BuiltinNode::OitAccum, oit_pass.is_some());
                    forward_pass::record_forward(
                        forward_pass::ForwardFields {
                            entries: &mut self.entries,
                            meshes: &mut self.meshes,
                            materials: &mut self.materials,
                            objects: &mut self.objects,
                            mask_decals: &mut self.mask_decals,
                            glass: &mut self.glass,
                            shared_draw: &mut self.shared_draw,
                        },
                        &mut rec.encoder,
                        &frame_pipes.forward_pipes(&lite_colors),
                        &forward_pass::ForwardInputs {
                            scene,
                            target: &target,
                            draws: &draws,
                            opaque_ix: &opaque_ix,
                            blend_ix: &blend_ix,
                            graph_sets: &graph_sets,
                            gpu_color,
                            glass_pipes: &frame_pipes.glass_pipes,
                            glass_pipes_ds: &frame_pipes.glass_pipes_ds,
                            glass_bgs: &glass_bgs,
                            mask_sets: &mask_sets,
                            lightmap: &lightmap_frame,
                            film: &film_frame,
                            single_indirect,
                            texture_table_group: &texture_table_group,
                            timer: &rec.timer,
                            view_bg: &view_bg,
                            depth_view: &depth_view,
                            msaa_depth: &msaa_depth,
                            msaa_color: &msaa_color,
                            hdr_view: &hdr_view,
                            front_views: &front_views,
                            refract: &refract,
                            viewport: cx.modes.viewport,
                            strategy: cx.modes.strategy,
                            attach_w,
                            attach_h,
                            internal_w: cx.modes.internal_w,
                            internal_h: cx.modes.internal_h,
                            samples,
                            pre_exposure: cx.modes.pre_exposure,
                            airless: cx.modes.airless,
                            canopy_cheap,
                            in_pass_stamps: rec.in_pass_stamps,
                            linear_hdr: cx.modes.linear_hdr,
                            sky_pass: cx.modes.sky_pass,
                            physical_sky: prepared_sky.as_ref(),
                            trims: rec.trims,
                            vis_resolved,
                            subsurface_drawn: subsurface_counts.is_some(),
                            oit: oit_pass.as_ref(),
                        },
                        forward_pass::ForwardState {
                            binds: &mut rec.binds,
                            draw_counts: &mut rec.draw_counts,
                            glass_stamped: &mut rec.glass_stamped,
                            shared_color_stamped: &mut rec.shared_color_stamped,
                        },
                    )?;
                    if let Some(counts) = subsurface_counts.take() {
                        rec.draw_counts.opaque = rec.draw_counts.opaque.plus(counts);
                    }
                    if self.dispatch_node(BuiltinNode::OitComposite, oit_pass.is_some())
                        && let Some(pass) = oit_pass
                    {
                        self.oit.finish(pass);
                    }
                    // A texture view's draws are part of the host's frame, not the frame's own counts.
                    if !cx.open.view_pass {
                        self.draw_counts = rec.draw_counts;
                    }
                    rec.finish_forward_timestamps();
                    // After the forward stamp, so the forward time is the colour pass alone (the terrain
                    // virtual texture's feedback reduction lands in the next bucket).
                    if samples == 1
                        && !cx.open.view_pass
                        && let Some(draw) = self.shared_draw.as_ref()
                    {
                        draw.encode_after_color(&mut rec.encoder);
                    }
                    front_layers = Some(front_views);
                }
                FrameSite::Slot(GraphSlot::AfterOpaque) => {
                    // Host passes over the lit opaque HDR image (ADR-1552 `after_opaque`).
                    if let Some(color) = hdr_view.as_ref() {
                        self.extension_passes.run_slot_view(
                            crate::GraphSlot::AfterOpaque,
                            extension::SlotFrame {
                                capturing_probe: self.capturing_probe,
                                frame_index: self.frame_index,
                                texture_view: self.view_pass,
                            },
                            d,
                            q,
                            &mut rec.encoder,
                            &extension::SlotView::whole(color, slot_camera),
                        );
                    }
                }
                FrameSite::ScreenSpace => {
                    let frame_pipes = steps::recording::site_input(&frame_pipes, "frame_pipes")?;
                    let screen = steps::recording::site_input(&screen, "screen_reuse")?;
                    let front_views = steps::recording::site_input(&front_layers, "front_views")?;
                    // Screen-space GI / SSR need a single-sample depth prepass and an HDR
                    // colour they can sample. The lab baseline and the agent determinism
                    // capture (depth prepass off) skip both, so their pixels stay put.
                    // A static hit reuses the previous composite (this frame's raw HDR is
                    // not the post input). `screen_space` was computed before the AO pass.
                    self.dispatch_nodes(&[BuiltinNode::RadianceCache, BuiltinNode::Ssgi, BuiltinNode::Ssr]);
                    shaded = self.record_screen_space(
                        d,
                        &mut rec.encoder,
                        &screen_space::ScreenSpaceInputs {
                            scene,
                            hdr_view: &hdr_view,
                            depth_view: &depth_view,
                            front_views,
                            white_view: &frame_pipes.white_view,
                            timer: &rec.timer,
                            screen_key: screen.screen_key,
                            unjittered_proj: cx.modes.unjittered_proj,
                            proj: cx.modes.proj,
                            view_proj: cx.modes.view_proj,
                            internal_w: cx.modes.internal_w,
                            internal_h: cx.modes.internal_h,
                            color_format,
                            pre_exposure: cx.modes.pre_exposure,
                            temporal_index: cx.open.temporal_index,
                            taau_frozen: screen.taau_frozen,
                            reuse_hit: screen.reuse_hit,
                            screen_space: screen.screen_space,
                            screen_space_gi: cx.modes.gi_quality.screen_space_gi,
                            taau_on: cx.modes.taau_on,
                            ssgi_half: screen.ssgi_half,
                            gather_reuse: screen.gather_reuse,
                            ssgi_relative: screen.ssgi_relative,
                            ssr_wanted: screen.ssr_wanted,
                            sssr_frame: screen.sssr_frame,
                            view_pass: cx.open.view_pass,
                        },
                    )?;
                }
                FrameSite::Outdoor => {
                    let screen = steps::recording::site_input(&screen, "screen_reuse")?;
                    self.dispatch_nodes(&[BuiltinNode::Atmosphere, BuiltinNode::Clouds, BuiltinNode::Fog]);
                    shaded = self.record_outdoor(
                        device,
                        d,
                        q,
                        &mut rec.encoder,
                        shaded.take(),
                        &outdoor_passes::OutdoorInputs {
                            scene,
                            depth_view: &depth_view,
                            timer: &rec.timer,
                            plan: &plan,
                            csm_views: &csm_views,
                            uploaded: &uploaded,
                            directional: &directional,
                            view_proj: cx.modes.view_proj,
                            unjittered_vp: cx.modes.unjittered_vp,
                            prev_vp: cx.modes.prev_vp,
                            csm_size,
                            attach_w,
                            attach_h,
                            samples,
                            color_format,
                            pre_exposure: cx.modes.pre_exposure,
                            linear_hdr: cx.modes.linear_hdr,
                            planet_sky: cx.modes.planet_sky,
                            prepared_sky: prepared_sky.as_ref(),
                            taau_frozen: screen.taau_frozen,
                            screen_keyed: screen.screen_key.is_some(),
                        },
                    )?;
                    // The interval after this cut is the velocity and TAA resolve.
                    if cx.modes.linear_hdr
                        && (cx.modes.scaled || cx.modes.taau_on)
                        && !cx.modes.sgsr_on
                        && shaded.is_some()
                    {
                        self.finish_workers.cut(d, &mut rec.encoder, &mut rec.segments);
                    }
                }
                FrameSite::Temporal => {
                    let screen = steps::recording::site_input(&screen, "screen_reuse")?;
                    crate::pipeline_warm::warm_checkpoint()?;
                    // Temporal upscale (or a bilinear upsample) into an output-sized HDR view.
                    self.dispatch_nodes(&[BuiltinNode::Velocity, BuiltinNode::Taau]);
                    upscaled = self.record_temporal(
                        device,
                        d,
                        &mut rec.encoder,
                        &mut rec.segments,
                        &temporal::TemporalInputs {
                            scene,
                            draws: &draws,
                            opaque_ix: &opaque_ix,
                            graph_sets: &graph_sets,
                            packed_joints: &packed_joints,
                            shaded: &shaded,
                            depth_view: &depth_view,
                            timer: &rec.timer,
                            view_proj: cx.modes.view_proj,
                            unjittered_vp: cx.modes.unjittered_vp,
                            prev_vp: cx.modes.prev_vp,
                            jitter: cx.modes.jitter,
                            width,
                            height,
                            internal_w: cx.modes.internal_w,
                            internal_h: cx.modes.internal_h,
                            samples,
                            pre_exposure: cx.modes.pre_exposure,
                            post_exposure: cx.modes.post_exposure,
                            history_rescale: cx.modes.history_rescale,
                            taau_frozen: screen.taau_frozen,
                            linear_hdr: cx.modes.linear_hdr,
                            scaled: cx.modes.scaled,
                            taau_on: cx.modes.taau_on,
                            sgsr_on: cx.modes.sgsr_on,
                            trims: rec.trims,
                            vis_drawn,
                            reactive: oit_reactive.as_ref(),
                        },
                    )?;

                    // The interval after this cut is the post stack (and the translucent layers).
                    if cx.modes.forced_post.is_some() {
                        self.finish_workers.cut(d, &mut rec.encoder, &mut rec.segments);
                    }
                }
                FrameSite::TranslucentHdr => {
                    let screen = steps::recording::site_input(&screen, "screen_reuse")?;
                    // Translucent layers draw into the pre-tonemap HDR buffer so bloom sees cores above
                    // 1, and decals sit under the fire. Same encoder as the scene (ADR-037): no second
                    // submit. MSAA and Rg11b10 (no alpha) stay on the display pass. On a TAAU frame
                    // (Desktop High) they draw after the resolve, into a copy of its output-sized
                    // Rgba16Float result (the resolve is next frame's history, so it stays clean):
                    // effects are not smeared by the history and still get exposure, bloom and the
                    // tonemap. The output-sized depth comes from `depth_for_output` (ADR-041).
                    // Layers with nothing to draw leave the frame as if none were installed (ADR-2585).
                    let layers_live =
                        !cx.open.view_pass && self.translucent.iter().any(|layer| layer.is_active());
                    hdr_particles = samples == 1
                        && cx.modes.forced_post.is_some()
                        // An SGSR frame (scaled, no HDR upscale) keeps its HDR image at the internal size.
                        && (upscaled.is_some()
                            || (color_format == wgpu::TextureFormat::Rgba16Float && shaded.is_some() && !cx.modes.scaled))
                        && layers_live
                        && self.translucent.iter().any(|layer| layer.participates_in_hdr());
                    // The HDR image the layers draw over, and its format.
                    let layer_hdr = if hdr_particles {
                        upscaled.clone().or_else(|| shaded.clone())
                    } else {
                        None
                    };
                    let layer_hdr_format = if upscaled.is_some() {
                        wgpu::TextureFormat::Rgba16Float
                    } else {
                        color_format
                    };

                    // Output-sized depth for the decal receiver and the translucent pass.
                    // `depth_for_output` copies when a scaled frame's scene depth is smaller
                    // (ADR-041), and resolves an MSAA depth to one sample (the colour pass wrote
                    // only the multisampled attachment). Decals sample this view, so the copy is
                    // recorded before simulate builds the decal bind group.
                    crate::pipeline_warm::warm_checkpoint()?;
                    // ---- Translucent layers (VFX): simulate after the colour passes are recorded ----
                    // Effects simulate once per host frame, with the main camera: a texture view draws none.
                    if layers_live {
                        output_depth = Some(self.simulate_translucent_layers(
                            device,
                            d,
                            &mut rec.encoder,
                            scene,
                            &target,
                            &display_passes::SimulateInputs {
                                layer_hdr: &layer_hdr,
                                layer_hdr_format,
                                hdr_particles,
                                fov_y: cx.modes.fov_y,
                                width,
                                height,
                                post_exposure: cx.modes.post_exposure,
                                pre_exposure: cx.modes.pre_exposure,
                                directional: &directional,
                                msaa_depth: &msaa_depth,
                                depth_view: &depth_view,
                                attach: (attach_w, attach_h),
                            },
                        )?);
                    }

                    // Receiver stamps for the decal pass (`steps::receiver_stamps`).
                    stamps = self.receiver_stamps(&draws, &output_depth, rec.trims, &mut rec.submit_stats);

                    let hdr_layers = hdr_particles && output_depth.is_some() && layer_hdr.is_some();
                    if self.dispatch_node(BuiltinNode::TranslucentHdr, hdr_layers)
                        && let Some(layer_depth) = output_depth.as_ref()
                        && let Some(hdr) = layer_hdr.clone()
                    {
                        let draw_target = self.record_translucent_hdr(
                            d,
                            q,
                            &mut rec.encoder,
                            hdr,
                            &display_passes::HdrLayerInputs {
                                layer_depth,
                                color_format: layer_hdr_format,
                                stamps: &stamps,
                                view_proj: cx.modes.view_proj,
                                samples,
                                keep_source: screen.screen_key.is_some() || upscaled.is_some(),
                                double_sided: scene.double_sided,
                            },
                        )?;
                        if upscaled.is_some() {
                            upscaled = Some(draw_target);
                        } else {
                            shaded = Some(draw_target);
                        }
                    }
                }
                FrameSite::Slot(GraphSlot::AfterTransparent) => {
                    // Host passes over the HDR image after the translucent layers (ADR-1552 `after_transparent`).
                    if let Some(color) = upscaled.as_ref().or(shaded.as_ref()) {
                        self.extension_passes.run_slot_view(
                            crate::GraphSlot::AfterTransparent,
                            extension::SlotFrame {
                                capturing_probe: self.capturing_probe,
                                frame_index: self.frame_index,
                                texture_view: self.view_pass,
                            },
                            d,
                            q,
                            &mut rec.encoder,
                            &extension::SlotView::whole(color, slot_camera),
                        );
                    }
                }
                // `before_post` records inside the post stack (`post_stack.rs`).
                FrameSite::Slot(GraphSlot::BeforePost) => {}
                FrameSite::PostStack => {
                    let screen = steps::recording::site_input(&screen, "screen_reuse")?;
                    crate::pipeline_warm::warm_checkpoint()?;
                    // ---- Post stack, in the same encoder as the scene ----
                    if self.dispatch_node(BuiltinNode::PostStack, cx.modes.forced_post.is_some())
                        && let Some(settings) = cx.modes.forced_post.as_ref()
                    {
                        cx.open.readback_ms += self.record_post_stack(
                            d,
                            q,
                            &mut rec.encoder,
                            scene,
                            &target,
                            &post_stack::PostStackInputs {
                                settings,
                                upscaled: &upscaled,
                                shaded: &shaded,
                                width,
                                height,
                                internal_w: cx.modes.internal_w,
                                internal_h: cx.modes.internal_h,
                                msaa_depth: &msaa_depth,
                                depth_view: &depth_view,
                                unjittered_vp: cx.modes.unjittered_vp,
                                unjittered_proj: cx.modes.unjittered_proj,
                                pre_exposure: cx.modes.pre_exposure,
                                samples,
                                taau_on: cx.modes.taau_on,
                                taau_frozen: screen.taau_frozen,
                                fov_y: cx.modes.fov_y,
                                viewport: cx.modes.viewport,
                                screen_key_some: screen.screen_key.is_some(),
                                sgsr_on: cx.modes.sgsr_on,
                            },
                        )?;
                    }
                }
                FrameSite::TranslucentLayers => {
                    crate::pipeline_warm::warm_checkpoint()?;
                    // ---- Translucent layers (VFX): final colour pass over the tonemapped image ----
                    // Draw depth attachment is read-only (`depth_ops: None`): effect shaders sample the
                    // scene depth for soft particles, decal projection and depth collision.
                    // The receiver stamp, the terrain receiver and the scene-colour copy are recorded
                    // on this encoder, before the pass, so the frame still submits once (ADR-037).
                    // Depth is the output-sized view from `depth_for_output` (ADR-041). Pixels no mesh
                    // and no heightfield cover stay the terrain clear (layer 0, world +Y). On an HDR
                    // frame the decals and the participating layers already drew before the post stack.
                    // On an HDR frame where every layer participates, they all drew before the post stack:
                    // this display-referred pass would record nothing, so it is not begun (ADR-508).
                    let ldr_layers_draw =
                        !hdr_particles || self.translucent.iter().any(|layer| !layer.participates_in_hdr());
                    if rec.trims && !ldr_layers_draw && output_depth.is_some() {
                        rec.submit_stats.passes_culled += 1;
                    }
                    let ldr_layers = output_depth.is_some() && (!rec.trims || ldr_layers_draw);
                    if self.dispatch_node(BuiltinNode::TranslucentLayers, ldr_layers)
                        && let Some(layer_depth) = output_depth.as_ref()
                    {
                        let depth = self.ldr_layer_depth(device, &mut rec.encoder, layer_depth, &target)?;
                        self.record_translucent_layers(
                            d,
                            q,
                            &mut rec.encoder,
                            scene,
                            &target,
                            &display_passes::LayerInputs {
                                layer_depth: &depth,
                                hdr_particles,
                                stamps: &stamps,
                                view_proj: cx.modes.view_proj,
                                viewport: cx.modes.viewport,
                                samples,
                            },
                        )?;
                    }
                }
                FrameSite::Slot(GraphSlot::AfterPost) => {
                    crate::pipeline_warm::warm_checkpoint()?;
                    // ---- Host passes over the display image (ADR-1552 `after_post`) ----
                    self.run_extension_slot(
                        crate::GraphSlot::AfterPost,
                        d,
                        q,
                        &mut rec.encoder,
                        &extension::SlotView::display(&target, cx.modes.viewport, slot_camera),
                    );
                }
                FrameSite::Overlays => {
                    // ---- Scalar overlays and gizmos over the finished image ----
                    self.dispatch_nodes(&[BuiltinNode::ScalarOverlays, BuiltinNode::Gizmos]);
                    self.record_overlays(
                        d,
                        q,
                        &mut rec.encoder,
                        &target,
                        &display_passes::OverlayInputs {
                            msaa_depth: &msaa_depth,
                            depth_view: &depth_view,
                            viewport: cx.modes.viewport,
                            unjittered_vp: cx.modes.unjittered_vp,
                            unjittered_proj: cx.modes.unjittered_proj,
                        },
                    );
                }
                FrameSite::Slot(GraphSlot::Overlay) => {
                    // ---- Host overlay passes, last over the display image (ADR-1552 `overlay`) ----
                    self.run_extension_slot(
                        crate::GraphSlot::Overlay,
                        d,
                        q,
                        &mut rec.encoder,
                        &extension::SlotView::display(&target, cx.modes.viewport, slot_camera),
                    );
                }
            }
            // Read-only copies of what this site wrote, only for a requested capture (ADR-7860).
            self.copy_site(
                d,
                &mut rec,
                [&upscaled, &shaded, &hdr_view],
                &depth_view,
                &target,
                &cx.modes,
            );
        }

        // Every held built-in node was entered once, in dependency order (or is counted).
        self.extension_passes.dispatch.finish();
        // ---- Publish copies, timestamp resolve and the frame's one submit (`steps::submit`) ----
        if let Some(cuts) = pass_cuts {
            cuts.finish(&mut rec.segments);
        }
        let (submit_count, submit_ms) = self.submit_frame(q, rec.encoder, rec.segments)?;

        self.finish_frame(frame_end::FrameEnd {
            frame_started: cx.open.frame_started,
            submit_ms,
            readback_ms: cx.open.readback_ms,
            submit_count,
            shadow_cpu: cx.shadow_cpu,
            view_pass: cx.open.view_pass,
            binds: rec.binds,
            submit_stats: rec.submit_stats,
            stamps,
            unjittered_vp: cx.modes.unjittered_vp,
            draws,
            packed_joints,
            taau_on: cx.modes.taau_on,
            width,
            height,
            now: cx.open.now,
            commit_csm,
            commit_atlas,
            paging,
            uploaded,
            opaque_ix,
            blend_ix,
            shadow_ix,
            trans_ix,
            csm_splits,
            caster_info,
            trans_info,
            cascade_uv_lists,
            casters,
            caster_slot,
            shadow_jobs,
        });
        Ok(())
    }
}

/// Whether a draw joins the translucent shadow list. A ghost or x-ray override casts no shadow
/// (it is a see-through highlight, not geometry), and the translucency maps read the material,
/// not the override, so a blend-overridden draw would otherwise cast the material's full shadow.
fn casts_translucent_shadow(alpha_mode: u32, shadow: ShadowCast, over: [u32; 2]) -> bool {
    alpha_mode >= ALPHA_BLEND
        && alpha_mode != ALPHA_ADDITIVE
        && shadow != ShadowCast::Off
        && over[1] & crate::render_override::OVERRIDE_BLEND == 0
}

/// Whether a shadow caster batches by its material in the instanced shadow lists so its batch
/// can bind the material for the alpha test: a masked caster under alpha-tested shadows
/// ([`FrameRenderer::set_alpha_tested_shadows`]). Hair keeps its translucency-map shadow; every
/// other caster batches by mesh alone (material 0).
fn shadow_batches_by_material(alpha_tested: bool, alpha_mode: u32, hair: bool) -> bool {
    alpha_tested && alpha_mode == crate::ALPHA_MASK && !hair
}

/// Frame index (mod 16) for screen-space noise under temporal AA; 0 otherwise.
fn temporal_noise_index(taa: bool, frame_index: u64) -> f32 {
    if taa { (frame_index % 16) as f32 } else { 0.0 }
}

// ---------------------------------------------------------------------------
// Pipeline / module construction (once per configuration)
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Upload helpers
// ---------------------------------------------------------------------------

/// This frame's and last frame's palette of a skinned draw (`joint_base`, `joint_count` in
/// `packed`). The previous palette is `prev_joints[start..start + count]` when the object drew
/// last frame with the same joint count; otherwise (first frame, a camera cut, a changed joint
/// count) it is the current palette, and only the model matrix moves the object.
fn skin_palettes<'a>(
    packed: &'a [JointMatrix],
    prev_joints: &'a [JointMatrix],
    prev: Option<(u32, u32)>,
    joint_base: u32,
    joint_count: u32,
) -> (&'a [JointMatrix], &'a [JointMatrix]) {
    let span = |joints: &'a [JointMatrix], start: u32, count: u32| {
        let start = start as usize;
        joints.get(start..start + count as usize)
    };
    let current = span(packed, joint_base, joint_count).unwrap_or(&[]);
    let previous = prev
        .filter(|&(_, count)| count == joint_count)
        .and_then(|(start, count)| span(prev_joints, start, count))
        .unwrap_or(current);
    (current, previous)
}

/// Generation of [`signal_memory_pressure`] calls.
static MEMORY_PRESSURE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Asks every [`FrameRenderer`] to free its idle GPU memory ([`FrameRenderer::trim_memory`])
/// at its next frame. The render plugin calls it on the OS low-memory event
/// (`LifecycleEvent::LowMemory`); a host may call it on its own memory-budget signal.
pub fn signal_memory_pressure() {
    MEMORY_PRESSURE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// A [`FrameRenderer`] pinned to the [`RenderDevice`] its cached GPU resources
/// were created on.
///
/// Long-lived callers (the editor viewport, the material preview, agent
/// screenshots) keep one renderer for the process lifetime. That is correct
/// for a game, which owns exactly one device — but headless tests and tools
/// legitimately create several devices per process, and a renderer that cached
/// buffers on a dropped device panics in wgpu ("cannot get non-existent
/// resource") on its next frame. This wrapper compares the incoming device
/// with the one the caches were built for and transparently rebuilds the
/// renderer when it changed (pipelines recompile once per device switch).
///
/// ```
/// use regia_render::DeviceFrameRenderer;
///
/// let mut scoped = DeviceFrameRenderer::new();
/// // scoped.get(&device) — same device reuses the renderer,
/// // a different device rebuilds it.
/// ```
#[derive(Default)]
pub struct DeviceFrameRenderer {
    device: Option<u64>,
    renderer: FrameRenderer,
}

impl DeviceFrameRenderer {
    /// An empty scope: no device bound yet, an empty renderer.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// The renderer for `device`, rebuilt if the device changed since the last
    /// frame.
    pub fn get(&mut self, device: &RenderDevice) -> &mut FrameRenderer {
        let id = device.device_id();
        if self.device != Some(id) {
            self.renderer = FrameRenderer::new();
            self.device = Some(id);
        }
        &mut self.renderer
    }
}

// ---------------------------------------------------------------------------
// Bind group layout entries (mirror the lab frame contract)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #[test]
    fn only_masked_non_hair_casters_batch_by_material_under_alpha_tested_shadows() {
        use super::shadow_batches_by_material;
        // On: a masked caster binds its material; opaque and blended ones batch by mesh.
        assert!(shadow_batches_by_material(true, crate::ALPHA_MASK, false));
        assert!(!shadow_batches_by_material(true, crate::ALPHA_OPAQUE, false));
        assert!(!shadow_batches_by_material(true, crate::ALPHA_BLEND, false));
        // Hair keeps its translucency-map shadow.
        assert!(!shadow_batches_by_material(true, crate::ALPHA_MASK, true));
        // Off (the default): every caster batches by mesh, as before.
        assert!(!shadow_batches_by_material(false, crate::ALPHA_MASK, false));
    }

    #[test]
    fn ordered_cull_defaults_on_the_other_two_tree_switches_stay_off() {
        let mut renderer = super::FrameRenderer::new();
        // The order-fixed pack is the default (two runs of one submission draw the
        // same image); the atomicAdd pack is the explicit speed switch.
        assert!(renderer.ordered_gpu_cull());
        assert!(!renderer.taa_hashed_alpha());
        assert!(!renderer.alpha_tested_shadows());
        renderer.set_ordered_gpu_cull(false);
        assert!(!renderer.ordered_gpu_cull());
        renderer.set_ordered_gpu_cull(true);
        renderer.set_taa_hashed_alpha(true);
        renderer.set_alpha_tested_shadows(true);
        assert!(renderer.ordered_gpu_cull());
        assert!(renderer.taa_hashed_alpha());
        assert!(renderer.alpha_tested_shadows());
    }

    /// Muse's read-only review of fix-render6 (finding 2): a shader-epoch rebuild keeps the
    /// complete-frames switch but dropped the exposure wait the switch forces, so the post
    /// stack came back with waits off and the map-callback metering nondeterminism the switch
    /// closes reopened until it was toggled. A plain host wait must survive the rebuild too.
    #[test]
    fn a_shader_epoch_rebuild_keeps_the_exposure_wait() {
        // Forced by the complete-frames switch: the rebuild must keep it.
        let mut renderer = super::FrameRenderer::new();
        renderer.set_complete_frames(true);
        assert!(renderer.waits_for_exposure_readback());
        crate::shader_reload::clear_shader_overrides();
        assert!(renderer.sync_shader_epoch());
        assert!(renderer.complete_frames());
        assert!(
            renderer.waits_for_exposure_readback(),
            "the rebuild keeps the wait the switch forces"
        );
        renderer.set_complete_frames(false);
        assert!(!renderer.waits_for_exposure_readback());

        // A plain host wait survives the rebuild as well.
        let mut host_wait = super::FrameRenderer::new();
        host_wait.set_wait_for_exposure_readback(true);
        crate::shader_reload::clear_shader_overrides();
        assert!(host_wait.sync_shader_epoch());
        assert!(host_wait.waits_for_exposure_readback());
        host_wait.set_wait_for_exposure_readback(false);

        // The default stays off.
        let mut off = super::FrameRenderer::new();
        crate::shader_reload::clear_shader_overrides();
        assert!(off.sync_shader_epoch());
        assert!(!off.waits_for_exposure_readback());
    }

    #[test]
    fn a_ghost_or_xray_override_casts_no_translucent_shadow() {
        use super::{ALPHA_ADDITIVE, ALPHA_BLEND, ShadowCast, casts_translucent_shadow};
        let blend = crate::render_override::OVERRIDE_BLEND;
        // A blended material casts its translucent shadow; an opaque one does not.
        assert!(casts_translucent_shadow(ALPHA_BLEND, ShadowCast::All, [0, 0]));
        assert!(!casts_translucent_shadow(0, ShadowCast::All, [0, 0]));
        assert!(!casts_translucent_shadow(ALPHA_ADDITIVE, ShadowCast::All, [0, 0]));
        assert!(!casts_translucent_shadow(ALPHA_BLEND, ShadowCast::Off, [0, 0]));
        // The same draw ghosted (the effective mode is blend) casts none.
        assert!(!casts_translucent_shadow(
            ALPHA_BLEND,
            ShadowCast::All,
            [0, blend]
        ));
    }

    #[test]
    fn fog_reads_the_first_cascade_that_reaches_its_shadow_range() {
        let mut csm = crate::ShadowUniforms {
            cascades: [crate::ShadowUniform::IDENTITY; crate::forward::CASCADE_COUNT as usize],
            count: [4.0, 0.0, 0.0, 0.0],
        };
        // The 12 km ladder: 40 / 200 / 962 / 4000 m. Cascade 0 is too short for shafts.
        for (c, split) in csm.cascades.iter_mut().zip([40.0, 200.0, 962.1, 4000.0]) {
            c.split_distance = split;
        }
        assert_eq!(fog_cascade_index(&csm, 4), 1);
        // Short view ranges (Desktop High at 400 m: 25.6 / 54.7 / 112.7 / 400): cascade 3.
        for (c, split) in csm.cascades.iter_mut().zip([25.6, 54.7, 112.7, 400.0]) {
            c.split_distance = split;
        }
        assert_eq!(fog_cascade_index(&csm, 4), 3);
        // Nothing reaches 150 m: the last drawn cascade.
        csm.count[0] = 2.0;
        assert_eq!(fog_cascade_index(&csm, 4), 1);
    }

    #[test]
    fn a_far_view_plane_keeps_the_first_sun_cascade_near_the_camera() {
        // A 12 km far plane (the parity harness, render_lab planets at 20 km) used to split
        // four cascades over the whole range: the first reached ~751 m, ~0.8 m per texel at
        // 2048², and a 0.2 m truss cast nothing. The shadow distance caps the range at 4 km
        // (look-r2 round 5) and the split ladder keeps the near cascades tight (vfx-look
        // round 3): 16 m / 69 m / 300 m / 4 km (first cap 40 m before ADR-115, dyn-light;
        // last split 962 m before ADR-140, csm-far).
        assert_eq!(csm_far(12_000.0), CSM_MAX_DISTANCE);
        assert_eq!(csm_far(300.0), 300.0, "a near far plane is kept");
        assert_eq!(csm_far(f32::INFINITY), CSM_MAX_FAR);
        let uncapped = crate::cascade_splits(0.1, 12_000.0, 4, 0.75)[0];
        assert!(uncapped > 700.0, "the old first split {uncapped:.0} m");
        let b = crate::csm_boundaries(0.1, csm_far(12_000.0), 4);
        assert_eq!(b.len(), 5);
        assert!((b[1] - 16.0).abs() < 1e-3, "{b:?}");
        assert!((b[2] - 69.28).abs() < 1e-2, "{b:?}");
        assert!((b[3] - 300.0).abs() < 0.5, "{b:?}");
        assert_eq!(b[4], CSM_MAX_DISTANCE);
        // Casters up to 2 km toward the sun reach every cascade.
        assert_eq!(cascade_back(20.0, csm_far(12_000.0)), CSM_CASTER_REACH);
        assert_eq!(
            cascade_back(20.0, 60.0),
            60.0,
            "a short view range reaches its own far plane"
        );
        assert_eq!(
            cascade_back(3000.0, 4000.0),
            3000.0,
            "never closer than one radius"
        );
    }

    /// Splits as Desktop High fits them (λ 0.75, 400 m): receivers 0–25.6, 25.6–54.7, …
    const SPLITS: [f32; 4] = [25.6, 54.7, 112.7, 400.0];

    #[test]
    fn local_casters_go_to_their_own_cascade_and_the_next() {
        let rock = ShadowCast::Local {
            reach: 2.0,
            max_cascades: 2,
            shadow_scale: 1.0,
            shadow_lift: 0.0,
            anchor: None,
            lod_switch_keeps_pages: false,
        };
        // Mid cascade 0: only cascade 0.
        assert_eq!(cascade_mask(rock, 10.0, 0.5, &SPLITS), 0b0001);
        // Near the 0/1 split: the shadow reaches cascade 1's receivers too.
        assert_eq!(cascade_mask(rock, 24.0, 0.5, &SPLITS), 0b0011);
        // In cascade 1's blend band toward cascade 2: 1 and 2.
        assert_eq!(cascade_mask(rock, 52.0, 0.5, &SPLITS), 0b0110);
        // Far: only the last cascade.
        assert_eq!(cascade_mask(rock, 300.0, 0.5, &SPLITS), 0b1000);
        // Beyond the shadow distance: nothing.
        assert_eq!(cascade_mask(rock, 500.0, 0.5, &SPLITS), 0);
        // A long reach is capped at two cascades starting at the caster's own.
        let long = ShadowCast::Local {
            reach: 80.0,
            max_cascades: 2,
            shadow_scale: 1.0,
            shadow_lift: 0.0,
            anchor: None,
            lod_switch_keeps_pages: false,
        };
        assert_eq!(cascade_mask(long, 60.0, 1.0, &SPLITS), 0b1100);
        assert_eq!(cascade_mask(ShadowCast::All, 300.0, 0.5, &SPLITS), 0b1111);
        assert_eq!(cascade_mask(ShadowCast::Off, 10.0, 0.5, &SPLITS), 0);
    }
    use super::*;

    #[test]
    fn gpu_structs_match_vec4_layout() {
        assert_eq!(size_of::<GpuLight>(), 64);
        // M5 prefix through post_exposure is 208 bytes. Fog (32) plus quality (16)
        // plus the two mat4s used by motion vectors (64 + 64) plus the PCSS tap
        // counts (16) bring the uniform to 400; the interior box (32) to 432; the
        // planet-shine fill (32) to 464; the specular-AA parameters (16) to 480; the cloud
        // transmittance map parameters (16) to 496; the image light's ground plane, view and
        // background rows plus the local capture boxes (224) to 720; the surface weather rows
        // (32) to 752; the graph clock (16) to 768; the lights-and-views block (304, ADR-7211)
        // to 1072; the ambient volumes' rotations (64, ADR-7275) to 1136.
        // `cluster_build.wgsl` keeps the prefix through `extra`; a larger buffer bound
        // to that smaller uniform is legal.
        // The hard-surface block on `GpuMaterial` is seven extra vec4s (112 bytes)
        // past the four-vec4 base, 176; the volume transmittance vec4 makes 192; the shading
        // model block (five vec4s) 272; the five hair vec4s after it make 352.
        assert_eq!(size_of::<LabFrame>(), 1136);
        assert_eq!(size_of::<GpuMaterial>(), 352);
        assert_eq!(size_of::<LabVertex>(), 96);
        assert_eq!(size_of::<ObjectData>(), 80);
        // mat4(64) + vec4(16) + vec4(16) = 96; WGSL storage-array stride matches.
        assert_eq!(size_of::<ShadowTileGpu>(), 96);
    }

    #[test]
    fn composes_frame_shaders() {
        compose_source(
            include_str!("../shaders/cluster_build.wgsl"),
            "cluster_build.wgsl",
            HashMap::new(),
        )
        .expect("cluster");
        compose_source(
            include_str!("../shaders/lab_forward.wgsl"),
            "lab_forward.wgsl",
            HashMap::new(),
        )
        .expect("forward");
        compose_source(
            include_str!("../shaders/lab_shadow.wgsl"),
            "lab_shadow.wgsl",
            HashMap::new(),
        )
        .expect("shadow");
        let mut defs = HashMap::new();
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
        compose_source(
            include_str!("../shaders/lab_forward.wgsl"),
            "lab_forward.wgsl",
            defs,
        )
        .expect("forward linear variant");
        let mut gpu = HashMap::new();
        gpu.insert("GPU_DRIVEN".to_string(), ShaderDefValue::Bool(true));
        gpu.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
        compose_source(
            include_str!("../shaders/lab_forward.wgsl"),
            "lab_forward.wgsl",
            gpu,
        )
        .expect("gpu driven");
        for (name, source) in [
            ("gpu_cull.wgsl", include_str!("../shaders/gpu_cull.wgsl")),
            ("hiz_copy.wgsl", include_str!("../shaders/hiz_copy.wgsl")),
            ("hiz_down.wgsl", include_str!("../shaders/hiz_down.wgsl")),
            ("ssao.wgsl", include_str!("../shaders/ssao.wgsl")),
            (
                "contact_shadows.wgsl",
                include_str!("../shaders/contact_shadows.wgsl"),
            ),
            ("taau.wgsl", include_str!("../shaders/taau.wgsl")),
            ("taau_sharpen.wgsl", include_str!("../shaders/taau_sharpen.wgsl")),
            ("velocity.wgsl", include_str!("../shaders/velocity.wgsl")),
            ("ssgi.wgsl", include_str!("../shaders/ssgi.wgsl")),
            ("ssr.wgsl", include_str!("../shaders/ssr.wgsl")),
            ("page_clear.wgsl", include_str!("../shaders/page_clear.wgsl")),
        ] {
            compose_source(source, name, HashMap::new()).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }

    #[test]
    fn quality_maps_the_profile_table() {
        let desktop = FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
            crate::profile::RenderProfilePreset::DesktopHigh,
        ));
        assert_eq!(desktop.csm_cascade_count, 4);
        assert_eq!(desktop.csm_resolution, 2048);
        assert_eq!(desktop.local_shadow_atlas_size, 8192);
        assert_eq!(desktop.max_visible_lights, 256);
        assert!(!desktop.interior_ambient);
        assert_eq!(desktop.msaa_samples, 1, "desktop high AA is TAA (post)");
        let mobile_high = FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
            crate::profile::RenderProfilePreset::MobileHigh,
        ));
        // No shipping preset uses MSAA: the shared terrain draw and the planet sky need a
        // single-sample frame (look-r2 round 8).
        assert_eq!(mobile_high.msaa_samples, 1, "mobile high runs FXAA, not MSAA");
        assert_eq!(mobile_high.csm_cascade_count, 2);
        let mobile_low = FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
            crate::profile::RenderProfilePreset::MobileLow,
        ));
        assert_eq!(
            mobile_low.local_shadow_atlas_size, 0,
            "mobile low has no local shadows"
        );
        assert!(desktop.paged_sun_shadows && desktop.ssr && desktop.screen_space_gi);
        assert_eq!(desktop.gi, crate::profile::GiMode::Probes);
        assert!(!mobile_low.ssr && !mobile_low.paged_sun_shadows);
        assert_eq!(FrameQuality::LAB.gi, crate::profile::GiMode::Ambient);
        assert_eq!(
            FrameQuality::LAB,
            FrameQuality::default(),
            "the lab baseline stays the default for FrameScene builders"
        );
    }

    /// A quad centred at `x` on the XZ plane (object space), radius 1.
    fn quad_at(x: f32) -> Vec<LabVertex> {
        let corners = [
            [x - 1.0, 0.0, -1.0],
            [x + 1.0, 0.0, -1.0],
            [x + 1.0, 0.0, 1.0],
            [x - 1.0, 0.0, 1.0],
        ];
        let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        corners
            .into_iter()
            .zip(uvs)
            .map(|(position, uv)| LabVertex {
                position,
                normal: [0.0, 1.0, 0.0],
                uv,
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect()
    }

    fn scene_with_quads(quality: FrameQuality, quads: &[f32]) -> FrameScene<'static> {
        let meshes = quads
            .iter()
            .map(|&x| {
                // Leaked on purpose: the test scene borrows 'static vertex data.
                let vertices: &'static [LabVertex] = Box::leak(quad_at(x).into_boxed_slice());
                FrameMesh {
                    is_static: false,
                    mesh_key: x.to_bits() as u64,
                    material_key: 1,
                    object_key: x.to_bits() as u64,
                    vertices,
                    indices: &[0, 2, 1, 0, 3, 2],
                    material: &LAB_TEST_MATERIAL,
                    model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow: ShadowCast::All,
                    uv1: &[],
                    lod_dither: 0.0,
                }
            })
            .collect::<Vec<_>>();
        FrameScene {
            width: 128,
            height: 128,
            eye: [0.0, 3.0, 6.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 30.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 8_000.0,
            double_sided: false,
            post: None,
            fog: crate::FogSettings::default(),
            directional: DirectionalLightUniform::new([0.0, -1.0, -0.4], [1.0; 3], 100_000.0),
            local_lights: &[],
            quality,
            meshes,
        }
    }

    static LAB_TEST_MATERIAL: LabMaterial = LabMaterial {
        base_color: [0.7, 0.7, 0.7, 1.0],
        metallic: 0.0,
        roughness: 0.6,
        ..LabMaterial::EMPTY
    };

    /// A mesh fully outside the view frustum is not drawn (`specs/13` § 5 step 3).
    #[test]
    fn frustum_culls_meshes_outside_the_view() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        // One quad in front of the camera, one far behind it.
        let scene = scene_with_quads(FrameQuality::LAB, &[0.0, -50.0]);
        let result = renderer.render_offscreen(&device, &scene).expect("render");
        let stats = renderer.draw_stats();
        assert_eq!(stats.drawn, 1, "the in-view quad draws");
        assert_eq!(stats.culled, 1, "the quad behind the camera is culled");
        let max_c = result.pixels.iter().copied().max().unwrap_or(0);
        assert!(max_c > 40, "the surviving quad must be lit, got max={max_c}");
        // The same two meshes with both in view: nothing culled.
        let scene = scene_with_quads(FrameQuality::LAB, &[0.0, 2.0]);
        renderer.render_offscreen(&device, &scene).expect("render 2");
        let stats = renderer.draw_stats();
        assert_eq!(stats.drawn, 2);
        assert_eq!(stats.culled, 0, "partially visible meshes stay visible");
    }

    /// A held camera reuses the cached per-page caster lists; the first frame builds them, a
    /// moved camera rebuilds them.
    #[test]
    fn held_camera_rebuilds_no_caster_lists() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        let mut scene: FrameScene<'_> = scene_with_quads(FrameQuality::LAB, &[0.0, 2.0]);
        scene.directional_shadows = true;
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("caster lists"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let target = FrameTarget::full(&view, format, 64, 64);
        renderer.render_into(&device, &scene, target).expect("warm");
        assert!(
            renderer.shadow_cache_report().caster_lists_rebuilt > 0,
            "the first frame must build the caster lists"
        );
        // A second, identical frame reuses every cached list.
        renderer.render_into(&device, &scene, target).expect("held");
        assert_eq!(
            renderer.shadow_cache_report().caster_lists_rebuilt,
            0,
            "a held camera must rebuild no caster lists"
        );
        // Moving the camera invalidates the cascade matrices and rebuilds.
        scene.eye = [0.5, 3.0, 6.0];
        renderer.render_into(&device, &scene, target).expect("moved");
        assert!(
            renderer.shadow_cache_report().caster_lists_rebuilt > 0,
            "a moved camera must rebuild the lists"
        );
    }

    /// Two identical frames keep the draw-list allocation. The second frame does not grow it.
    #[test]
    fn steady_frames_do_not_grow_the_draw_list() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        let scene = scene_with_quads(FrameQuality::LAB, &[0.0, 2.0]);
        renderer.render_offscreen(&device, &scene).expect("first");
        let (ptr, cap, growths) = renderer.scratch.draw_list_probe();
        assert!(cap >= 2, "the draw list was allocated, capacity {cap}");
        renderer.render_offscreen(&device, &scene).expect("second");
        let (ptr2, cap2, growths2) = renderer.scratch.draw_list_probe();
        assert_eq!(ptr2, ptr, "the draw list was allocated again");
        assert_eq!(cap2, cap);
        assert_eq!(growths2, growths, "the second frame grew the draw list");
    }

    /// A settled stream pass keeps the order list and the in-scene map, and sums resident
    /// bytes once per frame.
    #[test]
    fn settled_stream_pass_does_not_grow_scratch() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        let mut scene = scene_with_quads(FrameQuality::LAB, &[0.0, 2.0]);
        // The frame copies this policy in `stream_frame.begin`. Lab quality ships streaming
        // off, so the pass would not build the map.
        scene.quality.texture_streaming.enabled = true;
        scene.meshes[0].material_key = 1;
        scene.meshes[1].material_key = 2;
        renderer.render_offscreen(&device, &scene).expect("first");
        let (order_cap, order_growths, map_cap, map_growths) = renderer.scratch.stream_pass_probe();
        let scans = renderer.resident_scans;
        assert!(map_cap > 0, "the in-scene map was allocated, capacity {map_cap}");
        assert_eq!(
            scans, 1,
            "the first stream pass scanned resident bytes {scans} times"
        );
        let uploaded = renderer.stream_report().uploaded_bytes;
        renderer.render_offscreen(&device, &scene).expect("second");
        let (order_cap2, order_growths2, map_cap2, map_growths2) = renderer.scratch.stream_pass_probe();
        assert_eq!(order_cap2, order_cap);
        assert_eq!(
            order_growths2, order_growths,
            "the second frame grew the stream order"
        );
        assert_eq!(map_cap2, map_cap);
        assert_eq!(
            map_growths2, map_growths,
            "the second frame grew the in-scene map"
        );
        assert_eq!(
            renderer.resident_scans,
            scans + 1,
            "the second frame scanned twice"
        );
        assert_eq!(renderer.stream_report().uploaded_bytes, uploaded);
    }

    /// Three time-reading graphs share one clock: a later clock does not upload, a later slot does.
    #[test]
    fn time_reading_graphs_share_one_clock_write() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        let scene = scene_with_quads(FrameQuality::LAB, &[0.0]);
        renderer.render_offscreen(&device, &scene).expect("pipes");
        let mut graph = crate::surface_graph::SurfaceGraph::from_code(
            &crate::surface_graph::GraphCode::surface("fn material_surface() {}"),
        );
        graph.uses_time = true;
        let mat = LabMaterial {
            surface_graph: Some(std::sync::Arc::new(graph.clone())),
            ..LabMaterial::EMPTY
        };
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        renderer.set_material_clock(1.0, 0.0);
        for key in [10_u64, 11, 12] {
            renderer
                .ensure_material(&device.device, &queue, key, &mat, 1)
                .expect("first upload");
        }
        assert_eq!(renderer.graph_block_writes, 3, "each graph is written once");
        renderer.set_material_clock(2.0, 1.0);
        for key in [10_u64, 11, 12] {
            renderer
                .ensure_material(&device.device, &queue, key, &mat, 2)
                .expect("clock upload");
        }
        assert_eq!(
            renderer.graph_block_writes, 3,
            "a clock change rewrote a time-reading block"
        );
        let mut changed = graph;
        changed.params[1][0] = 4.0;
        let mat = LabMaterial {
            surface_graph: Some(std::sync::Arc::new(changed)),
            ..LabMaterial::EMPTY
        };
        renderer
            .ensure_material(&device.device, &queue, 10, &mat, 3)
            .expect("param upload");
        assert_eq!(renderer.graph_block_writes, 4, "a later slot did not upload");
    }

    /// A slot-0 value seen under a live clock but never written is written once the clock is
    /// unset (corr-render-frame-upscaler LOW 10): the material's own slot 0 equals the last
    /// clock the block saw, so a compare against that block found nothing to write and the GPU
    /// kept the first clock's slot 0.
    #[test]
    fn a_slot_zero_seen_under_a_clock_uploads_after_the_clock_is_unset() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        let scene = scene_with_quads(FrameQuality::LAB, &[0.0]);
        renderer.render_offscreen(&device, &scene).expect("pipes");
        let mut graph = crate::surface_graph::SurfaceGraph::from_code(
            &crate::surface_graph::GraphCode::surface("fn material_surface() {}"),
        );
        graph.uses_time = true;
        graph.params[0] = [2.0, 1.0, 0.0, 0.0];
        let mat = LabMaterial {
            surface_graph: Some(std::sync::Arc::new(graph)),
            ..LabMaterial::EMPTY
        };
        let queue = crate::device::TrackedQueue::observe(&device.queue);
        renderer.set_material_clock(1.0, 0.0);
        renderer
            .ensure_material(&device.device, &queue, 20, &mat, 1)
            .expect("first upload");
        assert_eq!(renderer.graph_block_writes, 1);
        renderer.set_material_clock(2.0, 1.0);
        renderer
            .ensure_material(&device.device, &queue, 20, &mat, 2)
            .expect("clock frame");
        assert_eq!(renderer.graph_block_writes, 1, "a clock change wrote the block");
        renderer.set_material_time(None);
        renderer
            .ensure_material(&device.device, &queue, 20, &mat, 3)
            .expect("clock off");
        assert_eq!(
            renderer.graph_block_writes, 2,
            "the GPU kept the first clock's slot 0 after the clock was unset"
        );
        renderer
            .ensure_material(&device.device, &queue, 20, &mat, 4)
            .expect("steady");
        assert_eq!(renderer.graph_block_writes, 2, "a steady frame wrote again");
    }

    /// The shadow CPU row ([`FrameRenderer::shadow_cpu_timings`]): a frame with sun shadows
    /// times its plan and its cascade passes; the parts nest inside the plan and add up to the
    /// total; a frame without sun shadows records no cascade encode. Times are only checked for
    /// being recorded and consistent, never against a budget (a loaded box is slower).
    #[test]
    fn shadow_cpu_timings_cover_the_plan_and_the_cascades() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        let mut scene: FrameScene<'_> = scene_with_quads(FrameQuality::LAB, &[0.0, 2.0]);
        scene.directional_shadows = true;
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shadow cpu"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let target = FrameTarget::full(&view, format, 64, 64);
        renderer.render_into(&device, &scene, target).expect("shadowed");
        let t = renderer.shadow_cpu_timings();
        assert!(t.plan_ms > 0.0, "the plan was not timed: {t:?}");
        assert!(t.encode_ms[0] > 0.0, "cascade 0 was drawn but not timed: {t:?}");
        assert!(
            t.shared_prepare_ms + t.caster_lists_ms <= t.plan_ms,
            "the plan's parts exceed it: {t:?}"
        );
        let sum = t.plan_ms + t.vis_cull_ms + t.encode_ms.iter().sum::<f32>() + t.translucent_ms;
        assert!((t.total_ms() - sum).abs() < 1e-6, "{t:?}");
        scene.directional_shadows = false;
        renderer.render_into(&device, &scene, target).expect("unshadowed");
        let t = renderer.shadow_cpu_timings();
        assert_eq!(
            &t.encode_ms[..4],
            &[0.0; 4],
            "no cascade without sun shadows: {t:?}"
        );
    }

    /// A caster whose mesh changes under the same object key (an entity's mesh swapped, an
    /// asset reloaded, a LOD switch) must redraw its shadow pages even when the new mesh fits
    /// the old bounds: the paged cache used to compare bounds only and kept the old shadow.
    /// A [`ShadowCast::Local`] caster that opts into `lod_switch_keeps_pages` keeps them (the
    /// scatter page budget).
    #[test]
    fn a_mesh_swapped_inside_the_same_bounds_redraws_its_shadow_pages() {
        static GROUND: [u32; 6] = [0, 2, 1, 0, 3, 2];
        static QUAD: [u32; 6] = [0, 2, 1, 0, 3, 2];
        // Three corners of the same four vertices: the same bounds, half the shadow.
        static TRIANGLE: [u32; 3] = [0, 2, 1];
        let device = crate::test_device();
        let ground: Vec<LabVertex> = quad_at(0.0)
            .into_iter()
            .map(|mut v| {
                v.position = [v.position[0] * 8.0, 0.0, v.position[2] * 8.0];
                v
            })
            .collect();
        let caster: Vec<LabVertex> = quad_at(0.0)
            .into_iter()
            .map(|mut v| {
                v.position[1] = 1.0;
                v
            })
            .collect();
        let mut quality = FrameQuality::LAB;
        quality.paged_sun_shadows = true;
        let (ground, caster) = (&ground[..], &caster[..]);
        let scene = move |indices: &'static [u32], mesh_key: u64, shadow: ShadowCast| {
            let mut scene: FrameScene<'_> = scene_with_quads(quality, &[]);
            scene.directional_shadows = true;
            scene.double_sided = true;
            for (mesh_key, object_key, vertices, indices, shadow) in [
                (0x6720_0001, 0x6720_0001, ground, &GROUND[..], ShadowCast::All),
                (mesh_key, 0xCA57_0001, caster, indices, shadow),
            ] {
                scene.meshes.push(FrameMesh {
                    is_static: false,
                    mesh_key,
                    material_key: 1,
                    object_key,
                    vertices,
                    indices,
                    material: &LAB_TEST_MATERIAL,
                    model: regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                    joints: &[],
                    receiver_layer: 0,
                    shadow,
                    uv1: &[],
                    lod_dither: 0.0,
                });
            }
            scene
        };
        let kept_dirty = |renderer: &FrameRenderer| {
            let report = renderer.shadow_cache_report();
            report
                .dirty_pages
                .iter()
                .filter(|page| {
                    !report
                        .cascade_full_redraw
                        .get(page.cascade as usize)
                        .copied()
                        .unwrap_or(true)
                })
                .count()
        };
        for (shadow, keeps) in [
            (ShadowCast::All, false),
            (
                ShadowCast::Local {
                    reach: 4.0,
                    max_cascades: 2,
                    shadow_scale: 1.0,
                    shadow_lift: 0.0,
                    anchor: None,
                    lod_switch_keeps_pages: false,
                },
                false,
            ),
            (
                ShadowCast::Local {
                    reach: 4.0,
                    max_cascades: 2,
                    shadow_scale: 1.0,
                    shadow_lift: 0.0,
                    anchor: None,
                    lod_switch_keeps_pages: true,
                },
                true,
            ),
        ] {
            let mut renderer = FrameRenderer::new();
            let quad = scene(&QUAD, 0xCA57_0A01, shadow);
            renderer.render_offscreen(&device, &quad).expect("warm");
            let held = renderer.render_offscreen(&device, &quad).expect("held");
            assert_eq!(
                kept_dirty(&renderer),
                0,
                "{shadow:?}: a held frame dirties nothing"
            );
            let swapped = scene(&TRIANGLE, 0xCA57_0A02, shadow);
            let cached = renderer.render_offscreen(&device, &swapped).expect("swapped");
            let fresh = FrameRenderer::new()
                .render_offscreen(&device, &swapped)
                .expect("fresh");
            assert_ne!(fresh.pixels, held.pixels, "the swap must change the shadow");
            if keeps {
                assert_eq!(kept_dirty(&renderer), 0, "{shadow:?}: opted to keep its pages");
            } else {
                assert!(kept_dirty(&renderer) > 0, "{shadow:?}: the swap must dirty pages");
                assert!(
                    cached.pixels == fresh.pixels,
                    "{shadow:?}: the cached shadow must match a full redraw after the swap"
                );
            }
        }
    }

    /// A caster that turns in place (a hull yawing on its spot, a turret) must redraw its
    /// shadow pages. The page cache tracked a caster by the cube around its bounding sphere,
    /// which a rotation about the sphere's centre leaves unchanged, so the pages kept the
    /// old pose's shadow: on DO27's hulls, black copies of the raised deck plates offset
    /// from the plates (v0.1.1–v0.1.2, lane v013-shadow-patches).
    #[test]
    fn a_caster_turned_in_place_redraws_its_shadow_pages() {
        static GROUND: [u32; 6] = [0, 2, 1, 0, 3, 2];
        static BAR: [u32; 6] = [0, 2, 1, 0, 3, 2];
        let device = crate::test_device();
        let ground: Vec<LabVertex> = quad_at(0.0)
            .into_iter()
            .map(|mut v| {
                v.position = [v.position[0] * 8.0, 0.0, v.position[2] * 8.0];
                v
            })
            .collect();
        // A long thin bar 1 m up, centred on the Y axis: turning it about Y keeps its
        // bounding sphere (and the cube around it) where it was.
        let bar: Vec<LabVertex> = quad_at(0.0)
            .into_iter()
            .map(|mut v| {
                v.position = [v.position[0] * 1.6, 1.0, v.position[2] * 0.25];
                v
            })
            .collect();
        let mut quality = FrameQuality::LAB;
        quality.paged_sun_shadows = true;
        let (ground, bar) = (&ground[..], &bar[..]);
        let scene = move |yaw: f32| {
            let mut scene: FrameScene<'_> = scene_with_quads(quality, &[]);
            scene.directional_shadows = true;
            scene.double_sided = true;
            let turned = regia_core::glam::Mat4::from_rotation_y(yaw).to_cols_array_2d();
            for (key, vertices, indices, model) in [
                (
                    0x6721_0001,
                    ground,
                    &GROUND[..],
                    regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                ),
                (0x6721_0002, bar, &BAR[..], turned),
            ] {
                scene.meshes.push(FrameMesh {
                    is_static: false,
                    mesh_key: key,
                    material_key: 1,
                    object_key: key,
                    vertices,
                    indices,
                    material: &LAB_TEST_MATERIAL,
                    model,
                    joints: &[],
                    receiver_layer: 0,
                    shadow: ShadowCast::All,
                    uv1: &[],
                    lod_dither: 0.0,
                });
            }
            scene
        };
        let mut renderer = FrameRenderer::new();
        renderer.render_offscreen(&device, &scene(0.0)).expect("warm");
        let held = renderer.render_offscreen(&device, &scene(0.0)).expect("held");
        let turned = scene(std::f32::consts::FRAC_PI_2);
        let cached = renderer.render_offscreen(&device, &turned).expect("turned");
        let fresh = FrameRenderer::new()
            .render_offscreen(&device, &turned)
            .expect("fresh");
        assert_ne!(fresh.pixels, held.pixels, "the turn must change the shadow");
        assert!(
            cached.pixels == fresh.pixels,
            "the cached shadow must match a full redraw after the turn"
        );
    }

    /// `specs/14` A1 end to end: a `Static`-marked mesh reaches the paged sun cache as a static
    /// caster ([`ShadowCacheReport::static_casters`]), and a turn far below the cascade's move
    /// tolerance keeps its pages exactly as it keeps an unmarked caster's (ADR-5906 superseded
    /// ADR-5704's redraw-on-any-change; integ-v049, ADR-5993).
    #[test]
    fn spec14_a1_static_mesh_reaches_the_sun_page_cache() {
        static GROUND: [u32; 6] = [0, 2, 1, 0, 3, 2];
        static BAR: [u32; 6] = [0, 2, 1, 0, 3, 2];
        const BAR_KEY: u64 = 0x6722_0002;
        let device = crate::test_device();
        let ground: Vec<LabVertex> = quad_at(0.0)
            .into_iter()
            .map(|mut v| {
                v.position = [v.position[0] * 8.0, 0.0, v.position[2] * 8.0];
                v
            })
            .collect();
        let bar: Vec<LabVertex> = quad_at(0.0)
            .into_iter()
            .map(|mut v| {
                v.position = [v.position[0] * 1.6, 1.0, v.position[2] * 0.25];
                v
            })
            .collect();
        let mut quality = FrameQuality::LAB;
        quality.paged_sun_shadows = true;
        let (ground, bar) = (&ground[..], &bar[..]);
        let scene = move |bar_static: bool, yaw: f32| {
            let mut scene: FrameScene<'_> = scene_with_quads(quality, &[]);
            scene.directional_shadows = true;
            scene.double_sided = true;
            let turned = regia_core::glam::Mat4::from_rotation_y(yaw).to_cols_array_2d();
            for (key, vertices, indices, model) in [
                (
                    0x6722_0001,
                    ground,
                    &GROUND[..],
                    regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                ),
                (BAR_KEY, bar, &BAR[..], turned),
            ] {
                scene.meshes.push(FrameMesh {
                    is_static: key == BAR_KEY && bar_static,
                    mesh_key: key,
                    material_key: 1,
                    object_key: key,
                    vertices,
                    indices,
                    material: &LAB_TEST_MATERIAL,
                    model,
                    joints: &[],
                    receiver_layer: 0,
                    shadow: ShadowCast::All,
                    uv1: &[],
                    lod_dither: LOD_DITHER_NONE,
                });
            }
            scene
        };
        // Pages dirtied in cascades that are not redrawn whole.
        let kept_dirty = |renderer: &FrameRenderer| {
            let report = renderer.shadow_cache_report();
            report
                .dirty_pages
                .iter()
                .filter(|page| {
                    !report
                        .cascade_full_redraw
                        .get(page.cascade as usize)
                        .copied()
                        .unwrap_or(true)
                })
                .count()
        };
        // 1e-6 rad about Y: the pose bits change, the bar's ends move about 2e-6 m.
        let creep = 1.0e-6;
        for bar_static in [false, true] {
            let mut renderer = FrameRenderer::new();
            renderer
                .render_offscreen(&device, &scene(bar_static, 0.0))
                .expect("warm");
            renderer
                .render_offscreen(&device, &scene(bar_static, 0.0))
                .expect("held");
            assert_eq!(
                renderer.shadow_cache_report().static_casters,
                u32::from(bar_static),
                "static={bar_static}: exactly the marked bar is a static caster"
            );
            assert_eq!(
                kept_dirty(&renderer),
                0,
                "static={bar_static}: a held frame dirties nothing"
            );
            renderer
                .render_offscreen(&device, &scene(bar_static, creep))
                .expect("crept");
            assert_eq!(
                kept_dirty(&renderer),
                0,
                "static={bar_static}: a caster keeps its pages below the move tolerance, Static or not"
            );
        }
    }

    /// A renderer cached on one device must transparently rebuild for another
    /// (headless tests create several devices per process; the editor's
    /// persistent viewport renderer outlives each of them).
    #[test]
    fn device_scoped_renderer_survives_device_changes() {
        let a = match RenderDevice::headless() {
            Ok(d) => d,
            Err(err) => panic!("no GPU: {err}"),
        };
        let b = match RenderDevice::headless() {
            Ok(d) => d,
            Err(err) => panic!("no GPU: {err}"),
        };
        let mut scoped = DeviceFrameRenderer::new();
        let scene = scene_with_quads(FrameQuality::LAB, &[0.0]);
        let r = scoped.get(&a);
        let first = r.render_offscreen(&a, &scene).expect("first device renders");
        assert!(first.pixels.iter().copied().max().unwrap_or(0) > 40, "lit quad");
        // Switching devices must not reuse the first device's cached buffers.
        let r = scoped.get(&b);
        let second = r.render_offscreen(&b, &scene).expect("second device renders");
        assert!(second.pixels.iter().copied().max().unwrap_or(0) > 40, "lit quad");
        // Same device twice reuses the renderer; dropping the old device is fine.
        drop(a);
        let r = scoped.get(&b);
        let third = r
            .render_offscreen(&b, &scene)
            .expect("renders after the first device died");
        assert!(third.pixels.iter().copied().max().unwrap_or(0) > 40, "lit quad");
    }

    /// The MSAA path (a host that asks for 4x on the Mobile High quality) renders and
    /// resolves correctly. No shipping preset selects it any more.
    #[test]
    fn msaa_quality_renders_and_resolves() {
        let device = crate::test_device();
        let mobile_high = FrameQuality {
            msaa_samples: 4,
            ..FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
                crate::profile::RenderProfilePreset::MobileHigh,
            ))
        };
        let mut renderer = FrameRenderer::new();
        let scene = scene_with_quads(mobile_high, &[0.0]);
        let result = renderer.render_offscreen(&device, &scene).expect("msaa render");
        let stats = renderer.draw_stats();
        assert_eq!(stats.drawn, 1, "MSAA must not cull the visible quad");
        let max_c = result.pixels.iter().copied().max().unwrap_or(0);
        assert!(max_c > 40, "resolved image must be lit, got max={max_c}");
        // A second frame at the same quality reuses the MSAA attachments.
        renderer
            .render_offscreen(&device, &scene_with_quads(mobile_high, &[0.0]))
            .expect("msaa render 2");
    }

    /// Shadow maps honour the profile's CSM resolution and cascade count.
    #[test]
    fn csm_cascade_count_varies_without_breaking_shadows() {
        let device = crate::test_device();
        for (count, resolution) in [(1u32, 512u32), (2, 512), (4, 1024)] {
            let quality = FrameQuality {
                csm_cascade_count: count,
                csm_resolution: resolution,
                ..FrameQuality::LAB
            };
            let mut scene = scene_with_quads(quality, &[0.0]);
            scene.directional_shadows = true;
            let mut renderer = FrameRenderer::new();
            let result = renderer.render_offscreen(&device, &scene).expect("render");
            let max_c = result.pixels.iter().copied().max().unwrap_or(0);
            assert!(max_c > 40, "{count} cascades must render lit pixels");
        }
    }

    #[test]
    fn material_params_update_in_place_without_rebinding() {
        // The same material object (same cache key) with changed parameters must
        // rewrite only its uniform: the GPU entry count stays at one and the
        // rendered pixels change.
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        let mut scene = golden_scene();
        let first = renderer
            .render_offscreen(&device, &scene.as_frame_scene())
            .expect("first frame")
            .pixels;
        // A visible parameter change under the same material address: the GPU
        // entry must be reused (no re-upload) while the pixels change.
        scene.materials[0].base_color = [0.85, 0.1, 0.08, 1.0];
        let second = renderer
            .render_offscreen(&device, &scene.as_frame_scene())
            .expect("second frame")
            .pixels;
        let (_, materials, _) = renderer.cache_sizes();
        assert_eq!(materials, 1, "same material key reuses the entry");
        assert!(
            first.iter().zip(&second).any(|(a, b)| a != b),
            "base color change must reach the shader"
        );
    }

    /// A minimal lit scene (one quad, one material) for renderer unit tests.
    fn golden_scene() -> crate::LabScene {
        use crate::lab_frame::{LabMesh, LabVertex};
        let corners = [
            [-1.0, 0.0, -1.0],
            [1.0, 0.0, -1.0],
            [1.0, 0.0, 1.0],
            [-1.0, 0.0, 1.0],
        ];
        let uvs = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]];
        let vertices = corners
            .into_iter()
            .zip(uvs)
            .map(|(position, uv)| LabVertex {
                position,
                normal: [0.0, 1.0, 0.0],
                uv,
                tangent: [1.0, 0.0, 0.0, 1.0],
                color: [1.0; 4],
                joints: [0; 4],
                weights: [0.0; 4],
            })
            .collect::<Vec<_>>();
        crate::LabScene {
            width: 64,
            height: 64,
            eye: [0.0, 2.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 20.0,
            depth_prepass: true,
            directional_shadows: true,
            ibl: true,
            exposure_ev: 14.0,
            ambient_lux: 8_000.0,
            double_sided: false,
            post: None,
            directional: crate::DirectionalLightUniform::new([0.0, -1.0, -0.4], [1.0; 3], 100_000.0),
            local_lights: Vec::new(),
            materials: vec![crate::LabMaterial::default()],
            meshes: vec![LabMesh {
                vertices,
                indices: vec![0, 2, 1, 0, 3, 2],
                material: 0,
                ..LabMesh::unskinned()
            }],
        }
    }

    /// ADR-220 (DO27 rehearsal B): a renderer built after another on the same device (the
    /// stand-in a game-frame warm leaves, whose graph prewarm builds its pipes; a second
    /// renderer) takes the device-level pipelines `build_pipes` makes itself (cluster build, page
    /// clear, translucent casters) instead of compiling them on its thread, and a depth match
    /// at a new output size (a resize, a fullscreen toggle) compiles nothing either.
    #[test]
    fn a_second_renderer_and_a_resize_take_the_device_level_pipelines() {
        let device = crate::test_device();
        let shared = device.shared_pipelines();
        let mut first = FrameRenderer::new();
        first
            .prewarm_material_graphs(&device, &[], &[])
            .expect("the first renderer's pipes");
        DepthMatch::new(&device.device, shared, 64, 64, false).expect("depth match");
        let before = crate::frame_stats::thread_pipeline_creates();
        let mut second = FrameRenderer::new();
        second
            .prewarm_material_graphs(&device, &[], &[])
            .expect("the second renderer's pipes");
        assert!(second.pipes.is_some());
        for (width, height) in [(64, 64), (96, 54), (128, 72)] {
            let matched = DepthMatch::new(&device.device, shared, width, height, false).expect("depth match");
            assert_eq!(matched.size, (width, height));
        }
        assert_eq!(
            crate::frame_stats::thread_pipeline_creates() - before,
            0,
            "a second renderer or a new output size compiled a device-level pipeline"
        );
    }

    /// After warm-up, frames allocate no pipelines or bind groups and do not block on the GPU.
    #[test]
    fn frame_path_does_not_block_or_rebuild_pipelines() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        renderer.set_gpu_timing(&device, true);
        let quality = FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
            crate::profile::RenderProfilePreset::DesktopHigh,
        ));
        // The contact-shadow pass runs in these frames too (depth prepass, sun shadows).
        assert!(quality.contact_shadows);
        let settings = crate::PostSettings {
            auto_exposure: true,
            bloom: true,
            bloom_mips: 3,
            taa: true,
            ..crate::PostSettings::default()
        };
        let mut scene: FrameScene<'_> = scene_with_quads(quality, &[0.0]);
        scene.post = Some(&settings);
        scene.width = 64;
        scene.height = 64;
        scene.depth_prepass = true;
        scene.directional_shadows = true;
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame path"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let target = FrameTarget::full(&view, format, 64, 64);
        // Post rotates 16 parameter slots. Warm-up covers a full ring so the
        // measured frames only hit cached bind groups.
        for _ in 0..24 {
            renderer.render_into(&device, &scene, target).expect("warm-up");
        }
        let pipelines = crate::frame_stats::pipeline_creates();
        let polls = crate::frame_stats::blocking_polls();
        let groups = crate::frame_stats::bind_group_creates();
        let before_submits = crate::frame_stats::submits();
        for _ in 0..6 {
            renderer.render_into(&device, &scene, target).expect("frame");
        }
        assert_eq!(
            crate::frame_stats::pipeline_creates(),
            pipelines,
            "a frame after warm-up created a pipeline"
        );
        assert_eq!(
            crate::frame_stats::blocking_polls(),
            polls,
            "the frame path called a blocking poll"
        );
        assert_eq!(
            crate::frame_stats::bind_group_creates(),
            groups,
            "a frame after warm-up created a bind group: {:?}",
            crate::frame_stats::bind_group_labels_from(groups)
        );
        let submits = crate::frame_stats::submits() - before_submits;
        assert!(
            (6..=12).contains(&submits),
            "six warm frames submitted {submits} times; expected 1 or 2 each"
        );
    }

    /// How a dense-cut test's warm-up reserved the dense view.
    #[derive(Clone, Copy, PartialEq)]
    enum CutReserve {
        /// [`FrameRenderer::prewarm`] with the dense view's instance count.
        Prewarm,
        /// [`FrameRenderer::reserve_instances`] alone, as [`crate::RuntimeRenderer`] and a game's
        /// level load do.
        Instances,
        /// [`crate::RUNTIME_INSTANCE_RESERVE`] at the start, then a level's larger count after the
        /// first frame made the belt's ring: a menu, then a level. A view large enough that its
        /// instance list is more than a 1 MiB ring chunk holds.
        Raised,
    }

    /// How a dense-cut test's warm-up frames met the GPU.
    #[derive(Clone, Copy, PartialEq)]
    enum CutPace {
        /// As fast as the frames go. A GPU that falls behind grows the belt's ring on the way.
        FreeRunning,
        /// The GPU finished each frame before the next one started: a fast card, or a CPU so
        /// loaded that the frames are the slow part. Nothing here is left for lag to make.
        GpuKeptUp,
    }

    /// A camera cut from a sparse view onto one dense enough for Hi-Z occlusion
    /// ([`HIZ_MIN_RIGID_CASTERS`] rigid casters) creates nothing on the cut frame when the level
    /// reserved that many instances ([`FrameRenderer::prewarm`]): the pyramid, its bind groups
    /// and the cull's buffers exist from warm-up (`specs/27` § 2.4). On 2026-10-08 that first
    /// dense view built them inside a soak's cut frame: 13 bind groups and a 20 MiB pyramid,
    /// 10–12 ms of encode and once 638 ms.
    #[test]
    fn a_cut_onto_a_dense_view_creates_nothing() {
        cut_onto_a_dense_view(false, CutReserve::Prewarm, CutPace::FreeRunning);
    }

    /// The same cut with the depth prepass's two-phase cull on (ADR-1887): its pipelines,
    /// buffers and bind group are made on the warm-up frames of the reserving level, so the cut
    /// frame that first culls in two phases creates nothing either.
    #[test]
    fn a_cut_onto_a_dense_view_creates_nothing_with_two_phase_occlusion() {
        cut_onto_a_dense_view(true, CutReserve::Prewarm, CutPace::FreeRunning);
    }

    /// The same cut after a warm-up the GPU kept up with, also from a frame behind (ADR-3189). Every
    /// warm-up frame found the belt's chunk free, so none grew the ring: before the fix the cut made
    /// the second chunk and the spill, on any GPU, because the frame's uploads were more than the
    /// reservation and only that growth had hidden it. A loaded RTX 5090 hit this in a full-lib run.
    #[test]
    fn a_cut_onto_a_dense_view_creates_nothing_after_a_warm_up_the_gpu_kept_up_with() {
        cut_onto_a_dense_view(true, CutReserve::Prewarm, CutPace::GpuKeptUp);
    }

    /// The same cut on a renderer that only reserved instances, which is what the game path does:
    /// [`crate::RuntimeRenderer`] never calls [`FrameRenderer::prewarm`]. Its first frame makes the
    /// belt's room (ADR-3189).
    #[test]
    fn a_cut_onto_a_dense_view_creates_nothing_with_instances_reserved_alone() {
        cut_onto_a_dense_view(true, CutReserve::Instances, CutPace::GpuKeptUp);
    }

    /// The same cut when the level raised the reservation after the first frame, as a game does
    /// after its menu (ADR-3189). The ring's chunks add up to the new reservation, but the frame's
    /// instance list is one write that fits neither of them.
    #[test]
    fn a_cut_onto_a_dense_view_creates_nothing_with_the_reservation_raised_after_a_frame() {
        cut_onto_a_dense_view(true, CutReserve::Raised, CutPace::GpuKeptUp);
    }

    fn cut_onto_a_dense_view(two_phase: bool, reserve: CutReserve, pace: CutPace) {
        // Its own device, like the belt's in-flight tests: on the shared test device another
        // test's thread can take the remap's callback in its own poll, and the wait below then
        // returns before that thread has run it, so the cut finds the belt's chunk still unmapped.
        let device = crate::RenderDevice::headless().expect("no GPU adapter (install Mesa lavapipe)");
        if crate::gpu_driven::IndirectStrategy::select(&device.capabilities)
            == crate::gpu_driven::IndirectStrategy::Cpu
        {
            eprintln!("skip: the device has no indirect draws, so the frame never culls on the GPU");
            return;
        }
        let mut renderer = FrameRenderer::new();
        renderer.set_two_phase_occlusion(two_phase);
        let quality = FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
            crate::profile::RenderProfilePreset::DesktopHigh,
        ));
        let settings = crate::PostSettings {
            taa: true,
            ..crate::PostSettings::default()
        };
        // One small quad mesh drawn as many objects on a grid in front of the camera.
        let vertices: &'static [LabVertex] = Box::leak(
            quad_at(0.0)
                .into_iter()
                .map(|mut v| {
                    v.position = [v.position[0] * 0.05, 0.0, v.position[2] * 0.05];
                    v
                })
                .collect::<Vec<_>>()
                .into_boxed_slice(),
        );
        let object = |i: usize| -> FrameMesh<'static> {
            let (col, row) = ((i % 64) as f32, (i / 64) as f32);
            FrameMesh {
                is_static: false,
                mesh_key: 7,
                material_key: 1,
                object_key: i as u64 + 1,
                vertices,
                indices: &[0, 2, 1, 0, 3, 2],
                material: &LAB_TEST_MATERIAL,
                model: regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(
                    -3.2 + col * 0.1,
                    0.0,
                    -3.0 + row * 0.1,
                ))
                .to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        };
        let dense = match reserve {
            CutReserve::Raised => 3000,
            CutReserve::Prewarm | CutReserve::Instances => HIZ_MIN_RIGID_CASTERS + 64,
        };
        match reserve {
            CutReserve::Prewarm => renderer
                .prewarm(&device, &[object(0)], dense as u32)
                .expect("prewarm"),
            CutReserve::Instances => renderer.reserve_instances(dense as u32),
            CutReserve::Raised => renderer.reserve_instances(crate::RUNTIME_INSTANCE_RESERVE),
        }
        let mut scene: FrameScene<'_> = scene_with_quads(quality, &[]);
        scene.post = Some(&settings);
        scene.width = 64;
        scene.height = 64;
        scene.depth_prepass = true;
        scene.directional_shadows = true;
        scene.meshes = (0..8).map(object).collect();
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("dense cut"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let target = FrameTarget::full(&view, format, 64, 64);
        // Warm-up on the sparse view (post rotates 16 parameter slots: a full ring).
        for frame in 0..24 {
            renderer.render_into(&device, &scene, target).expect("warm-up");
            if frame == 0 && reserve == CutReserve::Raised {
                renderer.reserve_instances(dense as u32);
            }
            if pace == CutPace::GpuKeptUp {
                device
                    .tracked()
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("wait for the warm-up frame");
            }
        }
        // The GPU has run every warm-up frame, so all the belt's chunks are free: the first cut below
        // measures the reservation alone. The GPU being behind on the cut is the second cut's case.
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("wait for the warm-up frames");
        let sparse = scene.meshes.clone();
        let sparse_view = (scene.eye, scene.target);
        // The cut: another camera, and every object of the dense view at once.
        let dense_view = ([2.0, 4.0, 5.0], [0.0, 0.0, -1.0]);
        let dense_meshes: Vec<FrameMesh<'static>> = (0..dense).map(object).collect();
        let cut_creates_nothing = |renderer: &mut FrameRenderer, scene: &mut FrameScene<'_>, when: &str| {
            (scene.eye, scene.target) = dense_view;
            scene.meshes = dense_meshes.clone();
            let before = crate::frame_stats::counters();
            crate::frame_stats::trace(true);
            renderer.render_into(&device, scene, target).expect("cut");
            let created = crate::frame_stats::take_events();
            crate::frame_stats::trace(false);
            let cut = crate::frame_stats::counters().since(before);
            assert!(
                renderer.draw_stats().drawn >= HIZ_MIN_RIGID_CASTERS,
                "the cut view must be dense enough to cull against the pyramid: {:?}",
                renderer.draw_stats()
            );
            assert_eq!(
                (
                    cut.pipelines,
                    cut.bind_groups,
                    cut.buffers,
                    cut.textures,
                    cut.blocking_polls
                ),
                (0, 0, 0, 0, 0),
                "the cut frame {when} created resources: {created:?} ({} upload bytes)",
                cut.upload_bytes
            );
        };
        cut_creates_nothing(&mut renderer, &mut scene, "with every chunk free");
        // The frame's uploads fit what the level reserved for them, with no help from the rounding
        // of a chunk's size or from the rest of the ring.
        let (wrote, reserved) = (renderer.belt.frame_bytes(), belt_reserve_bytes(dense as u32));
        assert!(
            wrote <= reserved,
            "the cut wrote {wrote} belt bytes; {dense} reserved instances have room for {reserved}"
        );
        if two_phase && crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
            assert!(
                renderer.prepass_occlusion_stats().active,
                "the cut frame did not cull its prepass in two phases: {:?}",
                renderer.prepass_occlusion_stats()
            );
        }
        if pace == CutPace::GpuKeptUp {
            // Back on the sparse view, then the cut again with no wait: the chunk the frame before
            // wrote may be in flight, so its bytes go to the rest of the ring and the spill. "May":
            // the frame's own poll can find the GPU done. `staging`'s
            // `a_dense_frame_behind_the_gpu_creates_nothing` holds the chunk in flight.
            (scene.eye, scene.target) = sparse_view;
            scene.meshes = sparse;
            renderer.render_into(&device, &scene, target).expect("sparse");
            device
                .tracked()
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("wait for the sparse frame");
            renderer.render_into(&device, &scene, target).expect("sparse");
            cut_creates_nothing(&mut renderer, &mut scene, "with the GPU a frame behind");
        }
    }

    /// A frame that builds its forward set compiles the GPU-driven colour variants its forward
    /// pass binds in the same join (lane v018-stalls: those two uber-shader families back to
    /// back were the longest serial stretch of a cold game-frame warm). The prediction is exact:
    /// the forward pass builds none of them itself, and the overlapped frame builds the same
    /// variants as the serial order (which builds them all in the forward pass).
    #[test]
    fn gpu_driven_colour_variants_compile_alongside_the_forward_set() {
        static DOUBLE_SIDED: LabMaterial = LabMaterial {
            base_color: [0.7, 0.7, 0.7, 1.0],
            metallic: 0.0,
            roughness: 0.6,
            double_sided: true,
            ..LabMaterial::EMPTY
        };
        let device = crate::test_device();
        if !crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects() {
            eprintln!("skip: the device draws no GPU objects (no multi-draw indirect)");
            return;
        }
        let quality = FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
            crate::profile::RenderProfilePreset::DesktopHigh,
        ));
        let vertices: &'static [LabVertex] = Box::leak(quad_at(0.0).into_boxed_slice());
        // Rigid objects: culled ones and one double-sided (the cull-none variants too).
        let object = |i: usize| -> FrameMesh<'static> {
            let double_sided = i == 0;
            FrameMesh {
                is_static: false,
                mesh_key: 7,
                material_key: 1 + u64::from(double_sided),
                object_key: i as u64 + 1,
                vertices,
                indices: &[0, 2, 1, 0, 3, 2],
                material: if double_sided {
                    &DOUBLE_SIDED
                } else {
                    &LAB_TEST_MATERIAL
                },
                model: regia_core::glam::Mat4::from_translation(regia_core::glam::Vec3::new(
                    i as f32 * 0.3 - 0.6,
                    0.0,
                    -1.0,
                ))
                .to_cols_array_2d(),
                joints: &[],
                receiver_layer: 0,
                shadow: ShadowCast::All,
                uv1: &[],
                lod_dither: 0.0,
            }
        };
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("forward overlap"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let target = FrameTarget::full(&view, format, 64, 64);
        let first_frame = |serial: bool| {
            let mut renderer = FrameRenderer::new();
            renderer.serial_forward_families = serial;
            let mut scene: FrameScene<'_> = scene_with_quads(quality, &[]);
            scene.width = 64;
            scene.height = 64;
            scene.depth_prepass = true;
            scene.meshes = (0..4).map(object).collect();
            renderer
                .render_into(&device, &scene, target)
                .expect("first frame");
            assert!(
                renderer.culler.instance_count() > 0,
                "the frame culled on the GPU"
            );
            (
                renderer.culler.color_pipelines_built(),
                renderer.late_gpu_color_builds,
            )
        };
        let (overlapped, late) = first_frame(false);
        assert_eq!(
            late, 0,
            "the forward pass built a GPU-driven colour variant itself"
        );
        let (serial, serial_late) = first_frame(true);
        assert_eq!(
            overlapped, serial,
            "the overlap compiled other variants than the forward pass binds"
        );
        assert_eq!(
            (serial, serial_late),
            (4, 4),
            "culled + cull-none, opaque + mask, all built by the forward pass in the serial order"
        );
    }

    /// A warm Desktop High frame submits once through [`crate::device::TrackedQueue`].
    ///
    /// GPU timing is off, so the timestamp ring does not map. Auto exposure maps
    /// exactly one histogram slot; that sample is read two frames later. A raw
    /// `queue.submit` on [`wgpu::Queue`] does not increment the counter, so the
    /// lower bound fails if the frame skipped the wrapper. `cpu_timings().submits`
    /// is that same counter, not a local increment.
    #[test]
    fn steady_frame_submits_at_most_twice() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        let quality = FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
            crate::profile::RenderProfilePreset::DesktopHigh,
        ));
        let settings = crate::PostSettings {
            auto_exposure: true,
            bloom: true,
            bloom_mips: 3,
            taa: true,
            ..crate::PostSettings::default()
        };
        let mut scene: FrameScene<'_> = scene_with_quads(quality, &[0.0]);
        scene.post = Some(&settings);
        scene.width = 64;
        scene.height = 64;
        scene.depth_prepass = true;
        scene.directional_shadows = true;
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("steady submits"),
            size: wgpu::Extent3d {
                width: 64,
                height: 64,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let target = FrameTarget::full(&view, format, 64, 64);
        for _ in 0..24 {
            renderer.render_into(&device, &scene, target).expect("warm-up");
        }
        for _ in 0..4 {
            // Pace like a presented frame: wait for the GPU outside the counted frame. This
            // loop renders offscreen with nothing throttling the CPU, so on a GPU slower per
            // frame than the CPU (a shared, loaded card) the queue falls more than
            // READBACK_SLOTS frames behind, every histogram slot is still in flight, and the
            // frame rightly skips the histogram instead of blocking (`record_histogram`).
            // The counts below are of one steady frame, not of GPU throughput.
            let _ = device.device.poll(wgpu::PollType::wait_indefinitely());
            let submits = crate::frame_stats::submits();
            let maps = crate::frame_stats::map_asyncs();
            renderer.render_into(&device, &scene, target).expect("frame");
            let delta = crate::frame_stats::submits() - submits;
            assert!((1..=2).contains(&delta), "steady frame submitted {delta} times");
            assert_eq!(
                crate::frame_stats::map_asyncs() - maps,
                1,
                "auto exposure maps one histogram slot; timestamps stay unmapped"
            );
            assert_eq!(
                u64::from(renderer.cpu_timings().submits),
                delta,
                "cpu timings must report the TrackedQueue count"
            );
        }
    }

    /// Timing off and auto exposure off: the frame maps nothing.
    /// Timing on: one timestamp map, and that frame's stamps are not readable yet.
    #[test]
    fn timer_maps_only_when_timing_is_on_and_reads_late() {
        let device = crate::test_device();
        let mut renderer = FrameRenderer::new();
        let quality = FrameQuality::from_profile(&crate::profile::RenderProfile::from_preset(
            crate::profile::RenderProfilePreset::DesktopHigh,
        ));
        let settings = crate::PostSettings {
            auto_exposure: false,
            bloom: false,
            taa: false,
            ..crate::PostSettings::default()
        };
        let mut scene: FrameScene<'_> = scene_with_quads(quality, &[0.0]);
        scene.post = Some(&settings);
        scene.width = 32;
        scene.height = 32;
        scene.depth_prepass = false;
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let texture = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("timer maps"),
            size: wgpu::Extent3d {
                width: 32,
                height: 32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let target = FrameTarget::full(&view, format, 32, 32);
        let maps = crate::frame_stats::map_asyncs();
        renderer.render_into(&device, &scene, target).expect("no timing");
        assert_eq!(crate::frame_stats::map_asyncs(), maps, "timing off must not map");
        renderer.set_gpu_timing(&device, true);
        let maps = crate::frame_stats::map_asyncs();
        renderer.render_into(&device, &scene, target).expect("timing on");
        // Metal (Apple GPUs) has no in-encoder timestamps: the timer stays off and nothing maps.
        assert_eq!(
            crate::frame_stats::map_asyncs() - maps,
            u64::from(device.capabilities.timestamp_query),
            "timing on maps the timestamp slot once when the adapter can time passes"
        );
        assert_eq!(
            renderer.gpu_timings().scene_ms,
            0.0,
            "the slot just mapped is not readable for two frames"
        );
    }

    /// Hot-path modules cannot submit a raw queue. Post, GI and SSAO contain no
    /// submit call. The frame module does not call submit on the plain device queue.
    #[test]
    fn frame_path_has_no_raw_queue_submit() {
        for (name, src) in [
            ("post_pass.rs", include_str!("post_pass.rs")),
            ("highend.rs", include_str!("highend.rs")),
            ("ssao.rs", include_str!("ssao.rs")),
            ("contact_shadows.rs", include_str!("contact_shadows.rs")),
        ] {
            assert!(
                !src.contains(".submit("),
                "{name} submits on its own; the frame encoder is submitted once"
            );
        }
        // Split so this source file does not itself contain the forbidden call.
        let needle = concat!("device.", "queue.submit");
        assert!(
            !include_str!("frame.rs").contains(needle),
            "render_into must submit through TrackedQueue"
        );
        // The frame's submit moved into `frame/steps/submit.rs` (ADR-2300): the same rule holds there.
        assert!(
            !include_str!("frame/steps/submit.rs").contains(needle),
            "render_into's submit step must submit through TrackedQueue"
        );
        assert!(
            include_str!("frame/steps/submit.rs").contains("q.submit(buffers)"),
            "render_into's submit step submits the frame's buffers on the tracked queue"
        );
    }

    /// Six units, each its own quad mesh (vertex count from its index: its own key) and its
    /// own 32 × 32 textured material.
    fn thrash_lab() -> crate::LabScene {
        let unit = |i: usize| {
            let x = i as f32 * 0.4 - 1.0;
            let mut vertices = vec![
                LabVertex::from_pos_normal_uv([x - 0.15, -0.15, 0.0], [0.0, 0.0, 1.0], [0.0, 1.0]),
                LabVertex::from_pos_normal_uv([x + 0.15, -0.15, 0.0], [0.0, 0.0, 1.0], [1.0, 1.0]),
                LabVertex::from_pos_normal_uv([x + 0.15, 0.15, 0.0], [0.0, 0.0, 1.0], [1.0, 0.0]),
                LabVertex::from_pos_normal_uv([x - 0.15, 0.15, 0.0], [0.0, 0.0, 1.0], [0.0, 0.0]),
            ];
            for k in 0..i {
                let t = (k + 1) as f32 / (i + 1) as f32;
                vertices.push(LabVertex::from_pos_normal_uv(
                    [x - 0.15 + 0.3 * t, -0.15, 0.0],
                    [0.0, 0.0, 1.0],
                    [t, 1.0],
                ));
            }
            crate::LabMesh {
                vertices,
                indices: vec![0, 1, 2, 0, 2, 3],
                material: i,
                ..crate::LabMesh::unskinned()
            }
        };
        let material = |i: usize| crate::LabMaterial {
            albedo: Some(crate::LabTexture {
                width: 32,
                height: 32,
                rgba: [40 * i as u8, 128, 255 - 40 * i as u8, 255].repeat(32 * 32),
            }),
            ..crate::LabMaterial::default()
        };
        crate::LabScene {
            width: 32,
            height: 32,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 10.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 5_000.0,
            double_sided: false,
            post: None,
            directional: DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials: (0..6).map(material).collect(),
            meshes: (0..6).map(unit).collect(),
        }
    }

    /// Units 1–2 and 3–5 take turns in view (unit 0 always), 300 frames each, four turns:
    /// every turn passes an eviction sweep with the other group idle.
    fn cycle_units(renderer: &mut FrameRenderer, device: &RenderDevice, full: &FrameScene<'_>) {
        let with = |slots: &[usize]| FrameScene {
            meshes: slots.iter().map(|&i| full.meshes[i]).collect(),
            ..full.clone()
        };
        let groups = [with(&[0, 1, 2]), with(&[0, 3, 4, 5])];
        for turn in 0..4 {
            for _ in 0..300 {
                if let Err(err) = renderer.render_offscreen(device, &groups[turn % 2]) {
                    panic!("frame: {err}");
                }
            }
        }
    }

    /// v0.1.7 (tester logs: meshes evicted and re-uploaded as units came back into view while
    /// material textures alone filled the one budget): over the material budget, units cycling
    /// in and out of view evict and re-create materials, never meshes. The ADR-183 single
    /// budget of the same size re-uploads meshes.
    #[test]
    fn units_cycling_over_the_texture_budget_reupload_no_meshes() {
        let device = match RenderDevice::headless() {
            Ok(d) => d,
            Err(err) => panic!("no GPU: {err}"),
        };
        let lab = thrash_lab();
        let full = lab.as_frame_scene();
        let residency = CacheResidency {
            budget_bytes: 0,
            min_idle_frames: 50,
            ..CacheResidency::default()
        };
        // Derived budgets: materials 1 byte (the textures are always over theirs), meshes
        // 1 GiB, no seconds floor.
        let mut split = FrameRenderer::new();
        split.set_cache_residency(residency);
        split.derived_override = Some((1 << 30, 1, 0.0));
        cycle_units(&mut split, &device, &full);
        let stats = split.cache_stats();
        let budgets = split.cache_budgets();
        eprintln!("split: {stats:?}\n{budgets:?}");
        assert!(budgets.split, "{budgets:?}");
        assert_eq!(stats.mesh_uploads, 6, "each unit's mesh uploads once: {stats:?}");
        assert_eq!(stats.meshes_evicted, 0, "{stats:?}");
        assert!(
            stats.materials_evicted > 0 && stats.material_uploads > 6,
            "textures over their budget still evict idle materials: {stats:?}"
        );
        // One budget for both, the meshes' bytes plus 1: the textures' excess evicts idle
        // meshes too, and they upload again when their units come back.
        let mut combined = FrameRenderer::new();
        combined.set_cache_residency(CacheResidency {
            budget_bytes: budgets.mesh_bytes + 1,
            ..residency
        });
        cycle_units(&mut combined, &device, &full);
        let old = combined.cache_stats();
        eprintln!("combined: {old:?}");
        assert!(old.meshes_evicted > 0 && old.mesh_uploads > 6, "{old:?}");
    }

    /// Under the derived budgets an entry drawn within the seconds floor stays however far
    /// over budget and however many frames idle; without the floor the same cycle evicts.
    #[test]
    fn the_derived_budgets_keep_what_was_drawn_within_the_seconds_floor() {
        let device = match RenderDevice::headless() {
            Ok(d) => d,
            Err(err) => panic!("no GPU: {err}"),
        };
        let lab = thrash_lab();
        let full = lab.as_frame_scene();
        let residency = CacheResidency {
            budget_bytes: 0,
            min_idle_frames: 50,
            ..CacheResidency::default()
        };
        let run = |floor_secs: f32| {
            let mut renderer = FrameRenderer::new();
            renderer.set_cache_residency(residency);
            renderer.derived_override = Some((1, 1, floor_secs));
            cycle_units(&mut renderer, &device, &full);
            (renderer.cache_stats(), renderer.cache_budgets())
        };
        let (kept, _) = run(3600.0);
        assert_eq!((kept.meshes_evicted, kept.materials_evicted), (0, 0), "{kept:?}");
        assert_eq!((kept.mesh_uploads, kept.material_uploads), (6, 6), "{kept:?}");
        let (swept, budgets) = run(0.0);
        assert!(
            swept.meshes_evicted > 0 && swept.materials_evicted > 0,
            "{swept:?}"
        );
        // Over budget, a cache goes down to its low-water mark (7/8 of 1 byte: 0): only what
        // was drawn within `min_idle_frames` stays.
        eprintln!("swept: {swept:?}\n{budgets:?}");
        assert!(
            budgets.mesh_bytes > 0 && budgets.material_bytes > 0,
            "{budgets:?}"
        );
    }

    /// Determinism audit (render1, finding 2): complete frames never read the
    /// sweep clock, so a derived-budget eviction follows the frame-index gate
    /// alone: two capture runs on machines of different speed keep and drop the
    /// same entries. The interactive renderer still honors the seconds floor.
    #[test]
    fn complete_frames_derived_sweep_ignores_the_wall_clock() {
        let device = match RenderDevice::headless() {
            Ok(d) => d,
            Err(err) => panic!("no GPU: {err}"),
        };
        let lab = thrash_lab();
        let full = lab.as_frame_scene();
        let residency = CacheResidency {
            budget_bytes: 0,
            min_idle_frames: 50,
            ..CacheResidency::default()
        };
        // An hour's floor: no sweep sample ever ages into it during a run.
        let run = |complete: bool| {
            let mut renderer = FrameRenderer::new();
            renderer.set_cache_residency(residency);
            renderer.derived_override = Some((1, 1, 3600.0));
            if complete {
                renderer.set_complete_frames(true);
            }
            cycle_units(&mut renderer, &device, &full);
            renderer.cache_stats()
        };
        let interactive = run(false);
        assert_eq!(
            (interactive.meshes_evicted, interactive.materials_evicted),
            (0, 0),
            "the seconds floor still protects the interactive renderer: {interactive:?}"
        );
        let complete = run(true);
        assert!(
            complete.materials_evicted > 0,
            "complete frames evict on the frame gate alone: {complete:?}"
        );
    }

    #[test]
    fn the_texture_budget_scales_with_a_discrete_cards_vram() {
        let high = crate::TextureStreaming::DESKTOP_HIGH;
        let ultra = crate::TextureStreaming::DESKTOP_ULTRA;
        // The tester's RTX 4080 (16 GB): 4 GiB instead of 2 at Desktop High.
        assert_eq!(high.vram_scaled_budget_mib(Some(16_376), true), 4094);
        // The tier's smallest card keeps the preset's budget; never below it.
        assert_eq!(high.vram_scaled_budget_mib(Some(8_192), true), 2048);
        assert_eq!(high.vram_scaled_budget_mib(Some(4_096), true), 2048);
        assert_eq!(ultra.vram_scaled_budget_mib(Some(16_384), true), 4096);
        assert_eq!(ultra.vram_scaled_budget_mib(Some(24_576), true), 6144);
        // Integrated / unknown memory, and unbudgeted policies, keep what they had.
        assert_eq!(high.vram_scaled_budget_mib(Some(16_384), false), 2048);
        assert_eq!(high.vram_scaled_budget_mib(None, true), 2048);
        assert_eq!(
            crate::TextureStreaming::OFF.vram_scaled_budget_mib(Some(16_384), true),
            0
        );
        assert_eq!(
            crate::TextureStreaming::MOBILE_HIGH.vram_scaled_budget_mib(Some(8_192), false),
            512
        );
    }

    #[test]
    fn the_derived_mesh_and_material_budgets_are_separate_and_vram_scaled() {
        let gib = 1u64 << 30;
        let mib = 1u64 << 20;
        let desktop = |textures, vram| derived_cache_budgets(textures, vram, false);
        // The tester's 16 GB card at Desktop High, streaming budget scaled to 4 GiB: meshes
        // get 2 GiB of their own, materials the streaming budget plus 1 GiB.
        assert_eq!(desktop(4 * gib, Some(16 * 1024)), (2 * gib, 5 * gib));
        // VRAM unknown (DX12): 1 GiB of meshes beside the material budget.
        assert_eq!(desktop(2 * gib, None), (gib, 3 * gib));
        // An 8 GB card: 1 GiB of meshes, 3 GiB of materials.
        assert_eq!(desktop(2 * gib, Some(8 * 1024)), (gib, 3 * gib));
        // A 4 GB card: the materials take half the VRAM, the meshes what is left of 5/8.
        assert_eq!(desktop(gib, Some(4 * 1024)), (512 * mib, 2 * gib));
        // A 32 GB card: meshes capped at 4 GiB.
        assert_eq!(desktop(8 * gib, Some(32 * 1024)), (4 * gib, 9 * gib));
        // Android: both within 1 GiB.
        let (mesh, material) = derived_cache_budgets(u64::MAX, None, true);
        assert_eq!((mesh, material), (256 * mib, 768 * mib));
        let (mesh, material) = derived_cache_budgets(4 * gib, Some(8 * 1024), true);
        assert!(mesh + material <= gib, "{mesh} + {material}");
    }

    #[test]
    fn the_derived_cache_budget_respects_vram_and_the_platform() {
        let gib = 1u64 << 30;
        let mib = 1u64 << 20;
        let desktop = |textures, vram| derived_cache_budget(textures, vram, false);
        let android = |textures, vram| derived_cache_budget(textures, vram, true);
        // Streaming unbudgeted, VRAM unknown: the fallback plus the mesh allowance.
        assert_eq!(desktop(u64::MAX, None), 3 * gib);
        assert_eq!(android(u64::MAX, None), 768 * mib);
        // DO27 (4 GiB streaming budget, 24 GiB card): the streaming budget plus the allowance.
        assert_eq!(desktop(4 * gib, Some(24 * 1024)), 5 * gib);
        // A 4 GiB card holds at most half its VRAM in idle caches, whatever the streaming budget.
        assert_eq!(desktop(u64::MAX, Some(4 * 1024)), 2 * gib);
        assert_eq!(desktop(4 * gib, Some(4 * 1024)), 2 * gib);
        // A stepped-down streaming budget (memory pressure) lowers it.
        assert_eq!(desktop(256 * mib, None), gib + 256 * mib);
        // Android: never above 1 GiB, even with a large streaming budget and a big shared heap.
        assert_eq!(android(4 * gib, Some(8 * 1024)), gib);
        assert_eq!(android(256 * mib, Some(8 * 1024)), 512 * mib);
    }

    /// The rest-pose sphere stays on the entry whichever way the rigid-skin fold is set.
    /// A folded visit draws rigid and counts as folded; a skinned visit draws skinned and
    /// still keeps that sphere. A mesh that has been folded stays marked static. One that
    /// has only been drawn skinned stores the sphere and stays unmarked. The fold check is
    /// cached per key and vertex count.
    #[test]
    fn the_rigid_skin_fold_switch_keeps_cull_bounds_honest() {
        let device = match RenderDevice::headless() {
            Ok(d) => d,
            Err(err) => panic!("no GPU: {err}"),
        };
        let base = scene_with_quads(FrameQuality::LAB, &[0.5]);
        let skinned: Vec<LabVertex> = quad_at(0.5)
            .into_iter()
            .map(|v| LabVertex {
                weights: [1.0, 0.0, 0.0, 0.0],
                joints: [0; 4],
                ..v
            })
            .collect();
        let palette = [JointMatrix::identity()];
        const KEY: u64 = 7;
        let expect = mesh_bounds(&skinned).expect("the quad has a rest sphere");
        let render = |renderer: &mut FrameRenderer, vertices: &[LabVertex]| {
            let scene = FrameScene {
                meshes: vec![FrameMesh {
                    mesh_key: KEY,
                    vertices,
                    joints: &palette,
                    ..base.meshes[0]
                }],
                ..base.clone()
            };
            renderer.render_offscreen(&device, &scene).expect("frame");
            let entry = renderer.meshes.get(&KEY).expect("the mesh stays resident");
            let bounds = entry.bounds.expect("the rest pose stays stored");
            assert_eq!(bounds.center, expect.center, "rest center");
            assert!(
                (bounds.radius - expect.radius).abs() < 1.0e-5,
                "rest radius {} vs {}",
                bounds.radius,
                expect.radius
            );
            (entry.static_bound, renderer.draw_stats().rigid_skin_folded)
        };
        let mut renderer = FrameRenderer::new();
        assert_eq!(render(&mut renderer, &skinned), (true, 1), "folded: static draw");
        renderer.set_rigid_skin_fold(false);
        assert_eq!(
            render(&mut renderer, &skinned),
            (true, 0),
            "a later skinned visit keeps the rest pose and the static flag"
        );
        renderer.set_rigid_skin_fold(true);
        assert_eq!(render(&mut renderer, &skinned), (true, 1), "folded again");
        let mut off = FrameRenderer::new();
        off.set_rigid_skin_fold(false);
        assert_eq!(
            render(&mut off, &skinned),
            (false, 0),
            "never folded: the rest pose is stored and the mesh is not marked static"
        );
        // A host that rebuilds the vertex array each frame under a stable key is not rescanned
        // (the key names the content, as for the mesh cache): even a changed weight keeps the
        // cached verdict until the vertex count or key changes.
        let mut rebuilt = skinned.clone();
        rebuilt[0].weights = [0.5, 0.0, 0.0, 0.0];
        assert_eq!(render(&mut renderer, &rebuilt), (true, 1), "cached per key");
        rebuilt.push(rebuilt[0]);
        assert_eq!(
            render(&mut renderer, &rebuilt).1,
            0,
            "a new vertex count rechecks"
        );
    }

    #[test]
    fn rigid_skin_check_and_fold() {
        use regia_core::glam::{Mat4, Quat, Vec3};
        let vertex = |weights: [f32; 4], joints: [u32; 4]| LabVertex {
            weights,
            joints,
            ..LabVertex::from_pos_normal_uv([0.3, -0.2, 0.7], [0.0, 1.0, 0.0], [0.0, 0.0])
        };
        assert!(rigid_to_joint_zero(&[vertex([1.0, 0.0, 0.0, 0.0], [0, 7, 3, 2])]));
        assert!(rigid_to_joint_zero(&[vertex([0.5, 0.5, 0.0, 0.0], [0, 0, 9, 9])]));
        assert!(
            !rigid_to_joint_zero(&[vertex([0.0; 4], [0; 4])]),
            "unskinned vertex"
        );
        assert!(
            !rigid_to_joint_zero(&[vertex([0.9, 0.0, 0.0, 0.0], [0; 4])]),
            "sum below one"
        );
        assert!(
            !rigid_to_joint_zero(&[vertex([0.5, 0.5, 0.0, 0.0], [0, 1, 0, 0])]),
            "second joint"
        );
        // `fold(model, joint) × p` is `model × (joint × p)` (the skinned vertex).
        let joint = Mat4::from_rotation_translation(Quat::from_rotation_z(0.7), Vec3::new(1.0, -2.0, 0.5));
        let model = Mat4::from_scale_rotation_translation(
            Vec3::splat(2.0),
            Quat::from_rotation_y(-0.4),
            Vec3::new(5.0, 0.0, -3.0),
        );
        let folded = Mat4::from_cols_array_2d(&fold_joint(
            model.to_cols_array_2d(),
            &JointMatrix::from_mat4(joint),
        ));
        let p = Vec3::new(0.3, -0.2, 0.7);
        let skinned = model.transform_point3(joint.transform_point3(p));
        assert!((folded.transform_point3(p) - skinned).length() < 1e-5);
    }

    /// ADR-869: an ordinary camera passes through untouched (no copy); every degenerate field is
    /// replaced with a value the matrices stay finite for, and the rest of the scene is kept.
    #[test]
    fn usable_camera_repairs_only_degenerate_cameras() {
        use regia_core::glam::Vec3;
        let good = scene_with_quads(FrameQuality::LAB, &[0.0]);
        assert!(usable_camera(&good).is_none());
        // Straight down is not a repair: the look-at handles it.
        let mut down = scene_with_quads(FrameQuality::LAB, &[0.0]);
        down.eye = [0.0, 10.0, 0.0];
        down.target = [0.0, 0.0, 0.0];
        assert!(usable_camera(&down).is_none());

        let mut bad = scene_with_quads(FrameQuality::LAB, &[0.0, 1.0]);
        bad.eye = [f32::NAN, 0.0, 0.0];
        bad.target = [1.0, 0.0, 0.0];
        bad.far = f32::INFINITY;
        bad.near = 0.0;
        bad.fov_y = f32::NAN;
        let fixed = usable_camera(&bad).expect("a degenerate camera is repaired");
        assert_eq!(fixed.eye, [0.0, 0.0, 0.0]);
        assert_eq!(fixed.target, [1.0, 0.0, 0.0], "the finite target is kept");
        assert_eq!(fixed.far, crate::view_math::INFINITE_FAR_STANDIN);
        assert_eq!(fixed.near, crate::view_math::FALLBACK_NEAR);
        assert_eq!(fixed.fov_y, std::f32::consts::FRAC_PI_4);
        assert_eq!(fixed.meshes.len(), 2);
        assert_eq!(fixed.width, bad.width);

        let mut on_target = scene_with_quads(FrameQuality::LAB, &[0.0]);
        on_target.target = on_target.eye;
        let fixed = usable_camera(&on_target).expect("an eye on its target is repaired");
        let [x, y, z] = on_target.eye;
        assert_eq!(fixed.target, [x, y, z - 1.0]);
        let view = look_at_view(
            Vec3::from_array(fixed.eye),
            Vec3::from_array(fixed.target),
            Vec3::Y,
        );
        let proj = perspective(fixed.fov_y, 1.0, fixed.near, fixed.far);
        assert!((proj * view).to_cols_array().iter().all(|v| v.is_finite()));
    }

    /// ADR-1367: a light without `LightSourceSize` packs the 45 cm constant into its tile's
    /// `bias.w` bit for bit (frames stay pixel-identical); a light with one packs its radius,
    /// and the specular slot (`cone_shadow.w`) is 0 without it.
    #[test]
    fn local_shadow_tiles_pack_the_light_source_radius() {
        let point = |source_radius: Option<f32>, x: f32| {
            crate::LabLocalLight::Point(crate::LabPointLight {
                position: [x, 2.0, 0.0],
                color: [1.0; 3],
                intensity: 800.0,
                radius: 8.0,
                cast_shadows: true,
                source_radius,
            })
        };
        let spot = |source_radius: Option<f32>| {
            crate::LabLocalLight::Spot(crate::LabSpotLight {
                position: [0.0, 3.0, 1.0],
                direction: [0.0, -1.0, 0.0],
                color: [1.0; 3],
                intensity: 1000.0,
                range: 10.0,
                inner_angle: 0.3,
                outer_angle: 0.5,
                cast_shadows: true,
                source_radius,
            })
        };
        let lights = [
            point(None, -1.0),
            spot(Some(0.2)),
            point(Some(0.0), 1.0),
            spot(None),
        ];
        let scene = scene_with_quads(FrameQuality::LAB, &[]);
        let view = crate::light_budget::LightView::new(
            scene.eye,
            scene.target,
            scene.fov_y,
            1.0,
            scene.near,
            scene.far,
        );
        let mut anchors = [None; 4];
        let mut near_anchor = None;
        let warnings = crate::light_budget::LightDropWarnings::default();
        let plan = plan_shadows(
            &scene,
            &lights,
            &view,
            [0.0, -1.0, -0.4],
            CascadeSticky {
                anchors: &mut anchors,
                mask: 0,
                inv_view_proj: regia_core::glam::Mat4::IDENTITY,
                near_anchor: &mut near_anchor,
                sun_range_m: f32::INFINITY,
                precip: None,
            },
            &warnings,
        );
        let faces = [6usize, 1, 6, 1];
        let want = [crate::shadow::LOCAL_SHADOW_SOURCE_RADIUS_M, 0.2, 0.0, 0.45];
        for (index, light) in lights.iter().enumerate() {
            let first = usize::try_from(plan.light_tile[index]).expect("every light got a tile");
            for tile in &plan.tiles[first..first + faces[index]] {
                assert_eq!(
                    tile.gpu.bias[3].to_bits(),
                    want[index].to_bits(),
                    "light {index}: bias.w {}",
                    tile.gpu.bias[3]
                );
            }
            let packed = pack_light(light, plan.light_tile[index]);
            let specular = crate::light_source::light_source_radius(light).unwrap_or(0.0);
            assert_eq!(
                packed.cone_shadow[3].to_bits(),
                specular.to_bits(),
                "light {index}"
            );
        }
        assert_eq!(
            pack_light(&lights[0], -1).cone_shadow[3].to_bits(),
            0.0f32.to_bits()
        );
    }

    fn daylit_scene(eye: [f32; 3], sun_dir: [f32; 3]) -> FrameScene<'static> {
        FrameScene {
            width: 32,
            height: 32,
            eye,
            target: [eye[0], eye[1], eye[2] - 1.0],
            fov_y: 1.0,
            near: 0.1,
            far: 50.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 8.0,
            ambient_lux: 40.0,
            double_sided: false,
            post: None,
            fog: crate::FogSettings::default(),
            directional: DirectionalLightUniform::new(sun_dir, [1.0, 1.0, 1.0], 80_000.0),
            local_lights: &[],
            quality: FrameQuality {
                gi: crate::profile::GiMode::Probes,
                interior_ambient: true,
                ..FrameQuality::LAB
            },
            meshes: Vec::new(),
        }
    }

    fn bake_daylit(renderer: &mut FrameRenderer, eye: [f32; 3], sun_dir: [f32; 3]) {
        let scene = daylit_scene(eye, sun_dir);
        renderer.update_daylit_interior(&scene, scene.directional, &[]);
    }

    /// With no authored volume, a small eye step stays inside the 12×6×12 m fallback box, so the
    /// bake key and the trace count stay put. Leaving that box bakes again.
    #[test]
    fn fallback_daylit_box_stays_while_the_eye_is_inside() {
        let mut renderer = FrameRenderer::new();
        renderer.set_interior_scene(Some(crate::SdfScene::default()));
        let sun = [0.2, -1.0, 0.3];
        let eye = [0.0, 1.5, 0.0];
        let before = crate::interior::daylit_bakes();
        bake_daylit(&mut renderer, eye, sun);
        let baked = crate::interior::daylit_bakes();
        assert!(
            baked > before,
            "the first eye must bake, bakes {before} -> {baked}"
        );
        let key = renderer.gi_auto;
        assert!(key.is_some(), "the bake must install an auto field");
        bake_daylit(&mut renderer, [eye[0] + 0.25, eye[1], eye[2]], sun);
        assert_eq!(
            crate::interior::daylit_bakes(),
            baked,
            "a 0.25 m step inside the fallback box retraced (key {key:?} -> {:?})",
            renderer.gi_auto
        );
        assert_eq!(
            renderer.gi_auto, key,
            "the bake key changed for an eye still inside the box"
        );
        bake_daylit(&mut renderer, [eye[0] + 7.0, eye[1], eye[2]], sun);
        assert_eq!(
            crate::interior::daylit_bakes(),
            baked + 1,
            "leaving the 6 m box must bake the new fallback"
        );
        assert_ne!(renderer.gi_auto, key, "the new box must not reuse the old key");
    }

    /// An authored volume does not follow the eye. A step inside it keeps the bake; a sun move
    /// that survives the 1/200 direction quantizer bakes again.
    #[test]
    fn authored_interior_rebakes_when_the_sun_moves() {
        let mut renderer = FrameRenderer::new();
        renderer.set_interior_scene(Some(crate::SdfScene::default()));
        let eye = [0.0, 1.5, 0.0];
        let mut params = crate::OutdoorParams::default();
        params
            .interiors
            .push(crate::InteriorVolume::with_floor(eye, [1.0, 1.0, 1.0]));
        renderer.set_outdoor(params);
        let sun = [0.2, -1.0, 0.3];
        let before = crate::interior::daylit_bakes();
        bake_daylit(&mut renderer, eye, sun);
        let baked = crate::interior::daylit_bakes();
        assert_eq!(baked, before + 1, "the authored volume must bake once");
        let key = renderer.gi_auto;
        assert!(key.is_some(), "the bake must install an auto field");
        bake_daylit(&mut renderer, [eye[0] + 0.25, eye[1], eye[2]], sun);
        assert_eq!(
            crate::interior::daylit_bakes(),
            baked,
            "an eye step inside the volume retraced"
        );
        assert_eq!(renderer.gi_auto, key);
        let moved_sun = [-0.8, -0.2, 0.1];
        bake_daylit(&mut renderer, [eye[0] + 0.25, eye[1], eye[2]], moved_sun);
        assert_eq!(
            crate::interior::daylit_bakes(),
            baked + 1,
            "a sun move must bake the authored volume again"
        );
        assert_ne!(renderer.gi_auto, key, "the sun move must change the bake key");
    }
}
