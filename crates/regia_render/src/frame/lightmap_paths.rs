// SPDX-License-Identifier: MPL-2.0
//! The lightmap receivers' other draw paths (ADR-2107), beside the CPU colour loop of
//! [`super::lightmap_forward`]:
//!
//! - **GPU-driven**: a receiver joins the instanced batches. Its entry's `scale_offset` is read in
//!   the vertex stage from a table with one row per object slot (`lightmap_slots`, group 3 binding
//!   17), because `ObjectData`'s stride is shared with every other instanced draw. A batch with a
//!   receiver in it binds the `GPU_INSTANCED` + `LIGHTMAP` variant and the UV2 stream. Batches stay
//!   keyed as they were, so a non-receiver can share a receiver's batch: its row is zero and the
//!   shader's receiver rule keeps its probe light.
//! - **Visibility buffer**: a receiver is a cluster instance like any other. The resolve shades the
//!   whole frame with its `LIGHTMAP` variant when any instance is a receiver; a row per instance
//!   holds the entry's `scale_offset` and where the mesh's UV2 words are (a range of their own in
//!   the geometry buffer, so a mesh that went resident without UV2 needs no rebuild).
//! - **Mobile lite tiers**: the lite shader composed with `LIGHTMAP`, on the CPU path like every
//!   lite draw. One bilinear fetch at Low; one more for the direction at High.
//!
//! Nothing here runs without a receiver: frames without one keep their pipelines, bind groups and
//! allocations.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use naga_oil::compose::ShaderDefValue;
use regia_asset::lightmap::Lightmap;

use super::lightmap_forward::{LightmapForwardGpu, color_pipeline};
use super::{Draw, FrameRenderer, PipelineKey, RenderDevice, RenderError, shader_module};
use crate::forward_lite::{FORWARD_LITE_SHADER_FILE, ForwardLite, lite_defs};
use crate::pipeline_warm::DeferredPipeline;
use crate::visbuffer::VisBufferPass;

/// Group 3 binding of the per-slot or per-instance table (`lightmap_slots`, `vis_lightmap`).
pub(super) const TABLE_BINDING: u32 = 17;

/// One visibility instance's row (`VisLightmap` in `vis_resolve.wgsl`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct VisRow {
    scale_offset: [f32; 4],
    uv2_base: u32,
    vertex_base: u32,
    /// Source vertices the UV2 chart covers (`uv2_count` in `vis_resolve.wgsl`).
    uv2_count: u32,
    pad: u32,
}

/// The lite variant of one target configuration and tier.
#[derive(Clone)]
pub(super) struct LiteSet {
    /// Opaque draws: no `discard`, so a tile GPU keeps its hidden-surface removal.
    pub(super) opaque: DeferredPipeline<wgpu::RenderPipeline>,
    /// Masked draws.
    pub(super) tested: DeferredPipeline<wgpu::RenderPipeline>,
}

/// The instanced variant of one target configuration.
#[derive(Clone)]
pub(super) struct InstancedSet {
    /// Opaque batches: no `discard`, like the plain instanced opaque pipelines.
    pub(super) opaque: DeferredPipeline<wgpu::RenderPipeline>,
    /// Masked batches.
    pub(super) mask: DeferredPipeline<wgpu::RenderPipeline>,
}

type ModuleCell = Arc<OnceLock<wgpu::ShaderModule>>;

/// Layouts, modules and variants of these paths (in [`super::Pipes`], beside the CPU variants).
#[derive(Default)]
pub(super) struct PathPipes {
    /// Group 3 with the table in place of the draw block.
    table_bgl: Option<wgpu::BindGroupLayout>,
    instanced_layout: Option<wgpu::PipelineLayout>,
    instanced_modules: HashMap<(bool, bool, bool), ModuleCell>,
    instanced: HashMap<PipelineKey, InstancedSet>,
    lite_modules: HashMap<(ForwardLite, bool, bool), ModuleCell>,
    lite: HashMap<(PipelineKey, ForwardLite), LiteSet>,
    vis_layout: Option<wgpu::PipelineLayout>,
    vis_modules: HashMap<(bool, bool), ModuleCell>,
    vis: HashMap<(wgpu::TextureFormat, bool, bool), DeferredPipeline<wgpu::RenderPipeline>>,
}

