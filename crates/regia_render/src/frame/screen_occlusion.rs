// SPDX-License-Identifier: MPL-2.0
//! The screen occlusion passes of `FrameRenderer::render_into` (ADR-1552 split by pass): the
//! probe irradiance lookups at the visible surfaces (DDGI and the baked volume), the SSAO kernel
//! and the contact shadows (or the sun trace in their place, ADR-2035), all over the single-sample
//! scene depth. Moved out of `frame.rs`
//! unchanged. The pass takes the renderer fields it touches ([`OcclusionFields`]) instead of
//! `self`, like the other passes recorded while the frame holds borrows of its own state.

use regia_core::glam::Mat4;

use super::{CONTACT_QUERY, DDGI_QUERY_BASE, FrameScene, ScreenPassReport, temporal_noise_index};
use crate::{RenderError, TrackedDevice};

/// The renderer fields the screen occlusion passes read and write.
pub(super) struct OcclusionFields<'a> {
    pub(super) ddgi: &'a mut crate::ddgi_gpu::DdgiRuntime,
    pub(super) baked_gi: &'a mut crate::baked_gi::BakedGiRuntime,
    pub(super) ssao_pass: &'a mut crate::ssao::SsaoPass,
    pub(super) blue_noise: Option<&'a std::sync::Arc<crate::blue_noise::BlueNoiseGpu>>,
    pub(super) contact_pass: &'a mut crate::contact_shadows::ContactShadowPass,
    pub(super) sun_trace: &'a mut super::sun_trace::SunTraceState,
    pub(super) restir_gi: &'a mut super::restir_gi::RestirGiState,
    pub(super) materials: &'a crate::key_hash::KeyMap<u64, super::MaterialEntry>,
    pub(super) ddgi_uniform: &'a wgpu::Buffer,
    pub(super) ddgi_data: &'a wgpu::Buffer,
    pub(super) gi_lookup: crate::ddgi::GiLookupResolution,
    pub(super) q: &'a crate::device::TrackedQueue,
    pub(super) belt: &'a mut crate::staging::StagingBelt,
    pub(super) screen_report: &'a mut ScreenPassReport,
}

/// What the screen occlusion passes read from the frame.
pub(super) struct OcclusionInputs<'a> {
    /// The frame's scene (near and far planes, sun direction, reflection occlusion switch).
    pub(super) scene: &'a FrameScene<'a>,
    /// The single-sample scene depth.
    pub(super) depth_view: &'a wgpu::TextureView,
    /// The frame's timestamp query set, when GPU timing is on.
    pub(super) timer: &'a Option<(super::FrameStamps, f32)>,
    /// The SSAO and contact-shadow target size.
    pub(super) ssao_size: (u32, u32),
    /// The SSAO kernel of the frame's GI quality.
    pub(super) ssao_mode: crate::profile::SsaoMode,
    /// The projection without TAAU jitter (the SSAO reconstructs view positions with it).
    pub(super) unjittered_proj: Mat4,
    /// The projection (jitter included) times the view: the one that drew the depth.
    pub(super) view_proj: Mat4,
    /// Unjittered view-projection for ReSTIR GI's camera-only motion.
    pub(super) unjittered_vp: Mat4,
    /// Texture-view render (ReSTIR GI stays on the main view).
    pub(super) view_pass: bool,
    /// The frame's viewport in the target.
    pub(super) viewport: [f32; 4],
    /// Internal (pre-upscale) render size.
    pub(super) internal_w: u32,
    /// Internal (pre-upscale) render size.
    pub(super) internal_h: u32,
    /// Camera-to-world rotation for the bent normals (ADR-1375), `None` when they are off.
    pub(super) bent_to_world: Option<[[f32; 4]; 3]>,
    /// Frame index since the last camera cut (temporal noise).
    pub(super) temporal_index: u64,
    /// TAAU is on this frame.
    pub(super) taau_on: bool,
    /// The scene renders to a linear HDR target.
    pub(super) linear_hdr: bool,
    /// The SSAO kernel runs this frame.
    pub(super) ssao_active: bool,
    /// The contact shadows run this frame.
    pub(super) contact_active: bool,
    /// The sun's variant this frame: anything but the shadow map traces here.
    pub(super) sun_variant: crate::hwrt::SunShadowVariant,
    /// The mesh cache (the acceleration structures build from its buffers).
    pub(super) meshes: &'a crate::key_hash::KeyMap<u64, super::MeshEntry>,
    /// The frame's draws (the sun trace's casters).
    pub(super) draws: &'a [super::Draw],
    /// The device's shared pipelines.
    pub(super) shared_pipelines: &'a std::sync::Arc<crate::device_pipelines::DevicePipelines>,
    /// The baked probe volume lights this frame.
    pub(super) baked_lit: bool,
    /// The screen passes reuse last frame's results (converged held camera).
    pub(super) reuse_hit: bool,
    /// The SSGI gather reuses its history, so the SSAO is skipped.
    pub(super) gather_reuse: bool,
}

