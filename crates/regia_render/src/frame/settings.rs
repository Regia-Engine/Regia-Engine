// SPDX-License-Identifier: MPL-2.0
//! `FrameRenderer` switches, settings and read-only reports that do not belong to one pass
//! (moved out of `frame.rs` unchanged, ADR-1552).

use super::*;

impl FrameRenderer {
    /// Marks the next frame as a camera cut (`crate::camera_cut`): it starts with no
    /// temporal history, exactly like a fresh renderer's first frame. Dropped are the TAAU/TSR
    /// history and motion, the froxel fog history, the static-screen SSGI/SSR cache, the
    /// cached shadow cascades, pages and cascade anchors, and the auto-exposure meter (the
    /// exposure snaps; [`Self::camera_cut_with`] keeps it adapting). GPU resources stay.
    /// Probe faces and texture views do not take the cut; the next host view does.
    pub fn camera_cut(&mut self) {
        self.camera_cut_with(crate::CutExposure::Snap);
    }

    /// [`Self::camera_cut`] with a choice of what happens to auto exposure.
    pub fn camera_cut_with(&mut self, exposure: crate::CutExposure) {
        self.cut.pending = Some(exposure);
    }

    /// Replaces the automatic camera-cut detection (`None`: only [`Self::camera_cut`] cuts).
    /// On by default with [`crate::CameraCutDetection::default`].
    pub fn set_camera_cut_detection(&mut self, detection: Option<crate::CameraCutDetection>) {
        self.cut.detection = detection;
    }

    /// The automatic camera-cut detection in use.
    #[must_use]
    pub fn camera_cut_detection(&self) -> Option<crate::CameraCutDetection> {
        self.cut.detection
    }

    /// Frame index and reason of the last camera cut, requested or detected.
    #[must_use]
    pub fn last_camera_cut(&self) -> Option<(u64, crate::CameraCutReason)> {
        self.cut.last
    }

    /// Camera cuts since the renderer was built.
    #[must_use]
    pub fn camera_cut_count(&self) -> u64 {
        self.cut.count
    }

    /// Frames rendered since the renderer was built (each `render_into`, probe faces and
    /// texture views included).
    #[must_use]
    pub fn frame_index(&self) -> u64 {
        self.frame_index
    }

    /// Forces the lite forward shader's tier for this renderer (ADR-1874); `None` follows the
    /// profile's [`crate::RenderProfile::forward_lite`] on the adapters
    /// [`crate::forward_lite::adapter_takes_lite`] accepts. The A/B switch of the agent's
    /// `render.set_forward_lite`, and how a test draws the lite path on a software rasterizer.
    pub fn set_forward_lite(&mut self, tier: Option<crate::forward_lite::ForwardLite>) {
        self.forward_lite_override = tier;
    }

    /// The override [`Self::set_forward_lite`] set.
    #[must_use]
    pub fn forward_lite_override(&self) -> Option<crate::forward_lite::ForwardLite> {
        self.forward_lite_override
    }

    /// Dithers the display image before it is stored in 8 bits (ADR-2486): triangular noise of plus or minus
    /// one code value, so a smooth gradient (a clear sky at 4K, fog, a dusk ramp) does not quantize into
    /// visible bands. Off by default (every default frame and golden is unchanged); a float display target
    /// (a deep readout, HDR output) is never dithered. The agent's `render.set_output_dither`.
    pub fn set_output_dither(&mut self, on: bool) {
        self.output_dither = on;
    }

    /// Whether [`Self::set_output_dither`] is on.
    #[must_use]
    pub fn output_dither(&self) -> bool {
        self.output_dither
    }

    /// Keeps the star field at every output height (ADR-2486). Off, the sky sizes its pixel-wide stars from an
    /// `acos` of an `f32` dot product, which breaks down at 3840x2160 (the stars shrink below a pixel and most
    /// vanish), and a star gets a quarter of its 1080p light at 4K. On, the pixel angle comes from the chord in
    /// `f64` and the star brightness scales by `(height / 1080)²`. Off by default, so default frames are unchanged.
    /// The agent's `render.set_star_flux`.
    pub fn set_star_flux(&mut self, on: bool) {
        self.outdoor.set_star_flux(on);
    }

    /// Whether [`Self::set_star_flux`] is on.
    #[must_use]
    pub fn star_flux(&self) -> bool {
        self.outdoor.star_flux()
    }

    /// Whether the outdoor composite's aerial perspective takes the air inside the interior volumes
    /// ([`crate::OutdoorParams::interiors`], the four nearest the eye past four, ADR-7975) out of the sky in-scatter (ADR-4757). On by default:
    /// the air under a roof takes no sky in-scatter, and a ray that crosses no box is unchanged. Off draws the air
    /// as before ADR-4757 everywhere (a look comparison's before plates). The froxel fog and the interior shade
    /// still read the volumes either way. The agent's `render.set_aerial_interiors`.
    pub fn set_aerial_interiors(&mut self, on: bool) {
        self.installed_since.aerial_interiors = true;
        if self.outdoor.aerial_interiors() != on {
            self.outdoor.set_aerial_interiors(on);
            self.screen_cache_key = None;
            self.screen_hold = 0;
            self.taau_ready = false;
            self.taau_pass.invalidate();
            self.tsr_pass.invalidate();
            self.fsr3_pass.invalidate();
        }
    }

    /// Whether [`Self::set_aerial_interiors`] is on.
    #[must_use]
    pub fn aerial_interiors(&self) -> bool {
        self.outdoor.aerial_interiors()
    }

    /// Caps how far the sun cascades reach, metres (ADR-2486); `None` keeps the tier's reach (the view's shadow
    /// range, or [`crate::forward_lite::LOW_SUN_RANGE_M`] on the lite Low path). On the lite forward path,
    /// shadows fade out over the last 15 % of that range. On the desktop full-forward shader the fade to
    /// unshadowed light is the last 15 % of the last cascade's own interval (from the previous split to the
    /// range); a one-cascade desktop frame starts that interval at 0, so the fade is 15 % of the range. On a
    /// one-cascade tier (Mobile Low) off the lite path, the single 1024² cascade spans
    /// the whole shadow range, metres per texel, and a forest's tree shadows vanish; a cap of 60-150 m brings
    /// them back near the camera. A non-finite or non-positive value is `None`; others clamp to
    /// [`Self::SUN_SHADOW_RANGE_MIN_M`]..[`Self::SUN_SHADOW_RANGE_MAX_M`]. Off by default (default frames
    /// unchanged). The agent's `render.set_sun_shadow_range`.
    pub fn set_sun_shadow_range(&mut self, range_m: Option<f32>) {
        self.sun_shadow_range = range_m
            .filter(|r| r.is_finite() && *r > 0.0)
            .map(|r| r.clamp(Self::SUN_SHADOW_RANGE_MIN_M, Self::SUN_SHADOW_RANGE_MAX_M));
    }

    /// The cap [`Self::set_sun_shadow_range`] set, after clamping.
    #[must_use]
    pub fn sun_shadow_range(&self) -> Option<f32> {
        self.sun_shadow_range
    }

    /// Shortest sun shadow range [`Self::set_sun_shadow_range`] accepts, metres.
    pub const SUN_SHADOW_RANGE_MIN_M: f32 = 8.0;
    /// Longest sun shadow range [`Self::set_sun_shadow_range`] accepts, metres (the cascades' own cap).
    pub const SUN_SHADOW_RANGE_MAX_M: f32 = 4000.0;

    /// Selects the temporal upscaler for TAA frames (the render profile's
    /// [`crate::RenderProfile::temporal_upscaler`]). Switching drops the new
    /// upscaler's history.
    pub fn set_temporal_upscaler(&mut self, upscaler: crate::TemporalUpscaler) {
        if self.temporal_upscaler != upscaler {
            self.temporal_upscaler = upscaler;
            self.tsr_pass.invalidate();
            self.fsr3_pass.invalidate();
            self.taau_ready = false;
        }
    }

    /// The temporal upscaler TAA frames use.
    #[must_use]
    pub fn temporal_upscaler(&self) -> crate::TemporalUpscaler {
        self.temporal_upscaler
    }

    /// Selects a vendor upscaler (ADR-2002) for single-sample TAA frames, tried before
    /// [`Self::temporal_upscaler`]; `None` turns it off. Whenever it cannot run (not built, its
    /// runtime missing, a device without NGX's extensions, a render size outside its range, a
    /// failure) the frame runs the built-in upscaler and [`Self::vendor_upscaler_report`] says
    /// why. A change drops the vendor's feature; the next frame creates a new one.
    pub fn set_vendor_upscaler(&mut self, vendor: Option<crate::upscaler::VendorUpscaler>) {
        self.vendor_upscaler.select(vendor);
    }

    /// What the vendor upscaler did on the last frame (`render.get_vendor_upscaler`).
    #[must_use]
    pub fn vendor_upscaler_report(&self) -> crate::upscaler::vendor::dlss::VendorFrameReport {
        self.vendor_upscaler.report()
    }

    /// Tuning for the TSR upscaler ([`crate::TsrSettings::for_scale`] by default).
    pub fn set_tsr_settings(&mut self, settings: crate::TsrSettings) {
        self.tsr_pass.set_settings(settings);
    }

    /// The TSR tuning in use.
    #[must_use]
    pub fn tsr_settings(&self) -> crate::TsrSettings {
        self.tsr_pass.settings()
    }

    /// Material and terrain texture mip bias the last frame used: `log2(render / output)`
    /// plus [`crate::TsrSettings::texture_bias_offset`] while TSR upscales, else 0.
    #[must_use]
    pub fn texture_lod_bias(&self) -> f32 {
        self.texture_lod_bias
    }

    /// Mips the material sampler adds to [`Self::texture_lod_bias`] on this renderer's device: the
    /// anisotropic-blend calibration (ADR-5670, [`crate::material_anisotropy_lod_offset`]). 0 before
    /// the first frame and on adapters whose anisotropic filter is never sharper than exact.
    #[must_use]
    pub fn material_anisotropy_lod_offset(&self) -> f32 {
        self.pipes.as_ref().map_or(0.0, |p| p.material_lod_offset)
    }

    /// The material samples' mip bias: [`Self::texture_lod_bias`] plus
    /// [`Self::material_anisotropy_lod_offset`].
    pub(super) fn material_texture_bias(&self) -> f32 {
        self.texture_lod_bias + self.material_anisotropy_lod_offset()
    }

    /// Tuning for the FSR 3.1 upscaler ([`crate::Fsr3Settings::default`] is the SDK's).
    pub fn set_fsr3_settings(&mut self, settings: crate::Fsr3Settings) {
        self.fsr3_pass.set_settings(settings);
    }

    /// The FSR 3.1 tuning in use.
    #[must_use]
    pub fn fsr3_settings(&self) -> crate::Fsr3Settings {
        self.fsr3_pass.settings()
    }

