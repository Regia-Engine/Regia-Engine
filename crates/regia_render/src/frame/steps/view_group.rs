// SPDX-License-Identifier: MPL-2.0
//! The frame's screen-space inputs and its view bind group (ADR-2300): the SSAO, contact-shadow
//! and sun-trace targets, the AO sampler, the probe begin (baked or DDGI), the view-group cache
//! invalidations and the view bind group itself. Moved out of `FrameRenderer::render_into`
//! unchanged.

use crate::frame::*;

/// What the view group step reads.
pub(in crate::frame) struct ViewGroupInputs<'a> {
    /// The scene.
    pub(in crate::frame) scene: &'a FrameScene<'a>,
    /// The frame's target.
    pub(in crate::frame) target: &'a FrameTarget<'a>,
    /// The CPU shadow plan (the atlas size keys the view group).
    pub(in crate::frame) plan: &'a shadow_plan::ShadowPlan,
    /// The sun.
    pub(in crate::frame) directional: DirectionalLightUniform,
    /// The fill light.
    pub(in crate::frame) fill_light: Option<[[f32; 3]; 2]>,
    /// Sun cascade views.
    pub(in crate::frame) csm_views: &'a [wgpu::TextureView],
    /// Local-light atlas view.
    pub(in crate::frame) atlas_view: &'a wgpu::TextureView,
    /// Jittered view-projection.
    pub(in crate::frame) view_proj: regia_core::glam::Mat4,
    /// Internal width.
    pub(in crate::frame) internal_w: u32,
    /// Internal height.
    pub(in crate::frame) internal_h: u32,
    /// MSAA samples.
    pub(in crate::frame) samples: u32,
    /// MSAA samples as the frame's modes chose them.
    pub(in crate::frame) samples_early: u32,
    /// Sun cascade size.
    pub(in crate::frame) csm_size: u32,
    /// The quality the GI passes read.
    pub(in crate::frame) gi_quality: FrameQuality,
    /// The sun's variant this frame.
    pub(in crate::frame) sun_variant: crate::hwrt::SunShadowVariant,
    /// Baked GI lights this frame.
    pub(in crate::frame) baked_on: bool,
    /// The frame renders into a texture view.
    pub(in crate::frame) view_pass: bool,
    /// Linear HDR target.
    pub(in crate::frame) linear_hdr: bool,
    /// The many-light reservoirs this frame writes and their key bits (1 + parity), from
    /// `ManyLightState::begin`; `None` binds the stand-in (ADR-2652).
    pub(in crate::frame) many_light: Option<(wgpu::Buffer, u64)>,
}

/// The frame's screen-space inputs and view bind group.
pub(in crate::frame) struct ViewGroup {
    /// Size of the SSAO and visibility targets.
    pub(in crate::frame) ssao_size: (u32, u32),
    /// SSAO runs.
    pub(in crate::frame) ssao_active: bool,
    /// Layer 0 of the AO texture (white when SSAO is off).
    pub(in crate::frame) ao_bound: wgpu::TextureView,
    /// Contact shadows run.
    pub(in crate::frame) contact_active: bool,
    /// The visibility texture (white when nothing fills it).
    pub(in crate::frame) contact_bound: wgpu::TextureView,
    /// Something fills the visibility texture.
    pub(in crate::frame) visibility_active: bool,
    /// The frame's forward tier.
    pub(in crate::frame) forward_tier: crate::forward_lite::ForwardLite,
    /// The baked volume lights this frame.
    pub(in crate::frame) baked_lit: bool,
    /// The indirect-light uniform buffer (baked or DDGI).
    pub(in crate::frame) ddgi_uniform: wgpu::Buffer,
    /// The indirect-light data buffer (baked or DDGI).
    pub(in crate::frame) ddgi_data: wgpu::Buffer,
    /// The indirect-light buffers hold live data.
    pub(in crate::frame) ddgi_live: bool,
    /// The frame's view bind group.
    pub(in crate::frame) view_bg: wgpu::BindGroup,
}

