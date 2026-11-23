// SPDX-License-Identifier: MPL-2.0
//! GPU frustum culling, Hi-Z occlusion and indirect draws (`specs/26` § 4, M15a).
//!
//! The CPU path still builds the same bounding spheres ([`crate::Frustum`]). This
//! pass writes `DrawIndexedIndirect` commands: `instance_count` is 1 when the
//! sphere is inside the frustum and not fully behind the Hi-Z pyramid, else 0.
//! A fully occluded mesh therefore adds no colour samples, so the picture matches
//! the CPU path.
//!
//! Vertex-shader-skinned meshes join the batches when they have a joint-palette bound (ADR-248,
//! ADR-1368): the cull tests that world sphere, and each object's `skin.x` is its `joint_base`
//! in the frame's shared palette, so one instanced draw skins every visible instance of a
//! `(cull mode, mesh, material, skinned)` batch (Dudash 2007). A skinned mesh without a bound
//! (bounds off, ephemeral key, unboundable weights), a compute-skinned twin (its own vertex
//! buffer per instance) and every skinned draw on a device without `INDIRECT_FIRST_INSTANCE`
//! stay on the CPU path.
//!
//! With `INDIRECT_FIRST_INSTANCE` (the [`IndirectStrategy::MultiDrawCount`] and
//! [`IndirectStrategy::MultiDraw`] strategies) the cull also packs the visible object slots of
//! each `(mesh, material)` batch and counts them into one instanced `DrawIndexedIndirect` per
//! batch, so the colour pass issues one draw per batch however many instances it holds (terrain
//! scatter: thousands of rocks in a few dozen draws). Otherwise the renderer falls back to one
//! indirect draw per object (the existing uniform-object shader), then to plain CPU draws.

use crate::frame_work::counted::*;
use std::collections::HashMap;

#[cfg(test)]
use crate::RenderDevice;
use crate::capabilities::RenderCapabilities;
use crate::shader_lib::compose_source;
use crate::{LabVertex, RenderError};

/// How indirect draws are submitted for the GPU-driven path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IndirectStrategy {
    /// One instanced indirect draw per `(mesh, material)` batch; the cull writes the
    /// instance count and the visible-slot list (device also has multi-draw-indirect-count).
    MultiDrawCount,
    /// Same instanced per-batch draws on a device without the indirect-count feature.
    MultiDraw,
    /// One `draw_indexed_indirect` per object. `first_instance` is 0, so the
    /// existing object-uniform shader is pixel-identical to a direct draw.
    SingleIndirect,
    /// No indirect execution on this device. The CPU draw loop is used.
    Cpu,
}

impl IndirectStrategy {
    /// Picks the best strategy the device can actually run.
    #[must_use]
    pub fn select(caps: &RenderCapabilities) -> Self {
        if !caps.multi_draw_indirect {
            return Self::Cpu;
        }
        if caps.indirect_first_instance && caps.multi_draw_indirect_count {
            return Self::MultiDrawCount;
        }
        if caps.indirect_first_instance {
            return Self::MultiDraw;
        }
        Self::SingleIndirect
    }

    /// True when the colour pass should use the storage-object shader.
    #[must_use]
    pub fn uses_gpu_objects(self) -> bool {
        matches!(self, Self::MultiDrawCount | Self::MultiDraw)
    }
}

/// One mesh the culler should consider: rigid, or vertex-shader skinned with a joint-palette
/// bound ([`Self::skinned`]).
#[derive(Clone, Copy, Debug)]
pub struct CullMesh {
    /// Mesh cache key (vertex/index buffers).
    pub mesh: u64,
    /// Material cache key.
    pub material: u64,
    /// Object cache key.
    pub object: u64,
    /// World-space bounding-sphere centre.
    pub center: [f32; 3],
    /// World-space bounding-sphere radius.
    pub radius: f32,
    /// Index count of the mesh.
    pub index_count: u32,
    /// First index of the mesh in its index buffer (a mesh-arena range).
    pub first_index: u32,
    /// First vertex of the mesh in its vertex buffer.
    pub base_vertex: i32,
    /// Double-sided material in a frame that culls back faces: the batch draws with the
    /// cull-none pipelines ([`GpuBatch::double_sided`]).
    pub double_sided: bool,
    /// Vertex-shader skinned: the sphere is the joint-palette bound in world space (ADR-248)
    /// and the object's `skin.x` names its palette. Part of the batch key ([`GpuBatch::skinned`]).
    pub skinned: bool,
    /// The instance is static-eligible ([`static_batch_eligible`], `specs/14` A1): the entity
    /// carries `regia_scene::components::Static` and the draw is rigid and whole. Not part of
    /// the batch key (ADR-5906): static and dynamic instances of one mesh and material share
    /// one indirect batch; [`GpuBatch::is_static`] reports whether every member is static.
    pub is_static: bool,
}

/// A draw the colour pass can submit from the indirect buffer.
#[derive(Clone, Copy, Debug)]
pub struct GpuDraw {
    /// Object cache key.
    pub object: u64,
    /// Command index in the indirect buffers.
    pub index: u32,
    /// Slot in the object storage buffer (`instance_index` / `first_instance`).
    pub object_slot: u32,
}

/// One `(cull mode, mesh, material, skinned)` group of [`GpuDraw`]s, contiguous in the indirect
/// buffer.
#[derive(Clone, Copy, Debug)]
pub struct GpuBatch {
    /// Mesh cache key.
    pub mesh: u64,
    /// Material cache key.
    pub material: u64,
    /// Draw with the cull-none pipelines ([`CullMesh::double_sided`]).
    pub double_sided: bool,
    /// The batch holds vertex-shader-skinned instances ([`CullMesh::skinned`]).
    pub skinned: bool,
    /// Every instance of the batch is static-eligible ([`CullMesh::is_static`], `specs/14` A1).
    /// A diagnostic only (ADR-5906): the batch draws exactly like a mixed one.
    pub is_static: bool,
    /// First command index.
    pub start: u32,
    /// Command count (the CPU count passed to multi-draw).
    pub count: u32,
    /// Index into the count buffer.
    pub batch_id: u32,
}

/// Bytes of one GPU object (`mat4` model + `uvec4` skin). Matches `ObjectData`.
pub const GPU_OBJECT_STRIDE: u64 = 80;

const CMD_STRIDE: u64 = 20;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct CullParams {
    view_proj: [[f32; 4]; 4],
    planes: [[f32; 4]; 6],
    screen: [f32; 4],
    forward: [f32; 4],
    instance_count: u32,
    /// 1 = order-fixed compaction ([`GpuCuller::set_ordered`]), 0 = packed by `atomicAdd`.
    ordered: u32,
    _pad: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SphereGpu {
    center_radius: [f32; 4],
    index_count: u32,
    first_index: u32,
    base_vertex: u32,
    object_slot: u32,
    batch_id: u32,
    batch_base: u32,
    /// Index of this sphere inside its batch (submission order); the order-fixed cull's slot.
    local: u32,
    _pad1: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct PipeKey {
    format: wgpu::TextureFormat,
    encode: bool,
    double_sided: bool,
    samples: u32,
    /// Fragment `discard` for alpha-tested materials. Opaque pipelines leave
    /// this off so early-Z can reject the forward fragment shader.
    alpha_test: bool,
    /// Per-batch instanced variant (objects through the visible-slot list).
    instanced: bool,
    /// The ray-traced sun's forward module (ADR-2035).
    sun_rt: bool,
    /// The studio-off forward module (`STUDIO_IBL_OFF`, ADR-2308).
    studio_off: bool,
    /// The many-light forward module (`RESTIR_DI`, ADR-2652).
    restir_di: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct DepthPipeKey {
    double_sided: bool,
    samples: u32,
    /// Mask pipelines keep `discard`. Opaque pipelines omit it so early-Z stays on.
    alpha_test: bool,
}

/// One rigid instance packed into the shadow/depth object buffer.
#[derive(Clone, Copy, Debug)]
pub(crate) struct InstanceSrc {
    /// Mesh cache key.
    pub mesh: u64,
    /// Material cache key. Ignored when instances are grouped by mesh only.
    pub material: u64,
    /// Model matrix.
    pub model: [[f32; 4]; 4],
    /// First joint in the shared palette.
    pub joint_base: u32,
    /// [`crate::FrameMesh::lod_dither`] (`f32` bits in the object's `skin.y`).
    pub lod_dither: f32,
    /// Double-sided material in a back-face-culling frame: batched apart, drawn cull-none.
    pub double_sided: bool,
}

/// A contiguous instanced draw. `instance_index` runs over `start..start + count`
/// and indexes the object storage buffer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InstanceBatch {
    /// Mesh cache key.
    pub mesh: u64,
    /// Material cache key (0 when grouped by mesh only).
    pub material: u64,
    /// Draw with the cull-none pipeline ([`InstanceSrc::double_sided`]).
    pub double_sided: bool,
    /// First `instance_index`.
    pub start: u32,
    /// Instance count.
    pub count: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct InstanceObject {
    model: [[f32; 4]; 4],
    skin: [u32; 4],
}

struct HizPyramid {
    /// Owned with the views. wgpu views keep the allocation alive on their own;
    /// this field makes the pyramid the owner the renderer drops.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    sample: wgpu::TextureView,
    storage: Vec<wgpu::TextureView>,
    sampled_mips: Vec<wgpu::TextureView>,
    width: u32,
    height: u32,
    mips: u32,
    /// Single-pass build of this chain (`None` keeps the per-mip dispatches).
    spd: Option<crate::depth_pyramid::SpdChain>,
}

impl HizPyramid {
    /// Width, height and bytes of every mip (`R32Float`).
    fn extent(&self) -> (u32, u32, u64) {
        let bytes = (0..self.mips)
            .map(|level| {
                u64::from((self.width >> level).max(1)) * u64::from((self.height >> level).max(1)) * 4
            })
            .sum();
        (self.width, self.height, bytes)
    }
}

/// A window view's Hi-Z pyramid kept aside while another view's is in the culler (ADR-7329):
/// [`GpuCuller::swap_hiz`] trades it with the culler's, so each split-screen view keeps the
/// pyramid of its own last frame. Dropping it frees the pyramid.
#[derive(Default)]
pub(crate) struct ParkedHiz(Option<HizPyramid>);

impl ParkedHiz {
    /// Whether a pyramid is kept here.
    pub(crate) fn holds(&self) -> bool {
        self.0.is_some()
    }

    /// The kept pyramid's width, height and bytes.
    pub(crate) fn extent(&self) -> Option<(u32, u32, u64)> {
        self.0.as_ref().map(HizPyramid::extent)
    }
}

/// Persistent GPU culler: indirect buffers, the Hi-Z pyramid and the storage-object pipeline.
impl Default for GpuCuller {
    fn default() -> Self {
        Self {
            cull_pipe: None,
            cull_bgl: None,
            copy_pipe: None,
            copy_bgl: None,
            down_pipe: None,
            down_bgl: None,
            spd: None,
            legacy_hiz: false,
            params: None,
            spheres: None,
            indirect: None,
            slots: None,
            batch_cmds: None,
            instanced_bgl: None,
            instanced_layout: None,
            instanced_cache: crate::bind_cache::BindCache::default(),
            sphere_cap: 0,
            count_cap: 0,
            hiz: None,
            dummy_hiz: None,
            draws: Vec::new(),
            batches: Vec::new(),
            instances: 0,
            mip_count: 0,
            object_buf: None,
            object_bgl: None,
            object_cap: 0,
            color_layout: None,
            color_pipes: HashMap::default(),
            sun_rt: false,
            studio_off: false,
            restir_di: false,
            color_built: 0,
            batch_objects: None,
            batch_cap: 0,
            shadow_layout: None,
            shadow_pipes: HashMap::default(),
            mask_shadow_layout: None,
            mask_shadow_pipes: HashMap::default(),
            depth_layout: None,
            depth_pipes: HashMap::default(),
            hiz_copy_cache: crate::bind_cache::BindCache::default(),
            hiz_down_cache: crate::bind_cache::BindCache::default(),
            cull_cache: crate::bind_cache::BindCache::default(),
            instance_cache: crate::bind_cache::BindCache::default(),
            sphere_bytes: Vec::new(),
            param_bytes: Vec::new(),
            object_bytes: Vec::new(),
            batch_bytes: Vec::new(),
            reserved: 0,
            no_hiz: false,
            // Order-fixed compaction on: two runs of one submission draw the same image
            // (determinism by default). The atomicAdd pack is the explicit speed switch
            // ([`Self::set_ordered`]`(false)`).
            ordered: true,
            foliage_soft_q: 0,
            order_scratch: crate::frame::scratch::Slot::default(),
            sphere_scratch: crate::frame::scratch::Slot::default(),
            cmd_scratch: crate::frame::scratch::Slot::default(),
            depth_order: Vec::new(),
            instance_order: crate::frame::scratch::Slot::default(),
            instance_objects: crate::frame::scratch::Slot::default(),
            instance_shadow_src: crate::frame::scratch::Slot::default(),
            instance_depth_src: crate::frame::scratch::Slot::default(),
            last_instance_bytes: 0,
        }
    }
}

/// Persistent GPU culler: indirect buffers, the Hi-Z pyramid and the storage-object pipeline.
pub struct GpuCuller {
    cull_pipe: Option<wgpu::ComputePipeline>,
    cull_bgl: Option<wgpu::BindGroupLayout>,
    copy_pipe: Option<wgpu::ComputePipeline>,
    copy_bgl: Option<wgpu::BindGroupLayout>,
    down_pipe: Option<wgpu::ComputePipeline>,
    down_bgl: Option<wgpu::BindGroupLayout>,
    /// Single-pass farthest-depth pyramid (`None` when the device binds too few storage textures).
    spd: Option<crate::depth_pyramid::SpdKernel>,
    /// Tests: build with the per-mip dispatches even when SPD is available.
    legacy_hiz: bool,
    params: Option<wgpu::Buffer>,
    spheres: Option<wgpu::Buffer>,
    indirect: Option<wgpu::Buffer>,
    /// Visible object slots packed per batch (instanced colour path).
    slots: Option<wgpu::Buffer>,
    /// One `DrawIndexedIndirect` per batch; the cull increments `instance_count`.
    batch_cmds: Option<wgpu::Buffer>,
    /// Objects + skin + slots for the instanced colour pipeline.
    instanced_bgl: Option<wgpu::BindGroupLayout>,
    instanced_layout: Option<wgpu::PipelineLayout>,
    instanced_cache: crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::Buffer, wgpu::Buffer)>,
    sphere_cap: u32,
    count_cap: u32,
    hiz: Option<HizPyramid>,
    dummy_hiz: Option<(wgpu::Texture, wgpu::TextureView)>,
    draws: Vec<GpuDraw>,
    batches: Vec<GpuBatch>,
    instances: u32,
    mip_count: u32,
    object_buf: Option<wgpu::Buffer>,
    object_bgl: Option<wgpu::BindGroupLayout>,
    object_cap: u64,
    color_layout: Option<wgpu::PipelineLayout>,
    color_pipes: HashMap<PipeKey, wgpu::RenderPipeline>,
    /// The colour pipelines asked for from now on use the ray-traced sun's forward module
    /// ([`Self::set_forward_sun_rt`]).
    sun_rt: bool,
    /// The colour pipelines asked for from now on use the studio-off forward module
    /// ([`Self::set_forward_studio_off`], ADR-2308).
    studio_off: bool,
    /// The colour pipelines asked for from now on shade the many-light reservoir
    /// ([`Self::set_forward_restir_di`], ADR-2652).
    restir_di: bool,
    /// Colour pipelines compiled into [`Self::color_pipes`] so far.
    color_built: u64,
    /// Object storage for instanced shadow and depth draws. Separate from
    /// [`Self::object_buf`]: both are written before the same submit, and a
    /// second `write_buffer` would replace the first before the GPU runs.
    batch_objects: Option<wgpu::Buffer>,
    batch_cap: u64,
    shadow_layout: Option<wgpu::PipelineLayout>,
    shadow_pipes: HashMap<bool, wgpu::RenderPipeline>,
    /// Instanced shadow layout with the material group (and the texture table on the table
    /// path), for alpha-tested casters ([`Self::instanced_mask_shadow_pipeline`]).
    mask_shadow_layout: Option<wgpu::PipelineLayout>,
    /// Alpha-tested instanced shadow pipelines by cull-none.
    mask_shadow_pipes: HashMap<bool, wgpu::RenderPipeline>,
    depth_layout: Option<wgpu::PipelineLayout>,
    depth_pipes: HashMap<DepthPipeKey, wgpu::RenderPipeline>,
    /// Hi-Z copy / downsample / cull bind groups, keyed by the views and buffers they bind.
    hiz_copy_cache: crate::bind_cache::BindCache<(wgpu::TextureView, wgpu::TextureView)>,
    hiz_down_cache: crate::bind_cache::BindCache<(wgpu::TextureView, wgpu::TextureView)>,
    cull_cache: crate::bind_cache::BindCache<CullBgKey>,
    instance_cache: crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::Buffer)>,
    /// Last uploaded sphere and parameter bytes. Counts are zeroed every frame.
    sphere_bytes: Vec<u8>,
    param_bytes: Vec<u8>,
    object_bytes: Vec<u8>,
    batch_bytes: Vec<u8>,
    /// Instances the per-instance buffers are sized for at least ([`Self::reserve`]).
    reserved: u32,
    /// The active camera never tests or binds the Hi-Z pyramid ([`CullViewData::texture_view`]).
    no_hiz: bool,
    /// Order-fixed compaction ([`Self::set_ordered`]). Renderer-wide: not swapped per view.
    ordered: bool,
    /// Softness of the alpha-tested shadow edge in hundredths of alpha (`0`: the hard cutoff,
    /// [`Self::set_foliage_shadow_softness`]).
    foliage_soft_q: u32,
    /// [`Self::upload`]'s per-frame lists, kept between frames (ADR-385; not per view: each
    /// upload rebuilds them).
    order_scratch: crate::frame::scratch::Slot<Vec<usize>>,
    sphere_scratch: crate::frame::scratch::Slot<Vec<SphereGpu>>,
    cmd_scratch: crate::frame::scratch::Slot<Vec<u32>>,
    /// For each depth-prepass object of the last [`Self::upload_instance_lists`], in object
    /// order: its index in that call's `depth` list (the prepass's two-phase cull,
    /// `crate::prepass_occlusion`).
    depth_order: Vec<u32>,
    /// Sort keys of [`Self::upload_instance_lists`], kept between frames.
    instance_order: crate::frame::scratch::Slot<Vec<(bool, u64, u64, u32)>>,
    /// Packed instance objects of [`Self::upload_instance_lists`], kept between frames.
    instance_objects: crate::frame::scratch::Slot<Vec<InstanceObject>>,
    /// Per-job shadow instance sources, kept between frames.
    instance_shadow_src: crate::frame::scratch::Slot<crate::frame::scratch::ListOfLists<InstanceSrc>>,
    /// Depth-prepass instance sources, kept between frames.
    instance_depth_src: crate::frame::scratch::Slot<Vec<InstanceSrc>>,
    /// Bytes [`crate::staging::defer_changed`] wrote on the last instance-list upload.
    last_instance_bytes: usize,
}

impl GpuCuller {
    /// The shadow and depth instance-source lists, emptied, for one
    /// [`Self::upload_instance_lists`]. [`Self::give_instance_lists`] keeps them.
    pub(crate) fn take_instance_lists(
        &mut self,
    ) -> (crate::frame::scratch::ListOfLists<InstanceSrc>, Vec<InstanceSrc>) {
        (self.instance_shadow_src.take(), self.instance_depth_src.take())
    }

    /// Returns the lists from [`Self::take_instance_lists`].
    pub(crate) fn give_instance_lists(
        &mut self,
        shadow: crate::frame::scratch::ListOfLists<InstanceSrc>,
        depth: Vec<InstanceSrc>,
    ) {
        self.instance_shadow_src.give(shadow);
        self.instance_depth_src.give(depth);
    }
}

/// The camera-dependent half of a [`GpuCuller`]: the cull and object buffers, what the last
/// upload produced, the byte mirrors that skip unchanged uploads and the bind groups over those
/// buffers (ADR-184 addendum "Texture views draw through the GPU-driven batches").
///
/// A render-to-texture view swaps its own data in for its frame ([`GpuCuller::swap_view_data`])
/// and the main view's back after, so neither overwrites the other's buffers nor invalidates its
/// mirrors. Pipelines, layouts, the reservation and the Hi-Z pyramid stay shared; a texture
/// view never builds, tests or binds the pyramid (it is the main view's depth).
#[derive(Default)]
pub(crate) struct CullViewData {
    params: Option<wgpu::Buffer>,
    spheres: Option<wgpu::Buffer>,
    indirect: Option<wgpu::Buffer>,
    slots: Option<wgpu::Buffer>,
    batch_cmds: Option<wgpu::Buffer>,
    instanced_cache: crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::Buffer, wgpu::Buffer)>,
    sphere_cap: u32,
    count_cap: u32,
    draws: Vec<GpuDraw>,
    batches: Vec<GpuBatch>,
    instances: u32,
    mip_count: u32,
    object_buf: Option<wgpu::Buffer>,
    object_cap: u64,
    batch_objects: Option<wgpu::Buffer>,
    batch_cap: u64,
    cull_cache: crate::bind_cache::BindCache<CullBgKey>,
    instance_cache: crate::bind_cache::BindCache<(wgpu::Buffer, wgpu::Buffer)>,
    sphere_bytes: Vec<u8>,
    param_bytes: Vec<u8>,
    object_bytes: Vec<u8>,
    batch_bytes: Vec<u8>,
    no_hiz: bool,
}

