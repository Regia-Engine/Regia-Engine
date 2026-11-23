// SPDX-License-Identifier: MPL-2.0
//! Two-phase occlusion culling of the forward depth prepass (`docs/specs/98-visibility-buffer.md`,
//! ADR-1887; Haar & Aaltonen 2015).
//!
//! The visibility buffer already culls its clusters in two phases (`crate::visbuffer`). The
//! forward meshes' depth prepass drew every opaque instance in the view, and only the colour pass
//! culled against a pyramid of that depth. With the switch on
//! ([`crate::FrameRenderer::set_two_phase_occlusion`]) the GPU-driven prepass is culled too:
//!
//! 1. **Phase 1** (compute): every depth-prepass instance takes the frustum test and the
//!    conservative Hi-Z sphere test (ADR-1238) against *last frame's* pyramid, projected with the
//!    view-projection that pyramid was built with. The survivors draw into the depth buffer, with
//!    one `multi_draw_indexed_indirect` per instanced batch (an instance count of 0 or 1 per
//!    command, so the batch keeps its instance order).
//! 2. The pyramid is rebuilt from that depth.
//! 3. **Phase 2** (compute): the instances phase 1 skipped are tested against the new pyramid
//!    with this frame's matrix, and the survivors draw in a second depth pass.
//!
//! Phase 2's pyramid holds a subset of this frame's opaque depth, so it is never nearer than the
//! full prepass: what it hides, the full prepass hides too. Depth writes under a `Less` test
//! keep the nearest value whatever the order, so the two phases write the depth buffer an unculled
//! prepass writes. Phase 1 only decides how much work phase 2 has; a camera cut makes phase 1
//! cull too much and phase 2 draws the rest.
//!
//! Without last frame's pyramid (the first frame, a size change, a new pyramid) phase 1 draws
//! every instance in the frustum and phase 2 is not recorded.

use crate::RenderError;
use crate::frame_work::counted::*;

/// What the depth prepass's two-phase occlusion did in the last main-view frame
/// ([`crate::FrameRenderer::prepass_occlusion_stats`]). CPU-side: the per-phase GPU counts are
/// [`crate::FrameRenderer::prepass_occlusion_counters`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrepassOcclusionStats {
    /// The switch is on ([`crate::FrameRenderer::set_two_phase_occlusion`]).
    pub enabled: bool,
    /// The last main-view frame culled its depth prepass in two phases.
    pub active: bool,
    /// Why the last main-view frame did not, when it did not.
    pub skipped: Option<PrepassOcclusionSkip>,
    /// Phase 1 of the last active frame tested last frame's pyramid (and phase 2 ran). `false`
    /// on the first frame, after a size change or a new pyramid.
    pub history: bool,
    /// Depth-prepass instances the last active frame tested.
    pub instances: u32,
    /// Of those, instances with no bound (compute-skinned twins, vertex-stage graphs): always
    /// drawn in phase 1. A rigid morph keeps its rest sphere plus `morph.inflate` and is culled
    /// with it, so it is not counted here.
    pub unbounded: u32,
    /// Instanced batches: one indirect multi-draw per batch and phase.
    pub batches: u32,
    /// Frames that culled their prepass in two phases since the renderer was made.
    pub frames: u64,
}

/// Why a frame's depth prepass was not culled in two phases ([`PrepassOcclusionStats::skipped`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrepassOcclusionSkip {
    /// The switch is off.
    Off,
    /// The frame has no depth prepass.
    NoPrepass,
    /// The prepass is not GPU-driven: no storage-object instancing on this device or frame
    /// (CPU draw path, MSAA, `gpu_driven` off in the quality).
    NoGpuObjects,
    /// The frame is multisampled (the pyramid needs single-sample depth).
    Multisampled,
    /// Named for a render-to-texture view. The frame recorder does not write it: a texture view
    /// returns before `skip`, so the report stays the last main view (`skipped` is `null` while
    /// that view is the only one that has run).
    TextureView,
    /// Fewer depth-prepass instances than the Hi-Z threshold (2048): the full-frame pyramid would
    /// cost more than the draws it rejects.
    FewInstances,
    /// Hi-Z occlusion is off ([`crate::FrameRenderer::set_hiz_occlusion`]).
    HizOff,
}

