// SPDX-License-Identifier: MPL-2.0
//! Two-phase occlusion culling of the shadow casters (ADR-2072, `docs/specs/98-visibility-buffer.md`
//! § 10a; Haar & Aaltonen 2015, applied to light views).
//!
//! The forward depth prepass already culls in two phases (`crate::prepass_occlusion`). With the
//! switch on ([`crate::FrameRenderer::set_shadow_occlusion`]) the GPU-driven shadow casters of a
//! redrawn sun cascade or of the local-light atlas are culled the same way, per shadow-map kind:
//!
//! 1. **Phase 1** (compute): every caster instance of the kind's jobs takes the conservative
//!    sphere test against the kind's coarse farthest-depth pyramid as the kind's last redraw left
//!    it, projected with the matrix that redraw used. Survivors draw in the shadow pass, with the
//!    page clears, the visibility-buffer clusters and the shared (terrain) draw.
//! 2. The pyramid is rebuilt from that depth.
//! 3. **Phase 2** (compute): what phase 1 skipped is tested against the new pyramid with this
//!    frame's matrix, and the survivors draw in a second pass over the same map (loaded), each in
//!    its job's viewport and scissor.
//!
//! Phase 2's pyramid holds a subset of the depth the unculled pass writes, so it is never nearer:
//! what it hides, the full pass hides too, and `Less` depth writes keep the nearest value
//! whatever the order. Outside a sun page the map keeps what it held, which is the final depth
//! there, and the page's scissor drops a caster's texels outside the page. So the map, and every
//! pixel shaded from it, is the unculled map's (`tests/shadow_occlusion.rs` compares both).
//!
//! The pyramids are coarse (one base texel per 8×8 map texels, rounded up to a power of two):
//! four 4096² cascades and an 8192² atlas take about 11 MB with every mip, against 170 MB for
//! full-resolution pyramids. Retained cascades draw nothing and are not culled; a kind with
//! fewer caster instances than [`crate::FrameRenderer::set_shadow_occlusion_min_casters`] draws
//! them directly. Only the main view culls (texture views draw directly). Nothing is allocated
//! while the switch is off.

use crate::RenderError;
use crate::frame_work::counted::*;

/// Process-wide counts of what the shadow casters' cull recorded (ADR-2482): what a culled frame
/// adds to the GPU work, for the counter tests and `book/agents/feature-costs.md`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowOcclusionWork {
    /// Compute passes: one for every culled kind's phase 1, one per kind for its pyramid and
    /// phase 2.
    pub compute_passes: u64,
    /// Of those, pyramid passes.
    pub pyramid_passes: u64,
    /// Phase 1 cull dispatches (one per culled kind).
    pub phase1_dispatches: u64,
    /// Phase 2 cull dispatches (one per culled kind with history).
    pub phase2_dispatches: u64,
}

static WORK: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Counts one recorded item: 0 a phase 1 pass, 1 a pyramid pass, 2 / 3 a phase 1 / 2 dispatch.
fn note_pass(what: usize) {
    WORK[what].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// [`ShadowOcclusionWork`] since the process started (take differences).
#[must_use]
pub fn shadow_occlusion_work() -> ShadowOcclusionWork {
    let at = |i: usize| WORK[i].load(std::sync::atomic::Ordering::Relaxed);
    ShadowOcclusionWork {
        compute_passes: at(0) + at(1),
        pyramid_passes: at(1),
        phase1_dispatches: at(2),
        phase2_dispatches: at(3),
    }
}

/// Map texels per pyramid base texel, along each axis.
pub(crate) const PYRAMID_BLOCK: u32 = 8;

/// Shadow-map kinds: sun cascades 0..4, then the local-light atlas.
pub const SHADOW_OCCLUSION_KINDS: usize = 5;

/// Default fewest caster instances in one kind's jobs for that kind to cull in two phases.
pub const SHADOW_OCCLUSION_MIN_CASTERS: u32 = 512;

/// Stable protocol name of shadow-map kind `kind` (`cascade0` … `cascade3`, `atlas`).
#[must_use]
pub fn shadow_kind_name(kind: usize) -> &'static str {
    match kind {
        0 => "cascade0",
        1 => "cascade1",
        2 => "cascade2",
        3 => "cascade3",
        _ => "atlas",
    }
}

/// What one shadow-map kind's two-phase cull did in the last main-view frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowKindOcclusion {
    /// The kind was redrawn and culled in two phases.
    pub active: bool,
    /// Shadow jobs of the kind (a whole cascade, its dirty pages, or atlas tiles).
    pub jobs: u32,
    /// Of those, jobs whose phase 1 tested last redraw's pyramid (the rest draw every caster in
    /// phase 1).
    pub history_jobs: u32,
    /// Caster instances tested.
    pub instances: u32,
    /// Of those, instances with no bound (vertex-stage graphs, morphs, compute-skinned twins):
    /// always drawn in phase 1.
    pub unbounded: u32,
}

/// What the shadow casters' two-phase occlusion did in the last main-view frame
/// ([`crate::FrameRenderer::shadow_occlusion_stats`]). CPU-side: the per-phase GPU counts are
/// [`crate::FrameRenderer::shadow_occlusion_counters`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowOcclusionStats {
    /// The switch is on ([`crate::FrameRenderer::set_shadow_occlusion`]).
    pub enabled: bool,
    /// At least one kind culled its casters in two phases in the last main-view frame.
    pub active: bool,
    /// Why the last main-view frame culled no kind, when it did not.
    pub skipped: Option<ShadowOcclusionSkip>,
    /// Per kind: cascades 0..4, then the atlas ([`shadow_kind_name`]).
    pub kinds: [ShadowKindOcclusion; SHADOW_OCCLUSION_KINDS],
    /// Fewest instances a kind needs ([`crate::FrameRenderer::set_shadow_occlusion_min_casters`]).
    pub min_casters: u32,
    /// GPU bytes of the pyramids held now.
    pub pyramid_bytes: u64,
    /// Frames that culled at least one kind since the renderer was made.
    pub frames: u64,
}

