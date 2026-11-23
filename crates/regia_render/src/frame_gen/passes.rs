// SPDX-License-Identifier: MPL-2.0
//! The GPU passes of frame generation (ADR-2067): prepare inputs, optical flow, the frame
//! interpolator. The WGSL under `shaders/` is ported from the AMD FidelityFX SDK v1.1.4 (MIT; the
//! notice is in each file, what changed in its header and in ADR-2067); this driver runs the passes
//! in the SDK's order and is Regia Engine's own.
//!
//! [`FrameGenPasses::record`] takes two presented images and the frame's depth and motion and
//! leaves an interpolated image in [`FrameGenPasses::output_view`]. It owns its targets and builds
//! them on the first use at a size, so nothing here runs or exists while frame generation is off.

use crate::frame_work::counted::*;
use std::collections::HashMap;

use regia_core::glam::{DMat4, DVec3, Mat4};

use super::{TransferFunction, optical_flow, wgsl};
use crate::RenderError;
use crate::fsr3::device_to_view_depth;
use crate::shader_lib::compose_source;
use crate::tsr::eye_relative;

/// Bytes between the entries of a dynamic uniform buffer (the largest alignment a device may ask).
const UNIFORM_STRIDE: u64 = 256;
/// Levels of the inpainting pyramid at most (the SDK's reader stops at 11).
const MAX_PYRAMID_MIPS: u32 = 11;
/// The pyramid level whose histograms detect a scene change (a quarter of the display each way).
const SCENE_LEVEL: usize = 2;
/// Words in one frame's histograms (9 sections of 16 bins).
const HISTOGRAM_WORDS: u64 = 144;

/// The image format of [`FrameGenPasses::output_view`]: 16-bit float holds every encoding the
/// display stage or a surface uses.
pub const OUTPUT_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba16Float;

/// One level of the optical flow run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlowLevelPlan {
    /// Pixels of the level.
    pub size: (u32, u32),
    /// Where its pixels start in the luma buffer, counted in pixels.
    ///
    /// Each row is padded to a multiple of four pixels ([`flow_luma_stride`]) so a row never
    /// splits a packed word. A width that is already a multiple of four keeps the tight
    /// `width * height` offsets.
    pub luma_offset: u32,
    /// Blocks of the level.
    pub blocks: (u32, u32),
}

/// Pixels per row of the packed luma buffer: `width` rounded up to a multiple of four.
#[must_use]
pub fn flow_luma_stride(width: u32) -> u32 {
    width.div_ceil(4) * 4
}

/// Bytes of the luma pyramid [`flow_levels`] describes: one byte per padded pixel.
#[must_use]
pub fn flow_luma_storage_bytes(display: (u32, u32)) -> u64 {
    flow_levels(display)
        .iter()
        .map(|level| u64::from(flow_luma_stride(level.size.0)) * u64::from(level.size.1))
        .sum()
}

/// The pyramid of the optical flow at a display size: the same levels as
/// [`optical_flow::estimate_flow`], finest first.
#[must_use]
pub fn flow_levels(display: (u32, u32)) -> Vec<FlowLevelPlan> {
    let mut size = (display.0.max(1), display.1.max(1));
    let mut out = Vec::with_capacity(optical_flow::MAX_LEVELS);
    let mut offset = 0u32;
    loop {
        out.push(FlowLevelPlan {
            size,
            luma_offset: offset,
            blocks: optical_flow::flow_field_size(size, optical_flow::BLOCK_SIZE),
        });
        offset += flow_luma_stride(size.0) * size.1;
        let next = (size.0.div_ceil(2), size.1.div_ceil(2));
        if out.len() >= optical_flow::MAX_LEVELS
            || next.0 < optical_flow::BLOCK_SIZE
            || next.1 < optical_flow::BLOCK_SIZE
        {
            return out;
        }
        size = next;
    }
}

/// How many levels the inpainting pyramid of a `size` image has: each halves (truncated) from
/// `size / 2`, down to the last with both sides above 0.
#[must_use]
pub fn pyramid_mips(size: (u32, u32)) -> u32 {
    let mut s = (size.0 / 2, size.1 / 2);
    let mut mips = 0;
    while s.0 > 0 && s.1 > 0 && mips < MAX_PYRAMID_MIPS {
        mips += 1;
        s = (s.0 / 2, s.1 / 2);
    }
    mips.max(1)
}