fn storage_table_entry() -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding: TABLE_BINDING,
        visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn defs(names: &[&str], encode: bool) -> HashMap<String, ShaderDefValue> {
    let mut defs: HashMap<String, ShaderDefValue> = names
        .iter()
        .map(|name| ((*name).to_string(), ShaderDefValue::Bool(true)))
        .collect();
    defs.insert("LIGHTMAP".to_string(), ShaderDefValue::Bool(true));
    if !encode {
        defs.insert("skip_srgb_encode".to_string(), ShaderDefValue::Bool(true));
    }
    defs
}

impl PathPipes {
    /// Group 3 with the table (created on first use).
    pub(super) fn table_bgl(&mut self, d: &wgpu::Device) -> wgpu::BindGroupLayout {
        self.table_bgl
            .get_or_insert_with(|| {
                d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                    label: Some("lab lightmap table bgl"),
                    entries: &super::lightmap_forward::shared_entries(storage_table_entry()),
                })
            })
            .clone()
    }

    /// The instanced variant for `key`, registered for a background build.
    pub(super) fn instanced_set(
        &mut self,
        d: &wgpu::Device,
        groups: [&wgpu::BindGroupLayout; 3],
        key: PipelineKey,
    ) -> InstancedSet {
        // This family keeps the full forward module under either studio key (ADR-2308).
        let key = key.full_studio();
        if let Some(set) = self.instanced.get(&key) {
            return set.clone();
        }
        let bgl = self.table_bgl(d);
        let layout = self
            .instanced_layout
            .get_or_insert_with(|| {
                let table = crate::texture_table::table_layout(d);
                d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("lab lightmap instanced layout"),
                    bind_group_layouts: &crate::texture_table::with_table(
                        &[Some(groups[0]), Some(groups[1]), Some(groups[2]), Some(&bgl)],
                        table.as_ref(),
                    ),
                    immediate_size: 0,
                })
            })
            .clone();
        let build = |alpha_test: bool, this: &mut Self| {
            let module = this
                .instanced_modules
                .entry((alpha_test, key.encode, key.sun_rt))
                .or_default()
                .clone();
            let device = wgpu::Device::clone(d);
            let layout = layout.clone();
            DeferredPipeline::new("lab lightmap instanced", move || {
                let module =
                    module.get_or_init(|| compile_instanced(&device, alpha_test, key.encode, key.sun_rt));
                color_pipeline(&device, &layout, module, module, key)
            })
        };
        let set = InstancedSet {
            opaque: build(false, self),
            mask: build(true, self),
        };
        self.instanced.insert(key, set.clone());
        set
    }

    /// The lite variant for `key` at `tier`, registered for a background build. `layout` and
    /// `vertex` are the CPU variant's (group 3 with the draw block, the lab `LIGHTMAP` vertex stage).
    pub(super) fn lite_set(
        &mut self,
        d: &wgpu::Device,
        layout: &wgpu::PipelineLayout,
        vertex: &ModuleCell,
        key: PipelineKey,
        tier: ForwardLite,
    ) -> LiteSet {
        // This family keeps the full forward module under either studio key (ADR-2308).
        let key = key.full_studio();
        if let Some(set) = self.lite.get(&(key, tier)) {
            return set.clone();
        }
        let build = |alpha_test: bool, this: &mut Self| {
            let module = this
                .lite_modules
                .entry((tier, alpha_test, key.encode))
                .or_default()
                .clone();
            let device = wgpu::Device::clone(d);
            let layout = layout.clone();
            let vertex = vertex.clone();
            DeferredPipeline::new("lab lightmap lite", move || {
                let fragment = module.get_or_init(|| compile_lite(&device, tier, alpha_test, key.encode));
                let vertex =
                    vertex.get_or_init(|| super::lightmap_forward::compile(&device, key.encode, key.sun_rt));
                color_pipeline(&device, &layout, vertex, fragment, key)
            })
        };
        let set = LiteSet {
            opaque: build(false, self),
            tested: build(true, self),
        };
        self.lite.insert((key, tier), set.clone());
        set
    }

    /// The visibility resolve's `LIGHTMAP` shade pipeline, registered for a background build.
    /// `groups`: the view, resolve and material layouts.
    pub(super) fn vis_pipeline(
        &mut self,
        d: &wgpu::Device,
        groups: [&wgpu::BindGroupLayout; 3],
        format: wgpu::TextureFormat,
        encode: bool,
        sun_rt: bool,
    ) -> DeferredPipeline<wgpu::RenderPipeline> {
        if let Some(pipe) = self.vis.get(&(format, encode, sun_rt)) {
            return pipe.clone();
        }
        let bgl = self.table_bgl(d);
        let layout = self
            .vis_layout
            .get_or_insert_with(|| {
                let table = crate::texture_table::table_layout(d);
                d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                    label: Some("vis resolve lightmap"),
                    bind_group_layouts: &crate::texture_table::with_table(
                        &[Some(groups[0]), Some(groups[1]), Some(groups[2]), Some(&bgl)],
                        table.as_ref(),
                    ),
                    immediate_size: 0,
                })
            })
            .clone();
        let module = self.vis_modules.entry((encode, sun_rt)).or_default().clone();
        let device = wgpu::Device::clone(d);
        let pipe = DeferredPipeline::new("vis resolve lightmap", move || {
            let module = module.get_or_init(|| compile_vis(&device, encode, sun_rt));
            VisBufferPass::shade_pipeline(&device, &layout, module, "vis resolve lightmap", format)
        });
        self.vis.insert((format, encode, sun_rt), pipe.clone());
        pipe
    }
}