/// Why a frame's shadow casters were not culled in two phases ([`ShadowOcclusionStats::skipped`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShadowOcclusionSkip {
    /// The switch is off.
    Off,
    /// The shadow casters are not GPU-driven: no storage-object instancing on this device or
    /// frame (the CPU draw path).
    NoGpuObjects,
    /// No shadow map is redrawn this frame (every cascade and tile kept its depth).
    NoJobs,
    /// Every redrawn kind has fewer caster instances than the threshold.
    FewCasters,
    /// The cull's resources could not be made this frame (logged); the casters drew directly.
    Failed,
}

impl ShadowOcclusionSkip {
    /// Stable lowercase name (agent protocol).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::NoGpuObjects => "no_gpu_objects",
            Self::NoJobs => "no_jobs",
            Self::FewCasters => "few_casters",
            Self::Failed => "failed",
        }
    }
}

/// Per-phase draws of one kind ([`ShadowOcclusionCounters::kinds`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowPhaseCounts {
    /// Instances drawn in phase 1.
    pub phase1: u32,
    /// Instances drawn in phase 2 (skipped by phase 1, visible against this frame's pyramid).
    pub phase2: u32,
    /// Instances drawn in neither phase.
    pub culled: u32,
}

/// GPU counts of the last frame's two-phase shadow cull
/// ([`crate::FrameRenderer::shadow_occlusion_counters`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ShadowOcclusionCounters {
    /// Sum over the kinds.
    pub total: ShadowPhaseCounts,
    /// Per kind: cascades 0..4, then the atlas.
    pub kinds: [ShadowPhaseCounts; SHADOW_OCCLUSION_KINDS],
}

/// One caster instance as the cull reads it (`shadow_occlusion.wgsl` `Entry`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct ShadowCullEntry {
    /// World centre and radius; a negative radius has no bound (always drawn in phase 1).
    pub center_radius: [f32; 4],
    /// Index count of the mesh.
    pub index_count: u32,
    /// First index of the mesh in its index buffer.
    pub first_index: u32,
    /// The command's `base_vertex` bits (an `i32`).
    pub base_vertex: u32,
    /// The entry's shadow job (index into the frame's jobs).
    pub job: u32,
    /// Command slot: the instance's slot in the shadow object buffer.
    pub slot: u32,
    /// Padding to the shader's 16-byte stride.
    pub pad: [u32; 3],
}

