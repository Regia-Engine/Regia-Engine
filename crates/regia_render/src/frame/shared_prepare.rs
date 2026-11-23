// SPDX-License-Identifier: MPL-2.0
//! The mesh occluders and the shared draw's prepare step of `FrameRenderer::render_into`
//! (ADR-1552 split, line m): the large-mesh proxies' block, then the shared draw's
//! `SharedFrameInfo`, uploads and ground bounce. Moved out of `frame.rs` unchanged.

use super::*;

/// What the shared draw's prepare step reads from the frame.
pub(super) struct SharedPrepareInputs<'a> {
    /// The frame's scene (eye, clip planes, quality, exposure).
    pub(super) scene: &'a FrameScene<'a>,
    /// The CPU shadow plan (sun cascades).
    pub(super) plan: &'a shadow_plan::ShadowPlan,
    /// The frame's sun.
    pub(super) directional: &'a DirectionalLightUniform,
    /// The sky body's fill light, when there is one.
    pub(super) fill_light: Option<[[f32; 3]; 2]>,
    /// The fog the forward pass applies.
    pub(super) forward_fog: crate::FogSettings,
    /// The planet IBL lights the frame.
    pub(super) planet_ibl: bool,
    /// The jittered view-projection.
    pub(super) view_proj: regia_core::glam::Mat4,
    /// The unjittered view-projection.
    pub(super) unjittered_vp: regia_core::glam::Mat4,
    /// The previous frame's unjittered view-projection.
    pub(super) prev_vp: [[f32; 4]; 4],
    /// Vertical field of view (radians).
    pub(super) fov_y: f32,
    /// Internal (pre-upscale) render width.
    pub(super) internal_w: u32,
    /// Internal (pre-upscale) render height.
    pub(super) internal_h: u32,
    /// The color format the passes render into.
    pub(super) color_format: wgpu::TextureFormat,
    /// The frame renders linear HDR.
    pub(super) linear_hdr: bool,
    /// The frame's pre-exposure scale.
    pub(super) pre_exposure: f32,
    /// Temporal AA/upscale is on.
    pub(super) taau_on: bool,
    /// Texture LOD bias of the meshes.
    pub(super) texture_lod_bias: f32,
    /// Texture LOD bias of the terrain.
    pub(super) terrain_texture_lod_bias: f32,
    /// Stochastic screen-space reflections read the material codes.
    pub(super) sssr_on: bool,
    /// MSAA sample count.
    pub(super) samples: u32,
    /// Sun cascade map edge in texels.
    pub(super) csm_size: u32,
    /// Sun cascades drawn this frame, as an index bound.
    pub(super) cascade_count: usize,
    /// Per cascade: the UV union of the mesh pages redrawn.
    pub(super) shadow_regions: [[f32; 4]; 4],
    /// Sun cascades the dynamic-caster class refreshes this frame (ADR-2028; 0 unpaged).
    pub(super) dynamic_shadow_cascades: u8,
}

