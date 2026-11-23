// SPDX-License-Identifier: MIT
//! FSR 3.1 temporal upscaler, the comparison baseline for TSR (`specs/27` § 4.2 stage 4).
//!
//! Ported from AMD FidelityFX SDK v1.1.4 (<https://github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK>),
//! the FSR 3.1 upscaler (`sdk/include/FidelityFX/gpu/fsr3upscaler/*`, MIT; tag v1.1.4 only, the
//! SDK's later main branch is under a different licence). Copyright (C) 2024 Advanced Micro
//! Devices, Inc. The full notice is in each `fsr3_*.wgsl`.
//!
//! An opt-in sibling of [`crate::tsr`]: select it with [`crate::TemporalUpscaler::Fsr3`]. The
//! colour pass renders at `render_scale` with the Halton(2, 3) jitter, then seven compute
//! passes run (the SDK's dispatch order):
//!
//! 1. **Prepare inputs** (render res): nearest-depth dilated motion, the current depth
//!    scattered to its previous-frame position, farthest depth, current luma.
//! 2. **Pyramid** (render / 2 … / 8): the shading-change luma differences against the
//!    previous frame, and farthest depth at half resolution.
//! 3. **Shading change** (render / 2).
//! 4. **Prepare reactivity** (render res): disocclusion, motion divergence, accumulation
//!    count, thin-feature locks.
//! 5. **Luma instability** (render res): four-frame luma history.
//! 6. **Accumulate** (output res): Lanczos-2 upsample of the current samples, Lanczos-2
//!    history, variance-box rectification, locks.
//! 7. **RCAS** (output res): robust contrast-adaptive sharpening.
//!
//! What changed from the source is listed in each shader's header and in ADR-113.

use crate::frame_work::counted::*;
use std::collections::HashMap;

use regia_core::glam::{DMat4, DVec3};

use crate::RenderError;
use crate::shader_lib::compose_source;
use crate::tsr::{TsrInputs, eye_relative, sample_offset_px};

/// Tuning for the FSR 3.1 upscaler (`FrameRenderer::set_fsr3_settings`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fsr3Settings {
    /// RCAS sharpness, 0–1 (the SDK's `sharpness`; 0 turns RCAS off).
    pub sharpness: f32,
    /// Scales the velocity that lowers the history weight (the SDK's `velocityFactor`, 1).
    pub velocity_factor: f32,
    /// Scales the reactive mask (the SDK's `reactivenessScale`, 1).
    pub reactiveness_scale: f32,
    /// Scales the detected shading change (the SDK's `shadingChangeScale`, 1).
    pub shading_change_scale: f32,
    /// Accumulation gained per frame (the SDK's `accumulationAddedPerFrame`). Regia Engine's default
    /// is 1/4 (the SDK's 1/3): a restarted pixel takes four frames, not three, to reach the widest
    /// history box (ADR-7678).
    pub accumulation_added_per_frame: f32,
    /// Accumulation a disoccluded pixel restarts from (the SDK's `minDisocclusionAccumulation`).
    /// Regia Engine's default is −1 (the SDK's −1/3): a pixel at least half disoccluded restarts
    /// instead of keeping part of what covered it (ADR-7678).
    pub min_disocclusion_accumulation: f32,
    /// Debug view instead of the image: 0 = off, 1 = accumulation, 2 = lock, 3 = disocclusion,
    /// 4 = shading change, 5 = luma instability, 6 = reactive (motion divergence).
    pub debug_view: u32,
}

impl Default for Fsr3Settings {
    fn default() -> Self {
        Self {
            // The SDK sample's default RCAS strength.
            sharpness: 0.8,
            velocity_factor: 1.0,
            reactiveness_scale: 1.0,
            shading_change_scale: 1.0,
            // ADR-7678 (the SDK's defaults are 1/3 and −1/3): FSR 3.1 at 0.67 met spec 27's
            // ghosting target (0.029 → 0.019 on the temporal chart).
            accumulation_added_per_frame: 1.0 / 4.0,
            min_disocclusion_accumulation: -1.0,
            debug_view: 0,
        }
    }
}

/// One way Regia Engine's FSR 3.1 resolve differs from the reference algorithm it ports (AMD
/// FidelityFX SDK v1.1.4, FSR 3.1 upscaler). The agent status (`render.get_upscalers`) and the
/// release notes list them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fsr3ReferenceDifference {
    /// Stable snake_case id.
    pub id: &'static str,
    /// The decision record.
    pub adr: &'static str,
    /// What changed, one sentence.
    pub summary: &'static str,
}

/// The reference algorithm Regia Engine's FSR 3.1 resolve ports.
pub const FSR3_REFERENCE: &str = "AMD FidelityFX SDK v1.1.4, FSR 3.1 upscaler";

/// Every way the default FSR 3.1 resolve differs from [`FSR3_REFERENCE`] in what it outputs
/// (porting details that keep the output, such as WGSL and Regia Engine's own motion vectors, are
/// in the shader headers and ADR-113).
pub const FSR3_REFERENCE_DIFFERENCES: &[Fsr3ReferenceDifference] = &[
    Fsr3ReferenceDifference {
        id: "nearest_sample_restart",
        adr: "ADR-7678",
        summary: "a restarting pixel takes its nearest render sample, not the 3x3 Gaussian mean",
    },
    Fsr3ReferenceDifference {
        id: "accumulation_tuning",
        adr: "ADR-7678",
        summary: "default accumulationAddedPerFrame 1/4 and minDisocclusionAccumulation -1 (SDK 1/3 and -1/3)",
    },
];

/// FSR's jitter phase count for a render → display width ratio (`ffxFsr3UpscalerGetJitterPhaseCount`).
///
/// Regia Engine's Halton sequence does not wrap; FSR only uses the count to pace lock decay.
#[must_use]
pub fn jitter_phase_count(render_width: u32, display_width: u32) -> u32 {
    let ratio = display_width.max(1) as f32 / render_width.max(1) as f32;
    ((8.0 * ratio * ratio) as u32).max(1)
}

