// SPDX-License-Identifier: MPL-2.0
//! The studio frame's settings and reads (ADR-2000): the environment's base resolution and what
//! was kept of it, the image light's ground plane and its debug views. Set through
//! [`FrameRenderer`]; the agent protocol is `render.set_studio_environment` and
//! `render.get_studio_environment` (`book/agents/studio-environment.md`).

use super::*;
use crate::{
    EnvGroundError, EnvGroundPlane, EnvironmentAuthority, StudioBackground, StudioEnvInfo,
    StudioEnvResolution,
};

/// See the module docs.
#[derive(Default)]
pub(super) struct StudioState {
    /// The base resolution the next bake uses.
    resolution: StudioEnvResolution,
    /// Largest base width; 0 is [`crate::STUDIO_ENV_DEFAULT_BUDGET`].
    budget: u32,
    /// What the installed gradient or equirect environment kept.
    pub(super) installed: Option<StudioEnvInfo>,
    /// The image light's ground plane.
    ground: Option<EnvGroundPlane>,
    /// 0 lit, 1 the image light alone, 2 its occlusion terms.
    image_light_view: u32,
    /// What the sky draw shows.
    background: StudioBackground,
    /// The background image's view while one is set.
    background_image: Option<(wgpu::Texture, wgpu::TextureView)>,
    /// 1×1 stand-ins for the two studio bindings.
    stand_in: Option<StandIn>,
    /// The local reflection captures (`crate::local_capture`).
    pub(super) captures: crate::local_capture::LocalCaptures,
    /// Whether an installed environment or the physical atmosphere draws the sky and the image
    /// light of a frame with both (ADR-2016).
    pub(super) authority: EnvironmentAuthority,
    /// The installed environment's maps hold the atmosphere's bake (written under
    /// [`EnvironmentAuthority::Atmosphere`] before a switch to the studio authority): setting the
    /// same environment again rebuilds it.
    pub(super) maps_mixed: bool,
    /// How frames pick the forward shader's studio axis (ADR-2308, `forward_studio`).
    pub(super) forward_mode: crate::ForwardStudioMode,
}

struct StandIn {
    capture: wgpu::TextureView,
    background: wgpu::TextureView,
}