/// What one [`FrameGenPasses::record`] reads.
pub struct FrameGenFrame<'a> {
    /// The current presented image, without the overlay, with no sRGB decode in its view.
    pub current_color: &'a wgpu::TextureView,
    /// The previous presented image, the same way.
    pub previous_color: &'a wgpu::TextureView,
    /// The scene depth at `render_size` (single sample, standard depth).
    pub depth: &'a wgpu::TextureView,
    /// The velocity target, when the frame wrote one.
    pub velocity: Option<&'a wgpu::TextureView>,
    /// The reactive or transparency mask at the render size (red, 0 to 1), when the frame has one.
    pub reactive: Option<&'a wgpu::TextureView>,
    /// The size the scene rendered at.
    pub render_size: (u32, u32),
    /// The presented size.
    pub display_size: (u32, u32),
    /// Jittered view-projection of the frame (world space).
    pub view_proj: Mat4,
    /// Its jitter-free view-projection.
    pub unjittered_view_proj: Mat4,
    /// The previous view-projection the velocity pass used.
    pub velocity_prev_view_proj: Mat4,
    /// Camera position.
    pub eye: [f32; 3],
    /// Vertical field of view (radians).
    pub fov_y: f32,
    /// How the images are encoded.
    pub transfer: TransferFunction,
    /// `[min, max]` display luminance in cd/m² for the HDR encodings.
    pub min_max_luminance: [f32; 2],
    /// A camera cut or history reset: the output is the current image.
    pub reset: bool,
    /// Bars at the image edges (the SDK's tear-line view).
    pub debug_tear_lines: bool,
    /// The four-pane view of the internal surfaces.
    pub debug_view: bool,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FgFrame {
    render_size: [f32; 4],
    display_size: [f32; 4],
    flow_size: [f32; 4],
    depth: [f32; 4],
    tune: [f32; 4],
    debug_bar: [f32; 4],
    flags: [u32; 4],
    inv_jittered: [[f32; 4]; 4],
    cur_vp: [[f32; 4]; 4],
    prev_vp: [[f32; 4]; 4],
    velocity_prev_vp: [[f32; 4]; 4],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct LumaParams {
    src_size: [u32; 2],
    dst_size: [u32; 2],
    src_offset: u32,
    dst_offset: u32,
    transfer: u32,
    histogram_base: u32,
    pq_range: [f32; 2],
    has_history: u32,
    reset: u32,
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct FlowLevelParams {
    size: [u32; 2],
    luma_offset: u32,
    top: u32,
    blocks: [u32; 2],
    fine_blocks: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct PyramidParams {
    size: [u32; 2],
    source_size: [u32; 2],
}

struct Tex {
    /// Owned with the view so the texture drops with the pass.
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

struct Pipelines {
    luma_bgl: wgpu::BindGroupLayout,
    luma_base: wgpu::ComputePipeline,
    luma_down: wgpu::ComputePipeline,
    histogram: wgpu::ComputePipeline,
    compare: wgpu::ComputePipeline,
    flow_bgl: wgpu::BindGroupLayout,
    search: wgpu::ComputePipeline,
    filter: wgpu::ComputePipeline,
    upscale: wgpu::ComputePipeline,
    prepare_bgl: wgpu::BindGroupLayout,
    prepare: wgpu::ComputePipeline,
    reconstruct_bgl: wgpu::BindGroupLayout,
    reconstruct: wgpu::ComputePipeline,
    game_bgl: wgpu::BindGroupLayout,
    game: wgpu::ComputePipeline,
    /// Y word of the game vector that won the X word. Dispatched after `game`.
    game_y: wgpu::ComputePipeline,
    flow_field_bgl: wgpu::BindGroupLayout,
    flow_field: wgpu::ComputePipeline,
    /// Y word of the flow vector that won the X word. Dispatched after `flow_field`.
    flow_field_y: wgpu::ComputePipeline,
    disocclusion_bgl: wgpu::BindGroupLayout,
    disocclusion: wgpu::ComputePipeline,
    pyramid_bgl: wgpu::BindGroupLayout,
    game_first: wgpu::ComputePipeline,
    game_down: wgpu::ComputePipeline,
    color_first: wgpu::ComputePipeline,
    color_down: wgpu::ComputePipeline,
    interpolate_bgl: wgpu::BindGroupLayout,
    interpolate: wgpu::ComputePipeline,
    inpaint_bgl: wgpu::BindGroupLayout,
    inpaint: wgpu::ComputePipeline,
    debug_bgl: wgpu::BindGroupLayout,
    debug: wgpu::ComputePipeline,
    sampler: wgpu::Sampler,
    /// 1 × 1 zero texture for a missing velocity or reactive input and the unused pyramid source.
    dummy: Tex,
    params: wgpu::Buffer,
}

/// The bind groups of one pyramid family: one per mip.
struct PyramidGroups {
    first: wgpu::BindGroup,
    down: Vec<wgpu::BindGroup>,
    first_offset: u32,
}

/// Everything sized by the render and display sizes.
struct Targets {
    render: (u32, u32),
    display: (u32, u32),
    levels: Vec<FlowLevelPlan>,
    /// The luma pyramids of the two newest frames.
    luma: [wgpu::Buffer; 2],
    histograms: wgpu::Buffer,
    state: wgpu::Buffer,
    filtered: wgpu::Buffer,
    motion: Tex,
    reconstructed_previous: wgpu::Buffer,
    reconstructed_interpolated: wgpu::Buffer,
    game_x: wgpu::Buffer,
    game_y: wgpu::Buffer,
    flow_x: wgpu::Buffer,
    flow_y: wgpu::Buffer,
    mask: Tex,
    pyramid: Tex,
    render_mips: u32,
    display_mips: u32,
    interpolated: Tex,
    output: Tex,
    /// Per-frame luma entries, kept so a frame allocates nothing.
    luma_entries: Vec<u8>,
    flow_groups: [[wgpu::BindGroup; 3]; 2],
    reconstruct_group: wgpu::BindGroup,
    disocclusion_group: wgpu::BindGroup,
    inpaint_group: wgpu::BindGroup,
    debug_group: wgpu::BindGroup,
    game_pyramid: PyramidGroups,
    color_pyramid: PyramidGroups,
}

/// The bind groups that hold the frame's own images.
#[derive(Clone)]
struct ExternalGroups {
    luma: wgpu::BindGroup,
    prepare: wgpu::BindGroup,
    game: wgpu::BindGroup,
    flow_field: wgpu::BindGroup,
    interpolate: wgpu::BindGroup,
}

/// The current and previous image, depth, velocity, reactive mask and the luma parity.
type GroupKey = (
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    usize,
);

/// The frame uniform's place in the parameter buffer.
const FRAME_OFFSET: u64 = 0;
/// The luma entries start here: base, one per downsample, histogram and compare in one entry.
const LUMA_OFFSET: u64 = 2 * UNIFORM_STRIDE;
/// The parameter buffer: frame, luma, flow levels and two pyramid families.
const PARAMS_BYTES: u64 = 2 * UNIFORM_STRIDE
    + (optical_flow::MAX_LEVELS as u64 + 1 + optical_flow::MAX_LEVELS as u64 + 2 * MAX_PYRAMID_MIPS as u64)
        * UNIFORM_STRIDE;
const fn luma_entries(levels: usize) -> u64 {
    levels as u64 + 1
}

/// The pipelines, targets and history of the interpolator. One per presented window.
#[derive(Default)]
pub struct FrameGenPasses {
    pipes: Option<Pipelines>,
    targets: Option<Targets>,
    groups: crate::bind_cache::BindCache<GroupKey, ExternalGroups>,
    /// Which luma buffer this frame writes.
    parity: usize,
    /// The other luma buffer and the histograms hold the previous frame at the current sizes.
    history_valid: bool,
    /// The previous frame's jitter-free view-projection.
    prev_unjittered: Option<DMat4>,
    /// Cycles the debug bar's color.
    debug_bar: u32,
    /// Frames recorded (`render.frame_gen`).
    frames: u64,
}

impl FrameGenPasses {
    /// The interpolated image of the last [`Self::record`] that generated.
    #[must_use]
    pub fn output_view(&self) -> Option<&wgpu::TextureView> {
        self.targets.as_ref().map(|t| &t.output.view)
    }

    /// The texture behind [`Self::output_view`] (a copy source).
    #[must_use]
    pub fn output_texture(&self) -> Option<&wgpu::Texture> {
        self.targets.as_ref().map(|t| &t.output.texture)
    }

    /// The finest flow field of the last frame: `vec2<i32>` per block, row-major, in pixels toward
    /// the previous position (a copy source), with its blocks per side.
    #[must_use]
    pub fn flow_field(&self) -> Option<(&wgpu::Buffer, (u32, u32))> {
        let t = self.targets.as_ref()?;
        Some((&t.filtered, t.levels[0].blocks))
    }

    /// The last frame's dilated motion at render size (`Rgba32Float`, a copy source): `xy` the
    /// motion toward the previous frame in UV units, `z` the nearest device depth around the
    /// texel. The input of every later pass, for readback in diagnostics and tests.
    #[must_use]
    pub fn dilated_motion_texture(&self) -> Option<&wgpu::Texture> {
        self.targets.as_ref().map(|t| &t.motion.texture)
    }

    /// `[scene history bits, frames since reset]`: two `u32` (a copy source).
    #[must_use]
    pub fn state_buffer(&self) -> Option<&wgpu::Buffer> {
        self.targets.as_ref().map(|t| &t.state)
    }

    /// Frames recorded since creation.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Forgets the history: the next frame interpolates from nothing and copies its image.
    pub fn invalidate(&mut self) {
        self.history_valid = false;
        self.prev_unjittered = None;
    }

    /// Builds the pipelines (a settings change, not a frame). `Err` keeps frame generation off.
    ///
    /// # Errors
    /// A shader fails to compile, or the device refuses a pipeline.
    pub fn prepare_pipelines(&mut self, device: &wgpu::Device) -> Result<(), RenderError> {
        if self.pipes.is_none() {
            let scope = device.push_error_scope(wgpu::ErrorFilter::Validation);
            let built = build_pipelines(device);
            let error = regia_core::tasks::block_on(scope.pop());
            if let Some(error) = error {
                return Err(RenderError::Shader(format!(
                    "frame generation pipelines: {error}"
                )));
            }
            self.pipes = Some(built?);
        }
        Ok(())
    }

    fn ensure(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        render: (u32, u32),
        display: (u32, u32),
    ) -> Result<(), RenderError> {
        self.prepare_pipelines(device)?;
        let stale = self
            .targets
            .as_ref()
            .is_none_or(|t| t.render != render || t.display != display);
        if stale {
            let Some(pipes) = self.pipes.as_ref() else {
                return Err(RenderError::Shader("frame generation pipelines missing".into()));
            };
            let scope = device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
            let targets = build_targets(device, queue, pipes, render, display);
            if let Some(error) = regia_core::tasks::block_on(scope.pop()) {
                return Err(RenderError::Shader(format!("frame generation targets: {error}")));
            }
            self.targets = Some(targets);
            self.groups.clear();
            self.invalidate();
        }
        Ok(())
    }

    /// Records one frame. `generate` runs the whole interpolator; without it only the luma pyramid
    /// and the scene-change test run, which the next frame's interpolation reads.
    ///
    /// # Errors
    /// The pipelines or targets could not be built.
    pub fn record(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        input: &FrameGenFrame<'_>,
        generate: bool,
    ) -> Result<(), RenderError> {
        let _span = tracing::info_span!("frame_gen_passes").entered();
        let _crumb = crate::breadcrumbs::frame_pass("frame_gen_passes");
        let device = &crate::device::TrackedDevice::observe(device);
        let render = (input.render_size.0.max(1), input.render_size.1.max(1));
        let display = (input.display_size.0.max(1), input.display_size.1.max(1));
        self.ensure(device, queue, render, display)?;
        // No history behaves like a reset on every side: the interpolator copies this image.
        let reset = input.reset || !self.history_valid;
        let parity = self.parity;
        let (Some(pipes), Some(targets)) = (self.pipes.as_ref(), self.targets.as_mut()) else {
            return Err(RenderError::Shader("frame generation resources missing".into()));
        };

        let frame = frame_uniform(
            input,
            render,
            display,
            targets,
            reset,
            self.prev_unjittered,
            self.debug_bar,
        );
        queue.write_buffer(&pipes.params, FRAME_OFFSET, bytemuck::bytes_of(&frame));
        write_luma_entries(queue, pipes, targets, input, parity, reset, self.history_valid);

        let histogram_base = parity as u64 * HISTOGRAM_WORDS * 4;
        encoder.clear_buffer(&targets.histograms, histogram_base, Some(HISTOGRAM_WORDS * 4));
        if generate {
            encoder.clear_buffer(&targets.reconstructed_previous, 0, None);
            encoder.clear_buffer(&targets.reconstructed_interpolated, 0, None);
            encoder.clear_buffer(&targets.game_x, 0, None);
            encoder.clear_buffer(&targets.game_y, 0, None);
            encoder.clear_buffer(&targets.flow_x, 0, None);
            encoder.clear_buffer(&targets.flow_y, 0, None);
        }

        let velocity = input.velocity.unwrap_or(&pipes.dummy.view).clone();
        let reactive = input.reactive.unwrap_or(&pipes.dummy.view).clone();
        let key = (
            input.current_color.clone(),
            input.previous_color.clone(),
            input.depth.clone(),
            velocity.clone(),
            reactive.clone(),
            parity,
        );
        let groups = self.groups.get_or_insert_with(key, || {
            build_external_groups(device, pipes, targets, input, &velocity, &reactive, parity)
        });

        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("frame_gen"),
                timestamp_writes: None,
            });
            record_luma(&mut pass, pipes, targets, &groups);
            if generate {
                record_flow(&mut pass, pipes, targets, parity);
                record_interpolation(&mut pass, pipes, targets, &groups, reset, input.debug_view);
            }
        }
        self.parity = 1 - parity;
        self.history_valid = true;
        self.prev_unjittered = Some(input.unjittered_view_proj.as_dmat4());
        self.debug_bar = self.debug_bar.wrapping_add(1);
        self.frames += 1;
        Ok(())
    }
}

fn groups_of(n: u32) -> u32 {
    n.div_ceil(8).max(1)
}

fn record_luma(
    pass: &mut wgpu::ComputePass<'_>,
    pipes: &Pipelines,
    targets: &Targets,
    groups: &ExternalGroups,
) {
    let mut offset = LUMA_OFFSET;
    let finest = targets.levels[0].size;
    pass.counted_set_pipeline(&pipes.luma_base);
    pass.counted_set_bind_group(0, &groups.luma, &[offset as u32]);
    pass.counted_dispatch(groups_of(finest.0), groups_of(finest.1), 1);
    pass.counted_set_pipeline(&pipes.luma_down);
    for level in &targets.levels[1..] {
        offset += UNIFORM_STRIDE;
        pass.counted_set_bind_group(0, &groups.luma, &[offset as u32]);
        pass.counted_dispatch(groups_of(level.size.0), groups_of(level.size.1), 1);
    }
    let scene = targets.levels[SCENE_LEVEL.min(targets.levels.len() - 1)];
    offset += UNIFORM_STRIDE;
    pass.counted_set_pipeline(&pipes.histogram);
    pass.counted_set_bind_group(0, &groups.luma, &[offset as u32]);
    pass.counted_dispatch(groups_of(scene.size.0), groups_of(scene.size.1), 1);
    pass.counted_set_pipeline(&pipes.compare);
    pass.counted_dispatch(1, 1, 1);
}

fn flow_offset(levels: &[FlowLevelPlan], level: usize) -> u32 {
    (LUMA_OFFSET + (luma_entries(levels.len()) + level as u64) * UNIFORM_STRIDE) as u32
}

fn record_flow(pass: &mut wgpu::ComputePass<'_>, pipes: &Pipelines, targets: &Targets, parity: usize) {
    let [search, filter, upscale] = &targets.flow_groups[parity];
    for index in (0..targets.levels.len()).rev() {
        let level = targets.levels[index];
        let offset = flow_offset(&targets.levels, index);
        pass.counted_set_pipeline(&pipes.search);
        pass.counted_set_bind_group(0, search, &[offset]);
        pass.counted_dispatch(groups_of(level.blocks.0), groups_of(level.blocks.1), 1);
        pass.counted_set_pipeline(&pipes.filter);
        pass.counted_set_bind_group(0, filter, &[offset]);
        pass.counted_dispatch(groups_of(level.blocks.0), groups_of(level.blocks.1), 1);
        if index > 0 {
            let fine = targets.levels[index - 1].blocks;
            pass.counted_set_pipeline(&pipes.upscale);
            pass.counted_set_bind_group(0, upscale, &[offset]);
            pass.counted_dispatch(groups_of(fine.0), groups_of(fine.1), 1);
        }
    }
}

fn record_pyramid(
    pass: &mut wgpu::ComputePass<'_>,
    first: &wgpu::ComputePipeline,
    down: &wgpu::ComputePipeline,
    groups: &PyramidGroups,
    mips: u32,
    base: (u32, u32),
) {
    let mut size = (base.0 / 2, base.1 / 2);
    let mut offset = groups.first_offset;
    pass.counted_set_pipeline(first);
    pass.counted_set_bind_group(0, &groups.first, &[offset]);
    pass.counted_dispatch(groups_of(size.0), groups_of(size.1), 1);
    pass.counted_set_pipeline(down);
    for group in groups.down.iter().take(mips.saturating_sub(1) as usize) {
        size = (size.0 / 2, size.1 / 2);
        offset += UNIFORM_STRIDE as u32;
        pass.counted_set_bind_group(0, group, &[offset]);
        pass.counted_dispatch(groups_of(size.0), groups_of(size.1), 1);
    }
}

fn record_interpolation(
    pass: &mut wgpu::ComputePass<'_>,
    pipes: &Pipelines,
    targets: &Targets,
    groups: &ExternalGroups,
    reset: bool,
    debug_view: bool,
) {
    let (rw, rh) = (groups_of(targets.render.0), groups_of(targets.render.1));
    let (dw, dh) = (groups_of(targets.display.0), groups_of(targets.display.1));
    pass.counted_set_pipeline(&pipes.prepare);
    pass.counted_set_bind_group(0, &groups.prepare, &[]);
    pass.counted_dispatch(rw, rh, 1);
    // A reset copies the image: the SDK skips the preparation passes then.
    if !reset {
        pass.counted_set_pipeline(&pipes.reconstruct);
        pass.counted_set_bind_group(0, &targets.reconstruct_group, &[]);
        pass.counted_dispatch(rw, rh, 1);
        pass.counted_set_pipeline(&pipes.game);
        pass.counted_set_bind_group(0, &groups.game, &[]);
        pass.counted_dispatch(rw, rh, 1);
        // The Y word is chosen only after every X `atomicMax` is visible. A second dispatch
        // of the same samples; the pyramid below reads both words.
        pass.counted_set_pipeline(&pipes.game_y);
        pass.counted_set_bind_group(0, &groups.game, &[]);
        pass.counted_dispatch(rw, rh, 1);
        record_pyramid(
            pass,
            &pipes.game_first,
            &pipes.game_down,
            &targets.game_pyramid,
            targets.render_mips,
            targets.render,
        );
        let flow = targets.levels[0].blocks;
        pass.counted_set_pipeline(&pipes.flow_field);
        pass.counted_set_bind_group(0, &groups.flow_field, &[]);
        pass.counted_dispatch(groups_of(flow.0), groups_of(flow.1), 1);
        pass.counted_set_pipeline(&pipes.flow_field_y);
        pass.counted_set_bind_group(0, &groups.flow_field, &[]);
        pass.counted_dispatch(groups_of(flow.0), groups_of(flow.1), 1);
        pass.counted_set_pipeline(&pipes.disocclusion);
        pass.counted_set_bind_group(0, &targets.disocclusion_group, &[]);
        pass.counted_dispatch(rw, rh, 1);
    }
    pass.counted_set_pipeline(&pipes.interpolate);
    pass.counted_set_bind_group(0, &groups.interpolate, &[]);
    pass.counted_dispatch(dw, dh, 1);
    record_pyramid(
        pass,
        &pipes.color_first,
        &pipes.color_down,
        &targets.color_pyramid,
        targets.display_mips,
        targets.display,
    );
    if debug_view {
        pass.counted_set_pipeline(&pipes.debug);
        pass.counted_set_bind_group(0, &targets.debug_group, &[]);
    } else {
        pass.counted_set_pipeline(&pipes.inpaint);
        pass.counted_set_bind_group(0, &targets.inpaint_group, &[]);
    }
    pass.counted_dispatch(dw, dh, 1);
}

/// The SDK's debug bar sequence: white, red, green, blue.
fn debug_bar_color(index: u32) -> [f32; 4] {
    match index % 4 {
        0 => [1.0, 1.0, 1.0, 0.0],
        1 => [1.0, 0.0, 0.0, 0.0],
        2 => [0.0, 1.0, 0.0, 0.0],
        _ => [0.0, 0.0, 1.0, 0.0],
    }
}

fn frame_uniform(
    input: &FrameGenFrame<'_>,
    render: (u32, u32),
    display: (u32, u32),
    targets: &Targets,
    reset: bool,
    prev_unjittered: Option<DMat4>,
    debug_bar: u32,
) -> FgFrame {
    let cur_world = input.unjittered_view_proj.as_dmat4();
    let prev_world = prev_unjittered.unwrap_or(cur_world);
    let eye = DVec3::new(
        f64::from(input.eye[0]),
        f64::from(input.eye[1]),
        f64::from(input.eye[2]),
    );
    let to_eye = DMat4::from_translation(eye);
    let cur_eye = cur_world * to_eye;
    let inv_jittered = eye_relative(input.view_proj, input.eye).inverse();
    let [c0, c1] = device_to_view_depth(cur_eye);
    // The source's `a` and `b`: view x and y per NDC per view depth.
    let tan_half = (input.fov_y.max(1.0e-4) * 0.5).tan();
    let aspect = render.0 as f32 / render.1 as f32;
    let flow = targets.levels[0].blocks;
    let mut flags = 0;
    if input.debug_tear_lines {
        flags |= 1;
    }
    if input.debug_view {
        flags |= 4;
    }
    FgFrame {
        render_size: [
            render.0 as f32,
            render.1 as f32,
            1.0 / render.0 as f32,
            1.0 / render.1 as f32,
        ],
        display_size: [
            display.0 as f32,
            display.1 as f32,
            1.0 / display.0 as f32,
            1.0 / display.1 as f32,
        ],
        flow_size: [
            flow.0 as f32,
            flow.1 as f32,
            1.0 / display.0 as f32,
            1.0 / display.1 as f32,
        ],
        depth: [c0, c1, tan_half * aspect, tan_half],
        tune: [
            tan_half * aspect,
            input.min_max_luminance[0],
            input.min_max_luminance[1].max(1.0e-3),
            if input.reactive.is_some() { 1.0 } else { 0.0 },
        ],
        debug_bar: debug_bar_color(debug_bar),
        flags: [transfer_code(input.transfer), u32::from(reset), flags, 0],
        inv_jittered: inv_jittered.as_mat4().to_cols_array_2d(),
        cur_vp: cur_eye.as_mat4().to_cols_array_2d(),
        prev_vp: (prev_world * to_eye).as_mat4().to_cols_array_2d(),
        velocity_prev_vp: (input.velocity_prev_view_proj.as_dmat4() * to_eye)
            .as_mat4()
            .to_cols_array_2d(),
    }
}

const fn transfer_code(transfer: TransferFunction) -> u32 {
    match transfer {
        TransferFunction::Srgb => 0,
        TransferFunction::Pq => 1,
        TransferFunction::ScRgb => 2,
    }
}

/// Writes this frame's luma entries: the sizes do not change, the histogram side and flags do.
fn write_luma_entries(
    queue: &wgpu::Queue,
    pipes: &Pipelines,
    targets: &mut Targets,
    input: &FrameGenFrame<'_>,
    parity: usize,
    reset: bool,
    has_history: bool,
) {
    let common = LumaParams {
        src_size: [0; 2],
        dst_size: [0; 2],
        src_offset: 0,
        dst_offset: 0,
        transfer: transfer_code(input.transfer),
        histogram_base: parity as u32 * HISTOGRAM_WORDS as u32,
        pq_range: [
            optical_flow::pq_encode(input.min_max_luminance[0]),
            optical_flow::pq_encode(input.min_max_luminance[1]),
        ],
        has_history: u32::from(has_history),
        reset: u32::from(reset),
    };
    let levels = &targets.levels;
    let scene = levels[SCENE_LEVEL.min(levels.len() - 1)];
    let bytes = &mut targets.luma_entries;
    bytes.fill(0);
    let mut put = |index: usize, params: LumaParams| {
        let start = index * UNIFORM_STRIDE as usize;
        bytes[start..start + std::mem::size_of::<LumaParams>()].copy_from_slice(bytemuck::bytes_of(&params));
    };
    put(
        0,
        LumaParams {
            dst_size: [levels[0].size.0, levels[0].size.1],
            ..common
        },
    );
    for (i, level) in levels.iter().enumerate().skip(1) {
        let src = levels[i - 1];
        put(
            i,
            LumaParams {
                src_size: [src.size.0, src.size.1],
                dst_size: [level.size.0, level.size.1],
                src_offset: src.luma_offset,
                dst_offset: level.luma_offset,
                ..common
            },
        );
    }
    put(
        levels.len(),
        LumaParams {
            src_size: [scene.size.0, scene.size.1],
            src_offset: scene.luma_offset,
            ..common
        },
    );
    queue.write_buffer(&pipes.params, LUMA_OFFSET, bytes);
}

fn build_targets(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    pipes: &Pipelines,
    render: (u32, u32),
    display: (u32, u32),
) -> Targets {
    use wgpu::TextureFormat as F;
    let levels = flow_levels(display);
    let luma_bytes = flow_luma_storage_bytes(display);
    let buffer = |label: &str, size: u64, copy_src: bool| {
        let extra = if copy_src {
            wgpu::BufferUsages::COPY_SRC
        } else {
            wgpu::BufferUsages::empty()
        };
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: size.max(16),
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST | extra,
            mapped_at_creation: false,
        })
    };
    let render_words = u64::from(render.0) * u64::from(render.1);
    let blocks = levels[0].blocks;
    let block_count = u64::from(blocks.0) * u64::from(blocks.1);
    let luma = [
        buffer("frame_gen luma 0", luma_bytes, false),
        buffer("frame_gen luma 1", luma_bytes, false),
    ];
    let histograms = buffer("frame_gen histograms", HISTOGRAM_WORDS * 8, false);
    let state = buffer("frame_gen state", 8, true);
    let guess = buffer("frame_gen flow guess", block_count * 8, false);
    let raw = buffer("frame_gen flow raw", block_count * 8, false);
    let filtered = buffer("frame_gen flow", block_count * 8, true);
    let reconstructed_previous = buffer("frame_gen reconstructed previous depth", render_words * 4, false);
    let reconstructed_interpolated = buffer("frame_gen interpolated depth", render_words * 4, false);
    let game_x = buffer("frame_gen game field x", render_words * 4, false);
    let game_y = buffer("frame_gen game field y", render_words * 4, false);
    let flow_x = buffer("frame_gen flow field x", block_count * 4, false);
    let flow_y = buffer("frame_gen flow field y", block_count * 4, false);
    let motion = target(device, render, F::Rgba32Float, "frame_gen dilated motion", 1);
    let mask = target(device, render, F::Rgba8Unorm, "frame_gen disocclusion", 1);
    let display_mips = pyramid_mips(display);
    let render_mips = pyramid_mips(render).min(display_mips);
    let half_display = ((display.0 / 2).max(1), (display.1 / 2).max(1));
    let pyramid = target(
        device,
        half_display,
        OUTPUT_FORMAT,
        "frame_gen inpainting pyramid",
        display_mips,
    );
    let interpolated = target(device, display, OUTPUT_FORMAT, "frame_gen interpolated", 1);
    let output = target(device, display, OUTPUT_FORMAT, "frame_gen output", 1);

    let bind = |layout: &wgpu::BindGroupLayout, label: &str, entries: &[wgpu::BindGroupEntry<'_>]| {
        crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries,
        })
    };
    let flow_size = wgpu::BufferSize::new(std::mem::size_of::<FlowLevelParams>() as u64);
    let flow_params = wgpu::BufferBinding {
        buffer: &pipes.params,
        offset: 0,
        size: flow_size,
    };
    let flow_group =
        |current: &wgpu::Buffer, previous: &wgpu::Buffer, input: &wgpu::Buffer, out: &wgpu::Buffer| {
            bind(
                &pipes.flow_bgl,
                "frame_gen flow",
                &[
                    entry_buffer_binding(0, flow_params.clone()),
                    entry_buffer(1, current),
                    entry_buffer(2, previous),
                    entry_buffer(3, input),
                    entry_buffer(4, out),
                    entry_buffer(5, &state),
                ],
            )
        };
    let flow_side = |current: &wgpu::Buffer, previous: &wgpu::Buffer| {
        [
            flow_group(current, previous, &guess, &raw),
            flow_group(current, previous, &raw, &filtered),
            flow_group(current, previous, &filtered, &guess),
        ]
    };
    let flow_groups = [flow_side(&luma[0], &luma[1]), flow_side(&luma[1], &luma[0])];

    let frame_binding = wgpu::BufferBinding {
        buffer: &pipes.params,
        offset: FRAME_OFFSET,
        size: wgpu::BufferSize::new(std::mem::size_of::<FgFrame>() as u64),
    };
    let reconstruct_group = bind(
        &pipes.reconstruct_bgl,
        "frame_gen reconstruct depth",
        &[
            entry_buffer_binding(0, frame_binding.clone()),
            entry_view(1, &motion.view),
            entry_buffer(2, &reconstructed_interpolated),
        ],
    );
    let disocclusion_group = bind(
        &pipes.disocclusion_bgl,
        "frame_gen disocclusion",
        &[
            entry_buffer_binding(0, frame_binding.clone()),
            entry_view(1, &motion.view),
            entry_buffer(2, &reconstructed_previous),
            entry_buffer(3, &reconstructed_interpolated),
            entry_buffer(4, &game_x),
            entry_buffer(5, &game_y),
            entry_view(6, &pyramid.view),
            entry_view(7, &mask.view),
        ],
    );
    let inpaint_group = bind(
        &pipes.inpaint_bgl,
        "frame_gen inpaint",
        &[
            entry_buffer_binding(0, frame_binding.clone()),
            entry_buffer(1, &state),
            entry_view(2, &interpolated.view),
            entry_view(3, &pyramid.view),
            entry_view(4, &output.view),
        ],
    );
    let debug_group = bind(
        &pipes.debug_bgl,
        "frame_gen debug",
        &[
            entry_buffer_binding(0, frame_binding.clone()),
            entry_buffer(1, &game_x),
            entry_buffer(2, &game_y),
            entry_buffer(3, &flow_x),
            entry_buffer(4, &flow_y),
            entry_view(5, &mask.view),
            entry_view(6, &interpolated.view),
            wgpu::BindGroupEntry {
                binding: 7,
                resource: wgpu::BindingResource::Sampler(&pipes.sampler),
            },
            entry_view(8, &output.view),
        ],
    );

    // The dynamic entries: luma (rewritten per frame), then flow levels, then two pyramid families.
    let flow_base = LUMA_OFFSET + luma_entries(levels.len()) * UNIFORM_STRIDE;
    let top = levels.len() - 1;
    let mut static_entries = vec![0u8; (levels.len() as u64 * UNIFORM_STRIDE) as usize];
    for (i, level) in levels.iter().enumerate() {
        let fine = if i > 0 { levels[i - 1].blocks } else { level.blocks };
        let params = FlowLevelParams {
            size: [level.size.0, level.size.1],
            luma_offset: level.luma_offset,
            top: u32::from(i == top),
            blocks: [level.blocks.0, level.blocks.1],
            fine_blocks: [fine.0, fine.1],
        };
        let start = i * UNIFORM_STRIDE as usize;
        static_entries[start..start + std::mem::size_of::<FlowLevelParams>()]
            .copy_from_slice(bytemuck::bytes_of(&params));
    }
    let pyramid_base = flow_base + levels.len() as u64 * UNIFORM_STRIDE;
    let mip_views: Vec<wgpu::TextureView> = (0..display_mips)
        .map(|mip| {
            pyramid.texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("frame_gen pyramid mip"),
                base_mip_level: mip,
                mip_level_count: Some(1),
                ..Default::default()
            })
        })
        .collect();
    let pyramid_params = wgpu::BufferBinding {
        buffer: &pipes.params,
        offset: 0,
        size: wgpu::BufferSize::new(std::mem::size_of::<PyramidParams>() as u64),
    };
    let family = |first_source: &wgpu::TextureView, label: &str, first_offset: u64| PyramidGroups {
        first: bind(
            &pipes.pyramid_bgl,
            label,
            &[
                entry_buffer_binding(0, pyramid_params.clone()),
                entry_view(1, first_source),
                entry_view(2, &mip_views[0]),
                entry_buffer(3, &game_x),
                entry_buffer(4, &game_y),
            ],
        ),
        down: mip_views
            .windows(2)
            .map(|pair| {
                bind(
                    &pipes.pyramid_bgl,
                    label,
                    &[
                        entry_buffer_binding(0, pyramid_params.clone()),
                        entry_view(1, &pair[0]),
                        entry_view(2, &pair[1]),
                        entry_buffer(3, &game_x),
                        entry_buffer(4, &game_y),
                    ],
                )
            })
            .collect(),
        first_offset: first_offset as u32,
    };
    let game_pyramid = family(&pipes.dummy.view, "frame_gen game pyramid", pyramid_base);
    let color_base = pyramid_base + u64::from(MAX_PYRAMID_MIPS) * UNIFORM_STRIDE;
    let color_pyramid = family(&interpolated.view, "frame_gen color pyramid", color_base);
    let mut pyramid_bytes = vec![0u8; (u64::from(MAX_PYRAMID_MIPS) * 2 * UNIFORM_STRIDE) as usize];
    for (family_index, base) in [render, display].into_iter().enumerate() {
        let mut source = base;
        for mip in 0..MAX_PYRAMID_MIPS as usize {
            let size = (source.0 / 2, source.1 / 2);
            let params = PyramidParams {
                size: [size.0, size.1],
                source_size: [source.0, source.1],
            };
            let start = (family_index * MAX_PYRAMID_MIPS as usize + mip) * UNIFORM_STRIDE as usize;
            pyramid_bytes[start..start + std::mem::size_of::<PyramidParams>()]
                .copy_from_slice(bytemuck::bytes_of(&params));
            source = size;
        }
    }

    queue.write_buffer(&pipes.params, flow_base, &static_entries);
    queue.write_buffer(&pipes.params, pyramid_base, &pyramid_bytes);
    let luma_entries_len = (luma_entries(levels.len()) * UNIFORM_STRIDE) as usize;
    Targets {
        render,
        display,
        levels,
        luma,
        histograms,
        state,
        filtered,
        motion,
        reconstructed_previous,
        reconstructed_interpolated,
        game_x,
        game_y,
        flow_x,
        flow_y,
        mask,
        pyramid,
        render_mips,
        display_mips,
        interpolated,
        output,
        luma_entries: vec![0; luma_entries_len],
        flow_groups,
        reconstruct_group,
        disocclusion_group,
        inpaint_group,
        debug_group,
        game_pyramid,
        color_pyramid,
    }
}

