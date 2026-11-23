// SPDX-License-Identifier: MPL-2.0
//! TSR-grade temporal upscaler (`specs/27` § 2.3, § 4.2 stage 4).
//!
//! An opt-in alternative to [`crate::taau`]: select it with
//! [`crate::TemporalUpscaler::Tsr`] on the render profile. The colour pass still
//! renders at `render_scale` with the Halton(2, 3) jitter; this upscaler then
//! runs three passes:
//!
//! 1. **Prepare** (render resolution): depth-dilated, jitter-free motion for every
//!    texel (camera motion reprojected from depth, object motion from the velocity
//!    target), a disocclusion factor from the previous frame's linear depth, and a
//!    thin-geometry flag (`tsr_prepare.wgsl`).
//! 2. **Resolve** (output resolution): jitter-aware reconstruction of the current
//!    samples, Catmull-Rom history, motion-aware YCoCg variance clipping, thin-feature
//!    locks, anti-flicker, the reactive mask (`tsr_resolve.wgsl`).
//! 3. **Sharpen** (output resolution): a clamped unsharp mask that cannot ring
//!    past the local min/max; the history itself stays unsharpened (`tsr_sharpen.wgsl`).
//!
//! Public sources are cited in each shader and in `docs/provenance.md`.

use crate::frame_work::counted::*;
use std::collections::HashMap;

use regia_core::glam::{DMat4, DVec3, Mat4};

use crate::RenderError;
use crate::shader_lib::compose_source;

/// Tuning for the TSR upscaler (`FrameRenderer::set_tsr_settings`). [`TsrSettings::for_scale`] is the shipped default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TsrSettings {
    /// Output sharpening amount (0 = off). Clamped to the 3×3 min/max, so it cannot ring.
    pub sharpness: f32,
    /// Most frames a pixel accumulates before the history weight stops growing.
    pub max_frames: f32,
    /// Variance-clip gamma (in standard deviations) for a still pixel. The variance
    /// box applies to moving objects, reactive pixels and near-disocclusions; a
    /// static surface under depth-validated camera motion uses the min/max box.
    pub static_gamma: f32,
    /// Variance-clip gamma for a pixel moving ≥ 2 output pixels per frame.
    pub moving_gamma: f32,
    /// Relative luma contrast that marks a one-sample ridge (a thin feature).
    pub ridge_threshold: f32,
    /// Relative view-depth mismatch where disocclusion starts (full at twice this).
    pub depth_tolerance: f32,
    /// Per-frame multiplier on a lock that is not refreshed.
    pub lock_decay: f32,
    /// Sharpness of the per-sample reconstruction kernel `exp(−k·d²)`, `d` in output
    /// pixels. Larger keeps finer detail and accumulates more slowly.
    pub kernel_sharpness: f32,
    /// Added to `log2(render / output)` for the material and terrain texture mip bias
    /// while TSR upscales (`FrameRenderer::texture_lod_bias` reports it). Slightly
    /// negative: the reconstruction kernel and the clip soften the accumulated texels a
    /// little.
    pub texture_bias_offset: f32,
    /// Extra output sharpening at full motion (0 = off), per axis by the pixel's own motion,
    /// reaching full strength at 1 output pixel per frame. A moving view resamples its
    /// history every frame and loses fine detail through the feedback; held and converged
    /// pixels (no motion) are sharpened exactly as without it.
    pub motion_sharpness: f32,
    /// Masked (alpha-tested) materials take a hashed alpha test on TSR frames, which the
    /// accumulation resolves into fractional edge coverage. `false` keeps the hard cutoff.
    pub hashed_alpha: bool,
    /// Debug view instead of the image: 0 = off, 1 = accumulated frames,
    /// 2 = thin-feature lock, 3 = disocclusion, 4 = horizontal motion error against a
    /// 2 px/frame pan × 10 around mid grey, 5 = this frame's reconstruction alone.
    /// Views 3–5 overwrite `aux.z` after the blend. Anti-flicker still scales the history
    /// weight of a still, unclipped pixel, and the color history keeps that blend.
    pub debug_view: u32,
}

impl TsrSettings {
    /// Defaults for an internal `render_scale` (0.5–1.0).
    ///
    /// Fewer input samples per output pixel need more frames to converge and a
    /// little more sharpening to undo the reconstruction kernel.
    #[must_use]
    pub fn for_scale(render_scale: f32) -> Self {
        let scale = render_scale.clamp(0.25, 1.0);
        let samples_per_pixel = scale * scale;
        Self {
            sharpness: if scale < 0.99 { 0.5 } else { 0.25 },
            max_frames: (12.0 / samples_per_pixel.max(0.25)).clamp(12.0, 32.0),
            static_gamma: 1.5,
            moving_gamma: 1.0,
            ridge_threshold: 0.25,
            depth_tolerance: 0.04,
            lock_decay: 0.92,
            // Chart sweep (lavapipe, 0.67, 2 px/frame pan): k = 2 → 67 % of native
            // MTF50, 3 → 91 %, 4 → 100 %.
            kernel_sharpness: if scale < 0.99 { 4.0 } else { 3.0 },
            texture_bias_offset: -0.1,
            // DO27 pan (0.25°/frame, 1080p, 0.67): gradient energy of the moving frame over
            // the native un-jittered frame 0.64–0.67 at 0, 0.69–0.73 at 0.5, 0.73–0.79 at 1;
            // the chart's MTF50, lattice and ghost are unchanged. Native scale: not measured.
            motion_sharpness: if scale < 0.99 { 1.0 } else { 0.0 },
            hashed_alpha: true,
            debug_view: 0,
        }
    }
}

