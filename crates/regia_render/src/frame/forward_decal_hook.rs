// SPDX-License-Identifier: MPL-2.0
//! Forward decals on the renderer (ADR-3255): the host API and the per-view staging.
//!
//! The host hands this frame's decals ([`FrameRenderer::set_forward_decals`]) and the atlas
//! ([`FrameRenderer::set_forward_decal_atlas`]); every view stages its pack with its frame uniform,
//! so a view's forward pass reads tiles built for it. A translucent layer can feed the decals
//! instead (`TranslucentLayer::forward_decal_feed`: the VFX runtime does). Without the process switch
//! ([`crate::process_forward_decals`]) or on a device without room, nothing is kept or staged.

use super::FrameRenderer;
use crate::forward_decals::{
    ForwardDecal, ForwardDecalAtlas, ForwardDecalError, ForwardDecalStats, ForwardDecalView,
};

impl FrameRenderer {
    /// Sets the decals the next frame shades in the forward pass (replacing the last set).
    /// Returns `false` and keeps none while the process switch is off
    /// ([`crate::process_forward_decals`]).
    pub fn set_forward_decals(&mut self, decals: &[ForwardDecal]) -> bool {
        self.forward_decal_host.set_decals(decals)
    }

    /// Sets the forward decal atlas (staged on the next frame).
    ///
    /// # Errors
    /// [`ForwardDecalError::AtlasSize`] when the bytes do not match the layer count.
    pub fn set_forward_decal_atlas(&mut self, atlas: &ForwardDecalAtlas) -> Result<(), ForwardDecalError> {
        self.forward_decal_host.set_atlas(atlas)
    }

    /// The last staged pack's counts, or `None` when this renderer's device has no forward-decal
    /// binding (switch off, Android, or no room; no frame drawn yet).
    #[must_use]
    pub fn forward_decal_stats(&self) -> Option<ForwardDecalStats> {
        self.pipes.as_ref()?.forward_decals.as_ref().map(|b| b.stats())
    }

    /// Tells every translucent layer whether this device has the forward-decal binding and takes
    /// the feeds of the layers that supply forward decals (`TranslucentLayer::forward_decal_feed`).
    /// Asked per view; a feed is copied only when its generation moved, so later views cost a
    /// pointer compare. With several feeding layers the last one wins.
    fn pull_forward_decal_feeds(&mut self) {
        let binding = self.pipes.as_ref().is_some_and(|p| p.forward_decals.is_some());
        for layer in &self.translucent {
            if let Some(feed) = layer.forward_decal_feed(binding)
                && let Err(err) = self.forward_decal_host.take_feed(&feed)
            {
                tracing::warn!(target: "regia_render", "forward decal atlas from a layer: {err}");
            }
        }
    }

    /// Stages this view's pack on the frame's belt (`write_frame_uniforms`).
    pub(super) fn stage_forward_decals(
        &mut self,
        d: &crate::TrackedDevice,
        view: regia_core::glam::Mat4,
        view_proj: regia_core::glam::Mat4,
        eye: [f32; 3],
        screen: [f32; 4],
    ) {
        self.pull_forward_decal_feeds();
        let Some(bindings) = self.pipes.as_mut().and_then(|p| p.forward_decals.as_mut()) else {
            return;
        };
        let fd_view = ForwardDecalView {
            view_proj,
            view,
            eye: regia_core::glam::Vec3::from(eye),
            width: screen[0].max(1.0) as u32,
            height: screen[1].max(1.0) as u32,
        };
        let belt = &mut self.belt;
        bindings.stage(&self.forward_decal_host, &fd_view, |buffer, offset, bytes| {
            belt.defer(d, buffer, offset, bytes);
        });
    }
}
