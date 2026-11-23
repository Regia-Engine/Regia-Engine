// SPDX-License-Identifier: MPL-2.0
//! The frame's GPU state, frame uniform and per-frame buffer writes in
//! `FrameRenderer::render_into` (ADR-1552 split, line m): the pipelines, IBL, LTC table, probe
//! atlas and cloud transmittance plan created or refreshed once, then the `LabFrame` uniform,
//! the GI grid and probes, the sun, the light records and the shadow tiles written through the
//! staging belt. Moved out of `frame.rs` unchanged.

use super::*;

/// What the frame's GPU state reads from the frame.
pub(super) struct GpuStateInputs<'a> {
    /// The frame's scene (IBL switch, eye, quality).
    pub(super) scene: &'a FrameScene<'a>,
    /// The frame renders into a texture view, not the main view.
    pub(super) view_pass: bool,
    /// The planet IBL lights the frame.
    pub(super) planet_ibl: bool,
    /// The frame renders linear HDR.
    pub(super) linear_hdr: bool,
    /// The frame's sun.
    pub(super) directional: DirectionalLightUniform,
    /// The emissive lights' probe texels for the probe atlas.
    pub(super) emissive_probes: Vec<f32>,
}

/// What the frame uniform and the per-frame buffers are built from.
pub(super) struct FrameUniformInputs<'a> {
    /// The frame's scene (eye, clip planes, quality, exposure).
    pub(super) scene: &'a FrameScene<'a>,
    /// The CPU shadow plan (atlas size, tiles, light tiles).
    pub(super) plan: &'a shadow_plan::ShadowPlan,
    /// The local lights uploaded this frame.
    pub(super) uploaded: &'a [crate::LabLocalLight],
    /// The emissive meshes' fitted light records.
    pub(super) emissive_records: Vec<[[f32; 4]; 4]>,
    /// The frame's sun.
    pub(super) directional: DirectionalLightUniform,
    /// The sky body's fill light, when there is one.
    pub(super) fill_light: Option<[[f32; 3]; 2]>,
    /// The fog the forward pass applies.
    pub(super) forward_fog: crate::FogSettings,
    /// The view matrix.
    pub(super) view: regia_core::glam::Mat4,
    /// The jittered view-projection.
    pub(super) view_proj: regia_core::glam::Mat4,
    /// The unjittered view-projection.
    pub(super) unjittered_vp: regia_core::glam::Mat4,
    /// The previous frame's unjittered view-projection.
    pub(super) prev_vp: [[f32; 4]; 4],
    /// Vertical field of view (radians).
    pub(super) fov_y: f32,
    /// Output aspect ratio.
    pub(super) aspect: f32,
    /// Output width.
    pub(super) width: u32,
    /// Output height.
    pub(super) height: u32,
    /// Internal (pre-upscale) render width.
    pub(super) internal_w: u32,
    /// Internal (pre-upscale) render height.
    pub(super) internal_h: u32,
    /// The frame renders linear HDR.
    pub(super) linear_hdr: bool,
    /// The frame's pre-exposure scale.
    pub(super) pre_exposure: f32,
    /// Temporal AA/upscale is on.
    pub(super) taau_on: bool,
    /// The temporal upscaler is TSR this frame.
    pub(super) tsr_frame: bool,
    /// Frames since the last camera cut (jitter and noise phase).
    pub(super) temporal_index: u64,
    /// Texture LOD bias of the meshes.
    pub(super) texture_lod_bias: f32,
    /// Stochastic screen-space reflections read the material codes.
    pub(super) sssr_on: bool,
    /// The AO texture's layer 1 holds bent normals.
    pub(super) bent_on: bool,
    /// A planet frame without the LUT sky (the bake's sky map is the background).
    pub(super) planet_ldr: bool,
}

impl FrameRenderer {
    /// The frame's blue-noise table (a step of `render_into`): uploaded on first use when `on`, and
    /// handed to the high-end gather (`None` when off).
    pub(super) fn begin_blue_noise(&mut self, d: &crate::TrackedDevice, queue: &wgpu::Queue, on: bool) {
        self.blue_noise.begin_frame(d, queue, on);
        self.highend.set_blue_noise(self.blue_noise.active().cloned());
    }