impl CullViewData {
    /// A texture view's data before its first frame: frustum culling only, the 1×1 dummy bound
    /// in place of the pyramid.
    pub(crate) fn texture_view() -> Self {
        Self {
            no_hiz: true,
            ..Self::default()
        }
    }
}

/// Instanced shadow/depth lists [`GpuCuller::reserve`] sizes the object buffer for: four sun
/// cascades and the depth prepass.
const RESERVED_LISTS: u64 = 5;

/// One per-mip reduction: its bind group and the level's size.
type HizDown = (wgpu::BindGroup, u32, u32);

/// Buffers and the Hi-Z view bound by one cull dispatch.
type CullBgKey = (
    wgpu::Buffer,
    wgpu::Buffer,
    wgpu::Buffer,
    wgpu::TextureView,
    wgpu::Buffer,
    wgpu::Buffer,
);

impl GpuCuller {
    /// Sizes the per-instance buffers (cull spheres, indirect, slots, objects, instanced
    /// shadow/depth objects) for at least `instances` draws, so a frame that reaches that many
    /// does not reallocate them and rebuild their bind groups (`specs/27` § 2.4).
    pub fn reserve(&mut self, instances: u32) {
        self.reserved = self.reserved.max(instances);
    }

    /// Instances reserved with [`Self::reserve`] (0 when nothing was reserved).
    #[must_use]
    pub fn reserved(&self) -> u32 {
        self.reserved
    }

    /// Order-fixed compaction of the instanced colour batches (on by default).
    ///
    /// On, every object keeps its own slot (its index in the batch): a culled object's slot
    /// holds a marker the vertex stage collapses to a point, and a batch's instance count is
    /// its highest visible index plus one (an `atomicMax`, which does not depend on thread
    /// order). The image is the same frame to frame and run to run; the cost is the vertex
    /// work of culled objects that sit below the batch's last visible one. Off, the cull packs
    /// each batch's visible objects with an `atomicAdd`, so the order of the instances inside
    /// a batch follows the order the GPU threads finish in, which changes from run to run;
    /// where two instances meet at one depth the one drawn later wins, and temporal AA carries
    /// that pixel into later frames — the explicit speed switch for a frame that does not need
    /// repeatability.
    pub fn set_ordered(&mut self, on: bool) {
        self.ordered = on;
    }

    /// Whether the order-fixed compaction is on ([`Self::set_ordered`]).
    #[must_use]
    pub fn ordered(&self) -> bool {
        self.ordered
    }

    /// Sets the softness of alpha-tested shadow edges in hundredths of alpha (`0` keeps the hard
    /// cutoff); the alpha-tested shadow pipelines are rebuilt when it changes.
    pub fn set_foliage_shadow_softness(&mut self, hundredths: u32) {
        if self.foliage_soft_q != hundredths {
            self.foliage_soft_q = hundredths;
            self.mask_shadow_pipes.clear();
        }
    }

    /// The alpha-tested shadow edge softness in hundredths ([`Self::set_foliage_shadow_softness`]).
    #[must_use]
    pub fn foliage_shadow_softness(&self) -> u32 {
        self.foliage_soft_q
    }

    /// Exchanges the active camera's cull data with `view` ([`CullViewData`]). Swapping twice
    /// restores both.
    pub(crate) fn swap_view_data(&mut self, view: &mut CullViewData) {
        use std::mem::swap;
        swap(&mut self.params, &mut view.params);
        swap(&mut self.spheres, &mut view.spheres);
        swap(&mut self.indirect, &mut view.indirect);
        swap(&mut self.slots, &mut view.slots);
        swap(&mut self.batch_cmds, &mut view.batch_cmds);
        swap(&mut self.instanced_cache, &mut view.instanced_cache);
        swap(&mut self.sphere_cap, &mut view.sphere_cap);
        swap(&mut self.count_cap, &mut view.count_cap);
        swap(&mut self.draws, &mut view.draws);
        swap(&mut self.batches, &mut view.batches);
        swap(&mut self.instances, &mut view.instances);
        swap(&mut self.mip_count, &mut view.mip_count);
        swap(&mut self.object_buf, &mut view.object_buf);
        swap(&mut self.object_cap, &mut view.object_cap);
        swap(&mut self.batch_objects, &mut view.batch_objects);
        swap(&mut self.batch_cap, &mut view.batch_cap);
        swap(&mut self.cull_cache, &mut view.cull_cache);
        swap(&mut self.instance_cache, &mut view.instance_cache);
        swap(&mut self.sphere_bytes, &mut view.sphere_bytes);
        swap(&mut self.param_bytes, &mut view.param_bytes);
        swap(&mut self.object_bytes, &mut view.object_bytes);
        swap(&mut self.batch_bytes, &mut view.batch_bytes);
        swap(&mut self.no_hiz, &mut view.no_hiz);
    }

