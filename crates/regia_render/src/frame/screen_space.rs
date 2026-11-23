// SPDX-License-Identifier: MPL-2.0
//! The screen-space GI and reflection passes of `FrameRenderer::render_into` (ADR-1552 split by
//! pass): the SSGI composite, the stochastic or full-screen reflections over it, and the
//! static-screen cache that a held view reuses. Moved out of `frame.rs` unchanged. Nothing else
//! of the renderer is borrowed here, so this is a `&mut self` method.

use regia_core::glam::Mat4;

use super::{
    FrameRenderer, FrameScene, MEMORY_PRESSURE, SSSR_QUERY_BASE, ScreenCacheKey, temporal_noise_index,
};
use crate::{RenderError, TrackedDevice};

/// What the screen-space passes read from the frame.
pub(super) struct ScreenSpaceInputs<'a> {
    /// The frame's scene (planes, eye, exposure, reflection quality).
    pub(super) scene: &'a FrameScene<'a>,
    /// The lit linear HDR colour (the passes' source), `None` on a display-referred frame.
    pub(super) hdr_view: &'a Option<wgpu::TextureView>,
    /// The single-sample scene depth.
    pub(super) depth_view: &'a wgpu::TextureView,
    /// The glass front layer (depth, transmittance) when the frame drew one.
    pub(super) front_views: &'a Option<(wgpu::TextureView, wgpu::TextureView)>,
    /// White fallback for the front-layer transmittance.
    pub(super) white_view: &'a wgpu::TextureView,
    /// The frame's timestamp query set, when GPU timing is on.
    pub(super) timer: &'a Option<(super::FrameStamps, f32)>,
    /// The static-screen cache key of this frame, `None` when the cache is off.
    pub(super) screen_key: Option<ScreenCacheKey>,
    /// The projection without TAAU jitter.
    pub(super) unjittered_proj: Mat4,
    /// The jittered projection.
    pub(super) proj: Mat4,
    /// The jittered projection times the view.
    pub(super) view_proj: Mat4,
    /// Internal (pre-upscale) render size.
    pub(super) internal_w: u32,
    /// Internal (pre-upscale) render size.
    pub(super) internal_h: u32,
    /// The HDR colour format.
    pub(super) color_format: wgpu::TextureFormat,
    /// Pre-exposure of the HDR image.
    pub(super) pre_exposure: f32,
    /// Frame index since the last camera cut (temporal noise, SSSR sequence).
    pub(super) temporal_index: u64,
    /// The TAAU resolve shows its last output: nothing here runs.
    pub(super) taau_frozen: bool,
    /// The held view reuses the cached composite.
    pub(super) reuse_hit: bool,
    /// The frame can run screen-space passes (linear HDR, depth prepass, single sample).
    pub(super) screen_space: bool,
    /// The GI quality asks for the SSGI composite.
    pub(super) screen_space_gi: bool,
    /// TAAU is on this frame.
    pub(super) taau_on: bool,
    /// The SSGI gathers at half resolution.
    pub(super) ssgi_half: bool,
    /// The SSGI reuses its gather (held view).
    pub(super) gather_reuse: bool,
    /// The SSGI weighs by relative depth.
    pub(super) ssgi_relative: bool,
    /// The full-screen reflection march is wanted.
    pub(super) ssr_wanted: bool,
    /// The stochastic reflections run this frame.
    pub(super) sssr_frame: bool,
    /// The frame renders a probe or offscreen view (no memory trim).
    pub(super) view_pass: bool,
}