/// One shadow job as the cull reads it (`shadow_occlusion.wgsl` `Job`).
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct ShadowCullJob {
    /// This frame's light view-projection.
    pub view_proj: [[f32; 4]; 4],
    /// The matrix the kind's pyramid was built with (with `flags[0]` 1).
    pub prev_view_proj: [[f32; 4]; 4],
    /// Viewport in map texels: x, y, width, height.
    pub viewport: [f32; 4],
    /// Scissor in map texels: x0, y0, x1, y1 (exclusive).
    pub clip: [f32; 4],
    /// `[history, 0, 0, 0]`.
    pub flags: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
struct KindParams {
    entry_offset: u32,
    entry_count: u32,
    block: u32,
    levels: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ReduceParams {
    src_size: [u32; 2],
    block: u32,
    pad: u32,
}

/// Bytes of one `DrawIndexedIndirect` command.
pub(crate) const CMD_STRIDE: u64 = 20;

/// Base edge of the pyramid of a `map_size`² map: one texel per [`PYRAMID_BLOCK`]² map texels,
/// rounded up to a power of two (so every level halves exactly).
pub(crate) fn pyramid_base(map_size: u32) -> u32 {
    map_size.max(1).div_ceil(PYRAMID_BLOCK).next_power_of_two()
}

/// Mip levels of a pyramid with base edge `base`.
pub(crate) fn pyramid_levels(base: u32) -> u32 {
    base.max(1).ilog2() + 1
}

/// GPU bytes of a full-chain `R32Float` pyramid with base edge `base`.
pub(crate) fn pyramid_bytes(base: u32) -> u64 {
    (0..pyramid_levels(base))
        .map(|level| {
            let edge = u64::from((base >> level).max(1));
            edge * edge * 4
        })
        .sum()
}

struct Pipes {
    cull_layout: wgpu::BindGroupLayout,
    phase1: wgpu::ComputePipeline,
    phase2: wgpu::ComputePipeline,
    base_layout: wgpu::BindGroupLayout,
    base: wgpu::ComputePipeline,
    down_layout: wgpu::BindGroupLayout,
    down: wgpu::ComputePipeline,
}

/// One kind's pyramid and the groups that build it from its map.
struct Pyramid {
    /// The map it reduces.
    source: wgpu::TextureView,
    map_size: u32,
    base: u32,
    levels: u32,
    /// Every level (the cull's binding).
    full: wgpu::TextureView,
    base_group: wgpu::BindGroup,
    /// Level `i + 1` from level `i`.
    down_groups: Vec<wgpu::BindGroup>,
    /// The base pass's [`ReduceParams`] (kept alive with its group).
    _params: wgpu::Buffer,
}

/// What a kind's pyramid holds: the map and, per job rectangle, the matrix it was drawn with.
struct KindHistory {
    source: wgpu::TextureView,
    map_size: u32,
    rects: Vec<([u32; 4], [[f32; 4]; 4])>,
}

type CullKey = (wgpu::Buffer, wgpu::Buffer, wgpu::Buffer, wgpu::TextureView);

/// The shadow casters' two-phase cull: pipelines, pyramids, the entry and job lists and both
/// phases' commands.
#[derive(Default)]
pub(crate) struct ShadowOcclusion {
    /// [`crate::FrameRenderer::set_shadow_occlusion`].
    pub(crate) enabled: bool,
    /// [`crate::FrameRenderer::set_shadow_occlusion_min_casters`]; `None` = the default.
    pub(crate) min_casters: Option<u32>,
    pipes: Option<Pipes>,
    entries: Option<wgpu::Buffer>,
    entry_cap: u32,
    jobs: Option<wgpu::Buffer>,
    job_cap: u32,
    phase1: Option<wgpu::Buffer>,
    phase2: Option<wgpu::Buffer>,
    cmd_cap: u32,
    kind_params: Vec<wgpu::Buffer>,
    kind_param_cache: [Option<KindParams>; SHADOW_OCCLUSION_KINDS],
    pyramids: [Option<Pyramid>; SHADOW_OCCLUSION_KINDS],
    cull_groups: [Option<(CullKey, wgpu::BindGroup)>; SHADOW_OCCLUSION_KINDS],
    history: [Option<KindHistory>; SHADOW_OCCLUSION_KINDS],
    /// This frame's cull list (kinds in order), kept so a culled frame allocates nothing.
    pub(crate) entries_scratch: Vec<ShadowCullEntry>,
    /// This frame's jobs, one per shadow job.
    pub(crate) jobs_scratch: Vec<ShadowCullJob>,
    /// Sort keys of one job's casters (the instance lists' grouping).
    pub(crate) sort_scratch: Vec<(bool, u64, u64, u32)>,
    entry_bytes: Vec<u8>,
    job_bytes: Vec<u8>,
    /// This frame's `(entry offset, entry count)` per kind; count 0 = the kind draws directly.
    ranges: [(u32, u32); SHADOW_OCCLUSION_KINDS],
    /// Highest command slot + 1 written this frame.
    slots: u32,
    phase2_ran: [bool; SHADOW_OCCLUSION_KINDS],
    /// This frame's phase 1 is recorded: the first [`Self::record_phase1`] records every culled
    /// kind's in one compute pass, the later calls nothing (ADR-2482).
    phase1_recorded: bool,
    /// The frame being recorded culls ([`Self::upload`] sets it; a skip or a texture view clears
    /// it, so a texture view never draws the main view's commands).
    frame_active: bool,
    pub(crate) stats: ShadowOcclusionStats,
}

impl ShadowOcclusion {
    /// The threshold in effect.
    pub(crate) fn min_casters(&self) -> u32 {
        self.min_casters.unwrap_or(SHADOW_OCCLUSION_MIN_CASTERS)
    }

    /// Turns the cull on or off; off drops every GPU resource it held.
    pub(crate) fn set_enabled(&mut self, enabled: bool) {
        if enabled == self.enabled {
            return;
        }
        if enabled {
            self.enabled = true;
        } else {
            let min_casters = self.min_casters;
            let frames = self.stats.frames;
            *self = Self {
                min_casters,
                ..Self::default()
            };
            self.stats.frames = frames;
        }
        self.stats.enabled = enabled;
    }

    /// Records a frame that culled no kind.
    pub(crate) fn skip(&mut self, why: ShadowOcclusionSkip) {
        self.stats.enabled = self.enabled;
        self.stats.active = false;
        self.stats.skipped = Some(why);
        self.stats.kinds = [ShadowKindOcclusion::default(); SHADOW_OCCLUSION_KINDS];
        self.stats.min_casters = self.min_casters();
        self.stats.pyramid_bytes = self.held_pyramid_bytes();
        self.ranges = [(0, 0); SHADOW_OCCLUSION_KINDS];
        self.phase2_ran = [false; SHADOW_OCCLUSION_KINDS];
        self.frame_active = false;
    }

    /// GPU bytes of the pyramids held now.
    pub(crate) fn held_pyramid_bytes(&self) -> u64 {
        self.pyramids
            .iter()
            .flatten()
            .map(|p| pyramid_bytes(p.base))
            .sum()
    }

    /// Whether `kind` culls in two phases in the frame being recorded.
    pub(crate) fn kind_active(&self, kind: usize) -> bool {
        self.frame_active && self.ranges.get(kind).is_some_and(|(_, count)| *count > 0)
    }

    /// A frame that culls nothing and must not touch the main view's report (a texture view).
    pub(crate) fn pass_through(&mut self) {
        self.frame_active = false;
    }

    /// The matrix `kind`'s pyramid was built with over `rect` of `source`, when it was built from
    /// that map at this size. Cascades keep one matrix for the whole map (`rect` is ignored).
    pub(crate) fn history_for(
        &self,
        kind: usize,
        source: &wgpu::TextureView,
        map_size: u32,
        rect: [u32; 4],
    ) -> Option<[[f32; 4]; 4]> {
        let history = self.history.get(kind)?.as_ref()?;
        if history.source != *source || history.map_size != map_size {
            return None;
        }
        if kind < 4 {
            return history.rects.first().map(|(_, vp)| *vp);
        }
        history.rects.iter().find(|(r, _)| *r == rect).map(|(_, vp)| *vp)
    }

    /// Drops `kind`'s pyramid, history and cull group when its map is no longer `current`.
    pub(crate) fn forget_stale(&mut self, kind: usize, current: Option<&wgpu::TextureView>) {
        let stale = self
            .pyramids
            .get(kind)
            .and_then(Option::as_ref)
            .is_some_and(|p| current != Some(&p.source));
        if stale {
            self.pyramids[kind] = None;
            self.history[kind] = None;
            self.cull_groups[kind] = None;
            self.kind_param_cache[kind] = None;
        }
    }

    /// Whether a job of `kind` tested last redraw's pyramid in phase 1 this frame (without one,
    /// phase 1 drew every caster and phase 2 has nothing to draw).
    pub(crate) fn kind_has_history(&self, kind: usize) -> bool {
        self.kind_active(kind) && self.stats.kinds.get(kind).is_some_and(|k| k.history_jobs > 0)
    }

    /// Records what `kind`'s pyramid now holds (after this frame's rebuild).
    pub(crate) fn note_history(
        &mut self,
        kind: usize,
        source: &wgpu::TextureView,
        map_size: u32,
        rects: impl Iterator<Item = ([u32; 4], [[f32; 4]; 4])>,
    ) {
        let Some(slot) = self.history.get_mut(kind) else {
            return;
        };
        let mut history = slot.take().unwrap_or_else(|| KindHistory {
            source: source.clone(),
            map_size,
            rects: Vec::new(),
        });
        history.source = source.clone();
        history.map_size = map_size;
        history.rects.clear();
        history.rects.extend(rects);
        *slot = Some(history);
    }

    /// Phase 1's commands (one per shadow instance slot).
    pub(crate) fn phase1_commands(&self) -> Option<&wgpu::Buffer> {
        self.phase1.as_ref()
    }

    /// Phase 2's commands (one per shadow instance slot).
    pub(crate) fn phase2_commands(&self) -> Option<&wgpu::Buffer> {
        self.phase2.as_ref()
    }

    /// The cull's compute pipelines (phase 1, phase 2, the pyramid's base and downsample), built
    /// once. The game-frame warm may call it ahead of the first culled frame (ADR-2482: the
    /// `shadow_occlusion` warm key the frame owner adds), so a switch turned on mid-play compiles
    /// nothing on the frame thread.
    pub(crate) fn ensure_pipes(&mut self, device: &crate::TrackedDevice) {
        if self.pipes.is_some() {
            return;
        }
        let storage = |binding: u32, read_only: bool| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let uniform = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Uniform,
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        };
        let texture = |binding: u32, sample_type: wgpu::TextureSampleType| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Texture {
                sample_type,
                view_dimension: wgpu::TextureViewDimension::D2,
                multisampled: false,
            },
            count: None,
        };
        let storage_texture = |binding: u32| wgpu::BindGroupLayoutEntry {
            binding,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::StorageTexture {
                access: wgpu::StorageTextureAccess::WriteOnly,
                format: wgpu::TextureFormat::R32Float,
                view_dimension: wgpu::TextureViewDimension::D2,
            },
            count: None,
        };
        let unfilterable = wgpu::TextureSampleType::Float { filterable: false };
        let cull_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow occlusion"),
            entries: &[
                uniform(0),
                storage(1, true),
                storage(2, true),
                storage(3, false),
                storage(4, false),
                texture(5, unfilterable),
            ],
        });
        let base_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow pyramid base"),
            entries: &[
                texture(0, wgpu::TextureSampleType::Depth),
                storage_texture(1),
                uniform(2),
            ],
        });
        let down_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("shadow pyramid down"),
            entries: &[storage_texture(1), texture(3, unfilterable)],
        });
        let cull_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadow occlusion"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/shadow_occlusion.wgsl").into()),
        });
        let pyramid_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("shadow pyramid"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/shadow_pyramid.wgsl").into()),
        });
        let pipe = |layout: &wgpu::BindGroupLayout, module: &wgpu::ShaderModule, entry: &str| {
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("shadow occlusion"),
                bind_group_layouts: &[Some(layout)][..],
                immediate_size: 0,
            });
            device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some(entry),
                layout: Some(&pipeline_layout),
                module,
                entry_point: Some(entry),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: None,
            })
        };
        self.pipes = Some(Pipes {
            phase1: pipe(&cull_layout, &cull_module, "cs_phase1"),
            phase2: pipe(&cull_layout, &cull_module, "cs_phase2"),
            base: pipe(&base_layout, &pyramid_module, "cs_base"),
            down: pipe(&down_layout, &pyramid_module, "cs_down"),
            cull_layout,
            base_layout,
            down_layout,
        });
    }

    /// The pyramid of `kind` over `source` (`map_size`²), made when the map or its size changed.
    /// A new pyramid holds nothing yet, so the kind's history is dropped with the old one.
    pub(crate) fn ensure_pyramid(
        &mut self,
        device: &crate::TrackedDevice,
        kind: usize,
        source: &wgpu::TextureView,
        map_size: u32,
    ) -> Result<(), RenderError> {
        self.ensure_pipes(device);
        let Some(pipes) = self.pipes.as_ref() else {
            return Err(RenderError::Shader("shadow occlusion pipelines missing".into()));
        };
        if self
            .pyramids
            .get(kind)
            .and_then(Option::as_ref)
            .is_some_and(|p| p.source == *source && p.map_size == map_size)
        {
            return Ok(());
        }
        let base = pyramid_base(map_size);
        let levels = pyramid_levels(base);
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("shadow occlusion pyramid"),
            size: wgpu::Extent3d {
                width: base,
                height: base,
                depth_or_array_layers: 1,
            },
            mip_level_count: levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R32Float,
            usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let level_view = |level: u32| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("shadow occlusion pyramid level"),
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        let levels_views: Vec<wgpu::TextureView> = (0..levels).map(level_view).collect();
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("shadow pyramid params"),
            size: std::mem::size_of::<ReduceParams>() as u64,
            usage: wgpu::BufferUsages::UNIFORM,
            mapped_at_creation: true,
        });
        {
            let reduce = ReduceParams {
                src_size: [map_size, map_size],
                block: PYRAMID_BLOCK,
                pad: 0,
            };
            params
                .slice(..)
                .get_mapped_range_mut()
                .map_err(|err| RenderError::Shader(format!("shadow pyramid params: {err}")))?
                .copy_from_slice(bytemuck::bytes_of(&reduce));
        }
        params.unmap();
        let base_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("shadow pyramid base"),
            layout: &pipes.base_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(source),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&levels_views[0]),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params.as_entire_binding(),
                },
            ],
        });
        let down_groups = levels_views
            .windows(2)
            .map(|pair| {
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("shadow pyramid down"),
                    layout: &pipes.down_layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(&pair[1]),
                        },
                        wgpu::BindGroupEntry {
                            binding: 3,
                            resource: wgpu::BindingResource::TextureView(&pair[0]),
                        },
                    ],
                })
            })
            .collect();
        let full = texture.create_view(&wgpu::TextureViewDescriptor {
            label: Some("shadow occlusion pyramid"),
            ..Default::default()
        });
        if let Some(slot) = self.pyramids.get_mut(kind) {
            *slot = Some(Pyramid {
                source: source.clone(),
                map_size,
                base,
                levels,
                full,
                base_group,
                down_groups,
                _params: params,
            });
        }
        if let Some(history) = self.history.get_mut(kind) {
            *history = None;
        }
        Ok(())
    }

    fn ensure_buffers(&mut self, device: &crate::TrackedDevice, entries: u32, jobs: u32, slots: u32) {
        let cap_for = |want: u32| want.max(64).next_power_of_two();
        if self.entries.is_none() || self.entry_cap < entries {
            self.entry_cap = cap_for(entries);
            self.entries = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("shadow occlusion entries"),
                size: u64::from(self.entry_cap) * std::mem::size_of::<ShadowCullEntry>() as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.entry_bytes.clear();
        }
        if self.jobs.is_none() || self.job_cap < jobs {
            self.job_cap = cap_for(jobs);
            self.jobs = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("shadow occlusion jobs"),
                size: u64::from(self.job_cap) * std::mem::size_of::<ShadowCullJob>() as u64,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.job_bytes.clear();
        }
        if self.phase1.is_none() || self.cmd_cap < slots {
            self.cmd_cap = cap_for(slots);
            let commands = |label: &str| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size: u64::from(self.cmd_cap) * CMD_STRIDE,
                    usage: wgpu::BufferUsages::STORAGE
                        | wgpu::BufferUsages::INDIRECT
                        | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                })
            };
            self.phase1 = Some(commands("shadow occlusion phase 1"));
            self.phase2 = Some(commands("shadow occlusion phase 2"));
        }
        if self.kind_params.len() < SHADOW_OCCLUSION_KINDS {
            self.kind_params = (0..SHADOW_OCCLUSION_KINDS)
                .map(|_| {
                    device.create_buffer(&wgpu::BufferDescriptor {
                        label: Some("shadow occlusion kind"),
                        size: std::mem::size_of::<KindParams>() as u64,
                        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                        mapped_at_creation: false,
                    })
                })
                .collect();
            self.kind_param_cache = [None; SHADOW_OCCLUSION_KINDS];
        }
    }

    /// Uploads this frame's entries ([`Self::entries_scratch`], kinds in order, `ranges` per kind)
    /// and jobs ([`Self::jobs_scratch`]), and makes the buffers and groups for them. The kinds with
    /// a non-empty range must have their pyramid ([`Self::ensure_pyramid`]).
    pub(crate) fn upload(
        &mut self,
        device: &crate::TrackedDevice,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        ranges: [(u32, u32); SHADOW_OCCLUSION_KINDS],
        slots: u32,
    ) -> Result<(), RenderError> {
        let entries = std::mem::take(&mut self.entries_scratch);
        let jobs = std::mem::take(&mut self.jobs_scratch);
        self.ensure_buffers(device, entries.len() as u32, jobs.len() as u32, slots);
        let result = self.upload_lists(device, belt, encoder, &entries, &jobs, ranges);
        self.entries_scratch = entries;
        self.jobs_scratch = jobs;
        self.ranges = ranges;
        self.slots = slots;
        self.phase2_ran = [false; SHADOW_OCCLUSION_KINDS];
        self.phase1_recorded = false;
        self.frame_active = result.is_ok();
        result
    }

    fn upload_lists(
        &mut self,
        device: &crate::TrackedDevice,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        entries: &[ShadowCullEntry],
        jobs: &[ShadowCullJob],
        ranges: [(u32, u32); SHADOW_OCCLUSION_KINDS],
    ) -> Result<(), RenderError> {
        let (Some(entry_buf), Some(job_buf), Some(phase1), Some(phase2)) = (
            self.entries.clone(),
            self.jobs.clone(),
            self.phase1.clone(),
            self.phase2.clone(),
        ) else {
            return Err(RenderError::Shader("shadow occlusion buffers missing".into()));
        };
        crate::staging::write_changed(
            belt,
            device,
            encoder,
            &entry_buf,
            bytemuck::cast_slice(entries),
            &mut self.entry_bytes,
        );
        crate::staging::write_changed(
            belt,
            device,
            encoder,
            &job_buf,
            bytemuck::cast_slice(jobs),
            &mut self.job_bytes,
        );
        let Some(pipes) = self.pipes.as_ref() else {
            return Err(RenderError::Shader("shadow occlusion pipelines missing".into()));
        };
        for (kind, &(offset, count)) in ranges.iter().enumerate() {
            if count == 0 {
                continue;
            }
            let (Some(pyramid), Some(params_buf)) = (
                self.pyramids.get(kind).and_then(Option::as_ref),
                self.kind_params.get(kind),
            ) else {
                return Err(RenderError::Shader("shadow occlusion pyramid missing".into()));
            };
            let params = KindParams {
                entry_offset: offset,
                entry_count: count,
                block: PYRAMID_BLOCK,
                levels: pyramid.levels,
            };
            if self.kind_param_cache[kind] != Some(params) {
                belt.write(device, encoder, params_buf, 0, bytemuck::bytes_of(&params));
                self.kind_param_cache[kind] = Some(params);
            }
            let key: CullKey = (
                entry_buf.clone(),
                job_buf.clone(),
                phase1.clone(),
                pyramid.full.clone(),
            );
            if self.cull_groups[kind].as_ref().is_some_and(|(k, _)| *k == key) {
                continue;
            }
            let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("shadow occlusion"),
                layout: &pipes.cull_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: params_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: entry_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: job_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 3,
                        resource: phase1.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 4,
                        resource: phase2.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 5,
                        resource: wgpu::BindingResource::TextureView(&pyramid.full),
                    },
                ],
            });
            self.cull_groups[kind] = Some((key, group));
        }
        Ok(())
    }

    /// Records `kind`'s cull of `phase2` (or phase 1) into `pass`; nothing for a kind without
    /// entries this frame. Returns whether it dispatched.
    fn dispatch_cull(&self, pass: &mut wgpu::ComputePass<'_>, kind: usize, phase2: bool) -> bool {
        let (Some(pipes), Some((_, group)), Some(&(_, count))) = (
            self.pipes.as_ref(),
            self.cull_groups.get(kind).and_then(Option::as_ref),
            self.ranges.get(kind),
        ) else {
            return false;
        };
        if count == 0 {
            return false;
        }
        pass.counted_set_pipeline(if phase2 { &pipes.phase2 } else { &pipes.phase1 });
        pass.counted_set_bind_group(0, group, &[]);
        pass.counted_dispatch(count.div_ceil(64), 1, 1);
        note_pass(if phase2 { 3 } else { 2 });
        true
    }

    /// Records phase 1 against the pyramids as they stand (after [`Self::upload`]). The first
    /// call of a frame records every culled kind's phase 1, one dispatch each, in a single compute
    /// pass: they read only what [`Self::upload`] wrote and last redraw's pyramids, which no
    /// shadow pass of this frame has touched yet, and write disjoint command slots. Later calls
    /// of the frame (one per culled kind) record nothing (ADR-2482: one pass instead of one per
    /// kind; wgpu allocates for every pass).
    pub(crate) fn record_phase1(&mut self, encoder: &mut wgpu::CommandEncoder, kind: usize) {
        let _ = kind;
        if self.phase1_recorded {
            return;
        }
        self.phase1_recorded = true;
        let _span = tracing::info_span!("shadow_occlusion_phase1").entered();
        let _crumb = crate::breadcrumbs::frame_pass("shadow_occlusion_phase1");
        if !(0..SHADOW_OCCLUSION_KINDS).any(|k| self.ranges[k].1 > 0 && self.cull_groups[k].is_some()) {
            return;
        }
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("shadow occlusion phase 1"),
            timestamp_writes: None,
        });
        for k in 0..SHADOW_OCCLUSION_KINDS {
            self.dispatch_cull(&mut pass, k, false);
        }
        drop(pass);
        note_pass(0);
    }

    /// Rebuilds `kind`'s pyramid from its map (after phase 1's pass): next redraw's phase 1
    /// tests it, and so does this frame's phase 2. When the kind has history (so the frame runs
    /// its phase 2), phase 2's cull is recorded in the same compute pass, right after the
    /// pyramid it reads, and [`Self::record_phase2`] records nothing more (ADR-2482).
    pub(crate) fn rebuild_pyramid(&mut self, encoder: &mut wgpu::CommandEncoder, kind: usize) {
        let _span = tracing::info_span!("shadow_occlusion_pyramid").entered();
        let _crumb = crate::breadcrumbs::frame_pass("shadow_occlusion_pyramid");
        let (Some(pipes), Some(pyramid)) = (
            self.pipes.as_ref(),
            self.pyramids.get(kind).and_then(Option::as_ref),
        ) else {
            return;
        };
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("shadow occlusion pyramid"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(&pipes.base);
        pass.counted_set_bind_group(0, &pyramid.base_group, &[]);
        let groups = pyramid.base.div_ceil(8);
        pass.counted_dispatch(groups, groups, 1);
        pass.counted_set_pipeline(&pipes.down);
        for (i, group) in pyramid.down_groups.iter().enumerate() {
            let edge = (pyramid.base >> (i + 1)).max(1);
            pass.counted_set_bind_group(0, group, &[]);
            pass.counted_dispatch(edge.div_ceil(8), edge.div_ceil(8), 1);
        }
        let phase2 = self.kind_has_history(kind) && {
            let _phase2 = tracing::info_span!("shadow_occlusion_phase2").entered();
            self.dispatch_cull(&mut pass, kind, true)
        };
        drop(pass);
        note_pass(1);
        if phase2 && let Some(ran) = self.phase2_ran.get_mut(kind) {
            *ran = true;
        }
    }

    /// Phase 2 of `kind` against the pyramid [`Self::rebuild_pyramid`] just built: recorded there,
    /// in the pyramid's compute pass. Kept as the frame's call site; records nothing itself.
    pub(crate) fn record_phase2(&mut self, encoder: &mut wgpu::CommandEncoder, kind: usize) {
        let _ = (encoder, kind);
        let _crumb = crate::breadcrumbs::frame_pass("shadow_occlusion_phase2");
    }

    /// Reads the last frame's commands back and counts each phase's draws per kind. Blocks on the
    /// GPU (tests, benches, the agent). `None` before the first two-phase frame.
    pub(crate) fn read_counters(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<ShadowOcclusionCounters>, RenderError> {
        let (Some(phase1), Some(phase2)) = (self.phase1.as_ref(), self.phase2.as_ref()) else {
            return Ok(None);
        };
        if self.slots == 0 {
            return Ok(Some(ShadowOcclusionCounters::default()));
        }
        let first = read_instance_counts(device, phase1, self.slots)?;
        let second = read_instance_counts(device, phase2, self.slots)?;
        let mut counters = ShadowOcclusionCounters::default();
        for (kind, &(offset, count)) in self.ranges.iter().enumerate() {
            let entries = self
                .entries_scratch
                .get(offset as usize..(offset + count) as usize)
                .unwrap_or(&[]);
            let ran2 = self.phase2_ran.get(kind).copied().unwrap_or(false);
            let out = &mut counters.kinds[kind];
            for entry in entries {
                let a = first.get(entry.slot as usize).copied().unwrap_or(0);
                let b = if ran2 {
                    second.get(entry.slot as usize).copied().unwrap_or(0)
                } else {
                    0
                };
                out.phase1 += a;
                out.phase2 += b;
                if a == 0 && b == 0 {
                    out.culled += 1;
                }
            }
            counters.total.phase1 += out.phase1;
            counters.total.phase2 += out.phase2;
            counters.total.culled += out.culled;
        }
        Ok(Some(counters))
    }
}

/// The `instance_count` word of the first `count` commands of `buffer`. Blocks on the GPU.
fn read_instance_counts(
    device: &crate::RenderDevice,
    buffer: &wgpu::Buffer,
    count: u32,
) -> Result<Vec<u32>, RenderError> {
    let bytes = u64::from(count) * CMD_STRIDE;
    let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shadow occlusion readback"),
        size: bytes,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("shadow occlusion readback"),
        });
    encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, bytes);
    device.tracked_queue().submit([encoder.finish()]);
    let mapped = map_read(device, &staging)?;
    let (commands, _) = mapped.as_chunks::<{ CMD_STRIDE as usize }>();
    let counts = commands
        .iter()
        .map(|c| u32::from_le_bytes([c[4], c[5], c[6], c[7]]))
        .collect();
    Ok(counts)
}