/// Texture mip bias for a frame TSR upscales from `render_width` to `output_width`
/// pixels: `log2(render / output) + offset`, never positive; 0 when nothing upscales.
///
/// The renderer samples textures at the mip the output resolution would pick, so the
/// sub-pixel jitter reaches texture detail the render resolution alone cannot hold
/// (the accumulation resolves it). Vendor upscaler guides recommend the same `log2`
/// ratio (AMD FidelityFX FSR 2 README, "Mipmap biasing", with an extra −1; NVIDIA DLSS
/// programming guide, "Mip-map bias").
#[must_use]
pub(crate) fn texture_lod_bias(render_width: u32, output_width: u32, offset: f32) -> f32 {
    if render_width == 0 || render_width >= output_width {
        return 0.0;
    }
    ((render_width as f32 / output_width as f32).log2() + offset).min(0.0)
}

impl Default for TsrSettings {
    fn default() -> Self {
        Self::for_scale(0.67)
    }
}

/// One frame's inputs to [`TsrPass::resolve`].
pub struct TsrInputs<'a> {
    /// Scene-referred HDR colour at the render resolution (jittered).
    pub current: &'a wgpu::TextureView,
    /// Single-sample `Depth32Float` scene depth at the render resolution.
    pub depth: &'a wgpu::TextureView,
    /// Object motion (`previous uv − current uv`, alpha 1 where written), if recorded.
    pub velocity: Option<&'a wgpu::TextureView>,
    /// Reactive mask (red channel, 0–1) at the render resolution: VFX, decals, alpha.
    pub reactive: Option<&'a wgpu::TextureView>,
    /// Render (input) size.
    pub render_size: (u32, u32),
    /// Output size.
    pub output_size: (u32, u32),
    /// This frame's projection jitter in NDC ([`crate::taau::halton_jitter`]).
    pub jitter: [f32; 2],
    /// Camera position (world), used to keep the reprojection eye-relative.
    pub eye: [f32; 3],
    /// Jittered view-projection the colour pass used.
    pub view_proj: Mat4,
    /// The same without jitter.
    pub unjittered_view_proj: Mat4,
    /// The previous view-projection the velocity pass used (jittered or not).
    pub velocity_prev_view_proj: Mat4,
    /// Exposure multiplier (linear HDR → display-referred), for the tonemapped domain.
    pub exposure: f32,
    /// Drop the history (camera cut, first frame).
    pub reset: bool,
    /// Other views the frame hands as [`Self::current`] on other frames without reallocating
    /// them (the stochastic SSR pass's input while it is dormant and its output while it runs,
    /// ADR-8122): their resolve bind groups are built with this frame's, so the switch between
    /// them after the warm creates none. `None` entries are skipped.
    pub alternates: [Option<&'a wgpu::TextureView>; 2],
}

/// Sample offset of every input texel, in input pixels, for an NDC `jitter`.
///
/// [`crate::taau::jitter_projection`] moves the geometry by `+jitter` in NDC, so
/// the texel centre at `i + 0.5` sees the jitter-free image at `i + 0.5 + offset`:
/// `x` flips sign (NDC → pixels) and `y` keeps it (NDC y is up, pixels go down).
#[must_use]
pub fn sample_offset_px(jitter: [f32; 2], render_w: u32, render_h: u32) -> [f32; 2] {
    [
        -jitter[0] * 0.5 * render_w.max(1) as f32,
        jitter[1] * 0.5 * render_h.max(1) as f32,
    ]
}