    /// Uploads spheres and cull parameters. Call [`Self::build_hiz`] before
    /// [`Self::dispatch`] when `hiz` is true.
    #[allow(clippy::too_many_arguments)]
    pub fn upload(
        &mut self,
        device: &wgpu::Device,
        sink: &mut dyn crate::staging::BufferSink,
        meshes: &[CullMesh],
        view_proj: [[f32; 4]; 4],
        planes: [[f32; 4]; 6],
        forward: [f32; 3],
        width: u32,
        height: u32,
        hiz: bool,
    ) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_pipes(device)?;
        // Batches sort by (cull mode, mesh, material, skinned): back-face-culled batches first in
        // their usual order, then the double-sided ones, so the colour pass switches pipelines
        // once. A mesh drawn both rigid and skinned gets two batches, side by side. The `Static`
        // marker is not part of the key (ADR-5906). The index
        // breaks ties, so the unstable sort (no merge buffer) keeps the stable order.
        let mut order = self.order_scratch.take();
        order.extend(0..meshes.len());
        order.sort_unstable_by_key(|&i| (batch_key(&meshes[i]), i));
        self.draws.clear();
        self.batches.clear();
        let mut spheres = self.sphere_scratch.take();
        let mut batch_start = 0u32;
        let mut batch_id = 0u32;
        let mut current: Option<(bool, u64, u64, bool)> = None;
        // Every member of the open batch is static-eligible so far.
        let mut all_static = true;
        for (sorted_i, &src) in order.iter().enumerate() {
            let mesh = &meshes[src];
            let key = batch_key(mesh);
            if current != Some(key) {
                if let Some((double_sided, mesh_key, material_key, skinned)) = current {
                    let count = sorted_i as u32 - batch_start;
                    self.batches.push(GpuBatch {
                        mesh: mesh_key,
                        material: material_key,
                        double_sided,
                        skinned,
                        is_static: all_static,
                        start: batch_start,
                        count,
                        batch_id: batch_id.saturating_sub(1),
                    });
                }
                current = Some(key);
                all_static = true;
                batch_start = sorted_i as u32;
                batch_id += 1;
            }
            all_static &= mesh.is_static;
            let index = sorted_i as u32;
            self.draws.push(GpuDraw {
                object: mesh.object,
                index,
                object_slot: index,
            });
            spheres.push(SphereGpu {
                center_radius: [
                    mesh.center[0],
                    mesh.center[1],
                    mesh.center[2],
                    mesh.radius.max(0.0),
                ],
                index_count: mesh.index_count,
                first_index: mesh.first_index,
                // The command's `base_vertex` is an `i32`; the shader copies its bits.
                base_vertex: mesh.base_vertex as u32,
                object_slot: index,
                batch_id: batch_id.saturating_sub(1),
                batch_base: batch_start,
                local: index - batch_start,
                _pad1: 0,
            });
        }
        if let Some((double_sided, mesh_key, material_key, skinned)) = current {
            self.batches.push(GpuBatch {
                mesh: mesh_key,
                material: material_key,
                double_sided,
                skinned,
                is_static: all_static,
                start: batch_start,
                count: spheres.len() as u32 - batch_start,
                batch_id: batch_id.saturating_sub(1),
            });
        }
        self.instances = spheres.len() as u32;
        let batches = self.batches.len().max(1) as u32;
        self.ensure_buffers(device, self.instances.max(1), batches)?;
        let mips = width.max(height).max(1).ilog2() + 1;
        self.mip_count = mips;
        if spheres.is_empty() {
            spheres.push(SphereGpu {
                center_radius: [0.0, 0.0, 0.0, 0.0],
                index_count: 0,
                first_index: 0,
                base_vertex: 0,
                object_slot: 0,
                batch_id: 0,
                batch_base: 0,
                local: 0,
                _pad1: 0,
            });
        }
        let mips = self.mip_count.max(1) as f32;
        let params = CullParams {
            view_proj,
            planes,
            screen: [
                width.max(1) as f32,
                height.max(1) as f32,
                if hiz && !self.no_hiz { 1.0 } else { 0.0 },
                mips,
            ],
            forward: [forward[0], forward[1], forward[2], 0.0],
            instance_count: self.instances,
            ordered: u32::from(self.ordered),
            _pad: [0; 2],
        };
        // Instanced commands: index count and batch base per batch, instance count 0. The cull
        // increments the count, so they are rewritten even when the spheres did not change.
        let mut cmds = self.cmd_scratch.take();
        cmds.resize(batches as usize * 5, 0);
        for batch in &self.batches {
            let at = batch.batch_id as usize * 5;
            if let Some(first) = spheres.get(batch.start as usize) {
                cmds[at] = first.index_count;
                cmds[at + 2] = first.first_index;
                cmds[at + 3] = first.base_vertex;
            }
            cmds[at + 4] = batch.start;
        }
        sink.write_buffer(
            self.batch_cmds.as_ref().expect("cull batch commands"),
            0,
            bytemuck::cast_slice(&cmds),
        );
        let param_bytes = bytemuck::bytes_of(&params);
        if !crate::bind_cache::bytes_match(&self.param_bytes, param_bytes) {
            sink.write_buffer(self.params.as_ref().expect("cull params"), 0, param_bytes);
            self.param_bytes.clear();
            self.param_bytes.extend_from_slice(param_bytes);
        }
        let sphere_bytes = bytemuck::cast_slice(&spheres);
        if let Some(buf) = self.spheres.as_ref() {
            crate::staging::sink_write_changed(sink, buf, sphere_bytes, &mut self.sphere_bytes);
        }
        self.order_scratch.give(order);
        self.sphere_scratch.give(spheres);
        self.cmd_scratch.give(cmds);
        Ok(())
    }

    /// Times [`Self::upload`]'s reusable lists had to grow (`FrameRenderer::frame_scratch_stats`).
    pub(crate) fn scratch_growths(&self) -> u64 {
        self.order_scratch.growths()
            + self.sphere_scratch.growths()
            + self.cmd_scratch.growths()
            + self.instance_order.growths()
            + self.instance_objects.growths()
            + self.instance_shadow_src.growths()
            + self.instance_depth_src.growths()
    }

    /// The Hi-Z pyramid's sampled view (every mip), or `None` before it exists or while a
    /// texture view culls (the prepass's two-phase cull, `crate::prepass_occlusion`).
    pub(crate) fn hiz_view(&self) -> Option<&wgpu::TextureView> {
        self.hiz.as_ref().filter(|_| !self.no_hiz).map(|hiz| &hiz.sample)
    }

    /// Trades the culler's Hi-Z pyramid with `parked` (ADR-7329): the renderer parks the
    /// outgoing window view's pyramid and brings in the next view's. Bind groups that read a
    /// pyramid stay in their caches, keyed by its views, so a steady split screen makes none.
    pub(crate) fn swap_hiz(&mut self, parked: &mut ParkedHiz) {
        std::mem::swap(&mut self.hiz, &mut parked.0);
    }

    /// The culler's Hi-Z pyramid's width, height and bytes (`None` before it exists).
    pub(crate) fn hiz_extent(&self) -> Option<(u32, u32, u64)> {
        self.hiz.as_ref().map(HizPyramid::extent)
    }

    /// For each depth-prepass object of the last instance-list upload (object order), its index
    /// in the uploaded depth list.
    pub(crate) fn depth_order(&self) -> &[u32] {
        &self.depth_order
    }

    /// Allocates the Hi-Z pyramid for a `width`×`height` depth target and builds every bind
    /// group that reads `depth` into it, without recording a dispatch (`specs/27` § 2.4).
    ///
    /// The renderer calls this on every frame that may cull against the pyramid, from warm-up
    /// on, so the first frame that does — a camera cut onto a dense view — creates nothing. In
    /// the 2026-10-08 hour soaks that first build created the pyramid and 13 bind groups at a
    /// cut and cost 10–12 ms of encode, once 638 ms. From here on [`Self::dispatch`] binds the
    /// pyramid whether or not a frame tests against it (the parameter flag decides), so the
    /// cull's bind group does not change at that frame either.
    ///
    /// # Errors
    ///
    /// The cull or pyramid shaders failed to build.
    pub fn prepare_hiz(
        &mut self,
        device: &wgpu::Device,
        depth: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        // A texture view's depth never replaces the main view's pyramid.
        if self.no_hiz {
            return Ok(());
        }
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_pipes(device)?;
        self.ensure_hiz(device, width.max(1), height.max(1))?;
        if !self.legacy_hiz
            && let (Some(kernel), Some(hiz)) = (self.spd.as_ref(), self.hiz.as_mut())
            && let Some(chain) = hiz.spd.as_mut()
        {
            chain.prepare(device, kernel, depth);
            return Ok(());
        }
        let _ = self.legacy_groups(device, depth, width.max(1), height.max(1));
        Ok(())
    }

    /// Builds the Hi-Z pyramid from a single-sample depth view (standard Z, clear = 1).
    pub fn build_hiz(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        depth: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) -> Result<(), RenderError> {
        if self.no_hiz {
            return Ok(());
        }
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_pipes(device)?;
        self.ensure_hiz(device, width.max(1), height.max(1))?;
        // Single-pass build (two dispatches at 1440p instead of twelve); same texels.
        if !self.legacy_hiz
            && let (Some(kernel), Some(hiz)) = (self.spd.as_ref(), self.hiz.as_mut())
            && let Some(chain) = hiz.spd.as_mut()
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("hiz spd"),
                timestamp_writes: None,
            });
            chain.record(device, kernel, &mut pass, depth);
            return Ok(());
        }
        let Some((copy_bg, downs)) = self.legacy_groups(device, depth, width.max(1), height.max(1)) else {
            return Ok(());
        };
        let copy_pipe = self.copy_pipe.clone().expect("hiz copy");
        let down_pipe = self.down_pipe.clone().expect("hiz down");
        // One compute pass for the copy and every reduction: wgpu scopes usage per
        // dispatch and inserts the barrier between a mip's write and the next one's read,
        // so this is the same work as a pass per mip without a command buffer per mip.
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("hiz"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(&copy_pipe);
        pass.counted_set_bind_group(0, &copy_bg, &[]);
        pass.counted_dispatch(width.div_ceil(8).max(1), height.div_ceil(8).max(1), 1);
        pass.counted_set_pipeline(&down_pipe);
        for (bg, dw, dh) in &downs {
            pass.counted_set_bind_group(0, bg, &[]);
            pass.counted_dispatch(dw.div_ceil(8).max(1), dh.div_ceil(8).max(1), 1);
        }
        Ok(())
    }

    /// The per-mip path's copy group and one `(group, width, height)` per reduced level, built
    /// on first use of each `depth` view (`None` before [`Self::ensure_hiz`]).
    fn legacy_groups(
        &mut self,
        device: &crate::device::TrackedDevice,
        depth: &wgpu::TextureView,
        width: u32,
        height: u32,
    ) -> Option<(wgpu::BindGroup, Vec<HizDown>)> {
        let (storage0, mips, sampled, storage) = {
            let hiz = self.hiz.as_ref()?;
            (
                hiz.storage[0].clone(),
                hiz.mips,
                hiz.sampled_mips.clone(),
                hiz.storage.clone(),
            )
        };
        let copy_layout = self.copy_bgl.clone()?;
        let down_layout = self.down_bgl.clone()?;
        let copy_bg = self
            .hiz_copy_cache
            .get_or_insert_with((depth.clone(), storage0.clone()), || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("hiz copy"),
                    layout: &copy_layout,
                    entries: &[view_binding(0, depth), view_binding(1, &storage0)],
                })
            });
        let downs = (1..mips)
            .map(|level| {
                let src = sampled[level as usize - 1].clone();
                let dst = storage[level as usize].clone();
                let bg = self
                    .hiz_down_cache
                    .get_or_insert_with((src.clone(), dst.clone()), || {
                        device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("hiz down"),
                            layout: &down_layout,
                            entries: &[view_binding(0, &src), view_binding(1, &dst)],
                        })
                    });
                (bg, (width >> level).max(1), (height >> level).max(1))
            })
            .collect();
        Some((copy_bg, downs))
    }

    /// Runs the cull. No-op when nothing was uploaded.
    pub fn dispatch(&mut self, device: &wgpu::Device, encoder: &mut wgpu::CommandEncoder) {
        let device = &crate::device::TrackedDevice::observe(device);
        if self.instances == 0 {
            return;
        }
        let (Some(pipe), Some(layout)) = (self.cull_pipe.clone(), self.cull_bgl.clone()) else {
            return;
        };
        let hiz_view = self
            .hiz
            .as_ref()
            .filter(|_| !self.no_hiz)
            .map(|hiz| hiz.sample.clone())
            .or(self.dummy_hiz.as_ref().map(|(_, view)| view.clone()));
        let Some(hiz_view) = hiz_view else {
            return;
        };
        let params = self.params.clone().expect("params");
        let spheres = self.spheres.clone().expect("spheres");
        let indirect = self.indirect.clone().expect("indirect");
        let slots = self.slots.clone().expect("slots");
        let batch_cmds = self.batch_cmds.clone().expect("batch commands");
        let key = (
            params.clone(),
            spheres.clone(),
            indirect.clone(),
            hiz_view.clone(),
            slots.clone(),
            batch_cmds.clone(),
        );
        let bg = self.cull_cache.get_or_insert_with(key, || {
            device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("gpu cull"),
                layout: &layout,
                entries: &[
                    buf_binding(0, &params),
                    buf_binding(1, &spheres),
                    buf_binding(2, &indirect),
                    view_binding(3, &hiz_view),
                    buf_binding(7, &slots),
                    buf_binding(8, &batch_cmds),
                ],
            })
        });
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("gpu cull"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(&pipe);
        pass.counted_set_bind_group(0, &bg, &[]);
        pass.counted_dispatch(self.instances.div_ceil(64).max(1), 1, 1);
    }

    /// Indirect commands with `first_instance = 0` (single-indirect path and readback).
    #[must_use]
    pub fn indirect(&self) -> Option<&wgpu::Buffer> {
        self.indirect.as_ref()
    }

    /// One instanced `DrawIndexedIndirect` per batch (at `batch_id * 20` bytes): every visible
    /// object of the batch in one draw, `first_instance` = the batch's slot base.
    #[must_use]
    pub fn batch_commands(&self) -> Option<&wgpu::Buffer> {
        self.batch_cmds.as_ref()
    }

    /// Bind group of the storage objects, the frame skin palette and the visible-slot list
    /// (instanced colour pipeline, [`Self::color_pipeline`] with `instanced`).
    pub fn instanced_object_bind_group(
        &mut self,
        device: &wgpu::Device,
        skin: &wgpu::Buffer,
    ) -> Result<wgpu::BindGroup, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_object_layout(device)?;
        let objects = self
            .object_buf
            .clone()
            .ok_or_else(|| RenderError::Shader("GPU object buffer was not uploaded".into()))?;
        let slots = self
            .slots
            .clone()
            .ok_or_else(|| RenderError::Shader("GPU cull slots were not allocated".into()))?;
        let layout = self.instanced_bgl.clone().expect("instanced object layout");
        Ok(self
            .instanced_cache
            .get_or_insert_with((objects.clone(), skin.clone(), slots.clone()), || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("gpu instanced objects"),
                    layout: &layout,
                    entries: &[
                        buf_binding(0, &objects),
                        buf_binding(1, skin),
                        buf_binding(2, &slots),
                    ],
                })
            }))
    }

    /// Draws produced by the last [`Self::upload`], in indirect-buffer order.
    #[must_use]
    pub fn draws(&self) -> &[GpuDraw] {
        &self.draws
    }

    /// Batches produced by the last [`Self::upload`].
    #[must_use]
    pub fn batches(&self) -> &[GpuBatch] {
        &self.batches
    }

    /// How many meshes the last upload submitted to the cull.
    #[must_use]
    pub fn instance_count(&self) -> u32 {
        self.instances
    }

    /// Colour pipeline variants [`Self::color_pipelines`] has compiled so far.
    pub(crate) fn color_pipelines_built(&self) -> u64 {
        self.color_built
    }

    /// Writes tightly packed object records (`GPU_OBJECT_STRIDE` each) for the storage-object shader.
    pub fn write_objects(
        &mut self,
        device: &wgpu::Device,
        sink: &mut dyn crate::staging::BufferSink,
        bytes: &[u8],
    ) -> Result<(), RenderError> {
        let needed = (bytes.len() as u64)
            .max(GPU_OBJECT_STRIDE)
            .max(u64::from(self.reserved) * GPU_OBJECT_STRIDE);
        if self.object_buf.as_ref().is_none_or(|_| self.object_cap < needed) {
            let cap = needed.next_power_of_two().max(GPU_OBJECT_STRIDE);
            self.object_buf = Some(make_buf(
                device,
                "gpu objects",
                cap,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            ));
            self.object_cap = cap;
            self.object_bytes.clear();
            self.instanced_cache.clear();
        }
        // Only the objects that changed since the last write (a moved vehicle, not
        // every static rock).
        if let Some(buf) = self.object_buf.as_ref() {
            crate::staging::sink_write_changed(sink, buf, bytes, &mut self.object_bytes);
        }
        Ok(())
    }

    /// The frame's forward module: `true` is the ray-traced sun's (ADR-2035). Off by default, so
    /// every pipeline is the one built before that existed.
    pub(crate) fn set_forward_sun_rt(&mut self, sun_rt: bool) {
        self.sun_rt = sun_rt;
    }

    /// The module [`Self::set_forward_sun_rt`] left.
    pub(crate) fn forward_sun_rt(&self) -> bool {
        self.sun_rt
    }

    /// The frame's studio axis: `true` is the forward module without the studio extras
    /// (`STUDIO_IBL_OFF`, ADR-2308). Off by default, so every pipeline is the full one.
    pub(crate) fn set_forward_studio_off(&mut self, studio_off: bool) {
        self.studio_off = studio_off;
    }

    /// The module [`Self::set_forward_studio_off`] left.
    pub(crate) fn forward_studio_off(&self) -> bool {
        self.studio_off
    }

    /// The frame's many-light axis: `true` composes `RESTIR_DI` into the colour pipelines (all
    /// opaque), which shade the pixel's reservoir light (ADR-2652). Off by default.
    pub(crate) fn set_forward_restir_di(&mut self, restir_di: bool) {
        self.restir_di = restir_di;
    }

    /// The module [`Self::set_forward_restir_di`] left.
    pub(crate) fn forward_restir_di(&self) -> bool {
        self.restir_di
    }

    /// Colour pipelines built so far, `(studio-off, full)` (ADR-2308).
    pub(crate) fn color_pipelines_by_studio_axis(&self) -> (usize, usize) {
        let off = self.color_pipes.keys().filter(|key| key.studio_off).count();
        (off, self.color_pipes.len() - off)
    }

    /// Colour pipeline for the storage-object shader variant. `instanced` selects the
    /// per-batch instanced variant (objects read through the visible-slot list).
    #[allow(clippy::too_many_arguments)]
    pub fn color_pipeline(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        encode: bool,
        double_sided: bool,
        samples: u32,
        alpha_test: bool,
        instanced: bool,
        view_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        Ok(self
            .color_pipelines(
                device,
                format,
                encode,
                samples,
                instanced,
                view_bgl,
                mat_bgl,
                &[(double_sided, alpha_test)],
            )?
            .remove(0))
    }

    /// [`Self::color_pipeline`] for several `(double_sided, alpha_test)` variants at once: the
    /// missing ones compile side by side (`pipeline_warm::join_all`; each is the forward uber
    /// shader, ~1 s on a cold NVIDIA driver). Returned in `variants` order.
    #[allow(clippy::too_many_arguments)]
    pub fn color_pipelines(
        &mut self,
        device: &wgpu::Device,
        format: wgpu::TextureFormat,
        encode: bool,
        samples: u32,
        instanced: bool,
        view_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
        variants: &[(bool, bool)],
    ) -> Result<Vec<wgpu::RenderPipeline>, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_object_layout(device)?;
        // Standard materials on the texture table path bind it as group 4 (ADR-863).
        let table = crate::texture_table::table_layout(device);
        if instanced && self.instanced_layout.is_none() {
            self.instanced_layout = Some(device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("gpu driven instanced layout"),
                bind_group_layouts: &crate::texture_table::with_table(
                    &[
                        Some(view_bgl),
                        Some(self.instanced_bgl.as_ref().expect("instanced bgl")),
                        Some(mat_bgl),
                    ],
                    table.as_ref(),
                ),
                immediate_size: 0,
            }));
        }
        if self.color_layout.is_none() {
            self.color_layout = Some(device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("gpu driven layout"),
                bind_group_layouts: &crate::texture_table::with_table(
                    &[
                        Some(view_bgl),
                        Some(self.object_bgl.as_ref().expect("object bgl")),
                        Some(mat_bgl),
                    ],
                    table.as_ref(),
                ),
                immediate_size: 0,
            }));
        }
        let (sun_rt, studio_off, restir_di) = (self.sun_rt, self.studio_off, self.restir_di);
        let key_of = |(double_sided, alpha_test): (bool, bool)| PipeKey {
            format,
            encode,
            double_sided,
            samples,
            alpha_test,
            instanced,
            sun_rt,
            studio_off,
            restir_di,
        };
        let mut missing: Vec<PipeKey> = Vec::new();
        for &variant in variants {
            let key = key_of(variant);
            if !self.color_pipes.contains_key(&key) && !missing.contains(&key) {
                missing.push(key);
            }
        }
        let this: &Self = self;
        let jobs: Vec<_> = missing
            .iter()
            .map(|&key| move || (key, this.build_color_pipeline(device, key)))
            .collect();
        for (key, pipe) in crate::pipeline_warm::join_all(jobs) {
            self.color_pipes.insert(key, pipe?);
            self.color_built += 1;
        }
        variants
            .iter()
            .map(|&variant| {
                self.color_pipes
                    .get(&key_of(variant))
                    .cloned()
                    .ok_or_else(|| RenderError::Shader("gpu driven color pipeline missing".into()))
            })
            .collect()
    }

    /// Compiles one colour pipeline variant (layouts already created).
    fn build_color_pipeline(
        &self,
        device: &crate::device::TrackedDevice,
        key: PipeKey,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        let PipeKey {
            format,
            encode,
            double_sided,
            samples,
            alpha_test,
            instanced,
            sun_rt,
            studio_off,
            restir_di,
        } = key;
        let mut defs = HashMap::new();
        if sun_rt {
            crate::hwrt::SunShadowVariant::RayQuery.add_forward_defs(&mut defs);
        }
        crate::forward_permutations::add_studio_off_def(&mut defs, studio_off);
        crate::forward_permutations::add_restir_di_def(&mut defs, restir_di);
        defs.insert("GPU_DRIVEN".into(), naga_oil::compose::ShaderDefValue::Bool(true));
        if instanced {
            defs.insert(
                "GPU_INSTANCED".into(),
                naga_oil::compose::ShaderDefValue::Bool(true),
            );
        }
        if alpha_test {
            defs.insert("ALPHA_TEST".into(), naga_oil::compose::ShaderDefValue::Bool(true));
        }
        if !encode {
            defs.insert(
                "skip_srgb_encode".into(),
                naga_oil::compose::ShaderDefValue::Bool(true),
            );
        }
        let module = shader_module(
            device,
            "lab_forward_gpu",
            "lab_forward.wgsl",
            include_str!("../shaders/lab_forward.wgsl"),
            defs,
        )?;
        let multisample = wgpu::MultisampleState {
            count: samples.max(1),
            mask: u64::MAX,
            alpha_to_coverage_enabled: false,
        };
        let layout = if instanced {
            self.instanced_layout.as_ref().expect("instanced layout")
        } else {
            self.color_layout.as_ref().expect("gpu layout")
        };
        let pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(match (instanced, double_sided) {
                (true, false) => "gpu driven forward (instanced)",
                (true, true) => "gpu driven forward (instanced, cull none)",
                (false, false) => "gpu driven forward",
                (false, true) => "gpu driven forward (cull none)",
            }),
            layout: Some(layout),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[Some(LabVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
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
            multisample,
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        Ok(pipe)
    }

    /// Reads `instance_count` from each indirect command (after the cull has been submitted).
    ///
    /// Test-only: the frame path does not read the counts back.
    #[cfg(test)]
    pub fn read_instance_counts(&self, device: &RenderDevice) -> Result<Vec<u32>, RenderError> {
        let count = self.instances as usize;
        if count == 0 {
            return Ok(Vec::new());
        }
        let indirect = self
            .indirect
            .as_ref()
            .ok_or_else(|| RenderError::Readback("no indirect buffer".into()))?;
        let bytes = (count as u64) * CMD_STRIDE;
        let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cull readback"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cull readback"),
            });
        encoder.copy_buffer_to_buffer(indirect, 0, &staging, 0, bytes);
        device.queue.submit([encoder.finish()]);
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
        let mut counts = Vec::with_capacity(count);
        for chunk in mapped.chunks(CMD_STRIDE as usize) {
            if chunk.len() < 8 {
                break;
            }
            let word = u32::from_le_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]);
            counts.push(word);
        }
        drop(mapped);
        staging.unmap();
        Ok(counts)
    }

    /// Packs shadow casters (grouped by mesh and material; casters that need no material pass
    /// material 0, so they group by mesh) and depth-prepass draws (grouped by mesh and
    /// material) into one object buffer.
    ///
    /// Depth batches are offset past the shadow range so both passes can read
    /// the buffer in the same submit. Empty inputs produce empty batch lists.
    /// Uploads one instanced caster list per shadow job, then the depth-prepass list, into
    /// one object buffer; returns each list's batches with `start` offset into that buffer.
    pub(crate) fn upload_instance_lists(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        shadow: &[Vec<InstanceSrc>],
        depth: &[InstanceSrc],
    ) -> Result<(Vec<Vec<InstanceBatch>>, Vec<InstanceBatch>), RenderError> {
        let mut objects = self.instance_objects.take();
        let mut keys = self.instance_order.take();
        let mut shadow_batches = Vec::with_capacity(shadow.len());
        for list in shadow {
            shadow_batches.push(pack_instances(list, true, None, &mut keys, &mut objects));
        }
        let depth_batches = pack_instances(depth, true, Some(&mut self.depth_order), &mut keys, &mut objects);
        if objects.is_empty() {
            self.instance_objects.give(objects);
            self.instance_order.give(keys);
            self.last_instance_bytes = 0;
            return Ok((shadow_batches, depth_batches));
        }
        // Shadow and depth lists share the buffer: a reserved instance can be in every sun
        // cascade's list and the depth list. Two records per instance regrew it on the first
        // view where most casters reached all four cascades (a cut onto a dense view).
        let needed = (std::mem::size_of_val(objects.as_slice()) as u64)
            .max(GPU_OBJECT_STRIDE)
            .max(RESERVED_LISTS * u64::from(self.reserved) * GPU_OBJECT_STRIDE);
        if self
            .batch_objects
            .as_ref()
            .is_none_or(|_| self.batch_cap < needed)
        {
            let cap = needed.next_power_of_two().max(GPU_OBJECT_STRIDE);
            self.batch_objects = Some(make_buf(
                device,
                "instance objects",
                cap,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            ));
            self.batch_cap = cap;
            self.batch_bytes.clear();
            self.instance_cache.clear();
        }
        let written = match self.batch_objects.clone() {
            Some(buf) => {
                let bytes = bytemuck::cast_slice(&objects);
                crate::staging::defer_changed(belt, device, &buf, bytes, &mut self.batch_bytes)
            }
            None => {
                self.instance_objects.give(objects);
                self.instance_order.give(keys);
                self.last_instance_bytes = 0;
                return Err(RenderError::Shader(
                    "instance object buffer was not uploaded".into(),
                ));
            }
        };
        self.last_instance_bytes = written;
        self.instance_objects.give(objects);
        self.instance_order.give(keys);
        Ok((shadow_batches, depth_batches))
    }

    /// Bind group of the instanced object buffer plus the frame skin palette.
    pub(crate) fn instance_bind_group(
        &mut self,
        device: &wgpu::Device,
        skin: &wgpu::Buffer,
    ) -> Result<wgpu::BindGroup, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_object_layout(device)?;
        let objects = self
            .batch_objects
            .clone()
            .ok_or_else(|| RenderError::Shader("instance object buffer was not uploaded".into()))?;
        let layout = self
            .object_bgl
            .clone()
            .ok_or_else(|| RenderError::Shader("gpu object layout missing".into()))?;
        let skin_key = skin.clone();
        Ok(self
            .instance_cache
            .get_or_insert_with((objects.clone(), skin_key), || {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("instance objects"),
                    layout: &layout,
                    entries: &[buf_binding(0, &objects), buf_binding(1, skin)],
                })
            }))
    }

    /// Depth-only shadow pipeline that reads one object per `instance_index`.
    pub(crate) fn instanced_shadow_pipeline(
        &mut self,
        device: &wgpu::Device,
        shadow_bgl: &wgpu::BindGroupLayout,
        double_sided: bool,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_object_layout(device)?;
        let object_bgl = self
            .object_bgl
            .clone()
            .ok_or_else(|| RenderError::Shader("gpu object layout missing".into()))?;
        if self.shadow_layout.is_none() {
            self.shadow_layout = Some(device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("instanced shadow layout"),
                bind_group_layouts: &[Some(shadow_bgl), Some(&object_bgl)],
                immediate_size: 0,
            }));
        }
        if let Some(pipe) = self.shadow_pipes.get(&double_sided) {
            return Ok(pipe.clone());
        }
        let mut defs = HashMap::new();
        defs.insert("GPU_DRIVEN".into(), naga_oil::compose::ShaderDefValue::Bool(true));
        let module = shader_module(
            device,
            "lab_shadow_instanced",
            "lab_shadow.wgsl",
            include_str!("../shaders/lab_shadow.wgsl"),
            defs,
        )?;
        let pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(if double_sided {
                "instanced shadow (cull none)"
            } else {
                "instanced shadow"
            }),
            layout: Some(
                self.shadow_layout
                    .as_ref()
                    .ok_or_else(|| RenderError::Shader("instanced shadow layout missing".into()))?,
            ),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[Some(LabVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[],
                compilation_options: Default::default(),
            }),
            primitive: cull_primitive(double_sided),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                // `Less`, matching the CPU shadow pipeline. Equal depths store
                // the same value, so grouping draws by mesh does not move edges.
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        self.shadow_pipes.insert(double_sided, pipe.clone());
        Ok(pipe)
    }

    /// Depth-prepass pipeline (storage objects).
    ///
    /// `alpha_test` compiles `fs_depth` with `discard` for masked materials.
    /// Opaque draws leave it off: a `discard` disables early-Z for every fragment
    /// in the pipeline, which is most of the prepass cost on a fully opaque scene.
    pub(crate) fn instanced_depth_pipeline(
        &mut self,
        device: &wgpu::Device,
        view_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
        double_sided: bool,
        samples: u32,
        alpha_test: bool,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_object_layout(device)?;
        let object_bgl = self
            .object_bgl
            .clone()
            .ok_or_else(|| RenderError::Shader("gpu object layout missing".into()))?;
        if self.depth_layout.is_none() {
            let table = crate::texture_table::table_layout(device);
            self.depth_layout = Some(device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("instanced depth layout"),
                bind_group_layouts: &crate::texture_table::with_table(
                    &[Some(view_bgl), Some(&object_bgl), Some(mat_bgl)],
                    table.as_ref(),
                ),
                immediate_size: 0,
            }));
        }
        let key = DepthPipeKey {
            double_sided,
            samples: samples.max(1),
            alpha_test,
        };
        if let Some(pipe) = self.depth_pipes.get(&key) {
            return Ok(pipe.clone());
        }
        let mut defs = HashMap::new();
        defs.insert("GPU_DRIVEN".into(), naga_oil::compose::ShaderDefValue::Bool(true));
        if alpha_test {
            defs.insert("ALPHA_TEST".into(), naga_oil::compose::ShaderDefValue::Bool(true));
        }
        let module = shader_module(
            device,
            "lab_forward_depth_instanced",
            "lab_forward.wgsl",
            include_str!("../shaders/lab_forward.wgsl"),
            defs,
        )?;
        let pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(match (alpha_test, double_sided) {
                (true, false) => "instanced depth mask",
                (true, true) => "instanced depth mask (cull none)",
                (false, false) => "instanced depth",
                (false, true) => "instanced depth (cull none)",
            }),
            layout: Some(
                self.depth_layout
                    .as_ref()
                    .ok_or_else(|| RenderError::Shader("instanced depth layout missing".into()))?,
            ),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[Some(LabVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_depth"),
                targets: &[],
                compilation_options: Default::default(),
            }),
            primitive: cull_primitive(double_sided),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState {
                count: key.samples,
                mask: u64::MAX,
                alpha_to_coverage_enabled: false,
            },
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        self.depth_pipes.insert(key, pipe.clone());
        Ok(pipe)
    }

    /// The instanced shadow pipeline of alpha-tested (`ALPHA_MASK`) casters
    /// (`FrameRenderer::set_alpha_tested_shadows`): `lab_shadow.wgsl` with `ALPHA_TEST`, the
    /// standard material group at group 2 (and the texture table at group 4 on the table path),
    /// so a batch binds its material and the fragment stage discards what the base-colour alpha
    /// cuts away. `mat_bgl` is the standard material layout the colour pass binds.
    pub(crate) fn instanced_mask_shadow_pipeline(
        &mut self,
        device: &wgpu::Device,
        shadow_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
        double_sided: bool,
    ) -> Result<wgpu::RenderPipeline, RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        self.ensure_object_layout(device)?;
        let object_bgl = self
            .object_bgl
            .clone()
            .ok_or_else(|| RenderError::Shader("gpu object layout missing".into()))?;
        if self.mask_shadow_layout.is_none() {
            let table = crate::texture_table::table_layout(device);
            self.mask_shadow_layout = Some(device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("instanced mask shadow layout"),
                bind_group_layouts: &crate::texture_table::with_table(
                    &[Some(shadow_bgl), Some(&object_bgl), Some(mat_bgl)],
                    table.as_ref(),
                ),
                immediate_size: 0,
            }));
        }
        if let Some(pipe) = self.mask_shadow_pipes.get(&double_sided) {
            return Ok(pipe.clone());
        }
        let mut defs = HashMap::new();
        defs.insert("GPU_DRIVEN".into(), naga_oil::compose::ShaderDefValue::Bool(true));
        defs.insert("ALPHA_TEST".into(), naga_oil::compose::ShaderDefValue::Bool(true));
        if self.foliage_soft_q > 0 {
            defs.insert("SOFT_ALPHA".into(), naga_oil::compose::ShaderDefValue::Bool(true));
            defs.insert(
                "FOLIAGE_SOFT_Q".into(),
                naga_oil::compose::ShaderDefValue::UInt(self.foliage_soft_q),
            );
        }
        let module = shader_module(
            device,
            "lab_shadow_mask_instanced",
            "lab_shadow.wgsl",
            include_str!("../shaders/lab_shadow.wgsl"),
            defs,
        )?;
        let pipe = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some(if double_sided {
                "instanced mask shadow (cull none)"
            } else {
                "instanced mask shadow"
            }),
            layout: Some(
                self.mask_shadow_layout
                    .as_ref()
                    .ok_or_else(|| RenderError::Shader("instanced mask shadow layout missing".into()))?,
            ),
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                buffers: &[Some(LabVertex::layout())],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                targets: &[],
                compilation_options: Default::default(),
            }),
            primitive: cull_primitive(double_sided),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                // `Less`, as the opaque instanced shadow pipeline.
                depth_compare: Some(wgpu::CompareFunction::Less),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        });
        self.mask_shadow_pipes.insert(double_sided, pipe.clone());
        Ok(pipe)
    }

    /// Layouts of the instanced colour and depth passes plus the object storage layout, for
    /// material-graph pipelines built off the frame path (`graph_pipelines`).
    pub(crate) fn graph_layouts(
        &mut self,
        device: &wgpu::Device,
        view_bgl: &wgpu::BindGroupLayout,
        mat_bgl: &wgpu::BindGroupLayout,
    ) -> Result<(wgpu::PipelineLayout, wgpu::PipelineLayout, wgpu::BindGroupLayout), RenderError> {
        self.ensure_object_layout(device)?;
        let object_bgl = self
            .object_bgl
            .clone()
            .ok_or_else(|| RenderError::Shader("gpu object layout missing".into()))?;
        let instanced_bgl = self
            .instanced_bgl
            .clone()
            .ok_or_else(|| RenderError::Shader("instanced object layout missing".into()))?;
        // On the texture table path the standard layouts above carry the table and a record-only
        // material group; graph materials keep their texture bindings (`mat_bgl` is the graph
        // layout there), so they get layouts of their own (ADR-863).
        if crate::texture_table::device_support(device).capacity().is_some() {
            let layout = |label, groups: &[Option<&wgpu::BindGroupLayout>]| {
                device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some(label),
                    bind_group_layouts: groups,
                    immediate_size: 0,
                })
            };
            let color = layout(
                "graph instanced layout",
                &[Some(view_bgl), Some(&instanced_bgl), Some(mat_bgl)],
            );
            let depth = layout(
                "graph instanced depth layout",
                &[Some(view_bgl), Some(&object_bgl), Some(mat_bgl)],
            );
            return Ok((color, depth, object_bgl));
        }
        let color = self
            .instanced_layout
            .get_or_insert_with(|| {
                device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("gpu driven instanced layout"),
                    bind_group_layouts: &[Some(view_bgl), Some(&instanced_bgl), Some(mat_bgl)],
                    immediate_size: 0,
                })
            })
            .clone();
        let depth = self
            .depth_layout
            .get_or_insert_with(|| {
                device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("instanced depth layout"),
                    bind_group_layouts: &[Some(view_bgl), Some(&object_bgl), Some(mat_bgl)],
                    immediate_size: 0,
                })
            })
            .clone();
        Ok((color, depth, object_bgl))
    }

    /// The instanced object group's layout (group 1 of the `LIGHTMAP` instanced variant, ADR-2107).
    pub(crate) fn instanced_object_layout(
        &mut self,
        device: &wgpu::Device,
    ) -> Result<wgpu::BindGroupLayout, RenderError> {
        self.ensure_object_layout(device)?;
        self.instanced_bgl
            .clone()
            .ok_or_else(|| RenderError::Shader("instanced object layout missing".into()))
    }

    fn ensure_object_layout(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        if self.object_bgl.is_some() {
            return Ok(());
        }
        self.object_bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gpu object bgl"),
            entries: &[storage_entry(0, true, true), storage_entry(1, true, true)],
        }));
        self.instanced_bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gpu instanced object bgl"),
            entries: &[
                storage_entry(0, true, true),
                storage_entry(1, true, true),
                storage_entry(2, true, true),
            ],
        }));
        if self.object_buf.is_none() {
            self.object_buf = Some(make_buf(
                device,
                "gpu objects",
                GPU_OBJECT_STRIDE,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            ));
            self.object_cap = GPU_OBJECT_STRIDE;
        }
        Ok(())
    }

    /// Game-frame warms (ADR-7885): the cull and compaction pipelines, also when the warm frame
    /// drew no GPU-driven batch (a warm on a frame before the map's meshes arrived). Returns
    /// whether it built them now.
    pub(crate) fn prewarm_pipes(&mut self, device: &wgpu::Device) -> Result<bool, RenderError> {
        let built = self.cull_pipe.is_none();
        self.ensure_pipes(device)?;
        Ok(built)
    }

    fn ensure_pipes(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        if self.cull_pipe.is_some() {
            return Ok(());
        }
        let cull_mod = shader_module(
            device,
            "gpu_cull",
            "gpu_cull.wgsl",
            include_str!("../shaders/gpu_cull.wgsl"),
            HashMap::new(),
        )?;
        let copy_mod = shader_module(
            device,
            "hiz_copy",
            "hiz_copy.wgsl",
            include_str!("../shaders/hiz_copy.wgsl"),
            HashMap::new(),
        )?;
        let down_mod = shader_module(
            device,
            "hiz_down",
            "hiz_down.wgsl",
            include_str!("../shaders/hiz_down.wgsl"),
            HashMap::new(),
        )?;
        self.cull_bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("gpu cull bgl"),
            entries: &[
                uniform_entry(0),
                storage_entry(1, true, false),
                storage_entry(2, false, false),
                float_tex_entry(3),
                storage_entry(7, false, false),
                storage_entry(8, false, false),
            ],
        }));
        self.copy_bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hiz copy bgl"),
            entries: &[depth_tex_entry(0), storage_tex_entry(1)],
        }));
        self.down_bgl = Some(device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("hiz down bgl"),
            entries: &[float_tex_entry(0), storage_tex_entry(1)],
        }));
        let cull_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("gpu cull layout"),
            bind_group_layouts: &[Some(self.cull_bgl.as_ref().expect("bgl"))],
            immediate_size: 0,
        });
        let copy_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("hiz copy layout"),
            bind_group_layouts: &[Some(self.copy_bgl.as_ref().expect("bgl"))],
            immediate_size: 0,
        });
        let down_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("hiz down layout"),
            bind_group_layouts: &[Some(self.down_bgl.as_ref().expect("bgl"))],
            immediate_size: 0,
        });
        self.cull_pipe = Some(device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("gpu cull"),
            layout: Some(&cull_layout),
            module: &cull_mod,
            entry_point: Some("cs_main"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        }));
        self.copy_pipe = Some(device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("hiz copy"),
            layout: Some(&copy_layout),
            module: &copy_mod,
            entry_point: Some("copy_depth"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        }));
        self.down_pipe = Some(device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some("hiz down"),
            layout: Some(&down_layout),
            module: &down_mod,
            entry_point: Some("downsample"),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(&device).as_deref(),
        }));
        if crate::depth_pyramid::SpdKernel::supported(device) {
            self.spd = Some(crate::depth_pyramid::SpdKernel::new(
                device,
                crate::depth_pyramid::Reduce::Max,
            )?);
        }
        if self.dummy_hiz.is_none() {
            let texture = device.create_texture(&wgpu::TextureDescriptor {
                label: Some("hiz dummy"),
                size: wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::R32Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            self.dummy_hiz = Some((texture, view));
        }
        Ok(())
    }

    fn ensure_hiz(&mut self, device: &wgpu::Device, width: u32, height: u32) -> Result<(), RenderError> {
        let device = &crate::device::TrackedDevice::observe(device);
        if self
            .hiz
            .as_ref()
            .is_some_and(|hiz| hiz.width == width && hiz.height == height)
        {
            self.mip_count = self.hiz.as_ref().map(|hiz| hiz.mips).unwrap_or(1);
            return Ok(());
        }
        let mips = width.max(height).max(1).ilog2() + 1;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("hiz"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: mips,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            // COPY_SRC: tests read the pyramid back.
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::STORAGE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let sample = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("hiz sample"),
            format: Some(wgpu::TextureFormat::R32Float),
            usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
            mip_level_count: Some(mips),
            ..Default::default()
        });
        let mut storage = Vec::with_capacity(mips as usize);
        let mut sampled_mips = Vec::with_capacity(mips as usize);
        for level in 0..mips {
            storage.push(texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("hiz storage mip"),
                format: Some(wgpu::TextureFormat::R32Float),
                usage: Some(wgpu::TextureUsages::STORAGE_BINDING),
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            }));
            sampled_mips.push(texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("hiz sampled mip"),
                format: Some(wgpu::TextureFormat::R32Float),
                usage: Some(wgpu::TextureUsages::TEXTURE_BINDING),
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            }));
        }
        self.mip_count = mips;
        self.hiz_copy_cache.clear();
        self.hiz_down_cache.clear();
        self.cull_cache.clear();
        let spd = self.spd.as_ref().map(|kernel| {
            crate::depth_pyramid::SpdChain::new(device, kernel, &storage, &sampled_mips, width, height)
        });
        self.hiz = Some(HizPyramid {
            texture,
            sample,
            storage,
            sampled_mips,
            width,
            height,
            mips,
            spd,
        });
        Ok(())
    }

    fn ensure_buffers(
        &mut self,
        device: &wgpu::Device,
        instances: u32,
        batches: u32,
    ) -> Result<(), RenderError> {
        let instances = instances.max(1).max(self.reserved);
        // A batch per distinct (mesh, material): at most one per instance. Reserved like the
        // instances, so a view with more variants than the last one does not regrow the
        // commands and rebuild the cull group (the "gpu cull" group at a fixed frame of every
        // 2026-10-08 soak).
        let batches = batches.max(1).max(self.reserved);
        if self.sphere_cap < instances || self.indirect.is_none() {
            let cap = instances.next_power_of_two().max(64);
            let usage = wgpu::BufferUsages::STORAGE
                | wgpu::BufferUsages::INDIRECT
                | wgpu::BufferUsages::COPY_SRC
                | wgpu::BufferUsages::COPY_DST;
            self.spheres = Some(make_buf(
                device,
                "cull spheres",
                u64::from(cap) * 48,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            ));
            self.indirect = Some(make_buf(
                device,
                "cull indirect",
                u64::from(cap) * CMD_STRIDE,
                usage,
            ));
            self.slots = Some(make_buf(
                device,
                "cull slots",
                u64::from(cap) * 4,
                wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::COPY_SRC,
            ));
            self.instanced_cache.clear();
            self.sphere_cap = cap;
            self.sphere_bytes.clear();
            self.cull_cache.clear();
        }
        if self.count_cap < batches || self.batch_cmds.is_none() {
            let cap = batches.next_power_of_two().max(64);
            self.batch_cmds = Some(make_buf(
                device,
                "cull batch commands",
                u64::from(cap) * CMD_STRIDE,
                wgpu::BufferUsages::STORAGE
                    | wgpu::BufferUsages::INDIRECT
                    | wgpu::BufferUsages::COPY_DST
                    | wgpu::BufferUsages::COPY_SRC,
            ));
            self.count_cap = cap;
            self.cull_cache.clear();
        }
        if self.params.is_none() {
            self.params = Some(make_buf(
                device,
                "cull params",
                std::mem::size_of::<CullParams>() as u64,
                wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            ));
        }
        Ok(())
    }
}