/// A black `Rgba16Float` texel, as one array layer or a plain 2D view.
fn black_texture(d: &wgpu::Device, q: &crate::device::TrackedQueue, array: bool) -> wgpu::TextureView {
    let texture = d.create_texture(&wgpu::TextureDescriptor {
        label: Some("studio stand-in"),
        size: extent3d(1, 1),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba16Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    q.write_texture(
        texture.as_image_copy(),
        &[0u8; 8],
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(8),
            rows_per_image: Some(1),
        },
        extent3d(1, 1),
    );
    texture.create_view(&wgpu::TextureViewDescriptor {
        dimension: Some(if array {
            wgpu::TextureViewDimension::D2Array
        } else {
            wgpu::TextureViewDimension::D2
        }),
        ..Default::default()
    })
}

/// [`StudioEnvInfo`] of freshly baked maps.
pub(super) fn installed_info(maps: &crate::ibl::IblMaps, source: Option<(u32, u32)>) -> StudioEnvInfo {
    let (base_width, base_height) = maps
        .prefilter
        .first()
        .map_or((0, 0), |mip| (mip.width, mip.height));
    let texels = |w: u32, h: u32| u64::from(w) * u64::from(h);
    let chain: u64 = maps
        .prefilter
        .iter()
        .map(|mip| texels(mip.width, mip.height))
        .sum();
    let irradiance = texels(maps.irradiance.width, maps.irradiance.height)
        + maps
            .lower
            .as_ref()
            .map_or(0, |lower| texels(lower.width, lower.height));
    StudioEnvInfo {
        base_width,
        base_height,
        mips: maps.prefilter.len() as u32,
        source,
        gpu_bytes: (chain + irradiance) * 8,
    }
}

impl StudioState {
    fn budget(&self) -> u32 {
        if self.budget == 0 {
            crate::STUDIO_ENV_DEFAULT_BUDGET
        } else {
            self.budget
        }
    }

    /// The base width of a source `source_width` wide (`None`: analytic) under the settings.
    pub(super) fn base_width(&self, source_width: Option<u32>) -> u32 {
        self.resolution.effective_width(source_width, self.budget())
    }

    /// Creates the stand-in views once; the view group needs them from the first frame.
    pub(super) fn ensure_views(&mut self, d: &wgpu::Device, q: &crate::device::TrackedQueue) {
        if self.stand_in.is_none() {
            self.stand_in = Some(StandIn {
                capture: black_texture(d, q, true),
                background: black_texture(d, q, false),
            });
        }
    }

    pub(super) fn capture_view(&self) -> &wgpu::TextureView {
        match (self.captures.view(), self.stand_in.as_ref()) {
            (Some(view), _) => view,
            (None, Some(stand_in)) => &stand_in.capture,
            (None, None) => unreachable!("the frame creates the stand-ins before the view group"),
        }
    }

    pub(super) fn background_view(&self) -> &wgpu::TextureView {
        match (self.background_image.as_ref(), self.stand_in.as_ref()) {
            (Some((_, view)), _) => view,
            (None, Some(stand_in)) => &stand_in.background,
            (None, None) => unreachable!("the frame creates the stand-ins before the view group"),
        }
    }

    /// The `env_view` uniform. A capture's faces show the lighting environment, never the
    /// background: what a capture reflects is lighting, and the background must not reach it.
    pub(super) fn env_view(&self, capturing: bool) -> [f32; 4] {
        let background = match &self.background {
            _ if capturing => 0.0,
            StudioBackground::Environment => 0.0,
            StudioBackground::Solid(_) => 1.0,
            StudioBackground::Gradient(_) => 2.0,
            StudioBackground::Image(_) => 3.0,
        };
        let captures = if capturing { 0 } else { self.captures.layer_count() };
        [
            self.image_light_view as f32,
            background,
            captures as f32,
            self.captures.top_mip(),
        ]
    }

    /// The `background` uniform.
    pub(super) fn background_uniform(&self) -> [[f32; 4]; 3] {
        let v = |c: [f32; 3], w: f32| [c[0], c[1], c[2], w];
        match &self.background {
            StudioBackground::Solid(color) => [v(*color, 1.0), [0.0; 4], [0.0; 4]],
            StudioBackground::Gradient(g) => [v(g.zenith, g.falloff), v(g.horizon, 0.0), v(g.ground, 0.0)],
            StudioBackground::Environment | StudioBackground::Image(_) => [[0.0; 4]; 3],
        }
    }

    pub(super) fn capture_center(&self) -> [[f32; 4]; 4] {
        self.captures.uniform_centers()
    }

    pub(super) fn capture_half(&self) -> [[f32; 4]; 4] {
        self.captures.uniform_halves()
    }

    /// The studio features on under `ibl` ([`crate::StudioFeatures`], ADR-2308): exactly what
    /// sets the `env_ground_light.w`, `env_view.x` and `env_view.z` uniforms the full forward
    /// shader tests, or more (a capture face zeroes the captures term; this still counts them).
    pub(super) fn features(&self, ibl: Option<&IblGpu>) -> crate::StudioFeatures {
        crate::StudioFeatures {
            ground_plane: self.ground.is_some() && ibl.is_some_and(|ibl| ibl.plane_feed.is_some()),
            captures: u32::try_from(self.captures.layer_count()).unwrap_or(u32::MAX),
            image_light_view: self.image_light_view,
        }
    }

    /// The `env_ground` and `env_ground_light` uniforms of a frame. Zero without a plane, or on
    /// an IBL without the image's lower-hemisphere light.
    pub(super) fn ground_uniforms(
        &self,
        ibl: Option<&IblGpu>,
        sun: &DirectionalLightUniform,
    ) -> ([f32; 4], [f32; 4]) {
        let (Some(plane), Some(feed)) = (self.ground.as_ref(), ibl.and_then(|ibl| ibl.plane_feed)) else {
            return ([0.0; 4], [0.0; 4]);
        };
        let lux = sun.color.map(|c| c * sun.illuminance);
        let [r, g, b] = plane.radiance(feed, lux, -sun.direction[1]);
        (plane.uniform(), [r, g, b, 1.0])
    }
}

impl FrameRenderer {
    /// The lite forward tier of a frame that asks for `asked`. The lite shader has no ground
    /// plane, capture or image-light view term, so a frame that uses one draws the full shader:
    /// the setting's look holds on every tier, at the full shader's cost on a tile GPU.
    #[must_use]
    pub fn frame_forward_tier(
        &self,
        asked: crate::forward_lite::ForwardLite,
    ) -> crate::forward_lite::ForwardLite {
        if self.studio.features(self.ibl.as_ref()).any() {
            return crate::forward_lite::ForwardLite::Off;
        }
        crate::forward_lite::frame_tier(asked, self.forward_lite_override)
    }

    /// Which of an installed studio environment and the physical atmosphere draws the sky and
    /// gives the image light of a frame that has both ([`EnvironmentAuthority`]). The default,
    /// [`EnvironmentAuthority::Atmosphere`], keeps the frames of v0.2.8 and earlier; frames
    /// without the atmosphere or without a studio environment do not depend on it.
    ///
    /// Set it before installing the environment. Under the atmosphere the bake writes into the
    /// installed maps, so after switching to [`EnvironmentAuthority::Studio`] once such frames
    /// were drawn, set the environment again ([`Self::set_studio_gradient`],
    /// [`Self::set_studio_equirect`]; the same parameters rebuild it); until then the frames keep
    /// the mixed maps. `set_uniform_sky` and `set_studio_sky` always rebuild.
    pub fn set_environment_authority(&mut self, authority: EnvironmentAuthority) {
        if self.studio.authority == authority {
            return;
        }
        self.studio.authority = authority;
        self.screen_cache_key = None;
        if authority == EnvironmentAuthority::Studio && self.studio_background && self.ibl_from_atmosphere {
            // The maps hold the bake now. Writing the procedural sky over them (what a frame
            // leaving the atmosphere does) would lose the environment entirely; keep them until
            // the host sets the environment again, which rebuilds it.
            self.ibl_from_atmosphere = false;
            self.outdoor.forget_ibl();
            self.studio.maps_mixed = true;
        }
    }

    /// The authority set with [`Self::set_environment_authority`].
    #[must_use]
    pub fn environment_authority(&self) -> EnvironmentAuthority {
        self.studio.authority
    }

    /// Whether an installed studio environment is this frame's sky and image light: one is
    /// installed and [`EnvironmentAuthority::Studio`] is set.
    pub(super) fn studio_environment_rules(&self) -> bool {
        self.studio_background && self.studio.authority == EnvironmentAuthority::Studio
    }

    /// How wide the next studio environment's base is ([`Self::set_studio_gradient`],
    /// [`Self::set_studio_equirect`]): the mirror mip and the background. The default keeps the
    /// standard 256 × 128 base. An installed environment changes when it is set again.
    pub fn set_studio_env_resolution(&mut self, resolution: StudioEnvResolution) {
        self.studio.resolution = resolution;
    }

    /// The resolution set with [`Self::set_studio_env_resolution`].
    #[must_use]
    pub fn studio_env_resolution(&self) -> StudioEnvResolution {
        self.studio.resolution
    }

    /// The largest base width any [`StudioEnvResolution`] may keep, in texels; 0 returns to
    /// [`crate::STUDIO_ENV_DEFAULT_BUDGET`]. The base is also held to the device's texture limit.
    pub fn set_studio_env_budget(&mut self, max_width: u32) {
        self.studio.budget = max_width;
    }

    /// The budget in force ([`Self::set_studio_env_budget`]).
    #[must_use]
    pub fn studio_env_budget(&self) -> u32 {
        self.studio.budget()
    }

    /// What the renderer kept of the installed gradient or equirect environment; `None` for any
    /// other IBL.
    #[must_use]
    pub fn studio_env_info(&self) -> Option<StudioEnvInfo> {
        self.studio.installed.filter(|_| self.ibl.is_some())
    }

    /// Width, height and mip count of the specular chain the frames read now (`None` before the
    /// first frame on the default IBL): the effective resolution of any IBL.
    #[must_use]
    pub fn environment_resolution(&self) -> Option<(u32, u32, u32)> {
        self.ibl.as_ref().map(|ibl| {
            (
                ibl.prefilter_tex.width(),
                ibl.prefilter_tex.height(),
                ibl.prefilter_tex.mip_level_count(),
            )
        })
    }

    /// The ground plane the image light sees (`env_ground.rs`): the part of the image it
    /// hides gives way to the plane's own radiance, in the diffuse term and in reflections. The
    /// sun and the local lights are not touched and nothing is drawn. Takes effect on studio
    /// environments and the uniform and two-tone skies ([`Self::environment_ground_active`]);
    /// `None` removes it.
    ///
    /// # Errors
    /// [`EnvGroundError`] for a bad value; the plane in use stays.
    pub fn set_environment_ground(&mut self, plane: Option<EnvGroundPlane>) -> Result<(), EnvGroundError> {
        if let Some(plane) = &plane {
            plane.validate()?;
        }
        self.studio.ground = plane;
        self.screen_cache_key = None;
        Ok(())
    }

    /// The plane set with [`Self::set_environment_ground`].
    #[must_use]
    pub fn environment_ground(&self) -> Option<EnvGroundPlane> {
        self.studio.ground
    }

    /// Whether the installed IBL carries what the ground plane needs: the procedural default and
    /// the planet atmosphere's do not, so a plane set on them changes nothing.
    #[must_use]
    pub fn environment_ground_active(&self) -> bool {
        self.studio.ground.is_some() && self.ibl.as_ref().is_some_and(|ibl| ibl.plane_feed.is_some())
    }

    /// The image-light debug view of the following frames: [`crate::DebugViewMode::ImageLight`],
    /// [`crate::DebugViewMode::ImageLightOcclusion`], anything else draws the lit frame.
    pub fn set_image_light_view(&mut self, mode: crate::DebugViewMode) {
        let view = match mode {
            crate::DebugViewMode::ImageLight => 1,
            crate::DebugViewMode::ImageLightOcclusion => 2,
            _ => 0,
        };
        // Called every frame by the runtime: only a change drops the static-screen composite.
        if view != self.studio.image_light_view {
            self.studio.image_light_view = view;
            self.screen_cache_key = None;
        }
    }

    /// What the sky draw shows behind the frame: the lighting environment (the default), or a
    /// solid color, a gradient or a second image. The lighting never reads it, a local capture's
    /// faces and reflections show the environment, and glass refracts it as it does any sky. An
    /// image is kept up to the environment budget ([`Self::set_studio_env_budget`]).
    ///
    /// # Errors
    /// [`crate::StudioEnvError`] for a bad value; the background in use stays.
    pub fn set_studio_background(
        &mut self,
        device: &RenderDevice,
        background: StudioBackground,
    ) -> Result<(), crate::StudioEnvError> {
        background.validate()?;
        let image = if let StudioBackground::Image(image) = &background {
            let limit = device.device.limits().max_texture_dimension_2d;
            let width = StudioEnvResolution::Source
                .effective_width(Some(image.width()), self.studio.budget().min(limit));
            let bytes = image.encoded(width);
            let height = width / 2;
            let q = crate::device::TrackedQueue::observe(&device.queue);
            let texture = device.device.create_texture(&wgpu::TextureDescriptor {
                label: Some("studio background"),
                size: extent3d(width, height),
                mip_level_count: 1,
                sample_count: 1,
                dimension: wgpu::TextureDimension::D2,
                format: wgpu::TextureFormat::Rgba16Float,
                usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
                view_formats: &[],
            });
            let (padded, bytes_per_row) = crate::outdoor::pad_rows_bytes(&bytes, width * 8, height);
            q.write_texture(
                texture.as_image_copy(),
                &padded,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(bytes_per_row),
                    rows_per_image: Some(height),
                },
                extent3d(width, height),
            );
            let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
            Some((texture, view))
        } else {
            None
        };
        // The view groups bind the background image.
        if image.is_some() || self.studio.background_image.is_some() {
            self.ibl_generation += 1;
        }
        self.studio.background_image = image;
        self.studio.background = background;
        self.screen_cache_key = None;
        Ok(())
    }

    /// The background set with [`Self::set_studio_background`].
    #[must_use]
    pub fn studio_background(&self) -> &StudioBackground {
        &self.studio.background
    }

    /// Places a local reflection capture, or moves the one with this id; it reflects nothing
    /// until [`Self::bake_local_capture`]. See `local_capture.rs`.
    ///
    /// # Errors
    /// [`crate::LocalCaptureError`] for a bad value or when every slot is taken.
    pub fn place_local_capture(
        &mut self,
        desc: crate::LocalCaptureDesc,
    ) -> Result<(), crate::LocalCaptureError> {
        self.screen_cache_key = None;
        self.studio.captures.place(desc)
    }

    /// Removes a capture; returns whether it existed. Once the frame's selection runs (the runtime
    /// does each frame, ADR-2860) the others keep their layers and maps. A host that never runs it
    /// has slot `i` in layer `i`, so the other baked captures rebake (their array layers move): it
    /// calls [`Self::bake_pending_local_captures`], which also starts the selection.
    pub fn remove_local_capture(&mut self, id: u64) -> bool {
        self.screen_cache_key = None;
        self.studio.captures.remove(id)
    }

    /// Asks for a bake on the runtime renderer's next window frame ([`crate::render_extracted_into`]
    /// too): of the capture `id`, or of every capture with `None`. Returns how many were queued.
    ///
    /// # Errors
    /// [`crate::LocalCaptureError::Unknown`] for an id that was never placed.
    pub fn request_local_capture_bake(&mut self, id: Option<u64>) -> Result<usize, crate::LocalCaptureError> {
        self.studio.captures.request_bake(id)
    }

    /// The placed captures, in slot order.
    #[must_use]
    pub fn local_captures(&self) -> Vec<crate::LocalCaptureInfo> {
        self.studio.captures.infos()
    }

    /// Bakes the captures queued with [`Self::request_local_capture_bake`] from `scene`, then
    /// runs the frame's capture selection for `scene`'s eye (ADR-2860): the
    /// [`crate::local_capture_budget`] nearest baked boxes, with hysteresis, a swap faded over
    /// [`crate::LOCAL_CAPTURE_FADE_FRAMES`] frames, and a layer write only where a selected
    /// capture's map is not in its layer yet (bakes installed from a `.fcap` included). The
    /// runtime calls this once per window frame; a frame where nothing moved writes nothing.
    ///
    /// # Errors
    /// A frame or its readback failed.
    pub fn bake_pending_local_captures(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
    ) -> Result<usize, RenderError> {
        let pending = self.studio.captures.pending();
        for id in &pending {
            // A failed bake is not asked for again every frame.
            self.studio.captures.clear_pending(*id);
            self.bake_local_capture(device, scene, *id)?;
        }
        self.select_local_captures(device, scene.eye, crate::local_capture_budget(&scene.quality));
        Ok(pending.len())
    }

    /// The capture selection for `eye` under `budget` (ADR-2860), without baking: see
    /// [`Self::bake_pending_local_captures`]. A host that renders with [`Self::render_offscreen`]
    /// calls it once per frame to read more captures than [`crate::LOCAL_CAPTURE_LAYERS`] or to
    /// show bakes placed with [`crate::LocalCaptureDesc::bake`]. Until the first call a capture's
    /// slot is its layer, as before.
    pub fn select_local_captures(&mut self, device: &RenderDevice, eye: [f32; 3], budget: usize) {
        let q = crate::device::TrackedQueue::observe(&device.queue);
        let prepared = self
            .studio
            .captures
            .prepare(&device.device, &q, Some(eye), budget);
        if prepared.created {
            self.ibl_generation += 1;
        }
        if prepared.changed {
            self.screen_cache_key = None;
        }
    }

    /// `(array textures created, selection swaps)` of the local captures (ADR-2860): a frame that
    /// changes nothing moves neither, nor any capture's [`crate::LocalCaptureInfo::uploads`].
    #[must_use]
    pub fn local_capture_counters(&self) -> (u32, u32) {
        (self.studio.captures.textures_created, self.studio.captures.swaps)
    }

    /// Renders the capture `id`'s six HDR faces from `scene` and installs the filtered map: frames
    /// reflect it from the next one on. Six frames on this renderer: it blocks until the GPU has
    /// them, and the host's frame state is kept, as for [`Self::capture_reflection_probe`]. Any
    /// mesh of `scene` shows, so a capture inside a closed single-sided object sees past it (its
    /// back faces are culled).
    ///
    /// # Errors
    /// [`RenderError::Readback`] for an unknown id, or a frame or its readback failed.
    #[tracing::instrument(skip_all)]
    pub fn bake_local_capture(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        id: u64,
    ) -> Result<crate::LocalCaptureInfo, RenderError> {
        let Some(desc) = self.studio.captures.desc(id) else {
            return Err(RenderError::Readback(format!("no local capture {id}")));
        };
        self.probe_captured = true;
        let faces = self.with_probe_state(|this| this.render_capture_faces(device, scene, desc.center))?;
        let (chain, coverage) = crate::local_capture::bake_faces(&faces, crate::LOCAL_CAPTURE_FACE);
        let q = crate::device::TrackedQueue::observe(&device.queue);
        if self
            .studio
            .captures
            .store(&device.device, &q, id, &chain, coverage)
        {
            self.ibl_generation += 1;
        }
        self.screen_cache_key = None;
        self.studio
            .captures
            .infos()
            .into_iter()
            .find(|info| info.desc.id == id)
            .ok_or_else(|| RenderError::Readback(format!("no local capture {id}")))
    }

    fn render_capture_faces(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        center: [f32; 3],
    ) -> Result<[crate::local_capture::Face; 6], RenderError> {
        let size = crate::LOCAL_CAPTURE_FACE;
        let post = crate::PostSettings {
            auto_exposure: false,
            exposure_ev: scene.exposure_ev,
            bloom: false,
            ..crate::PostSettings::default()
        };
        let mut faces = Vec::with_capacity(6);
        for face in 0..6u32 {
            let (dir, up) = crate::reflections::cube_face_axes(face);
            self.camera_up = up;
            let target = [center[0] + dir[0], center[1] + dir[1], center[2] + dir[2]];
            let mut face_scene = probe_face_scene(scene, center, target, size);
            // A post-stack frame keeps the linear image the capture reads back.
            face_scene.post = Some(&post);
            self.request_hdr_capture();
            self.render_offscreen(device, &face_scene)?;
            let hdr = self
                .take_hdr_capture(device)?
                .ok_or_else(|| RenderError::Readback(format!("capture face {face}: no HDR image")))?;
            if hdr.width != size || hdr.height != size {
                return Err(RenderError::Readback(format!(
                    "capture face {face} came back {}x{}, expected {size}x{size}",
                    hdr.width, hdr.height
                )));
            }
            let depth = self.read_scene_depth(device, size, size)?;
            faces.push(crate::local_capture::Face { rgb: hdr.rgb, depth });
        }
        faces
            .try_into()
            .map_err(|_| RenderError::Readback("capture faces".into()))
    }
}
