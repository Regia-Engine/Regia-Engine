// SPDX-License-Identifier: MPL-2.0
//! Offscreen renders, render-to-texture views and their readbacks (moved out of `frame.rs`
//! unchanged, ADR-1552).

use super::*;

impl FrameRenderer {
    /// Renders `scene` offscreen and returns tightly packed RGBA8 pixels
    /// (the golden-image path).
    #[tracing::instrument(skip_all)]
    pub fn render_offscreen(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
    ) -> Result<ForwardResult, RenderError> {
        let target = self.begin_offscreen(device, scene, PROBE_CAPTURE_FORMAT)?;
        self.read_offscreen(device, target)
    }

    /// Renders a pixel window of a frozen, full-resolution scene (ADR-8200).
    ///
    /// `scene.width` and `scene.height` describe the complete image, even when larger than
    /// the device texture limit. Only `rect` (`[x, y, width, height]`, top-left origin) is
    /// allocated and returned. Call once per tile, then stitch the tightly packed RGBA8 rows.
    /// The caller must keep scene inputs and installed renderer state fixed across tiles.
    ///
    /// This built-in still policy applies even to a full-image rectangle: native resolution, fixed
    /// exposure, no temporal AA/history, screen AO/GI/reflections/refraction/contact shadows,
    /// screen subsurface scattering, volumetric fog/clouds, bloom, FXAA/SMAA, blur, local
    /// exposure, vignette, grain or lens effects. Consequently no warm frames are needed.
    /// Tone mapping and color grading are retained. Sun shadows use the full camera frustum.
    /// Complete-frame mode settles pending material/sky work and streamed uploads without
    /// fades for the tile; the caller's previous complete-frame setting is restored afterward.
    /// Custom frame passes must themselves support tiled inputs; this method cannot freeze or
    /// adapt opaque project hooks. It does not change graph order or permanently change the
    /// renderer's camera.
    ///
    /// # Errors
    ///
    /// [`RenderError::Readback`] for an invalid rectangle or a tile larger than the device
    /// limit, otherwise the same errors as [`Self::render_offscreen`].
    #[tracing::instrument(skip_all, fields(?rect))]
    pub fn render_camera_window(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        rect: [u32; 4],
    ) -> Result<ForwardResult, RenderError> {
        let size = [scene.width, scene.height];
        let aspect = scene.width as f32 / scene.height.max(1) as f32;
        let fov_y = if scene.quality.horizontal_plus {
            crate::output::horizontal_plus_fov_y(scene.fov_y, aspect)
        } else {
            scene.fov_y
        };
        let camera = crate::ExtractedCamera {
            eye: scene.eye,
            target: scene.target,
            fov_y,
            near: scene.near,
            far: scene.far,
        }
        .window(size, rect)
        .ok_or_else(|| RenderError::Readback("invalid camera window".into()))?;
        let limit = device.device.limits().max_texture_dimension_2d;
        if rect[2] > limit || rect[3] > limit {
            return Err(RenderError::Readback(
                "camera window exceeds device texture limit".into(),
            ));
        }
        // Sync before installing temporary state: shader replacement can rebuild the renderer.
        self.sync_shader_epoch();
        let previous_complete = self.complete_frames();
        self.set_complete_frames(true);
        let previous_camera = self.stereo.camera.replace(camera);
        let previous_size = self.stereo.capture_size.replace(size);
        let mut post = scene.post.cloned();
        if let Some(post) = post.as_mut() {
            post.bloom = false;
            post.auto_exposure = false;
            post.fxaa = false;
            post.taa = false;
            post.vignette = 0.0;
            post.film_grain = 0.0;
            post.motion_blur = None;
            post.depth_of_field = None;
            post.local_exposure = None;
            post.lens_effects = crate::LensEffects::OFF;
        }
        let mut tile = FrameScene {
            width: rect[2],
            height: rect[3],
            fov_y,
            post: post.as_ref(),
            ..scene.clone()
        };
        let quality = &mut tile.quality;
        quality.horizontal_plus = false;
        quality.render_scale_permille = 1000;
        quality.upscale_quality = None;
        quality.taau = false;
        quality.ssao = crate::SsaoMode::Off;
        quality.contact_shadows = false;
        quality.reflection_occlusion = false;
        quality.bent_normals = false;
        quality.screen_space_gi = false;
        quality.ssr = false;
        quality.screen_space_refraction = false;
        quality.subsurface_scattering = false;
        quality.smaa = false;
        quality.volumetric_fog = false;
        quality.volumetric_clouds = false;
        quality.cloud_reprojection = false;
        quality.gi_probes = false;
        quality.restir_gi = false;
        quality.many_light_sampling = false;
        let result = self.render_offscreen(device, &tile);
        self.stereo.camera = previous_camera;
        self.stereo.capture_size = previous_size;
        self.set_complete_frames(previous_complete);
        result
    }

