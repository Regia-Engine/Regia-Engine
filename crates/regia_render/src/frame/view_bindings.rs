// SPDX-License-Identifier: MPL-2.0
//! The frame's view bind group (group 0 of the forward, terrain and resolve pipelines) of
//! `FrameRenderer::render_into` (ADR-1552 split): frame and light buffers, the light clusters,
//! sun cascades and local-light atlas, image-based light, AO and contact shadows, GI probes or
//! the baked volume, translucency maps, the LTC table, the emissive probe atlas and the cloud
//! transmittance map, cached per configuration key. Moved out of `frame.rs` unchanged; a lane that
//! adds a frame-wide binding adds it here and in the view layout (`frame/layouts.rs`).

use super::{IblGpu, Pipes, binding, texture_binding, translucent_shadow};
use crate::TrackedDevice;

/// The renderer fields the view bind group reads (and its cache).
pub(super) struct ViewBindFields<'a> {
    pub(super) view_bgs: &'a mut crate::bind_cache::BindCache<(u64, u64)>,
    pub(super) ibl: &'a Option<IblGpu>,
    pub(super) brdf_lut: &'a Option<wgpu::TextureView>,
    pub(super) frame_buf: &'a Option<wgpu::Buffer>,
    pub(super) dir_buf: &'a Option<wgpu::Buffer>,
    pub(super) lights_buf: &'a Option<wgpu::Buffer>,
    pub(super) counts_buf: &'a Option<wgpu::Buffer>,
    pub(super) indices_buf: &'a Option<wgpu::Buffer>,
    pub(super) csm_buf: &'a Option<wgpu::Buffer>,
    pub(super) tiles_buf: &'a Option<wgpu::Buffer>,
    pub(super) gi_buf: &'a Option<wgpu::Buffer>,
    pub(super) gi_grid_buf: &'a Option<wgpu::Buffer>,
    pub(super) ground_bounce_buf: &'a Option<wgpu::Buffer>,
    pub(super) mesh_occluder_buf: &'a Option<wgpu::Buffer>,
    pub(super) ltc_view: &'a Option<wgpu::TextureView>,
    pub(super) probe_atlas: &'a Option<(wgpu::Texture, wgpu::TextureView)>,
    pub(super) studio: &'a super::studio_state::StudioState,
    pub(super) outdoor: &'a crate::outdoor::OutdoorState,
}

/// What the view bind group binds from the frame.
pub(super) struct ViewBindInputs<'a> {
    /// The frame's pipeline sets (view layout, white fallback view).
    pub(super) pipes: &'a Pipes,
    /// The configuration key the group is cached under (IBL, GI, baked shading, atlas and
    /// cascade sizes, AO and contact-shadow targets).
    pub(super) key: (u64, u64),
    /// The scene has image-based light.
    pub(super) ibl_on: bool,
    /// The sun cascade views.
    pub(super) csm_views: &'a [wgpu::TextureView],
    /// The local-light shadow atlas.
    pub(super) atlas_view: &'a wgpu::TextureView,
    /// Both layers of the AO target (layer 1: bent normals), or the white stand-in.
    pub(super) ao_layers: &'a wgpu::TextureView,
    /// The AO sampler.
    pub(super) ao_sampler: &'a wgpu::Sampler,
    /// The contact-shadow target, or the white stand-in.
    pub(super) contact_bound: &'a wgpu::TextureView,
    /// The probe (or baked volume) uniform buffer.
    pub(super) ddgi_uniform: &'a wgpu::Buffer,
    /// The probe (or baked volume) data buffer.
    pub(super) ddgi_data: &'a wgpu::Buffer,
    /// The translucency maps' receiver views.
    pub(super) tviews: &'a translucent_shadow::ReceiverViews<'a>,
    /// The many-light reservoirs this frame writes, or the stand-in (binding 37, ADR-2652; only
    /// on a layout that has it).
    pub(super) reservoirs: &'a wgpu::Buffer,
    /// The blue-noise table when the frame uses it (binding 38, ADR-2652; only on a layout that
    /// has it); the white array stand-in otherwise.
    pub(super) blue_noise: Option<&'a wgpu::TextureView>,
}