    /// Asks the next [`Self::render_into`] to copy its linear HDR scene colour — lighting,
    /// emission, sky and aerial perspective in physical units, before exposure, bloom and the
    /// tonemap — for [`Self::take_hdr_capture`]. Only frames that run the post stack have a
    /// linear HDR target. Opt-in and copy-only: the frame's pixels do not change. This is the
    /// real-time side of the reference comparison (`specs/27` § 2).
    pub fn request_hdr_capture(&mut self) {
        self.hdr_capture_requested = true;
    }

    /// Makes the post stack's final blit — or the SGSR upscale, when it replaces the blit —
    /// write linear light in paper-white units, expanding the tonemapped highlights up to
    /// `headroom` (`peak / paper white`, [`crate::calibration::hdr_highlight_expand`]) — the
    /// input of the HDR display stage. `None` (the default) writes the display-encoded image
    /// as before.
    pub fn set_display_headroom(&mut self, headroom: Option<f32>) {
        self.display_headroom = headroom.map(|h| h.max(1.0));
    }

    /// Waits for and decodes the HDR copy recorded by the last frame (see
    /// [`Self::request_hdr_capture`]). `None` when no copy was recorded (no post stack, or an
    /// HDR format the copy does not decode).
    ///
    /// # Errors
    ///
    /// Mapping the readback buffer failed.
    pub fn take_hdr_capture(&mut self, device: &RenderDevice) -> Result<Option<HdrCapture>, RenderError> {
        let Some(((buffer, width, height, padded, format), pre_exposure)) = self.hdr_capture.take() else {
            return Ok(None);
        };
        let (tx, rx) = std::sync::mpsc::channel();
        buffer.slice(..).map_async(wgpu::MapMode::Read, move |r| {
            let _ = tx.send(r);
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
        let mut rgb = Vec::with_capacity((width * height) as usize);
        for y in 0..height as usize {
            let row = &mapped[y * padded as usize..];
            for x in 0..width as usize {
                let c = decode_hdr_texel(row, x, format);
                rgb.push([c[0] / pre_exposure, c[1] / pre_exposure, c[2] / pre_exposure]);
            }
        }
        drop(mapped);
        buffer.unmap();
        Ok(Some(HdrCapture { width, height, rgb }))
    }

    /// Records what every following [`Self::render_into`] draws ([`crate::FrameExport`]: mesh
    /// instances with their materials and this frame's joint palettes, the camera, the sun
    /// after a time-of-day override, the sky body's fill and the outdoor parameters), for
    /// the reference path tracer. Mesh and material data are copied once per key and shared
    /// between exports. Reflection-probe faces are not recorded. `false` stops recording and
    /// frees the copies.
    pub fn set_scene_export(&mut self, enabled: bool) {
        match (enabled, self.scene_export.is_some()) {
            (true, false) => self.scene_export = Some(Default::default()),
            (false, true) => self.scene_export = None,
            _ => {}
        }
    }

    /// The last frame's export, when [`Self::set_scene_export`] is on and a frame was drawn.
    #[must_use]
    pub fn scene_export(&self) -> Option<&crate::FrameExport> {
        self.scene_export.as_ref().and_then(|e| e.last.as_ref())
    }

    /// The surface the installed shared draw renders (the terrain heightfield), from
    /// [`crate::SharedPassDraw::reference_heightfield`]. Built on request; `None` without a
    /// shared draw or when the draw does not describe one.
    #[must_use]
    pub fn reference_heightfield(&self) -> Option<crate::ReferenceHeightfield> {
        self.shared_draw.as_ref()?.reference_heightfield()
    }

    /// Installs translucent effect layers (VFX particles/decals/trails), replacing any
    /// previous set. Kept for the renderer's lifetime; drawn once per rendered frame (see
    /// [`crate::translucent`]).
    pub fn set_translucent_layers(
        &mut self,
        layers: Vec<std::sync::Arc<dyn crate::translucent::TranslucentLayer>>,
    ) {
        self.translucent = layers;
    }

    /// The installed translucent layers, in draw order.
    #[must_use]
    pub fn translucent_layers(&self) -> &[std::sync::Arc<dyn crate::translucent::TranslucentLayer>] {
        &self.translucent
    }

    /// Inserts one translucent layer at `index` of the draw order (past the end appends),
    /// keeping the others, so independent crates (effects, water) share the seam (ADR-1399).
    pub fn insert_translucent_layer(
        &mut self,
        index: usize,
        layer: std::sync::Arc<dyn crate::translucent::TranslucentLayer>,
    ) {
        let index = index.min(self.translucent.len());
        self.translucent.insert(index, layer);
    }

    /// Finishes the frame's command buffers on `threads` worker threads while the rest
    /// of the frame is recorded (default 2, capped at the CPU count minus one;
    /// `REGIA_ENCODE_THREADS` overrides the default). The frame is cut into a few
    /// command buffers at pass boundaries and still submits once, in recording order, so
    /// the image is unchanged. `0` finishes on the calling thread: one encoder on desktop,
    /// bounded pass segments on Android to limit pooled command-buffer growth.
    pub fn set_encode_threads(&mut self, threads: usize) {
        self.finish_workers.set_threads(threads);
    }

    /// Worker threads that finish command buffers ([`Self::set_encode_threads`]).
    #[must_use]
    pub fn encode_threads(&self) -> usize {
        self.finish_workers.threads()
    }

    /// Test hook (cargo feature `test-hooks`): `hook` runs first on each encode worker thread
    /// ([`Self::set_encode_threads`]) started after this call. The workers start on the first
    /// frame that cuts its encoder, so set it before the first frame.
    ///
    /// The steady-frame allocation tests count the frame's allocations on the drawing thread and
    /// on these workers with it: a segment no worker has started yet is finished on the drawing
    /// thread instead, so a count of the drawing thread alone moves by a segment's allocations
    /// whenever a worker runs late (ADR-2299).
    #[cfg(feature = "test-hooks")]
    pub fn set_encode_worker_start_hook(&mut self, hook: fn()) {
        self.finish_workers.set_start_hook(hook);
    }

    /// The pipelining switches the last frame with sun pages applied ([`crate::FramePipelining`],
    /// ADR-2074).
    #[must_use]
    pub fn frame_pipelining(&self) -> crate::FramePipelining {
        crate::FramePipelining {
            parallel_shadow_plan: self.sun_pages.parallel(),
        }
    }

    /// The last frame's sun page update ran on compute-pool tasks: the switch was on, with enough
    /// casters and more than one cascade.
    #[must_use]
    pub fn shadow_plan_on_tasks(&self) -> bool {
        self.sun_pages.ran_on_tasks()
    }

    /// Turns the render-submission trims (ADR-508) on or off from the next frame on. On (the
    /// default; `REGIA_SUBMIT_TRIMS=0` starts with them off), a mesh pass records a vertex or
    /// index buffer bind only when the buffer changes, and the frame skips translucent-tail
    /// work nothing reads. Off records the frame as before, for an A/B measurement. The image
    /// is the same either way.
    pub fn set_submission_trims(&mut self, enabled: bool) {
        self.submission.set_enabled(enabled);
    }

    /// Whether the render-submission trims are on ([`Self::set_submission_trims`]).
    #[must_use]
    pub fn submission_trims(&self) -> bool {
        self.submission.current()
    }

    /// Binds recorded and skipped, passes culled and decal receiver stamps built by the last
    /// main-view [`Self::render_into`] (texture views are not counted).
    #[must_use]
    pub fn submission_stats(&self) -> crate::SubmissionStats {
        self.submission.last()
    }

    /// Sets the material clock: graphs that read time get `seconds` in their parameter slot 0
    /// (`x`) and the previous call's value in `y` (motion vectors of animated vertex offsets).
    /// `None` (the default) leaves the slot as each material carries it. When a clock or wind
    /// is set, the forward, shadow and velocity shaders read one frame uniform instead, and the
    /// per-material block is not rewritten for slot 0 alone (ADR-5160).
    pub fn set_material_time(&mut self, seconds: Option<f32>) {
        self.material_time = seconds.map(|now| {
            let prev = self.material_time.map_or(now, |(last, _)| last);
            (now, prev)
        });
    }

    /// Sets the material clock to `now` seconds, with `prev` as the previous frame's value
    /// (motion vectors of animated vertex offsets). [`crate::material_clock()`] derives both
    /// from gameplay time, wrapped so they stay exact in `f32` in a long session (ADR-1238).
    /// Unlike [`Self::set_material_time`], `prev` does not depend on earlier calls. A set clock
    /// is one frame uniform (ADR-5160); slot 0 of each material is not uploaded every frame.
    pub fn set_material_clock(&mut self, now: f32, prev: f32) {
        self.material_time = Some((now, prev));
    }

    /// The material clock `(now, prev)` the next frame writes into time-reading graphs, if
    /// one is set.
    #[must_use]
    pub fn material_clock(&self) -> Option<(f32, f32)> {
        self.material_time
    }

    /// Sets the scene wind, metres per second on world X and Z, that time-reading graphs read
    /// from parameter slot 0 `zw` beside the material clock (`specs/88`, ADR-2028;
    /// [`crate::FoliageSway`] bends along it). Non-finite components count as zero and a vector
    /// longer than [`crate::FOLIAGE_WIND_MAX_M_PER_S`] is shortened to it. `None` (the default)
    /// leaves the slot as each material carries it.
    pub fn set_foliage_wind(&mut self, wind_m_per_s: Option<[f32; 2]>) {
        self.foliage_wind = wind_m_per_s.map(crate::foliage_wind::sanitize_wind);
    }

    /// The scene wind the next frame writes ([`Self::set_foliage_wind`]), if one is set.
    #[must_use]
    pub fn foliage_wind(&self) -> Option<[f32; 2]> {
        self.foliage_wind
    }

    /// Chooses how shadow casters with a time-reading vertex stage (wind) are cached
    /// ([`crate::DynamicCasterShadows`], ADR-2028). The default,
    /// [`crate::DynamicCasterShadows::Tier`], lets the frame quality decide.
    pub fn set_dynamic_caster_shadows(&mut self, mode: crate::foliage_wind::DynamicCasterShadows) {
        self.dynamic_casters = mode;
    }

    /// The dynamic-caster mode ([`Self::set_dynamic_caster_shadows`]).
    #[must_use]
    pub fn dynamic_caster_shadows(&self) -> crate::foliage_wind::DynamicCasterShadows {
        self.dynamic_casters
    }

    /// What the dynamic-caster class did on the last frame: the resolved policy, the animated
    /// casters and the pages and tiles they made the frame redraw.
    #[must_use]
    pub fn dynamic_caster_report(&self) -> crate::foliage_wind::DynamicCasterReport {
        self.shadow_report.dynamic
    }

    /// Problems that broke the last frame [`Self::render_into`] drew (empty when it drew
    /// as asked): e.g. an MSAA frame with a shared terrain draw, which skips the terrain and
    /// the planet sky ([`crate::host_checklist::PROBLEM_MSAA_SKIPS_TERRAIN`]). The agent
    /// protocol reports them (`render.problems`) and [`Self::host_checklist_report`] lists
    /// them as errors.
    #[must_use]
    pub fn frame_problems(&self) -> &[crate::host_checklist::FrameProblem] {
        &self.frame_problems
    }

    /// Whether a shared draw (native terrain) is registered ([`Self::set_shared_draw`]).
    #[must_use]
    pub fn has_shared_draw(&self) -> bool {
        self.shared_draw.is_some()
    }

    /// Sizes the per-instance buffers (GPU cull, instanced shadow/depth objects, motion
    /// vectors, decal receivers) for at least `max_instances` draws, so a frame that reaches
    /// that many never reallocates them and rebuilds their bind groups (`specs/27` § 2.4). Cheap:
    /// the buffers are sized when next allocated, about 0.3 KiB per instance. The next frame also
    /// makes the staging belt's room for that many instances' uploads, at most 32 MiB.
    pub fn reserve_instances(&mut self, max_instances: u32) {
        self.installed_since.reserved = self.installed_since.reserved.max(max_instances);
        self.culler.reserve(max_instances);
        self.taau_pass.reserve(max_instances);
        self.decal_receiver.reserve(max_instances as usize);
    }

    /// Pins mesh and material keys ([`FrameMesh::mesh_key`], [`FrameMesh::material_key`]) so the
    /// cache sweep never evicts them, whatever their age or the budget. Uploads nothing and
    /// builds no pipeline: a key not yet resident uploads when a frame first draws it (or
    /// [`Self::prewarm`] does) and then stays. Cheap enough to call with a match's whole unit
    /// set; key `0` is ignored.
    pub fn pin_keys(&mut self, keys: impl IntoIterator<Item = u64>) {
        self.pinned.extend(keys.into_iter().filter(|key| *key != 0));
    }

    /// Releases keys [`Self::pin_keys`] or [`Self::prewarm`] pinned: the sweep may evict them
    /// again ([`CacheResidency`]).
    pub fn unpin_keys(&mut self, keys: impl IntoIterator<Item = u64>) {
        for key in keys {
            self.pinned.remove(&key);
        }
    }

    /// Releases every pinned key.
    pub fn clear_pinned_keys(&mut self) {
        self.pinned.clear();
    }

    /// Sets when idle meshes and materials are evicted ([`CacheResidency`]).
    pub fn set_cache_residency(&mut self, residency: CacheResidency) {
        self.residency = residency;
    }

    /// When idle meshes and materials are evicted.
    #[must_use]
    pub fn cache_residency(&self) -> CacheResidency {
        self.residency
    }

    /// Scales the texture streaming budget with the adapter's VRAM (on by default): a discrete
    /// card keeps at least a quarter of its device-local memory in streamed textures
    /// ([`crate::TextureStreaming::vram_scaled_budget_mib`]), so a 16 GB card at Desktop High
    /// holds 4 GiB instead of the preset's 2 GiB. The derived material cache budget follows
    /// ([`CacheResidency::budget_bytes`]). Off: the frame quality's budget as is.
    pub fn set_vram_scaled_budgets(&mut self, enabled: bool) {
        self.vram_budgets_off = !enabled;
    }

    /// Whether the texture streaming budget scales with VRAM ([`Self::set_vram_scaled_budgets`]).
    #[must_use]
    pub fn vram_scaled_budgets(&self) -> bool {
        !self.vram_budgets_off
    }

    /// The mesh and material caches' bytes against their budgets as of the last sweep: under
    /// the derived budgets ([`CacheResidency::budget_bytes`] `0`) each cache has its own, so
    /// material textures over theirs never evict meshes.
    #[must_use]
    pub fn cache_budgets(&self) -> CacheBudgets {
        let mut budgets = self.cache_budgets;
        if self.residency.budget_bytes != 0 {
            budgets.split = false;
            budgets.mesh_budget_bytes = self.residency.budget_bytes;
            budgets.material_budget_bytes = 0;
        } else {
            let (mesh, material) = self.derived_budgets();
            budgets.split = true;
            budgets.mesh_budget_bytes = mesh;
            budgets.material_budget_bytes = material;
        }
        budgets
    }

    /// Mesh and material cache totals: entries, pins, uploads and evictions so far, and the
    /// resident bytes against the budget as of the last sweep.
    #[must_use]
    pub fn cache_stats(&self) -> CacheStats {
        CacheStats {
            meshes: self.meshes.len(),
            materials: self.materials.len(),
            pinned: self.pinned.len(),
            budget_bytes: self.cache_budget_bytes(),
            ..self.cache_stats
        }
    }

    /// Occupancy of the mesh cache's vertex and index arenas: `(vertices, indices)`. Cached
    /// meshes are ranges in shared chunk buffers; an eviction frees a range and a later mesh
    /// reuses it without creating a buffer. Meshes over half a chunk keep their own buffers
    /// and are not counted here.
    #[must_use]
    pub fn mesh_arena_stats(&self) -> (crate::ArenaStats, crate::ArenaStats) {
        self.mesh_arenas
            .as_ref()
            .map(|arenas| (arenas.vertices.stats(), arenas.indices.stats()))
            .unwrap_or_default()
    }

    /// Folds the joint of a one-joint rigid skin into its model matrix (on by default): a mesh
    /// whose palette has one joint and whose every vertex is bound to it with weights summing to
    /// one draws as a rigid mesh — instanced, frustum-culled, visibility buffer — instead of one
    /// by one with a CPU uniform. Canopies, fans and doors skinned to a single bone take this
    /// path. The decal receiver layer stays the character layer, and GI proxies and emissive
    /// light fitting still treat the mesh as skinned. Off keeps the skinned per-object path.
    pub fn set_rigid_skin_fold(&mut self, enabled: bool) {
        self.rigid_skin_fold_off = !enabled;
    }

    /// Rebuild the atmosphere tables and sky IBL for a moved sun or camera height on a
    /// worker thread instead of inside the frame (`specs/27` § 2.4; ~50 ms of CPU per
    /// rebuild). Off by default so lab sequences and goldens integrate every change inline;
    /// [`crate::RuntimeRenderer`] turns it on. The first tables are always built inline.
    pub fn set_async_sky(&mut self, enabled: bool) {
        self.outdoor.set_async_sky(enabled);
    }

    /// A sky rebuild ([`Self::set_async_sky`]) is running on its worker.
    #[must_use]
    pub fn sky_rebuilding(&self) -> bool {
        self.outdoor.sky_busy()
    }

    /// Waits for a sky rebuild started by [`Self::set_async_sky`] and installs it (a capture
    /// that needs the settled sky; never call on the frame path).
    pub fn settle_sky(&mut self) {
        self.outdoor.settle_sky();
    }

    /// The outdoor parameters last set with [`Self::set_outdoor`].
    #[must_use]
    pub fn outdoor_params(&self) -> &crate::OutdoorParams {
        self.outdoor.params()
    }

    /// Frames, since this renderer was made, whose aerial perspective read the cloud
    /// transmittance map: the air under the cloud layer took the layer's shadow (ADR-5944). It
    /// counts exactly the frames whose surfaces read the map for their sun term
    /// ([`crate::CloudSettings::shadow_map`] on, clouds drawn, a planet with air); a frame with
    /// the map off never counts and draws its air as before.
    #[must_use]
    pub fn cloud_air_frames(&self) -> u64 {
        self.outdoor.cloud_air_frames()
    }

    /// Installs a draw that records into this frame's depth, cascades, HDR colour
    /// and motion vectors. `None` removes it. The renderer calls
    /// [`crate::SharedPassDraw::prepare`] before the command encoder and
    /// [`crate::SharedPassDraw::draw`] inside those passes, including the decal
    /// receiver. Single-sample frames only; an MSAA frame skips the draw.
    pub fn set_shared_draw(&mut self, draw: Option<Box<dyn crate::SharedPassDraw + Send + Sync>>) {
        self.shared_draw = draw;
        self.shared_draw_set = true;
        self.shared_shadow_keys = [None; 4];
        self.ddgi.hf_epoch = self.ddgi.hf_epoch.wrapping_add(1);
    }

    /// Hands the installed shared draw (the terrain, its grass and impostors) and the translucent
    /// layers (the water) the outdoors the next view sees from inside a cell
    /// ([`crate::viscells::OutdoorPortals`], plan row W-22, ADR-8413, ADR-8843): they leave out of
    /// the camera passes what lies in none of the list's frusta. `None`, an unrestricted list, or
    /// one made for another eye than the frame's draws as without cells. Kept until the next call;
    /// the window paths call it before each view with
    /// [`crate::ExtractedFrame::outdoor_portals_for`].
    pub fn set_outdoor_portals(&mut self, portals: Option<&crate::viscells::OutdoorPortals>) {
        if let Some(draw) = self.shared_draw.as_mut() {
            draw.set_outdoor_portals(portals);
        }
        for layer in &self.translucent {
            layer.set_outdoor_portals(portals);
        }
    }

    /// What the installed shared draw culled through the outdoor frusta on its last main-view
    /// prepare ([`crate::SharedPassDraw::outdoor_cull_counts`]); `None` without a shared draw or
    /// for a draw that does not cull.
    #[must_use]
    pub fn outdoor_cull_counts(&self) -> Option<crate::viscells::OutdoorCullCounts> {
        self.shared_draw.as_ref()?.outdoor_cull_counts()
    }

    /// Takes the installed shared draw off this renderer (the frame then draws without it, as after
    /// `set_shared_draw(None)`) and returns it, so a caller can put the same draw back later
    /// without rebuilding it: a parked terrain (spec 37 § 13, ADR-7200). `None` when none is set.
    pub fn take_shared_draw(&mut self) -> Option<Box<dyn crate::SharedPassDraw + Send + Sync>> {
        let draw = self.shared_draw.take()?;
        self.set_shared_draw(None);
        Some(draw)
    }

    /// Cascade passes that drew the shared geometry (terrain) since this
    /// renderer was built. A held camera with a retained cascade adds nothing.
    #[must_use]
    pub fn shared_shadow_draws(&self) -> u64 {
        self.shared_shadow_draws
    }

    /// Cascade passes that redrew the shared geometry only inside the pages under moved
    /// mesh casters ([`crate::SharedPassDraw::draw_shadow_region`]), since this renderer
    /// was built. Not counted in [`Self::shared_shadow_draws`].
    #[must_use]
    pub fn shared_shadow_region_draws(&self) -> u64 {
        self.shared_shadow_region_draws
    }

    /// Buffer writes and bytes the shared draw queued through
    /// [`crate::SharedUpload`] on the last frame.
    #[must_use]
    pub fn shared_upload_stats(&self) -> (u32, u64) {
        self.shared_upload_last
    }

    /// Interior geometry (the [`crate::EnclosureScene`] shell) the renderer traces to light
    /// an interior by daylight.
    ///
    /// When the eye is in an interior (`FrameQuality::interior_ambient`), the profile
    /// uses probe GI and the game set no field of its own, the renderer bakes
    /// [`crate::build_daylit_interior`] for the interior volume that contains the eye
    /// (or a 12 × 6 × 12 m box around the eye, kept while the eye stays inside that
    /// box): the transmitted sun, the planet's sky through the openings and three
    /// bounces, cached until the sun, the volume or this scene changes. Leaving the
    /// interior drops the field.
    pub fn set_interior_scene(&mut self, scene: Option<crate::SdfScene>) {
        use std::hash::{Hash, Hasher};
        self.installed_since.interior = true;
        let keyed = scene.map(|scene| {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            format!("{scene:?}").hash(&mut hasher);
            (hasher.finish(), scene)
        });
        if keyed.as_ref().map(|(k, _)| *k) != self.interior_scene.as_ref().map(|(k, _)| *k) {
            self.interior_scene = keyed;
            self.gi_fallback = None;
            if self.gi_auto.take().is_some() {
                self.gi_field = None;
            }
        }
    }

    /// [`Self::set_interior_scene`] from a borrowed scene: copies and rehashes it only when it
    /// differs from the installed one, so the window frame can hand the same shell every frame
    /// without a per-frame copy of it.
    pub(crate) fn sync_interior_scene(&mut self, scene: Option<&crate::SdfScene>) {
        if self.interior_scene.as_ref().map(|(_, installed)| installed) == scene {
            self.installed_since.interior = true;
            return;
        }
        self.set_interior_scene(scene.cloned());
    }

    /// The interior shell [`Self::set_interior_scene`] installed last, or `None`.
    #[must_use]
    pub fn interior_scene(&self) -> Option<&crate::SdfScene> {
        self.interior_scene.as_ref().map(|(_, scene)| scene)
    }

    /// Replaces the per-vertex scalar overlays ([`crate::ScalarOverlay`], Tincal F10), keyed by
    /// [`FrameMesh::object_key`]. Pass [`crate::ExtractedScalarOverlays::entries`] every frame; an
    /// empty iterator clears them (the next frame is the plain lit one). The overlay draws after
    /// the post stack, unlit over the lit frame, in [`Self::render_into`] only (texture views and
    /// probe captures never draw it). Values upload once per distinct `Arc`: re-submitting the
    /// same overlays every frame is free.
    pub fn set_scalar_overlays(&mut self, overlays: impl IntoIterator<Item = (u64, crate::ScalarOverlay)>) {
        self.scalar_overlays.clear();
        self.scalar_overlays.extend(overlays);
    }

    /// Number of overlays set ([`Self::set_scalar_overlays`]).
    #[must_use]
    pub fn scalar_overlay_count(&self) -> usize {
        self.scalar_overlays.len()
    }

    /// What the scalar overlay pass did in the last frame, and its upload totals.
    #[must_use]
    pub fn scalar_overlay_stats(&self) -> crate::ScalarOverlayStats {
        self.scalar_pass.stats()
    }

    /// Sets the gizmo lines [`Self::render_into`] draws after the scene (and the post stack and
    /// translucent layers), before any overlay. `None` (the default) draws none. Texture-target
    /// cameras ([`Self::render_into_texture`]) and probe captures never draw gizmos.
    pub fn set_gizmos(&mut self, gizmos: Option<std::sync::Arc<crate::gizmos::GizmoFrame>>) {
        self.gizmos = gizmos.filter(|frame| !frame.is_empty());
    }

    /// Replaces the image-based light with a uniform environment of `radiance` (cd/m², linear
    /// RGB) from every direction — a reference studio's flat world colour — for lab frames that
    /// run IBL without an outdoor sky (`FrameScene::ibl`). Radiance above
    /// [`crate::SKY_IBL_SCALE`] clips. `None` returns to the procedural sky.
    pub fn set_uniform_sky(&mut self, device: &RenderDevice, radiance: Option<[f32; 3]>) {
        self.studio_background = radiance.is_some();
        self.studio_env_key = None;
        self.studio.installed = None;
        self.studio.maps_mixed = false;
        self.forget_atmosphere_ibl();
        self.ibl = radiance.map(|r| {
            let k = 1.0 / crate::SKY_IBL_SCALE;
            let maps = crate::ibl::build_uniform_ibl([r[0] * k, r[1] * k, r[2] * k]);
            let q = crate::device::TrackedQueue::observe(&device.queue);
            build_ibl(&device.device, &q, &maps)
        });
        self.ibl_generation += 1;
    }

    /// Uses a reference studio's environment as IBL — a uniform sky radiance above the horizon
    /// and a uniform ground radiance below it, cd/m² per channel (the ground stands in for the
    /// first bounce off a large diffuse plane) — or, with `None`, the default IBL again. The
    /// sky draw shows the same environment as the background. Like
    /// [`Self::set_uniform_sky`], for frames without the physical atmosphere.
    pub fn set_studio_sky(&mut self, device: &RenderDevice, radiance: Option<([f32; 3], [f32; 3])>) {
        self.studio_background = radiance.is_some();
        self.studio_env_key = None;
        self.studio.installed = None;
        self.studio.maps_mixed = false;
        self.forget_atmosphere_ibl();
        self.ibl = radiance.map(|(sky, ground)| {
            let k = 1.0 / crate::SKY_IBL_SCALE;
            let maps = crate::ibl::build_studio_ibl(sky.map(|v| v * k), ground.map(|v| v * k));
            let q = crate::device::TrackedQueue::observe(&device.queue);
            build_ibl(&device.device, &q, &maps)
        });
        self.ibl_generation += 1;
    }

    /// Uses a horizon-to-zenith gradient with a ground colour as the studio environment: both
    /// the diffuse irradiance and the specular prefilter come from it, and the sky draw shows it
    /// as the background (no sun disc). Radiances are cd/m² as in [`Self::set_studio_sky`]; a
    /// channel above [`crate::SKY_IBL_SCALE`] is kept, not clipped. `None` returns to the default
    /// IBL. The maps are rebuilt only when the parameters differ from those of the environment
    /// already installed: returns `Ok(true)` when it rebuilt, `Ok(false)` when it kept them.
    ///
    /// # Errors
    /// [`crate::StudioEnvError`] for a non-finite or negative radiance or a falloff out of range;
    /// the installed environment is left as it was.
    pub fn set_studio_gradient(
        &mut self,
        device: &RenderDevice,
        gradient: Option<&crate::StudioGradient>,
    ) -> Result<bool, crate::StudioEnvError> {
        let Some(gradient) = gradient else {
            self.clear_studio_environment();
            return Ok(false);
        };
        gradient.validate()?;
        let base = self.studio.base_width(None);
        Ok(
            self.install_studio_environment(device, gradient.cache_key(), base, None, |width| {
                gradient.bake(width)
            }),
        )
    }

    /// Uses an HDR equirect image as the studio environment (diffuse irradiance and specular
    /// prefilter from the image, the same background as [`Self::set_studio_gradient`]); `None`
    /// returns to the default IBL. Rebuilt only when the image (size and texels) differs from
    /// the installed one: `Ok(true)` when it rebuilt. The image is validated when it is built
    /// ([`crate::StudioEquirect::new`]).
    pub fn set_studio_equirect(
        &mut self,
        device: &RenderDevice,
        image: Option<&crate::StudioEquirect>,
    ) -> bool {
        let Some(image) = image else {
            self.clear_studio_environment();
            return false;
        };
        let base = self.studio.base_width(Some(image.width()));
        let source = Some((image.width(), image.height()));
        self.install_studio_environment(device, image.cache_key(), base, source, |width| image.bake(width))
    }

    /// The cache key of the gradient or equirect studio environment installed now
    /// ([`crate::StudioGradient::cache_key`], [`crate::StudioEquirect::cache_key`]); `None`
    /// when the IBL is anything else.
    #[must_use]
    pub fn studio_environment_key(&self) -> Option<u64> {
        self.studio_env_key.filter(|_| self.ibl.is_some())
    }

    /// The maps are about to be replaced: nothing of the atmosphere's bake is in them any more, and
    /// the next frame must neither restore the procedural sky over them nor keep a stale bake key.
    fn forget_atmosphere_ibl(&mut self) {
        self.ibl_from_atmosphere = false;
        self.outdoor.forget_ibl();
        // A still frame must not replay the composite of the old environment.
        self.screen_cache_key = None;
    }

    fn clear_studio_environment(&mut self) {
        self.studio_background = false;
        self.studio_env_key = None;
        self.studio.installed = None;
        self.studio.maps_mixed = false;
        self.forget_atmosphere_ibl();
        self.ibl = None;
        self.ibl_generation += 1;
    }

    fn install_studio_environment(
        &mut self,
        device: &RenderDevice,
        key: u64,
        base_width: u32,
        source: Option<(u32, u32)>,
        bake: impl FnOnce(u32) -> crate::ibl::IblMaps,
    ) -> bool {
        let base_width = base_width.min(device.device.limits().max_texture_dimension_2d);
        // Maps the atmosphere wrote into before a switch to the studio authority rebuild
        // (`set_environment_authority`).
        if self.studio_env_key == Some(key)
            && self.ibl.is_some()
            && !self.studio.maps_mixed
            && self
                .studio
                .installed
                .is_some_and(|info| info.base_width == base_width)
        {
            return false;
        }
        let _span = tracing::info_span!("studio_environment_rebuild", base_width).entered();
        let maps = bake(base_width);
        let q = crate::device::TrackedQueue::observe(&device.queue);
        self.ibl = Some(build_ibl(&device.device, &q, &maps));
        self.ibl_generation += 1;
        self.forget_atmosphere_ibl();
        self.studio.maps_mixed = false;
        self.studio_background = true;
        self.studio_env_key = Some(key);
        self.studio.installed = Some(studio_state::installed_info(&maps, source));
        true
    }

    /// Replaces the atmosphere, fog and cloud inputs for subsequent frames.
    pub fn set_outdoor(&mut self, params: crate::OutdoorParams) {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        format!("{params:?}").hash(&mut hasher);
        self.outdoor_key = hasher.finish();
        // The scene wind reaches the foliage (spec 99 § 3.3): while a `SceneWind` exists it wins
        // over a direct `set_foliage_wind`.
        if let Some(wind) = params.scene_wind {
            self.set_foliage_wind(wind);
        }
        self.foliage_wind_from_scene = params.scene_wind.is_some();
        self.set_surface_weather(params.surface_weather);
        self.set_precipitation_extinction(params.precip_extinction_per_m);
        self.outdoor.set_params(params);
    }

    /// Sets the valley mist ([`crate::ValleyMist`]) for subsequent frames; `None` removes it.
    pub fn set_valley_mist(&mut self, mist: Option<crate::ValleyMist>) {
        self.outdoor.set_valley_mist(mist);
    }

    /// Replaces the large meshes' occlusion and bounce proxies for subsequent frames
    /// ([`crate::mesh_occluders`]): each box's sun shadow darkens the ground a mesh's IBL
    /// sees, and the box replaces the sky or ground it hides (its bounce onto the terrain and
    /// meshes beside it) and shadows the sky body's fill. Applies only on frames lit by the
    /// planet atmosphere's IBL; an empty list leaves every frame unchanged. The nearest
    /// [`crate::MAX_MESH_OCCLUDERS`] to the eye are used.
    pub fn set_mesh_occluders(&mut self, occluders: Vec<crate::MeshOccluder>) {
        self.mesh_occluders = occluders;
    }

    /// The proxies set with [`Self::set_mesh_occluders`].
    #[must_use]
    pub fn mesh_occluders(&self) -> &[crate::MeshOccluder] {
        &self.mesh_occluders
    }

    /// How many proxies the last frame shaded with (0 without the planet IBL).
    #[must_use]
    pub fn mesh_occluders_in_use(&self) -> usize {
        self.mesh_occluder_block.count()
    }

    /// How the last frame split the proxies: full quality, far-band contact blobs
    /// ([`crate::MeshOccluder::far_band`], ADR-7970) and dropped (all 0 without the planet IBL).
    #[must_use]
    pub fn mesh_occluder_bands(&self) -> crate::OccluderBands {
        self.mesh_occluder_block.bands()
    }

    /// Per fog-light slot ([`crate::OutdoorParams::lights`] order): whether the last frame's froxel
    /// fog shadowed that light from its spot tile in the local shadow atlas (ADR-1365). False for
    /// point lights, spots that cast no shadow or got no tile, and frames without froxel fog.
    #[must_use]
    pub fn fog_light_shadowed(&self) -> [bool; 4] {
        self.fog_light_shadowed
    }

    /// The IES profiles of the scene's point and spot lights and its authored rect and tube lights for
    /// the next frames (`light_profiles`, ADR-2592); the default changes nothing.
    pub fn set_light_extras(&mut self, extras: crate::light_profiles::LightExtras) {
        self.light_extras = extras;
    }

    /// The light extras [`Self::set_light_extras`] installed last (the default when none was).
    #[must_use]
    pub fn light_extras(&self) -> &crate::light_profiles::LightExtras {
        &self.light_extras
    }

    /// Replaces the per-instance glass state (own-canopy mode, crack pages) for subsequent
    /// frames. Instances are matched to draws by [`FrameMesh::object_key`]; an empty list draws
    /// every mesh exactly as without the glass feature.
    pub fn set_glass(&mut self, instances: Vec<crate::glass::GlassInstance>) {
        use std::hash::{Hash, Hasher};
        self.glass_key = if instances.is_empty() {
            0
        } else {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            for instance in &instances {
                instance.object_key.hash(&mut hasher);
                instance.inside.hash(&mut hasher);
                instance.specular_scale.to_bits().hash(&mut hasher);
                if let Some(damage) = &instance.damage {
                    damage.mask.version.hash(&mut hasher);
                    for col in damage.mask_from_world {
                        for v in col {
                            v.to_bits().hash(&mut hasher);
                        }
                    }
                }
            }
            hasher.finish() | 1
        };
        self.glass = instances;
    }

    /// Overrides how this renderer shades viewer-inside glass (own canopy), or `None` to follow
    /// the process-wide [`crate::glass::set_canopy_glass`]. [`crate::glass::CANOPY_FULL_ENV`]
    /// beats both.
    pub fn set_canopy_glass(&mut self, settings: Option<crate::glass::CanopyGlass>) {
        self.canopy_glass = settings;
    }

    /// The viewer-inside glass setting this renderer draws with (the environment override,
    /// this renderer's own, or the process-wide one).
    #[must_use]
    pub fn canopy_glass(&self) -> crate::glass::CanopyGlass {
        crate::glass::resolve_canopy(self.canopy_glass)
    }

    /// Whether per-pass GPU timing is on ([`Self::set_gpu_timing`] on a device with
    /// `TIMESTAMP_QUERY`).
    #[must_use]
    pub fn gpu_timing_enabled(&self) -> bool {
        self.gpu_timer.is_some()
    }

    /// Turns the GPU-driven cull's Hi-Z occlusion test on (the default) or off; frustum culling stays.
    ///
    /// The test reads a pyramid built at the start of the frame from the size's shared depth target, which
    /// holds whatever was last rendered at that size. On an RTX 5090 that input made two identical 120-frame
    /// render-outs with temporal anti-aliasing differ by a few edge pixels from a few dozen frames on, while
    /// with the test off they matched bit for bit (ADR-1402). Off draws every instance in the frustum: the
    /// filming setting when two renders of one sequence must be identical.
    pub fn set_hiz_occlusion(&mut self, enabled: bool) {
        self.hiz_occlusion_off = !enabled;
    }

    /// Whether the GPU-driven cull tests Hi-Z occlusion ([`Self::set_hiz_occlusion`]).
    #[must_use]
    pub fn hiz_occlusion(&self) -> bool {
        !self.hiz_occlusion_off
    }

    /// Order-fixed compaction of the GPU-driven instanced colour batches (on by default).
    ///
    /// On, every instance keeps the slot of its submission order (a culled one draws nothing)
    /// and a batch's instance count is its highest visible slot plus one: the GPU-driven path
    /// draws the same image run to run, at the vertex cost of the culled instances below each
    /// batch's last visible one. Off, the GPU cull packs each `(mesh, material)` batch's
    /// visible instances with an atomic counter, so the order of instances inside a batch
    /// follows the order GPU threads finish in, which differs from run to run; where two
    /// instances meet at one depth the later one wins, and temporal AA carries the pixel into
    /// later frames, so two render-outs of one sequence drift apart (ADR-1402) — the explicit
    /// speed switch for a frame that does not need repeatability.
    /// [`Self::set_complete_frames`] keeps the ordered pack on for its length and puts back
    /// what it found (ADR-2582). Combine with [`Self::set_hiz_occlusion`]`(false)` for
    /// repeatable render-outs (ADR-1415).
    pub fn set_ordered_gpu_cull(&mut self, on: bool) {
        self.culler.set_ordered(on);
    }

    /// Whether the order-fixed GPU cull is on ([`Self::set_ordered_gpu_cull`]).
    #[must_use]
    pub fn ordered_gpu_cull(&self) -> bool {
        self.culler.ordered()
    }

    /// Hashed alpha test for masked materials on temporal-AA frames (off by default).
    ///
    /// Masked materials (alpha-tested foliage cards, fences) take the hashed threshold of
    /// Wyman & McGuire 2017 where their alpha texture is minified — today on TSR frames only.
    /// On, every frame the temporal AA (TAAU) resolves takes it too, so a distant twig card's
    /// coverage averages over frames instead of switching all-or-nothing at the cutoff
    /// (shimmer). The threshold follows the frame's temporal noise index (frames since the last
    /// camera cut), so it is the same in every run. Frames without temporal AA keep the hard
    /// cutoff.
    pub fn set_taa_hashed_alpha(&mut self, on: bool) {
        self.taa_hashed_alpha = on;
    }

    /// Whether masked materials take the hashed alpha test on TAA frames
    /// ([`Self::set_taa_hashed_alpha`]).
    #[must_use]
    pub fn taa_hashed_alpha(&self) -> bool {
        self.taa_hashed_alpha
    }

    /// Alpha-tested shadows for masked materials (off by default).
    ///
    /// Off, a masked caster (`ALPHA_MASK`) writes its whole triangles into the shadow maps, so a
    /// twig card casts a solid rectangle. On, the GPU-driven instanced shadow passes batch masked
    /// casters by material and draw them with an alpha-tested depth pipeline: base-colour
    /// texture alpha × material alpha × vertex alpha against the material's cutoff (a hard
    /// cutoff, no hash, so the shadow holds still frame to frame). Hair keeps its translucency
    /// shadow path. The CPU (non-instanced) shadow path draws masked casters opaque either way.
    pub fn set_alpha_tested_shadows(&mut self, on: bool) {
        if self.alpha_tested_shadows != on {
            self.alpha_tested_shadows = on;
            // Stored cascades, pages and tiles hold the other kind of caster.
            self.csm_valid = false;
            self.atlas_valid = false;
            let cascades = self.csm.as_ref().map_or(1, |(_, count, _)| *count);
            self.sun_pages =
                crate::paged_shadow::SunPageCache::new(cascades, crate::paged_shadow::SUN_PAGES_PER_AXIS);
            self.atlas_tiles = crate::paged_shadow::AtlasTileCache::new();
        }
    }

    /// Whether masked casters are alpha-tested in the shadow passes
    /// ([`Self::set_alpha_tested_shadows`]).
    #[must_use]
    pub fn alpha_tested_shadows(&self) -> bool {
        self.alpha_tested_shadows
    }

    /// Largest foliage shadow softness ([`Self::set_foliage_shadow_softness`]).
    pub const MAX_FOLIAGE_SHADOW_SOFTNESS: f32 = 0.5;

    /// Soft leaf shadows: the width of the band around the material's alpha cutoff over which an
    /// alpha-tested caster's coverage rises from nothing to whole (`0`, the default, keeps the
    /// hard cutoff; clamped to [`Self::MAX_FOLIAGE_SHADOW_SOFTNESS`]; non-finite reads as `0`).
    ///
    /// Only matters with [`Self::set_alpha_tested_shadows`] on. The partial coverage is written as
    /// a fixed per-texel pattern (the same every frame and every run), and the shadow filter
    /// averages it into a penumbra on the leaf edges. Quantized to hundredths. Changing it
    /// rebuilds the alpha-tested shadow pipelines and the stored shadow maps.
    pub fn set_foliage_shadow_softness(&mut self, softness: f32) {
        let softness = if softness.is_finite() {
            softness.clamp(0.0, Self::MAX_FOLIAGE_SHADOW_SOFTNESS)
        } else {
            0.0
        };
        let hundredths = (softness * 100.0).round() as u32;
        if hundredths != self.culler.foliage_shadow_softness() {
            self.culler.set_foliage_shadow_softness(hundredths);
            self.csm_valid = false;
            self.atlas_valid = false;
            let cascades = self.csm.as_ref().map_or(1, |(_, count, _)| *count);
            self.sun_pages =
                crate::paged_shadow::SunPageCache::new(cascades, crate::paged_shadow::SUN_PAGES_PER_AXIS);
            self.atlas_tiles = crate::paged_shadow::AtlasTileCache::new();
        }
    }

    /// The foliage shadow softness ([`Self::set_foliage_shadow_softness`]), alpha.
    #[must_use]
    pub fn foliage_shadow_softness(&self) -> f32 {
        self.culler.foliage_shadow_softness() as f32 / 100.0
    }

    /// Makes auto exposure deterministic: the post stack waits for each exposure histogram readback on the
    /// frame it is due (two frames after it was recorded) instead of using it on whichever later frame its
    /// buffer finishes mapping. Without it, two identical render-outs meter the same histograms on different
    /// frames whenever the GPU or the CPU runs at another speed (ADR-1402). The wait stalls the CPU until the
    /// GPU has finished the frame two frames back, which an offline render does anyway.
    pub fn set_wait_for_exposure_readback(&mut self, wait: bool) {
        self.wait_exposure_readback = wait;
        if let Some(post) = self.post.as_mut() {
            post.set_wait_readbacks(wait);
        }
    }

    /// Whether auto exposure waits for its histogram readbacks
    /// ([`Self::set_wait_for_exposure_readback`]).
    #[must_use]
    pub fn waits_for_exposure_readback(&self) -> bool {
        self.wait_exposure_readback
    }

    /// Complete frames for an offline capture (render-out, ADR-2582). The runtime renderer moves
    /// work off the frame on purpose: material-graph pipelines and sky rebuilds build on worker
    /// threads (their draws use the standard surface or the last sky until they land), emissive
    /// light fits run on a worker inside a 1 ms frame budget, texture streaming uploads within a
    /// time budget at the measured upload rate and fades new mips in, the stochastic SSR pass goes
    /// dormant on a counter readback, and the GPU cull appends instances in whatever order its
    /// threads run. Each of those lands on a frame that depends on the CPU's and the GPU's speed,
    /// so two identical offline jobs, or a job and the frames before it, could show different
    /// frames. With the switch on, every frame is complete: graphs and the sky build inline (what
    /// was queued is waited for now), fits run on the frame thread (work already queued is waited
    /// for), streaming uploads every set it wants in that frame with no fade, the SSR pass never
    /// sleeps, the probes' mesh bricks bake on the frame thread, the cull keeps submission
    /// order ([`Self::set_ordered_gpu_cull`]), and auto exposure waits for each histogram
    /// readback on the frame it is due ([`Self::set_wait_for_exposure_readback`]). Frames cost
    /// what that work costs. Turning it off puts back what it changed, so a graph, sky or cull
    /// setting the host changes while the switch is on does not last past it. The switch follows
    /// the renderer through a game-frame warm: set on the stand-in, it holds on the renderer that
    /// comes back (ADR-2728).
    pub fn set_complete_frames(&mut self, on: bool) {
        match (on, self.complete_frames.take()) {
            (true, None) => {
                let saved = CompleteSaved {
                    async_graphs: self.graph_cache.is_async(),
                    async_sky: self.outdoor.async_sky(),
                    ordered_cull: self.culler.ordered(),
                    exposure_hold: self.exposure_zones.holds_on_missing_dt(),
                    ddgi_bake_sync: self.ddgi.bake_sync,
                    wait_exposure_readback: self.wait_exposure_readback,
                };
                self.graph_cache.set_async(false);
                self.graph_cache.settle();
                self.outdoor.set_async_sky(false);
                self.outdoor.settle_sky();
                self.culler.set_ordered(true);
                self.exposure_zones.set_hold_on_missing_dt(true);
                self.emissive_cache.set_complete(true);
                self.highend.set_never_sleep(true);
                // The probes' bricks bake on the frame thread and pending ones are waited
                // for, so no frame of the job traces a brick set the task pool's timing
                // chose (audit fix-render4 #1).
                self.ddgi.bake_sync = true;
                self.set_wait_for_exposure_readback(true);
                self.complete_frames = Some(saved);
            }
            (true, Some(saved)) => self.complete_frames = Some(saved),
            (false, Some(saved)) => {
                self.graph_cache.set_async(saved.async_graphs);
                self.outdoor.set_async_sky(saved.async_sky);
                self.culler.set_ordered(saved.ordered_cull);
                self.exposure_zones.set_hold_on_missing_dt(saved.exposure_hold);
                self.emissive_cache.set_complete(false);
                self.highend.set_never_sleep(false);
                self.ddgi.bake_sync = saved.ddgi_bake_sync;
                self.set_wait_for_exposure_readback(saved.wait_exposure_readback);
            }
            (false, None) => {}
        }
    }

    /// Whether frames are complete ([`Self::set_complete_frames`]).
    #[must_use]
    pub fn complete_frames(&self) -> bool {
        self.complete_frames.is_some()
    }

    /// Bytes of the reflection targets allocated now: the "ssr color" output plus the
    /// stochastic SSR set. They are allocated by the first reflective frame and freed after
    /// 60 frames in a row that want no reflection pass (a scene with no glossy material at
    /// all). A dormant stochastic SSR pass (glossy materials, none in view) keeps them
    /// (ADR-168) until [`Self::trim_memory`].
    #[must_use]
    pub fn reflection_target_bytes(&self) -> u64 {
        self.highend.reflection_target_bytes()
    }

    /// Bytes of the stochastic SSR targets alone (0 on tiers without the pass: Desktop Low
    /// and Medium, mobile).
    #[must_use]
    pub fn sssr_target_bytes(&self) -> u64 {
        self.highend.sssr_target_bytes()
    }

    /// Whether the stochastic SSR pass is dormant: its readbacks saw no glossy pixel for a
    /// while, so only its quarter-resolution probe runs.
    #[must_use]
    pub fn sssr_dormant(&self) -> bool {
        self.highend.sssr_dormant()
    }

    /// Frees GPU memory that is allocated but idle, and returns the bytes freed now: the
    /// stochastic SSR targets and the reflection output while the pass is dormant (≈ 185 MiB at
    /// 2560×1440). The end of the next frame also evicts every unpinned mesh and material not
    /// drawn for ~0.5 s, whatever the cache budget ([`CacheResidency`]; counted in
    /// [`CacheStats::meshes_evicted`] and [`CacheStats::materials_evicted`], not in the return
    /// value), and every compute skinning twin not drawn on the frame before it
    /// ([`Self::skinning_gpu_bytes`]). Anything freed is allocated again when next needed,
    /// mid-play, so call this only under real memory pressure. [`signal_memory_pressure`] (the OS low-memory event, the VRAM
    /// probe) makes every renderer call it at its next frame.
    pub fn trim_memory(&mut self) -> u64 {
        self.cache_trim_pending = true;
        self.skin_compute.request_trim();
        let freed = self.highend.trim_memory();
        if freed > 0 {
            tracing::info!(target: "regia_render", "memory pressure: freed {freed} bytes of idle reflection targets");
        }
        freed
    }

    /// Ray and tile counts of a recent stochastic SSR frame (read back a few frames late;
    /// `None` before the first).
    #[must_use]
    pub fn sssr_stats(&self) -> Option<crate::SssrStats> {
        self.highend.sssr_stats()
    }

    /// Latest GPU pass timings (zeros when timing is off).
    ///
    /// [`Self::render_into`] never waits on the timestamp query, so these values
    /// are at least two frames old on that path. [`Self::read_offscreen`] and
    /// [`Self::read_texture_target`] already wait for pixels, and they publish
    /// the timings of the frame they just read.
    #[must_use]
    pub fn gpu_timings(&self) -> GpuTimings {
        self.last_timings
    }

    /// The auto-exposure meter's smoothed scene luminance (cd/m²) from the post chain,
    /// `None` before the first histogram readback or without auto exposure. With
    /// [`crate::ev100_from_luminance`] and the view's clamp and compensation this is the
    /// EV the frame was exposed at (diagnostics, parity reports).
    #[must_use]
    pub fn metered_luminance(&self) -> Option<f32> {
        self.post
            .as_ref()
            .and_then(crate::post_pass::PostChain::metered_luminance)
    }

    /// The auto-exposure meter's smoothed bright-subject coverage (fraction of the metered
    /// pixels at or above `ExposureMeter::bright_threshold`; 0 with it off or before the
    /// first histogram readback).
    #[must_use]
    pub fn metered_bright_coverage(&self) -> f32 {
        self.post
            .as_ref()
            .map_or(0.0, crate::post_pass::PostChain::metered_bright_coverage)
    }

    /// Sets the main view's exposure-zone target; `None` clears it (ADR-322).
    ///
    /// With a target, auto exposure clamps the newest histogram's EV into the target range
    /// (shifted by the exposure compensation) and the eye walks toward it with
    /// [`crate::adapt_ev`] at the target's speeds, instead of the smoothed meter. With `None`,
    /// once an eye released by its zone has handed back, every frame is exposed exactly as
    /// without zones. Manual exposure (auto exposure off, or a camera `exposure_ev`) ignores
    /// zones. Texture views are never zoned. `regia_framework::exposure_zones` calls this
    /// every frame from the zones at the active camera; see
    /// [`crate::exposure_zones::ZoneExposure`].
    pub fn set_exposure_target(&mut self, target: Option<crate::EvTarget>) {
        self.exposure_zones.set_target(target);
    }

    /// The host's step, in seconds, for the next main-view frame's zone adaptation; `None`
    /// (or a frame without a call) measures the wall clock — except under
    /// [`Self::set_complete_frames`], where an omitted step holds the eye at dt 0 so a capture
    /// reads no wall clock. See
    /// [`crate::exposure_zones::ZoneExposure::set_frame_dt`].
    pub fn set_exposure_dt(&mut self, seconds: Option<f32>) {
        self.exposure_zones.set_frame_dt(seconds);
    }

    /// The main view's zone-driven eye: the target, the last frame's
    /// [`crate::exposure_zones::ExposureReport`] and the opt-in exposure log.
    #[must_use]
    pub fn exposure_zones(&self) -> &crate::exposure_zones::ZoneExposure {
        &self.exposure_zones
    }

    /// Mutable access to [`Self::exposure_zones`] (start or stop the exposure log).
    pub fn exposure_zones_mut(&mut self) -> &mut crate::exposure_zones::ZoneExposure {
        &mut self.exposure_zones
    }

    /// CPU phase times of the last `render_into`.
    #[must_use]
    pub fn cpu_timings(&self) -> FrameCpuTimings {
        self.last_cpu
    }

    /// Moves out the last submission queued by [`Self::render_into`].
    ///
    /// [`wgpu::SubmissionIndex`] is not `Clone`. A bench waits on this index so
    /// two or three frames stay in flight. The second call returns `None`.
    #[must_use]
    pub fn take_submission(&mut self) -> Option<wgpu::SubmissionIndex> {
        self.last_submission.take()
    }

    /// Pages and atlas tiles the last frame decided to redraw.
    #[must_use]
    pub fn shadow_cache_report(&self) -> &ShadowCacheReport {
        &self.shadow_report
    }

    /// CPU time of the last frame's shadow work, by part ([`ShadowCpuTimings`]).
    #[must_use]
    pub fn shadow_cpu_timings(&self) -> ShadowCpuTimings {
        self.shadow_cpu
    }

    /// Screen-space passes (AO, SSGI gather and composite, SSR) the last frame recorded.
    #[must_use]
    pub fn screen_pass_report(&self) -> ScreenPassReport {
        self.screen_report
    }

    /// How many local lights the last frame uploaded, and which cap applied.
    #[must_use]
    pub fn light_upload(&self) -> crate::LightUploadReport {
        self.light_upload
    }

    /// Area lights the last frame fitted to emissive meshes and uploaded
    /// ([`crate::FrameQuality::emissive_lights`]).
    #[must_use]
    pub fn emissive_lights(&self) -> crate::emissive_lights::EmissiveLightReport {
        self.emissive_report
    }

    /// Cumulative counters of the emissive fit cache (v0.1.18): fits done and sent to the
    /// background worker, content-cache and probe hits, budget holds, stand-ins, and the
    /// frame-thread and worker milliseconds. Take two snapshots and
    /// [`crate::emissive_lights::EmissiveFitStats::since`] for a window.
    #[must_use]
    pub fn emissive_fit_stats(&self) -> crate::emissive_lights::EmissiveFitStats {
        self.emissive_cache.stats()
    }

    /// Lends this frame's emissive meshes to the emissive fit cache: a mesh is pinned under
    /// the generation id [`crate::runtime::mesh_key`] gives its `Arc`, and the cache trusts
    /// the content hash it memoized for a key while the mesh's weak lives — the weaks pin the
    /// allocation and `Arc::get_mut` refuses while any weak lives, so the public buffers
    /// cannot change in safe code — with no per-frame check.
    ///
    /// The trust engages only when the submitted [`crate::FrameMesh`]'s `mesh_key` is exactly
    /// `mesh_key(&arc)` — the key of a non-chained draw (LOD 0 without `draw_indices`). A host
    /// building [`FrameScene`]s itself must key its lent meshes the same way; keying by
    /// `Arc::as_ptr` or another scheme leaves the pins unmatched, which is safe but keeps the
    /// per-frame 32-sample fingerprint. The runtime draws a `MeshLodChain` under
    /// `lod_variant_key` with the level's index buffer, so chained LOD draws (LOD ≥ 1 always,
    /// LOD 0 with `draw_indices`) also keep the fingerprint fallback.
    ///
    /// The runtime calls this with [`crate::ExtractedFrame::emissive_meshes`]; a host building
    /// [`FrameScene`]s itself may skip it (the cache then checks each key's buffers and a
    /// sampled fingerprint every frame).
    pub fn hold_mesh_identities<'a>(
        &mut self,
        meshes: impl IntoIterator<Item = &'a std::sync::Arc<crate::CpuMesh>>,
    ) {
        self.emissive_cache.hold(meshes);
    }

    /// The temporal upscaler's state (velocity readback in `taau_tests.rs`).
    #[cfg(test)]
    pub(crate) fn taau_pass_for_tests(&self) -> &crate::taau::TaauPass {
        &self.taau_pass
    }

    /// Replaces the per-instance render overrides (tint, ghost, x-ray, unlit; Tincal F5), keyed
    /// by [`FrameMesh::object_key`]. Materials are untouched: the override rides the instance's
    /// object data (`crate::render_override`). Pass
    /// [`crate::ExtractedRenderOverrides::keys`] every frame; an empty iterator clears them.
    /// Identity overrides are dropped.
    pub fn set_render_overrides(
        &mut self,
        overrides: impl IntoIterator<Item = (u64, crate::RenderOverride)>,
    ) {
        self.render_overrides.clear();
        for (key, value) in overrides {
            let packed = value.pack();
            if packed != [0, 0] {
                self.render_overrides.insert(key, packed);
            }
        }
    }

    /// Number of instances drawing with a render override ([`Self::set_render_overrides`]).
    #[must_use]
    pub fn render_override_count(&self) -> usize {
        self.render_overrides.len()
    }

    /// Replaces the material shading models (skin, eye, fabric sheen; Tincal B3, B4, B6), keyed
    /// by [`FrameMesh::material_key`] ([`crate::shading_model`]). Pass
    /// [`crate::ExtractedMaterialShading::entries`] every frame; an empty iterator clears them.
    /// Standard entries are dropped. The next frame rewrites only the uniforms that change.
    pub fn set_material_shading(&mut self, shading: impl IntoIterator<Item = (u64, crate::MaterialShading)>) {
        self.material_shading.clear();
        for (key, value) in shading {
            if !value.is_standard() {
                self.material_shading.insert(key, value.pack());
            }
        }
    }

    /// Number of materials drawing with a non-standard shading model
    /// ([`Self::set_material_shading`]).
    #[must_use]
    pub fn material_shading_count(&self) -> usize {
        self.material_shading.len()
    }

    /// The material sampler's anisotropy clamp.
    pub(super) fn material_anisotropy(&self) -> u16 {
        self.material_anisotropy.unwrap_or(MATERIAL_ANISOTROPY)
    }

    /// Overrides the material sampler's anisotropy clamp; must be called before the first frame
    /// (the sampler is built with the renderer's pipelines). For tests that compare paths on a
    /// driver whose anisotropic filter ignores explicit gradients (see `visbuffer::tests`).
    #[cfg(test)]
    pub(crate) fn set_material_anisotropy(&mut self, clamp: u16) {
        debug_assert!(self.pipes.is_none(), "set before the first frame");
        self.material_anisotropy = Some(clamp.max(1));
    }

    /// How many meshes the last frame drew vs frustum-culled (`specs/13` § 5 step 3).
    #[must_use]
    pub fn draw_stats(&self) -> DrawStats {
        self.last_draw_stats
    }

    /// Draws, pipeline and bind group switches and material batches per pass of the last
    /// main-view frame (bindless step 1, ADR-316; `render.get_frame_stats`). All zero before the
    /// first frame.
    #[must_use]
    pub fn draw_counts(&self) -> crate::frame_stats::FrameDrawCounts {
        self.draw_counts
    }

    /// The material constants storage buffer (`None` on the uniform path, or before the first
    /// standard material): live slots, capacity, stride and how many buffers it created.
    #[must_use]
    pub fn material_arena_stats(&self) -> Option<crate::bindless::MaterialArenaStats> {
        self.material_arena
            .as_ref()
            .map(crate::bindless::MaterialArena::stats)
    }

    /// What the material texture table holds, how often it was rebuilt, and how many shared
    /// material record groups were built (`texture_table`, ADR-863). `None` off the table path,
    /// or before the first standard material.
    #[must_use]
    pub fn texture_table_stats(&self) -> Option<crate::texture_table::TextureTableStats> {
        let record_groups = self.record_group.built;
        self.texture_table
            .as_ref()
            .map(|table| crate::texture_table::TextureTableStats {
                record_groups,
                ..table.stats()
            })
    }

    /// Growths of the frame's reusable light, shadow-caster, culling and object lists (ADR-385):
    /// flat once the scene and the view settle (telemetry's `frame_scratch`).
    #[must_use]
    pub fn frame_scratch_stats(&self) -> FrameScratchStats {
        self.scratch.stats(self.culler.scratch_growths())
    }

    /// Shadow casters in the last frame: opaque meshes that cast a shadow, culled ones included
    /// (telemetry's `counts.shadow_casters`).
    #[must_use]
    pub fn shadow_caster_count(&self) -> usize {
        self.last_shadow_casters
    }

    /// Number of cached mesh/material/object entries (diagnostics).
    #[must_use]
    pub fn cache_sizes(&self) -> (usize, usize, usize) {
        (self.meshes.len(), self.materials.len(), self.objects.len())
    }

    /// Mesh data of `frame` this renderer has not uploaded yet: `(bytes, meshes)`, each
    /// distinct mesh counted once. Counting stops once both `stop` thresholds are reached
    /// (a caller asking "is this a big new scene" need not walk a whole map).
    pub(crate) fn pending_mesh_uploads(
        &self,
        frame: &crate::ExtractedFrame,
        stop: (u64, usize),
    ) -> (u64, usize) {
        let mut seen = crate::key_hash::KeyMap::<u64, ()>::default();
        let (mut bytes, mut meshes) = (0u64, 0usize);
        for mesh in &frame.meshes {
            if bytes >= stop.0 && meshes >= stop.1 {
                break;
            }
            let key = crate::runtime::mesh_key(&mesh.mesh);
            if self.meshes.contains_key(&key) || seen.insert(key, ()).is_some() {
                continue;
            }
            bytes += (std::mem::size_of_val(mesh.mesh.vertices.as_slice())
                + std::mem::size_of_val(mesh.mesh.indices.as_slice())) as u64;
            meshes += 1;
        }
        (bytes, meshes)
    }
}

/// The renderer fields the frame's problem list reads and writes.
pub(super) struct ProblemFields<'a> {
    pub(super) frame_problems: &'a mut Vec<crate::host_checklist::FrameProblem>,
    pub(super) texture_problems: &'a [crate::host_checklist::FrameProblem],
    pub(super) capability_problems: &'a mut Vec<crate::host_checklist::FrameProblem>,
    pub(super) capability_device: &'a mut Option<u64>,
}

