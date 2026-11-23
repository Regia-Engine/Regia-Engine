// SPDX-License-Identifier: MPL-2.0
//! Frame generation's view of the main view (ADR-514, ADR-2067; feature `frame-gen`): the FSR 3.1
//! upscaler's surfaces, and what the frame hands the prepare-inputs pass from the temporal seam.

use regia_core::glam::Mat4;

use super::FrameRenderer;
use super::temporal::TemporalInputs;

/// What the frame's temporal seam hands frame generation (`FrameRenderer::frame_gen_inputs`).
#[derive(Debug, Clone)]
pub struct FrameGenInputs {
    /// The single-sample scene depth.
    pub depth: wgpu::TextureView,
    /// The velocity target, when this frame's temporal pass wrote it.
    pub velocity: Option<wgpu::TextureView>,
    /// The reactive or transparency mask of the upscaler seam, when the frame has one.
    pub reactive: Option<wgpu::TextureView>,
    /// The size the scene rendered at.
    pub render_size: (u32, u32),
    /// Jittered view-projection (world space).
    pub view_proj: Mat4,
    /// Jitter-free view-projection (world space).
    pub unjittered_view_proj: Mat4,
    /// The previous view-projection the velocity pass used.
    pub velocity_prev_view_proj: Mat4,
    /// Camera position.
    pub eye: [f32; 3],
    /// Vertical field of view (radians).
    pub fov_y: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
}

/// The renderer's side of the seam: off (nothing kept) unless frame generation asked for it.
#[derive(Default)]
pub(super) struct FrameGenCapture {
    wanted: bool,
    inputs: Option<FrameGenInputs>,
    reactive: Option<wgpu::TextureView>,
}

impl FrameRenderer {
    /// The FSR 3.1 surfaces a frame interpolator reads for the main (window) view: dilated motion
    /// and depth and the reconstructed previous depth, as of the last frame this renderer drew.
    ///
    /// `None` unless FSR 3.1 is the temporal upscaler and has resolved a frame at the current
    /// sizes. Texture views swap their own upscaler in and out during their frame, so between
    /// frames this is always the window's.
    #[must_use]
    pub fn frame_gen_surfaces(&self) -> Option<crate::frame_gen::UpscalerSurfaces<'_>> {
        if self.temporal_upscaler != crate::TemporalUpscaler::Fsr3 {
            return None;
        }
        self.fsr3_pass.frame_gen_surfaces()
    }

    /// Keeps (or stops keeping) the main view's inputs for frame generation.
    pub fn set_frame_gen_capture(&mut self, on: bool) {
        self.frame_gen_capture.wanted = on;
        if !on {
            self.frame_gen_capture.inputs = None;
            self.frame_gen_capture.reactive = None;
        }
    }

    /// The main view's inputs of the last frame rendered with the capture on.
    #[must_use]
    pub fn frame_gen_inputs(&self) -> Option<&FrameGenInputs> {
        self.frame_gen_capture.inputs.as_ref()
    }

    /// The upscaler seam's masks of this frame (the reactive one wins, as TSR and FSR 3.1 fold the
    /// transparency mask into it).
    pub(super) fn frame_gen_note_masks(
        &mut self,
        reactive: Option<&wgpu::TextureView>,
        transparency: Option<&wgpu::TextureView>,
    ) {
        if self.frame_gen_capture.wanted {
            self.frame_gen_capture.reactive = reactive.or(transparency).cloned();
        }
    }

    /// The temporal seam of this frame, after the velocity pass. Texture views and probe captures
    /// run this too; only the main view counts.
    pub(super) fn frame_gen_note_temporal(&mut self, inputs: &TemporalInputs<'_>) {
        if !self.frame_gen_capture.wanted || self.view_pass.is_some() || self.capturing_probe {
            return;
        }
        let velocity = if inputs.taau_on {
            self.taau_pass.velocity_view()
        } else {
            None
        };
        self.frame_gen_capture.inputs = Some(FrameGenInputs {
            depth: inputs.depth_view.clone(),
            velocity,
            reactive: self.frame_gen_capture.reactive.take(),
            render_size: (inputs.internal_w, inputs.internal_h),
            view_proj: inputs.view_proj,
            unjittered_view_proj: inputs.unjittered_vp,
            velocity_prev_view_proj: Mat4::from_cols_array_2d(&inputs.prev_vp),
            eye: inputs.scene.eye,
            fov_y: inputs.scene.fov_y,
            near: inputs.scene.near,
            far: inputs.scene.far,
        });
    }
}
