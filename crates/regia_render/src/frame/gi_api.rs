// SPDX-License-Identifier: MPL-2.0
//! Global illumination settings and reads: the GI field, reflection probes, DDGI, SSGI and baked
//! GI volumes (moved out of `frame.rs` unchanged, ADR-1552).

use super::*;

impl FrameRenderer {
    /// Installs the irradiance field the forward shader samples.
    ///
    /// `None`, or [`crate::GiMode::Ambient`], leaves the lab lighting equation
    /// unchanged (`quality.y` stays 0). The GPU buffer is fixed-size; only the
    /// contents change.
    pub fn set_gi_field(&mut self, field: Option<crate::gi::GiField>) {
        self.installed_since.gi = true;
        self.gi_auto = None;
        self.gi_fallback = None;
        self.gi_field = field;
    }

    /// Installs reflection probes, replacing the whole set. The SSR pass blends at most the
    /// profile budget (the probes nearest the camera). In stochastic SSR (Desktop High and
    /// Ultra) a probe reflects inside its box and fades out over a margin outside it (a quarter
    /// of its largest half extent, at least 0.5 m); past every box a pixel keeps the frame's own
    /// environment. The older SSR pass (lab, editor, MSAA frames) blends them everywhere.
    pub fn set_reflection_probes(&mut self, probes: Vec<crate::reflections::ReflectionProbe>) {
        self.installed_since.probes = true;
        self.reflection_probes = probes;
    }

    /// Renders six cubemap faces from `center` and packs them into one probe.
    ///
    /// Each face looks along [`crate::cube_face_axes`] and uses that face's up
    /// vector. World +Y is parallel to the ±Y looks, so those faces stay black
    /// if the view up is left at +Y. Quality is [`crate::capture_quality`]: GI,
    /// screen-space GI, SSR and other probes stay off, and the face is a native
    /// 90° square of [`crate::PROBE_FACE_SIZE`]. The image is tonemapped in the
    /// material shader (no post stack) into the sRGB equirect
    /// [`crate::equirect_from_faces`] builds.
    ///
    /// Capture submits six frames on this renderer, so it advances the frame
    /// index and the shadow cache. The camera up is restored when the call
    /// returns, including on error.
    #[tracing::instrument(skip_all)]
    pub fn capture_reflection_probe(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        id: u64,
        center: [f32; 3],
        half_extent: [f32; 3],
    ) -> Result<crate::reflections::ReflectionProbe, RenderError> {
        self.probe_captured = true;
        self.with_probe_state(|this| {
            this.capture_reflection_probe_inner(device, scene, id, center, half_extent)
        })
    }

    /// Runs `faces` (probe-face frames) with the host's frame state saved and restored around
    /// them ([`Self::capture_reflection_probe`]).
    pub(super) fn with_probe_state<R>(
        &mut self,
        faces: impl FnOnce(&mut Self) -> Result<R, RenderError>,
    ) -> Result<R, RenderError> {
        let saved_up = self.camera_up;
        // Reflections show the world, not debug lines.
        let gizmos = self.gizmos.take();
        // The probe's faces are not the frame the scene export describes.
        let export = self.scene_export.take();
        // Faces are not the host's view: no cut detection, and a pending cut waits.
        self.cut.suspended += 1;
        // The host's next frame continues from the host's last frame, not from the last face:
        // - motion: each face stores its view-projection and models as "previous", so the next
        //   frame's camera motion (TAA history, motion blur) was measured against a 90° face
        //   looking elsewhere, a whole-screen smear after every capture;
        // - pre-exposure: the TAA history rescale is relative to the host's last frame;
        // - the faces place their own (unpaged) cascades and clear the kept placements, which
        //   moved every shadow edge by a texel phase on the next frame.
        let anchors = self.cascade_anchors;
        let near_anchor = self.near_anchor;
        let prev_view_proj = self.prev_view_proj;
        let have_prev_vp = self.have_prev_vp;
        let prev_models = std::mem::take(&mut self.prev_models);
        let prev_palettes = std::mem::take(&mut self.prev_palettes);
        let prev_joints = std::mem::take(&mut self.prev_joints);
        let prev_pre_exposure = self.prev_pre_exposure;
        self.capturing_probe = true;
        let result = faces(self);
        self.capturing_probe = false;
        self.cascade_anchors = anchors;
        self.near_anchor = near_anchor;
        self.prev_view_proj = prev_view_proj;
        self.have_prev_vp = have_prev_vp;
        self.prev_models = prev_models;
        self.prev_palettes = prev_palettes;
        self.prev_joints = prev_joints;
        self.prev_pre_exposure = prev_pre_exposure;
        self.cut.suspended -= 1;
        self.camera_up = saved_up;
        self.gizmos = gizmos;
        self.scene_export = export;
        result
    }