/// Rebuilds the frame's problem list (`FrameRenderer::frame_problems`): texture problems, the
/// device's capability fallbacks (recomputed when the device changes), GPU health, and the
/// MSAA frame that skips the shared terrain draw and the planet sky (`shared_draw`: a shared
/// draw is installed). Moved out of `render_into` unchanged (ADR-1552).
pub(super) fn collect_frame_problems(
    fields: ProblemFields<'_>,
    device: &RenderDevice,
    scene: &FrameScene<'_>,
    samples: u32,
    shared_draw: bool,
    outdoor: &crate::outdoor::OutdoorState,
) {
    let ProblemFields {
        frame_problems,
        texture_problems,
        capability_problems,
        capability_device,
    } = fields;
    frame_problems.clear();
    frame_problems.extend(texture_problems.iter().cloned());
    // Capability fallbacks are a function of the device, not the frame: recompute only
    // when the device changed (a clamped min-spec profile, a hot-swapped adapter).
    if *capability_device != Some(device.device_id()) {
        *capability_problems = device.capabilities.fallback_notes();
        *capability_device = Some(device.device_id());
        for note in capability_problems.iter() {
            tracing::info!(
                target: "regia_render",
                code = note.code,
                "{} (fallback active)", note.message
            );
        }
    }
    frame_problems.extend(capability_problems.iter().cloned());
    crate::gpu_health::push_problems(frame_problems);
    if samples > 1 && (shared_draw || !outdoor.params().is_earth()) {
        frame_problems.push(crate::host_checklist::FrameProblem {
            code: crate::host_checklist::PROBLEM_MSAA_SKIPS_TERRAIN,
            message: format!(
                "msaa_samples {} with {}: the shared terrain draw and the planet sky are not drawn \
                 on MSAA frames; use msaa_samples 1 (TAA)",
                scene.quality.msaa_samples,
                if shared_draw {
                    "a shared terrain draw"
                } else {
                    "a planet sky"
                }
            ),
        });
        // Shared draws (native terrain) and the planet LUT sky (airless/Mars skies, sky
        // bodies, moons) run on single-sample frames only. On an MSAA frame they are
        // skipped, and auto exposure then meters whatever is left (often a black frame,
        // which drives the exposure to its minimum and blows meshes out).
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                target: "regia_render",
                msaa_samples = scene.quality.msaa_samples,
                "MSAA frame: the shared terrain draw and the planet sky (sky bodies, moons) are \
                 skipped; use a single-sample (TAA) quality for terrain and planet skies"
            );
        }
    }
}