// The engine's own shaders with fixed define sets: `shader_check` validates these combinations, so
// a failure here is a build defect, as for the other forward variants.

fn compile_instanced(d: &wgpu::Device, alpha_test: bool, encode: bool, sun_rt: bool) -> wgpu::ShaderModule {
    let mut names = vec!["GPU_DRIVEN", "GPU_INSTANCED"];
    if alpha_test {
        names.push("ALPHA_TEST");
    }
    let mut defs = defs(&names, encode);
    super::add_sun_rt_def(&mut defs, sun_rt);
    shader_module(
        d,
        "lab_forward_lightmap_instanced",
        include_str!("../../shaders/lab_forward.wgsl"),
        "lab_forward.wgsl",
        defs,
    )
    .expect("instanced LIGHTMAP forward variant")
}

fn compile_lite(d: &wgpu::Device, tier: ForwardLite, alpha_test: bool, encode: bool) -> wgpu::ShaderModule {
    // The dry variant: a lightmap receiver on a lite tier takes no surface weather (ADR-2106).
    let mut defs = lite_defs(tier, alpha_test, encode, false);
    defs.insert("LIGHTMAP".to_string(), ShaderDefValue::Bool(true));
    shader_module(
        d,
        "lite forward lightmap",
        include_str!("../../shaders/forward_mobile.wgsl"),
        FORWARD_LITE_SHADER_FILE,
        defs,
    )
    .expect("lite LIGHTMAP forward variant")
}

fn compile_vis(d: &wgpu::Device, encode: bool, sun_rt: bool) -> wgpu::ShaderModule {
    let mut defs = defs(&["VISBUFFER"], encode);
    super::add_sun_rt_def(&mut defs, sun_rt);
    shader_module(
        d,
        "vis resolve lightmap",
        crate::visbuffer::gpu::resolve_source(),
        "vis_resolve.wgsl",
        defs,
    )
    .expect("visibility resolve LIGHTMAP variant")
}

/// One table: its CPU rows, what was last written, the buffer and group 3 over it.
struct Table<T> {
    rows: Vec<T>,
    written: Vec<T>,
    buffer: Option<wgpu::Buffer>,
    /// Rows the buffer holds.
    capacity: usize,
    bg: Option<wgpu::BindGroup>,
}

impl<T> Default for Table<T> {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            written: Vec::new(),
            buffer: None,
            capacity: 0,
            bg: None,
        }
    }
}