/// Groups `items` into instanced batches. `by_material` keeps alpha-tested
/// depth draws on the right material; shadows group by mesh only. The cull mode is part of the
/// key: double-sided batches sort after the culled ones (one pipeline switch per list).
/// The instanced colour batch of `mesh`: `(cull mode, mesh, material, skinned)`. The `Static`
/// marker is not part of it (ADR-5906): splitting a mesh+material by it would only add draws,
/// since instance upload already skips unchanged bytes.
fn batch_key(mesh: &CullMesh) -> (bool, u64, u64, bool) {
    (mesh.double_sided, mesh.mesh, mesh.material, mesh.skinned)
}

/// Whether a submitted mesh is static-eligible (`specs/14` A1), for the all-static batch count.
///
/// The entity must carry the `regia_scene::components::Static` marker, and the draw must be
/// rigid (a skinned draw's joint palette is re-uploaded per frame) and whole (a LOD
/// cross-fade's dither changes per frame). The GPU cull does not split batches by it
/// (ADR-5906); `GpuBatch::is_static` and `DrawStats::static_batches` report batches whose
/// every member is eligible. A moved `Static` entity is a spec violation the editor warns
/// about at play time; it renders correctly.
#[must_use]
pub fn static_batch_eligible(mesh: &crate::FrameMesh<'_>) -> bool {
    mesh.is_static && mesh.joints.is_empty() && mesh.lod_dither == crate::LOD_DITHER_NONE
}