/// RCAS strength constant for an SDK `sharpness` in 0–1 (`FsrRcasCon` after the SDK's
/// `−2 × sharpness + 2` remap to stops): 1 at full sharpness, 0.25 at none.
#[must_use]
pub fn rcas_strength(sharpness: f32) -> f32 {
    let stops = -2.0 * sharpness.clamp(0.0, 1.0) + 2.0;
    (-stops).exp2()
}

/// FSR's device → view depth factors `[c0, c1]` (`view = c1 / (depth − c0)`) from a
/// perspective view-projection: the depth row of the projection, recovered from the
/// matrix's `z` and `w` rows. Works for finite, infinite, world or eye-relative matrices.
#[must_use]
pub fn device_to_view_depth(view_proj: DMat4) -> [f32; 2] {
    let row = |r: usize| {
        [
            view_proj.x_axis[r],
            view_proj.y_axis[r],
            view_proj.z_axis[r],
            view_proj.w_axis[r],
        ]
    };
    let z = row(2);
    let w = row(3);
    // `w` row = −(view z row) and `z` row = P22 · (view z row) + P32 · (0, 0, 0, 1), so
    // `z[k] / w[k]` = −P22 for any k, and P32 = z[3] + P22 · w[3].
    let mut k = 0;
    for i in 1..3 {
        if w[i].abs() > w[k].abs() {
            k = i;
        }
    }
    if w[k].abs() < 1.0e-12 {
        return [1.0, -1.0];
    }
    let minus_p22 = z[k] / w[k];
    let p32 = z[3] - minus_p22 * w[3];
    // d = −P22 + P32 / w ⇒ w = P32 / (d − (−P22)).
    [minus_p22 as f32, p32 as f32]
}

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Fsr3Frame {
    render_size: [f32; 4],
    upscale_size: [f32; 4],
    previous_sizes: [f32; 4],
    jitter: [f32; 4],
    depth: [f32; 4],
    tune: [f32; 4],
    tune2: [f32; 4],
    tune3: [f32; 4],
    inv_jittered: [[f32; 4]; 4],
    cur_vp: [[f32; 4]; 4],
    prev_vp: [[f32; 4]; 4],
    velocity_prev_vp: [[f32; 4]; 4],
}

struct Tex {
    /// Owned with the view so the texture drops with the pass.
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
}

struct Pipelines {
    prepare: wgpu::ComputePipeline,
    prepare_bgl: wgpu::BindGroupLayout,
    pyramid: wgpu::ComputePipeline,
    pyramid_bgl: wgpu::BindGroupLayout,
    shading: wgpu::ComputePipeline,
    shading_bgl: wgpu::BindGroupLayout,
    reactivity: wgpu::ComputePipeline,
    reactivity_bgl: wgpu::BindGroupLayout,
    instability: wgpu::ComputePipeline,
    instability_bgl: wgpu::BindGroupLayout,
    accumulate: wgpu::ComputePipeline,
    accumulate_bgl: wgpu::BindGroupLayout,
    rcas: wgpu::ComputePipeline,
    rcas_bgl: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    params: wgpu::Buffer,
    /// 1×1 zero texture standing in for a missing velocity or reactive input.
    dummy: Tex,
}

/// Every target at the current render and output sizes.
struct Targets {
    render: (u32, u32),
    output: (u32, u32),
    motion: Tex,
    luma: [Tex; 2],
    reconstructed_depth: wgpu::Buffer,
    mips: [Tex; 3],
    shading_change: Tex,
    masks: Tex,
    accumulation: [Tex; 2],
    new_locks: wgpu::Buffer,
    luma_history: [Tex; 2],
    instability: Tex,
    history: [Tex; 2],
    output_tex: Tex,
}

#[derive(Clone)]
struct Groups {
    prepare: wgpu::BindGroup,
    pyramid: wgpu::BindGroup,
    shading: wgpu::BindGroup,
    reactivity: wgpu::BindGroup,
    instability: wgpu::BindGroup,
    accumulate: wgpu::BindGroup,
    rcas: wgpu::BindGroup,
}

/// External views plus the ping-pong parity.
type GroupKey = (
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    wgpu::TextureView,
    usize,
);

/// History, internal targets and the seven FSR 3.1 pipelines. One per view.
#[derive(Default)]
pub struct Fsr3Pass {
    pipes: Option<Pipelines>,
    targets: Option<Targets>,
    /// Index of the ping-pong targets written this frame.
    which: usize,
    /// History holds a frame at the current sizes.
    history_valid: bool,
    /// Frames since the last reset (FSR's `frameIndex`).
    frame_index: u32,
    /// The previous frame's jitter-free view-projection (world space, f64).
    prev_unjittered: Option<DMat4>,
    /// The previous frame's jitter in render pixels (FSR convention).
    prev_jitter: [f32; 2],
    settings: Fsr3Settings,
    /// Scale applied to the history on the next [`Self::resolve`] (0 reads as 1).
    history_scale: f32,
    groups: crate::bind_cache::BindCache<GroupKey, Groups>,
    /// Frames resolved since creation (frame generation checks the surfaces are this frame's).
    #[cfg(feature = "frame-gen")]
    resolves: u64,
}

impl Fsr3Pass {
    /// Replaces the tuning (takes effect next frame; history is kept).
    pub fn set_settings(&mut self, settings: Fsr3Settings) {
        self.settings = settings;
    }

    /// The current tuning.
    #[must_use]
    pub fn settings(&self) -> Fsr3Settings {
        self.settings
    }

    /// Scales the history by `scale` on the next [`Self::resolve`]: the HDR target's
    /// pre-exposure changed by that factor since the history was written (FSR's `deltaPreExposure`).
    pub fn set_history_scale(&mut self, scale: f32) {
        self.history_scale = scale;
    }

    /// Drops the history; the next frame starts a fresh accumulation.
    pub fn invalidate(&mut self) {
        self.history_valid = false;
        self.prev_unjittered = None;
    }