impl FrameRenderer {
    /// Records the SSGI composite and the reflections (stamps 7, `SSSR_QUERY_BASE..+3` and 8) and
    /// returns the shaded HDR image the outdoor passes and the resolve read: this frame's
    /// composite, the cached one on a reuse hit, or the plain HDR colour.
    pub(super) fn record_screen_space(
        &mut self,
        d: &TrackedDevice,
        encoder: &mut wgpu::CommandEncoder,
        inputs: &ScreenSpaceInputs<'_>,
    ) -> Result<Option<wgpu::TextureView>, RenderError> {
        let ScreenSpaceInputs {
            scene,
            hdr_view,
            depth_view,
            front_views,
            white_view,
            timer,
            screen_key,
            unjittered_proj,
            proj,
            view_proj,
            internal_w,
            internal_h,
            color_format,
            pre_exposure,
            temporal_index,
            taau_frozen,
            reuse_hit,
            screen_space,
            screen_space_gi,
            taau_on,
            ssgi_half,
            gather_reuse,
            ssgi_relative,
            ssr_wanted,
            sssr_frame,
            view_pass,
        } = *inputs;
        let mut shaded = hdr_view.clone();
        crate::pipeline_warm::warm_checkpoint()?;
        if taau_frozen {
            // The resolve shows its last output; nothing below reads this frame's composite.
        } else if reuse_hit {
            shaded = self.screen_cache_view.clone();
            if !view_pass {
                self.radiance_cache.note_held();
            }
        } else if screen_space
            && screen_space_gi
            && let Some(src) = shaded.clone()
        {
            let _span = tracing::info_span!("ssgi").entered();
            let _crumb = crate::breadcrumbs::frame_pass("ssgi");
            // The world-space radiance cache behind the gather (ADR-2085): off in every preset.
            let cache = self.radiance_cache_group(
                d,
                encoder,
                scene,
                proj,
                view_proj,
                pre_exposure,
                color_format,
                view_pass,
            )?;
            shaded = Some({
                let highend = &mut self.highend;
                let belt = &mut self.belt;
                highend.record_ssgi(
                    d,
                    belt,
                    encoder,
                    &src,
                    depth_view,
                    front_views.as_ref().map_or(white_view, |(_, t)| t),
                    internal_w,
                    internal_h,
                    color_format,
                    [
                        unjittered_proj.x_axis.x,
                        unjittered_proj.y_axis.y,
                        scene.near,
                        scene.far,
                    ],
                    temporal_noise_index(taau_on, temporal_index),
                    ssgi_half,
                    gather_reuse,
                    ssgi_relative,
                    cache.as_ref(),
                )?
            });
            self.screen_report.ssgi_composite = true;
            self.screen_report.ssgi_gather = self.highend.ssgi_gathered();
        } else if !view_pass {
            self.radiance_cache.note_inactive();
        }
        let want_reflections = !reuse_hit && !taau_frozen && (ssr_wanted || sssr_frame);
        // Frees the reflection targets after a run of frames with no reflection pass wanted
        // (a held view that reuses its composite still counts as wanted).
        self.highend.note_reflections(ssr_wanted || sssr_frame);
        let pressure = MEMORY_PRESSURE.load(std::sync::atomic::Ordering::Relaxed);
        if !view_pass && pressure != self.memory_pressure_seen {
            self.memory_pressure_seen = pressure;
            self.trim_memory();
        }
        let run_sssr = want_reflections && sssr_frame && shaded.is_some();
        if let Some((set_ts, _)) = timer {
            set_ts.write(encoder, 7);
            if !run_sssr {
                set_ts.write(encoder, SSSR_QUERY_BASE);
                set_ts.write(encoder, SSSR_QUERY_BASE + 1);
                set_ts.write(encoder, SSSR_QUERY_BASE + 2);
            }
        }
        crate::pipeline_warm::warm_checkpoint()?;
        // A screen-space miss inside a local capture's box keeps the forward pass's HDR capture
        // (ADR-2860); all zero without captures, which leaves both passes as before.
        self.highend
            .set_local_capture_boxes(self.studio.capture_center(), self.studio.capture_half());
        if run_sssr && let Some(src) = shaded.clone() {
            let _span = tracing::info_span!("sssr").entered();
            let _crumb = crate::breadcrumbs::frame_pass("sssr");
            self.screen_report.sssr = true;
            let probes = self.reflection_probes.clone();
            let exposure = crate::post::exposure_from_ev100(scene.exposure_ev).max(1.0e-6);
            let eye = regia_core::glam::Vec3::from_array(scene.eye);
            let forward = (regia_core::glam::Vec3::from_array(scene.target) - eye).normalize_or_zero();
            shaded = Some({
                let highend = &mut self.highend;
                let belt = &mut self.belt;
                highend.record_sssr(
                    d,
                    belt,
                    encoder,
                    &src,
                    depth_view,
                    internal_w,
                    internal_h,
                    color_format,
                    view_proj,
                    proj,
                    scene.eye,
                    forward.to_array(),
                    scene.near,
                    temporal_index,
                    &probes,
                    scene.quality.probe_budget(probes.len() as u32),
                    pre_exposure / exposure,
                    timer
                        .as_ref()
                        .map(|(set, _)| (set.lend(SSSR_QUERY_BASE, 3), SSSR_QUERY_BASE)),
                )?
            });
        } else if want_reflections
            && ssr_wanted
            && let Some(src) = shaded.clone()
        {
            let _span = tracing::info_span!("ssr").entered();
            let _crumb = crate::breadcrumbs::frame_pass("ssr");
            self.screen_report.ssr = true;
            let probes = self.reflection_probes.clone();
            let exposure = crate::post::exposure_from_ev100(scene.exposure_ev).max(1.0e-6);
            shaded = Some({
                let highend = &mut self.highend;
                let belt = &mut self.belt;
                highend.record_ssr(
                    d,
                    belt,
                    encoder,
                    &src,
                    depth_view,
                    internal_w,
                    internal_h,
                    color_format,
                    view_proj.inverse().to_cols_array_2d(),
                    view_proj.to_cols_array_2d(),
                    scene.eye,
                    &probes,
                    scene.quality.probe_budget(probes.len() as u32),
                    pre_exposure / exposure,
                    scene.quality.ssr,
                )?
            });
        }
        if !reuse_hit
            && !taau_frozen
            && let Some(key) = screen_key
            && let Some(view) = shaded.clone()
        {
            self.screen_cache_key = Some(key);
            self.screen_cache_view = Some(view);
        }
        if let Some((set_ts, _)) = timer {
            set_ts.write(encoder, 8);
        }
        Ok(shaded)
    }
}