/// What [`FrameRenderer::set_complete_frames`] changed, put back when it ends.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CompleteSaved {
    async_graphs: bool,
    async_sky: bool,
    ordered_cull: bool,
    exposure_hold: bool,
    ddgi_bake_sync: bool,
    wait_exposure_readback: bool,
}

#[cfg(test)]
mod complete_frames_tests {
    use super::*;

    #[test]
    fn aerial_toggle_survives_a_warm_worker_and_stand_in_changes() {
        let mut renderer = FrameRenderer::new();
        renderer.set_aerial_interiors(false);
        let mut stand_in = FrameRenderer::new();
        renderer.share_installs_with(&mut stand_in);
        assert!(!stand_in.aerial_interiors());
        renderer.absorb_stand_in(&mut stand_in);
        assert!(!renderer.aerial_interiors());
        stand_in.set_aerial_interiors(true);
        renderer.absorb_stand_in(&mut stand_in);
        assert!(renderer.aerial_interiors());
        stand_in.set_aerial_interiors(false);
        renderer.absorb_stand_in(&mut stand_in);
        assert!(!renderer.aerial_interiors());
    }

    #[test]
    fn aerial_toggle_invalidates_only_when_the_value_changes() {
        let mut renderer = FrameRenderer::new();
        renderer.taau_ready = true;
        renderer.screen_hold = 40;
        renderer.set_aerial_interiors(true);
        assert!(renderer.taau_ready);
        assert_eq!(renderer.screen_hold, 40);
        renderer.set_aerial_interiors(false);
        assert!(!renderer.taau_ready);
        assert_eq!(renderer.screen_hold, 0);
        renderer.taau_ready = true;
        renderer.screen_hold = 40;
        renderer.set_aerial_interiors(false);
        assert!(renderer.taau_ready);
        assert_eq!(renderer.screen_hold, 40);
        renderer.set_aerial_interiors(true);
        assert!(!renderer.taau_ready);
        assert_eq!(renderer.screen_hold, 0);
    }