    pub(super) fn capture_reflection_probe_inner(
        &mut self,
        device: &RenderDevice,
        scene: &FrameScene<'_>,
        id: u64,
        center: [f32; 3],
        half_extent: [f32; 3],
    ) -> Result<crate::reflections::ReflectionProbe, RenderError> {
        let size = crate::reflections::PROBE_FACE_SIZE;
        let mut faces: [Vec<u8>; 6] = std::array::from_fn(|_| Vec::new());
        for face in 0..6u32 {
            let (dir, up) = crate::reflections::cube_face_axes(face);
            self.camera_up = up;
            let target = [center[0] + dir[0], center[1] + dir[1], center[2] + dir[2]];
            let face_scene = probe_face_scene(scene, center, target, size);
            let pixels = self.render_offscreen(device, &face_scene)?.pixels;
            let expected = size as usize * size as usize * 4;
            if pixels.len() != expected {
                return Err(RenderError::Readback(format!(
                    "probe face {face} returned {} bytes, expected {expected}",
                    pixels.len()
                )));
            }
            faces[face as usize] = pixels;
        }
        let refs: [&[u8]; 6] = [
            faces[0].as_slice(),
            faces[1].as_slice(),
            faces[2].as_slice(),
            faces[3].as_slice(),
            faces[4].as_slice(),
            faces[5].as_slice(),
        ];
        let equirect = crate::reflections::equirect_from_faces(
            &refs,
            size,
            crate::reflections::EQUIRECT_WIDTH,
            crate::reflections::EQUIRECT_HEIGHT,
        );
        Ok(crate::reflections::ReflectionProbe {
            id,
            center,
            half_extent,
            equirect,
            width: crate::reflections::EQUIRECT_WIDTH,
            height: crate::reflections::EQUIRECT_HEIGHT,
        })
    }

    /// Records how long the CPU probe trace took, for the render lab.
    pub fn set_gi_cpu_ms(&mut self, ms: f32) {
        self.gi_cpu_ms = if ms.is_finite() { ms.max(0.0) } else { 0.0 };
    }

    /// CPU probe-trace time from [`Self::set_gi_cpu_ms`]. Probe sampling itself
    /// is inside [`GpuTimings::forward_ms`].
    #[must_use]
    pub fn gi_cpu_ms(&self) -> f32 {
        self.gi_cpu_ms
    }

    /// Settings of the GPU irradiance probes (`GiMode::Probes`). A change takes effect on
    /// the next frame; a new volume size recreates the probes (they re-converge).
    pub fn set_ddgi_settings(&mut self, settings: crate::DdgiSettings) {
        self.installed_since.ddgi_settings = true;
        // A still frame reuses its composite (`ScreenCacheKey`), which holds the old mode's light.
        if settings.specular != self.ddgi.settings.specular {
            self.screen_cache_key = None;
        }
        self.ddgi.settings = settings.clamped();
    }

    /// The GPU irradiance probe settings in use.
    #[must_use]
    pub fn ddgi_settings(&self) -> &crate::DdgiSettings {
        &self.ddgi.settings
    }

    /// `Some(false)` turns the GPU irradiance probes off whatever the profile says,
    /// `Some(true)` turns them on, `None` (the default) follows `FrameQuality::gi_probes`
    /// (on in Desktop High and Ultra). They run on linear-HDR frames only.
    pub fn set_ddgi_enabled(&mut self, enabled: Option<bool>) {
        self.installed_since.ddgi_enabled = true;
        self.ddgi.enabled = enabled;
    }

    /// The probe override set by [`Self::set_ddgi_enabled`].
    #[must_use]
    pub fn ddgi_enabled(&self) -> Option<bool> {
        self.ddgi.enabled
    }