impl FrameRenderer {
    /// Builds the mesh occluders' block and runs the shared draw's prepare step (single-sample
    /// frames); returns the shared draw's frame info.
    pub(super) fn prepare_shared_frame(
        &mut self,
        d: &crate::TrackedDevice,
        inputs: SharedPrepareInputs<'_>,
        shadow_cpu: &mut ShadowCpuTimings,
    ) -> Result<Option<crate::SharedFrameInfo>, RenderError> {
        let SharedPrepareInputs {
            scene,
            plan,
            directional,
            fill_light,
            forward_fog,
            planet_ibl,
            view_proj,
            unjittered_vp,
            prev_vp,
            fov_y,
            internal_w,
            internal_h,
            color_format,
            linear_hdr,
            pre_exposure,
            taau_on,
            texture_lod_bias,
            terrain_texture_lod_bias,
            sssr_on,
            samples,
            csm_size,
            cascade_count,
            shadow_regions,
            dynamic_shadow_cascades,
        } = inputs;
        // Large-mesh proxies (`set_mesh_occluders`), shaded with the planet IBL's hemisphere
        // light: none without it, so frames lit otherwise are unchanged.
        let sky_ambient = if scene.ibl && planet_ibl {
            self.outdoor.hemisphere_ambient()
        } else {
            None
        };
        {
            let _span = tracing::info_span!("mesh_occluders").entered();
            *self.mesh_occluder_block = match sky_ambient {
                Some(sky_ambient) if !self.mesh_occluders.is_empty() => {
                    let travel = directional.direction;
                    crate::MeshOccluderBlock::build(
                        &self.mesh_occluders,
                        &crate::OccluderLighting {
                            toward_sun: [-travel[0], -travel[1], -travel[2]],
                            sun: directional.color.map(|c| c * directional.illuminance),
                            sky_ambient,
                            fill: fill_light,
                            eye: scene.eye,
                        },
                    )
                }
                _ => crate::MeshOccluderBlock::default(),
            };
            if let Some(buf) = self.mesh_occluder_buf.as_ref() {
                crate::staging::defer_if_changed(
                    &mut self.belt,
                    d,
                    &mut self.mesh_occluder_bytes,
                    buf,
                    bytemuck::bytes_of(&*self.mesh_occluder_block),
                );
            }
        }

        // Shared draws (terrain) upload before any pass uses the buffers.
        self.shared_upload_last = (0, 0);
        let mut shared_info: Option<crate::SharedFrameInfo> = None;
        crate::pipeline_warm::warm_checkpoint()?;
        if samples == 1
            && let Some(draw) = self.shared_draw.as_mut()
        {
            let _span = tracing::info_span!("shared_prepare").entered();
            let _time = AddElapsedMs(&mut shadow_cpu.shared_prepare_ms, std::time::Instant::now());
            let mut cascades = [[[0.0f32; 4]; 4]; 4];
            for (i, cascade) in plan.csm.cascades.iter().take(cascade_count).enumerate() {
                cascades[i] = cascade.light_view_proj;
            }
            let mut csm_splits = [0.0f32; 4];
            let mut csm_bias = [[0.0f32; 4]; 4];
            for (i, cascade) in plan.csm.cascades.iter().take(4).enumerate() {
                csm_splits[i] = cascade.split_distance;
                csm_bias[i] = [cascade.bias[0], cascade.bias[1], csm_size as f32, 0.0];
            }
            let (_search, filter) = scene.quality.pcss_taps();
            let info = crate::SharedFrameInfo {
                view_proj: view_proj.to_cols_array_2d(),
                unjittered_view_proj: unjittered_vp.to_cols_array_2d(),
                prev_view_proj: prev_vp,
                eye: scene.eye,
                fov_y,
                near: scene.near,
                far: scene.far,
                width: internal_w,
                height: internal_h,
                color_format,
                linear_hdr,
                exposure_ev: scene.exposure_ev,
                hdr_scale: pre_exposure,
                // The sun the meshes, the sky and the cascades use: a level clock
                // replaces the authored light with the atmosphere's transmitted sun.
                sun_dir: directional.direction,
                sun_color: directional.color,
                sun_illuminance: directional.illuminance,
                ambient_lux: scene.ambient_lux,
                // Meshes take the atmosphere bake as IBL exactly when this holds.
                sky_ambient,
                fill_light,
                extra_directional: self.lights_views.extra_dir,
                mesh_occluders: *self.mesh_occluder_block,
                fog: forward_fog,
                cascades,
                cascade_count: cascade_count as u32,
                csm_splits,
                csm_bias,
                pcss_filter_taps: filter,
                shadow_regions,
                terrain_budget: scene.quality.terrain_budget,
                surface_weather: self.weather.surface,
                precip_occlusion: self.weather.view.filter(|_| self.view_pass.is_none()),
                precip_extinction_per_m: self.weather.extinction_per_m,
                noise_index: temporal_noise_index(taau_on, self.frame_index.wrapping_sub(self.cut.origin)),
                texture_lod_bias,
                terrain_texture_lod_bias,
                sssr_codes: sssr_on,
                origin_offset: self.origin_offset,
                dynamic_shadow_cascades,
                foliage_wind: self.foliage_wind.filter(|_| !self.foliage_wind_from_scene),
            };
            let mut upload = crate::SharedUpload::new(&mut self.belt, d);
            draw.prepare(&info, &mut upload);
            self.shared_upload_last = (upload.writes(), upload.bytes());
            // The light the shared surface bounces onto the ground, for meshes on it.
            // One header vec4 per level (origin XZ, cell, size; zeros when absent), one
            // horizon header (the atmosphere bake's ground radiance, then 1 when the levels
            // carry horizons and the meshes take that bake as IBL), the levels' radiance cells
            // in order, then their horizon cells in the same order (`lab_forward.wgsl`).
            let levels = crate::shared_draw::GROUND_BOUNCE_LEVELS;
            let mut packed: Vec<[f32; 4]> = vec![[0.0; 4]; levels + 1];
            if let Some(gb) = draw.ground_bounce() {
                let valid: Vec<&crate::GroundBounceLevel> =
                    gb.levels.iter().filter(|l| l.is_valid()).take(levels).collect();
                for (slot, level) in valid.iter().enumerate() {
                    // The levels sit in the draw's world-anchored space; meshes read them locally.
                    packed[slot] = [
                        level.origin[0] - info.origin_offset[0],
                        level.origin[1] - info.origin_offset[2],
                        level.cell_m,
                        level.size as f32,
                    ];
                    packed.extend_from_slice(&level.radiance);
                }
                let horizons = !valid.is_empty() && valid.iter().all(|l| !l.horizon.is_empty());
                if let (true, Some([_, ground])) = (horizons, info.sky_ambient) {
                    packed[levels] = [ground[0], ground[1], ground[2], 1.0];
                    for level in &valid {
                        packed.extend(level.horizon.iter().copied());
                    }
                }
            }
            if let Some(buf) = self.ground_bounce_buf.as_ref() {
                crate::staging::defer_if_changed(
                    &mut self.belt,
                    d,
                    &mut self.ground_bounce_bytes,
                    buf,
                    bytemuck::cast_slice(&packed),
                );
            }
            shared_info = Some(info);
        }
        Ok(shared_info)
    }
}