impl PrepassOcclusionSkip {
    /// Stable lowercase name (agent protocol).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::NoPrepass => "no_prepass",
            Self::NoGpuObjects => "no_gpu_objects",
            Self::Multisampled => "multisampled",
            Self::TextureView => "texture_view",
            Self::FewInstances => "few_instances",
            Self::HizOff => "hiz_off",
        }
    }
}

/// GPU counts of the last two-phase frame ([`crate::FrameRenderer::prepass_occlusion_counters`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PrepassOcclusionCounters {
    /// Instances drawn in phase 1.
    pub phase1: u32,
    /// Instances drawn in phase 2 (skipped by phase 1, visible against this frame's pyramid).
    pub phase2: u32,
    /// Instances drawn in neither phase.
    pub culled: u32,
}

/// One depth-prepass instance as the cull reads it.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct OcclusionInstance {
    /// World centre and radius; a negative radius has no bound (always drawn in phase 1).
    pub center_radius: [f32; 4],
    /// Index count of the mesh.
    pub index_count: u32,
    /// First index of the mesh in its index buffer.
    pub first_index: u32,
    /// The command's `base_vertex` bits (an `i32`).
    pub base_vertex: u32,
    /// The instance's slot in the instance object buffer.
    pub first_instance: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    view_proj: [[f32; 4]; 4],
    prev_view_proj: [[f32; 4]; 4],
    planes: [[f32; 4]; 6],
    screen: [f32; 4],
    forward: [f32; 4],
    instance_count: u32,
    _pad: [u32; 3],
}

/// Bytes of one `DrawIndexedIndirect` command.
pub(crate) const CMD_STRIDE: u64 = 20;

/// What phase 1 needs from the frame.
pub(crate) struct Phase1View<'a> {
    /// This frame's unjittered view-projection.
    pub view_proj: [[f32; 4]; 4],
    /// This frame's frustum planes (inside: distance >= 0).
    pub planes: [[f32; 4]; 6],
    /// Camera forward (world, unit).
    pub forward: [f32; 3],
    /// Attachment size.
    pub width: u32,
    /// Attachment size.
    pub height: u32,
    /// The Hi-Z pyramid's sampled view (`crate::gpu_driven::GpuCuller::hiz_view`).
    pub hiz: &'a wgpu::TextureView,
}

/// One window view's Hi-Z pyramid as the renderer keeps it for that view's occlusion history
/// ([`crate::FrameRenderer::occlusion_pyramids_into`], ADR-7329).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct OcclusionViewPyramid {
    /// The view's index in `ExtractedFrame::views`.
    pub view: u32,
    /// Pyramid width (the view's attachment width).
    pub width: u32,
    /// Pyramid height.
    pub height: u32,
    /// Bytes of every mip (`R32Float`).
    pub bytes: u64,
    /// The pyramid is the culler's live one (the last window view drawn); the others are kept
    /// aside until their view draws again.
    pub resident: bool,
}

/// Window views that keep a pyramid of their own at most. A view index past it shares the live
/// pyramid (the behavior before ADR-7329: still conservative).
pub(crate) const MAX_OCCLUSION_VIEWS: usize = 64;

/// Rounds a window view may go undrawn before the renderer frees its occlusion history: its Hi-Z
/// pyramid, its visibility-buffer cluster bits and its cluster LOD history (ADR-8620). A round
/// ends when a view draws a second time, so one frame of split screen is one round however many
/// views it draws, and a host that draws one view counts one round per frame. A view that comes
/// back after it was retired starts as a new view (its first phase draws nothing, its first
/// pyramid is made again). [`crate::FrameRenderer::retain_occlusion_views`] frees views at once.
pub const OCCLUSION_VIEW_RETIRE_ROUNDS: u64 = 16;

/// The bind group over the cull's buffers and one pyramid, with the objects it binds.
type CullBind = ((wgpu::Buffer, wgpu::TextureView), wgpu::BindGroup);

/// A window view's occlusion history while another view draws (ADR-7329): its pyramid, the
/// matrix that pyramid was built with and the cull's bind group over it.
#[derive(Default)]
struct ViewSlot {
    hiz: crate::gpu_driven::ParkedHiz,
    history: Option<History>,
    bind: Option<CullBind>,
}

/// The pyramid a phase 1 may test: the view it was built into, its size and its matrix.
struct History {
    view: wgpu::TextureView,
    width: u32,
    height: u32,
    view_proj: [[f32; 4]; 4],
}