    /// ADR-2582: the switch makes the runtime renderer's off-frame work inline for its length and
    /// puts back exactly what it found.
    #[test]
    fn complete_frames_turn_off_frame_work_inline_and_put_it_back() {
        let mut renderer = FrameRenderer::new();
        renderer.set_async_material_graphs(true);
        renderer.set_async_sky(true);
        assert!(!renderer.complete_frames());
        assert!(renderer.ordered_gpu_cull());
        renderer.set_complete_frames(true);
        assert!(renderer.complete_frames());
        assert!(!renderer.graph_cache.is_async());
        assert!(!renderer.outdoor.async_sky());
        assert!(renderer.ordered_gpu_cull());
        assert!(renderer.emissive_cache.complete());
        assert!(renderer.exposure_zones().holds_on_missing_dt());
        // The probes' bricks bake on the frame thread for the job's whole length, so no
        // frame traces a brick set the task pool's timing chose (audit fix-render4 #1).
        assert!(renderer.ddgi.bake_sync);
        // Turning it on twice keeps the first saved state.
        renderer.set_complete_frames(true);
        renderer.set_complete_frames(false);
        assert!(!renderer.complete_frames());
        assert!(renderer.graph_cache.is_async());
        assert!(renderer.outdoor.async_sky());
        assert!(renderer.ordered_gpu_cull());
        assert!(!renderer.emissive_cache.complete());
        assert!(!renderer.exposure_zones().holds_on_missing_dt());
        assert!(!renderer.ddgi.bake_sync);
        // A host that ordered its cull keeps it ordered afterwards.
        renderer.set_ordered_gpu_cull(true);
        renderer.set_async_material_graphs(false);
        renderer.set_complete_frames(true);
        renderer.set_complete_frames(false);
        assert!(renderer.ordered_gpu_cull());
        assert!(!renderer.graph_cache.is_async());
        renderer.set_complete_frames(false);
        assert!(!renderer.complete_frames());
    }