/// Records the probe screen lookups, the SSAO kernel and the contact shadows, with their
/// timestamps (`DDGI_QUERY_BASE + 2`, `+ 3`, `CONTACT_QUERY` and stamp 5).
pub(super) fn record_screen_occlusion(
    fields: OcclusionFields<'_>,
    d: &TrackedDevice,
    encoder: &mut wgpu::CommandEncoder,
    inputs: &OcclusionInputs<'_>,
) -> Result<(), RenderError> {
    let OcclusionFields {
        ddgi,
        baked_gi,
        ssao_pass,
        blue_noise,
        contact_pass,
        sun_trace,
        restir_gi,
        materials,
        ddgi_uniform,
        ddgi_data,
        gi_lookup,
        q,
        belt,
        screen_report,
    } = fields;
    let OcclusionInputs {
        scene,
        depth_view,
        timer,
        ssao_size,
        ssao_mode,
        unjittered_proj,
        view_proj,
        unjittered_vp,
        view_pass,
        viewport,
        internal_w,
        internal_h,
        bent_to_world,
        temporal_index,
        taau_on,
        linear_hdr,
        ssao_active,
        contact_active,
        sun_variant,
        meshes,
        draws,
        shared_pipelines,
        baked_lit,
        reuse_hit,
        gather_reuse,
    } = *inputs;
    // Probe irradiance at the visible surfaces, for the forward and terrain passes (both
    // stamps are written every frame so an idle frame reads 0 ms). Recorded before the SSAO:
    // the baked lookup leaves the probe-relative AO split there for its kernel (ADR-1104).
    // The SSAO stamp interval still spans both and subtracts this one (`ssao_ms`).
    if let Some((set_ts, _)) = timer {
        set_ts.write(encoder, DDGI_QUERY_BASE + 2);
    }
    if ddgi.active && !reuse_hit {
        ddgi.record_screen(d, encoder, depth_view);
    }
    if baked_lit && !reuse_hit {
        baked_gi.record_screen(d, encoder, depth_view);
    }
    if let Some((set_ts, _)) = timer {
        set_ts.write(encoder, DDGI_QUERY_BASE + 3);
    }
    if ssao_active && !reuse_hit && !gather_reuse {
        let _span = tracing::info_span!("ssao").entered();
        let _crumb = crate::breadcrumbs::frame_pass("ssao");
        screen_report.ssao = true;
        // Where the baked volume lights a texel, the kernel counts only what its probes
        // do not already hold (`None` without a lit volume or with the split off).
        let probe_split = if baked_lit { baked_gi.ao_split() } else { None };
        // A post volume's AO strength (ADR-1464); 1 without one.
        ssao_pass.set_blue_noise(d, blue_noise)?;
        ssao_pass.set_intensity_scale(scene.post.map_or(1.0, crate::PostSettings::ao_intensity_scale));
        // The horizon kernel on the GTAO rows (ADR-3202, `RenderProfile::horizon_ao`).
        ssao_pass.horizon = scene.quality.horizon_ao;
        ssao_pass.dispatch(
            d,
            belt,
            encoder,
            depth_view,
            ssao_size.0,
            ssao_size.1,
            ssao_mode,
            [
                unjittered_proj.x_axis.x,
                unjittered_proj.y_axis.y,
                scene.near,
                scene.far,
            ],
            temporal_noise_index(taau_on, temporal_index),
            scene.quality.reflection_occlusion,
            probe_split.as_ref(),
            bent_to_world,
        )?;
    }
    if let Some((set_ts, _)) = timer {
        set_ts.write(encoder, CONTACT_QUERY);
    }
    if contact_active && !reuse_hit {
        // The sun direction in clip space with the projection that drew the depth (TAAU
        // jitter included): its xy/w is the rays' vanishing point on screen.
        let to_sun = -regia_core::glam::Vec3::from_array(scene.directional.direction).normalize_or_zero();
        let clip_sun = view_proj * to_sun.extend(0.0);
        let contact_viewport = if linear_hdr {
            [0.0, 0.0, internal_w as f32, internal_h as f32]
        } else {
            viewport
        };
        contact_pass.dispatch(
            d,
            belt,
            encoder,
            depth_view,
            ssao_size.0,
            ssao_size.1,
            clip_sun.to_array(),
            contact_viewport,
            scene.near,
            scene.far,
        )?;
    }
    if sun_variant.writes_visibility() && !reuse_hit {
        let trace_viewport = if linear_hdr {
            [0.0, 0.0, internal_w as f32, internal_h as f32]
        } else {
            viewport
        };
        sun_trace.record(
            d,
            q,
            shared_pipelines,
            belt,
            encoder,
            &super::sun_trace::SunTraceInputs {
                scene,
                variant: sun_variant,
                depth_view,
                size: ssao_size,
                view_proj,
                viewport: trace_viewport,
                meshes,
                draws,
            },
        )?;
    }
    if let Some((set_ts, _)) = timer {
        set_ts.write(encoder, 5);
    }
    restir_gi.record(
        d,
        q,
        belt,
        encoder,
        sun_trace,
        &super::restir_gi::RestirGiRecord {
            scene,
            view_pass,
            linear_hdr,
            depth_view,
            size: (internal_w, internal_h),
            view_proj,
            unjittered_vp,
            unjittered_proj,
            viewport,
            meshes,
            draws,
            materials,
            ddgi_active: ddgi.active,
            ddgi_uniform,
            ddgi_data,
            ddgi_settings: &ddgi.settings,
            lookup: gi_lookup,
        },
    )?;
    Ok(())
}
