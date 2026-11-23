// SPDX-License-Identifier: MPL-2.0
//! [`FrameRenderer::set_lights_views`]: the extra directional lights, ambient volumes and shader
//! debug view a frame draws (ADR-7211, [`crate::lights_views`]).

use super::FrameRenderer;

impl FrameRenderer {
    /// The extra directional lights, ambient volumes and forward-shader debug view of the following
    /// frames. Lights past [`crate::MAX_EXTRA_DIRECTIONAL`] and volumes past
    /// [`crate::MAX_AMBIENT_VOLUMES`] are dropped. The runtime calls it every frame with
    /// [`crate::ExtractedFrame::lights_views`]; only a change drops the static-screen composite.
    pub fn set_lights_views(&mut self, views: &crate::LightsViews) {
        let packed = views.uniform();
        if packed != self.lights_views {
            self.lights_views = packed;
            self.screen_cache_key = None;
        }
    }

    /// The packed lights-and-views rows the next frame uploads.
    #[must_use]
    pub fn lights_views_uniform(&self) -> crate::LightsViewsUniform {
        self.lights_views
    }
}