/// `view_proj` applied to eye-relative positions (`world − eye`), computed in f64.
#[must_use]
pub fn eye_relative(view_proj: Mat4, eye: [f32; 3]) -> DMat4 {
    let eye = DVec3::new(f64::from(eye[0]), f64::from(eye[1]), f64::from(eye[2]));
    view_proj.as_dmat4() * DMat4::from_translation(eye)
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct TsrFrame {
    render_size: [f32; 4],
    output_size: [f32; 4],
    jitter: [f32; 4],
    tune: [f32; 4],
    tune2: [f32; 4],
    tune3: [f32; 4],
    inv_jittered: [[f32; 4]; 4],
    cur_vp: [[f32; 4]; 4],
    prev_vp: [[f32; 4]; 4],
    velocity_prev_vp: [[f32; 4]; 4],
    /// xy = last frame's sample offset in input pixels (the previous depth's raster), zw unused.
    prev_jitter: [f32; 4],
}

struct Tex {
    /// Owned with the view so the texture drops with the pass.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

/// The three TSR pipelines and their group layouts, once per device (ADR-220): a renderer's
/// first TSR frame (the upscaler switched on mid-play, a renderer built after the warm, a
/// texture view) compiles nothing once a warm built them ([`shared_pipelines`]).
#[derive(Clone)]
pub(crate) struct SharedPipelines {
    prepare: wgpu::RenderPipeline,
    prepare_bgl: wgpu::BindGroupLayout,
    resolve: wgpu::RenderPipeline,
    resolve_bgl: wgpu::BindGroupLayout,
    sharpen: wgpu::RenderPipeline,
    sharpen_bgl: wgpu::BindGroupLayout,
}

struct Pipelines {
    prepare: wgpu::RenderPipeline,
    prepare_bgl: wgpu::BindGroupLayout,
    resolve: wgpu::RenderPipeline,
    resolve_bgl: wgpu::BindGroupLayout,
    sharpen: wgpu::RenderPipeline,
    sharpen_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    /// 1×1 zero texture standing in for a missing velocity or reactive input.
    dummy: Tex,
}

/// Render-resolution targets.
struct InputTargets {
    size: (u32, u32),
    motion: Tex,
    depth: [Tex; 2],
}

/// Output-resolution targets.
struct OutputTargets {
    size: (u32, u32),
    history: [Tex; 2],
    aux: [Tex; 2],
    /// Motion each history pixel moved with (output px), for lock following.
    aux_motion: [Tex; 2],
    output: Tex,
}

type ResolveKey = (
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
);

/// History, motion and the three TSR pipelines. One per view.
#[derive(Default)]
pub struct TsrPass {
    pipes: Option<Pipelines>,
    inputs: Option<InputTargets>,
    outputs: Option<OutputTargets>,
    /// Index of the history/aux/depth written this frame.
    which: usize,
    /// History and previous depth hold a frame at the current sizes.
    history_valid: bool,
    /// The previous frame's jitter-free view-projection (world space, f64).
    prev_unjittered: Option<DMat4>,
    /// Last frame's sample offset (input pixels): where the previous depth's texels were sampled.
    prev_offset: [f32; 2],
    settings: TsrSettings,
    /// Scale applied to the history on the next [`Self::resolve`] (0 reads as 1): the HDR
    /// target's pre-exposure changed by that factor since the history was written.
    history_scale: f32,
    prepare_cache: crate::bind_cache::BindCache<(wgpu::TextureView, wgpu::TextureView, wgpu::TextureView)>,
    resolve_cache: crate::bind_cache::BindCache<ResolveKey>,
    sharpen_cache: crate::bind_cache::BindCache<(wgpu::TextureView, wgpu::TextureView, wgpu::TextureView)>,
}

impl TsrPass {
    /// Replaces the tuning (takes effect next frame; history is kept).
    pub fn set_settings(&mut self, settings: TsrSettings) {
        self.settings = settings;
    }

    /// The current tuning.
    #[must_use]
    pub fn settings(&self) -> TsrSettings {
        self.settings
    }

    /// Scales the history by `scale` on the next [`Self::resolve`]: the HDR target's
    /// pre-exposure changed by that factor since the history was written.
    pub fn set_history_scale(&mut self, scale: f32) {
        self.history_scale = scale;
    }

    /// Drops the history; the next frame starts a fresh accumulation.
    pub fn invalidate(&mut self) {
        self.history_valid = false;
        self.prev_unjittered = None;
    }

    /// Records prepare, resolve and sharpen. Returns the output-resolution HDR view.
    ///
    /// The view stays valid until the next call at a different size.
    pub fn resolve(
        &mut self,
        device: &wgpu::Device,
        shared: &crate::device_pipelines::DevicePipelines,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &TsrInputs<'_>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let _span = tracing::info_span!("tsr").entered();
        let _crumb = crate::breadcrumbs::frame_pass("tsr");
        let device = &crate::device::TrackedDevice::observe(device);
        let render = (inputs.render_size.0.max(1), inputs.render_size.1.max(1));
        let output = (inputs.output_size.0.max(1), inputs.output_size.1.max(1));
        self.ensure(device, shared, render, output)?;
        if inputs.reset {
            self.invalidate();
        }
        let (Some(pipes), Some(input_targets), Some(output_targets)) =
            (self.pipes.as_ref(), self.inputs.as_ref(), self.outputs.as_ref())
        else {
            return Err(RenderError::Shader("tsr resources missing".into()));
        };

        let cur_world = inputs.unjittered_view_proj.as_dmat4();
        let prev_world = self.prev_unjittered.unwrap_or(cur_world);
        let eye = DVec3::new(
            f64::from(inputs.eye[0]),
            f64::from(inputs.eye[1]),
            f64::from(inputs.eye[2]),
        );
        let to_eye = DMat4::from_translation(eye);
        let inv_jittered = eye_relative(inputs.view_proj, inputs.eye).inverse();
        let settings = self.settings;
        let offset = sample_offset_px(inputs.jitter, render.0, render.1);
        let frame = TsrFrame {
            render_size: [
                render.0 as f32,
                render.1 as f32,
                1.0 / render.0 as f32,
                1.0 / render.1 as f32,
            ],
            output_size: [
                output.0 as f32,
                output.1 as f32,
                1.0 / output.0 as f32,
                1.0 / output.1 as f32,
            ],
            jitter: [
                offset[0],
                offset[1],
                std::mem::take(&mut self.history_scale),
                settings.motion_sharpness.max(0.0),
            ],
            tune: [
                inputs.exposure.max(1.0e-12),
                settings.sharpness.max(0.0),
                if self.history_valid { 1.0 } else { 0.0 },
                settings.max_frames.max(1.0),
            ],
            tune2: [
                settings.static_gamma,
                settings.moving_gamma,
                settings.ridge_threshold,
                settings.depth_tolerance.max(1.0e-4),
            ],
            tune3: [
                if inputs.reactive.is_some() { 1.0 } else { 0.0 },
                settings.lock_decay.clamp(0.0, 1.0),
                settings.debug_view.min(5) as f32,
                settings.kernel_sharpness.clamp(0.5, 16.0),
            ],
            inv_jittered: inv_jittered.as_mat4().to_cols_array_2d(),
            cur_vp: (cur_world * to_eye).as_mat4().to_cols_array_2d(),
            prev_vp: (prev_world * to_eye).as_mat4().to_cols_array_2d(),
            velocity_prev_vp: (inputs.velocity_prev_view_proj.as_dmat4() * to_eye)
                .as_mat4()
                .to_cols_array_2d(),
            prev_jitter: [self.prev_offset[0], self.prev_offset[1], 0.0, 0.0],
        };
        belt.write(device, encoder, &pipes.params, 0, bytemuck::bytes_of(&frame));

        let dst = self.which;
        let src = 1 - dst;
        let velocity = inputs.velocity.unwrap_or(&pipes.dummy.view).clone();
        let reactive = inputs.reactive.unwrap_or(&pipes.dummy.view).clone();

        // 1. Prepare.
        let prev_depth = input_targets.depth[src].view.clone();
        let prepare_bg = {
            let layout = &pipes.prepare_bgl;
            let params = &pipes.params;
            let depth = inputs.depth;
            let velocity = &velocity;
            let prev_depth = &prev_depth;
            self.prepare_cache.get_or_insert_with(
                (depth.clone(), velocity.clone(), prev_depth.clone()),
                || {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("tsr prepare"),
                        layout,
                        entries: &[
                            view_entry(0, depth),
                            view_entry(1, velocity),
                            view_entry(2, prev_depth),
                            wgpu::BindGroupEntry {
                                binding: 3,
                                resource: params.as_entire_binding(),
                            },
                        ],
                    })
                },
            )
        };
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tsr prepare"),
                color_attachments: &[
                    Some(clear_attachment(&input_targets.motion.view)),
                    Some(clear_attachment(&input_targets.depth[dst].view)),
                ],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.counted_set_pipeline(&pipes.prepare);
            pass.counted_set_bind_group(0, &prepare_bg, &[]);
            pass.counted_draw(0..3, 0..1);
        }

        // 2. Resolve. Both ping-pong parities' groups for this frame's inputs are built together
        // (ADR-7885), for this frame's colour input and for the views the frame hands instead on
        // other frames (ADR-8122): the stochastic SSR pass returns its input unchanged once it
        // has seen 30 readbacks without a glossy pixel, so a still shot on a matte area switched
        // the resolve's input to a view it never bound during warm-up (the 2026-11-15
        // Switchbacks soak: two "tsr resolve" groups at frames 991-992, 31 frames into the still
        // shot that starts at 960).
        let resolve_bg = {
            let layout = &pipes.resolve_bgl;
            let params = &pipes.params;
            let sampler = &pipes.sampler;
            let motion = &input_targets.motion.view;
            let reactive = &reactive;
            let mut this_frame = None;
            // The frame's own input first (its group is this frame's), then the views it hands
            // on other frames (ADR-8122): only views of this input's size and format (a stale
            // output of another size is never bound, so no group would ever be used).
            let same_target = |view: &wgpu::TextureView| {
                let (a, b) = (view.texture(), inputs.current.texture());
                a.size() == b.size() && a.format() == b.format()
            };
            let currents = std::iter::once(inputs.current).chain(
                inputs
                    .alternates
                    .into_iter()
                    .flatten()
                    .filter(|view| *view != inputs.current && same_target(view)),
            );
            for (current, (read, write)) in currents.flat_map(|c| [(c, (src, dst)), (c, (dst, src))]) {
                let history_src = &output_targets.history[read].view;
                let aux_src = &output_targets.aux[read].view;
                let depth_cur = &input_targets.depth[write].view;
                let aux_motion_src = &output_targets.aux_motion[read].view;
                let group = self.resolve_cache.get_or_insert_with(
                    (
                        current.clone(),
                        motion.clone(),
                        history_src.clone(),
                        aux_src.clone(),
                        reactive.clone(),
                        depth_cur.clone(),
                        aux_motion_src.clone(),
                    ),
                    || {
                        device.create_bind_group(&wgpu::BindGroupDescriptor {
                            label: Some("tsr resolve"),
                            layout,
                            entries: &[
                                view_entry(0, current),
                                view_entry(1, motion),
                                view_entry(2, history_src),
                                view_entry(3, aux_src),
                                view_entry(4, reactive),
                                wgpu::BindGroupEntry {
                                    binding: 5,
                                    resource: wgpu::BindingResource::Sampler(sampler),
                                },
                                wgpu::BindGroupEntry {
                                    binding: 6,
                                    resource: params.as_entire_binding(),
                                },
                                view_entry(7, depth_cur),
                                view_entry(8, aux_motion_src),
                            ],
                        })
                    },
                );
                this_frame.get_or_insert(group);
            }
            this_frame.ok_or_else(|| RenderError::Encode("tsr resolve: no bind group".into()))?
        };
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tsr resolve"),
                color_attachments: &[
                    Some(clear_attachment(&output_targets.history[dst].view)),
                    Some(clear_attachment(&output_targets.aux[dst].view)),
                    Some(clear_attachment(&output_targets.aux_motion[dst].view)),
                ],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.counted_set_pipeline(&pipes.resolve);
            pass.counted_set_bind_group(0, &resolve_bg, &[]);
            pass.counted_draw(0..3, 0..1);
        }

        // 3. Sharpen into the output.
        let history_dst = output_targets.history[dst].view.clone();
        let aux_dst = output_targets.aux[dst].view.clone();
        let motion_dst = output_targets.aux_motion[dst].view.clone();
        let sharpen_bg = {
            let layout = &pipes.sharpen_bgl;
            let params = &pipes.params;
            let (history_dst, aux_dst, motion_dst) = (&history_dst, &aux_dst, &motion_dst);
            self.sharpen_cache.get_or_insert_with(
                (history_dst.clone(), aux_dst.clone(), motion_dst.clone()),
                || {
                    device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("tsr sharpen"),
                        layout,
                        entries: &[
                            view_entry(0, history_dst),
                            wgpu::BindGroupEntry {
                                binding: 1,
                                resource: params.as_entire_binding(),
                            },
                            view_entry(2, aux_dst),
                            view_entry(3, motion_dst),
                        ],
                    })
                },
            )
        };
        {
            let mut pass = encoder.counted_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("tsr sharpen"),
                color_attachments: &[Some(clear_attachment(&output_targets.output.view))],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.counted_set_pipeline(&pipes.sharpen);
            pass.counted_set_bind_group(0, &sharpen_bg, &[]);
            pass.counted_draw(0..3, 0..1);
        }
        let out = output_targets.output.view.clone();
        self.which = src;
        self.history_valid = true;
        self.prev_unjittered = Some(cur_world);
        self.prev_offset = offset;
        Ok(out)
    }

    fn ensure(
        &mut self,
        device: &wgpu::Device,
        shared: &crate::device_pipelines::DevicePipelines,
        render: (u32, u32),
        output: (u32, u32),
    ) -> Result<(), RenderError> {
        if self.pipes.is_none() {
            self.pipes = Some(build_pipelines(device, shared)?);
        }
        if self.inputs.as_ref().is_none_or(|t| t.size != render) {
            self.inputs = Some(InputTargets {
                size: render,
                motion: target(device, render, wgpu::TextureFormat::Rgba32Float, "tsr motion"),
                depth: [
                    target(device, render, wgpu::TextureFormat::Rgba32Float, "tsr depth 0"),
                    target(device, render, wgpu::TextureFormat::Rgba32Float, "tsr depth 1"),
                ],
            });
            self.history_valid = false;
            self.prepare_cache.clear();
            self.resolve_cache.clear();
        }
        if self.outputs.as_ref().is_none_or(|t| t.size != output) {
            let hdr = wgpu::TextureFormat::Rgba16Float;
            self.outputs = Some(OutputTargets {
                size: output,
                history: [
                    target(device, output, hdr, "tsr history 0"),
                    target(device, output, hdr, "tsr history 1"),
                ],
                aux: [
                    target(device, output, hdr, "tsr aux 0"),
                    target(device, output, hdr, "tsr aux 1"),
                ],
                // xy = the motion each pixel moved with, zw = its tracked thin feature's offset (ADR-7694).
                aux_motion: [
                    target(device, output, hdr, "tsr aux motion 0"),
                    target(device, output, hdr, "tsr aux motion 1"),
                ],
                // COPY_SRC: the HDR capture and `before_post` passes copy the post stack's input.
                output: target_usage(device, output, hdr, "tsr output", wgpu::TextureUsages::COPY_SRC),
            });
            self.history_valid = false;
            self.resolve_cache.clear();
            self.sharpen_cache.clear();
        }
        Ok(())
    }
}