    /// Creates the GPU state the frame needs once (pipelines, IBL or BRDF LUT, frame buffers,
    /// LTC table, probe atlas), refreshes the atmosphere IBL and plans the cloud transmittance
    /// map.
    pub(super) fn ensure_gpu_state(
        &mut self,
        device: &RenderDevice,
        d: &crate::TrackedDevice,
        q: &crate::device::TrackedQueue,
        inputs: GpuStateInputs<'_>,
    ) -> Result<(), RenderError> {
        let GpuStateInputs {
            scene,
            view_pass,
            planet_ibl,
            linear_hdr,
            directional,
            emissive_probes,
        } = inputs;
        // ---- GPU state (once) — before the frame uniform so the IBL LOD is known ----
        if self.pipes.is_none() {
            self.pipes = Some(build_pipes(
                d,
                q,
                device.shared_pipelines(),
                self.material_anisotropy(),
            )?);
        }
        if scene.ibl && self.ibl.is_none() {
            let maps = crate::ibl::build_procedural_ibl();
            self.ibl = Some(build_ibl(d, q, &maps));
            self.ibl_generation += 1;
        }
        if !scene.ibl && self.brdf_lut.is_none() {
            let lut = crate::ibl::build_brdf_lut();
            self.brdf_lut = Some(upload_rgba(d, q, lut.width, lut.height, &lut.rgba).1);
        }
        // A texture view lights with the IBL the main view baked.
        if scene.ibl && !view_pass {
            let textures = self
                .ibl
                .as_ref()
                .map(|ibl| (ibl.irradiance_tex.clone(), ibl.prefilter_tex.clone()));
            if let Some((irradiance, prefilter)) = textures {
                if planet_ibl {
                    self.outdoor.write_ibl(q, &irradiance, &prefilter);
                    self.ibl_from_atmosphere = true;
                } else if self.ibl_from_atmosphere {
                    crate::outdoor::write_procedural_ibl(q, &irradiance, &prefilter);
                    self.outdoor.forget_ibl();
                    self.ibl_from_atmosphere = false;
                }
            }
        }
        self.ensure_frame_buffers(d, scene.quality.max_lights_per_cluster)?;
        // The cloud transmittance map (ADR-320) is planned before the frame uniform and the view
        // group read it, and marched at the start of the frame encoder, before the forward pass.
        // Only frames that run the cloud march (single-sample HDR, a preset with clouds) build it;
        // a texture view keeps clouds off (ADR-184).
        self.outdoor.plan_cloud_shadow_map(
            d,
            q,
            device.shared_pipelines(),
            crate::outdoor::CloudMapFrame {
                sun_travel: directional.direction,
                eye: scene.eye,
                cone_light: scene.quality.cloud_cone_light,
                allowed: !view_pass
                    && scene.quality.msaa_samples < 4
                    && linear_hdr
                    && scene.quality.volumetric_clouds,
            },
        )?;
        if self.ltc_view.is_none() {
            self.ltc_view = Some(upload_ltc_table(d, q));
        }
        self.studio.ensure_views(d, q);
        let atlas = self.probe_atlas.get_or_insert_with(|| create_probe_atlas(d));
        let probe_bytes: &[u8] = bytemuck::cast_slice(&emissive_probes);
        if !probe_bytes.is_empty() && probe_bytes != self.probe_atlas_bytes.as_slice() {
            let width = crate::emissive_lights::PROBE_ATLAS_COLUMNS * crate::emissive_lights::PROBE_RES;
            let rows = (emissive_probes.len() as u32 / width).min(atlas.0.height());
            q.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &atlas.0,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                &probe_bytes[..(rows * width * 4) as usize],
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(rows),
                },
                extent3d(width, rows),
            );
            self.probe_atlas_bytes.clear();
            self.probe_atlas_bytes.extend_from_slice(probe_bytes);
        }
        Ok(())
    }

    /// Builds the frame uniform and defers this frame's buffer writes (frame, GI grid and
    /// probes, sun, lights, shadow tiles) to the staging belt.
    pub(super) fn write_frame_uniforms(&mut self, d: &crate::TrackedDevice, inputs: FrameUniformInputs<'_>) {
        let FrameUniformInputs {
            scene,
            plan,
            uploaded,
            emissive_records,
            directional,
            fill_light,
            forward_fog,
            view,
            view_proj,
            unjittered_vp,
            prev_vp,
            fov_y,
            aspect,
            width,
            height,
            internal_w,
            internal_h,
            linear_hdr,
            pre_exposure,
            taau_on,
            tsr_frame,
            temporal_index,
            texture_lod_bias,
            sssr_on,
            bent_on,
            planet_ldr,
        } = inputs;
        let material_bias = texture_lod_bias + self.pipes.as_ref().map_or(0.0, |p| p.material_lod_offset);
        let ibl_lod = if scene.ibl {
            self.ibl.as_ref().map_or(0.0, |ibl| ibl.prefilter_mips)
        } else {
            0.0
        };
        let light_count = (uploaded.len() + emissive_records.len()) as f32;
        let gi_on = matches!(scene.quality.gi, crate::profile::GiMode::Probes)
            && self
                .gi_field
                .as_ref()
                .is_some_and(|field| field.probe_count() > 0);
        // The interior volume that contains the eye localises the interior shade to
        // its box (the runtime copies the volumes into the outdoor params). With none,
        // an interior frame (enclosure test) is interior everywhere, as before.
        let interior_box = if scene.quality.interior_ambient {
            self.outdoor
                .params()
                .interiors
                .iter()
                .copied()
                .find(|volume| volume.contains(scene.eye))
        } else {
            None
        };
        // The planet bake's sky map is the background of a planet frame without the LUT sky.
        let studio = if self.studio_background || planet_ldr {
            1.0
        } else {
            0.0
        };
        let (env_ground, env_ground_light) = self.studio.ground_uniforms(self.ibl.as_ref(), &directional);
        // Bit 7 of `interior_half.w`: time-reading graphs read `clock` instead of slot 0.
        let clock_live = self.material_time.is_some() || self.foliage_wind.is_some();
        let clock = super::gpu_types::graph_clock_row(
            self.material_time.unwrap_or((0.0, 0.0)),
            self.foliage_wind.unwrap_or([0.0, 0.0]),
        );
        let frame = LabFrame {
            view_proj: view_proj.to_cols_array_2d(),
            view: view.to_cols_array_2d(),
            camera_pos: [
                scene.eye[0],
                scene.eye[1],
                scene.eye[2],
                if scene.quality.interior_ambient { 1.0 } else { 0.0 },
            ],
            params: [scene.near, scene.far, fov_y, aspect],
            screen: [
                if linear_hdr {
                    internal_w as f32
                } else {
                    width as f32
                },
                if linear_hdr {
                    internal_h as f32
                } else {
                    height as f32
                },
                light_count,
                scene.quality.max_lights_per_cluster as f32,
            ],
            extra: [
                if scene.directional_shadows { 1.0 } else { 0.0 },
                plan.atlas_size as f32,
                if scene.ibl { 1.0 } else { 0.0 },
                ibl_lod,
            ],
            post_exposure: [
                if linear_hdr { pre_exposure } else { 0.0 },
                crate::post::exposure_from_ev100(scene.exposure_ev),
                crate::SKY_IBL_SCALE,
                scene.ambient_lux,
            ],
            fog_params: forward_fog.params_vec4(),
            fog_color: forward_fog.color_vec4(),
            quality: [
                scene.quality.shadow_filter_mode() as f32,
                if gi_on { 2.0 } else { 0.0 },
                scene.quality.max_reflection_probes as f32,
                // Per-frame noise offset for the rotated shadow kernels: varies only
                // under TAA (which averages it), so a non-temporal frame is stable.
                // The interior flag lives in `camera_pos.w`; this channel is the noise.
                temporal_noise_index(taau_on, temporal_index),
            ],
            prev_view_proj: prev_vp,
            unjittered_view_proj: unjittered_vp.to_cols_array_2d(),
            pcss: {
                let (search, filter) = scene.quality.pcss_taps();
                // `z` is the parallax profile gate; `w` is the local-shadow debug
                // mode (0 off, 1 shadow factor, 2 atlas tile). `quality.w` stays
                // the noise index.
                [
                    search as f32,
                    filter as f32,
                    if scene.quality.parallax_occlusion {
                        1.0
                    } else {
                        0.0
                    },
                    scene.quality.shadow_debug as f32,
                ]
            },
            interior_center: interior_box.map_or([0.0; 4], |v| [v.center[0], v.center[1], v.center[2], 1.0]),
            // `w` flags: bit 0 = TSR hashed alpha test, bit 1 = the forward pass packs the
            // stochastic SSR surface code into the HDR alpha, bit 2 = emissive meshes are lit
            // by fitted area lights, so their pixels mark the HDR alpha for the screen-space GI
            // to skip (`EMITTER_ALPHA`), bit 3 = reflection occlusion (`fs_transmit_add` traces a
            // pane's reflection against the opaque depth), bit 4 = bent normals: layer 1 of the AO
            // texture holds each texel's bent normal (ADR-1375), bit 5 = the view is held (the
            // unjittered view-projection equals the last frame's, or there is no last frame:
            // `prev_vp` then falls back to this frame's): the hashed alpha test keeps interleaved
            // gradient noise, so a held sequence draws what it drew before ADR-1489; a moving view
            // takes the lattice-free phase.
            interior_half: {
                let hashed_alpha = (tsr_frame && self.tsr_pass.settings().hashed_alpha)
                    || (taau_on && self.taa_hashed_alpha);
                let emitters_as_lights = !emissive_records.is_empty();
                let view_held = prev_vp == unjittered_vp.to_cols_array_2d();
                // Bit 6: the forward pass's stochastic terms read the blue-noise table bound at
                // view binding 38 (ADR-2652).
                // Bit 7: the frame clock is live (ADR-5160). Graphs read `LabFrame::clock`.
                let forward_blue_noise =
                    scene.quality.blue_noise && self.pipes.as_ref().is_some_and(|p| p.blue_noise_binding);
                let flags = f32::from(
                    u8::from(hashed_alpha)
                        | (u8::from(sssr_on) << 1)
                        | (u8::from(emitters_as_lights) << 2)
                        | (u8::from(scene.quality.reflection_occlusion) << 3)
                        | (u8::from(bent_on) << 4)
                        | (u8::from(view_held) << 5)
                        | (u8::from(forward_blue_noise) << 6)
                        | (u8::from(clock_live) << 7),
                );
                let [x, y, z] = interior_box.map_or([0.0; 3], |v| v.half_extent);
                [x, y, z, flags]
            },
            fill_dir: fill_light.map_or([0.0; 4], |[d, _]| [d[0], d[1], d[2], 1.0]),
            // `specular_aa.w` = 1 when the IBL is a reference studio's environment
            // ([`FrameRenderer::set_studio_sky`]); `fill_color.w` holds the material texture mip
            // bias: the TSR bias plus the device's anisotropic-blend offset (ADR-5670).
            fill_color: fill_light.map_or([0.0, 0.0, 0.0, material_bias], |[_, c]| {
                [c[0], c[1], c[2], material_bias]
            }),
            specular_aa: if scene.quality.specular_aa {
                [
                    crate::specular_aa::SCREEN_VARIANCE,
                    crate::specular_aa::THRESHOLD,
                    crate::specular_aa::TOKSVIG_FLOOR,
                    studio,
                ]
            } else {
                [0.0, 0.0, 0.0, studio]
            },
            cloud_sun: self.outdoor.cloud_sun_params(),
            env_ground,
            env_ground_light,
            env_view: self.studio.env_view(self.capturing_probe),
            background: self.studio.background_uniform(),
            capture_center: self.studio.capture_center(),
            capture_half: self.studio.capture_half(),
            weather: self.weather.surface.row(),
            weather2: self.weather.uniform_row(plan.precip_tile),
            clock,
            // A capture's faces show the lit scene: no debug view there.
            view_mode: if self.capturing_probe {
                [
                    0.0,
                    self.lights_views.view_mode[1],
                    self.lights_views.view_mode[2],
                    0.0,
                ]
            } else {
                self.lights_views.view_mode
            },
            extra_dir: self.lights_views.extra_dir,
            ambient_box: self.lights_views.ambient_box,
            ambient_rot: self.lights_views.ambient_rot,
        };

        // ---- Per-frame buffer writes (skipped when the bytes already match) ----
        // Deferred until the frame encoder exists, then uploaded as one chunk.
        crate::staging::defer_if_changed(
            &mut self.belt,
            d,
            &mut self.frame_bytes,
            self.frame_buf.as_ref().expect("frame buffer"),
            bytemuck::bytes_of(&frame),
        );
        // The forward decals for this view (ADR-3255), staged like the frame uniform.
        self.stage_forward_decals(d, view, view_proj, scene.eye, frame.screen);
        let grid = self
            .gi_field
            .as_ref()
            .map(crate::gi::GiField::gpu_grid)
            .unwrap_or(crate::gi::GpuGiGrid {
                origin: [0.0; 4],
                spacing: [1.0, 1.0, 1.0, 0.0],
                dims: [0, 0, 0, 0],
            });
        crate::staging::defer_if_changed(
            &mut self.belt,
            d,
            &mut self.gi_grid_bytes,
            self.gi_grid_buf.as_ref().expect("gi grid"),
            bytemuck::bytes_of(&grid),
        );
        if self.gi_field.is_none() {
            if !self.gi_probes_zeroed {
                let zeros = vec![0u8; std::mem::size_of::<crate::gi::GpuProbe>() * crate::gi::MAX_PROBES];
                self.belt
                    .defer(d, self.gi_buf.as_ref().expect("gi probes"), 0, &zeros);
                self.gi_probes_zeroed = true;
                self.gi_probe_bytes.clear();
            }
        } else {
            self.gi_probes_zeroed = false;
            let mut packed = self.scratch.gi_probes.take();
            if let Some(field) = self.gi_field.as_ref() {
                field.gpu_probes_into(&mut packed);
            }
            packed.resize(crate::gi::MAX_PROBES, bytemuck::Zeroable::zeroed());
            crate::staging::defer_if_changed(
                &mut self.belt,
                d,
                &mut self.gi_probe_bytes,
                self.gi_buf.as_ref().expect("gi probes"),
                bytemuck::cast_slice(&packed),
            );
            self.scratch.gi_probes.give(packed);
        }
        crate::staging::defer_if_changed(
            &mut self.belt,
            d,
            &mut self.dir_bytes,
            self.dir_buf.as_ref().expect("dir buffer"),
            bytemuck::bytes_of(&directional),
        );
        let mut gpu_lights = self.scratch.gpu_lights.take();
        gpu_lights.resize(
            crate::MAX_UPLOADED_LIGHTS as usize,
            GpuLight {
                position_kind: [0.0; 4],
                color_intensity: [0.0; 4],
                direction_radius: [0.0; 4],
                cone_shadow: [0.0; 4],
            },
        );
        for (i, light) in uploaded.iter().enumerate() {
            let tile = plan.light_tile.get(i).copied().unwrap_or(-1);
            gpu_lights[i] = pack_light(light, tile);
            // ADR-2592: a profiled light's kind fraction, nadir and peak; others stay bit-identical.
            self.light_extras
                .adjust_uploaded(i, bytemuck::cast_mut(&mut gpu_lights[i]));
        }
        for (slot, record) in gpu_lights.iter_mut().skip(uploaded.len()).zip(&emissive_records) {
            *slot = GpuLight {
                position_kind: record[0],
                color_intensity: record[1],
                direction_radius: record[2],
                cone_shadow: record[3],
            };
        }
        crate::staging::defer_if_changed(
            &mut self.belt,
            d,
            &mut self.lights_bytes,
            self.lights_buf.as_ref().expect("lights buffer"),
            bytemuck::cast_slice(&gpu_lights),
        );
        self.scratch.gpu_lights.give(gpu_lights);
        let mut tile_gpu = self.scratch.shadow_tiles.take();
        tile_gpu.extend(plan.tiles.iter().map(|tile| tile.gpu));
        while tile_gpu.len() < MAX_SHADOW_TILES {
            tile_gpu.push(ShadowTileGpu {
                view_proj: [[0.0; 4]; 4],
                uv: [1.0, 1.0, 0.0, 0.0],
                bias: [0.0; 4],
            });
        }
        crate::staging::defer_if_changed(
            &mut self.belt,
            d,
            &mut self.tiles_bytes,
            self.tiles_buf.as_ref().expect("tiles buffer"),
            bytemuck::cast_slice(&tile_gpu),
        );
        self.scratch.shadow_tiles.give(tile_gpu);
    }
}