fn build_external_groups(
    device: &wgpu::Device,
    pipes: &Pipelines,
    targets: &Targets,
    input: &FrameGenFrame<'_>,
    velocity: &wgpu::TextureView,
    reactive: &wgpu::TextureView,
    parity: usize,
) -> ExternalGroups {
    let bind = |layout: &wgpu::BindGroupLayout, label: &str, entries: &[wgpu::BindGroupEntry<'_>]| {
        crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries,
        })
    };
    let frame = || wgpu::BufferBinding {
        buffer: &pipes.params,
        offset: FRAME_OFFSET,
        size: wgpu::BufferSize::new(std::mem::size_of::<FgFrame>() as u64),
    };
    let sampler = |binding| wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::Sampler(&pipes.sampler),
    };
    ExternalGroups {
        luma: bind(
            &pipes.luma_bgl,
            "frame_gen luma",
            &[
                entry_buffer_binding(
                    0,
                    wgpu::BufferBinding {
                        buffer: &pipes.params,
                        offset: 0,
                        size: wgpu::BufferSize::new(std::mem::size_of::<LumaParams>() as u64),
                    },
                ),
                entry_view(1, input.current_color),
                entry_buffer(2, &targets.luma[parity]),
                entry_buffer(3, &targets.histograms),
                entry_buffer(4, &targets.state),
            ],
        ),
        prepare: bind(
            &pipes.prepare_bgl,
            "frame_gen prepare",
            &[
                entry_buffer_binding(0, frame()),
                entry_view(1, input.depth),
                entry_view(2, velocity),
                entry_view(3, &targets.motion.view),
                entry_buffer(4, &targets.reconstructed_previous),
            ],
        ),
        game: bind(
            &pipes.game_bgl,
            "frame_gen game field",
            &[
                entry_buffer_binding(0, frame()),
                entry_view(1, &targets.motion.view),
                entry_view(2, input.previous_color),
                entry_view(3, input.current_color),
                sampler(4),
                entry_buffer(5, &targets.game_x),
                entry_buffer(6, &targets.game_y),
            ],
        ),
        flow_field: bind(
            &pipes.flow_field_bgl,
            "frame_gen flow field",
            &[
                entry_buffer_binding(0, frame()),
                entry_buffer(1, &targets.filtered),
                entry_view(2, input.previous_color),
                entry_view(3, input.current_color),
                sampler(4),
                entry_buffer(5, &targets.flow_x),
                entry_buffer(6, &targets.flow_y),
            ],
        ),
        interpolate: bind(
            &pipes.interpolate_bgl,
            "frame_gen interpolate",
            &[
                entry_buffer_binding(0, frame()),
                entry_buffer(1, &targets.state),
                entry_view(2, input.previous_color),
                entry_view(3, input.current_color),
                entry_buffer(4, &targets.game_x),
                entry_buffer(5, &targets.game_y),
                entry_buffer(6, &targets.flow_x),
                entry_buffer(7, &targets.flow_y),
                entry_view(8, &targets.pyramid.view),
                entry_view(9, &targets.mask.view),
                entry_view(10, reactive),
                sampler(11),
                entry_view(12, &targets.interpolated.view),
            ],
        ),
    }
}