impl<T: bytemuck::Pod + PartialEq> Table<T> {
    /// A buffer of at least `rows` rows and group 3 over it (doubling: a steady view creates
    /// nothing; [`FrameRenderer::prewarm_lightmap`] reserves a level's size).
    fn reserve(
        &mut self,
        d: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        gpu: &LightmapForwardGpu,
        rows: usize,
        label: &'static str,
    ) {
        if self.buffer.is_none() || rows > self.capacity {
            let capacity = rows.max(self.capacity.saturating_mul(2)).max(16);
            let device = crate::device::TrackedDevice::observe(d);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(label),
                size: (capacity * size_of::<T>()) as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.capacity = capacity;
            self.bg = None;
            self.written.clear();
        }
        if self.bg.is_none()
            && let Some(buffer) = self.buffer.as_ref()
        {
            self.bg = Some(gpu.table_group(d, bgl, buffer));
        }
    }

    /// Writes the rows when they changed since the last write.
    fn upload(&mut self, q: &wgpu::Queue) {
        if self.rows == self.written {
            return;
        }
        if let Some(buffer) = self.buffer.as_ref() {
            q.write_buffer(buffer, 0, bytemuck::cast_slice(&self.rows));
            self.written.clone_from(&self.rows);
        }
    }

    fn forget_group(&mut self) {
        self.bg = None;
    }
}

/// Per-renderer state of these paths (in the renderer's lightmap state).
#[derive(Default)]
pub(super) struct PathState {
    slots: Table<[f32; 4]>,
    vis: Table<VisRow>,
    /// `batch_id` → the batch has a receiver. The frame holds a clone for the forward pass; the
    /// next frame finds the only reference again and reuses the allocation.
    lit_batches: Arc<Vec<bool>>,
}

impl PathState {
    /// The GPU copy of the lightmap changed: the tables' groups point at the old one.
    pub(super) fn forget_groups(&mut self) {
        self.slots.forget_group();
        self.vis.forget_group();
    }

    /// Frame warms: the tables for a level's sizes and their groups.
    pub(super) fn reserve(
        &mut self,
        d: &wgpu::Device,
        bgl: &wgpu::BindGroupLayout,
        gpu: &LightmapForwardGpu,
        slots: usize,
        vis_rows: usize,
    ) {
        self.slots.reserve(d, bgl, gpu, slots, "lightmap slot rows");
        self.vis
            .reserve(d, bgl, gpu, vis_rows, "lightmap visibility rows");
    }
}

/// The frame's state of these paths, for the forward pass and the visibility resolve.
#[derive(Default)]
pub(super) struct PathFrame {
    pub(super) lite: [Option<LiteSet>; 2],
    pub(super) instanced: [Option<InstancedSet>; 2],
    table_bg: Option<wgpu::BindGroup>,
    lit_batches: Option<Arc<Vec<bool>>>,
    vis: Option<(wgpu::RenderPipeline, wgpu::BindGroup)>,
}

impl PathFrame {
    /// The pipeline and group 3 of instanced batch `batch_id`, when the batch has a receiver.
    pub(super) fn instanced_batch(
        &self,
        batch_id: u32,
        double_sided: bool,
        mask: bool,
    ) -> Option<(&wgpu::RenderPipeline, &wgpu::BindGroup)> {
        if !*self.lit_batches.as_ref()?.get(batch_id as usize)? {
            return None;
        }
        let set = self.instanced[usize::from(double_sided)].as_ref()?;
        let pipe = if mask {
            set.mask.try_get()?
        } else {
            set.opaque.try_get()?
        };
        Some((pipe, self.table_bg.as_ref()?))
    }

    /// The visibility resolve's pipeline and group 3, when the frame has a receiver there.
    pub(super) fn vis_resolve(&self) -> Option<(&wgpu::RenderPipeline, &wgpu::BindGroup)> {
        self.vis.as_ref().map(|(pipe, bg)| (pipe, bg))
    }
}

/// What [`prepare_vis_table`] reads.
pub(super) struct VisTableInputs<'a> {
    pub(super) d: &'a wgpu::Device,
    pub(super) q: &'a wgpu::Queue,
    pub(super) visbuffer: &'a VisBufferPass,
    pub(super) table_bgl: &'a wgpu::BindGroupLayout,
    pub(super) gpu: &'a LightmapForwardGpu,
    pub(super) map: &'a Lightmap,
}

