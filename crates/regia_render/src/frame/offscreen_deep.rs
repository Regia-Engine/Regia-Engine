// SPDX-License-Identifier: MPL-2.0
//! [`FrameRenderer::render_offscreen_deep`]: the offscreen capture path into a caller-owned
//! `Rgba16Float` target (`crate::deep_readout`, ADR-1404). Same frame as
//! [`FrameRenderer::render_offscreen`]; only the final image keeps 16 bits per channel.

use super::{FrameRenderer, FrameTarget};
use crate::deep_readout::{DEEP_FORMAT, DeepFrame, DeepTarget, read_deep};
use crate::{FrameScene, RenderDevice, RenderError};

impl FrameRenderer {
    /// Renders `scene` into `target` (an `Rgba16Float` [`DeepTarget`] the caller keeps at the
    /// scene's size) and reads it back as 16-bit sRGB-encoded codes.
    ///
    /// The post stack keeps its tonemapped image in half floats for a float target, so the result
    /// has no 8-bit step anywhere on the temporal and FXAA tiers (SMAA tiers keep SMAA's 8-bit
    /// blend; see `crate::deep_readout`). No window overlay or UI layer is drawn.
    ///
    /// # Errors
    ///
    /// `target` is not the scene's size ([`RenderError::Readback`]), or the frame or the readback
    /// failed.
    #[tracing::instrument(skip_all)]
    pub fn render_offscreen_deep(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        target: &DeepTarget,
    ) -> Result<DeepFrame, RenderError> {
        self.sync_shader_epoch();
        let (width, height) = (scene.width.max(1), scene.height.max(1));
        if !target.fits(width, height) {
            return Err(RenderError::Readback(format!(
                "deep target is {}x{}, the frame {width}x{height}",
                target.width, target.height
            )));
        }
        // As `begin_offscreen`: a captured frame takes the deterministic paths (sync probe
        // bakes, ordered cull, complete emissive fits) and puts them back after.
        let guard = super::offscreen::CaptureDeterminism::enter(self);
        let drawn = self.render_into(
            device,
            scene,
            FrameTarget {
                view: &target.view,
                format: DEEP_FORMAT,
                width,
                height,
                viewport: [0, 0, width, height],
                load_color: false,
                color: Some(&target.texture),
            },
        );
        guard.exit(self);
        drawn?;
        let frame = read_deep(device, target)?;
        // The read already waited: publish the timestamp slots, as `read_offscreen` does.
        self.collect_timings(device, 0);
        Ok(frame)
    }
}
