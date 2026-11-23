// SPDX-License-Identifier: MPL-2.0
//! The frame's modes and matrices (ADR-2300): the viewport, the internal size and upscaler, the
//! bind epoch, linear HDR and the post settings with their exposure, baked GI, bent normals,
//! stochastic SSR, the planet sky and fog, the indirect strategy, the jittered and unjittered
//! matrices and the culling frustum. Everything after it reads these. Moved out of
//! `FrameRenderer::render_into` unchanged.

use crate::frame::*;

/// The frame's modes and matrices.
pub(in crate::frame) struct FrameModes {
    /// The bind-cache epoch scope of the frame, entered after a new extent renewed the epoch; held to the end of `render_into`.
    /// Held for its drop only (in the frame recording context, ADR-4856), so nothing reads it.
    pub(in crate::frame) _bind_epoch: crate::bind_cache::EpochScope,
    /// A shadow diagnostic frame (a direct LDR image).
    pub(in crate::frame) shadow_debug: bool,
    /// The target is not sRGB: the shader encodes.
    pub(in crate::frame) encode_target: bool,
    /// Viewport rectangle clamped to the target.
    pub(in crate::frame) viewport: [f32; 4],
    /// The view matrix.
    pub(in crate::frame) view: regia_core::glam::Mat4,
    /// Vertical field of view (horizontal-plus applied).
    pub(in crate::frame) fov_y: f32,
    /// Internal (HDR) width.
    pub(in crate::frame) internal_w: u32,
    /// Internal (HDR) height.
    pub(in crate::frame) internal_h: u32,
    /// The internal size differs from the output.
    pub(in crate::frame) scaled: bool,
    /// Temporal AA/upscale runs.
    pub(in crate::frame) taau_on: bool,
    /// The SGSR spatial upscaler replaces the HDR upsample.
    pub(in crate::frame) sgsr_on: bool,
    /// Linear HDR target with exposure deferred to post.
    pub(in crate::frame) linear_hdr: bool,
    /// TSR resolves this frame.
    pub(in crate::frame) tsr_frame: bool,
    /// Material texture LOD bias.
    pub(in crate::frame) texture_lod_bias: f32,
    /// Terrain texture LOD bias.
    pub(in crate::frame) terrain_texture_lod_bias: f32,
    /// The post settings this frame records.
    pub(in crate::frame) forced_post: Option<crate::post::PostSettings>,
    /// The exposure the post stack applies.
    pub(in crate::frame) post_exposure: f32,
    /// Pre-exposure of the linear HDR target.
    pub(in crate::frame) pre_exposure: f32,
    /// Rescale of history written at another pre-exposure.
    pub(in crate::frame) history_rescale: f32,
    /// MSAA samples (1 or 4).
    pub(in crate::frame) samples_early: u32,
    /// Baked GI lights this frame.
    pub(in crate::frame) baked_on: bool,
    /// The quality the GI passes read (the baked policy when `baked_on`).
    pub(in crate::frame) gi_quality: FrameQuality,
    /// Camera-to-world rotation for bent normals, when they run.
    pub(in crate::frame) bent_to_world: Option<[[f32; 4]; 3]>,
    /// Bent normals run.
    pub(in crate::frame) bent_on: bool,
    /// Stochastic SSR is chosen for this frame's target.
    pub(in crate::frame) sssr_chosen: bool,
    /// Stochastic SSR runs (something glossy is in view).
    pub(in crate::frame) sssr_on: bool,
    /// The planet's LUT sky is wanted.
    pub(in crate::frame) planet_sky: bool,
    /// The LUT sky draws.
    pub(in crate::frame) sky_pass: bool,
    /// A planet's LDR probe face.
    pub(in crate::frame) planet_ldr: bool,
    /// The planet's atmosphere lights the IBL.
    pub(in crate::frame) planet_ibl: bool,
    /// The planet has no atmosphere.
    pub(in crate::frame) airless: bool,
    /// Fog the forward pass applies.
    pub(in crate::frame) forward_fog: crate::FogSettings,
    /// The indirect draw strategy.
    pub(in crate::frame) strategy: crate::gpu_driven::IndirectStrategy,
    /// Objects live in GPU storage.
    pub(in crate::frame) gpu_objects: bool,
    /// Sub-pixel jitter.
    pub(in crate::frame) jitter: [f32; 2],
    /// Projection without jitter.
    pub(in crate::frame) unjittered_proj: regia_core::glam::Mat4,
    /// Jittered projection.
    pub(in crate::frame) proj: regia_core::glam::Mat4,
    /// View-projection without jitter.
    pub(in crate::frame) unjittered_vp: regia_core::glam::Mat4,
    /// Jittered view-projection.
    pub(in crate::frame) view_proj: regia_core::glam::Mat4,
    /// The culling frustum (unjittered).
    pub(in crate::frame) frustum: Frustum,
    /// Previous frame's unjittered view-projection.
    pub(in crate::frame) prev_vp: [[f32; 4]; 4],
}