/// The prepass's two-phase cull: pipelines, the instance list and both phases' commands.
#[derive(Default)]
pub(crate) struct PrepassOcclusion {
    /// [`crate::FrameRenderer::set_two_phase_occlusion`].
    pub(crate) enabled: bool,
    pipes: Option<(
        wgpu::ComputePipeline,
        wgpu::ComputePipeline,
        wgpu::BindGroupLayout,
    )>,
    params: Option<wgpu::Buffer>,
    instances: Option<wgpu::Buffer>,
    phase1: Option<wgpu::Buffer>,
    phase2: Option<wgpu::Buffer>,
    cap: u32,
    bind: Option<CullBind>,
    instance_bytes: Vec<u8>,
    param_bytes: Vec<u8>,
    /// The frame's cull list, kept so a two-phase frame allocates nothing.
    pub(crate) scratch: Vec<OcclusionInstance>,
    history: Option<History>,
    count: u32,
    /// The last recorded frame ran phase 2.
    ran_phase2: bool,
    pub(crate) stats: PrepassOcclusionStats,
    /// Where each frame's result goes for `ViewVisibility` ([`crate::OcclusionFeedback`],
    /// ADR-7255); `None` reads nothing back.
    pub(crate) feedback: Option<crate::occlusion_feedback::OcclusionFeedback>,
    /// Per cull-list entry of the last frame: the draw's object key and whether its bound passed
    /// the CPU frustum test (kept, so a two-phase frame allocates nothing).
    pub(crate) keys: Vec<(u64, bool)>,
    /// The view the main-view frames speak for in `feedback` (its index in
    /// `ExtractedFrame::views`, ADR-7286).
    pub(crate) feedback_view: u32,
    /// The visibility buffer's per-instance bit ranges of the last frame (kept, so a cluster
    /// frame allocates nothing; ADR-7286).
    pub(crate) cluster_ranges: Vec<(u64, u32, u32)>,
    /// Per window view (its index in `ExtractedFrame::views`), the history kept aside while
    /// another view draws (ADR-7329). The resident view's slot is empty: its history is live.
    views: Vec<ViewSlot>,
    /// The window view whose pyramid, history and bind group are live.
    resident: Option<u32>,
    /// The current round of window-view draws (ADR-8620): it ends when a view draws again.
    round: u64,
    /// Per window view, the round it last drew in (0: not drawn since it was made or retired).
    drawn: Vec<u64>,
    /// Views retired for going undrawn since the renderer was built.
    retired: u64,
}

impl PrepassOcclusion {
    /// Records that the pyramid behind `view` now holds depth projected with `view_proj`.
    pub(crate) fn note_pyramid(
        &mut self,
        view: &wgpu::TextureView,
        width: u32,
        height: u32,
        view_proj: [[f32; 4]; 4],
    ) {
        self.history = Some(History {
            view: view.clone(),
            width,
            height,
            view_proj,
        });
    }

    /// Brings the next window view's ([`Self::feedback_view`]) pyramid, history and bind group
    /// in, and keeps the outgoing view's aside (ADR-7329): each split-screen view's phase 1 then
    /// tests its own last pyramid with its own matrix. One view never swaps; a steady split
    /// screen swaps without making anything. The first view adopts the live pyramid.
    pub(crate) fn select_view(&mut self, culler: &mut crate::gpu_driven::GpuCuller) {
        let view = self.feedback_view;
        let index = view as usize;
        if self.resident == Some(view) || index >= MAX_OCCLUSION_VIEWS {
            return;
        }
        if self.views.len() <= index {
            self.views.resize_with(index + 1, ViewSlot::default);
        }
        let parked = self
            .resident
            .map(|r| r as usize)
            .filter(|&r| r < self.views.len());
        if let Some(out) = parked {
            let slot = &mut self.views[out];
            culler.swap_hiz(&mut slot.hiz);
            slot.history = self.history.take();
            slot.bind = self.bind.take();
        } else if !self.views[index].hiz.holds() {
            // The first window view (or one after the live view went away without a pyramid of
            // its own): the live pyramid, if any, becomes this view's.
            self.resident = Some(view);
            return;
        }
        let slot = &mut self.views[index];
        culler.swap_hiz(&mut slot.hiz);
        // Without a parked view, `slot.hiz` now holds the pyramid of a view that went away.
        slot.hiz = crate::gpu_driven::ParkedHiz::default();
        self.history = slot.history.take();
        self.bind = slot.bind.take();
        self.resident = Some(view);
    }