    /// Records the seven passes. Returns the output-resolution HDR view (pre-exposed like the input).
    ///
    /// The view stays valid until the next call at a different size.
    pub fn resolve(
        &mut self,
        device: &wgpu::Device,
        belt: &mut crate::staging::StagingBelt,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &TsrInputs<'_>,
    ) -> Result<wgpu::TextureView, RenderError> {
        let _span = tracing::info_span!("fsr3").entered();
        let _crumb = crate::breadcrumbs::frame_pass("fsr3");
        let device = &crate::device::TrackedDevice::observe(device);
        let render = (inputs.render_size.0.max(1), inputs.render_size.1.max(1));
        let output = (inputs.output_size.0.max(1), inputs.output_size.1.max(1));
        self.ensure(device, render, output)?;
        if inputs.reset {
            self.invalidate();
        }
        if !self.history_valid {
            self.frame_index = 0;
        }
        let (Some(pipes), Some(targets)) = (self.pipes.as_ref(), self.targets.as_ref()) else {
            return Err(RenderError::Shader("fsr3 resources missing".into()));
        };

        let cur_world = inputs.unjittered_view_proj.as_dmat4();
        let prev_world = self.prev_unjittered.unwrap_or(cur_world);
        let eye = DVec3::new(
            f64::from(inputs.eye[0]),
            f64::from(inputs.eye[1]),
            f64::from(inputs.eye[2]),
        );
        let to_eye = DMat4::from_translation(eye);
        let cur_eye = cur_world * to_eye;
        let inv_jittered = eye_relative(inputs.view_proj, inputs.eye).inverse();
        let settings = self.settings;
        let offset = sample_offset_px(inputs.jitter, render.0, render.1);
        // FSR's jitter is where the sample sits relative to the texel centre: the opposite sign.
        let jitter = [-offset[0], -offset[1]];
        let prev_jitter = if self.history_valid {
            self.prev_jitter
        } else {
            jitter
        };
        let history_scale = std::mem::take(&mut self.history_scale);
        let depth = device_to_view_depth(cur_eye);
        let sharpen = settings.sharpness > 0.0;
        let frame = Fsr3Frame {
            render_size: [
                render.0 as f32,
                render.1 as f32,
                1.0 / render.0 as f32,
                1.0 / render.1 as f32,
            ],
            upscale_size: [
                output.0 as f32,
                output.1 as f32,
                1.0 / output.0 as f32,
                1.0 / output.1 as f32,
            ],
            previous_sizes: [render.0 as f32, render.1 as f32, output.0 as f32, output.1 as f32],
            jitter: [jitter[0], jitter[1], prev_jitter[0], prev_jitter[1]],
            depth: [
                depth[0],
                depth[1],
                if inputs.reactive.is_some() { 1.0 } else { 0.0 },
                self.frame_index.min(1_000_000) as f32,
            ],
            tune: [
                inputs.exposure.max(1.0e-12),
                if history_scale > 0.0 { history_scale } else { 1.0 },
                jitter_phase_count(render.0, output.0) as f32,
                settings.velocity_factor.max(0.0),
            ],
            tune2: [
                settings.accumulation_added_per_frame.clamp(0.0, 1.0),
                settings.min_disocclusion_accumulation.clamp(-1.0, 0.0),
                if sharpen {
                    rcas_strength(settings.sharpness)
                } else {
                    0.0
                },
                settings.debug_view.min(6) as f32,
            ],
            tune3: [
                settings.reactiveness_scale.max(0.0),
                settings.shading_change_scale.max(0.0),
                0.0,
                0.0,
            ],
            inv_jittered: inv_jittered.as_mat4().to_cols_array_2d(),
            cur_vp: cur_eye.as_mat4().to_cols_array_2d(),
            prev_vp: (prev_world * to_eye).as_mat4().to_cols_array_2d(),
            velocity_prev_vp: (inputs.velocity_prev_view_proj.as_dmat4() * to_eye)
                .as_mat4()
                .to_cols_array_2d(),
        };
        belt.write(device, encoder, &pipes.params, 0, bytemuck::bytes_of(&frame));
        encoder.clear_buffer(&targets.reconstructed_depth, 0, None);
        // `new_locks` is read for every output pixel by accumulate and only written
        // where prepare found a thin feature: the first frame on a fresh buffer (every
        // resolve after a size change rebuilds one) must not read undefined contents.
        encoder.clear_buffer(&targets.new_locks, 0, None);

        let dst = self.which;
        let velocity = inputs.velocity.unwrap_or(&pipes.dummy.view).clone();
        let reactive = inputs.reactive.unwrap_or(&pipes.dummy.view).clone();
        let key = (
            inputs.current.clone(),
            inputs.depth.clone(),
            velocity.clone(),
            reactive.clone(),
            dst,
        );
        let groups = self.groups.get_or_insert_with(key, || {
            build_groups(
                device,
                pipes,
                targets,
                inputs.current,
                inputs.depth,
                &velocity,
                &reactive,
                dst,
            )
        });

        let groups_of = |n: u32, size: u32| n.div_ceil(size).max(1);
        let half = half_size(render);
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("fsr3"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&pipes.prepare);
            pass.counted_set_bind_group(0, &groups.prepare, &[]);
            pass.counted_dispatch(groups_of(render.0, 8), groups_of(render.1, 8), 1);
            pass.counted_set_pipeline(&pipes.pyramid);
            pass.counted_set_bind_group(0, &groups.pyramid, &[]);
            pass.counted_dispatch(groups_of(half.0, 8), groups_of(half.1, 8), 1);
            pass.counted_set_pipeline(&pipes.shading);
            pass.counted_set_bind_group(0, &groups.shading, &[]);
            pass.counted_dispatch(groups_of(half.0, 8), groups_of(half.1, 8), 1);
            pass.counted_set_pipeline(&pipes.reactivity);
            pass.counted_set_bind_group(0, &groups.reactivity, &[]);
            pass.counted_dispatch(groups_of(render.0, 8), groups_of(render.1, 8), 1);
            pass.counted_set_pipeline(&pipes.instability);
            pass.counted_set_bind_group(0, &groups.instability, &[]);
            pass.counted_dispatch(groups_of(render.0, 8), groups_of(render.1, 8), 1);
            pass.counted_set_pipeline(&pipes.accumulate);
            pass.counted_set_bind_group(0, &groups.accumulate, &[]);
            pass.counted_dispatch(groups_of(output.0, 8), groups_of(output.1, 8), 1);
            if sharpen && settings.debug_view == 0 {
                pass.counted_set_pipeline(&pipes.rcas);
                pass.counted_set_bind_group(0, &groups.rcas, &[]);
                pass.counted_dispatch(groups_of(output.0, 8), groups_of(output.1, 8), 1);
            }
        }
        let out = targets.output_tex.view.clone();
        self.which = 1 - dst;
        self.history_valid = true;
        self.frame_index = self.frame_index.saturating_add(1);
        self.prev_unjittered = Some(cur_world);
        self.prev_jitter = jitter;
        #[cfg(feature = "frame-gen")]
        {
            self.resolves += 1;
        }
        Ok(out)
    }

    /// The surfaces frame generation reads after this frame's [`Self::resolve`] (ADR-514): the
    /// dilated motion and depth and the reconstructed previous depth. `None` before the first
    /// resolve at the current sizes.
    #[cfg(feature = "frame-gen")]
    #[must_use]
    pub fn frame_gen_surfaces(&self) -> Option<crate::frame_gen::UpscalerSurfaces<'_>> {
        let targets = self.targets.as_ref()?;
        self.history_valid.then_some(crate::frame_gen::UpscalerSurfaces {
            render_size: targets.render,
            display_size: targets.output,
            dilated_motion_depth: &targets.motion.view,
            reconstructed_previous_depth: &targets.reconstructed_depth,
            resolves: self.resolves,
        })
    }

    fn ensure(
        &mut self,
        device: &wgpu::Device,
        render: (u32, u32),
        output: (u32, u32),
    ) -> Result<(), RenderError> {
        if self.pipes.is_none() {
            self.pipes = Some(build_pipelines(device)?);
        }
        if self
            .targets
            .as_ref()
            .is_none_or(|t| t.render != render || t.output != output)
        {
            self.targets = Some(build_targets(device, render, output));
            self.history_valid = false;
            self.groups.clear();
        }
        Ok(())
    }
}