/// Fills and uploads the per-instance table of the frame's visibility receivers.
pub(super) fn prepare_vis_table(
    state: &mut PathState,
    inputs: &VisTableInputs<'_>,
    pipeline: &wgpu::RenderPipeline,
    draws: &[Draw],
    frame: &mut PathFrame,
) {
    let _span = tracing::info_span!("lightmap_vis_table").entered();
    let VisTableInputs {
        d,
        q,
        visbuffer,
        table_bgl,
        gpu,
        map,
    } = *inputs;
    let table = &mut state.vis;
    table.rows.clear();
    table
        .rows
        .resize(visbuffer.instance_count(), bytemuck::Zeroable::zeroed());
    let base = visbuffer.persistent_count();
    for draw in draws {
        let (Some(index), Some(entry)) = (draw.vis_instance, draw.lightmap) else {
            continue;
        };
        let Some(uv2) = visbuffer.uv2_of(draw.mesh) else {
            continue;
        };
        let Some(entry) = map.entries.get(entry as usize) else {
            continue;
        };
        if let Some(row) = table.rows.get_mut(base + index) {
            *row = VisRow {
                scale_offset: entry.packed_scale_offset(),
                uv2_base: uv2.uv2_base,
                vertex_base: uv2.vertex_base,
                uv2_count: uv2.uv2_vertices,
                pad: 0,
            };
        }
    }
    let rows = table.rows.len();
    table.reserve(d, table_bgl, gpu, rows, "lightmap visibility rows");
    table.upload(q);
    if let Some(bg) = table.bg.as_ref() {
        frame.vis = Some((pipeline.clone(), bg.clone()));
    }
}

/// Fills and uploads the per-slot table of the frame's instanced receivers and marks their batches
/// (after the cull has given each draw its slot). `slots`: the object slots the culler holds.
pub(super) fn prepare_instanced_table(
    state: &mut PathState,
    (d, q): (&wgpu::Device, &wgpu::Queue),
    (table_bgl, gpu, map): (&wgpu::BindGroupLayout, &LightmapForwardGpu, &Lightmap),
    (draws, batches): (&[Draw], &[crate::gpu_driven::GpuBatch]),
    frame: &mut PathFrame,
) {
    let _span = tracing::info_span!("lightmap_slot_table").entered();
    let slots = batches
        .iter()
        .map(|batch| (batch.start + batch.count) as usize)
        .max()
        .unwrap_or(0);
    let table = &mut state.slots;
    table.rows.clear();
    table.rows.resize(slots, [0.0; 4]);
    if Arc::get_mut(&mut state.lit_batches).is_none() {
        state.lit_batches = Arc::new(Vec::new());
    }
    let lit = Arc::make_mut(&mut state.lit_batches);
    lit.clear();
    let ids = batches
        .iter()
        .map(|batch| batch.batch_id as usize + 1)
        .max()
        .unwrap_or(0);
    lit.resize(ids, false);
    for draw in draws {
        let (Some(slot), Some(entry)) = (draw.indirect, draw.lightmap) else {
            continue;
        };
        let (Some(entry), Some(row)) = (map.entries.get(entry as usize), table.rows.get_mut(slot as usize))
        else {
            continue;
        };
        *row = entry.packed_scale_offset();
        // Batches are contiguous and in command order.
        let at = batches.partition_point(|batch| batch.start + batch.count <= slot);
        if let Some(batch) = batches.get(at)
            && let Some(flag) = lit.get_mut(batch.batch_id as usize)
        {
            *flag = true;
        }
    }
    table.reserve(d, table_bgl, gpu, slots, "lightmap slot rows");
    table.upload(q);
    frame.table_bg = table.bg.clone();
    frame.lit_batches = Some(state.lit_batches.clone());
}

impl FrameRenderer {
    /// Frame warms build the lightmap variants even while no lightmap is installed: a host that
    /// installs one after the warm has started sets this at level load, so the first receiver does
    /// not keep the probe light while a variant compiles. Off by default, and frames are unchanged.
    /// Kept until changed (moved over from a stand-in like [`Self::set_level_material_graphs`]).
    pub fn set_warm_lightmaps(&mut self, enabled: bool) {
        self.lightmap_forward.set_warm(enabled);
        self.installed_since.lightmap = true;
    }