fn entry_buffer(binding: u32, buffer: &wgpu::Buffer) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: buffer.as_entire_binding(),
    }
}

fn entry_buffer_binding(binding: u32, buffer: wgpu::BufferBinding<'_>) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::Buffer(buffer),
    }
}

fn entry_view(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

fn target(
    device: &wgpu::Device,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    label: &str,
    mips: u32,
) -> Tex {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0.max(1),
            height: size.1.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: mips,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::STORAGE_BINDING
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Tex { texture, view }
}

fn layout_entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    }
}

fn uniform(binding: u32, dynamic: bool) -> wgpu::BindGroupLayoutEntry {
    layout_entry(
        binding,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: dynamic,
            min_binding_size: None,
        },
    )
}

fn storage(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    layout_entry(
        binding,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
    )
}

fn sampled(binding: u32, filterable: bool) -> wgpu::BindGroupLayoutEntry {
    layout_entry(
        binding,
        wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
    )
}

fn depth_texture(binding: u32) -> wgpu::BindGroupLayoutEntry {
    layout_entry(
        binding,
        wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Depth,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
    )
}

fn storage_texture(binding: u32, format: wgpu::TextureFormat) -> wgpu::BindGroupLayoutEntry {
    layout_entry(
        binding,
        wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
    )
}