    /// Determinism audit (render1, finding 4): with complete frames on and the host
    /// giving no step, a zone-adapting main view holds its eye instead of measuring
    /// the wall clock, so two capture runs on machines of different speed expose
    /// the same EV on every frame.
    #[test]
    fn complete_frames_hold_the_zone_eye_without_a_host_step() {
        let target = crate::EvTarget::from_range(crate::exposure_zones::EvRange::new(8.0, 10.0));
        let meter = crate::exposure_zones::MeterReading::steady(5.0);
        let mut renderer = FrameRenderer::new();
        renderer.set_exposure_target(Some(target));
        renderer.set_complete_frames(true);
        let first = renderer.exposure_zones_mut().apply(meter);
        // The wall gap the bug read: without the hold, `frame_dt` measures it and
        // the eye walks by an amount that depends on the machine's speed.
        std::thread::sleep(std::time::Duration::from_millis(30));
        let second = renderer.exposure_zones_mut().apply(meter);
        assert_eq!(first, second, "no host step under complete frames: the eye holds");
        // An explicit host step still adapts, by the same amount on every run.
        let mut stepped = FrameRenderer::new();
        stepped.set_exposure_target(Some(target));
        stepped.set_complete_frames(true);
        stepped.set_exposure_dt(Some(1.0 / 30.0));
        let a = stepped.exposure_zones_mut().apply(meter);
        stepped.set_exposure_dt(Some(1.0 / 30.0));
        let b = stepped.exposure_zones_mut().apply(meter);
        assert_ne!(a, b, "an explicit step still walks the eye");
    }