/// Shading change size: FSR's `int(RenderSize * 0.5)`, at least 1.
fn half_size(render: (u32, u32)) -> (u32, u32) {
    ((render.0 / 2).max(1), (render.1 / 2).max(1))
}

fn build_targets(device: &wgpu::Device, render: (u32, u32), output: (u32, u32)) -> Targets {
    use wgpu::TextureFormat as F;
    let half = half_size(render);
    let quarter = ((half.0 / 2).max(1), (half.1 / 2).max(1));
    let eighth = ((quarter.0 / 2).max(1), (quarter.1 / 2).max(1));
    let buffer = |label: &str, texels: (u32, u32)| {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some(label),
            size: u64::from(texels.0) * u64::from(texels.1) * 4,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    };
    Targets {
        render,
        output,
        motion: target(device, render, F::Rgba32Float, "fsr3 dilated motion"),
        luma: [
            target(device, render, F::Rgba16Float, "fsr3 luma 0"),
            target(device, render, F::Rgba16Float, "fsr3 luma 1"),
        ],
        reconstructed_depth: buffer("fsr3 reconstructed previous depth", render),
        mips: [
            target(device, half, F::Rgba16Float, "fsr3 pyramid 0"),
            target(device, quarter, F::Rgba16Float, "fsr3 pyramid 1"),
            target(device, eighth, F::Rgba16Float, "fsr3 pyramid 2"),
        ],
        shading_change: target(device, half, F::Rgba8Unorm, "fsr3 shading change"),
        masks: target(device, render, F::Rgba8Unorm, "fsr3 dilated reactive masks"),
        accumulation: [
            target(device, render, F::Rgba8Unorm, "fsr3 accumulation 0"),
            target(device, render, F::Rgba8Unorm, "fsr3 accumulation 1"),
        ],
        new_locks: buffer("fsr3 new locks", output),
        luma_history: [
            target(device, render, F::Rgba16Float, "fsr3 luma history 0"),
            target(device, render, F::Rgba16Float, "fsr3 luma history 1"),
        ],
        instability: target(device, render, F::Rgba8Unorm, "fsr3 luma instability"),
        history: [
            target(device, output, F::Rgba16Float, "fsr3 history 0"),
            target(device, output, F::Rgba16Float, "fsr3 history 1"),
        ],
        // COPY_SRC: the HDR capture and `before_post` passes copy the post stack's input.
        output_tex: target_usage(
            device,
            output,
            F::Rgba16Float,
            "fsr3 output",
            wgpu::TextureUsages::COPY_SRC,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn build_groups(
    device: &wgpu::Device,
    pipes: &Pipelines,
    t: &Targets,
    color: &wgpu::TextureView,
    depth: &wgpu::TextureView,
    velocity: &wgpu::TextureView,
    reactive: &wgpu::TextureView,
    dst: usize,
) -> Groups {
    let src = 1 - dst;
    let params = pipes.params.as_entire_binding();
    let sampler = wgpu::BindingResource::Sampler(&pipes.sampler);
    let group = |label: &str, layout: &wgpu::BindGroupLayout, entries: &[wgpu::BindingResource<'_>]| {
        let entries: Vec<wgpu::BindGroupEntry<'_>> = entries
            .iter()
            .enumerate()
            .map(|(i, resource)| wgpu::BindGroupEntry {
                binding: i as u32,
                resource: resource.clone(),
            })
            .collect();
        crate::device::TrackedDevice::observe(device).create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some(label),
            layout,
            entries: &entries,
        })
    };
    let view = wgpu::BindingResource::TextureView;
    Groups {
        prepare: group(
            "fsr3 prepare inputs",
            &pipes.prepare_bgl,
            &[
                params.clone(),
                view(depth),
                view(velocity),
                view(color),
                view(&t.motion.view),
                view(&t.luma[dst].view),
                t.reconstructed_depth.as_entire_binding(),
            ],
        ),
        pyramid: group(
            "fsr3 pyramid",
            &pipes.pyramid_bgl,
            &[
                params.clone(),
                view(&t.luma[dst].view),
                view(&t.luma[src].view),
                view(&t.motion.view),
                view(&t.mips[0].view),
                view(&t.mips[1].view),
                view(&t.mips[2].view),
            ],
        ),
        shading: group(
            "fsr3 shading change",
            &pipes.shading_bgl,
            &[
                params.clone(),
                view(&t.mips[0].view),
                view(&t.mips[1].view),
                view(&t.mips[2].view),
                sampler.clone(),
                view(&t.shading_change.view),
            ],
        ),
        reactivity: group(
            "fsr3 prepare reactivity",
            &pipes.reactivity_bgl,
            &[
                params.clone(),
                view(&t.motion.view),
                t.reconstructed_depth.as_entire_binding(),
                view(reactive),
                view(&t.luma[dst].view),
                view(&t.shading_change.view),
                view(&t.accumulation[src].view),
                sampler.clone(),
                view(&t.masks.view),
                view(&t.accumulation[dst].view),
                t.new_locks.as_entire_binding(),
            ],
        ),
        instability: group(
            "fsr3 luma instability",
            &pipes.instability_bgl,
            &[
                params.clone(),
                view(&t.motion.view),
                view(&t.masks.view),
                view(&t.luma[dst].view),
                view(&t.luma_history[src].view),
                sampler.clone(),
                view(&t.luma_history[dst].view),
                view(&t.instability.view),
            ],
        ),
        accumulate: group(
            "fsr3 accumulate",
            &pipes.accumulate_bgl,
            &[
                params.clone(),
                view(color),
                view(&t.motion.view),
                view(&t.masks.view),
                view(&t.instability.view),
                view(&t.mips[0].view),
                view(&t.history[src].view),
                sampler.clone(),
                t.new_locks.as_entire_binding(),
                view(&t.history[dst].view),
                view(&t.output_tex.view),
            ],
        ),
        rcas: group(
            "fsr3 rcas",
            &pipes.rcas_bgl,
            &[params, view(&t.history[dst].view), view(&t.output_tex.view)],
        ),
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
        usage: wgpu::TextureUsages::STORAGE_BINDING | wgpu::TextureUsages::TEXTURE_BINDING | extra,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    Tex { texture, view }
}

/// Every pass's WGSL with the shared module prepended, `(file name, source)`.
pub(crate) fn checked_sources() -> Vec<(&'static str, &'static str)> {
    static SOURCES: std::sync::OnceLock<Vec<(&'static str, String)>> = std::sync::OnceLock::new();
    SOURCES
        .get_or_init(|| {
            [
                ("fsr3_prepare_inputs.wgsl", PREPARE_INPUTS_WGSL),
                ("fsr3_pyramid.wgsl", PYRAMID_WGSL),
                ("fsr3_shading_change.wgsl", SHADING_CHANGE_WGSL),
                ("fsr3_prepare_reactivity.wgsl", PREPARE_REACTIVITY_WGSL),
                ("fsr3_luma_instability.wgsl", LUMA_INSTABILITY_WGSL),
                ("fsr3_accumulate.wgsl", ACCUMULATE_WGSL),
                ("fsr3_rcas.wgsl", RCAS_WGSL),
            ]
            .into_iter()
            .map(|(name, pass)| (name, format!("{COMMON_WGSL}\n{pass}")))
            .collect()
        })
        .iter()
        .map(|(name, source)| (*name, source.as_str()))
        .collect()
}

fn compute_pipeline(
    device: &wgpu::Device,
    label: &str,
    pass: &str,
    entries: &[wgpu::BindGroupLayoutEntry],
) -> Result<(wgpu::ComputePipeline, wgpu::BindGroupLayout), RenderError> {
    let source = format!("{COMMON_WGSL}\n{pass}");
    let module = compose_source(&source, label, HashMap::new())?;
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
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some(label),
        layout: Some(&layout),
        module: &module,
        entry_point: Some("cs_main"),
        compilation_options: Default::default(),
        cache: crate::pipeline_cache::cache_for(&device).as_deref(),
    });
    Ok((pipeline, bgl))
}

fn entry(binding: u32, ty: wgpu::BindingType) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::COMPUTE,
        ty,
        count: None,
    }
}