    /// Copies the last velocity target (`previous uv − current uv` in `xy`).
    ///
    /// `Ok(None)` when the velocity pass has not built a target (temporal anti-aliasing
    /// has not run, and [`Self::prewarm`] has not built the 1×1 stand-in). Pixels no
    /// surface wrote are `(0, 0)`. The texture is `Rgba16Float`.
    ///
    /// # Errors
    ///
    /// The copy or the map failed ([`RenderError::Readback`], [`RenderError::Poll`]).
    #[tracing::instrument(skip_all)]
    pub fn read_motion(&self, device: &RenderDevice) -> Result<Option<MotionField>, RenderError> {
        let Some((texture, width, height)) = self.taau_pass.velocity_texture() else {
            return Ok(None);
        };
        if width == 0 || height == 0 {
            return Ok(None);
        }
        let row = width * 8;
        let padded = row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let buffer = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("velocity readback"),
            size: u64::from(padded) * u64::from(height),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("velocity readback"),
            });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            extent3d(width, height),
        );
        crate::device::TrackedQueue::observe(&device.queue).submit([encoder.finish()]);
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        crate::device::TrackedDevice::observe(&device.device)
            .poll(wgpu::PollType::wait_indefinitely())
            .map_err(RenderError::Poll)?;
        rx.recv()
            .map_err(|_| RenderError::Readback("map callback dropped".into()))?
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let mapped = buffer
            .slice(..)
            .get_mapped_range()
            .map_err(|e| RenderError::Readback(e.to_string()))?;
        let mut motion = Vec::with_capacity((width * height) as usize);
        for y in 0..height as usize {
            let start = y * padded as usize;
            let row_bytes = &mapped[start..start + row as usize];
            for x in 0..width as usize {
                let o = x * 8;
                let hx = u16::from_le_bytes([row_bytes[o], row_bytes[o + 1]]);
                let hy = u16::from_le_bytes([row_bytes[o + 2], row_bytes[o + 3]]);
                motion.push([f16_to_f32(hx), f16_to_f32(hy)]);
            }
        }
        drop(mapped);
        buffer.unmap();
        Ok(Some(MotionField {
            width,
            height,
            motion,
        }))
    }

    /// [`Self::render_offscreen`] with the window overlay layers (the quick draw layer, the
    /// runtime UI) drawn over the frame, as a visible window shows it.
    #[tracing::instrument(skip_all)]
    pub fn render_offscreen_with_overlay(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        overlay: &crate::WindowOverlay,
    ) -> Result<ForwardResult, RenderError> {
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = self.begin_offscreen(device, scene, format)?;
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("offscreen overlay"),
            });
        crate::overlay::record_overlay_on(
            overlay,
            device,
            &mut encoder,
            Some(&target.texture),
            &target.view,
            format,
            target.width,
            target.height,
        );
        crate::device::TrackedQueue::observe(&device.queue).submit([encoder.finish()]);
        self.read_offscreen(device, target)
    }

    /// [`Self::render_offscreen`] with only the window overlay layers `keep` accepts (by name:
    /// [`crate::WindowOverlay::layer_names`]) drawn over the frame. Render-out uses it to add the
    /// game UI (`"ui"`) without the other window layers.
    #[tracing::instrument(skip_all)]
    pub fn render_offscreen_with_layers(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        overlay: &crate::WindowOverlay,
        keep: impl Fn(&str) -> bool,
    ) -> Result<ForwardResult, RenderError> {
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = self.begin_offscreen(device, scene, format)?;
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("offscreen layers"),
            });
        crate::overlay::record_overlay_filtered(
            overlay,
            device,
            &mut encoder,
            &target.view,
            Some(&target.texture),
            format,
            target.width,
            target.height,
            keep,
        );
        crate::device::TrackedQueue::observe(&device.queue).submit([encoder.finish()]);
        self.read_offscreen(device, target)
    }

    /// Only the window overlay layers `keep` accepts (by name), drawn over a `clear`ed
    /// `width`×`height` target with no scene under them (ADR-7217: the agent's `ui` screenshot
    /// target renders the UI layer alone). `clear` is straight RGBA in 0..=1; transparent black
    /// keeps every pixel the layers leave alone at alpha 0.
    #[tracing::instrument(skip_all)]
    pub fn render_layers_only(
        &mut self,
        device: &RenderDevice,
        width: u32,
        height: u32,
        overlay: &crate::WindowOverlay,
        clear: [f64; 4],
        keep: impl Fn(&str) -> bool,
    ) -> Result<ForwardResult, RenderError> {
        let format = wgpu::TextureFormat::Rgba8Unorm;
        let target = self.offscreen_target(device, width.max(1), height.max(1), false);
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("offscreen layers only"),
            });
        {
            let _clear = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("offscreen layers only clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target.view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: clear[0],
                            g: clear[1],
                            b: clear[2],
                            a: clear[3],
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
        }
        crate::overlay::record_overlay_filtered(
            overlay,
            device,
            &mut encoder,
            &target.view,
            Some(&target.texture),
            format,
            target.width,
            target.height,
            keep,
        );
        crate::device::TrackedQueue::observe(&device.queue).submit([encoder.finish()]);
        self.read_offscreen(device, target)
    }

    /// The persistent offscreen LDR target at `width`×`height` (created on first
    /// use) for callers that composite several views into one image.
    ///
    /// Render each view with [`Self::render_into`] into `target.view` (viewport
    /// rects + `load_color`), then read the pixels with [`Self::read_offscreen`].
    #[tracing::instrument(skip_all)]
    pub fn offscreen_target(
        &mut self,
        device: &RenderDevice,
        width: u32,
        height: u32,
        srgb: bool,
    ) -> OffscreenTarget {
        self.offscreen_ldr_view(device, width, height, srgb);
        self.targets
            .get(&(width, height))
            .and_then(|pair| pair.ldr_for(srgb).cloned())
            .unwrap_or_else(|| unreachable!("target_pair creates the requested LDR target"))
    }

    /// Renders `scene` into an internal offscreen LDR target and hands it back
    /// for overlay passes and [`Self::read_offscreen`].
    ///
    /// `format` decides how the final LDR image is stored: `Rgba8Unorm`
    /// (sRGB-encoded bytes, the golden readback) or `Rgba8UnormSrgb` (same
    /// bytes; the hardware-viewable form, used for hidden-window screenshots so
    /// the UI overlay composites identically to the visible surface).
    ///
    /// This is the capture entry: a captured frame (goldens, screenshots) takes
    /// the deterministic paths and puts them back after — the probes' mesh
    /// bricks bake before the trace, the cull keeps submission order, and
    /// emissive fits run complete (no wall-clock budget) — because a capture
    /// feeds blessed reference images and cross-machine comparisons. An
    /// interactive viewport that only feeds the screen wants
    /// [`Self::begin_offscreen_live`] instead, which skips the capture
    /// overrides and draws exactly like the window path.
    #[tracing::instrument(skip_all)]
    pub fn begin_offscreen(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        format: wgpu::TextureFormat,
    ) -> Result<OffscreenTarget, RenderError> {
        self.begin_offscreen_impl(device, scene, format, true)
    }

    /// [`Self::begin_offscreen`] without the capture overrides: the frame draws
    /// with the renderer's own settings (the ordered cull stays on unless the
    /// host chose the speed switch, emissive fits keep their wall-clock budget,
    /// DDGI bakes stay asynchronous) — the same choices as the windowed path.
    /// For interactive viewports whose image only feeds the screen; captures and
    /// goldens use [`Self::begin_offscreen`].
    #[tracing::instrument(skip_all)]
    pub fn begin_offscreen_live(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        format: wgpu::TextureFormat,
    ) -> Result<OffscreenTarget, RenderError> {
        self.begin_offscreen_impl(device, scene, format, false)
    }

    fn begin_offscreen_impl(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        format: wgpu::TextureFormat,
        capture: bool,
    ) -> Result<OffscreenTarget, RenderError> {
        self.sync_shader_epoch();
        let (width, height) = (scene.width.max(1), scene.height.max(1));
        let srgb = matches!(format, wgpu::TextureFormat::Rgba8UnormSrgb);
        let view = self.offscreen_ldr_view(device, width, height, srgb);
        let color = self
            .targets
            .get(&(width, height))
            .and_then(|pair| pair.ldr_for(srgb))
            .map(|target| target.texture.clone());
        // A captured frame (goldens, screenshots) takes the deterministic paths and puts them
        // back after: the probes' mesh bricks bake before the trace, the cull keeps submission
        // order, and emissive fits run complete (no wall-clock budget) — a capture feeds
        // blessed reference images and cross-machine comparisons. The interactive viewport
        // path (`begin_offscreen_live`) draws with the renderer's own settings, like the window.
        let guard = capture.then(|| CaptureDeterminism::enter(self));
        let drawn = self.render_into(
            device,
            scene,
            FrameTarget {
                view: &view,
                format,
                width,
                height,
                viewport: [0, 0, width, height],
                load_color: false,
                color: color.as_ref(),
            },
        );
        if let Some(guard) = guard {
            guard.exit(self);
        }
        drawn?;
        let target = self
            .targets
            .get(&(width, height))
            .and_then(|pair| pair.ldr_for(srgb).cloned())
            .ok_or_else(|| RenderError::Readback("offscreen target missing".into()))?;
        Ok(target)
    }

    /// The internal LDR target for offscreen frames at this size (created on first use).
    pub(super) fn offscreen_ldr_view(
        &mut self,
        device: &RenderDevice,
        width: u32,
        height: u32,
        srgb: bool,
    ) -> wgpu::TextureView {
        let view = self
            .target_pair(&device.device, width, height, srgb)
            .ldr_for(srgb)
            .map(|t| t.view.clone());
        view.unwrap_or_else(|| unreachable!("target_pair creates the requested LDR target"))
    }

    /// Reads an offscreen frame back into tightly packed RGBA8.
    #[tracing::instrument(skip_all)]
    pub fn read_offscreen(
        &mut self,
        device: &RenderDevice,
        target: OffscreenTarget,
    ) -> Result<ForwardResult, RenderError> {
        let pixels = read_rgba8(
            &device.device,
            &device.queue,
            &target.texture,
            target.width,
            target.height,
        )?;
        // The pixel read already waited. Publish timestamp slots that wait mapped
        // so a one-frame capture reports the passes it ran.
        self.collect_timings(device, 0);
        Ok(ForwardResult {
            width: target.width,
            height: target.height,
            pixels,
        })
    }

    /// Scene depth of the last frame whose attachment was `width`×`height`.
    ///
    /// Row-major [`wgpu::TextureFormat::Depth32Float`]. The depth pass clears to 1,
    /// the far plane. A pixel with no geometry — the sky mask `outdoor_sky.wgsl`
    /// (`fs_sky_hdr`) uses — is exactly 1; geometry is strictly less. A scaled frame
    /// stores this at the internal size, which equals the output size at
    /// `render_scale` 1.
    ///
    /// # Errors
    ///
    /// No depth target exists at this size, or the copy or map failed.
    #[tracing::instrument(skip_all)]
    pub fn read_scene_depth(
        &self,
        device: &RenderDevice,
        width: u32,
        height: u32,
    ) -> Result<Vec<f32>, RenderError> {
        let (width, height) = (width.max(1), height.max(1));
        let texture = self
            .targets
            .get(&(width, height))
            .map(|pair| pair.depth_texture.clone())
            .ok_or_else(|| RenderError::Readback(format!("no scene depth at {width}×{height}")))?;
        read_depth32(&device.device, &device.queue, &texture, width, height)
    }

    /// Renders `scene` into the persistent texture owned for the
    /// `RenderTarget::Texture` camera identified by `asset_id`
    /// (created or resized on first use / size change).
    ///
    /// The frame uses the main view's state (targets, history, shadow caches): this is the
    /// main view drawn into a texture (benches, tools). A secondary camera next to a window view
    /// (a screen, a mirror, a map) uses [`Self::render_texture_view`], which keeps its own.
    #[tracing::instrument(skip_all)]
    pub fn render_into_texture(
        &mut self,
        device: &RenderDevice,
        asset_id: u64,
        scene: &FrameScene<'_>,
    ) -> Result<(), RenderError> {
        self.sync_shader_epoch();
        let width = scene.width.max(1);
        let height = scene.height.max(1);
        let stale = self
            .texture_targets
            .get(&asset_id)
            .is_some_and(|target| target.width != width || target.height != height);
        if stale {
            self.texture_targets.remove(&asset_id);
        }
        let target = self
            .texture_targets
            .entry(asset_id)
            .or_insert_with(|| make_ldr(&device.device, width, height, wgpu::TextureFormat::Rgba8UnormSrgb));
        let view = target.view.clone();
        let texture = target.texture.clone();
        // Mirrors and UI previews show the world, not debug lines.
        let gizmos = self.gizmos.take();
        // A texture view is not the host's view: no cut detection, and a pending cut waits.
        self.cut.suspended += 1;
        let result = self.render_into(
            device,
            scene,
            FrameTarget {
                view: &view,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                width,
                height,
                viewport: [0, 0, width, height],
                load_color: false,
                color: Some(&texture),
            },
        );
        self.cut.suspended -= 1;
        self.gizmos = gizmos;
        result
    }

    /// Renders the render-to-texture camera `asset_id` (ADR-184 "Texture views keep their own
    /// view state"): a cockpit screen, a target feed, a map.
    ///
    /// Unlike [`Self::render_into_texture`] (which renders as the main view, for benches and
    /// hosts that drive one camera into a texture), the view keeps its own camera- and
    /// size-dependent state: targets, temporal history, shadow maps and caches, post chain,
    /// GPU timer and reports, and the shared draw's per-camera state
    /// ([`crate::SharedPassDraw::set_view`]). The main view's targets, history, paged shadows,
    /// terrain uploads, probes, atmosphere tables and exposure are untouched, so rendering
    /// views changes neither its pixels nor its per-frame work. Meshes, materials, textures and
    /// pipelines stay shared.
    ///
    /// `settings` picks the reduced feature set ([`crate::TextureViewSettings::apply`]: no
    /// temporal AA or upscale, no SSR, SSGI, SSAO, contact shadows, probe update, volumetric
    /// fog or clouds, effects or motion blur; a few short sun cascades redrawn whole). The image
    /// lands in the persistent texture [`Self::read_texture_target`] and
    /// [`Self::request_texture_readback`] read, and, when `settings.texture` is bound by a
    /// material, in that material's GPU texture at the end of this frame (draw the view
    /// before the views that sample it: the runtime renders texture views before the window
    /// in the same frame, so a screen shows this frame's image; a view that sees its own
    /// screen shows its previous image).
    ///
    /// # Errors
    ///
    /// As [`Self::render_into`]. The main view's state is restored either way.
    #[tracing::instrument(skip_all, fields(asset_id))]
    pub fn render_texture_view(
        &mut self,
        device: &RenderDevice,
        asset_id: u64,
        scene: FrameScene<'_>,
        settings: &crate::TextureViewSettings,
    ) -> Result<TextureViewFrame, RenderError> {
        let started = std::time::Instant::now();
        self.sync_shader_epoch();
        let mut quality = scene.quality;
        let mut post = scene.post.cloned();
        settings.apply(&mut quality, post.as_mut());
        let scene = FrameScene {
            quality,
            post: post.as_ref(),
            directional_shadows: scene.directional_shadows && settings.sun_shadows(),
            ..scene
        };
        let width = scene.width.max(1);
        let height = scene.height.max(1);
        let stale = self
            .texture_targets
            .get(&asset_id)
            .is_some_and(|target| target.width != width || target.height != height);
        if stale {
            self.texture_targets.remove(&asset_id);
        }
        let target = self
            .texture_targets
            .entry(asset_id)
            .or_insert_with(|| make_ldr(&device.device, width, height, wgpu::TextureFormat::Rgba8UnormSrgb));
        let view = target.view.clone();
        let texture = target.texture.clone();
        // The material texture this view feeds. It is created with the view, before any
        // material binds it, so a screen shows the view's image from its first frame.
        let previous = match settings.texture.as_ref() {
            Some(handle) => self.texture_view_handles.insert(asset_id, handle.clone()),
            None => self.texture_view_handles.remove(&asset_id),
        };
        if let Some(previous) = previous
            && settings.texture.as_ref().map(crate::DynamicTexture::share_key) != Some(previous.share_key())
        {
            self.shared_textures.release_unbound(&previous);
        }
        if let Some(handle) = settings.texture.as_ref()
            && let Some(destination) = {
                let queue = device.tracked_queue();
                let env = crate::texture_residency::UploadEnv {
                    features: device.device.features(),
                    material_key: 0,
                    name: None,
                    now: self.frame_index,
                    device: &device.device,
                    queue: &queue,
                };
                self.shared_textures.prepare_dynamic(&env, handle)
            }
        {
            if destination.size() == texture.size()
                && destination.format().remove_srgb_suffix() == texture.format().remove_srgb_suffix()
            {
                self.publish_copies.push((texture.clone(), destination));
            } else {
                static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
                    tracing::warn!(
                        target: "regia_render",
                        asset_id,
                        "texture view {width}x{height} does not match its material texture {}x{}; \
                         create the TextureViewSettings texture at the view's size",
                        destination.size().width,
                        destination.size().height
                    );
                }
            }
        }
        // Swap the view's state in. A view of a timed renderer gets its own timer (created once).
        let mut state = self
            .texture_views
            .remove(&asset_id)
            .unwrap_or_else(|| Box::new(view_state::ViewState::new()));
        let timed = self.gpu_timer.is_some();
        self.swap_view_state(&mut state);
        if timed && self.gpu_timer.is_none() {
            self.set_gpu_timing(device, true);
        }
        self.view_pass = Some(asset_id);
        if let Some(draw) = self.shared_draw.as_mut() {
            draw.set_view(Some(asset_id));
        }
        // Mirrors and screens show the world, not debug lines; the probes follow the main view.
        let gizmos = self.gizmos.take();
        let probes_active = std::mem::replace(&mut self.ddgi.active, false);
        let result = self.render_into(
            device,
            &scene,
            FrameTarget {
                view: &view,
                format: wgpu::TextureFormat::Rgba8UnormSrgb,
                width,
                height,
                viewport: [0, 0, width, height],
                load_color: false,
                color: Some(&texture),
            },
        );
        // Restore the main view whatever happened.
        self.publish_copies.clear();
        self.ddgi.active = probes_active;
        self.gizmos = gizmos;
        if let Some(draw) = self.shared_draw.as_mut() {
            draw.set_view(None);
        }
        self.view_pass = None;
        self.swap_view_state(&mut state);
        let frame = TextureViewFrame {
            cpu_ms: started.elapsed().as_secs_f32() * 1000.0,
            gpu_ms: state.gpu_timings().frame_ms,
            shadow_passes: state.shadow_report().shadow_passes,
        };
        self.texture_views.insert(asset_id, state);
        result.map(|()| frame)
    }

    /// Releases texture view `asset_id`: its per-view state, its texture, a pending readback
    /// and the shared draw's per-camera state. A later frame of the same id starts fresh.
    pub fn drop_texture_view(&mut self, asset_id: u64) {
        if let Some(state) = self.texture_views.remove(&asset_id) {
            state.release();
        }
        self.texture_targets.remove(&asset_id);
        if let Some(handle) = self.texture_view_handles.remove(&asset_id) {
            self.shared_textures.release_unbound(&handle);
        }
        // Shared caches (cluster lights, sky, fog, clouds, DDGI) hold groups over the view's
        // own buffers and targets: a new epoch makes every cache drop them on its next use.
        self.cluster_bg.clear();
        self.bind_epoch = crate::bind_cache::new_epoch();
        if let Some(readback) = self.texture_readbacks.remove(&asset_id)
            && readback.state.load(std::sync::atomic::Ordering::Acquire) == crate::readback::ST_READY
        {
            readback.buffer.unmap();
        }
        if let Some(draw) = self.shared_draw.as_mut() {
            draw.drop_view(asset_id);
        }
    }

    /// Asset ids of the texture views that hold their own state ([`Self::render_texture_view`]).
    #[must_use]
    pub fn texture_view_ids(&self) -> Vec<u64> {
        let mut ids: Vec<u64> = self.texture_views.keys().copied().collect();
        ids.sort_unstable();
        ids
    }

    /// GPU pass timings of texture view `asset_id`'s newest timed frame (`None` for an id that
    /// has not rendered through [`Self::render_texture_view`]).
    #[must_use]
    pub fn texture_view_gpu_timings(&self, asset_id: u64) -> Option<GpuTimings> {
        self.texture_views.get(&asset_id).map(|state| state.gpu_timings())
    }

    /// CPU phases of texture view `asset_id`'s last frame.
    #[must_use]
    pub fn texture_view_cpu_timings(&self, asset_id: u64) -> Option<FrameCpuTimings> {
        self.texture_views.get(&asset_id).map(|state| state.cpu_timings())
    }

    /// The texture camera `asset_id` renders into (`None` before its first frame). Bind it
    /// where a material handle does not fit, such as a UI image; the texture is replaced only
    /// when the view's size changes.
    #[must_use]
    pub fn texture_target_view(&self, asset_id: u64) -> Option<wgpu::TextureView> {
        self.texture_targets
            .get(&asset_id)
            .map(|target| target.view.clone())
    }

    /// Starts a non-blocking copy of texture camera `asset_id`'s current image to the CPU.
    ///
    /// The copy is submitted now and mapped asynchronously; nothing waits. Collect the pixels
    /// with [`Self::take_texture_readback`] on a later frame (the frame path's non-blocking
    /// device poll runs the map callback, usually within one or two frames). A request while
    /// one is in flight for the same id keeps the first. The staging buffer is created on the
    /// first request and reused.
    ///
    /// # Errors
    ///
    /// [`RenderError::Readback`] when no texture has been rendered for this id.
    pub fn request_texture_readback(
        &mut self,
        device: &RenderDevice,
        asset_id: u64,
    ) -> Result<(), RenderError> {
        let (width, height, texture) = self
            .texture_targets
            .get(&asset_id)
            .map(|target| (target.width, target.height, target.texture.clone()))
            .ok_or_else(|| RenderError::Readback("no texture target rendered for this id".into()))?;
        if self
            .texture_readbacks
            .get(&asset_id)
            .is_some_and(|readback| readback.pending)
        {
            return Ok(());
        }
        let row = width * 4;
        let padded_row =
            row.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let reuse = self
            .texture_readbacks
            .get(&asset_id)
            .is_some_and(|readback| readback.width == width && readback.height == height);
        if !reuse {
            let buffer = device.tracked().create_buffer(&wgpu::BufferDescriptor {
                label: Some("texture view readback"),
                size: u64::from(padded_row) * u64::from(height),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            self.texture_readbacks.insert(
                asset_id,
                TextureReadback {
                    buffer,
                    width,
                    height,
                    padded_row,
                    state: std::sync::Arc::new(std::sync::atomic::AtomicU8::new(crate::readback::ST_IDLE)),
                    pending: false,
                },
            );
        }
        let Some(readback) = self.texture_readbacks.get_mut(&asset_id) else {
            return Err(RenderError::Readback("texture readback slot missing".into()));
        };
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("texture view readback"),
            });
        encoder.copy_texture_to_buffer(
            texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &readback.buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded_row),
                    rows_per_image: Some(height),
                },
            },
            extent3d(width, height),
        );
        device.tracked_queue().submit([encoder.finish()]);
        let state = std::sync::Arc::clone(&readback.state);
        state.store(crate::readback::ST_INFLIGHT, std::sync::atomic::Ordering::Release);
        crate::frame_stats::note_map_async();
        readback
            .buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                crate::readback::store_map_result(&state, result.is_ok());
            });
        readback.pending = true;
        Ok(())
    }

    /// The pixels of a [`Self::request_texture_readback`] once the copy is mapped (tightly
    /// packed RGBA8, sRGB-encoded): `None` while it is in flight or when nothing was requested.
    /// Never waits; callbacks run on the frame path's non-blocking device poll (or call
    /// [`RenderDevice::poll_nonblocking`] yourself).
    pub fn take_texture_readback(&mut self, asset_id: u64) -> Option<Result<ForwardResult, RenderError>> {
        let readback = self.texture_readbacks.get_mut(&asset_id)?;
        if !readback.pending {
            return None;
        }
        match readback.state.load(std::sync::atomic::Ordering::Acquire) {
            crate::readback::ST_READY => {
                let row = (readback.width * 4) as usize;
                let padded = readback.padded_row as usize;
                let pixels = match readback.buffer.slice(..).get_mapped_range() {
                    Ok(mapped) => {
                        let mut pixels = Vec::with_capacity(row * readback.height as usize);
                        for y in 0..readback.height as usize {
                            let start = y * padded;
                            pixels.extend_from_slice(&mapped[start..start + row]);
                        }
                        Ok(pixels)
                    }
                    Err(err) => Err(RenderError::Readback(err.to_string())),
                };
                readback.buffer.unmap();
                readback
                    .state
                    .store(crate::readback::ST_IDLE, std::sync::atomic::Ordering::Release);
                readback.pending = false;
                Some(pixels.map(|pixels| ForwardResult {
                    width: readback.width,
                    height: readback.height,
                    pixels,
                }))
            }
            crate::readback::ST_FAILED => {
                readback
                    .state
                    .store(crate::readback::ST_IDLE, std::sync::atomic::Ordering::Release);
                readback.pending = false;
                Some(Err(RenderError::Readback("texture readback map failed".into())))
            }
            _ => None,
        }
    }

    /// Reads the pixels of the texture rendered for camera `asset_id`
    /// (tightly packed RGBA8, sRGB-encoded).
    ///
    /// **Blocking, for tools and tests.** It submits a copy and waits for the GPU to finish
    /// everything queued (`poll(Wait)`), tens of milliseconds on a loaded frame. A game frame
    /// uses [`Self::request_texture_readback`] and [`Self::take_texture_readback`], or binds
    /// the image on the GPU ([`crate::TextureViewSettings::texture`]).
    #[tracing::instrument(skip_all)]
    pub fn read_texture_target(
        &mut self,
        device: &RenderDevice,
        asset_id: u64,
    ) -> Result<ForwardResult, RenderError> {
        let (width, height, texture) = self
            .texture_targets
            .get(&asset_id)
            .map(|target| (target.width, target.height, target.texture.clone()))
            .ok_or_else(|| RenderError::Readback("no texture target rendered for this id".into()))?;
        let pixels = read_rgba8(&device.device, &device.queue, &texture, width, height)?;
        // The pixel read already waited. Publish timestamp slots that wait mapped.
        self.collect_timings(device, 0);
        Ok(ForwardResult {
            width,
            height,
            pixels,
        })
    }
}