    /// Measurement setting: the resolution of the screen lookup through which probe GI (the
    /// GPU probes or a baked volume) reaches the forward and terrain passes. Every preset uses
    /// [`crate::GiLookupResolution::Half`]; `Full` is the reference it is measured against and
    /// `Quarter` the rejected step down (ADR-1033). A lookup too large for its buffer falls back
    /// to `Half`. Not carried to a stand-in renderer and not a player setting.
    #[doc(hidden)]
    pub fn set_gi_lookup_resolution(&mut self, resolution: crate::GiLookupResolution) {
        self.gi_lookup = resolution;
    }

    /// The lookup resolution set by [`Self::set_gi_lookup_resolution`].
    #[doc(hidden)]
    #[must_use]
    pub fn gi_lookup_resolution(&self) -> crate::GiLookupResolution {
        self.gi_lookup
    }

    /// Measurement setting: `Some(true)` gathers screen-space GI at half resolution with the
    /// joint-bilateral upsample (`ssgi.wgsl` `fs_upsample`) whatever the quality says,
    /// `Some(false)` gathers at full resolution, `None` (the default) follows
    /// [`FrameQuality::ssgi_half_res`] (the 0.67 Desktop High budget only). ADR-1033 measures it
    /// against the full gather. Not carried to a stand-in renderer and not a player setting.
    #[doc(hidden)]
    pub fn set_ssgi_half_res(&mut self, half: Option<bool>) {
        self.ssgi_half_res = half;
    }

    /// The override set by [`Self::set_ssgi_half_res`].
    #[doc(hidden)]
    #[must_use]
    pub fn ssgi_half_res(&self) -> Option<bool> {
        self.ssgi_half_res
    }

    /// Measurement setting: the half-resolution SSGI upsample weighs its four taps by relative
    /// linear depth (within 2 % + 5 cm of the pixel's, the probe lookup's band; the tap nearest in
    /// depth when none is) instead of exp(−80 |Δd|) on raw device depth, which barely separates
    /// far silhouettes (ADR-1033). `false`, the default, keeps the shipped weights. Only frames
    /// that gather at half resolution use the upsample.
    #[doc(hidden)]
    pub fn set_ssgi_upsample_relative_depth(&mut self, on: bool) {
        self.ssgi_relative_depth = on;
    }

    /// The variant set by [`Self::set_ssgi_upsample_relative_depth`].
    #[doc(hidden)]
    #[must_use]
    pub fn ssgi_upsample_relative_depth(&self) -> bool {
        self.ssgi_relative_depth
    }

    /// Puts the world-space radiance cache behind screen-space GI, or takes it away
    /// (`specs/79` § 12, ADR-2085). On a frame that runs both the probes and screen-space GI, in the
    /// main view, the gather becomes a depth-buffer ray march whose misses read the cache and whose
    /// result corrects the forward pass's probe light. Anywhere else, the plain gather runs. Turning
    /// it off frees its buffers. The runtime calls this every frame with
    /// `RenderSettings::effective_radiance_cache`; off until then, as in every preset.
    pub fn set_radiance_cache(&mut self, on: bool) {
        self.radiance_cache.set_enabled(on);
    }

    /// The cache's layout, budget and gather tunables. A new layout empties the cells; they refill
    /// over the next frames at the budget's pace.
    pub fn set_radiance_cache_settings(&mut self, settings: crate::RadianceCacheSettings) {
        self.radiance_cache.set_settings(settings);
    }

    /// The settings in use ([`Self::set_radiance_cache_settings`]).
    #[must_use]
    pub fn radiance_cache_settings(&self) -> crate::RadianceCacheSettings {
        self.radiance_cache.settings()
    }

    /// Whether the cache was asked for, whether the last frame read it, and its budget's counters
    /// (agent `render.get_radiance_cache`, `book/agents/radiance-cache.md`).
    #[must_use]
    pub fn radiance_cache_status(&self) -> crate::RadianceCacheStatus {
        self.radiance_cache.status()
    }