fn clear_attachment(view: &wgpu::TextureView) -> wgpu::RenderPassColorAttachment<'_> {
    wgpu::RenderPassColorAttachment {
        view,
        depth_slice: None,
        resolve_target: None,
        ops: wgpu::Operations {
            load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
            store: wgpu::StoreOp::Store,
        },
    }
}

fn view_entry(binding: u32, view: &wgpu::TextureView) -> wgpu::BindGroupEntry<'_> {
    wgpu::BindGroupEntry {
        binding,
        resource: wgpu::BindingResource::TextureView(view),
    }
}

fn target(device: &wgpu::Device, size: (u32, u32), format: wgpu::TextureFormat, label: &str) -> Tex {
    target_usage(device, size, format, label, wgpu::TextureUsages::empty())
}

/// [`target`] with `extra` usages.
fn target_usage(
    device: &wgpu::Device,
    size: (u32, u32),
    format: wgpu::TextureFormat,
    label: &str,
    extra: wgpu::TextureUsages,
) -> Tex {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: wgpu::Extent3d {
            width: size.0.max(1),
            height: size.1.max(1),
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING | extra,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Tex { texture, view }
}

fn module(device: &wgpu::Device, label: &str, source: &str) -> Result<wgpu::ShaderModule, RenderError> {
    let module = compose_source(source, label, HashMap::new())?;
    Ok(device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Naga(std::borrow::Cow::Owned(module)),
    }))
}