/// Deterministic-capture overrides for one offscreen frame (goldens, screenshots):
/// everything a complete-frames job turns on for its length ([`FrameRenderer::set_complete_frames`],
/// ADR-2582), applied to a single captured frame and put back after. A captured frame feeds
/// blessed reference images and cross-machine comparisons, so the GPU cull keeps submission
/// order, emissive fits run complete (no wall-clock budget), and the probes' bricks bake before
/// the trace (audit fix-render4 #1, #3, #4).
pub(super) struct CaptureDeterminism {
    ddgi_bake_sync: bool,
    ordered_cull: bool,
    emissive_complete: bool,
}

impl CaptureDeterminism {
    /// Turns the deterministic paths on, returning what to put back.
    pub(super) fn enter(renderer: &mut FrameRenderer) -> Self {
        #[cfg(test)]
        test_enters::inc();
        let saved = Self {
            ddgi_bake_sync: renderer.ddgi.bake_sync,
            ordered_cull: renderer.culler.ordered(),
            emissive_complete: renderer.emissive_cache.complete(),
        };
        renderer.ddgi.bake_sync = true;
        renderer.culler.set_ordered(true);
        renderer.emissive_cache.set_complete(true);
        saved
    }

    /// Puts back what [`Self::enter`] found.
    pub(super) fn exit(self, renderer: &mut FrameRenderer) {
        renderer.ddgi.bake_sync = self.ddgi_bake_sync;
        renderer.culler.set_ordered(self.ordered_cull);
        renderer.emissive_cache.set_complete(self.emissive_complete);
    }
}