impl FrameRenderer {
    /// Prepares the AO and visibility targets, begins the probes and builds (or reuses) the
    /// frame's view bind group.
    pub(in crate::frame) fn frame_view_group(
        &mut self,
        d: &crate::TrackedDevice,
        inputs: &ViewGroupInputs<'_>,
    ) -> Result<ViewGroup, RenderError> {
        let &ViewGroupInputs {
            scene,
            target,
            plan,
            directional,
            fill_light,
            csm_views,
            atlas_view,
            view_proj,
            internal_w,
            internal_h,
            samples,
            samples_early,
            csm_size,
            gi_quality,
            sun_variant,
            baked_on,
            view_pass,
            linear_hdr,
            many_light: _,
        } = inputs;
        // ---- View bind group (per ibl + shadow-target-size configuration) ----
        let ssao_size = if linear_hdr {
            (internal_w, internal_h)
        } else {
            (target.width.max(1), target.height.max(1))
        };
        let ssao_active =
            gi_quality.ssao != crate::profile::SsaoMode::Off && samples_early == 1 && scene.depth_prepass;
        let ao_bound = if ssao_active {
            self.ssao_pass
                .prepare(d, &mut self.transients, ssao_size.0, ssao_size.1, gi_quality.ssao)?
        } else {
            self.pipes.as_ref().expect("pipelines").white_view.clone()
        };
        // The forward pass binds both layers of the AO texture (layer 1: the bent normals); the
        // terrain reads layer 0 (`ao_bound`).
        let ao_layers = match self.ssao_pass.layers_view().filter(|_| ssao_active) {
            Some(layers) => layers,
            None => self.pipes.as_ref().expect("pipelines").white_array_view.clone(),
        };
        // Contact shadows march the same single-sample depth as SSAO, toward the sun; they
        // only darken the sun term, so they run only when the sun casts shadows.
        let contact_wanted = scene.quality.contact_shadows
            && samples_early == 1
            && scene.depth_prepass
            && scene.directional_shadows;
        // The sun trace writes the same texture in place of contact shadows (ADR-2035).
        let sun_traced = sun_variant.writes_visibility();
        let contact_active = contact_wanted && !sun_traced;
        let contact_bound = if sun_traced {
            self.sun_trace
                .prepare(d, &mut self.transients, ssao_size.0, ssao_size.1)
        } else if contact_active {
            self.contact_pass.prepare(d, ssao_size.0, ssao_size.1)?
        } else {
            self.pipes.as_ref().expect("pipelines").white_view.clone()
        };
        // Whatever fills the visibility texture this frame.
        let visibility_active = contact_active || sun_traced;
        let forward_tier = self.frame_forward_tier(scene.quality.forward_lite);
        let pipes = self.pipes.as_mut().expect("pipelines");
        if self.ao_sampler.is_none() {
            self.ao_sampler = Some(d.create_sampler(&wgpu::SamplerDescriptor {
                label: Some("ao nearest"),
                address_mode_u: wgpu::AddressMode::ClampToEdge,
                address_mode_v: wgpu::AddressMode::ClampToEdge,
                mag_filter: wgpu::FilterMode::Nearest,
                min_filter: wgpu::FilterMode::Nearest,
                ..Default::default()
            }));
        }
        let ao_sampler = self.ao_sampler.clone().expect("ao sampler");
        // The image light's ground plane as the forward pass reads it this frame: the probes' missed
        // rays land on it too (ADR-5716).
        let env_ground = {
            let (plane, light) = self.studio.ground_uniforms(self.ibl.as_ref(), &directional);
            (light[3] > 0.5).then_some((plane, [light[0], light[1], light[2]]))
        };
        // ADR-5944 item 9: under the cloud transmittance map the probes' ray hits keep the share
        // of the sun the layer leaves on the ground (1 on every other frame, and when no probe
        // update can run: the weather map's terms are measured only for frames that use them).
        let probe_sun_share = if !view_pass && self.ddgi.enabled.unwrap_or(gi_quality.gi_probes) {
            self.outdoor.probe_sun_share()
        } else {
            1.0
        };
        // The baked volume lights this frame (`baked_on` and its upload went through).
        let baked_lit = cluster_probes::begin_probes(
            cluster_probes::ProbeBeginFields {
                ddgi: &mut self.ddgi,
                baked_gi: &mut self.baked_gi,
                belt: &mut self.belt,
                shared_draw: &self.shared_draw,
                gi_lookup: self.gi_lookup,
                visbuffer: &self.visbuffer,
            },
            d,
            &cluster_probes::ProbeBeginInputs {
                scene,
                directional: &directional,
                sun_share: probe_sun_share,
                fill_light,
                env_ground,
                extra_directional: self.lights_views.extra_dir,
                view_proj,
                internal_w,
                internal_h,
                samples,
                gi_probes: gi_quality.gi_probes,
                baked_on,
                ssao_active,
                view_pass,
                linear_hdr,
                origin_offset: self.origin_offset,
            },
        );
        let baked_buffers = if baked_lit {
            self.baked_gi.shading_buffers()
        } else {
            None
        };
        let baked_shading = baked_buffers.is_some();
        let (ddgi_uniform, ddgi_data, ddgi_live) = match baked_buffers {
            Some((uniform, data)) => (uniform, data, true),
            None => self.ddgi.shading_buffers(d),
        };
        // New baked buffers (a volume or a larger screen lookup) drop the view groups that bind
        // the old ones.
        if self.baked_gi.generation != self.baked_view_gen {
            self.view_bgs.clear();
            self.baked_view_gen = self.baked_gi.generation;
        }
        // A reallocated translucency map drops the view groups that bind the old one.
        if self.translucent_maps.generation() != self.translucent_view_gen {
            self.view_bgs.clear();
            self.translucent_view_gen = self.translucent_maps.generation();
        }
        // A new cloud transmittance map drops the view groups that bind the stand-in or the old one.
        if self.outdoor.cloud_shadow_generation() != self.cloud_map_view_gen {
            self.view_bgs.clear();
            self.cloud_map_view_gen = self.outdoor.cloud_shadow_generation();
        }
        // A replaced IBL (studio sky, gradient, equirect, or the default after a clear) drops the
        // view groups that bind the old maps (ADR-1654).
        if self.ibl_generation != self.ibl_view_gen {
            self.view_bgs.clear();
            self.ibl_view_gen = self.ibl_generation;
        }
        // A larger cluster index buffer drops the view groups that bind the old one (ADR-3351).
        if self.cluster_indices_gen != self.cluster_indices_view_gen {
            self.view_bgs.clear();
            self.cluster_indices_view_gen = self.cluster_indices_gen;
        }
        // Resized many-light reservoirs drop the view groups that bind the old ones (ADR-2652).
        if self.many_light.generation() != self.many_light_view_gen {
            self.view_bgs.clear();
            self.many_light_view_gen = self.many_light.generation();
        }
        // The forward pass's blue-noise table (ADR-2652): this frame's, when it uses one.
        let blue_noise_view = self.blue_noise.active().map(|noise| noise.view.clone());
        let Some(tviews) = self.translucent_maps.receiver_views() else {
            return Err(RenderError::Shader("translucent shadow maps missing".into()));
        };
        let view_bg_key = (
            (if scene.ibl { 1u64 } else { 0 })
                | (if ddgi_live { 2u64 } else { 0 })
                | (if baked_shading { 4u64 } else { 0 })
                | (inputs.many_light.as_ref().map_or(0, |(_, bits)| *bits) << 3)
                | (u64::from(blue_noise_view.is_some()) << 5)
                | (u64::from(plan.atlas_size) << 8)
                | (u64::from(csm_size) << 20),
            if ssao_active || visibility_active {
                (u64::from(ssao_size.0) << 32)
                    | u64::from(ssao_size.1)
                    | (u64::from(ssao_active) << 63)
                    | (u64::from(contact_active) << 62)
                    | (u64::from(sun_traced) << 61)
            } else {
                0
            },
        );
        let view_bg = view_bindings::frame_view_bind_group(
            view_bindings::ViewBindFields {
                view_bgs: &mut self.view_bgs,
                ibl: &self.ibl,
                brdf_lut: &self.brdf_lut,
                frame_buf: &self.frame_buf,
                dir_buf: &self.dir_buf,
                lights_buf: &self.lights_buf,
                counts_buf: &self.counts_buf,
                indices_buf: &self.indices_buf,
                csm_buf: &self.csm_buf,
                tiles_buf: &self.tiles_buf,
                gi_buf: &self.gi_buf,
                gi_grid_buf: &self.gi_grid_buf,
                ground_bounce_buf: &self.ground_bounce_buf,
                mesh_occluder_buf: &self.mesh_occluder_buf,
                ltc_view: &self.ltc_view,
                probe_atlas: &self.probe_atlas,
                studio: &self.studio,
                outdoor: &self.outdoor,
            },
            d,
            &view_bindings::ViewBindInputs {
                pipes: &*pipes,
                key: view_bg_key,
                ibl_on: scene.ibl,
                csm_views,
                atlas_view,
                ao_layers: &ao_layers,
                ao_sampler: &ao_sampler,
                contact_bound: &contact_bound,
                ddgi_uniform: &ddgi_uniform,
                ddgi_data: &ddgi_data,
                tviews: &tviews,
                reservoirs: inputs
                    .many_light
                    .as_ref()
                    .map_or(&pipes.many_light_stand_in, |(buffer, _)| buffer),
                blue_noise: blue_noise_view.as_ref(),
            },
        );

        Ok(ViewGroup {
            ssao_size,
            ssao_active,
            ao_bound,
            contact_active,
            contact_bound,
            visibility_active,
            forward_tier,
            baked_lit,
            ddgi_uniform,
            ddgi_data,
            ddgi_live,
            view_bg,
        })
    }
}
