// SPDX-License-Identifier: MPL-2.0
//! The quick layer's window overlay pass: draws the frame's [`ImmediateBatch`] after
//! tonemapping, under the runtime UI.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_render::{
    ImmediateBatch, ImmediateRenderer, ImmediateState, ImmediateTarget, OverlayDraw, RenderDevice,
};

use crate::frame::FrameParams;
use crate::prelude_internal::*;

/// What the overlay draws: the batch built by extraction and the GPU renderer.
pub struct QuickOverlayState {
    /// GPU state (pipelines, buffers, images).
    pub renderer: ImmediateRenderer,
    /// This frame's triangles.
    pub batch: ImmediateBatch,
    /// Pixel density the batch was built for.
    pub scale_factor: f32,
    /// Logical window size the batch was tessellated for. Zero until a frame is built.
    ///
    /// A frozen frame keeps this size. The overlay projects with it, so a later change of
    /// pixel density does not move drawings that were baked in the old logical coordinates.
    pub logical_size: Vec2,
    /// Physical pixel size the batch's scissors were baked for. `(0, 0)` until a frame is built.
    pub built_physical: (u32, u32),
    /// Anti-aliasing samples (1 = none).
    pub samples: u32,
    /// Bumped when extraction rebuilds [`Self::batch`]. A frozen frame leaves it unchanged so
    /// the overlay can skip re-uploading the same vertex bytes.
    pub batch_generation: u64,
}

impl Default for QuickOverlayState {
    fn default() -> Self {
        Self {
            renderer: ImmediateRenderer::new(),
            batch: ImmediateBatch::new(),
            scale_factor: 1.0,
            logical_size: Vec2::ZERO,
            built_physical: (0, 0),
            samples: 4,
            batch_generation: 0,
        }
    }
}

impl QuickOverlayState {
    /// Clip-from-logical matrix for a target of `width` by `height` physical pixels.
    ///
    /// Uses [`Self::logical_size`] once a frame has been built, so a frozen batch keeps the
    /// projection it was tessellated with. Before the first frame, falls back to the target
    /// size divided by [`Self::scale_factor`].
    #[must_use]
    pub fn clip_from_local_for_target(&self, width: u32, height: u32) -> Mat4 {
        self.frame_params_for_target(width, height).clip_from_local()
    }

    /// Scissor rectangles of the current batch, scaled into a target of `width` by `height`
    /// physical pixels. `None` stays `None` (the whole target).
    #[must_use]
    pub fn scissors_for_target(&self, width: u32, height: u32) -> Vec<Option<[u32; 4]>> {
        let (sx, sy) = self.scissor_scale(width, height);
        self.batch
            .draws()
            .iter()
            .map(|draw| draw.state.scissor.map(|rect| scale_scissor(rect, sx, sy)))
            .collect()
    }

    fn frame_params_for_target(&self, width: u32, height: u32) -> FrameParams {
        let scale = self.scale_factor.max(1e-3);
        let size = if self.logical_size.x > 0.0 && self.logical_size.y > 0.0 {
            self.logical_size
        } else {
            Vec2::new(width as f32, height as f32) / scale
        };
        FrameParams {
            size,
            scale_factor: scale,
        }
    }

    /// `(x scale, y scale)` from the size scissors were baked for to `width`×`height`.
    fn scissor_scale(&self, width: u32, height: u32) -> (f32, f32) {
        let (bw, bh) = self.built_physical;
        if bw == 0 || bh == 0 || width == 0 || height == 0 {
            return (1.0, 1.0);
        }
        (width as f32 / bw as f32, height as f32 / bh as f32)
    }
}

/// Multiplies a physical scissor by `(sx, sy)`.
fn scale_scissor(rect: [u32; 4], sx: f32, sy: f32) -> [u32; 4] {
    let axis = |v: u32, s: f32| (v as f32 * s).round().max(0.0) as u32;
    [
        axis(rect[0], sx),
        axis(rect[1], sy),
        axis(rect[2], sx),
        axis(rect[3], sy),
    ]
}

fn scale_state(mut state: ImmediateState, sx: f32, sy: f32) -> ImmediateState {
    if let Some(rect) = state.scissor.as_mut() {
        *rect = scale_scissor(*rect, sx, sy);
    }
    state
}

/// A copy of `batch` whose scissor rectangles are multiplied by `(sx, sy)`.
fn scale_batch_scissors(batch: &ImmediateBatch, sx: f32, sy: f32) -> ImmediateBatch {
    let mut out = ImmediateBatch::new();
    out.push_vertices(batch.vertices());
    let mut covered = 0u32;
    for draw in batch.draws() {
        out.set_state(scale_state(draw.state, sx, sy));
        let range = draw.indices.start as usize..draw.indices.end as usize;
        out.push_indices(&batch.indices()[range]);
        covered = draw.indices.end;
    }
    let indices = batch.indices();
    if (indices.len() as u32) > covered {
        out.set_state(scale_state(batch.state(), sx, sy));
        out.push_indices(&indices[covered as usize..]);
    }
    out.flush();
    out
}

/// Shared handle to the overlay state (a resource; the overlay pass holds a clone).
#[derive(Resource, Clone, Default)]
pub struct QuickOverlay(pub Arc<Mutex<QuickOverlayState>>);

impl QuickOverlay {
    /// Locks the state (a poisoned lock is recovered: the batch is plain data).
    pub fn lock(&self) -> MutexGuard<'_, QuickOverlayState> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The [`OverlayDraw`] installed into [`regia_render::WindowOverlay`].
pub(crate) struct QuickOverlayPass(pub QuickOverlay);

impl OverlayDraw for QuickOverlayPass {
    fn draw(
        &mut self,
        device: &RenderDevice,
        encoder: &mut wgpu::CommandEncoder,
        view: &wgpu::TextureView,
        format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) {
        let _span = tracing::info_span!(target: "regia_quick", "quick_overlay_record").entered();
        let mut state = self.0.lock();
        if state.batch.is_empty() {
            return;
        }
        let state = &mut *state;
        let clip = state.clip_from_local_for_target(width, height);
        let (sx, sy) = state.scissor_scale(width, height);
        let target = ImmediateTarget {
            view,
            format,
            width,
            height,
        };
        let generation = state.batch_generation;
        let vertices = state.batch.vertices().len();
        let indices = state.batch.indices().len();
        state.renderer.arm_upload(generation, vertices, indices);
        let recorded = if sx != 1.0 || sy != 1.0 {
            let scaled = scale_batch_scissors(&state.batch, sx, sy);
            state
                .renderer
                .record(device, encoder, &target, &scaled, clip, state.samples)
        } else {
            state
                .renderer
                .record(device, encoder, &target, &state.batch, clip, state.samples)
        };
        if let Err(err) = recorded {
            tracing::warn!(target: "regia_quick", "quick draw pass skipped: {err}");
        }
    }
}