#[cfg(test)]
fn group_instances(items: &[InstanceSrc], by_material: bool) -> (Vec<InstanceObject>, Vec<InstanceBatch>) {
    group_instances_into(items, by_material, None)
}

/// [`group_instances`], also writing each object's index in `items` to `order` (object order).
#[cfg(test)]
fn group_instances_into(
    items: &[InstanceSrc],
    by_material: bool,
    order_out: Option<&mut Vec<u32>>,
) -> (Vec<InstanceObject>, Vec<InstanceBatch>) {
    let mut keys = Vec::new();
    let mut objects = Vec::new();
    let batches = pack_instances(items, by_material, order_out, &mut keys, &mut objects);
    (objects, batches)
}

/// Appends `items` to `objects`, reusing `keys` for the sort. Batch `start` is an index into
/// `objects` (absolute, so a second list continues past the first).
fn pack_instances(
    items: &[InstanceSrc],
    by_material: bool,
    mut order_out: Option<&mut Vec<u32>>,
    keys: &mut Vec<(bool, u64, u64, u32)>,
    objects: &mut Vec<InstanceObject>,
) -> Vec<InstanceBatch> {
    if let Some(out) = order_out.as_deref_mut() {
        out.clear();
    }
    keys.clear();
    let base = objects.len() as u32;
    keys.extend(items.iter().enumerate().map(|(index, item)| {
        (
            item.double_sided,
            item.mesh,
            if by_material { item.material } else { 0 },
            index as u32,
        )
    }));
    keys.sort_unstable();
    let mut batches = Vec::new();
    let mut batch_start = base;
    let mut current: Option<(bool, u64, u64)> = None;
    for (sorted_i, &(double_sided, mesh, material, src)) in keys.iter().enumerate() {
        let item = &items[src as usize];
        let key = (double_sided, mesh, material);
        let at = base + sorted_i as u32;
        if current != Some(key) {
            if let Some((double_sided, mesh, material)) = current {
                batches.push(InstanceBatch {
                    mesh,
                    material,
                    double_sided,
                    start: batch_start,
                    count: at - batch_start,
                });
            }
            current = Some(key);
            batch_start = at;
        }
        if let Some(out) = order_out.as_deref_mut() {
            out.push(src);
        }
        objects.push(InstanceObject {
            model: item.model,
            skin: [item.joint_base, item.lod_dither.to_bits(), 0, 0],
        });
    }
    if let Some((double_sided, mesh, material)) = current {
        batches.push(InstanceBatch {
            mesh,
            material,
            double_sided,
            start: batch_start,
            count: objects.len() as u32 - batch_start,
        });
    }
    batches
}

fn cull_primitive(double_sided: bool) -> wgpu::PrimitiveState {
    wgpu::PrimitiveState {
        topology: wgpu::PrimitiveTopology::TriangleList,
        cull_mode: if double_sided {
            None
        } else {
            Some(wgpu::Face::Back)
        },
        ..Default::default()
    }
}

/// Composes `source` (the engine file `file`, such as `lab_forward.wgsl`) and creates the module
/// as `label`. The composition goes by the file name, not the label: a dev edit of that file
/// (`crate::shader_reload`) and the forward permutation report (`note_compose`) key on it, so a
/// label here once hid every GPU-driven pipeline from both (ADR-3145).
fn shader_module(
    device: &wgpu::Device,
    label: &str,
    file: &str,
    source: &str,
    mut defs: HashMap<String, naga_oil::compose::ShaderDefValue>,
) -> Result<wgpu::ShaderModule, RenderError> {
    // The material binding matches this device's material layout (`bindless`).
    crate::bindless::add_material_defs(device, &mut defs);
    let module = compose_source(source, file, defs)?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
    }))
}

/// A buffer counted on the frame path ([`crate::device::TrackedDevice`]).
fn make_buf(device: &wgpu::Device, label: &str, size: u64, usage: wgpu::BufferUsages) -> wgpu::Buffer {
    crate::device::TrackedDevice::observe(device).create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: size.max(4),
        usage,
        mapped_at_creation: false,
    })
}

fn uniform_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn storage_entry(binding: u32, read_only: bool, vertex: bool) -> wgpu::BindGroupLayoutEntry {
    // Writable storage visible to the vertex stage needs `VERTEX_WRITABLE_STORAGE`,
    // which lavapipe does not enable. Cull outputs stay compute-only.
    let visibility = if vertex {
        wgpu::ShaderStages::COMPUTE | wgpu::ShaderStages::VERTEX
    } else {
        wgpu::ShaderStages::COMPUTE
    };
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn float_tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: false },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn depth_tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Depth,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn storage_tex_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
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