/// What the modes step reads from the frame's opening.
pub(in crate::frame) struct ModeInputs {
    /// Output width.
    pub(in crate::frame) width: u32,
    /// Output height.
    pub(in crate::frame) height: u32,
    /// Output aspect ratio.
    pub(in crate::frame) aspect: f32,
    /// The frame renders into a texture view.
    pub(in crate::frame) view_pass: bool,
    /// Frames since the last camera cut.
    pub(in crate::frame) temporal_index: u64,
}

impl FrameRenderer {
    /// Computes the frame's modes and matrices; renews the bind epoch on a new extent, records
    /// the extent, the texture LOD bias and the pre-exposure, and prepares the post exposure.
    pub(in crate::frame) fn frame_modes(
        &mut self,
        device: &RenderDevice,
        d: &crate::TrackedDevice,
        scene: &FrameScene<'_>,
        target: &FrameTarget<'_>,
        inputs: ModeInputs,
    ) -> FrameModes {
        let ModeInputs {
            width,
            height,
            aspect,
            view_pass,
            temporal_index,
        } = inputs;
        // A shadow diagnostic is a direct LDR image. Post and TAAU would tonemap
        // the debug colour and hide the factor.
        let shadow_debug = scene.quality.shadow_debug != 0;
        let post_enabled = !shadow_debug && scene.post.is_some_and(|settings| settings.enabled);
        let encode_target = !target.format.is_srgb();
        // Viewport rectangle clamped to the target (full target by default).
        let vp = target.viewport;
        let vp_w = vp[2]
            .min(target.width.saturating_sub(vp[0].min(target.width)))
            .max(1);
        let vp_h = vp[3]
            .min(target.height.saturating_sub(vp[1].min(target.height)))
            .max(1);
        let vp_x = vp[0].min(target.width.saturating_sub(1));
        let vp_y = vp[1].min(target.height.saturating_sub(1));
        let viewport = [vp_x as f32, vp_y as f32, vp_w as f32, vp_h as f32];

        // ---- View matrices ----
        // `camera_up` is +Y for every ordinary view. A cubemap face passes its
        // own up: ±Y is parallel to world up, and that look-at does not frame
        // the face.
        let eye_camera = self.stereo.camera;
        let view = eye_camera.map_or_else(
            || {
                look_at_view(
                    regia_core::glam::Vec3::from_array(scene.eye),
                    regia_core::glam::Vec3::from_array(scene.target),
                    camera_up_vector(self.camera_up),
                )
            },
            |eye| eye.view,
        );
        let fov_y = if scene.quality.horizontal_plus {
            crate::output::horizontal_plus_fov_y(scene.fov_y, aspect)
        } else {
            scene.fov_y
        };
        // `render_scale` changes the HDR size only when something will upscale it
        // (the post stack, or TAAU). Native LAB frames stay at the output size.
        let (internal_w, internal_h) = if post_enabled || (scene.quality.taau && !shadow_debug) {
            // A selected vendor upscaler renders at the size its runtime asks for (ADR-2679); every
            // other frame gets `requested` back.
            let vendor_frame = scene.quality.taau && !shadow_debug && scene.quality.msaa_samples < 4;
            let requested = scene.quality.scaled_extent(width, height);
            self.vendor_upscaler
                .render_extent(d, requested, (width, height), &scene.quality, vendor_frame)
        } else {
            (width, height)
        };
        let scaled = internal_w != width || internal_h != height;
        // Native-scale TAA resolves 1:1; a scaled frame upsamples as before.
        let taau_on = scene.quality.taau && !shadow_debug;
        // A scaled frame without TAAU on the SGSR profile skips the HDR bilinear upsample: the
        // post stack runs at the internal size and its last pass upscales the display image.
        let sgsr_on =
            scaled && !taau_on && scene.quality.spatial_upscaler == crate::profile::SpatialUpscaler::Sgsr;
        // A new extent replaces the size-dependent targets: the bind caches keyed by the old
        // ones must let them go (a fullscreen toggle or a dynamic-resolution step otherwise
        // kept every old set until the caches' cap; DO27, v0.1.4, 17.5 GB on an RX 7900).
        if self.bind_epoch == 0
            || self.last_extent.is_some_and(|last| {
                last.output != (width, height) || last.internal != (internal_w, internal_h)
            })
        {
            self.bind_epoch = crate::bind_cache::new_epoch();
            self.screen_cache_key = None;
            self.screen_cache_view = None;
        }
        let bind_epoch = crate::bind_cache::EpochScope::enter(self.bind_epoch);
        self.last_extent = Some(FrameExtent {
            output: (width, height),
            internal: (internal_w, internal_h),
            upscaler: if !scaled && !taau_on {
                "native"
            } else if taau_on {
                match self.temporal_upscaler {
                    crate::TemporalUpscaler::Taau => "TAAU",
                    crate::TemporalUpscaler::Tsr => "TSR",
                    crate::TemporalUpscaler::Fsr3 => "FSR 3",
                }
            } else if sgsr_on {
                "SGSR"
            } else {
                "bilinear"
            },
        });
        // Linear HDR (exposure deferred to post) whenever the post stack runs, or
        // TAAU has to upscale a scene-referred image before tonemap.
        let linear_hdr = post_enabled || taau_on;
        // Under TSR upscaling, material and terrain textures pick the mip the output
        // resolution would (negative bias); the jittered accumulation resolves it. Every
        // other frame keeps bias 0 (TAAU and native frames are unchanged).
        // TSR resolves this frame (single-sample TAA with the TSR upscaler selected): masked
        // materials take the hashed alpha test its accumulation turns into edge coverage.
        let tsr_frame = taau_on
            && scene.quality.msaa_samples < 4
            && self.temporal_upscaler == crate::TemporalUpscaler::Tsr;
        let texture_lod_bias = if tsr_frame && scaled {
            crate::tsr::texture_lod_bias(internal_w, width, self.tsr_pass.settings().texture_bias_offset)
        } else {
            0.0
        };
        self.texture_lod_bias = texture_lod_bias;
        // The terrain takes the same bias on every temporally upscaled frame, TAAU included
        // (ADR-192): its layers are high-frequency tiling detail that the jittered history
        // resolves, and at TAAU's 0.58–0.67 scales the render-resolution mips left grazing
        // ground at the mean color of its layers (DO27 request 20). Mesh materials keep
        // ADR-101's TSR-only rule.
        let terrain_texture_lod_bias = if taau_on && scene.quality.msaa_samples < 4 && scaled {
            crate::tsr::texture_lod_bias(internal_w, width, self.tsr_pass.settings().texture_bias_offset)
        } else {
            0.0
        };
        // The post settings this frame records. Computed before any colour pass: the HDR
        // target is pre-exposed by the exposure the post stack is about to apply (auto
        // exposure included), and a translucent layer drawing into it divides by that.
        // TAAU already rejected history; the single-frame post TAA must not run as well.
        // A scaled frame with post disabled still tonemaps here so the swapchain gets LDR.
        let mut forced_post = scene.post.filter(|settings| settings.enabled).cloned();
        if let Some(settings) = forced_post.as_mut() {
            if taau_on {
                settings.taa = false;
            }
        } else if linear_hdr {
            forced_post = Some(crate::post::PostSettings {
                enabled: true,
                bloom: false,
                auto_exposure: false,
                fxaa: false,
                taa: false,
                film_grain: 0.0,
                vignette: 0.0,
                exposure_ev: scene.exposure_ev,
                ..crate::post::PostSettings::default()
            });
        }
        // Exposure zones (ADR-322) drive the main view's eye only; a texture view meters alone.
        let post_exposure = match forced_post.as_ref() {
            Some(settings) => {
                let post = self.post.get_or_insert_with(Default::default);
                post.set_wait_readbacks(self.wait_exposure_readback);
                post.prepare_exposure(d, settings, (!view_pass).then_some(&mut self.exposure_zones))
            }
            None => 1.0,
        };
        // Pre-exposure of the linear HDR target (`pre_exposure_scale`): every pass writing
        // radiance into it multiplies by this, the post stack and the HDR capture divide it
        // out. 1 on a display-referred frame.
        let pre_exposure = if linear_hdr {
            crate::post::pre_exposure_scale(post_exposure)
        } else {
            1.0
        };
        // History written at another pre-exposure is rescaled into this one.
        let history_rescale = if self.prev_pre_exposure > 0.0 {
            pre_exposure / self.prev_pre_exposure
        } else {
            1.0
        };
        self.prev_pre_exposure = pre_exposure;
        // Indirect object storage needs single-sample colour. MSAA stays on the
        // uniform path so the resolve and the golden pixels stay unchanged.
        let samples_early = if scene.quality.msaa_samples >= 4 { 4 } else { 1 };
        // Baked GI (`crate::baked_gi`): the map's volume lights linear-HDR, single-sample
        // prepass frames of the main view. While it does, the probe update (DDGI) and SSGI are
        // off and full-resolution GTAO runs at half resolution: the bake carries the bounce and
        // the long-range sky visibility; the sun, its shadows and the paged cache stay live.
        let baked_on = !view_pass
            && linear_hdr
            && samples_early == 1
            && scene.depth_prepass
            && self.baked_gi.wants(scene.quality.baked_gi);
        let gi_quality = if baked_on {
            crate::baked_gi::policy(scene.quality)
        } else {
            scene.quality
        };
        // Bent normals (ADR-1375): the GTAO rows' SSAO kernel also writes each texel's bent normal
        // into layer 1 of its target (the camera-to-world rotation turns it into world space) and
        // the forward pass reads its indirect light along it (`interior_half.w` bit 4). The same
        // conditions as `ssao_active` below, on a GTAO row.
        let bent_to_world = (scene.quality.bent_normals
            && samples_early == 1
            && scene.depth_prepass
            && matches!(
                gi_quality.ssao,
                crate::profile::SsaoMode::GtaoHalfRes | crate::profile::SsaoMode::GtaoFull
            ))
        .then(|| {
            let to_world = view.inverse();
            [to_world.x_axis, to_world.y_axis, to_world.z_axis].map(|c| [c.x, c.y, c.z, 0.0])
        });
        let bent_on = bent_to_world.is_some();
        if !view_pass && !baked_on {
            self.baked_gi.deactivate();
        }
        // Stochastic SSR this frame: the forward pass packs its surface code into the HDR alpha
        // (`interior_half.w` bit 1), which needs the Rgba16Float target and a depth prepass.
        let rg11_hdr = matches!(scene.quality.hdr_target, crate::profile::HdrTarget::Rg11B10)
            && device.capabilities.rg11b10_renderable;
        let sssr_chosen = linear_hdr
            && samples_early == 1
            && scene.depth_prepass
            && !rg11_hdr
            && scene.quality.stochastic_ssr();
        // Nothing that can be glossy in view: no pass at all, and no older march in its place.
        // The shared draw (the terrain) counts only when it opted in (a wet or icy material,
        // `SharedPassDraw::screen_space_reflective`); its older 0..1 alpha is never traced.
        let sssr_on = sssr_chosen
            && (self
                .shared_draw
                .as_ref()
                .is_some_and(|draw| draw.screen_space_reflective())
                || scene
                    .meshes
                    .iter()
                    .any(|m| crate::sssr::material_may_be_glossy(m.material)));
        // The LUT sky is drawn only into a single-sample HDR target. A frame that
        // resolves straight to sRGB still uses the analytic sky in the forward
        // shader, so its IBL stays the procedural sky that matches that colour.
        // Earth uses the LUT sky on High/Ultra only (`physical_atmosphere`); Mars and an
        // airless moon have no cheap stand-in, so they use it wherever the frame is HDR
        // single-sample (the table lookup is two full-screen passes; fog and clouds stay
        // High/Ultra). ADR-039.
        // Under `EnvironmentAuthority::Studio` an installed studio environment wins: the
        // atmosphere's bake would overwrite its maps and the LUT sky hide its background
        // (ADR-2000 (0)). The default keeps v0.2.8's frame, the atmosphere over it (ADR-2016).
        let studio_rules = self.studio_environment_rules();
        let planet_sky = crate::atmosphere::planet_lut_sky(
            scene.quality.physical_atmosphere,
            self.outdoor.params().is_earth(),
            studio_rules,
        );
        let sky_pass = linear_hdr && samples_early == 1 && planet_sky;
        // A reflection-probe face on another planet (LDR, no post stack, so no LUT sky) is
        // still lit by that planet's atmosphere: its sun, its IBL bake, and its sky map as the
        // background. The analytic sky is Earth's (blue zenith), and the procedural IBL it
        // swapped in turned DO27's Mars probes sky-blue. Other LDR or MSAA frames on a planet
        // keep the analytic sky (unchanged in v0.1.5).
        let planet_ldr = self.capturing_probe
            && scene.ibl
            && !sky_pass
            && !self.outdoor.params().is_earth()
            && !studio_rules;
        let planet_ibl = sky_pass || planet_ldr;
        let airless = self.outdoor.airless();
        // The LUT sky replaces the procedural sky draw and the forward aerial
        // fog (aerial.wgsl): the outdoor composite adds aerial perspective and
        // froxel fog after SSR, so the forward pass must not haze the image too.
        let forward_fog = if sky_pass || airless || planet_ldr {
            crate::FogSettings {
                enabled: false,
                ..scene.fog
            }
        } else {
            scene.fog
        };
        let strategy = if scene.quality.gpu_driven && samples_early == 1 {
            crate::gpu_driven::IndirectStrategy::select(&device.capabilities)
        } else {
            crate::gpu_driven::IndirectStrategy::Cpu
        };
        let gpu_objects = strategy.uses_gpu_objects();
        let jitter = if taau_on {
            crate::taau::halton_jitter(temporal_index, internal_w, internal_h)
        } else {
            [0.0, 0.0]
        };
        let unjittered_proj = eye_camera.map_or_else(
            || perspective(fov_y, aspect, scene.near, scene.far),
            |eye| eye.projection,
        );
        let proj = crate::taau::jitter_projection(unjittered_proj, jitter);
        let unjittered_vp = unjittered_proj * view;
        let view_proj = proj * view;
        // Cull with the unjittered frustum so a sub-pixel jitter does not pop objects.
        let frustum = Frustum::from_view_proj(&eye_camera.map_or(unjittered_vp, |eye| eye.cull));
        // The previous frame's matrix without its jitter: a motion vector is the
        // movement of the surface, and the jitter is not movement. Stored jittered, a held
        // camera fetched its history half a pixel off in a new direction every frame — a
        // bilinear random walk that blurred the resolve to a third of the native detail
        // (motion-look lane; Karis 2014, Yang et al. 2020 § 3.1).
        let prev_vp = if self.have_prev_vp {
            self.prev_view_proj
        } else {
            unjittered_vp.to_cols_array_2d()
        };
        FrameModes {
            _bind_epoch: bind_epoch,
            shadow_debug,
            encode_target,
            viewport,
            view,
            fov_y,
            internal_w,
            internal_h,
            scaled,
            taau_on,
            sgsr_on,
            linear_hdr,
            tsr_frame,
            texture_lod_bias,
            terrain_texture_lod_bias,
            forced_post,
            post_exposure,
            pre_exposure,
            history_rescale,
            samples_early,
            baked_on,
            gi_quality,
            bent_to_world,
            bent_on,
            sssr_chosen,
            sssr_on,
            planet_sky,
            sky_pass,
            planet_ldr,
            planet_ibl,
            airless,
            forward_fog,
            strategy,
            gpu_objects,
            jitter,
            unjittered_proj,
            proj,
            unjittered_vp,
            view_proj,
            frustum,
            prev_vp,
        }
    }
}
