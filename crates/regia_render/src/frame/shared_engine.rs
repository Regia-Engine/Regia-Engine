// SPDX-License-Identifier: MPL-2.0
//! The engine inputs of the shared draw (native terrain) for this frame, set before the
//! passes record (ADR-1552 split): the sun cascades, SSAO and contact-shadow targets, the cloud
//! transmittance map, the sky irradiance, the probe or baked GI buffers and the forward pass's
//! cluster lists and local-light atlas, then the draw's own per-frame uploads. Moved out of
//! `frame.rs` unchanged. The frame still borrows its pipeline sets here, so this takes the
//! renderer fields it touches ([`SharedEngineFields`]) instead of `self`.

use super::{FrameScene, IblGpu};
use crate::TrackedDevice;

/// The renderer fields the shared draw's engine inputs read and write.
pub(super) struct SharedEngineFields<'a> {
    pub(super) shared_draw: &'a mut Option<Box<dyn crate::SharedPassDraw + Send + Sync>>,
    pub(super) outdoor: &'a crate::outdoor::OutdoorState,
    pub(super) ibl: &'a Option<IblGpu>,
    pub(super) frame_buf: &'a Option<wgpu::Buffer>,
    pub(super) lights_buf: &'a Option<wgpu::Buffer>,
    pub(super) counts_buf: &'a Option<wgpu::Buffer>,
    pub(super) indices_buf: &'a Option<wgpu::Buffer>,
    pub(super) tiles_buf: &'a Option<wgpu::Buffer>,
    pub(super) belt: &'a mut crate::staging::StagingBelt,
    pub(super) shared_upload_last: &'a mut (u32, u64),
}

/// What the shared draw's engine inputs read from the frame.
pub(super) struct SharedEngineInputs<'a> {
    /// The frame's scene (image-based lighting switch).
    pub(super) scene: &'a FrameScene<'a>,
    /// The shared draw's per-frame info from its prepare step, when it has one.
    pub(super) shared_info: &'a Option<crate::SharedFrameInfo>,
    /// The sun cascade views.
    pub(super) csm_views: &'a [wgpu::TextureView],
    /// The SSAO target (or the white stand-in).
    pub(super) ao_bound: &'a wgpu::TextureView,
    /// The contact-shadow target (or the white stand-in).
    pub(super) contact_bound: &'a wgpu::TextureView,
    /// The local-light shadow atlas.
    pub(super) atlas_view: &'a wgpu::TextureView,
    /// The probe (or baked volume) uniform buffer.
    pub(super) ddgi_uniform: &'a wgpu::Buffer,
    /// The probe (or baked volume) data buffer.
    pub(super) ddgi_data: &'a wgpu::Buffer,
    /// MSAA sample count (the shared draw runs on single-sample frames only).
    pub(super) samples: u32,
    /// The SSAO kernel runs this frame.
    pub(super) ssao_active: bool,
    /// The contact shadows run this frame.
    pub(super) contact_active: bool,
    /// The meshes take the atmosphere bake as image-based light this frame.
    pub(super) planet_ibl: bool,
    /// The probe or baked GI buffers light this frame.
    pub(super) ddgi_live: bool,
}

/// Hands the shared draw this frame's engine views and buffers and runs its per-frame uploads
/// (counted in `shared_upload_last`). Does nothing without a shared draw or on an MSAA frame.
pub(super) fn set_shared_engine_inputs(
    fields: SharedEngineFields<'_>,
    d: &TrackedDevice,
    inputs: &SharedEngineInputs<'_>,
) {
    let SharedEngineFields {
        shared_draw,
        outdoor,
        ibl,
        frame_buf,
        lights_buf,
        counts_buf,
        indices_buf,
        tiles_buf,
        belt,
        shared_upload_last,
    } = fields;
    let SharedEngineInputs {
        scene,
        shared_info,
        csm_views,
        ao_bound,
        contact_bound,
        atlas_view,
        ddgi_uniform,
        ddgi_data,
        samples,
        ssao_active,
        contact_active,
        planet_ibl,
        ddgi_live,
    } = *inputs;
    // The shared draw's cascades: engine views for its colour pass.
    if samples == 1
        && let Some(draw) = shared_draw.as_mut()
    {
        draw.set_engine_csm(csm_views);
        draw.set_engine_ao(ssao_active.then_some(ao_bound));
        draw.set_engine_contact_shadows(contact_active.then_some(contact_bound));
        // The cloud transmittance map for the shared draw's sun term (ADR-320).
        let cloud_params = outdoor.cloud_sun_params();
        draw.set_engine_cloud_shadow(
            outdoor
                .cloud_shadow_view()
                .filter(|_| cloud_params[3] > 0.5)
                .map(|view| (view, cloud_params)),
        );
        // The same condition as `SharedFrameInfo::sky_ambient`: meshes take the
        // atmosphere bake as IBL this frame.
        let sky_irradiance = if scene.ibl && planet_ibl && regia_core::brand::env_var_os("T_NO_DIR").is_none()
        {
            ibl.as_ref().map(|ibl| (&ibl.irradiance, &ibl.brdf))
        } else {
            None
        };
        draw.set_engine_sky_irradiance(sky_irradiance);
        draw.set_engine_gi(ddgi_live.then_some((ddgi_uniform, ddgi_data)));
        // The forward pass's cluster lists, lights and local-light shadow atlas, for the
        // shared draw's point and spot lights (`shared_local_lights.wgsl`).
        let engine_lights = match (frame_buf, lights_buf, counts_buf, indices_buf, tiles_buf) {
            (Some(frame), Some(lights), Some(counts), Some(indices), Some(tiles)) => {
                Some(crate::EngineLights {
                    frame: frame.clone(),
                    lights: lights.clone(),
                    counts: counts.clone(),
                    indices: indices.clone(),
                    tiles: tiles.clone(),
                    atlas: atlas_view.clone(),
                })
            }
            _ => None,
        };
        draw.set_engine_lights(engine_lights.as_ref());
        if let Some(info) = shared_info.as_ref() {
            let mut upload = crate::SharedUpload::new(belt, d);
            draw.finish_prepare(info, &mut upload);
            shared_upload_last.0 += upload.writes();
            shared_upload_last.1 += upload.bytes();
        }
    }
}