fn uniform(binding: u32) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Uniform,
            has_dynamic_offset: false,
            min_binding_size: None,
        },
    )
}

fn texture(binding: u32, filterable: bool) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
    )
}

fn depth_texture(binding: u32) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Depth,
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
    )
}

fn storage_texture(binding: u32, format: wgpu::TextureFormat) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::BindingType::StorageTexture {
            access: wgpu::StorageTextureAccess::WriteOnly,
            format,
            view_dimension: wgpu::TextureViewDimension::D2,
        },
    )
}

fn storage_buffer(binding: u32, read_only: bool) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::BindingType::Buffer {
            ty: wgpu::BufferBindingType::Storage { read_only },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
    )
}

fn sampler(binding: u32) -> wgpu::BindGroupLayoutEntry {
    entry(
        binding,
        wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
    )
}

fn build_pipelines(device: &wgpu::Device) -> Result<Pipelines, RenderError> {
    use wgpu::TextureFormat as F;
    let (prepare, prepare_bgl) = compute_pipeline(
        device,
        "fsr3 prepare inputs",
        PREPARE_INPUTS_WGSL,
        &[
            uniform(0),
            depth_texture(1),
            texture(2, false),
            texture(3, false),
            storage_texture(4, F::Rgba32Float),
            storage_texture(5, F::Rgba16Float),
            storage_buffer(6, false),
        ],
    )?;
    let (pyramid, pyramid_bgl) = compute_pipeline(
        device,
        "fsr3 pyramid",
        PYRAMID_WGSL,
        &[
            uniform(0),
            texture(1, false),
            texture(2, false),
            texture(3, false),
            storage_texture(4, F::Rgba16Float),
            storage_texture(5, F::Rgba16Float),
            storage_texture(6, F::Rgba16Float),
        ],
    )?;
    let (shading, shading_bgl) = compute_pipeline(
        device,
        "fsr3 shading change",
        SHADING_CHANGE_WGSL,
        &[
            uniform(0),
            texture(1, true),
            texture(2, true),
            texture(3, true),
            sampler(4),
            storage_texture(5, F::Rgba8Unorm),
        ],
    )?;
    let (reactivity, reactivity_bgl) = compute_pipeline(
        device,
        "fsr3 prepare reactivity",
        PREPARE_REACTIVITY_WGSL,
        &[
            uniform(0),
            texture(1, false),
            storage_buffer(2, true),
            texture(3, false),
            texture(4, false),
            texture(5, true),
            texture(6, true),
            sampler(7),
            storage_texture(8, F::Rgba8Unorm),
            storage_texture(9, F::Rgba8Unorm),
            storage_buffer(10, false),
        ],
    )?;
    let (instability, instability_bgl) = compute_pipeline(
        device,
        "fsr3 luma instability",
        LUMA_INSTABILITY_WGSL,
        &[
            uniform(0),
            texture(1, false),
            texture(2, true),
            texture(3, true),
            texture(4, true),
            sampler(5),
            storage_texture(6, F::Rgba16Float),
            storage_texture(7, F::Rgba8Unorm),
        ],
    )?;
    let (accumulate, accumulate_bgl) = compute_pipeline(
        device,
        "fsr3 accumulate",
        ACCUMULATE_WGSL,
        &[
            uniform(0),
            texture(1, false),
            texture(2, false),
            texture(3, true),
            texture(4, true),
            texture(5, true),
            texture(6, false),
            sampler(7),
            storage_buffer(8, false),
            storage_texture(9, F::Rgba16Float),
            storage_texture(10, F::Rgba16Float),
        ],
    )?;
    let (rcas, rcas_bgl) = compute_pipeline(
        device,
        "fsr3 rcas",
        RCAS_WGSL,
        &[uniform(0), texture(1, false), storage_texture(2, F::Rgba16Float)],
    )?;
    let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some("fsr3 linear clamp"),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: wgpu::FilterMode::Linear,
        min_filter: wgpu::FilterMode::Linear,
        ..Default::default()
    });
    let params = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("fsr3 frame"),
        size: std::mem::size_of::<Fsr3Frame>() as u64,
        usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let dummy = target(device, (1, 1), F::Rgba16Float, "fsr3 zero");
    Ok(Pipelines {
        prepare,
        prepare_bgl,
        pyramid,
        pyramid_bgl,
        shading,
        shading_bgl,
        reactivity,
        reactivity_bgl,
        instability,
        instability_bgl,
        accumulate,
        accumulate_bgl,
        rcas,
        rcas_bgl,
        sampler,
        params,
        dummy,
    })
}