    /// The [`Self::set_warm_lightmaps`] flag (a level that names a lightmap sets it when it
    /// opens, ADR-2313). An installed lightmap warms its variants whatever this says.
    #[must_use]
    pub fn warm_lightmaps(&self) -> bool {
        self.lightmap_forward.warm_asked()
    }

    /// Frame warms: the `LIGHTMAP` variants of every forward target the warm frame built, on every
    /// path this renderer can draw receivers through, the lightmap's GPU copy, and the tables for
    /// the reserved instance count, so a receiver drawn mid-play compiles and creates nothing
    /// (ADR-2107). Nothing runs without an installed lightmap or [`Self::set_warm_lightmaps`], so
    /// a project without lightmaps warms what it warmed before. Returns the pipelines asked for.
    ///
    /// # Errors
    ///
    /// The warm queue running this shut down ([`crate::warm_cancelled`]).
    pub(crate) fn prewarm_lightmap(&mut self, device: &RenderDevice) -> Result<usize, RenderError> {
        if !self.lightmap_forward.warms() {
            return Ok(0);
        }
        let _span = tracing::info_span!("prewarm_lightmap").entered();
        let d = &device.device;
        let instanced = crate::gpu_driven::IndirectStrategy::select(&device.capabilities).uses_gpu_objects();
        let objects = if instanced {
            Some(self.culler.instanced_object_layout(d)?)
        } else {
            None
        };
        let vis_group = if self.visbuffer.has_sources() && VisBufferPass::lightmap_supported(d) {
            Some(self.visbuffer.resolve_layout_group(d)?)
        } else {
            None
        };
        let tier = self.lightmap_forward.forward_tier();
        let Some(pipes) = self.pipes.as_mut() else {
            return Ok(0);
        };
        let keys: Vec<PipelineKey> = pipes.sets.keys().copied().collect();
        let mut need = Vec::new();
        for key in keys {
            let groups = [&pipes.view_bgl, &pipes.object_bgl, &pipes.mat_bgl];
            need.push(pipes.lightmap.set(d, groups, key).opaque);
            // A multisampled frame does not draw GPU-driven.
            if let Some(objects) = objects.as_ref().filter(|_| key.samples == 1) {
                let groups = [&pipes.view_bgl, objects, &pipes.mat_bgl];
                let set = pipes.lightmap.paths.instanced_set(d, groups, key);
                need.extend([set.opaque, set.mask]);
            }
            if tier != ForwardLite::Off {
                let groups = [&pipes.view_bgl, &pipes.object_bgl, &pipes.mat_bgl];
                let set = pipes.lightmap.lite_set(d, groups, key, tier);
                need.extend([set.opaque, set.tested]);
            }
            if let Some(resolve) = vis_group.as_ref()
                && key.samples == 1
            {
                let groups = [&pipes.view_bgl, resolve, &pipes.mat_bgl];
                need.push(
                    pipes
                        .lightmap
                        .paths
                        .vis_pipeline(d, groups, key.format, key.encode, key.sun_rt),
                );
            }
        }
        let asked = need.len();
        for pipe in need {
            crate::pipeline_warm::warm_checkpoint()?;
            pipe.get();
        }
        let reserved = self.culler.reserved() as usize;
        let vis_rows = self.visbuffer.instance_count().max(reserved);
        self.lightmap_forward
            .warm_gpu(device, d, &mut pipes.lightmap, reserved, vis_rows);
        Ok(asked)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The row struct is the shader's: sixteen bytes of transform, then two words and padding.
    #[test]
    fn a_visibility_row_has_the_shader_s_layout() {
        assert_eq!(size_of::<VisRow>(), 32);
        const SOURCE: &str = include_str!("../visbuffer/vis_resolve.wgsl");
        let start = SOURCE.find("struct VisLightmap {").expect("the shader's row");
        let fields: Vec<&str> = SOURCE[start..]
            .lines()
            .skip(1)
            .take_while(|line| !line.starts_with('}'))
            .map(str::trim)
            .collect();
        assert_eq!(
            fields,
            [
                "scale_offset: vec4<f32>,",
                "uv2_base: u32,",
                "vertex_base: u32,",
                "uv2_count: u32,",
                "pad1: u32,",
            ]
        );
    }
}