    /// Complete frames meter auto exposure on the frame each histogram is due, not on whichever
    /// later frame the readback's map callback lands (the render6 determinism audit): the switch
    /// turns the exposure readback wait on and puts back the host's setting when it ends.
    #[test]
    fn complete_frames_waits_for_exposure_readback_and_puts_it_back() {
        let mut renderer = FrameRenderer::new();
        assert!(!renderer.waits_for_exposure_readback());
        renderer.set_complete_frames(true);
        assert!(
            renderer.waits_for_exposure_readback(),
            "complete frames meter each histogram on its due frame"
        );
        renderer.set_complete_frames(false);
        assert!(!renderer.waits_for_exposure_readback());
        // A host that waited anyway keeps its wait afterwards.
        renderer.set_wait_for_exposure_readback(true);
        renderer.set_complete_frames(true);
        assert!(renderer.waits_for_exposure_readback());
        renderer.set_complete_frames(false);
        assert!(
            renderer.waits_for_exposure_readback(),
            "the switch puts back what it found"
        );
        // Turning it on twice still puts back the first saved state.
        renderer.set_wait_for_exposure_readback(false);
        renderer.set_complete_frames(true);
        renderer.set_complete_frames(true);
        renderer.set_complete_frames(false);
        assert!(!renderer.waits_for_exposure_readback());
    }