/// Shared constants, frame uniform and helpers.
pub(crate) const COMMON_WGSL: &str = include_str!("fsr3_common.wgsl");
/// Pass 1 source.
pub(crate) const PREPARE_INPUTS_WGSL: &str = include_str!("fsr3_prepare_inputs.wgsl");
/// Pass 2 source.
pub(crate) const PYRAMID_WGSL: &str = include_str!("fsr3_pyramid.wgsl");
/// Pass 3 source.
pub(crate) const SHADING_CHANGE_WGSL: &str = include_str!("fsr3_shading_change.wgsl");
/// Pass 4 source.
pub(crate) const PREPARE_REACTIVITY_WGSL: &str = include_str!("fsr3_prepare_reactivity.wgsl");
/// Pass 5 source.
pub(crate) const LUMA_INSTABILITY_WGSL: &str = include_str!("fsr3_luma_instability.wgsl");
/// Pass 6 source.
pub(crate) const ACCUMULATE_WGSL: &str = include_str!("fsr3_accumulate.wgsl");
/// Pass 7 source.
pub(crate) const RCAS_WGSL: &str = include_str!("fsr3_rcas.wgsl");

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "fsr3_accumulate.wgsl,fsr3_luma_instability.wgsl,fsr3_prepare_inputs.wgsl,fsr3_prepare_reactivity.wgsl,fsr3_pyramid.wgsl,fsr3_rcas.wgsl,fsr3_shading_change.wgsl",
        "Fsr3Frame",
        Fsr3Frame {
            render_size,
            upscale_size,
            previous_sizes,
            jitter,
            depth,
            tune,
            tune2,
            tune3,
            inv_jittered,
            cur_vp,
            prev_vp,
            velocity_prev_vp
        }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shaders_compose_and_validate() {
        for (name, source) in checked_sources() {
            compose_source(source, name, HashMap::new()).unwrap_or_else(|err| panic!("{name}: {err}"));
        }
    }

    #[test]
    fn frame_uniform_matches_the_wgsl_layout() {
        // Eight vec4 and four mat4x4: 8 × 16 + 4 × 64 bytes.
        assert_eq!(std::mem::size_of::<Fsr3Frame>(), 384);
    }

    #[test]
    fn view_depth_factors_invert_the_projection() {
        use regia_core::glam::camera::rh::proj::directx;
        use regia_core::glam::{DVec3, Vec3};
        let view = regia_core::glam::camera::rh::view::look_at_mat4(
            Vec3::new(5000.0, 30.0, -2000.0),
            Vec3::new(5010.0, 28.0, -2010.0),
            Vec3::Y,
        );
        for proj in [
            directx::perspective(1.0, 1.5, 0.1, 1000.0),
            directx::perspective_infinite(1.0, 1.5, 0.1),
        ] {
            // In f64, as the renderer derives the factors (from the eye-relative matrix).
            let vp = (proj * view).as_dmat4();
            let [c0, c1] = device_to_view_depth(vp);
            for world in [
                DVec3::new(5004.0, 29.0, -2003.0),
                DVec3::new(5100.0, 20.0, -2090.0),
                DVec3::new(5000.5, 30.0, -2000.5),
            ] {
                let clip = vp * world.extend(1.0);
                let d = clip.z / clip.w;
                let view_depth = f64::from(c1) / (d - f64::from(c0));
                assert!(
                    (view_depth - clip.w).abs() < 1.0e-3 * clip.w,
                    "{view_depth} vs {} (d = {d})",
                    clip.w
                );
            }
        }
    }

    #[test]
    fn jitter_is_the_negated_sample_offset() {
        // FSR's `Jitter()` is the sample position relative to the texel centre; Regia Engine's
        // sample offset is where the jitter-free image sits relative to the texel: opposite.
        let jitter_ndc = [0.25 * 2.0 / 640.0, -0.125 * 2.0 / 360.0];
        let offset = sample_offset_px(jitter_ndc, 640, 360);
        let fsr = [-offset[0], -offset[1]];
        assert!(
            (fsr[0] - 0.25).abs() < 1.0e-6 && (fsr[1] - 0.125).abs() < 1.0e-6,
            "{fsr:?}"
        );
    }

    #[test]
    fn phase_count_and_rcas_strength_follow_the_sdk() {
        assert_eq!(jitter_phase_count(1920, 1920), 8);
        assert_eq!(jitter_phase_count(1280, 1920), 18);
        assert_eq!(jitter_phase_count(960, 1920), 32);
        assert!((rcas_strength(1.0) - 1.0).abs() < 1.0e-6);
        assert!((rcas_strength(0.0) - 0.25).abs() < 1.0e-6);
        assert!((rcas_strength(0.8) - 2f32.powf(-0.4)).abs() < 1.0e-6);
    }

    /// CPU mirror of `lanczos2_approx_sq` (FSR1's approximation of Lanczos-2 in x²).
    fn lanczos2_approx_sq(x2: f32) -> f32 {
        let x2 = x2.min(4.0);
        let a = 0.4 * x2 - 1.0;
        let b = 0.25 * x2 - 1.0;
        (25.0 / 16.0 * a * a - (25.0 / 16.0 - 1.0)) * (b * b)
    }

    fn lanczos2(x: f32) -> f32 {
        let x = x.abs().min(2.0);
        if x < 6.1e-5 {
            return 1.0;
        }
        let pi = std::f32::consts::PI;
        (pi * x).sin() / (pi * x) * ((0.5 * pi * x).sin() / (0.5 * pi * x))
    }

    #[test]
    fn approximate_lanczos_tracks_the_reference() {
        for i in 0..=40 {
            let x = i as f32 * 0.05;
            let err = (lanczos2_approx_sq(x * x) - lanczos2(x)).abs();
            assert!(err < 0.06, "x = {x}: {err}");
        }
        assert!((lanczos2_approx_sq(0.0) - 1.0).abs() < 1.0e-6);
        assert!(lanczos2_approx_sq(4.0).abs() < 1.0e-6);
    }

    /// CPU mirror of `update_accumulation` (prepare reactivity) with the default tuning.
    fn update_accumulation(previous: f32, shading_change: f32, disocclusion: f32) -> (f32, f32) {
        update_accumulation_with(Fsr3Settings::default(), previous, shading_change, disocclusion)
    }

    /// CPU mirror of `update_accumulation` (prepare reactivity) with tuning `s`.
    fn update_accumulation_with(
        s: Fsr3Settings,
        previous: f32,
        shading_change: f32,
        disocclusion: f32,
    ) -> (f32, f32) {
        let mut acc = previous;
        acc += (0.0 - acc) * shading_change;
        acc += (acc.min(s.min_disocclusion_accumulation) - acc) * disocclusion;
        if (acc * 100.0).round() <= 1.0 {
            acc = 0.0;
        }
        (acc, (acc + s.accumulation_added_per_frame).clamp(0.0, 1.0))
    }

    /// The accumulation each frame uses, from a restart, with tuning `s` (five frames).
    fn ramp(s: Fsr3Settings) -> Vec<f32> {
        let (used, mut stored) = update_accumulation_with(s, 0.0, 0.0, 0.0);
        let mut seen = vec![used];
        for _ in 0..4 {
            let (used, next) = update_accumulation_with(s, stored, 0.0, 0.0);
            seen.push(used);
            stored = next;
        }
        seen
    }

    #[test]
    fn accumulation_ramps_over_three_frames_and_restarts() {
        // The SDK's tuning (1/3 per frame, −1/3 on a disocclusion).
        let sdk = Fsr3Settings {
            accumulation_added_per_frame: 1.0 / 3.0,
            min_disocclusion_accumulation: -1.0 / 3.0,
            ..Fsr3Settings::default()
        };
        let seen = ramp(sdk);
        assert_eq!(seen[0], 0.0);
        assert!(
            (seen[1] - 1.0 / 3.0).abs() < 1.0e-6 && (seen[3] - 1.0).abs() < 1.0e-6,
            "{seen:?}"
        );
        // A shading change or a disocclusion restarts the count.
        assert_eq!(update_accumulation_with(sdk, 1.0, 1.0, 0.0).0, 0.0);
        assert_eq!(update_accumulation_with(sdk, 1.0, 0.0, 1.0).0, 0.0);
        // A partial disocclusion keeps part of the count with the SDK's tuning.
        assert!(update_accumulation_with(sdk, 1.0, 0.0, 0.64).0 > 0.1);
    }

    /// Regia Engine's default tuning (ADR-7678): four frames to full accumulation, and a pixel at
    /// least half disoccluded restarts (the chart's ground stripe, 1 m under a box 213 m away,
    /// reads 0.64 and kept 15 % of its history with the SDK's tuning).
    #[test]
    fn default_accumulation_ramps_over_four_frames_and_half_disocclusion_restarts() {
        let seen = ramp(Fsr3Settings::default());
        for (frame, want) in [0.0, 0.25, 0.5, 0.75, 1.0].into_iter().enumerate() {
            assert!((seen[frame] - want).abs() < 1.0e-6, "{seen:?}");
        }
        assert_eq!(update_accumulation(1.0, 1.0, 0.0).0, 0.0);
        assert_eq!(update_accumulation(1.0, 0.0, 1.0).0, 0.0);
        assert_eq!(update_accumulation(1.0, 0.0, 0.5).0, 0.0);
        assert_eq!(update_accumulation(1.0, 0.0, 0.64).0, 0.0);
        // A slight disocclusion still keeps most of the count.
        assert!(update_accumulation(1.0, 0.0, 0.1).0 > 0.7);
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
            assert_ne!(
                RenderProfile::from_preset(preset).temporal_upscaler,
                TemporalUpscaler::Fsr3,
                "{preset:?}: FSR 3.1 is opt-in (ADR-041)"
            );
        }
    }

    /// Seeds every `new_locks` entry with the maximum lock (2.0) — what an undefined buffer
    /// read can act as — and resolves a flat field with the lock debug view on. The first
    /// frame, and the first frame after an output-size change, must store lock 0: the pass
    /// clears `new_locks` before the compute passes.
    #[test]
    fn flat_field_locks_are_zero_on_the_first_frame() {
        use regia_core::glam::Mat4;
        let device = crate::test_device();
        let d = &device.device;
        // Two flat input views at `size` (views keep their textures alive).
        let input_views = |size: (u32, u32)| -> (wgpu::TextureView, wgpu::TextureView) {
            // A depth format cannot be `write_texture`'d: it is cleared to 0.5 with a
            // render pass.
            let flat = |format: wgpu::TextureFormat, fill: &[u8]| {
                let texture = d.create_texture(&wgpu::TextureDescriptor {
                    label: Some("fsr3 input"),
                    size: wgpu::Extent3d {
                        width: size.0,
                        height: size.1,
                        depth_or_array_layers: 1,
                    },
                    mip_level_count: 1,
                    sample_count: 1,
                    dimension: wgpu::TextureDimension::D2,
                    format,
                    usage: if format == wgpu::TextureFormat::Depth32Float {
                        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::RENDER_ATTACHMENT
                    } else {
                        wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST
                    },
                    view_formats: &[],
                });
                if format == wgpu::TextureFormat::Depth32Float {
                    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
                    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("fsr3 depth clear"),
                    });
                    {
                        let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: Some("fsr3 depth clear"),
                            color_attachments: &[],
                            depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                                view: &view,
                                depth_ops: Some(wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(0.5),
                                    store: wgpu::StoreOp::Store,
                                }),
                                stencil_ops: None,
                            }),
                            timestamp_writes: None,
                            occlusion_query_set: None,
                            multiview_mask: None,
                        });
                    }
                    wgpu::Queue::submit(&device.queue, [encoder.finish()]);
                    view
                } else {
                    device.queue.write_texture(
                        wgpu::TexelCopyTextureInfo {
                            texture: &texture,
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All,
                        },
                        &fill.repeat((size.0 * size.1) as usize),
                        wgpu::TexelCopyBufferLayout {
                            offset: 0,
                            bytes_per_row: Some(size.0 * fill.len() as u32),
                            rows_per_image: Some(size.1),
                        },
                        wgpu::Extent3d {
                            width: size.0,
                            height: size.1,
                            depth_or_array_layers: 1,
                        },
                    );
                    texture.create_view(&wgpu::TextureViewDescriptor::default())
                }
            };
            (
                // Grey 1.0 in f16, depth 0.5 in f32.
                flat(wgpu::TextureFormat::Rgba16Float, &[0, 0x3C, 0, 0, 0, 0, 0, 0]),
                flat(wgpu::TextureFormat::Depth32Float, &[0, 0, 0, 0x3F]),
            )
        };
        let mut pass = Fsr3Pass::default();
        pass.set_settings(Fsr3Settings {
            sharpness: 0.0,
            debug_view: 2,
            ..Fsr3Settings::default()
        });
        // Runs one resolve with `new_locks` pre-seeded to 2.0 after recording, then reads the
        // lock debug view back: R per texel, f16.
        let resolve_locks = |pass: &mut Fsr3Pass, render: (u32, u32), output: (u32, u32)| -> Vec<u8> {
            let (color, depth) = input_views(render);
            let inputs = TsrInputs {
                current: &color,
                depth: &depth,
                velocity: None,
                reactive: None,
                render_size: render,
                output_size: output,
                jitter: [0.0; 2],
                eye: [0.0; 3],
                view_proj: Mat4::IDENTITY,
                unjittered_view_proj: Mat4::IDENTITY,
                velocity_prev_view_proj: Mat4::IDENTITY,
                exposure: 1.0,
                reset: true,
                alternates: [None, None],
            };
            let mut belt = crate::staging::StagingBelt::default();
            belt.begin_frame();
            let mut encoder =
                d.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("fsr3") });
            pass.resolve(d, &mut belt, &mut encoder, &inputs)
                .expect("resolve");
            belt.upload(&crate::device::TrackedQueue::observe(&device.queue));
            let targets = pass.targets.as_ref().expect("targets");
            let garbage = [2.0f32.to_le_bytes()].repeat((output.0 * output.1) as usize);
            device
                .queue
                .write_buffer(&targets.new_locks, 0, bytemuck::cast_slice(&garbage));
            device.queue.submit([encoder.finish()]);
            // The debug view lands in the returned output texture.
            let texture = &targets.output_tex.texture;
            let stride = (output.0 * 8).div_ceil(256) * 256;
            let staging = d.create_buffer(&wgpu::BufferDescriptor {
                label: Some("fsr3 lock readback"),
                size: u64::from(stride) * u64::from(output.1),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("fsr3 readback"),
            });
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &staging,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(stride),
                        rows_per_image: Some(output.1),
                    },
                },
                wgpu::Extent3d {
                    width: output.0,
                    height: output.1,
                    depth_or_array_layers: 1,
                },
            );
            device.queue.submit([encoder.finish()]);
            let (tx, rx) = std::sync::mpsc::channel();
            staging.slice(..).map_async(wgpu::MapMode::Read, move |result| {
                let _ = tx.send(result);
            });
            device
                .tracked()
                .poll(wgpu::PollType::wait_indefinitely())
                .expect("poll");
            rx.recv().expect("callback").expect("map");
            let mapped = staging.slice(..).get_mapped_range().expect("range");
            let mut out = Vec::with_capacity((output.0 * output.1 * 8) as usize);
            for y in 0..output.1 {
                let row = &mapped[(y * stride) as usize..(y * stride + output.0 * 8) as usize];
                out.extend_from_slice(row);
            }
            drop(mapped);
            staging.unmap();
            out
        };
        let render = (32, 18);
        let read_lock = |texel: &[u8]| texel[0] == 0 && texel[1] == 0;
        // The first frame of a fresh buffer.
        let first = resolve_locks(&mut pass, render, (64, 36));
        assert!(
            first.as_chunks::<8>().0.iter().all(|t| read_lock(t)),
            "frame 0 stored a lock from undefined new_locks"
        );
        // The first frame after an output-size change rebuilds the targets.
        let resized = resolve_locks(&mut pass, render, (48, 27));
        assert!(
            resized.as_chunks::<8>().0.iter().all(|t| read_lock(t)),
            "the first frame on a new buffer stored a lock from undefined new_locks"
        );
    }
}
