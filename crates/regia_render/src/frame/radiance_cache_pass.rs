// SPDX-License-Identifier: MPL-2.0
//! The radiance cache's step in the screen-space GI pass (`specs/79` § 12, ADR-2085): it updates the
//! cells and hands the gather their read group. A module of its own so `screen_space.rs` keeps one call.

use regia_core::glam::Mat4;

use super::{FrameRenderer, FrameScene};
use crate::{RenderError, TrackedDevice};

impl FrameRenderer {
    /// Updates the radiance cache and returns the group the gather reads, `None` when this frame's
    /// gather is the plain one (the cache is off, the probes did not run, or this is not the main
    /// view: the cache is camera-centered on the main view).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn radiance_cache_group(
        &mut self,
        d: &TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        scene: &FrameScene<'_>,
        proj: Mat4,
        view_proj: Mat4,
        pre_exposure: f32,
        color_format: wgpu::TextureFormat,
        view_pass: bool,
    ) -> Result<Option<wgpu::BindGroup>, RenderError> {
        // A texture view or an XR eye: the plain gather, and the main view's status stands.
        if view_pass {
            return Ok(None);
        }
        if !self.radiance_cache.enabled() || !self.ddgi.active {
            self.radiance_cache.note_inactive();
            return Ok(None);
        }
        self.radiance_cache.ensure_pipes(d)?;
        if let Some(read_layout) = self.radiance_cache.read_layout() {
            self.highend.ensure_ssgi_cached(d, color_format, read_layout)?;
        }
        let (uniform, data, live) = self.ddgi.shading_buffers(d);
        if !live {
            self.radiance_cache.note_inactive();
            return Ok(None);
        }
        // The view from the jittered pair (the jitter cancels), the translation from the eye itself.
        let mut view_to_world = (proj.inverse() * view_proj).inverse();
        view_to_world.w_axis = regia_core::glam::Vec4::new(scene.eye[0], scene.eye[1], scene.eye[2], 1.0);
        let group = self.radiance_cache.record_update(
            d,
            &mut self.belt,
            encoder,
            &crate::radiance_cache::RadianceCacheFrame {
                eye: scene.eye,
                view_to_world: view_to_world.to_cols_array_2d(),
                pre_exposure,
                probes: (&uniform, &data),
                probe_spacing: self.ddgi.settings.spacing,
                field_changing: self.ddgi.settled_frames() < super::DDGI_REUSE_FRAMES,
            },
        )?;
        Ok(Some(group))
    }
}

/// The radiance cache's term of the static-screen key (ADR-2085, a step of `render_into` moved
/// here by ADR-1562's ceiling): the gather that reads the cache is another image, and a cache still
/// filling changes the gather every frame on the frames that read it (`gathers`: the main view's
/// SSGI over live probes).
pub(super) fn screen_key(
    cache: &crate::radiance_cache::RadianceCacheRuntime,
    gathers: bool,
    frame_index: u64,
) -> u64 {
    (u64::from(cache.enabled()) << 51)
        ^ if cache.enabled() && gathers && !cache.settled() {
            frame_index.wrapping_mul(0xc2b2_ae3d_27d4_eb4f)
        } else {
            0
        }
}