    /// Installs the map's baked GI volume (`.fgiv`, baked by `regia bake-gi`;
    /// [`crate::baked_gi`]), or removes it with `None`. It uploads on the next frame. While it
    /// lights a frame ([`crate::FrameQuality::baked_gi`], a linear-HDR frame with the
    /// single-sample depth prepass, desktop), it replaces the probe GI at the forward and terrain
    /// passes and the sky-body fill where it covers; DDGI and SSGI are off and full-resolution
    /// GTAO runs at half resolution. The sun and its shadows stay live. A stand-in renderer for a
    /// warm worker gets it too (`share_installs_with`).
    pub fn set_baked_gi(&mut self, volume: Option<std::sync::Arc<regia_asset::baked_gi::BakedGiVolume>>) {
        self.installed_since.baked_gi = true;
        self.baked_gi.install(volume);
    }

    /// The installed baked GI volume.
    #[must_use]
    pub fn baked_gi(&self) -> Option<&std::sync::Arc<regia_asset::baked_gi::BakedGiVolume>> {
        self.baked_gi.volume.as_ref()
    }

    /// Blends more baked GI volumes with the installed one (ADR-1097,
    /// [`crate::baked_gi::BakedGiLayer`]): adjacent tiles, a streamed level's bake, a nested set
    /// piece. The installed volume ([`Self::set_baked_gi`]) takes part as a layer of priority 0,
    /// full weight and no blend distance, listed first; an empty slice blends none. Returns how
    /// many volumes the next frame blends (at most [`crate::baked_gi::MAX_BAKED_GI_VOLUMES`],
    /// in priority order). A change of weights, priorities or blend distances alone uploads no
    /// volume data; a new set of volumes uploads them all once. A stand-in renderer gets the
    /// layers too.
    pub fn set_baked_gi_layers(&mut self, layers: &[crate::baked_gi::BakedGiLayer]) -> usize {
        self.installed_since.baked_gi = true;
        self.baked_gi.set_layers(layers)
    }

    /// The layers blended with the installed volume ([`Self::set_baked_gi_layers`]).
    #[must_use]
    pub fn baked_gi_layers(&self) -> &[crate::baked_gi::BakedGiLayer] {
        &self.baked_gi.layers
    }

    /// The baked irradiance / π at world `p` (unit normal `n`, unit `v` toward the viewer) of
    /// the installed volume and the layers, blended as the screen pass blends them
    /// ([`crate::baked_gi::sample_static_gi_blend`]) with the last baked frame's sun and fill
    /// (none before the first: the sun-bounce and fill components then read 0). For gameplay
    /// queries (light a particle, check a seam); coverage 0 where no volume reaches.
    #[must_use]
    pub fn sample_baked_gi(&self, p: [f32; 3], n: [f32; 3], v: [f32; 3]) -> crate::StaticGiSample {
        self.baked_gi.sample(p, n, v)
    }

    /// Tunables of the baked GI lookup (biases, intensity, the sky and emissive levels, whether
    /// the sun and fill follow the frame's lights). Takes effect on the next frame.
    pub fn set_baked_gi_settings(&mut self, settings: crate::BakedGiSettings) {
        self.installed_since.baked_gi = true;
        // As `set_ddgi_settings`: the static composite holds the old mode's light.
        if settings.specular != self.baked_gi.settings.specular {
            self.screen_cache_key = None;
        }
        self.baked_gi.settings = settings;
    }

    /// The baked GI tunables in use.
    #[must_use]
    pub fn baked_gi_settings(&self) -> crate::BakedGiSettings {
        self.baked_gi.settings
    }

    /// `Some(false)` never lights a frame with the installed volume (DDGI and SSGI then run as
    /// the quality says), `Some(true)` always does, `None` (the default) follows
    /// [`crate::FrameQuality::baked_gi`].
    pub fn set_baked_gi_enabled(&mut self, enabled: Option<bool>) {
        self.installed_since.baked_gi = true;
        self.baked_gi.enabled = enabled;
    }

    /// The override set by [`Self::set_baked_gi_enabled`].
    #[must_use]
    pub fn baked_gi_enabled(&self) -> Option<bool> {
        self.baked_gi.enabled
    }

    /// Whether the baked volume lit the last main-view frame, its size and the light factors.
    #[must_use]
    pub fn baked_gi_status(&self) -> crate::BakedGiStatus {
        self.baked_gi.status()
    }