/// The deterministic-capture overrides one frame takes (`CaptureDeterminism`), counted per
/// thread: `enter` runs on the frame's calling thread, and lib tests render in parallel on
/// their own threads (a process-wide count would race them).
#[cfg(test)]
mod test_enters {
    use std::cell::Cell;
    std::thread_local! {
        static COUNT: Cell<u32> = const { Cell::new(0) };
    }

    pub(super) fn inc() {
        COUNT.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn get() -> u32 {
        COUNT.with(Cell::get)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The interactive viewport's offscreen draw takes no capture overrides; a capture does.
    /// Fails on the old code, where `begin_offscreen` wrapped every frame (the editor viewport
    /// included) in `CaptureDeterminism`.
    #[test]
    fn the_live_offscreen_path_skips_the_capture_overrides() {
        let device = crate::device::test_device();
        let scene = crate::lab_frame::LabScene {
            width: 48,
            height: 32,
            eye: [0.0, 0.0, 3.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 0.9,
            near: 0.1,
            far: 20.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 5_000.0,
            double_sided: false,
            post: None,
            directional: crate::DirectionalLightUniform::new([0.0, -0.3, -1.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials: vec![crate::lab_frame::LabMaterial::default()],
            meshes: vec![crate::lab_frame::LabMesh::unskinned()],
        };
        let scene = scene.as_frame_scene();
        let mut renderer = FrameRenderer::new();
        let before = test_enters::get();
        renderer
            .begin_offscreen_live(&device, &scene, wgpu::TextureFormat::Rgba8Unorm)
            .expect("live frame");
        assert_eq!(
            test_enters::get(),
            before,
            "the live viewport frame takes no capture overrides"
        );
        renderer
            .begin_offscreen(&device, &scene, wgpu::TextureFormat::Rgba8Unorm)
            .expect("capture frame");
        assert_eq!(
            test_enters::get(),
            before + 1,
            "a capture takes the overrides once"
        );
    }

    /// Audit fix-render4 #3 and #4 (medium): an offscreen capture takes the deterministic
    /// paths — ordered GPU cull, complete emissive fits (no wall-clock budget), sync DDGI
    /// bakes — and only for the capture.
    #[test]
    fn a_capture_forces_deterministic_paths_and_restores_them() {
        let mut renderer = FrameRenderer::new();
        // The order-fixed cull is the default since ADR-4777 (two runs of one submission draw
        // the same image); a capture keeps it on and leaves it on.
        assert!(renderer.culler.ordered());
        let guard = CaptureDeterminism::enter(&mut renderer);
        assert!(renderer.culler.ordered());
        guard.exit(&mut renderer);
        assert!(renderer.culler.ordered());
        // A host that chose the atomicAdd pack (the speed switch) gets the ordered cull for a
        // captured frame only.
        renderer.culler.set_ordered(false);
        assert!(!renderer.culler.ordered());
        assert!(!renderer.emissive_cache.complete());
        assert!(!renderer.ddgi.bake_sync);
        let guard = CaptureDeterminism::enter(&mut renderer);
        assert!(
            renderer.culler.ordered(),
            "the cull keeps submission order in a captured frame"
        );
        assert!(
            renderer.emissive_cache.complete(),
            "emissive fits run complete in a captured frame (no wall-clock budget)"
        );
        assert!(renderer.ddgi.bake_sync);
        guard.exit(&mut renderer);
        assert!(!renderer.culler.ordered());
        assert!(!renderer.emissive_cache.complete());
        assert!(!renderer.ddgi.bake_sync);
        // What the host set is put back, not cleared.
        renderer.culler.set_ordered(true);
        renderer.emissive_cache.set_complete(true);
        let guard = CaptureDeterminism::enter(&mut renderer);
        guard.exit(&mut renderer);
        assert!(renderer.culler.ordered());
        assert!(renderer.emissive_cache.complete());
    }
}