    /// Frees the pyramids of window views at index `views` and above (the frame has fewer views
    /// now; ADR-7329) and their reports in `feedback`.
    pub(crate) fn retain_views(&mut self, views: usize, culler: &mut crate::gpu_driven::GpuCuller) {
        self.views.truncate(views);
        self.drawn.truncate(views);
        if self.resident.is_some_and(|view| view as usize >= views) {
            let mut gone = crate::gpu_driven::ParkedHiz::default();
            culler.swap_hiz(&mut gone);
            self.history = None;
            self.bind = None;
            self.resident = None;
        }
        if let Some(feedback) = self.feedback.as_ref() {
            feedback.retain_pyramids(views);
        }
    }

    /// Notes that the window view [`Self::feedback_view`] draws now, and returns the views that
    /// went undrawn for more than [`OCCLUSION_VIEW_RETIRE_ROUNDS`] rounds as a bit mask
    /// (ADR-8620). Allocates only the first time a view index is seen.
    pub(crate) fn tick_views(&mut self) -> u64 {
        let index = self.feedback_view as usize;
        if index >= MAX_OCCLUSION_VIEWS {
            return 0;
        }
        if self.drawn.len() <= index {
            self.drawn.resize(index + 1, 0);
        }
        if self.round == 0 || self.drawn[index] == self.round {
            self.round += 1;
        }
        self.drawn[index] = self.round;
        let mut retire = 0u64;
        for (view, &at) in self.drawn.iter().enumerate() {
            if at != 0 && self.round - at > OCCLUSION_VIEW_RETIRE_ROUNDS {
                retire |= 1u64 << view;
            }
        }
        retire
    }

    /// Frees `view`'s pyramid, its pyramid history and bind group, and its pyramid report in
    /// `feedback` (ADR-8620's retirement). The view starts as a new one when it draws again.
    pub(crate) fn retire_view(&mut self, view: u32, culler: &mut crate::gpu_driven::GpuCuller) {
        let index = view as usize;
        if let Some(at) = self.drawn.get_mut(index) {
            *at = 0;
        }
        if let Some(slot) = self.views.get_mut(index) {
            *slot = ViewSlot::default();
        }
        if self.resident == Some(view) {
            let mut gone = crate::gpu_driven::ParkedHiz::default();
            culler.swap_hiz(&mut gone);
            self.history = None;
            self.bind = None;
            self.resident = None;
        }
        if let Some(feedback) = self.feedback.as_ref() {
            feedback.note_view_pyramid(view, 0);
        }
        self.retired += 1;
    }

    /// Window views retired for going undrawn since the renderer was built (ADR-8620).
    pub(crate) fn retired_views(&self) -> u64 {
        self.retired
    }

    /// Reports the live view's pyramid bytes to `feedback` when they changed (ADR-7329).
    pub(crate) fn report_pyramid(&self, culler: &crate::gpu_driven::GpuCuller) {
        let (Some(view), Some(feedback)) = (self.resident, self.feedback.as_ref()) else {
            return;
        };
        let bytes = culler.hiz_extent().map_or(0, |(_, _, bytes)| bytes);
        if feedback.view_pyramid_bytes(view) != bytes {
            feedback.note_view_pyramid(view, bytes);
        }
    }

    /// Each window view's pyramid, ascending by view, into `out` (its capacity reused).
    pub(crate) fn pyramids_into(
        &self,
        culler: &crate::gpu_driven::GpuCuller,
        out: &mut Vec<OcclusionViewPyramid>,
    ) {
        out.clear();
        for (index, slot) in self.views.iter().enumerate() {
            let Ok(view) = u32::try_from(index) else {
                break;
            };
            let resident = self.resident == Some(view);
            let extent = if resident {
                culler.hiz_extent()
            } else {
                slot.hiz.extent()
            };
            if let Some((width, height, bytes)) = extent {
                out.push(OcclusionViewPyramid {
                    view,
                    width,
                    height,
                    bytes,
                    resident,
                });
            }
        }
    }

    /// The matrix the pyramid behind `view` was last built with, when it was built at this size.
    fn history_for(&self, view: &wgpu::TextureView, width: u32, height: u32) -> Option<[[f32; 4]; 4]> {
        self.history
            .as_ref()
            .filter(|h| h.view == *view && h.width == width && h.height == height)
            .map(|h| h.view_proj)
    }