fn buf_binding<'a>(binding: u32, buffer: &'a wgpu::Buffer) -> wgpu::BindGroupEntry<'a> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn view_binding(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "gpu_cull.wgsl",
            "Sphere",
            SphereGpu {
                center_radius,
                index_count,
                first_index,
                base_vertex,
                object_slot,
                batch_id,
                batch_base,
                local,
                _pad1
            }
        ),
        crate::contract_row!(
            "gpu_cull.wgsl",
            "CullParams",
            CullParams {
                forward,
                instance_count,
                ordered,
                planes,
                screen,
                view_proj
            }
        )
        .partial(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `specs/14` A1: the static-batch eligibility rule behind [`GpuBatch::is_static`].
    #[test]
    fn spec14_a1_static_batch_eligibility() {
        let material = crate::lab_frame::LabMaterial::default();
        fn mesh<'a>(
            material: &'a crate::lab_frame::LabMaterial,
            is_static: bool,
            joints: &'a [crate::skin::JointMatrix],
            lod_dither: f32,
        ) -> crate::FrameMesh<'a> {
            crate::FrameMesh {
                is_static,
                mesh_key: 1,
                material_key: 1,
                object_key: 1,
                vertices: &[],
                indices: &[],
                material,
                model: [[0.0; 4]; 4],
                joints,
                receiver_layer: 0,
                shadow: crate::ShadowCast::All,
                uv1: &[],
                lod_dither,
            }
        }
        let rigid: &[crate::skin::JointMatrix] = &[];
        let skinned: &[crate::skin::JointMatrix] = &[crate::skin::JointMatrix::identity()];
        assert!(static_batch_eligible(&mesh(
            &material,
            true,
            rigid,
            crate::LOD_DITHER_NONE
        )));
        assert!(!static_batch_eligible(&mesh(
            &material,
            false,
            rigid,
            crate::LOD_DITHER_NONE
        )));
        assert!(
            !static_batch_eligible(&mesh(&material, true, skinned, crate::LOD_DITHER_NONE)),
            "a skinned draw re-uploads its palette per frame: not static-batch eligible"
        );
        assert!(
            !static_batch_eligible(&mesh(&material, true, rigid, 0.5)),
            "a LOD cross-fade's dither changes per frame: not static-batch eligible"
        );
    }

    /// `specs/14` A1, review finding 3 (ADR-5906): the `Static` marker does not split a
    /// mesh+material batch. Static and dynamic instances share one indirect batch;
    /// `GpuBatch::is_static` reports only the batches whose every member is static.
    #[test]
    fn spec14_a1_static_and_dynamic_instances_share_a_batch() {
        let device = crate::test_device();
        let mut culler = GpuCuller::default();
        let base = CullMesh {
            is_static: false,
            mesh: 7,
            material: 9,
            object: 0,
            center: [0.0, 0.0, 0.0],
            radius: 1.0,
            index_count: 3,
            first_index: 0,
            base_vertex: 0,
            double_sided: false,
            skinned: false,
        };
        let meshes = [
            CullMesh {
                object: 1,
                is_static: true,
                ..base
            },
            CullMesh {
                object: 2,
                is_static: true,
                ..base
            },
            CullMesh { object: 3, ..base },
            CullMesh {
                object: 4,
                material: 10,
                is_static: true,
                ..base
            },
        ];
        culler
            .upload(
                &device.device,
                &mut &device.queue,
                &meshes,
                regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                [[0.0; 4]; 6],
                [0.0, 0.0, 1.0],
                64,
                64,
                false,
            )
            .expect("upload");
        let summary: Vec<_> = culler
            .batches()
            .iter()
            .map(|batch| (batch.mesh, batch.material, batch.is_static, batch.count))
            .collect();
        assert_eq!(
            summary,
            vec![(7, 9, false, 3), (7, 10, true, 1)],
            "one batch per mesh+material whatever the marker; only the all-static one says so"
        );
    }

    /// Review finding 3 measurement: a scene of many statics mixed with dynamic instances of
    /// the same meshes and materials. 8 meshes x 4 materials x 16 instances, every other one
    /// `Static`: one indirect batch per mesh+material (32), as without any marker. The old
    /// static key split each into two (64 batches, so 64 indirect draws).
    #[test]
    fn spec14_a1_many_statics_cost_no_extra_batches() {
        let device = crate::test_device();
        let batches_for = |static_every: Option<u64>| {
            let mut culler = GpuCuller::default();
            let meshes: Vec<CullMesh> = (0..8u64 * 4 * 16)
                .map(|i| CullMesh {
                    // Instance `i / 32` of the mesh+material group `i % 32`.
                    is_static: static_every.is_some_and(|n| (i / 32).is_multiple_of(n)),
                    mesh: i % 8,
                    material: (i / 8) % 4,
                    object: i,
                    center: [i as f32, 0.0, 0.0],
                    radius: 1.0,
                    index_count: 3,
                    first_index: 0,
                    base_vertex: 0,
                    double_sided: false,
                    skinned: false,
                })
                .collect();
            culler
                .upload(
                    &device.device,
                    &mut &device.queue,
                    &meshes,
                    regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                    [[0.0; 4]; 6],
                    [0.0, 0.0, 1.0],
                    64,
                    64,
                    false,
                )
                .expect("upload");
            let all_static = culler.batches().iter().filter(|b| b.is_static).count();
            (culler.batches().len(), all_static)
        };
        let unmarked = batches_for(None);
        let half_static = batches_for(Some(2));
        let all_static = batches_for(Some(1));
        eprintln!(
            "static batching draws: unmarked {unmarked:?}, half static {half_static:?}, \
             all static {all_static:?} (batches, all-static batches)"
        );
        assert_eq!(unmarked, (32, 0));
        assert_eq!(half_static, (32, 0), "the marker adds no indirect batches");
        assert_eq!(
            all_static,
            (32, 32),
            "an all-static scene reports every batch static"
        );
    }

    fn caps(indirect: bool, count: bool, first: bool) -> RenderCapabilities {
        RenderCapabilities {
            adapter_name: "test".into(),
            backend: wgpu::Backend::Vulkan,
            timestamp_query: false,
            multi_draw_indirect: indirect,
            multi_draw_indirect_count: count,
            indirect_first_instance: first,
            rg11b10_renderable: false,
            texture_compression_bc: false,
            texture_compression_astc: false,
            texture_compression_etc2: false,
            early_depth_test: false,
            float32_filterable: false,
            max_storage_buffer_binding_size: 1 << 28,
            max_storage_buffers_per_shader_stage: 8,
            max_storage_textures_per_shader_stage: 8,
            max_uniform_buffers_per_shader_stage: 12,
            max_compute_workgroup_storage_size: 32 << 10,
            max_bind_groups: 4,
            max_texture_dimension_2d: 8192,
        }
    }

    #[test]
    fn strategy_falls_back_when_indirect_features_are_missing() {
        assert_eq!(
            IndirectStrategy::select(&caps(false, false, false)),
            IndirectStrategy::Cpu
        );
        assert_eq!(
            IndirectStrategy::select(&caps(true, false, false)),
            IndirectStrategy::SingleIndirect
        );
        assert_eq!(
            IndirectStrategy::select(&caps(true, false, true)),
            IndirectStrategy::MultiDraw
        );
        assert_eq!(
            IndirectStrategy::select(&caps(true, true, true)),
            IndirectStrategy::MultiDrawCount
        );
        assert!(!IndirectStrategy::SingleIndirect.uses_gpu_objects());
        assert!(IndirectStrategy::MultiDrawCount.uses_gpu_objects());
        let src = |mesh, material| InstanceSrc {
            mesh,
            material,
            model: [[0.0; 4]; 4],
            joint_base: 0,
            lod_dither: 0.0,
            double_sided: false,
        };
        let items = [src(2, 9), src(1, 4), src(2, 9), src(1, 7)];
        let (_objects, batches) = group_instances(&items, false);
        assert_eq!(batches.len(), 2, "shadows collapse to one batch per mesh");
        assert_eq!(batches[0].count + batches[1].count, 4);
        assert!(batches.iter().any(|batch| batch.mesh == 2 && batch.count == 2));
        let (_objects, depth) = group_instances(&items, true);
        assert_eq!(depth.len(), 3, "depth keeps distinct materials apart");
        // Double-sided instances batch (and instance) like culled ones, in batches of their own
        // after every culled batch.
        let ds = |mesh, material| InstanceSrc {
            double_sided: true,
            ..src(mesh, material)
        };
        let mixed = [ds(1, 5), src(2, 9), ds(1, 5), src(1, 4), ds(1, 5), src(2, 9)];
        let (objects, shadow) = group_instances(&mixed, false);
        assert_eq!(objects.len(), mixed.len());
        let culled: Vec<_> = shadow.iter().map(|b| (b.double_sided, b.mesh, b.count)).collect();
        assert_eq!(
            culled,
            vec![(false, 1, 1), (false, 2, 2), (true, 1, 3)],
            "one cull-none batch of three instances, after the culled batches"
        );
        let (_objects, depth) = group_instances(&mixed, true);
        assert_eq!(depth.len(), 3);
        assert!(
            depth
                .last()
                .is_some_and(|b| b.double_sided && b.count == 3 && b.material == 5)
        );
        assert_eq!(std::mem::size_of::<InstanceObject>(), GPU_OBJECT_STRIDE as usize);
        // Uniform CullParams is 208 bytes (a vec3 pad would round it to 224).
        assert_eq!(std::mem::size_of::<CullParams>(), 208);
        assert_eq!(std::mem::size_of::<SphereGpu>(), 48);
    }

    /// Two uploads of the same sources reuse the sort keys and the object buffer, and the
    /// second writes no instance bytes.
    #[test]
    fn steady_instance_lists_reuse_their_buffers() {
        let device = crate::test_device();
        let mut culler = GpuCuller::default();
        let mut belt = crate::staging::StagingBelt::default();
        let src = |mesh| InstanceSrc {
            mesh,
            material: 3,
            model: [[0.0; 4]; 4],
            joint_base: 0,
            lod_dither: 0.0,
            double_sided: false,
        };
        let shadow = vec![vec![src(1), src(2), src(1)]];
        let depth = vec![src(2), src(4)];
        let (shadow_batches, depth_batches) = culler
            .upload_instance_lists(&device.device, &mut belt, &shadow, &depth)
            .expect("upload");
        assert!(!shadow_batches[0].is_empty());
        assert!(!depth_batches.is_empty());
        let growths = culler.scratch_growths();
        assert!(growths > 0, "the first pack allocates the reusable lists");
        let ptr = culler.instance_objects.vec_ptr();
        let cap = culler.instance_objects.vec_capacity();
        assert!(cap >= shadow[0].len() + depth.len());
        assert!(
            culler.last_instance_bytes > 0,
            "the first pack uploads the objects"
        );
        let (again_s, again_d) = culler
            .upload_instance_lists(&device.device, &mut belt, &shadow, &depth)
            .expect("second");
        assert_eq!(again_s, shadow_batches);
        assert_eq!(again_d, depth_batches);
        assert_eq!(culler.scratch_growths(), growths, "the second pack grew a list");
        assert_eq!(
            culler.instance_objects.vec_ptr(),
            ptr,
            "the object buffer was allocated again"
        );
        assert_eq!(culler.instance_objects.vec_capacity(), cap);
        assert_eq!(
            culler.last_instance_bytes, 0,
            "unchanged objects were written again"
        );
    }

    /// The same shadow and depth lists a second time reuse the grouping scratch and write no
    /// object bytes. Batch grouping stays the one `group_instances` already locks.
    #[test]
    fn repeated_instance_lists_reuse_scratch() {
        let device = crate::test_device();
        let mut belt = crate::staging::StagingBelt::default();
        let mut culler = GpuCuller::default();
        let src = |mesh, material, double_sided, x: f32| {
            let mut model = [[0.0f32; 4]; 4];
            model[0][0] = 1.0;
            model[1][1] = 1.0;
            model[2][2] = 1.0;
            model[3][3] = 1.0;
            model[3][0] = x;
            InstanceSrc {
                mesh,
                material,
                model,
                joint_base: 3,
                lod_dither: 0.25,
                double_sided,
            }
        };
        let a = src(1, 4, false, 0.0);
        let b = src(2, 9, false, 1.0);
        let c = src(1, 5, true, 2.0);
        let shadow = [vec![a, b], vec![c, a], vec![b], vec![c, b, a]];
        let depth = [a, c, b];
        let (shadow_batches, depth_batches) = culler
            .upload_instance_lists(&device.device, &mut belt, &shadow, &depth)
            .expect("upload");
        for (list, batches) in shadow.iter().zip(&shadow_batches) {
            let (_, expect) = group_instances(list, true);
            let got: Vec<_> = batches
                .iter()
                .map(|batch| (batch.double_sided, batch.mesh, batch.material, batch.count))
                .collect();
            let want: Vec<_> = expect
                .iter()
                .map(|batch| (batch.double_sided, batch.mesh, batch.material, batch.count))
                .collect();
            assert_eq!(got, want);
        }
        let (_, depth_expect) = group_instances(&depth, true);
        let got: Vec<_> = depth_batches
            .iter()
            .map(|batch| (batch.double_sided, batch.mesh, batch.material, batch.count))
            .collect();
        let want: Vec<_> = depth_expect
            .iter()
            .map(|batch| (batch.double_sided, batch.mesh, batch.material, batch.count))
            .collect();
        assert_eq!(got, want);
        let caps = (
            culler.instance_objects.vec_capacity(),
            culler.instance_order.vec_capacity(),
        );
        let growths = culler.scratch_growths();
        assert!(caps.0 > 0 && caps.1 > 0, "scratch filled: {caps:?}");
        let (shadow2, depth2) = culler
            .upload_instance_lists(&device.device, &mut belt, &shadow, &depth)
            .expect("second upload");
        assert_eq!(shadow2, shadow_batches);
        assert_eq!(depth2, depth_batches);
        assert!(
            culler.scratch_growths() == growths
                && culler.instance_objects.vec_capacity() == caps.0
                && culler.instance_order.vec_capacity() == caps.1
                && culler.last_instance_bytes == 0,
            "growths {growths} -> {}, caps {caps:?} -> ({}, {}), wrote {}",
            culler.scratch_growths(),
            culler.instance_objects.vec_capacity(),
            culler.instance_order.vec_capacity(),
            culler.last_instance_bytes,
        );
    }

    #[test]
    fn occluded_sphere_behind_a_depth_wall_is_not_drawn() {
        let device = crate::test_device();
        let width = 64u32;
        let height = 64u32;
        let depth = device.device.create_texture(&wgpu::TextureDescriptor {
            label: Some("wall depth"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let mut culler = GpuCuller::default();
        // Identity projection: world z is standard-Z. The depth clear is a wall at 0.4.
        // The far sphere's nearest point is 0.83, past that wall. The near sphere is in front.
        let meshes = [
            CullMesh {
                is_static: false,
                mesh: 1,
                material: 1,
                object: 1,
                center: [0.0, 0.0, 0.85],
                radius: 0.02,
                index_count: 3,
                first_index: 0,
                base_vertex: 0,
                double_sided: false,
                skinned: false,
            },
            CullMesh {
                is_static: false,
                mesh: 1,
                material: 1,
                object: 2,
                center: [0.0, 0.0, 0.10],
                radius: 0.02,
                index_count: 3,
                first_index: 0,
                base_vertex: 0,
                double_sided: false,
                skinned: false,
            },
        ];
        culler
            .upload(
                &device.device,
                &mut &device.queue,
                &meshes,
                regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                [[0.0; 4]; 6],
                [0.0, 0.0, 1.0],
                width,
                height,
                true,
            )
            .expect("upload");
        // upload() records mip_count before the pyramid exists. Rebuild parameters
        // after the pyramid so the shader samples a real mip.
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("occlusion"),
            });
        {
            let pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear wall"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(0.4),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            drop(pass);
        }
        culler
            .build_hiz(&device.device, &mut encoder, &depth_view, width, height)
            .expect("hiz");
        device.queue.submit([encoder.finish()]);
        culler
            .upload(
                &device.device,
                &mut &device.queue,
                &meshes,
                regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                [[0.0; 4]; 6],
                [0.0, 0.0, 1.0],
                width,
                height,
                true,
            )
            .expect("reupload");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("cull") });
        culler.dispatch(&device.device, &mut encoder);
        device.queue.submit([encoder.finish()]);
        let counts = culler.read_instance_counts(&device).expect("readback");
        assert_eq!(counts.len(), 2, "two indirect commands");
        let hidden = counts[0];
        let shown = counts[1];
        assert_eq!(
            hidden, 0,
            "sphere behind the depth wall is culled, counts={counts:?}"
        );
        assert_eq!(shown, 1, "sphere in front of the wall draws, counts={counts:?}");
    }

    /// The single-pass pyramid must equal the per-mip chain texel for texel, so culling
    /// decisions cannot change (sizes with odd edges and a 1-texel-high tail included).
    #[test]
    fn spd_hiz_matches_the_per_mip_chain_bit_for_bit() {
        let device = crate::test_device();
        if !crate::depth_pyramid::SpdKernel::supported(&device.device) {
            return;
        }
        for (w, h) in [(333u32, 97u32), (1000, 8), (64, 36), (5, 3), (1, 1), (1280, 720)] {
            let depth = crate::depth_pyramid::test_util::hashed_depth(&device, w, h, w ^ (h << 8));
            let build = |legacy: bool| {
                let mut culler = GpuCuller {
                    legacy_hiz: legacy,
                    ..GpuCuller::default()
                };
                let mut encoder = device
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                culler
                    .build_hiz(&device.device, &mut encoder, &depth, w, h)
                    .expect("hiz");
                device.queue.submit([encoder.finish()]);
                assert!(culler.spd.is_some());
                culler
            };
            let legacy = build(true);
            let spd = build(false);
            let (Some(a), Some(b)) = (legacy.hiz.as_ref(), spd.hiz.as_ref()) else {
                panic!("no pyramid");
            };
            assert_eq!(a.mips, b.mips);
            for level in 0..a.mips {
                let (_, _, want) = crate::depth_pyramid::test_util::read_r32(&device, &a.texture, level);
                let (_, _, have) = crate::depth_pyramid::test_util::read_r32(&device, &b.texture, level);
                let diff = want
                    .iter()
                    .zip(&have)
                    .position(|(x, y)| x.to_bits() != y.to_bits());
                assert_eq!(diff, None, "{w}×{h} level {level} first differing texel");
            }
        }
    }

    /// Hi-Z build cost at 2560×1440, SPD against the per-mip chain (`--ignored`, on the bench GPU):
    /// 200 builds per encoder, wall time of the GPU work divided by 200.
    #[test]
    #[ignore = "timing; run on the bench GPU"]
    fn hiz_build_cost_spd_vs_per_mip() {
        let device = crate::test_device();
        let (w, h) = (2560u32, 1440u32);
        let depth = crate::depth_pyramid::test_util::hashed_depth(&device, w, h, 3);
        for legacy in [true, false, true, false] {
            let mut culler = GpuCuller {
                legacy_hiz: legacy,
                ..GpuCuller::default()
            };
            let run = |culler: &mut GpuCuller, n: u32| {
                let mut encoder = device
                    .device
                    .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                for _ in 0..n {
                    culler
                        .build_hiz(&device.device, &mut encoder, &depth, w, h)
                        .expect("hiz");
                }
                let start = std::time::Instant::now();
                device.queue.submit([encoder.finish()]);
                device
                    .tracked()
                    .poll(wgpu::PollType::wait_indefinitely())
                    .expect("poll");
                start.elapsed().as_secs_f64() * 1000.0 / f64::from(n)
            };
            run(&mut culler, 10);
            let ms = run(&mut culler, 200);
            eprintln!(
                "hiz build {w}×{h} {}: {ms:.4} ms",
                if legacy {
                    "per-mip (12 dispatches)"
                } else {
                    "SPD (2 dispatches)"
                }
            );
        }
    }

    #[test]
    fn hundred_thousand_spheres_are_frustum_culled_on_the_gpu() {
        let device = crate::test_device();
        const N: usize = 100_000;
        let meshes: Vec<CullMesh> = (0..N)
            .map(|i| {
                let visible = i % 2 == 0;
                CullMesh {
                    is_static: false,
                    mesh: 1,
                    material: 1,
                    object: i as u64,
                    center: [0.0, if visible { 0.0 } else { -5.0 }, 0.0],
                    radius: 0.1,
                    index_count: 36,
                    first_index: 0,
                    base_vertex: 0,
                    double_sided: false,
                    skinned: false,
                }
            })
            .collect();
        // Plane y = 0, inside is y >= -radius. The low spheres are fully outside.
        let mut planes = [[0.0; 4]; 6];
        planes[0] = [0.0, 1.0, 0.0, 0.0];
        let mut culler = GpuCuller::default();
        culler
            .upload(
                &device.device,
                &mut &device.queue,
                &meshes,
                regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                planes,
                [0.0, 0.0, 1.0],
                64,
                64,
                false,
            )
            .expect("upload");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cull 100k"),
            });
        culler.dispatch(&device.device, &mut encoder);
        device.queue.submit([encoder.finish()]);
        let counts = culler.read_instance_counts(&device).expect("readback");
        let drawn = counts.iter().filter(|count| **count == 1).count();
        let culled = counts.iter().filter(|count| **count == 0).count();
        assert_eq!(counts.len(), N);
        assert_eq!(drawn, N / 2, "half the spheres sit on the y = 0 plane");
        assert_eq!(culled, N / 2, "half the spheres are below the plane");
    }

    // ---- Hi-Z occlusion correctness (ADR-1238) ----------------------------------------------
    //
    // A sphere the cull drops still has its depth in the prepass, so a wrong cull is a hole of
    // clear colour, not a missing pebble. These tests hold the cull to one property: it drops a
    // sphere only when every pixel the sphere covers is nearer than the sphere's nearest point.

    use regia_core::glam::{Mat4, Vec2, Vec3, Vec4};

    /// One pyramid level: width, height, row-major texels.
    type Level = (u32, u32, Vec<f32>);

    /// CPU copy of the farthest-depth pyramid (`hiz_down.wgsl` and the SPD chain): wgpu level
    /// sizes `max(1, size >> level)`, a 2×2 max with reads clamped into the source level.
    fn cpu_pyramid(depth: &[f32], width: u32, height: u32) -> Vec<Level> {
        let mips = width.max(height).max(1).ilog2() + 1;
        let mut levels: Vec<Level> = vec![(width, height, depth.to_vec())];
        for level in 1..mips {
            let next = {
                let (sw, sh, src) = &levels[level as usize - 1];
                let (dw, dh) = ((width >> level).max(1), (height >> level).max(1));
                let mut dst = vec![0.0f32; (dw * dh) as usize];
                for y in 0..dh {
                    for x in 0..dw {
                        let mut far = 0.0f32;
                        for (ox, oy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                            let sx = (2 * x + ox).min(sw - 1);
                            let sy = (2 * y + oy).min(sh - 1);
                            far = far.max(src[(sy * sw + sx) as usize]);
                        }
                        dst[(y * dw + x) as usize] = far;
                    }
                }
                (dw, dh, dst)
            };
            levels.push(next);
        }
        levels
    }

    /// `hiz_cover` in `gpu_cull.wgsl`.
    fn hiz_cover(size: u32, mip: u32) -> u32 {
        let level = size >> mip;
        if level == 0 {
            1 << size.max(1).ilog2()
        } else {
            level << mip
        }
    }

    /// CPU mirror of `hiz_occluded` in `gpu_cull.wgsl` after ADR-1238.
    fn cpu_occluded(pyramid: &[Level], vp: &Mat4, forward: Vec3, center: Vec3, radius: f32) -> bool {
        let clip = *vp * center.extend(1.0);
        let m = vp.to_cols_array_2d();
        let ax = Vec3::new(m[0][0], m[1][0], m[2][0]);
        let ay = Vec3::new(m[0][1], m[1][1], m[2][1]);
        let aw = Vec3::new(m[0][3], m[1][3], m[2][3]);
        let reach = radius * aw.length();
        if clip.w - radius <= 0.0 || clip.w - reach <= 0.0 {
            return false;
        }
        let ndc = Vec2::new(clip.x / clip.w, clip.y / clip.w);
        let denom = clip.w - reach;
        let half_ndc = Vec2::new((ax - ndc.x * aw).length(), (ay - ndc.y * aw).length()) * (radius / denom);
        let (w0, h0) = (pyramid[0].0, pyramid[0].1);
        let dims = Vec2::new(w0 as f32, h0 as f32);
        let center_px = Vec2::new(ndc.x * 0.5 + 0.5, 0.5 - ndc.y * 0.5) * dims;
        let half_px = half_ndc * 0.5 * dims;
        let (lo_f, hi_f) = (center_px - half_px, center_px + half_px);
        if hi_f.x < 0.0 || hi_f.y < 0.0 || lo_f.x >= dims.x || lo_f.y >= dims.y {
            return false;
        }
        let lo = (lo_f.x.floor().max(0.0) as u32, lo_f.y.floor().max(0.0) as u32);
        let hi = (
            hi_f.x.floor().min(dims.x - 1.0) as u32,
            hi_f.y.floor().min(dims.y - 1.0) as u32,
        );
        let span = (hi.0 - lo.0).max(hi.1 - lo.1) + 1;
        // 2^(mip + 1) >= span: at most three texels per axis.
        let half_texel_mip = if span > 2 { (span - 1).ilog2() } else { 0 };
        let mip = half_texel_mip.min(pyramid.len() as u32 - 1);
        if hi.0 >= hiz_cover(w0, mip) || hi.1 >= hiz_cover(h0, mip) {
            return false;
        }
        let (t_lo, t_hi) = ((lo.0 >> mip, lo.1 >> mip), (hi.0 >> mip, hi.1 >> mip));
        if t_hi.0 - t_lo.0 > 2 || t_hi.1 - t_lo.1 > 2 {
            return false;
        }
        let (lw, _, texels) = &pyramid[mip as usize];
        let mut farthest = 0.0f32;
        for y in t_lo.1..=t_hi.1 {
            for x in t_lo.0..=t_hi.0 {
                farthest = farthest.max(texels[(y * lw + x) as usize]);
            }
        }
        let toward = if aw.length_squared() > 1.0e-12 {
            aw.normalize()
        } else {
            forward
        };
        let z = |p: Vec3| {
            let c = *vp * p.extend(1.0);
            c.z / c.w
        };
        z(center).min(z(center - toward * radius)) > farthest + 0.002
    }

    /// CPU mirror of the test before ADR-1238: one texel at the centre, the mip from
    /// `radius / w * max(width, height) / 2` pixels, the texel from `uv * level size`.
    fn cpu_occluded_before_fix(
        pyramid: &[Level],
        vp: &Mat4,
        forward: Vec3,
        center: Vec3,
        radius: f32,
    ) -> bool {
        let ndc_of = |p: Vec3| {
            let c = *vp * p.extend(1.0);
            Vec4::new(c.x / c.w, c.y / c.w, c.z / c.w, c.w)
        };
        let c = ndc_of(center);
        if c.w - radius <= 0.0 {
            return false;
        }
        let nearest_z = c.z.min(ndc_of(center - forward * radius).z);
        let (w, h) = (pyramid[0].0 as f32, pyramid[0].1 as f32);
        let px = radius / c.w.abs().max(1.0e-3) * 0.5 * w.max(h);
        let p = Vec2::new((c.x * 0.5 + 0.5) * w, (0.5 - c.y * 0.5) * h);
        if p.x < -px || p.y < -px || p.x > w + px || p.y > h + px {
            return false;
        }
        let mip = ((px * 2.0).max(1.0).log2().ceil() as u32).min(pyramid.len() as u32 - 1);
        let (lw, lh, texels) = &pyramid[mip as usize];
        let x = ((p.x / w) * *lw as f32).clamp(0.0, *lw as f32 - 1.0) as u32;
        let y = ((p.y / h) * *lh as f32).clamp(0.0, *lh as f32 - 1.0) as u32;
        nearest_z > texels[(y * lw + x) as usize] + 0.002
    }

    /// Ground truth: some pixel the sphere covers holds a depth at or behind the sphere's
    /// nearest point, so geometry inside the sphere may show there and a cull would be wrong.
    /// The footprint is the projection of a Fibonacci lattice on the sphere (dense enough for
    /// the radii below), so it never invents coverage.
    fn possibly_visible(
        depth: &[f32],
        width: u32,
        height: u32,
        vp: &Mat4,
        center: Vec3,
        radius: f32,
    ) -> bool {
        const SAMPLES: u32 = 8192;
        let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
        let mut nearest = f32::INFINITY;
        let mut pixels = Vec::new();
        for i in 0..=SAMPLES {
            let p = if i == SAMPLES {
                center
            } else {
                let y = 1.0 - 2.0 * (i as f32 + 0.5) / SAMPLES as f32;
                let ring = (1.0 - y * y).max(0.0).sqrt();
                let a = golden * i as f32;
                center + Vec3::new(a.cos() * ring, y, a.sin() * ring) * radius
            };
            let c = *vp * p.extend(1.0);
            if c.w <= 0.0 {
                return true;
            }
            nearest = nearest.min(c.z / c.w);
            let px = ((c.x / c.w) * 0.5 + 0.5) * width as f32;
            let py = (0.5 - (c.y / c.w) * 0.5) * height as f32;
            if px >= 0.0 && py >= 0.0 && px < width as f32 && py < height as f32 {
                pixels.push((px as u32, py as u32));
            }
        }
        pixels
            .iter()
            .any(|&(x, y)| depth[(y * width + x) as usize] >= nearest)
    }

    /// Deterministic generator for the randomized cases.
    struct Lcg(u64);

    impl Lcg {
        /// Uniform in `0..1`.
        fn unit(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            (self.0 >> 40) as f32 / (1u64 << 24) as f32
        }
    }

    /// Standard-Z depth of view distance `d` for [`Mat4::perspective_rh`] with near 1, far 200.
    fn depth_at(d: f32) -> f32 {
        let (near, far) = (1.0f32, 200.0f32);
        far * (d - near) / ((far - near) * d)
    }

    /// A wall at 4 m with sky-depth holes: blocks of 4–16 px, one-pixel slits, and sometimes
    /// the last row or column open (the texels an odd level drops).
    fn structured_depth(width: u32, height: u32, rng: &mut Lcg) -> Vec<f32> {
        let wall = depth_at(4.0);
        let mut depth = vec![wall; (width * height) as usize];
        let open = |x0: u32, y0: u32, x1: u32, y1: u32, depth: &mut [f32]| {
            for y in y0..y1.min(height) {
                for x in x0..x1.min(width) {
                    depth[(y * width + x) as usize] = 1.0;
                }
            }
        };
        for _ in 0..(width * height / 800).max(2) {
            let x = (rng.unit() * width as f32) as u32;
            let y = (rng.unit() * height as f32) as u32;
            let s = 4 + (rng.unit() * 12.0) as u32;
            open(x, y, x + s, y + s, &mut depth);
        }
        for x in 0..width {
            if rng.unit() < 1.0 / 32.0 {
                open(x, 0, x + 1, height, &mut depth);
            }
        }
        for y in 0..height {
            if rng.unit() < 1.0 / 32.0 {
                open(0, y, width, y + 1, &mut depth);
            }
        }
        if rng.unit() < 0.5 {
            open(0, height - 1, width, height, &mut depth);
        }
        if rng.unit() < 0.5 {
            open(width - 1, 0, width, height, &mut depth);
        }
        depth
    }

    /// A camera at the origin looking down -Z, depth 0 at 1 m and 1 at 200 m.
    fn perspective(fov_deg: f32, width: u32, height: u32) -> Mat4 {
        regia_core::glam::camera::rh::proj::directx::perspective(
            fov_deg.to_radians(),
            width as f32 / height as f32,
            1.0,
            200.0,
        )
    }

    /// `count` spheres behind the wall (6–40 m), 0.5–12 px in radius, centred anywhere on
    /// screen (and a few pixels past its edges).
    fn random_spheres(vp: &Mat4, height: u32, count: usize, rng: &mut Lcg) -> Vec<(Vec3, f32)> {
        let m = vp.to_cols_array_2d();
        let (sx, sy) = (m[0][0], m[1][1]);
        (0..count)
            .map(|_| {
                let d = 6.0 + rng.unit() * 34.0;
                let ndc = Vec2::new(rng.unit() * 2.2 - 1.1, rng.unit() * 2.2 - 1.1);
                let px = 0.5 + rng.unit() * 11.5;
                // A pixel is 2 / height NDC units; NDC radius ≈ radius · sy / d.
                let radius = px * 2.0 / height as f32 * d / sy;
                (Vec3::new(ndc.x * d / sx, ndc.y * d / sy, -d), radius)
            })
            .collect()
    }

    /// Every randomized case: the CPU mirror never drops a sphere that may show, and still
    /// drops a fair share of the hidden ones (the property is not met by culling nothing).
    #[test]
    fn hiz_mirror_never_culls_a_visible_sphere() {
        let mut rng = Lcg(0x5EED_1238);
        let (mut culled, mut hidden, mut total) = (0usize, 0usize, 0usize);
        for (width, height) in [(64u32, 64u32), (96, 54), (61, 37), (128, 72), (33, 100)] {
            for fov in [8.0f32, 45.0, 70.0, 100.0] {
                let depth = structured_depth(width, height, &mut rng);
                let pyramid = cpu_pyramid(&depth, width, height);
                let vp = perspective(fov, width, height);
                for (center, radius) in random_spheres(&vp, height, 200, &mut rng) {
                    total += 1;
                    let visible = possibly_visible(&depth, width, height, &vp, center, radius);
                    if !visible {
                        hidden += 1;
                    }
                    if cpu_occluded(&pyramid, &vp, Vec3::NEG_Z, center, radius) {
                        culled += 1;
                        assert!(
                            !visible,
                            "{width}×{height} fov {fov}: a sphere at {center} r {radius} shows through a hole \
                             but was culled"
                        );
                    }
                }
            }
        }
        // A Python port of this test (lane report, 2026-10-21) gave 1257 hidden of 4000 and 580
        // culled (46 %); the test before ADR-1238 culled 572 spheres that show.
        assert!(
            hidden > total / 5,
            "the cases must hide spheres ({hidden} of {total})"
        );
        assert!(
            culled * 3 > hidden,
            "the conservative test still culls a third of the hidden spheres ({culled} of {hidden})"
        );
    }

    /// A wall at depth 0.5 with a one-pixel open column at x = 32 (64×64, identity matrix). A
    /// sphere 2 px in radius centred at x = 31.75 covers that column's pixel centres. The test
    /// before ADR-1238 read only the mip-2 texel holding the centre (x 28..31) and culled it.
    fn slit_case() -> (Vec<f32>, [(Vec3, f32); 2]) {
        let (w, h) = (64u32, 64u32);
        let mut depth = vec![0.5f32; (w * h) as usize];
        for y in 0..h {
            depth[(y * w + 32) as usize] = 1.0;
        }
        let radius = 2.0 / 32.0;
        let through_slit = (Vec3::new(31.75 / 32.0 - 1.0, 0.0, 0.9), radius);
        let behind_wall = (Vec3::new(10.0 / 32.0 - 1.0, 0.0, 0.9), radius);
        (depth, [through_slit, behind_wall])
    }

    /// 64×36: level 2 is 16×9, so level 3 (8×4) covers rows 0..31 only. Rows 32..35 are open;
    /// a sphere centred on row 33 shows there. The old test mapped it to the level-3 texel of
    /// rows 24..31 (all wall) and culled it.
    fn odd_edge_case() -> (Vec<f32>, [(Vec3, f32); 2]) {
        let (w, h) = (64u32, 36u32);
        let mut depth = vec![0.5f32; (w * h) as usize];
        for y in 32..h {
            for x in 0..w {
                depth[(y * w + x) as usize] = 1.0;
            }
        }
        let radius = 0.1;
        // Row 33 centre: 0.5 - ndc_y / 2 = 33 / 36.
        let ndc_y = 1.0 - 2.0 * 33.0 / 36.0;
        let at_edge = (Vec3::new(20.0 / 32.0 - 1.0, ndc_y, 0.9), radius);
        let behind_wall = (Vec3::new(20.0 / 32.0 - 1.0, 1.0 - 2.0 * 10.0 / 36.0, 0.9), radius);
        (depth, [at_edge, behind_wall])
    }

    /// 64×64 at an 8° field of view: a 0.3 m sphere at 20 m is ~5.5 px in radius, but the old
    /// estimate (no projection scale) made it 0.5 px and read the single centre pixel. The wall
    /// is open from 3 px right of the centre.
    fn narrow_fov_case() -> (Mat4, Vec<f32>, [(Vec3, f32); 2]) {
        let (w, h) = (64u32, 64u32);
        let vp = perspective(8.0, w, h);
        let mut depth = vec![depth_at(4.0); (w * h) as usize];
        for y in 0..h {
            for x in 35..w {
                depth[(y * w + x) as usize] = 1.0;
            }
        }
        let behind_wall_left = (Vec3::new(-0.6, 0.0, -20.0), 0.3);
        (vp, depth, [(Vec3::new(0.0, 0.0, -20.0), 0.3), behind_wall_left])
    }

    /// One regression case: the first sphere shows through the opening, the second is behind
    /// the wall.
    struct HizCase {
        name: &'static str,
        depth: Vec<f32>,
        width: u32,
        height: u32,
        vp: Mat4,
        forward: Vec3,
        spheres: [(Vec3, f32); 2],
    }

    fn regression_cases() -> Vec<HizCase> {
        let (slit, slit_spheres) = slit_case();
        let (edge, edge_spheres) = odd_edge_case();
        let (narrow_vp, narrow, narrow_spheres) = narrow_fov_case();
        vec![
            HizCase {
                name: "slit",
                depth: slit,
                width: 64,
                height: 64,
                vp: Mat4::IDENTITY,
                forward: Vec3::Z,
                spheres: slit_spheres,
            },
            HizCase {
                name: "odd edge",
                depth: edge,
                width: 64,
                height: 36,
                vp: Mat4::IDENTITY,
                forward: Vec3::Z,
                spheres: edge_spheres,
            },
            HizCase {
                name: "narrow fov",
                depth: narrow,
                width: 64,
                height: 64,
                vp: narrow_vp,
                forward: Vec3::NEG_Z,
                spheres: narrow_spheres,
            },
        ]
    }

    #[test]
    fn hiz_regressions_show_through_and_stay_culled_behind_the_wall() {
        for case in regression_cases() {
            let HizCase {
                name,
                depth,
                width: w,
                height: h,
                vp,
                forward: fwd,
                spheres,
            } = case;
            let pyramid = cpu_pyramid(&depth, w, h);
            let (shows, hidden) = (spheres[0], spheres[1]);
            assert!(
                possibly_visible(&depth, w, h, &vp, shows.0, shows.1),
                "{name}: the case must actually show the first sphere"
            );
            assert!(
                !possibly_visible(&depth, w, h, &vp, hidden.0, hidden.1),
                "{name}: the case must actually hide the second sphere"
            );
            assert!(
                cpu_occluded_before_fix(&pyramid, &vp, fwd, shows.0, shows.1),
                "{name}: the old test culled the visible sphere (the defect this case pins)"
            );
            assert!(
                !cpu_occluded(&pyramid, &vp, fwd, shows.0, shows.1),
                "{name}: the visible sphere must be drawn"
            );
            assert!(
                cpu_occluded(&pyramid, &vp, fwd, hidden.0, hidden.1),
                "{name}: the hidden sphere is still culled"
            );
        }
    }

    #[test]
    fn hiz_cover_matches_the_pyramid_level_sizes() {
        // 1080 rows: levels 540, 270, 135, 67 (row 134 of 135 dropped), 33, 16, 8, 4, 2, 1.
        assert_eq!(hiz_cover(1080, 0), 1080);
        assert_eq!(hiz_cover(1080, 3), 1080);
        assert_eq!(hiz_cover(1080, 4), 67 * 16);
        assert_eq!(hiz_cover(1080, 10), 1024);
        // Past the axis' last real level the single texel keeps that level's cover.
        assert_eq!(hiz_cover(1080, 11), 1024);
        assert_eq!(hiz_cover(36, 3), 32);
        assert_eq!(hiz_cover(1, 5), 1);
        // The CPU pyramid agrees: a far value in the dropped row never reaches level 3.
        let mut depth = vec![0.25f32; 64 * 36];
        depth[35 * 64 + 7] = 1.0;
        let pyramid = cpu_pyramid(&depth, 64, 36);
        assert!(pyramid[3].2.iter().all(|&texel| texel == 0.25));
        assert_eq!(
            pyramid[2].2[8 * 16 + 1],
            1.0,
            "level 2 still holds row 35 (its row 8)"
        );
    }

    /// A Depth32Float holding `values` (row-major), written by a full-screen triangle: depth
    /// formats take no buffer copies.
    fn depth_from(device: &RenderDevice, width: u32, height: u32, values: &[f32]) -> wgpu::TextureView {
        let d = &device.device;
        let texture = d.create_texture(&wgpu::TextureDescriptor {
            label: Some("test depth"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let buffer = d.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test depth values"),
            size: (values.len() * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        device
            .queue
            .write_buffer(&buffer, 0, bytemuck::cast_slice(values));
        let source = format!(
            "@group(0) @binding(0) var<storage, read> values: array<f32>;
             @vertex fn vs(@builtin(vertex_index) i: u32) -> @builtin(position) vec4<f32> {{
                 let x = select(-1.0, 3.0, i == 1u);
                 let y = select(-1.0, 3.0, i == 2u);
                 return vec4<f32>(x, y, 0.5, 1.0);
             }}
             struct Out {{ @builtin(frag_depth) depth: f32 }}
             @fragment fn fs(@builtin(position) p: vec4<f32>) -> Out {{
                 return Out(values[u32(p.y) * {width}u + u32(p.x)]);
             }}"
        );
        let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("test depth values"),
            source: wgpu::ShaderSource::Wgsl(source.into()),
        });
        let pipe = d.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("test depth values"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs"),
                targets: &[],
                compilation_options: Default::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("test depth values"),
            layout: &pipe.get_bind_group_layout(0),
            entries: &[buf_binding(0, &buffer)],
        });
        let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("test depth values"),
                color_attachments: &[],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.counted_set_pipeline(&pipe);
            pass.counted_set_bind_group(0, &bind, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        device.queue.submit([encoder.finish()]);
        view
    }

    /// Builds the GPU pyramid from `depth`, culls `spheres` against it and returns whether each
    /// one was drawn, in input order; checks the GPU pyramid against [`cpu_pyramid`] first.
    fn gpu_cull(
        device: &RenderDevice,
        depth: &[f32],
        width: u32,
        height: u32,
        vp: &Mat4,
        forward: Vec3,
        spheres: &[(Vec3, f32)],
    ) -> Vec<bool> {
        let view = depth_from(device, width, height, depth);
        let mut culler = GpuCuller::default();
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("hiz") });
        culler
            .build_hiz(&device.device, &mut encoder, &view, width, height)
            .expect("hiz");
        device.queue.submit([encoder.finish()]);
        let pyramid = cpu_pyramid(depth, width, height);
        let texture = &culler.hiz.as_ref().expect("pyramid").texture;
        for (level, (_, _, want)) in pyramid.iter().enumerate() {
            let (_, _, have) = crate::depth_pyramid::test_util::read_r32(device, texture, level as u32);
            assert!(
                want.iter().zip(&have).all(|(a, b)| a.to_bits() == b.to_bits()),
                "{width}×{height}: GPU pyramid level {level} differs from the CPU copy"
            );
        }
        let meshes: Vec<CullMesh> = spheres
            .iter()
            .enumerate()
            .map(|(i, &(center, radius))| CullMesh {
                is_static: false,
                mesh: 1,
                material: 1,
                object: i as u64,
                center: center.to_array(),
                radius,
                index_count: 3,
                first_index: 0,
                base_vertex: 0,
                double_sided: false,
                skinned: false,
            })
            .collect();
        culler
            .upload(
                &device.device,
                &mut &device.queue,
                &meshes,
                vp.to_cols_array_2d(),
                [[0.0; 4]; 6],
                forward.to_array(),
                width,
                height,
                true,
            )
            .expect("upload");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("cull") });
        culler.dispatch(&device.device, &mut encoder);
        device.queue.submit([encoder.finish()]);
        let counts = culler.read_instance_counts(device).expect("readback");
        let mut drawn = vec![false; spheres.len()];
        for draw in culler.draws() {
            drawn[draw.object as usize] = counts[draw.index as usize] == 1;
        }
        drawn
    }

    /// The shader on the regression cases: the slit, the rows an odd level drops, and a
    /// narrow field of view.
    #[test]
    fn gpu_hiz_regressions_draw_what_shows_through() {
        let device = crate::test_device();
        for case in regression_cases() {
            let drawn = gpu_cull(
                &device,
                &case.depth,
                case.width,
                case.height,
                &case.vp,
                case.forward,
                &case.spheres,
            );
            assert_eq!(
                drawn,
                [true, false],
                "{}: draw what shows through the opening, cull what is behind the wall",
                case.name
            );
        }
    }

    /// The shader against the ground truth on the randomized cases (odd sizes, 8°–100°), and
    /// against its CPU mirror (decisions may differ only where a float lands on a pixel edge).
    #[test]
    fn gpu_hiz_never_culls_a_visible_sphere() {
        let device = crate::test_device();
        let mut rng = Lcg(0xC011_1238);
        let (mut culled, mut disagree, mut total) = (0usize, 0usize, 0usize);
        for (width, height) in [(64u32, 64u32), (61, 37), (128, 72)] {
            for fov in [8.0f32, 60.0, 100.0] {
                let depth = structured_depth(width, height, &mut rng);
                let vp = perspective(fov, width, height);
                let spheres = random_spheres(&vp, height, 256, &mut rng);
                let drawn = gpu_cull(&device, &depth, width, height, &vp, Vec3::NEG_Z, &spheres);
                let pyramid = cpu_pyramid(&depth, width, height);
                for (&(center, radius), &was_drawn) in spheres.iter().zip(&drawn) {
                    total += 1;
                    if !was_drawn {
                        culled += 1;
                        assert!(
                            !possibly_visible(&depth, width, height, &vp, center, radius),
                            "{width}×{height} fov {fov}: the GPU culled a sphere at {center} r {radius} \
                             that shows through a hole"
                        );
                    }
                    if was_drawn == cpu_occluded(&pyramid, &vp, Vec3::NEG_Z, center, radius) {
                        disagree += 1;
                    }
                }
            }
        }
        assert!(
            culled > total / 20,
            "the GPU still culls hidden spheres ({culled} of {total})"
        );
        assert!(
            disagree * 50 <= total,
            "the shader and its CPU mirror disagree on {disagree} of {total} spheres"
        );
    }

    /// Skinned instances batch by `(cull mode, mesh, material, skinned)` (ADR-1368): a crowd of
    /// one skinned mesh is one batch however many units it has, a rigid draw of the same mesh
    /// is a batch of its own, and a unit whose palette sphere is outside the frustum gets
    /// instance count 0.
    #[test]
    fn skinned_instances_batch_apart_from_rigid_and_cull_by_their_sphere() {
        let device = crate::test_device();
        let mesh = |object: u64, mesh: u64, skinned: bool, y: f32| CullMesh {
            is_static: false,
            mesh,
            material: 3,
            object,
            center: [object as f32 * 0.01, y, 0.0],
            radius: 0.1,
            index_count: 36,
            first_index: 0,
            base_vertex: 0,
            double_sided: false,
            skinned,
        };
        // 64 skinned units of mesh 7 (the last one outside the frustum by its sphere), three
        // rigid draws of mesh 7 and two skinned units of mesh 8, interleaved.
        let mut meshes = Vec::new();
        for i in 0..64u64 {
            meshes.push(mesh(i, 7, true, if i == 63 { -5.0 } else { 0.0 }));
            if i % 20 == 0 {
                meshes.push(mesh(1_000 + i, 7, false, 0.0));
            }
            if i % 40 == 0 {
                meshes.push(mesh(2_000 + i, 8, true, 0.0));
            }
        }
        let mut planes = [[0.0; 4]; 6];
        planes[0] = [0.0, 1.0, 0.0, 0.0];
        let mut culler = GpuCuller::default();
        culler
            .upload(
                &device.device,
                &mut &device.queue,
                &meshes,
                regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                planes,
                [0.0, 0.0, 1.0],
                64,
                64,
                false,
            )
            .expect("upload");
        let batches: Vec<_> = culler
            .batches()
            .iter()
            .map(|b| (b.mesh, b.skinned, b.count))
            .collect();
        assert_eq!(
            batches,
            vec![(7, false, 4), (7, true, 64), (8, true, 2)],
            "one batch per (mesh, skinned): the 64-unit crowd is one draw"
        );
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cull skinned"),
            });
        culler.dispatch(&device.device, &mut encoder);
        device.queue.submit([encoder.finish()]);
        let counts = culler.read_instance_counts(&device).expect("readback");
        let count_of = |object: u64| {
            culler
                .draws()
                .iter()
                .find(|draw| draw.object == object)
                .map(|draw| counts[draw.index as usize])
        };
        assert_eq!(count_of(0), Some(1), "a unit inside the frustum draws");
        assert_eq!(
            count_of(63),
            Some(0),
            "a unit outside the frustum by its palette sphere is culled"
        );
        assert_eq!(
            counts.iter().filter(|count| **count == 1).count(),
            meshes.len() - 1
        );
    }

    /// The first `words` u32s of `buffer` (a `COPY_SRC` buffer), after the queued work.
    fn read_words(device: &RenderDevice, buffer: &wgpu::Buffer, words: usize) -> Vec<u32> {
        let bytes = (words as u64 * 4).max(4);
        let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("ordered cull readback"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ordered cull readback"),
            });
        encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, bytes);
        device.queue.submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device
            .tracked()
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        rx.recv().expect("map callback").expect("map");
        let mapped = staging.slice(..).get_mapped_range().expect("mapped range");
        let out = mapped
            .as_chunks::<4>()
            .0
            .iter()
            .take(words)
            .map(|w| u32::from_le_bytes(*w))
            .collect();
        drop(mapped);
        staging.unmap();
        out
    }

    /// The order-fixed cull (ADR-1415): the CPU side writes each sphere's index inside its batch
    /// and the mode flag, and the cull leaves every object in its own slot (a culled one marked
    /// `0xffffffff`) with the batch's instance count at the highest visible index + 1. The packed
    /// mode keeps its old result: the visible objects, in some order, and their count.
    #[test]
    fn ordered_cull_keeps_submission_order_and_marks_culled_slots() {
        let device = crate::test_device();
        // The order-fixed pack is the default (determinism by default); the
        // atomicAdd pack is the explicit speed switch (`set_ordered(false)`).
        assert!(
            GpuCuller::default().ordered(),
            "order-fixed compaction is the default"
        );
        // One batch of six objects; objects 1 and 5 lie below the single plane (y >= 0).
        let mesh = |object: u64| CullMesh {
            is_static: false,
            mesh: 5,
            material: 2,
            object,
            center: [
                object as f32 * 0.01,
                if object == 1 || object == 5 { -5.0 } else { 0.0 },
                0.0,
            ],
            radius: 0.1,
            index_count: 3,
            first_index: 0,
            base_vertex: 0,
            double_sided: false,
            skinned: false,
        };
        // A second batch (another mesh) ahead of it in the sort, so the batch base is not 0.
        let other = |object: u64| CullMesh {
            mesh: 4,
            object,
            ..mesh(object)
        };
        let mut meshes: Vec<CullMesh> = (0..6u64).map(&mesh).collect();
        meshes.extend((100..103u64).map(other));
        let mut planes = [[0.0; 4]; 6];
        planes[0] = [0.0, 1.0, 0.0, 0.0];
        let mut ordered_readback: Option<(u32, Vec<u32>)> = None;
        for ordered in [false, true] {
            let mut culler = GpuCuller::default();
            culler.set_ordered(ordered);
            assert_eq!(culler.ordered(), ordered);
            culler
                .upload(
                    &device.device,
                    &mut &device.queue,
                    &meshes,
                    regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                    planes,
                    [0.0, 0.0, 1.0],
                    64,
                    64,
                    false,
                )
                .expect("upload");
            // CPU side: the flag after the instance count, each sphere's index in its batch.
            let flag = u32::from_le_bytes(culler.param_bytes[196..200].try_into().expect("4 bytes"));
            assert_eq!(flag, u32::from(ordered), "CullParams carries the mode");
            let locals: Vec<u32> = culler
                .sphere_bytes
                .as_chunks::<48>()
                .0
                .iter()
                .map(|sphere| u32::from_le_bytes([sphere[40], sphere[41], sphere[42], sphere[43]]))
                .collect();
            assert_eq!(locals, vec![0, 1, 2, 0, 1, 2, 3, 4, 5], "index inside each batch");
            let batch = *culler
                .batches()
                .iter()
                .find(|b| b.mesh == 5)
                .expect("the six-object batch");
            assert_eq!((batch.start, batch.count), (3, 6));
            let mut encoder = device
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                    label: Some("ordered cull"),
                });
            culler.dispatch(&device.device, &mut encoder);
            device.queue.submit([encoder.finish()]);
            let cmds = read_words(
                &device,
                culler.batch_commands().expect("batch commands"),
                culler.batches().len() * 5,
            );
            let count = cmds[batch.batch_id as usize * 5 + 1];
            let slots_buf = culler.slots.clone().expect("slots");
            let slots = read_words(&device, &slots_buf, 9);
            let own = &slots[3..9];
            // Object `k` of the batch sits in slot `3 + k` (its object slot is its sorted index).
            if ordered {
                assert_eq!(count, 5, "highest visible index (4) + 1");
                assert_eq!(
                    own,
                    &[3, u32::MAX, 5, 6, 7, u32::MAX],
                    "submission order, culled marked"
                );
                ordered_readback = Some((count, own.to_vec()));
            } else {
                assert_eq!(count, 4, "the four visible objects");
                let mut packed = own[..4].to_vec();
                packed.sort_unstable();
                assert_eq!(packed, vec![3, 5, 6, 7], "the visible objects, packed");
            }
        }
        // A fresh default culler packs the same submission into the same slots: the
        // readback is a function of the submission, not of GPU thread finish order.
        let (count, slots) = ordered_readback.expect("the ordered run");
        let mut again = GpuCuller::default();
        assert!(again.ordered(), "the default culler is order-fixed");
        again
            .upload(
                &device.device,
                &mut &device.queue,
                &meshes,
                regia_core::glam::Mat4::IDENTITY.to_cols_array_2d(),
                planes,
                [0.0, 0.0, 1.0],
                64,
                64,
                false,
            )
            .expect("upload");
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ordered cull, second run"),
            });
        again.dispatch(&device.device, &mut encoder);
        device.queue.submit([encoder.finish()]);
        let batch = *again
            .batches()
            .iter()
            .find(|b| b.mesh == 5)
            .expect("the six-object batch");
        let cmds = read_words(
            &device,
            again.batch_commands().expect("batch commands"),
            again.batches().len() * 5,
        );
        assert_eq!(
            cmds[batch.batch_id as usize * 5 + 1],
            count,
            "the instance count is stable across runs"
        );
        let again_slots = again.slots.clone().expect("slots");
        let own = &read_words(&device, &again_slots, 9)[3..9];
        assert_eq!(own, slots, "the slots are stable across runs");
    }
}
