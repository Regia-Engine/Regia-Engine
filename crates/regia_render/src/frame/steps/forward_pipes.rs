// SPDX-License-Identifier: MPL-2.0
//! The pipelines the frame's later passes draw with (ADR-2300): owned clones of the frame's
//! forward set and its cull-none set (the lite colour pipelines on the Mobile tiers), the layouts
//! and sampler the forward-side passes bind, and the `GLASS` sets of the draws that carry a glass
//! instance. Moved out of `FrameRenderer::render_into` unchanged.

use crate::frame::*;

/// Owned clones of the frame's pipelines, so `set` / `pipes` are not borrowed across
/// `&mut self.culler`.
pub(in crate::frame) struct FramePipeClones {
    /// `depth` pipeline (the frame's set).
    pub(in crate::frame) depth_pipe: wgpu::RenderPipeline,
    /// `color` pipeline (the frame's set).
    pub(in crate::frame) color_pipe: wgpu::RenderPipeline,
    /// `blend` pipeline (the frame's set).
    pub(in crate::frame) blend_pipe: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `premul` pipeline (the frame's set).
    pub(in crate::frame) premul_pipe: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `add` pipeline (the frame's set).
    pub(in crate::frame) add_pipe: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `xray` pipeline (the frame's set).
    pub(in crate::frame) xray_pipe: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `transmit_mul` pipeline (the frame's set).
    pub(in crate::frame) transmit_mul_pipe: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `transmit_add` pipeline (the frame's set).
    pub(in crate::frame) transmit_add_pipe: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `transmit_add_depth` pipeline (the frame's set).
    pub(in crate::frame) transmit_add_depth_pipe:
        crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `transmit_front` pipeline (the frame's set).
    pub(in crate::frame) transmit_front_pipe: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// 1×1 white: the SSGI composite's front-layer transmittance on frames without the layer.
    pub(in crate::frame) white_view: wgpu::TextureView,
    /// `transmit_refract` pipeline (the frame's set).
    pub(in crate::frame) transmit_refract_pipe: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// The screen-space refraction group layout.
    pub(in crate::frame) refract_bgl: wgpu::BindGroupLayout,
    /// The screen-space refraction sampler.
    pub(in crate::frame) refract_sampler: wgpu::Sampler,
    /// `sky` pipeline (the frame's set).
    pub(in crate::frame) sky_pipe: wgpu::RenderPipeline,
    /// `depth` pipeline (cull-none set).
    pub(in crate::frame) depth_pipe_ds: wgpu::RenderPipeline,
    /// `color` pipeline (cull-none set).
    pub(in crate::frame) color_pipe_ds: wgpu::RenderPipeline,
    /// `blend` pipeline (cull-none set).
    pub(in crate::frame) blend_pipe_ds: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `premul` pipeline (cull-none set).
    pub(in crate::frame) premul_pipe_ds: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `add` pipeline (cull-none set).
    pub(in crate::frame) add_pipe_ds: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `xray` pipeline (cull-none set).
    pub(in crate::frame) xray_pipe_ds: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `transmit_mul` pipeline (cull-none set).
    pub(in crate::frame) transmit_mul_pipe_ds: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `transmit_add` pipeline (cull-none set).
    pub(in crate::frame) transmit_add_pipe_ds: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `transmit_add_depth` pipeline (cull-none set).
    pub(in crate::frame) transmit_add_depth_pipe_ds:
        crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `transmit_front` pipeline (cull-none set).
    pub(in crate::frame) transmit_front_pipe_ds: crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// `transmit_refract` pipeline (cull-none set).
    pub(in crate::frame) transmit_refract_pipe_ds:
        crate::pipeline_warm::DeferredPipeline<wgpu::RenderPipeline>,
    /// The view group layout.
    pub(in crate::frame) view_bgl: wgpu::BindGroupLayout,
    /// The material group layout.
    pub(in crate::frame) mat_bgl: wgpu::BindGroupLayout,
    /// `GLASS` pipelines for the culled draws carrying a glass instance.
    pub(in crate::frame) glass_pipes: Option<(glass_gpu::GlassSet, wgpu::BindGroupLayout)>,
    /// `GLASS` pipelines for the double-sided (`Draw::cull_off`) draws carrying a glass instance.
    pub(in crate::frame) glass_pipes_ds: Option<(glass_gpu::GlassSet, wgpu::BindGroupLayout)>,
}

impl FramePipeClones {
    /// Clones the frame's forward sets (looked up again by key) and builds the `GLASS` sets the
    /// visible draws need.
    pub(in crate::frame) fn take(
        pipes: &mut Pipes,
        d: &crate::TrackedDevice,
        (key, key_ds): (PipelineKey, PipelineKey),
        any_cull_off: bool,
        lite_colors: &Option<[std::sync::Arc<forward_lite_gpu::LiteColor>; 2]>,
        draws: &[Draw],
    ) -> Result<Self, RenderError> {
        let (set, set_ds) = frame_sets(pipes, (key, key_ds), any_cull_off);
        // Clone pipelines before the culler so `set` / `pipes` are not borrowed
        // across `&mut self.culler`. `strategy` was chosen with the frame.
        let depth_pipe = set.depth.clone();
        let color_pipe = lite_colors
            .as_ref()
            .map_or_else(|| set.color.clone(), |l| l[0].opaque.clone());
        let blend_pipe = set.blend.clone();
        let premul_pipe = set.premul.clone();
        let add_pipe = set.add.clone();
        let xray_pipe = set.xray.clone();
        let transmit_mul_pipe = set.transmit_mul.clone();
        let transmit_add_pipe = set.transmit_add.clone();
        let transmit_add_depth_pipe = set.transmit_add_depth.clone();
        let transmit_front_pipe = set.transmit_front.clone();
        // 1×1 white: the SSGI composite's front-layer transmittance on frames without the layer.
        let white_view = pipes.white_view.clone();
        let transmit_refract_pipe = set.transmit_refract.clone();
        let refract_bgl = pipes.refract_bgl.clone();
        let refract_sampler = pipes.refract_sampler.clone();
        let sky_pipe = set.sky.clone();
        // Cull-none variants for double-sided materials (`Draw::cull_off`).
        let depth_pipe_ds = set_ds.depth.clone();
        let color_pipe_ds = lite_colors
            .as_ref()
            .map_or_else(|| set_ds.color.clone(), |l| l[1].opaque.clone());
        let blend_pipe_ds = set_ds.blend.clone();
        let premul_pipe_ds = set_ds.premul.clone();
        let add_pipe_ds = set_ds.add.clone();
        let xray_pipe_ds = set_ds.xray.clone();
        let transmit_mul_pipe_ds = set_ds.transmit_mul.clone();
        let transmit_add_pipe_ds = set_ds.transmit_add.clone();
        let transmit_add_depth_pipe_ds = set_ds.transmit_add_depth.clone();
        let transmit_front_pipe_ds = set_ds.transmit_front.clone();
        let transmit_refract_pipe_ds = set_ds.transmit_refract.clone();
        let view_bgl = pipes.view_bgl.clone();
        let mat_bgl = pipes.mat_bgl.clone();
        // `GLASS` pipelines for draws that carry a glass instance. A double-sided material in a
        // culled frame (`Draw::cull_off`) takes the cull-none set; [`FrameRenderer::prewarm_glass`]
        // builds both, so a prewarmed frame creates neither.
        let (need_culled, need_cull_off) = {
            let needs_glass = |cull_off: bool| {
                draws
                    .iter()
                    .any(|draw| draw.visible && draw.glass.is_some() && draw.cull_off == cull_off)
            };
            (needs_glass(false), needs_glass(true))
        };
        let glass_pipes = if need_culled {
            Some(
                pipes
                    .glass
                    .set(d, [&pipes.view_bgl, &pipes.object_bgl, &pipes.mat_bgl], key)?,
            )
        } else {
            None
        };
        let glass_pipes_ds = if need_cull_off {
            Some(
                pipes
                    .glass
                    .set(d, [&pipes.view_bgl, &pipes.object_bgl, &pipes.mat_bgl], key_ds)?,
            )
        } else {
            None
        };
        Ok(Self {
            depth_pipe,
            color_pipe,
            blend_pipe,
            premul_pipe,
            add_pipe,
            xray_pipe,
            transmit_mul_pipe,
            transmit_add_pipe,
            transmit_add_depth_pipe,
            transmit_front_pipe,
            white_view,
            transmit_refract_pipe,
            refract_bgl,
            refract_sampler,
            sky_pipe,
            depth_pipe_ds,
            color_pipe_ds,
            blend_pipe_ds,
            premul_pipe_ds,
            add_pipe_ds,
            xray_pipe_ds,
            transmit_mul_pipe_ds,
            transmit_add_pipe_ds,
            transmit_add_depth_pipe_ds,
            transmit_front_pipe_ds,
            transmit_refract_pipe_ds,
            view_bgl,
            mat_bgl,
            glass_pipes,
            glass_pipes_ds,
        })
    }

    /// The forward pass's pipelines: these clones, and the lite path's alpha-tested pipelines
    /// when the frame draws the lite tier.
    pub(in crate::frame) fn forward_pipes<'a>(
        &'a self,
        lite_colors: &'a Option<[std::sync::Arc<forward_lite_gpu::LiteColor>; 2]>,
    ) -> forward_pass::ForwardPipes<'a> {
        forward_pass::ForwardPipes {
            color_pipe: &self.color_pipe,
            color_pipe_ds: &self.color_pipe_ds,
            lite_tested: lite_colors.as_ref().map(|l| [&l[0].tested, &l[1].tested]),
            sky_pipe: &self.sky_pipe,
            add_pipe: &self.add_pipe,
            add_pipe_ds: &self.add_pipe_ds,
            blend_pipe: &self.blend_pipe,
            blend_pipe_ds: &self.blend_pipe_ds,
            premul_pipe: &self.premul_pipe,
            premul_pipe_ds: &self.premul_pipe_ds,
            xray_pipe: &self.xray_pipe,
            xray_pipe_ds: &self.xray_pipe_ds,
            transmit_add_depth_pipe: &self.transmit_add_depth_pipe,
            transmit_add_depth_pipe_ds: &self.transmit_add_depth_pipe_ds,
            transmit_add_pipe: &self.transmit_add_pipe,
            transmit_add_pipe_ds: &self.transmit_add_pipe_ds,
            transmit_front_pipe: &self.transmit_front_pipe,
            transmit_front_pipe_ds: &self.transmit_front_pipe_ds,
            transmit_mul_pipe: &self.transmit_mul_pipe,
            transmit_mul_pipe_ds: &self.transmit_mul_pipe_ds,
            transmit_refract_pipe: &self.transmit_refract_pipe,
            transmit_refract_pipe_ds: &self.transmit_refract_pipe_ds,
        }
    }
}

/// The frame's pipeline set and the set of its double-sided draws (`Draw::cull_off`; the frame's
/// own set when no draw is double-sided), looked up by key after `FrameRenderer::frame_pipelines`
/// inserted them.
pub(in crate::frame) fn frame_sets(
    pipes: &Pipes,
    (key, key_ds): (PipelineKey, PipelineKey),
    any_cull_off: bool,
) -> (&PipelineSet, &PipelineSet) {
    let set = pipes.sets.get(&key).expect("just inserted");
    let set_ds = if any_cull_off {
        pipes.sets.get(&key_ds).expect("just inserted")
    } else {
        set
    };
    (set, set_ds)
}