    /// Replaces the capsule occluders of dynamic units over the baked GI volume (a few per
    /// vehicle or character, world space; kept until the next call, so a host sets them every
    /// frame its units move). The baked screen pass darkens the baked light toward each
    /// direction by their analytic occlusion (`crate::baked_gi::GiCapsule`). Unusable capsules
    /// and those past [`crate::baked_gi::MAX_GI_OCCLUDERS`] are dropped
    /// ([`crate::BakedGiStatus::occluders_dropped`]); returns how many are used. Without an
    /// installed volume (and on Android, which has none) they cost nothing; an empty list
    /// uploads nothing.
    pub fn set_gi_occluders(&mut self, capsules: &[crate::baked_gi::GiCapsule]) -> usize {
        self.baked_gi.set_occluders(capsules)
    }

    /// The capsule occluders in use ([`Self::set_gi_occluders`]).
    #[must_use]
    pub fn gi_occluders(&self) -> &[crate::baked_gi::GiCapsule] {
        &self.baked_gi.occluders
    }

    /// Reads the baked GI screen lookup of the last frame back (tests and debugging: blocks and
    /// submits): lookup width, height and texels, row-major (half resolution unless
    /// [`Self::set_gi_lookup_resolution`] says otherwise).
    #[doc(hidden)]
    pub fn baked_gi_read_back(
        &self,
        device: &RenderDevice,
    ) -> Result<Option<(u32, u32, Vec<crate::DdgiScreenTexel>)>, RenderError> {
        self.baked_gi.read_back(device)
    }

    /// Reads the probe-relative AO split (ADR-1104) of the last frame's baked screen lookup back
    /// (tests and debugging: blocks and submits): lookup width, height and the split per texel,
    /// row-major.
    #[doc(hidden)]
    pub fn baked_gi_split_read_back(
        &self,
        device: &RenderDevice,
    ) -> Result<Option<(u32, u32, Vec<crate::baked_gi::ProbeAoSplit>)>, RenderError> {
        self.baked_gi.read_back_split(device)
    }

    /// Reads the irradiance probes back (tests and debugging: blocks and submits).
    #[doc(hidden)]
    pub fn ddgi_read_back(&self, device: &RenderDevice) -> Result<Option<crate::DdgiReadback>, RenderError> {
        self.ddgi.read_back(device)
    }