fn tex_entry(binding: u32, sample_type: wgpu::TextureSampleType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn unfilterable(binding: u32) -> wgpu::BindGroupLayoutEntry {
    tex_entry(binding, wgpu::TextureSampleType::Float { filterable: false })
}

fn uniform(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn fullscreen_pipeline(
    device: &wgpu::Device,
    label: &str,
    module: &wgpu::ShaderModule,
    bgl: &wgpu::BindGroupLayout,
    formats: &[wgpu::TextureFormat],
) -> wgpu::RenderPipeline {
    let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(bgl)],
        immediate_size: 0,
    });
    let targets: Vec<Option<wgpu::ColorTargetState>> = formats
        .iter()
        .map(|&format| {
            Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })
        })
        .collect();
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            buffers: &[],
            compilation_options: Default::default(),
        },
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some("fs_main"),
            targets: &targets,
            compilation_options: Default::default(),
        }),
        primitive: wgpu::PrimitiveState {
            topology: wgpu::PrimitiveTopology::TriangleList,
            ..Default::default()
        },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    })
}

/// The device's TSR pipelines, built on this thread when no warm built them yet. Frame warms
/// call it too (`FrameRenderer::prewarm_mid_play`): a renderer whose profile switches to TSR
/// mid-play (a settings or quality-tier change) then compiles nothing on its first TSR frame.
pub(crate) fn shared_pipelines(
    device: &wgpu::Device,
    shared: &crate::device_pipelines::DevicePipelines,
) -> Result<SharedPipelines, RenderError> {
    shared
        .get_or_create(crate::frame::shared_pipes_key("tsr pipelines"), || {
            build_shared(device).map_err(|e| e.to_string())
        })
        .map_err(RenderError::Shader)
}