    /// ADR-2728: a job that ends while the renderer is away on a game-frame warm turns the switch
    /// off on the stand-in. The renderer that comes back must not stay in complete frames for good.
    #[test]
    fn a_job_ending_during_a_warm_turns_the_switch_off_on_the_returning_renderer() {
        let mut renderer = FrameRenderer::new();
        renderer.set_async_material_graphs(true);
        renderer.set_complete_frames(true);
        let mut stand_in = FrameRenderer::new();
        renderer.share_installs_with(&mut stand_in);
        assert!(
            stand_in.complete_frames(),
            "the job's frames stay complete on the stand-in"
        );
        assert!(!stand_in.graph_cache.is_async());
        assert!(stand_in.ordered_gpu_cull());
        stand_in.set_complete_frames(false);
        assert!(
            stand_in.graph_cache.is_async(),
            "an abandoned warm leaves the stand-in with the host's settings"
        );
        renderer.absorb_stand_in(&mut stand_in);
        assert!(!renderer.complete_frames());
        assert!(renderer.graph_cache.is_async());
        assert!(renderer.ordered_gpu_cull());
    }

    /// ADR-2728: a job that starts during a warm keeps its frames complete after the renderer comes
    /// back, and when it ends the renderer gets its own settings back, not the stand-in's.
    #[test]
    fn a_job_starting_during_a_warm_holds_on_the_returning_renderer() {
        let mut renderer = FrameRenderer::new();
        renderer.set_async_material_graphs(true);
        let mut stand_in = FrameRenderer::new();
        renderer.share_installs_with(&mut stand_in);
        assert!(!stand_in.complete_frames());
        stand_in.set_complete_frames(true);
        renderer.absorb_stand_in(&mut stand_in);
        assert!(renderer.complete_frames());
        assert!(!renderer.graph_cache.is_async());
        renderer.set_complete_frames(false);
        assert!(renderer.graph_cache.is_async());
    }

    /// Muse's read-only review of fix-render6 (finding 1): the render.out start sequence must
    /// capture the host's complete-frames state before it forces the exposure wait — the switch
    /// saves the wait it finds, so forcing the wait first saved the forced `true`, and a job
    /// ending on the stand-in left the wait on for good on the renderer that came back. This
    /// mirrors `regia_agent::render_out`'s start sequence against the whole warm cycle with a
    /// host that ran with the wait off.
    #[test]
    fn a_render_out_job_ending_during_a_warm_restores_the_hosts_exposure_wait() {
        // The host ran with the exposure wait off.
        let mut renderer = FrameRenderer::new();
        assert!(!renderer.waits_for_exposure_readback());
        // render.out's start sequence (a world-rendering job): capture the host's wait and
        // complete-frames state first, then force the switch — the switch forces the wait
        // itself while it is on. Forcing the wait before the switch is the bug this test pins.
        let renders_world = true;
        let wait = renderer.waits_for_exposure_readback();
        let complete = renderer.complete_frames();
        renderer.set_complete_frames(renders_world || complete);
        assert!(renderer.waits_for_exposure_readback());
        // An in-place warm moves the renderer away; the job's frames stay complete on the
        // stand-in with the renderer's saved state (ADR-2728).
        let mut stand_in = FrameRenderer::new();
        renderer.share_installs_with(&mut stand_in);
        // The job ends on the stand-in: begin_close turns the switch off, then the job's own
        // saved wait (the host's, off) wins.
        stand_in.set_complete_frames(false);
        stand_in.set_wait_for_exposure_readback(wait);
        assert!(!stand_in.waits_for_exposure_readback());
        // The renderer comes back and absorbs the stand-in: the switch is off there, so its
        // own saved state applies — it must hold the host's wait (off), not the forced one.
        renderer.absorb_stand_in(&mut stand_in);
        assert!(
            !renderer.waits_for_exposure_readback(),
            "the forced wait must not outlive the job on the returning renderer"
        );
        assert!(!renderer.complete_frames());
    }
}