fn filtering_sampler(binding: u32) -> wgpu::BindGroupLayoutEntry {
    layout_entry(
        binding,
        wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
    )
}

/// One module, one bind group layout and the pipelines of the named entry points.
fn pipelines_of<const N: usize>(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    entries: &[wgpu::BindGroupLayoutEntry],
    entry_points: [&str; N],
) -> Result<(wgpu::BindGroupLayout, [wgpu::ComputePipeline; N]), RenderError> {
    let module = compose_source(source, label, HashMap::new())?;
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
    });
    let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some(label),
        entries,
    });
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(&bgl)],
        immediate_size: 0,
    });
    let pipelines = entry_points.map(|entry_point| {
        device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
            label: Some(label),
            layout: Some(&layout),
            module: &module,
            entry_point: Some(entry_point),
            compilation_options: Default::default(),
            cache: crate::pipeline_cache::cache_for(device).as_deref(),
        })
    });
    Ok((bgl, pipelines))
}

fn build_pipelines(device: &wgpu::Device) -> Result<Pipelines, RenderError> {
    use wgpu::TextureFormat as F;
    let (luma_bgl, [luma_base, luma_down, histogram, compare]) = pipelines_of(
        device,
        "frame_gen luma",
        wgsl::FLOW_LUMA,
        &[
            uniform(0, true),
            sampled(1, true),
            storage(2, false),
            storage(3, false),
            storage(4, false),
        ],
        ["cs_base", "cs_down", "cs_histogram", "cs_compare"],
    )?;
    let (flow_bgl, [search, filter, upscale]) = pipelines_of(
        device,
        "frame_gen flow",
        wgsl::FLOW,
        &[
            uniform(0, true),
            storage(1, true),
            storage(2, true),
            storage(3, true),
            storage(4, false),
            storage(5, true),
        ],
        ["cs_search", "cs_filter", "cs_upscale"],
    )?;
    let (prepare_bgl, [prepare]) = pipelines_of(
        device,
        "frame_gen prepare",
        &wgsl::with_common(wgsl::PREPARE),
        &[
            uniform(0, false),
            depth_texture(1),
            sampled(2, false),
            storage_texture(3, F::Rgba32Float),
            storage(4, false),
        ],
        ["cs_main"],
    )?;
    let (reconstruct_bgl, [reconstruct]) = pipelines_of(
        device,
        "frame_gen reconstruct depth",
        &wgsl::with_common(wgsl::RECONSTRUCT_DEPTH),
        &[uniform(0, false), sampled(1, false), storage(2, false)],
        ["cs_main"],
    )?;
    let (game_bgl, [game, game_y]) = pipelines_of(
        device,
        "frame_gen game field",
        &wgsl::with_common(wgsl::GAME_FIELD),
        &[
            uniform(0, false),
            sampled(1, false),
            sampled(2, true),
            sampled(3, true),
            filtering_sampler(4),
            storage(5, false),
            storage(6, false),
        ],
        ["cs_publish_x", "cs_publish_y"],
    )?;
    let (flow_field_bgl, [flow_field, flow_field_y]) = pipelines_of(
        device,
        "frame_gen flow field",
        &wgsl::with_common(wgsl::FLOW_FIELD),
        &[
            uniform(0, false),
            storage(1, true),
            sampled(2, true),
            sampled(3, true),
            filtering_sampler(4),
            storage(5, false),
            storage(6, false),
        ],
        ["cs_publish_x", "cs_publish_y"],
    )?;
    let (disocclusion_bgl, [disocclusion]) = pipelines_of(
        device,
        "frame_gen disocclusion",
        &wgsl::with_field(wgsl::DISOCCLUSION),
        &[
            uniform(0, false),
            sampled(1, false),
            storage(2, true),
            storage(3, true),
            storage(4, true),
            storage(5, true),
            sampled(6, false),
            storage_texture(7, F::Rgba8Unorm),
        ],
        ["cs_main"],
    )?;
    let (pyramid_bgl, [game_first, game_down, color_first, color_down]) = pipelines_of(
        device,
        "frame_gen pyramid",
        &wgsl::with_common(wgsl::PYRAMID),
        &[
            uniform(0, true),
            sampled(1, false),
            storage_texture(2, OUTPUT_FORMAT),
            storage(3, true),
            storage(4, true),
        ],
        ["cs_game_first", "cs_game_down", "cs_color_first", "cs_color_down"],
    )?;
    let (interpolate_bgl, [interpolate]) = pipelines_of(
        device,
        "frame_gen interpolate",
        &wgsl::with_field(wgsl::INTERPOLATE),
        &[
            uniform(0, false),
            storage(1, true),
            sampled(2, true),
            sampled(3, true),
            storage(4, true),
            storage(5, true),
            storage(6, true),
            storage(7, true),
            sampled(8, false),
            sampled(9, true),
            sampled(10, true),
            filtering_sampler(11),
            storage_texture(12, OUTPUT_FORMAT),
        ],
        ["cs_main"],
    )?;
    let (inpaint_bgl, [inpaint]) = pipelines_of(
        device,
        "frame_gen inpaint",
        &wgsl::with_common(wgsl::INPAINT),
        &[
            uniform(0, false),
            storage(1, true),
            sampled(2, false),
            sampled(3, false),
            storage_texture(4, OUTPUT_FORMAT),
        ],
        ["cs_main"],
    )?;
    let (debug_bgl, [debug]) = pipelines_of(
        device,
        "frame_gen debug",
        &wgsl::with_common(wgsl::DEBUG_VIEW),
        &[
            uniform(0, false),
            storage(1, true),
            storage(2, true),
            storage(3, true),
            storage(4, true),
            sampled(5, true),
            sampled(6, true),
            filtering_sampler(7),
            storage_texture(8, OUTPUT_FORMAT),
        ],
        ["cs_main"],
    )?;
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("frame_gen linear clamp"),
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let dummy = target(
        device,
        (1, 1),
        wgpu::TextureFormat::Rgba8Unorm,
        "frame_gen dummy",
        1,
    );
    let params = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("frame_gen params"),
        size: PARAMS_BYTES,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    Ok(Pipelines {
        luma_bgl,
        luma_base,
        luma_down,
        histogram,
        compare,
        flow_bgl,
        search,
        filter,
        upscale,
        prepare_bgl,
        prepare,
        reconstruct_bgl,
        reconstruct,
        game_bgl,
        game,
        game_y,
        flow_field_bgl,
        flow_field,
        flow_field_y,
        disocclusion_bgl,
        disocclusion,
        pyramid_bgl,
        game_first,
        game_down,
        color_first,
        color_down,
        interpolate_bgl,
        interpolate,
        inpaint_bgl,
        inpaint,
        debug_bgl,
        debug,
        sampler,
        dummy,
        params,
    })
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![
        crate::contract_row!(
            "fg_debug.wgsl,fg_disocclusion.wgsl,fg_flow_field.wgsl,fg_game_field.wgsl,fg_inpaint.wgsl,fg_interpolate.wgsl,fg_prepare.wgsl,fg_pyramid.wgsl,fg_reconstruct_depth.wgsl",
            "FgFrame",
            FgFrame {
                render_size,
                display_size,
                flow_size,
                depth,
                tune,
                debug_bar,
                flags,
                inv_jittered,
                cur_vp,
                prev_vp,
                velocity_prev_vp
            }
        ),
        crate::contract_row!(
            "fg_flow_luma.wgsl",
            "LumaParams",
            LumaParams {
                src_size,
                dst_size,
                src_offset,
                dst_offset,
                transfer,
                histogram_base,
                pq_range,
                has_history,
                reset
            }
        ),
        crate::contract_row!(
            "fg_flow.wgsl",
            "FlowLevel",
            FlowLevelParams {
                size,
                luma_offset,
                top,
                blocks,
                fine_blocks
            }
        ),
        crate::contract_row!(
            "fg_pyramid.wgsl",
            "PyramidLevel",
            PyramidParams { size, source_size }
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uniforms_match_the_wgsl_layouts() {
        // Seven vec4 and four mat4x4.
        assert_eq!(std::mem::size_of::<FgFrame>(), 7 * 16 + 4 * 64);
        assert_eq!(std::mem::size_of::<LumaParams>(), 48);
        assert_eq!(std::mem::size_of::<FlowLevelParams>(), 32);
        assert_eq!(std::mem::size_of::<PyramidParams>(), 16);
        assert!(std::mem::size_of::<FgFrame>() as u64 <= UNIFORM_STRIDE * 2);
    }

    #[test]
    fn the_flow_levels_follow_the_reference() {
        // 128 × 128: 128, 64, 32, 16, 8 (half of 8 would be under a block).
        let levels = flow_levels((128, 128));
        let sizes: Vec<_> = levels.iter().map(|l| l.size).collect();
        assert_eq!(sizes, [(128, 128), (64, 64), (32, 32), (16, 16), (8, 8)]);
        assert_eq!(levels[1].luma_offset, 128 * 128);
        assert_eq!(levels[4].blocks, (1, 1));
        // Odd sizes round up, 7 levels at most.
        let levels = flow_levels((3841, 2161));
        assert_eq!(levels.len(), optical_flow::MAX_LEVELS);
        assert_eq!(levels[1].size, (1921, 1081));
        assert_eq!(levels[0].blocks, (481, 271));
        // A tiny image still has its one level.
        assert_eq!(flow_levels((4, 4)).len(), 1);
    }

    #[test]
    fn audit_g3_luma_pyramid_stores_one_byte_per_pixel() {
        let display = (1920, 1080);
        let stored = flow_luma_storage_bytes(display);
        let packed: u64 = flow_levels(display)
            .iter()
            .map(|level| u64::from(level.size.0.div_ceil(4) * 4) * u64::from(level.size.1))
            .sum();
        let words: u64 = flow_levels(display)
            .iter()
            .map(|level| u64::from(level.size.0) * u64::from(level.size.1) * 4)
            .sum();
        assert!(
            packed * 3 < words,
            "packed {packed} should be about a quarter of the word layout {words}"
        );
        assert_eq!(
            stored, packed,
            "storage {stored} bytes, one byte per padded pixel is {packed} (word layout {words})"
        );
        // 1920 is a multiple of four, so the next level still starts at width * height.
        assert_eq!(flow_levels(display)[1].luma_offset, 1920 * 1080);
        // Width 30 pads to 32, so the next level does not start at 30 * 16.
        let narrow = flow_levels((30, 16));
        assert_eq!(narrow[1].luma_offset, 32 * 16);
        assert!(narrow[1].luma_offset > 30 * 16);
    }

    #[test]
    fn the_pyramid_halves_down_to_one_texel() {
        assert_eq!(pyramid_mips((1920, 1080)), 10);
        assert_eq!(pyramid_mips((64, 64)), 6);
        assert_eq!(pyramid_mips((1, 1)), 1);
        assert_eq!(pyramid_mips((8192, 8192)), MAX_PYRAMID_MIPS);
    }

    #[test]
    fn the_uniform_buffer_holds_every_entry() {
        let levels = optical_flow::MAX_LEVELS as u64;
        let used = LUMA_OFFSET
            + luma_entries(optical_flow::MAX_LEVELS) * UNIFORM_STRIDE
            + levels * UNIFORM_STRIDE
            + u64::from(MAX_PYRAMID_MIPS) * 2 * UNIFORM_STRIDE;
        assert!(used <= PARAMS_BYTES, "{used}");
    }

    // --- audit-g-render3-determinism 1: the game-field secondary walk ----------------
    //
    // CPU model of `fg_game_field.wgsl::cs_main` over one 64×64 field cleared to 0.
    // `stop_on_primary_preimage` mirrors the pre-fix shader rule: stop the walk after the
    // pre-image `atomicMax` returns shows four primary texels. The priority words the
    // luma/depth inputs contribute do not influence the race (the primary bit dominates
    // every secondary in `atomicMax`), so the model uses fixed priorities.

    const PRIMARY_BIT: u32 = 0x8000_0000;
    const PRIORITY_LOW_MAX: u32 = 31;
    const PRIORITY_HIGH_MAX: u32 = 1023;
    const PRIORITY_LOW_OFFSET: u32 = 16;
    const PRIORITY_HIGH_OFFSET: u32 = 21;
    const FIELD_N: u32 = 64;

    /// WGSL `round`: nearest integer, ties to even. Rust's `f32::round` ties away from zero.
    fn wgsl_round_ties_even(v: f32) -> i32 {
        let floor = v.floor();
        let frac = v - floor;
        let base = floor as i32;
        // A fraction of exactly 1/2 goes to the even integer.
        if frac > 0.5 {
            base + 1
        } else if frac < 0.5 || base.rem_euclid(2) == 0 {
            base
        } else {
            base + 1
        }
    }

    /// Biased unsigned component, matching `pack_uv` in `fg_common.wgsl`.
    fn model_pack_uv(v: f32) -> u32 {
        let biased = v.clamp(-1.0, 1.0) * 32767.0 + 32767.0;
        wgsl_round_ties_even(biased) as u32
    }

    fn model_pack_entry(primary: bool, high: u32, low: u32, mv: [f32; 2]) -> [u32; 2] {
        let priority = if primary { PRIMARY_BIT } else { 0 }
            | ((high & PRIORITY_HIGH_MAX) << PRIORITY_HIGH_OFFSET)
            | ((low & PRIORITY_LOW_MAX) << PRIORITY_LOW_OFFSET);
        [priority | model_pack_uv(mv[0]), priority | model_pack_uv(mv[1])]
    }

    /// Lexicographic max of one texel. The X word is priority plus biased motion x;
    /// on an equal X word the greater Y word wins. This is what the two dispatches
    /// store: every X word is published before any Y word, so the pair is one input.
    fn merge_field_words(x: &mut u32, y: &mut u32, entry: [u32; 2]) {
        if entry[0] > *x {
            *x = entry[0];
            *y = entry[1];
        } else if entry[0] == *x && entry[1] > *y {
            *y = entry[1];
        }
    }

    fn model_unpack_uv(bits: u32) -> f32 {
        let low = (bits & 0xffff) as u16;
        (f32::from(low) - 32767.0) / 32767.0
    }

    fn model_index(pos: [i32; 2]) -> usize {
        (pos[1] as u32 * FIELD_N + pos[0] as u32) as usize
    }

    /// `UpdateGameMotionVectorFieldEx` on the CPU: stores the larger words, returns the
    /// pre-image (the words stored before this write), as `update_field` did pre-fix.
    fn model_update(game_x: &mut [u32], game_y: &mut [u32], pos: [i32; 2], entry: [u32; 2]) -> u32 {
        let index = model_index(pos);
        let old = game_x[index].max(game_y[index]);
        merge_field_words(&mut game_x[index], &mut game_y[index], entry);
        old
    }

    /// Runs the audit's scenario and returns the stored `(game_x, game_y)` words. The
    /// walker at (20, 32) has a half-motion of +0.25 u along x, so its walk back crosses
    /// texels (34..20, 32); the four primary-only pixels splat primaries onto the first
    /// four of them, and no primary writes the texels further along. `primaries_first`
    /// applies every invocation's primary stores before any walk; otherwise each
    /// invocation finishes its walk before the next invocation's primaries land.
    fn game_field_model(stop_on_primary_preimage: bool, primaries_first: bool) -> (Vec<u32>, Vec<u32>) {
        let size = FIELD_N as f32;
        let mut game_x = vec![0u32; (FIELD_N * FIELD_N) as usize];
        let mut game_y = vec![0u32; (FIELD_N * FIELD_N) as usize];
        let walker_px = [20i32, 32];
        let motion_half = [0.25f32, 0.0];
        let walker_primary = model_pack_entry(true, 800, 7, motion_half);
        let secondary = model_pack_entry(false, PRIORITY_HIGH_MAX - 800, 3, motion_half);
        let primary_pixels = [[34i32, 32], [33, 32], [31, 32], [30, 32]];
        let still_primary = model_pack_entry(true, 800, 7, [0.0, 0.0]);

        // Primary vectors go to all four texels of the interpolated position; a
        // zero-motion pixel interpolates at its own texel, footprint `px + {0,1}²`.
        let store_primaries = |game_x: &mut Vec<u32>, game_y: &mut Vec<u32>| {
            let base = [
                (walker_px[0] as f32 + 0.5 + motion_half[0] * size - 0.5).floor() as i32,
                (walker_px[1] as f32 + 0.5 + motion_half[1] * size - 0.5).floor() as i32,
            ];
            for dy in 0..2 {
                for dx in 0..2 {
                    model_update(game_x, game_y, [base[0] + dx, base[1] + dy], walker_primary);
                }
            }
            for px in primary_pixels {
                for dy in 0..2 {
                    for dx in 0..2 {
                        model_update(game_x, game_y, [px[0] + dx, px[1] + dy], still_primary);
                    }
                }
            }
        };

        // The secondary walk back along the half-motion, `cs_main`'s loop verbatim.
        let walk = |game_x: &mut Vec<u32>, game_y: &mut Vec<u32>| {
            let uv = [
                (walker_px[0] as f32 + 0.5) / size,
                (walker_px[1] as f32 + 0.5) / size,
            ];
            let interpolated = [uv[0] + motion_half[0], uv[1] + motion_half[1]];
            let step_mv = [1.0f32, 0.0];
            let step_scale = (2.0f32).sqrt() / size;
            let break_dist = motion_half[0].min(0.5);
            let mut hits = 0u32;
            let mut scale = step_scale;
            loop {
                if scale > break_dist {
                    break;
                }
                let location = [
                    interpolated[0] - step_mv[0] * scale,
                    interpolated[1] - step_mv[1] * scale,
                ];
                let pos = [
                    (location[0] * size - 0.5).floor() as i32,
                    (location[1] * size - 0.5).floor() as i32,
                ];
                let pre_image = model_update(game_x, game_y, pos, secondary);
                if stop_on_primary_preimage && (pre_image & PRIMARY_BIT) != 0 {
                    hits += 1;
                    if hits > 3 {
                        break;
                    }
                }
                scale += step_scale;
            }
        };

        if primaries_first {
            store_primaries(&mut game_x, &mut game_y);
            walk(&mut game_x, &mut game_y);
        } else {
            walk(&mut game_x, &mut game_y);
            store_primaries(&mut game_x, &mut game_y);
        }
        (game_x, game_y)
    }

    #[test]
    fn game_field_secondary_walk_is_schedule_independent() {
        // The scenario must actually separate the schedules under the pre-fix rule, or the
        // test below would prove nothing.
        let (x_a, y_a) = game_field_model(true, true);
        let (x_b, y_b) = game_field_model(true, false);
        assert_ne!(
            x_a, x_b,
            "the pre-image stop must make the field schedule-dependent here"
        );
        assert_ne!(y_a, y_b);
        // The shader must implement the fixed rule: the walk never reads the atomicMax
        // pre-image, so it cannot stop on another invocation's primary stores.
        assert!(
            !crate::frame_gen::wgsl::GAME_FIELD.contains("PRIMARY_BIT"),
            "fg_game_field.wgsl must not branch on the atomic pre-image (schedule-dependent)"
        );
        // Under the fixed rule both schedules store identical words.
        let (x_a, y_a) = game_field_model(false, true);
        let (x_b, y_b) = game_field_model(false, false);
        assert_eq!(x_a, x_b);
        assert_eq!(y_a, y_b);
    }

    /// audit-g-render3-correctness 1. Two primary half-motions that share a priority
    /// and a texel. The stored pair is the one with the greater biased x, in either
    /// arrival order. A per-word `atomicMax` on signed components stores `(-0.1, -0.1)`,
    /// which is neither input.
    #[test]
    fn equal_priority_motion_vectors_stay_whole() {
        let positive_x = model_pack_entry(true, 400, 31, [0.1, -0.1]);
        let negative_x = model_pack_entry(true, 400, 31, [-0.1, 0.1]);
        let stored_of = |order: [[u32; 2]; 2]| {
            let mut x = 0u32;
            let mut y = 0u32;
            for entry in order {
                merge_field_words(&mut x, &mut y, entry);
            }
            [x, y]
        };
        for order in [[positive_x, negative_x], [negative_x, positive_x]] {
            let stored = stored_of(order);
            let motion = [model_unpack_uv(stored[0]), model_unpack_uv(stored[1])];
            assert_eq!(
                stored, positive_x,
                "equal-priority merge stored {stored:#x?} motion {motion:?}, not the whole \
                 vector with the greater x {positive_x:#x?}"
            );
            assert!(
                (motion[0] - 0.1).abs() < 1.0e-4 && (motion[1] + 0.1).abs() < 1.0e-4,
                "unpacked {motion:?}"
            );
        }
        // Equal x: the greater y wins, and the sign bit is not that comparison.
        let down = model_pack_entry(true, 400, 31, [0.1, -0.2]);
        let up = model_pack_entry(true, 400, 31, [0.1, 0.3]);
        assert_eq!(stored_of([down, up]), up);
        assert_eq!(stored_of([up, down]), up);
        let game = crate::frame_gen::wgsl::GAME_FIELD;
        let flow = crate::frame_gen::wgsl::FLOW_FIELD;
        assert!(
            crate::frame_gen::wgsl::COMMON.contains("32767.0 + 32767.0"),
            "pack_uv must store an unsigned biased component"
        );
        for (name, src) in [("fg_game_field.wgsl", game), ("fg_flow_field.wgsl", flow)] {
            assert!(
                src.contains("fn cs_publish_x") && src.contains("fn cs_publish_y"),
                "{name} must publish X and Y in separate dispatches"
            );
            assert!(
                src.contains("atomicLoad"),
                "{name} Y dispatch must keep the vector that owns the winning X word"
            );
        }
        // A higher priority still wins as a whole vector when its components are smaller.
        let faint = model_pack_entry(true, 900, 1, [-0.9, -0.9]);
        let loud = model_pack_entry(true, 100, 31, [0.9, 0.9]);
        let mut x = 0u32;
        let mut y = 0u32;
        merge_field_words(&mut x, &mut y, loud);
        merge_field_words(&mut x, &mut y, faint);
        assert_eq!([x, y], faint);
    }
}