fn build_shared(device: &wgpu::Device) -> Result<SharedPipelines, RenderError> {
    let prepare_module = module(device, "tsr prepare", PREPARE_WGSL)?;
    let resolve_module = module(device, "tsr resolve", RESOLVE_WGSL)?;
    let sharpen_module = module(device, "tsr sharpen", SHARPEN_WGSL)?;
    let prepare_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("tsr prepare bgl"),
        entries: &[
            tex_entry(0, wgpu::TextureSampleType::Depth),
            unfilterable(1),
            unfilterable(2),
            uniform(3),
        ],
    });
    let resolve_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("tsr resolve bgl"),
        entries: &[
            unfilterable(0),
            unfilterable(1),
            tex_entry(2, wgpu::TextureSampleType::Float { filterable: true }),
            unfilterable(3),
            unfilterable(4),
            wgpu::BindGroupLayoutEntry {
                binding: 5,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
            uniform(6),
            unfilterable(7),
            unfilterable(8),
        ],
    });
    let sharpen_bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("tsr sharpen bgl"),
        entries: &[unfilterable(0), uniform(1), unfilterable(2), unfilterable(3)],
    });
    let hdr = wgpu::TextureFormat::Rgba16Float;
    let prepare = fullscreen_pipeline(
        device,
        "tsr prepare",
        &prepare_module,
        &prepare_bgl,
        &[wgpu::TextureFormat::Rgba32Float, wgpu::TextureFormat::Rgba32Float],
    );
    let resolve = fullscreen_pipeline(
        device,
        "tsr resolve",
        &resolve_module,
        &resolve_bgl,
        &[hdr, hdr, hdr],
    );
    let sharpen = fullscreen_pipeline(device, "tsr sharpen", &sharpen_module, &sharpen_bgl, &[hdr]);
    Ok(SharedPipelines {
        prepare,
        prepare_bgl,
        resolve,
        resolve_bgl,
        sharpen,
        sharpen_bgl,
    })
}