    /// Phase 1's commands (one per instance).
    pub(crate) fn phase1_commands(&self) -> Option<&wgpu::Buffer> {
        self.phase1.as_ref()
    }

    /// Phase 2's commands (one per instance).
    pub(crate) fn phase2_commands(&self) -> Option<&wgpu::Buffer> {
        self.phase2.as_ref()
    }

    /// Whether the last recorded frame ran phase 2.
    pub(crate) fn ran_phase2(&self) -> bool {
        self.ran_phase2
    }

    /// Records a skipped frame.
    pub(crate) fn skip(&mut self, why: PrepassOcclusionSkip) {
        self.stats.enabled = self.enabled;
        self.stats.active = false;
        self.stats.skipped = Some(why);
        self.ran_phase2 = false;
    }

    fn ensure(
        &mut self,
        device: &crate::TrackedDevice,
        count: u32,
        reserved: u32,
    ) -> Result<(), RenderError> {
        if self.pipes.is_none() {
            let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
                label: Some("prepass occlusion"),
                source: wgpu::ShaderSource::Wgsl(include_str!("../shaders/prepass_occlusion.wgsl").into()),
            });
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
            let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("prepass occlusion"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Uniform,
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    storage(1, true),
                    storage(2, false),
                    storage(3, false),
                    wgpu::BindGroupLayoutEntry {
                        binding: 4,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Texture {
                            sample_type: wgpu::TextureSampleType::Float { filterable: false },
                            view_dimension: wgpu::TextureViewDimension::D2,
                            multisampled: false,
                        },
                        count: None,
                    },
                ],
            });
            let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                label: Some("prepass occlusion"),
                bind_group_layouts: &[Some(&layout)][..],
                immediate_size: 0,
            });
            let pipe = |entry: &str| {
                device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    label: Some("prepass occlusion"),
                    layout: Some(&pipeline_layout),
                    module: &module,
                    entry_point: Some(entry),
                    compilation_options: wgpu::PipelineCompilationOptions::default(),
                    cache: None,
                })
            };
            self.pipes = Some((pipe("cs_phase1"), pipe("cs_phase2"), layout));
        }
        if self.params.is_none() {
            self.params = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("prepass occlusion params"),
                size: std::mem::size_of::<Params>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            self.param_bytes.clear();
        }
        let want = count.max(reserved).max(1);
        if self.instances.is_none() || self.cap < want {
            let cap = want.next_power_of_two();
            let instance_size = u64::from(cap) * std::mem::size_of::<OcclusionInstance>() as u64;
            let cmd_size = u64::from(cap) * CMD_STRIDE;
            self.instances = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("prepass occlusion instances"),
                size: instance_size,
                usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
            let commands = |label: &str| {
                device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some(label),
                    size: cmd_size,
                    usage: wgpu::BufferUsages::STORAGE
                        | wgpu::BufferUsages::INDIRECT
                        | wgpu::BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                })
            };
            self.phase1 = Some(commands("prepass occlusion phase 1"));
            self.phase2 = Some(commands("prepass occlusion phase 2"));
            self.cap = cap;
            self.instance_bytes.clear();
            self.bind = None;
        }
        Ok(())
    }

    /// Uploads the instances and phase 1's parameters ([`Self::record_phase1`] dispatches).
    /// Returns whether phase 2 must run (phase 1 tests last frame's pyramid).
    pub(crate) fn upload(
        &mut self,
        device: &crate::TrackedDevice,
        sink: &mut dyn crate::staging::BufferSink,
        instances: &[OcclusionInstance],
        reserved: u32,
        view: &Phase1View<'_>,
    ) -> Result<bool, RenderError> {
        self.ensure(device, instances.len() as u32, reserved)?;
        let prev = self.history_for(view.hiz, view.width, view.height);
        let history = prev.is_some();
        self.count = instances.len() as u32;
        let params = Params {
            view_proj: view.view_proj,
            prev_view_proj: prev.unwrap_or(view.view_proj),
            planes: view.planes,
            screen: [
                view.width.max(1) as f32,
                view.height.max(1) as f32,
                if history { 1.0 } else { 0.0 },
                if history { 1.0 } else { 0.0 },
            ],
            forward: [view.forward[0], view.forward[1], view.forward[2], 0.0],
            instance_count: self.count,
            _pad: [0; 3],
        };
        let (Some(params_buf), Some(instance_buf)) = (self.params.clone(), self.instances.clone()) else {
            return Err(RenderError::Shader("prepass occlusion buffers missing".into()));
        };
        let param_bytes = bytemuck::bytes_of(&params);
        if !crate::bind_cache::bytes_match(&self.param_bytes, param_bytes) {
            sink.write_buffer(&params_buf, 0, param_bytes);
            self.param_bytes.clear();
            self.param_bytes.extend_from_slice(param_bytes);
        }
        crate::staging::sink_write_changed(
            sink,
            &instance_buf,
            bytemuck::cast_slice(instances),
            &mut self.instance_bytes,
        );
        self.ensure_bind(device, view.hiz)?;
        self.ran_phase2 = false;
        Ok(history)
    }

    /// Makes the pipelines, the buffers for `reserved` instances and the bind group over `hiz`
    /// ahead of the first two-phase frame (a level that reserved a dense view), so a camera cut
    /// onto that view creates nothing here (`specs/27` § 2.4).
    pub(crate) fn prepare(
        &mut self,
        device: &crate::TrackedDevice,
        reserved: u32,
        hiz: &wgpu::TextureView,
    ) -> Result<(), RenderError> {
        self.ensure(device, 0, reserved)?;
        self.ensure_bind(device, hiz)
    }

    /// The bind group over the current buffers and `hiz`, made when either changed.
    fn ensure_bind(
        &mut self,
        device: &crate::TrackedDevice,
        hiz: &wgpu::TextureView,
    ) -> Result<(), RenderError> {
        let (Some(params), Some(instances), Some(phase1), Some(phase2), Some((_, _, layout))) = (
            self.params.clone(),
            self.instances.clone(),
            self.phase1.clone(),
            self.phase2.clone(),
            self.pipes.clone(),
        ) else {
            return Err(RenderError::Shader("prepass occlusion buffers missing".into()));
        };
        let key = (instances.clone(), hiz.clone());
        if self.bind.as_ref().is_some_and(|(k, _)| *k == key) {
            return Ok(());
        }
        let group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("prepass occlusion"),
            layout: &layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: params.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: instances.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: phase1.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: phase2.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 4,
                    resource: wgpu::BindingResource::TextureView(hiz),
                },
            ],
        });
        self.bind = Some((key, group));
        Ok(())
    }

    /// Records phase 1 against last frame's pyramid (after [`Self::upload`]).
    pub(crate) fn record_phase1(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let _span = tracing::info_span!("prepass_occlusion_phase1").entered();
        let _crumb = crate::breadcrumbs::frame_pass("prepass_occlusion_phase1");
        let Some((pipe1, _, _)) = self.pipes.clone() else {
            return;
        };
        self.dispatch(encoder, &pipe1, "prepass occlusion phase 1");
    }

    /// Records phase 2 against the pyramid just rebuilt from phase 1's depth.
    pub(crate) fn record_phase2(&mut self, encoder: &mut wgpu::CommandEncoder) {
        let _span = tracing::info_span!("prepass_occlusion_phase2").entered();
        let _crumb = crate::breadcrumbs::frame_pass("prepass_occlusion_phase2");
        let Some((_, pipe2, _)) = self.pipes.clone() else {
            return;
        };
        self.dispatch(encoder, &pipe2, "prepass occlusion phase 2");
        self.ran_phase2 = true;
    }

    fn dispatch(&self, encoder: &mut wgpu::CommandEncoder, pipe: &wgpu::ComputePipeline, label: &str) {
        let Some((_, group)) = self.bind.as_ref() else {
            return;
        };
        if self.count == 0 {
            return;
        }
        let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some(label),
            timestamp_writes: None,
        });
        pass.counted_set_pipeline(pipe);
        pass.counted_set_bind_group(0, group, &[]);
        pass.counted_dispatch(self.count.div_ceil(64), 1, 1);
    }

    /// Reads the last frame's commands back and counts each phase's draws. Blocks on the GPU
    /// (tests and benches). `None` before the first two-phase frame.
    pub(crate) fn read_counters(
        &self,
        device: &crate::RenderDevice,
    ) -> Result<Option<PrepassOcclusionCounters>, RenderError> {
        let (Some(phase1), Some(phase2)) = (self.phase1.as_ref(), self.phase2.as_ref()) else {
            return Ok(None);
        };
        if self.count == 0 {
            return Ok(Some(PrepassOcclusionCounters::default()));
        }
        let first = read_instance_counts(device, phase1, self.count)?;
        let second = if self.ran_phase2 {
            read_instance_counts(device, phase2, self.count)?
        } else {
            vec![0; first.len()]
        };
        let mut counters = PrepassOcclusionCounters::default();
        for (a, b) in first.iter().zip(&second) {
            counters.phase1 += a;
            counters.phase2 += b;
            if *a == 0 && *b == 0 {
                counters.culled += 1;
            }
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
        label: Some("prepass occlusion readback"),
        size: bytes,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("prepass occlusion readback"),
        });
    encoder.copy_buffer_to_buffer(buffer, 0, &staging, 0, bytes);
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
    let (commands, _) = mapped.as_chunks::<{ CMD_STRIDE as usize }>();
    let counts = commands
        .iter()
        .map(|c| u32::from_le_bytes([c[4], c[5], c[6], c[7]]))
        .collect();
    drop(mapped);
    staging.unmap();
    Ok(counts)
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "prepass_occlusion.wgsl",
            "Instance",
            OcclusionInstance {
                center_radius,
                index_count,
                first_index,
                base_vertex,
                first_instance
            }
        ),
        crate::contract_row!(
            "prepass_occlusion.wgsl",
            "Params",
            Params {
                forward,
                instance_count,
                planes,
                prev_view_proj,
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

    #[test]
    fn params_match_the_shader_layout() {
        // Two matrices, six planes, two vec4s and a 16-byte scalar tail.
        assert_eq!(std::mem::size_of::<Params>(), 64 + 64 + 96 + 16 + 16 + 16);
        assert_eq!(std::mem::size_of::<OcclusionInstance>(), 32);
    }

    #[test]
    fn skip_reasons_have_stable_protocol_names() {
        let all = [
            PrepassOcclusionSkip::Off,
            PrepassOcclusionSkip::NoPrepass,
            PrepassOcclusionSkip::NoGpuObjects,
            PrepassOcclusionSkip::Multisampled,
            PrepassOcclusionSkip::TextureView,
            PrepassOcclusionSkip::FewInstances,
            PrepassOcclusionSkip::HizOff,
        ];
        let names: Vec<&str> = all.iter().map(|s| s.name()).collect();
        assert_eq!(
            names,
            [
                "off",
                "no_prepass",
                "no_gpu_objects",
                "multisampled",
                "texture_view",
                "few_instances",
                "hiz_off"
            ]
        );
        for name in names {
            assert!(name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
        }
    }

    /// ADR-8620: a round ends when a view draws again, so three views per frame and then view 0
    /// alone retire views 1 and 2 after `OCCLUSION_VIEW_RETIRE_ROUNDS + 1` frames of view 0, and
    /// a steady single view never grows the bookkeeping (no per-frame allocation).
    #[test]
    fn idle_views_retire_after_the_documented_rounds() {
        let mut occlusion = PrepassOcclusion::default();
        let tick = |occlusion: &mut PrepassOcclusion, view: u32| {
            occlusion.feedback_view = view;
            occlusion.tick_views()
        };
        for _ in 0..5 {
            for view in 0..3 {
                assert_eq!(tick(&mut occlusion, view), 0);
            }
        }
        for frame in 0..OCCLUSION_VIEW_RETIRE_ROUNDS {
            assert_eq!(
                tick(&mut occlusion, 0),
                0,
                "frame {frame}: views 1 and 2 still kept"
            );
        }
        assert_eq!(tick(&mut occlusion, 0), 0b110, "views 1 and 2 retire");
        // Views drawn in reverse order count one round per frame, too.
        let mut reverse = PrepassOcclusion::default();
        for _ in 0..(3 * OCCLUSION_VIEW_RETIRE_ROUNDS) {
            for view in (0..3).rev() {
                assert_eq!(tick(&mut reverse, view), 0, "every view draws each frame");
            }
        }
        // Steady single view: the bookkeeping never grows.
        let mut single = PrepassOcclusion::default();
        assert_eq!(tick(&mut single, 0), 0);
        let capacity = single.drawn.capacity();
        for _ in 0..100 {
            assert_eq!(tick(&mut single, 0), 0);
        }
        assert_eq!((single.drawn.len(), single.drawn.capacity()), (1, capacity));
        // A view past the tracked ones shares the live history and is never retired.
        assert_eq!(tick(&mut single, MAX_OCCLUSION_VIEWS as u32), 0);
    }
}