    /// Runs the last frame's probe screen lookup again, alone in its own submit (tests and
    /// debugging: blocks). `false` when no lookup has run.
    ///
    /// # Errors
    ///
    /// The device poll failed.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn ddgi_rerun_screen_lookup(&mut self, device: &RenderDevice) -> Result<bool, RenderError> {
        self.ddgi.rerun_screen(device)
    }

    /// The first `texels` texels of the probe screen lookup, 8 words each (tests and debugging:
    /// blocks and submits). `None` while the lookup is off.
    ///
    /// # Errors
    ///
    /// The readback failed.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn ddgi_screen_words_read_back(
        &self,
        device: &RenderDevice,
        texels: u32,
    ) -> Result<Option<Vec<u32>>, RenderError> {
        self.ddgi.read_back_screen_words(device, texels)
    }

    /// The irradiance probes' two uniform blocks, each as the bytes the CPU last queued and the
    /// bytes the GPU buffer holds now: `[screen CPU, screen GPU, probes CPU, probes GPU]` (tests and
    /// debugging: blocks and submits). `None` before the probes' first frame.
    ///
    /// # Errors
    ///
    /// A readback failed.
    #[cfg(feature = "test-hooks")]
    #[doc(hidden)]
    pub fn ddgi_uniforms_read_back(
        &self,
        device: &RenderDevice,
    ) -> Result<Option<[Vec<u8>; 4]>, RenderError> {
        self.ddgi.read_back_uniforms(device)
    }

    /// The planet sky's hemisphere ambient `[sky, ground]` (irradiance / π) of the last
    /// outdoor parameters, when the atmosphere sky is on (debugging).
    #[doc(hidden)]
    #[must_use]
    pub fn debug_hemisphere_ambient(&self) -> Option<[[f32; 3]; 2]> {
        self.outdoor.hemisphere_ambient()
    }

    /// Whether the GPU irradiance probes ran on the last frame.
    #[must_use]
    pub fn ddgi_active(&self) -> bool {
        self.ddgi.active
    }

    /// Emitters in the probes' emitter list on the last frame (`DdgiSettings::emitters`).
    #[must_use]
    pub fn ddgi_emitter_count(&self) -> usize {
        self.ddgi.emitter_count()
    }

    /// Mesh signed-distance bricks still baking on the task pool (those meshes do not
    /// occlude probe rays yet).
    #[must_use]
    pub fn ddgi_pending_bakes(&self) -> usize {
        self.ddgi.pending_bakes()
    }

    /// Replaces the mesh keys that skip signed-distance GI proxies.
    ///
    /// The runtime calls this every extracted frame with
    /// [`crate::ExtractedFrame::gi_proxy_disabled_keys`]. A direct caller (a lab scene, a
    /// golden) that never calls it traces every proxy candidate, which is the previous
    /// behavior. Pass an empty iterator to trace them all again. A key is
    /// [`FrameMesh::mesh_key`]. One opted-out instance opts that whole mesh out, because a
    /// brick is stored per mesh. Shadows are unchanged
    /// ([`crate::MeshRenderer::gi_proxy`]). The set does not recreate the probe volume.
    pub fn set_gi_proxy_disabled(&mut self, keys: impl IntoIterator<Item = u64>) {
        self.ddgi.set_gi_proxy_disabled(keys);
    }

    /// Whether `mesh_key` has a signed-distance brick in the pool after the last probe update.
    #[must_use]
    pub fn ddgi_brick_resident(&self, mesh_key: u64) -> bool {
        self.ddgi.brick_resident(mesh_key)
    }

    /// Whether `mesh_key`'s brick was in the last probe update's instance list (the rays
    /// trace it).
    #[must_use]
    pub fn ddgi_brick_traced(&self, mesh_key: u64) -> bool {
        self.ddgi.brick_traced(mesh_key)
    }

    /// Frames the probes' traced scene (volume placement, sun, sky, mesh proxies) has been
    /// unchanged; `u32::MAX` when the probes are off.
    #[must_use]
    pub fn ddgi_settled_frames(&self) -> u32 {
        self.ddgi.settled_frames()
    }

    /// Sizes the GPU irradiance probes for a quality tier (`specs/79`, ADR-1679): writes the
    /// tier's cascades, probes per axis, update divisor, bricks per cascade and brick pool into
    /// the probe settings and leaves the rest ([`crate::DynamicGiTier::apply`]). Nothing happens
    /// when `tier` is the tier already applied, so a host's own [`Self::set_ddgi_settings`]
    /// survives until the tier changes. A new volume size recreates the probes (they
    /// re-converge in about 8 frames). Returns whether the settings changed. The runtime calls
    /// this every frame with the profile's tier or the player's override
    /// (`RenderSettings::effective_dynamic_gi_tier`); [`crate::DynamicGiTier::High`] is
    /// applied until then (a fresh renderer's probe settings are
    /// `DynamicGiTier::High.settings()`, ADR-5001).
    pub fn set_dynamic_gi_tier(&mut self, tier: crate::DynamicGiTier) -> bool {
        if self.ddgi.tier == tier {
            return false;
        }
        self.installed_since.ddgi_settings = true;
        self.ddgi.tier = tier;
        let before = self.ddgi.settings.clone();
        let mut next = before.clone();
        tier.apply(&mut next);
        self.ddgi.settings = next.clamped();
        self.ddgi.settings != before
    }

    /// The quality tier last applied by [`Self::set_dynamic_gi_tier`].
    #[must_use]
    pub fn dynamic_gi_tier(&self) -> crate::DynamicGiTier {
        self.ddgi.tier
    }

    /// The dynamic GI's tier, whether it ran, its budget and its last measured GPU cost
    /// (agent `render.get_dynamic_gi`, `book/agents/dynamic-gi.md`).
    #[must_use]
    pub fn dynamic_gi_status(&self) -> crate::DynamicGiStatus {
        let timings = self.gpu_timings();
        crate::DynamicGiStatus {
            tier: self.ddgi.tier,
            custom: !self.ddgi.tier.matches(&self.ddgi.settings),
            active: self.ddgi.active,
            enabled_override: self.ddgi.enabled,
            budget: crate::dynamic_gi::budget_of(&self.ddgi.settings),
            settled_frames: self.ddgi.settled_frames(),
            pending_bakes: self.ddgi.pending_bakes(),
            emitters: self.ddgi.emitter_count(),
            gpu_ms: timings.gi_ms,
            gpu_stages_ms: timings.gi_stages_ms,
            pyramid: self.ddgi.pyramid_stats(),
        }
    }
}