fn build_pipelines(
    device: &wgpu::Device,
    shared: &crate::device_pipelines::DevicePipelines,
) -> Result<Pipelines, RenderError> {
    let SharedPipelines {
        prepare,
        prepare_bgl,
        resolve,
        resolve_bgl,
        sharpen,
        sharpen_bgl,
    } = shared_pipelines(device, shared)?;
    let hdr = wgpu::TextureFormat::Rgba16Float;
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("tsr history"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let params = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("tsr frame"),
        size: std::mem::size_of::<TsrFrame>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let dummy = target(device, (1, 1), hdr, "tsr zero");
    Ok(Pipelines {
        prepare,
        prepare_bgl,
        resolve,
        resolve_bgl,
        sharpen,
        sharpen_bgl,
        sampler,
        params,
        dummy,
    })
}

/// Prepare pass source.
pub(crate) const PREPARE_WGSL: &str = include_str!("tsr_prepare.wgsl");
/// Resolve pass source.
pub(crate) const RESOLVE_WGSL: &str = include_str!("tsr_resolve.wgsl");
/// Sharpen pass source.
pub(crate) const SHARPEN_WGSL: &str = include_str!("tsr_sharpen.wgsl");

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "tsr_prepare.wgsl,tsr_resolve.wgsl,tsr_sharpen.wgsl",
        "TsrFrame",
        TsrFrame {
            render_size,
            output_size,
            jitter,
            tune,
            tune2,
            tune3,
            inv_jittered,
            cur_vp,
            prev_vp,
            velocity_prev_vp,
            prev_jitter
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texture_lod_bias_follows_the_upscale_ratio() {
        assert_eq!(texture_lod_bias(1920, 1920, -0.1), 0.0);
        assert_eq!(texture_lod_bias(0, 1920, -0.1), 0.0);
        let b = texture_lod_bias(1286, 1920, 0.0);
        assert!((b - (1286.0f32 / 1920.0).log2()).abs() < 1e-6, "{b}");
        assert!((texture_lod_bias(960, 1920, -0.1) + 1.1).abs() < 1e-6);
        // A positive offset never blurs past the render-resolution mip.
        assert_eq!(texture_lod_bias(1900, 1920, 0.5), 0.0);
    }

    // CPU mirrors of the shader's Catmull-Rom weights and box clip.

    /// Catmull-Rom weights for the four taps around a fractional position `f` ∈ [0, 1).
    ///
    /// The shader evaluates the same polynomials (`history_catmull_rom`).
    fn catmull_rom_weights(f: f32) -> [f32; 4] {
        [
            f * (-0.5 + f * (1.0 - 0.5 * f)),
            1.0 + f * f * (-2.5 + 1.5 * f),
            f * (0.5 + f * (2.0 - 1.5 * f)),
            f * f * (-0.5 + 0.5 * f),
        ]
    }

    /// Clips `h` toward the centre of the box `[lo, hi]` (the shader's `clip_to_box`).
    fn clip_to_box(h: [f32; 3], lo: [f32; 3], hi: [f32; 3]) -> [f32; 3] {
        let mut centre = [0.0; 3];
        let mut d = [0.0; 3];
        let mut m = 0.0f32;
        for i in 0..3 {
            centre[i] = 0.5 * (lo[i] + hi[i]);
            let extent = (0.5 * (hi[i] - lo[i])).max(0.002 + 0.01 * hi[0].abs().max(lo[0].abs()));
            d[i] = h[i] - centre[i];
            m = m.max((d[i] / extent).abs());
        }
        if m > 1.0 {
            [centre[0] + d[0] / m, centre[1] + d[1] / m, centre[2] + d[2] / m]
        } else {
            h
        }
    }

    #[test]
    fn shaders_compose_and_validate() {
        for (name, source) in [
            ("tsr_prepare.wgsl", PREPARE_WGSL),
            ("tsr_resolve.wgsl", RESOLVE_WGSL),
            ("tsr_sharpen.wgsl", SHARPEN_WGSL),
        ] {
            compose_source(source, name, HashMap::new()).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }

    #[test]
    fn frame_uniform_matches_the_wgsl_layout() {
        // Seven vec4 and four mat4x4: 7 × 16 + 4 × 64 bytes.
        assert_eq!(std::mem::size_of::<TsrFrame>(), 368);
    }

    #[test]
    fn sample_offset_undoes_the_projection_jitter() {
        // A point at the screen centre, projected with a jittered matrix, lands
        // `offset` input pixels away from where the jitter-free projection puts it,
        // with the opposite sign: the texel at the jitter-free position therefore
        // saw the scene at `centre + offset`.
        let (w, h) = (640u32, 360u32);
        let jitter = [0.3 * 2.0 / w as f32, -0.2 * 2.0 / h as f32];
        let proj =
            regia_core::glam::camera::rh::proj::directx::perspective(1.0, w as f32 / h as f32, 0.1, 100.0);
        let jittered = crate::taau::jitter_projection(proj, jitter);
        let point = regia_core::glam::Vec4::new(0.7, -0.4, -5.0, 1.0);
        let to_px = |clip: regia_core::glam::Vec4| {
            let ndc = clip.truncate() / clip.w;
            [(ndc.x * 0.5 + 0.5) * w as f32, (0.5 - ndc.y * 0.5) * h as f32]
        };
        let a = to_px(proj * point);
        let b = to_px(jittered * point);
        let offset = sample_offset_px(jitter, w, h);
        assert!(
            (b[0] - a[0] + offset[0]).abs() < 1.0e-3,
            "x {a:?} {b:?} {offset:?}"
        );
        assert!(
            (b[1] - a[1] + offset[1]).abs() < 1.0e-3,
            "y {a:?} {b:?} {offset:?}"
        );
        assert!((offset[0] + 0.3).abs() < 1.0e-5 && (offset[1] + 0.2).abs() < 1.0e-5);
    }

    #[test]
    fn eye_relative_matches_world_space() {
        let view = regia_core::glam::camera::rh::view::look_at_mat4(
            regia_core::glam::Vec3::new(5000.0, 30.0, -2000.0),
            regia_core::glam::Vec3::new(5010.0, 28.0, -2010.0),
            regia_core::glam::Vec3::Y,
        );
        let proj = regia_core::glam::camera::rh::proj::directx::perspective(1.0, 1.5, 0.1, 1000.0);
        let vp = proj * view;
        let eye = [5000.0, 30.0, -2000.0];
        let rel = eye_relative(vp, eye);
        let world = DVec3::new(5004.0, 29.0, -2003.0);
        let a = vp.as_dmat4() * world.extend(1.0);
        let b = rel * (world - DVec3::new(5000.0, 30.0, -2000.0)).extend(1.0);
        assert!((a - b).length() < 1.0e-6, "{a:?} {b:?}");
    }

    #[test]
    fn catmull_rom_interpolates_and_sums_to_one() {
        let w0 = catmull_rom_weights(0.0);
        assert_eq!(w0, [0.0, 1.0, 0.0, 0.0]);
        for i in 0..16 {
            let f = i as f32 / 16.0;
            let w = catmull_rom_weights(f);
            assert!((w.iter().sum::<f32>() - 1.0).abs() < 1.0e-5, "{f}: {w:?}");
        }
        // Symmetric at the midpoint, with negative outer lobes (the sharpness).
        let mid = catmull_rom_weights(0.5);
        assert!((mid[1] - mid[2]).abs() < 1.0e-6 && mid[0] < 0.0 && mid[3] < 0.0);
    }

    #[test]
    fn clip_moves_history_onto_the_box_toward_its_centre() {
        let lo = [0.0, -0.1, -0.1];
        let hi = [0.5, 0.1, 0.1];
        assert_eq!(clip_to_box([0.2, 0.0, 0.0], lo, hi), [0.2, 0.0, 0.0]);
        let clipped = clip_to_box([1.25, 0.0, 0.0], lo, hi);
        assert!((clipped[0] - 0.5).abs() < 1.0e-6);
        // Clipping keeps the chroma direction (unlike a per-axis clamp).
        let c = clip_to_box([0.75, 0.1, 0.0], lo, hi);
        assert!(
            (c[0] - 0.5).abs() < 1.0e-6 && (c[1] - 0.05).abs() < 1.0e-6,
            "{c:?}"
        );
    }

    #[test]
    fn chroma_noise_on_a_grey_box_does_not_drag_luma_to_the_centre() {
        // Dark/bright grey neighbourhood: zero chroma extent. A history inside the
        // luma range with a hair of chroma must keep its luma (the shader's
        // minimum box extent; edges were pulled to 50 % without it).
        let lo = [0.05, 0.0, 0.0];
        let hi = [0.5, 0.0, 0.0];
        let c = clip_to_box([0.08, 1.0e-4, -1.0e-4], lo, hi);
        assert!((c[0] - 0.08).abs() < 1.0e-6, "{c:?}");
    }

    #[test]
    fn every_preset_keeps_the_taau_default() {
        use crate::{RenderProfile, RenderProfilePreset, TemporalUpscaler};
        for preset in [
            RenderProfilePreset::MobileLow,
            RenderProfilePreset::MobileHigh,
            RenderProfilePreset::DesktopLow,
            RenderProfilePreset::DesktopHigh,
            RenderProfilePreset::DesktopUltra,
            RenderProfilePreset::Deck,
        ] {
            assert_eq!(
                RenderProfile::from_preset(preset).temporal_upscaler,
                TemporalUpscaler::Taau,
                "{preset:?}: TSR is opt-in (ADR-041)"
            );
        }
        assert_eq!(
            crate::FrameRenderer::new().temporal_upscaler(),
            TemporalUpscaler::Taau
        );
    }

    #[test]
    fn settings_converge_longer_at_lower_scale() {
        let native = TsrSettings::for_scale(1.0);
        let budget = TsrSettings::for_scale(0.67);
        let half = TsrSettings::for_scale(0.5);
        assert!(native.max_frames <= budget.max_frames && budget.max_frames <= half.max_frames);
        assert!(budget.sharpness > native.sharpness);
        assert!(
            budget.static_gamma > budget.moving_gamma,
            "motion narrows the clip box"
        );
    }
}