/// The view bind group for `inputs.key`: the cached one, or a new one (cached).
pub(super) fn frame_view_bind_group(
    fields: ViewBindFields<'_>,
    d: &TrackedDevice,
    inputs: &ViewBindInputs<'_>,
) -> wgpu::BindGroup {
    let ViewBindFields {
        view_bgs,
        ibl,
        brdf_lut,
        frame_buf,
        dir_buf,
        lights_buf,
        counts_buf,
        indices_buf,
        csm_buf,
        tiles_buf,
        gi_buf,
        gi_grid_buf,
        ground_bounce_buf,
        mesh_occluder_buf,
        ltc_view,
        probe_atlas,
        studio,
        outdoor,
    } = fields;
    let ViewBindInputs {
        pipes,
        key,
        ibl_on,
        csm_views,
        atlas_view,
        ao_layers,
        ao_sampler,
        contact_bound,
        ddgi_uniform,
        ddgi_data,
        tviews,
        reservoirs,
        blue_noise,
    } = *inputs;
    view_bgs.get(&key).unwrap_or_else(|| {
        let (irr, pref, brdf) = if ibl_on {
            let ibl = ibl.as_ref().expect("ibl maps");
            (ibl.irradiance.clone(), ibl.prefilter.clone(), ibl.brdf.clone())
        } else {
            // No environment, but the DFG table is the BRDF's own (see `brdf_lut`).
            (
                pipes.white_view.clone(),
                pipes.white_view.clone(),
                brdf_lut.clone().unwrap_or_else(|| pipes.white_view.clone()),
            )
        };
        let sampler = if ibl_on {
            ibl.as_ref().expect("ibl maps").sampler.clone()
        } else {
            d.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("ibl"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Linear,
                min_filter: wgpu::FilterMode::Linear,
                mipmap_filter: wgpu::MipmapFilterMode::Linear,
                ..Default::default()
            })
        };
        let shadow_samp = d.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("shadow compare"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            compare: Some(wgpu::CompareFunction::LessEqual),
            ..Default::default()
        });
        let mut entries = vec![
            binding(0, frame_buf.as_ref().expect("frame buffer").as_entire_binding()),
            binding(1, dir_buf.as_ref().expect("dir buffer").as_entire_binding()),
            binding(2, lights_buf.as_ref().expect("lights buffer").as_entire_binding()),
            binding(3, counts_buf.as_ref().expect("counts").as_entire_binding()),
            binding(4, indices_buf.as_ref().expect("indices").as_entire_binding()),
            texture_binding(5, &csm_views[0]),
            texture_binding(6, &csm_views[1]),
            wgpu::BindGroupEntry {
                binding: 7,
                resource: wgpu::BindingResource::Sampler(&shadow_samp),
            },
            binding(8, csm_buf.as_ref().expect("csm buffer").as_entire_binding()),
            texture_binding(9, atlas_view),
            binding(10, tiles_buf.as_ref().expect("tiles buffer").as_entire_binding()),
            texture_binding(11, &irr),
            texture_binding(12, &pref),
            texture_binding(13, &brdf),
            wgpu::BindGroupEntry {
                binding: 14,
                resource: wgpu::BindingResource::Sampler(&sampler),
            },
            texture_binding(15, &csm_views[2.min(csm_views.len() - 1)]),
            texture_binding(16, &csm_views[3.min(csm_views.len() - 1)]),
            texture_binding(17, ao_layers),
            wgpu::BindGroupEntry {
                binding: 18,
                resource: wgpu::BindingResource::Sampler(ao_sampler),
            },
            binding(19, gi_buf.as_ref().expect("gi probes").as_entire_binding()),
            binding(20, gi_grid_buf.as_ref().expect("gi grid").as_entire_binding()),
            binding(
                21,
                ground_bounce_buf
                    .as_ref()
                    .expect("ground bounce")
                    .as_entire_binding(),
            ),
            texture_binding(22, contact_bound),
            binding(
                23,
                mesh_occluder_buf
                    .as_ref()
                    .expect("mesh occluders")
                    .as_entire_binding(),
            ),
            binding(24, ddgi_uniform.as_entire_binding()),
            binding(25, ddgi_data.as_entire_binding()),
            texture_binding(26, tviews.csm_color),
            texture_binding(27, tviews.csm_depth),
            texture_binding(28, tviews.atlas_color),
            texture_binding(29, tviews.atlas_depth),
            wgpu::BindGroupEntry {
                binding: 30,
                resource: wgpu::BindingResource::Sampler(tviews.sampler),
            },
            binding(31, tviews.bounds.as_entire_binding()),
            texture_binding(32, ltc_view.as_ref().expect("ltc table")),
            texture_binding(33, &probe_atlas.as_ref().expect("probe atlas").1),
            texture_binding(34, outdoor.cloud_shadow_view().unwrap_or(&pipes.white_view)),
            texture_binding(35, studio.capture_view()),
            texture_binding(36, studio.background_view()),
        ];
        if pipes.many_light_binding {
            entries.push(binding(37, reservoirs.as_entire_binding()));
        }
        if pipes.blue_noise_binding {
            entries.push(texture_binding(38, blue_noise.unwrap_or(&pipes.white_array_view)));
        }
        if let Some(forward_decals) = pipes.forward_decals.as_ref() {
            entries.push(forward_decals.bind_entry());
        }
        let bg = d.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("frame view"),
            layout: &pipes.view_bgl,
            entries: &entries,
        });
        view_bgs.insert(key, bg.clone());
        bg
    })
}