/// Maps `staging` and copies its bytes out. Blocks on the GPU.
fn map_read(device: &crate::RenderDevice, staging: &wgpu::Buffer) -> Result<Vec<u8>, RenderError> {
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
    let out = mapped.to_vec();
    drop(mapped);
    staging.unmap();
    Ok(out)
}

/// Copies a `size`² `Depth32Float` map's depth out through a compute copy (the maps are not
/// copy sources). Blocks on the GPU: tests and benches only.
pub(crate) fn read_depth_map(
    device: &crate::RenderDevice,
    view: &wgpu::TextureView,
    size: u32,
) -> Result<Vec<f32>, RenderError> {
    let d = &device.device;
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("shadow map snapshot"),
        source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/shadow_pyramid.wgsl").into()),
    });
    let layout = d.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("shadow map snapshot"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Depth,
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::StorageTexture {
                    access: wgpu::StorageTextureAccess::WriteOnly,
                    format: wgpu::TextureFormat::R32Float,
                    view_dimension: wgpu::TextureViewDimension::D2,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::COMPUTE,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            },
        ],
    });
    let pipeline_layout = d.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("shadow map snapshot"),
        bind_group_layouts: &[Some(&layout)][..],
        immediate_size: 0,
    });
    let pipe = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("shadow map snapshot"),
        layout: Some(&pipeline_layout),
        module: &module,
        entry_point: Some("cs_base"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });
    let size = size.max(1);
    let copy = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("shadow map snapshot"),
        size: wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::R32Float,
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let copy_view = copy.create_view(&wgpu::TextureViewDescriptor::default());
    let params = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shadow map snapshot"),
        size: std::mem::size_of::<ReduceParams>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    device.queue.write_buffer(
        &params,
        0,
        bytemuck::bytes_of(&ReduceParams {
            src_size: [size, size],
            block: 1,
            pad: 0,
        }),
    );
    let group = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("shadow map snapshot"),
        layout: &layout,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::TextureView(&copy_view),
            },
            wgpu::BindGroupEntry {
                binding: 2,
                resource: params.as_entire_binding(),
            },
        ],
    });
    let row = (size * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("shadow map snapshot"),
        size: u64::from(row) * u64::from(size),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("shadow map snapshot"),
    });
    {
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("shadow map snapshot"),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(&pipe);
        pass.counted_set_bind_group(0, &group, &[]);
        pass.counted_dispatch(size.div_ceil(8), size.div_ceil(8), 1);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &copy,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(size),
            },
        },
        wgpu::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
    );
    device.tracked_queue().submit([encoder.finish()]);
    let bytes = map_read(device, &staging)?;
    let mut out = Vec::with_capacity((size * size) as usize);
    for y in 0..size as usize {
        let start = y * row as usize;
        let line = bytes
            .get(start..start + size as usize * 4)
            .ok_or_else(|| RenderError::Readback("shadow map snapshot too short".into()))?;
        let (texels, _) = line.as_chunks::<4>();
        out.extend(texels.iter().map(|t| f32::from_le_bytes(*t)));
    }
    Ok(out)
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "shadow_occlusion.wgsl",
            "Job",
            ShadowCullJob {
                view_proj,
                prev_view_proj,
                viewport,
                clip,
                flags
            }
        ),
        crate::contract_row!(
            "shadow_occlusion.wgsl",
            "KindParams",
            KindParams {
                entry_offset,
                entry_count,
                block,
                levels
            }
        ),
        crate::contract_row!(
            "shadow_occlusion.wgsl",
            "Entry",
            ShadowCullEntry {
                base_vertex,
                center_radius,
                first_index,
                index_count,
                job,
                slot
            }
        )
        .partial(),
        crate::contract_row!("shadow_pyramid.wgsl", "Reduce", ReduceParams { block, src_size }).partial(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpu_structs_match_the_shader_layout() {
        assert_eq!(std::mem::size_of::<ShadowCullEntry>(), 48);
        assert_eq!(std::mem::size_of::<ShadowCullJob>(), 64 + 64 + 16 + 16 + 16);
        assert_eq!(std::mem::size_of::<KindParams>(), 16);
        assert_eq!(std::mem::size_of::<ReduceParams>(), 16);
    }

    #[test]
    fn pyramids_are_coarse_and_halve_exactly() {
        assert_eq!(pyramid_base(4096), 512);
        assert_eq!(pyramid_base(8192), 1024);
        assert_eq!(pyramid_base(2048), 256);
        // Not a multiple of the block, not a power of two: rounded up.
        assert_eq!(pyramid_base(1000), 128);
        assert_eq!(pyramid_base(4), 1);
        assert_eq!(pyramid_levels(512), 10);
        assert_eq!(pyramid_levels(1), 1);
        // Four 4096² cascades and an 8192² atlas: about 11 MB, every mip.
        let total = 4 * pyramid_bytes(pyramid_base(4096)) + pyramid_bytes(pyramid_base(8192));
        assert!(total < 12 * 1024 * 1024, "{total}");
        assert!(total > 10 * 1024 * 1024, "{total}");
    }

    #[test]
    fn skip_reasons_and_kinds_have_stable_protocol_names() {
        let names: Vec<&str> = [
            ShadowOcclusionSkip::Off,
            ShadowOcclusionSkip::NoGpuObjects,
            ShadowOcclusionSkip::NoJobs,
            ShadowOcclusionSkip::FewCasters,
            ShadowOcclusionSkip::Failed,
        ]
        .iter()
        .map(|s| s.name())
        .collect();
        assert_eq!(
            names,
            ["off", "no_gpu_objects", "no_jobs", "few_casters", "failed"]
        );
        let kinds: Vec<&str> = (0..SHADOW_OCCLUSION_KINDS).map(shadow_kind_name).collect();
        assert_eq!(kinds, ["cascade0", "cascade1", "cascade2", "cascade3", "atlas"]);
        for name in names.into_iter().chain(kinds) {
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            );
        }
    }

    #[test]
    fn shaders_validate() {
        for (name, source) in [
            (
                "shadow_occlusion.wgsl",
                include_str!("../shaders/shadow_occlusion.wgsl"),
            ),
            (
                "shadow_pyramid.wgsl",
                include_str!("../shaders/shadow_pyramid.wgsl"),
            ),
        ] {
            let module = naga::front::wgsl::parse_str(source).unwrap_or_else(|err| panic!("{name}: {err}"));
            naga::valid::Validator::new(
                naga::valid::ValidationFlags::all(),
                naga::valid::Capabilities::all(),
            )
            .validate(&module)
            .unwrap_or_else(|err| panic!("{name}: {err:?}"));
        }
    }

    #[test]
    fn switching_off_drops_the_state_and_keeps_the_threshold() {
        let mut occ = ShadowOcclusion {
            min_casters: Some(16),
            ..ShadowOcclusion::default()
        };
        occ.set_enabled(true);
        occ.stats.frames = 3;
        occ.ranges[2] = (0, 5);
        occ.frame_active = true;
        assert!(occ.kind_active(2));
        occ.pass_through();
        assert!(
            !occ.kind_active(2),
            "a texture view never draws the main view's commands"
        );
        occ.frame_active = true;
        occ.set_enabled(false);
        assert!(!occ.enabled && !occ.stats.enabled);
        assert!(!occ.kind_active(2));
        assert_eq!(occ.min_casters(), 16);
        assert_eq!(occ.stats.frames, 3);
        assert_eq!(
            ShadowOcclusion::default().min_casters(),
            SHADOW_OCCLUSION_MIN_CASTERS
        );
    }
}
