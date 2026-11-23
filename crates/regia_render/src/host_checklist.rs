// SPDX-License-Identifier: MPL-2.0
//! A host's frame set-up against the reference the look-dev harness renders with
//! (`lookdev::look_r2` and `do27_chase`; `book/user-guide/host-look-checklist.md`).
//!
//! [`crate::FrameRenderer::host_checklist_report`] compares the frame a host submits, and the
//! renderer state it set, with the harness: Desktop High quality, the HDR post stack with auto
//! exposure, physical lights, the planet sky and the shared terrain draw. A game shell can
//! print it once and diff it against the harness's own report.

use std::fmt;

use crate::{FrameQuality, FrameScene, RenderProfile, RenderProfilePreset, TonemapOperator};

/// One checked item.
#[derive(Debug, Clone, PartialEq)]
pub struct HostCheck {
    /// What is checked (a field path or a short name).
    pub item: &'static str,
    /// What the host has.
    pub actual: String,
    /// What the harness uses.
    pub expected: String,
    /// `true` when they agree (informational items are always `true`).
    pub ok: bool,
    /// The difference breaks the frame (something is not drawn or not lit), not only its
    /// look. Errors print first, as `ERROR`.
    pub error: bool,
}

/// [`FrameProblem::code`] of an MSAA frame with a shared terrain draw or a planet sky: the
/// renderer skips both on multisampled frames (the terrain, sky bodies and moons are not drawn,
/// and auto exposure meters what is left).
pub const PROBLEM_MSAA_SKIPS_TERRAIN: &str = "msaa_skips_terrain";

/// [`FrameProblem::code`] prefixes of the capability fallbacks ([`crate::RenderCapabilities`]):
/// a missing optional feature (or a limit below the fast path's want) with a working fallback
/// active. Differences, not errors — the frame draws, at a cost the message names.
pub const FALLBACK_CODE_PREFIX: &str = "fallback_";

/// [`FrameProblem::code`]: no GPU timestamp queries on the adapter (per-pass GPU timings off).
pub const PROBLEM_FALLBACK_NO_TIMESTAMPS: &str = "fallback_no_timestamps";
/// [`FrameProblem::code`]: no BC and no ASTC decompression (cooked maps CPU-decoded to RGBA8).
pub const PROBLEM_FALLBACK_NO_BLOCK_COMPRESSION: &str = "fallback_no_block_compression";
/// [`FrameProblem::code`]: no MULTI_DRAW_INDIRECT_COUNT (GPU-driven draws read the count back).
pub const PROBLEM_FALLBACK_CPU_INDIRECT_COUNT: &str = "fallback_cpu_indirect_count";
/// [`FrameProblem::code`]: non-zero `first_instance` in indirect draws is not honoured.
pub const PROBLEM_FALLBACK_INDIRECT_FIRST_INSTANCE: &str = "fallback_zero_first_instance";
/// [`FrameProblem::code`]: RG11B10UFLOAT is not a colour attachment (RGBA16F HDR instead).
pub const PROBLEM_FALLBACK_RGBA16F_HDR: &str = "fallback_rgba16f_hdr";
/// [`FrameProblem::code`]: 32-bit float textures are not filterable (terrain height pyramid).
pub const PROBLEM_FALLBACK_NO_FLOAT32_FILTER: &str = "fallback_no_float32_filter";
/// [`FrameProblem::code`]: no forced early depth tests (terrain VT feedback shades in full).
pub const PROBLEM_FALLBACK_NO_EARLY_DEPTH_TEST: &str = "fallback_no_early_depth_test";
/// [`FrameProblem::code`]: too few storage textures per stage (Hi-Z/SSSR pyramids per-mip).
pub const PROBLEM_FALLBACK_PER_MIP_DEPTH_PYRAMID: &str = "fallback_per_mip_depth_pyramid";
/// [`FrameProblem::code`]: too few storage buffers per stage (vis-buffer resolve off).
pub const PROBLEM_FALLBACK_VISBUFFER_STORAGE: &str = "fallback_visbuffer_storage";
/// [`FrameProblem::code`]: fewer than 5 bind groups (the terrain VT / self-shadow cascade off).
pub const PROBLEM_FALLBACK_NO_BIND_GROUPS: &str = "fallback_no_bind_groups";
/// [`FrameProblem::code`]: the sun shadow is PCF on the cascades only (Adreno driver workaround,
/// [`crate::shader_workarounds`], ADR-1749).
pub const PROBLEM_FALLBACK_SUN_SHADOW_PCF_ONLY: &str = "fallback_sun_shadow_pcf_only";

/// Something that broke a frame the renderer drew ([`crate::FrameRenderer::frame_problems`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameProblem {
    /// Stable code (e.g. [`PROBLEM_MSAA_SKIPS_TERRAIN`]).
    pub code: &'static str,
    /// What happened and what to change.
    pub message: String,
}

/// The whole report ([`fmt::Display`] prints one line per item, differences first).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct HostChecklist {
    /// Every item, in checklist order.
    pub checks: Vec<HostCheck>,
}

impl HostChecklist {
    /// Items that differ from the harness.
    #[must_use]
    pub fn differences(&self) -> Vec<&HostCheck> {
        self.checks.iter().filter(|c| !c.ok).collect()
    }

    /// Differences that break the frame ([`HostCheck::error`]).
    #[must_use]
    pub fn errors(&self) -> Vec<&HostCheck> {
        self.checks.iter().filter(|c| !c.ok && c.error).collect()
    }

    fn push(&mut self, item: &'static str, actual: impl fmt::Debug, expected: impl fmt::Debug, ok: bool) {
        self.checks.push(HostCheck {
            item,
            actual: format!("{actual:?}"),
            expected: format!("{expected:?}"),
            ok,
            error: false,
        });
    }

    fn push_error(
        &mut self,
        item: &'static str,
        actual: impl fmt::Debug,
        expected: impl fmt::Debug,
        ok: bool,
    ) {
        self.push(item, actual, expected, ok);
        if let Some(last) = self.checks.last_mut() {
            last.error = true;
        }
    }

    fn same<T: fmt::Debug + PartialEq>(&mut self, item: &'static str, actual: T, expected: T) {
        let ok = actual == expected;
        self.push(item, actual, expected, ok);
    }

    fn info(&mut self, item: &'static str, actual: impl fmt::Debug) {
        self.push(item, actual, "(any)", true);
    }
}

impl fmt::Display for HostChecklist {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let diffs = self.checks.iter().filter(|c| !c.ok).count();
        let errors = self.checks.iter().filter(|c| !c.ok && c.error).count();
        writeln!(
            f,
            "host look checklist: {diffs} difference(s) from the harness, {errors} error(s)"
        )?;
        for c in self.checks.iter().filter(|c| !c.ok && c.error) {
            writeln!(f, "  ERROR {}: {} (harness: {})", c.item, c.actual, c.expected)?;
        }
        for c in self.checks.iter().filter(|c| !c.ok && !c.error) {
            writeln!(f, "  DIFF {}: {} (harness: {})", c.item, c.actual, c.expected)?;
        }
        for c in self.checks.iter().filter(|c| c.ok) {
            writeln!(f, "  ok   {}: {}", c.item, c.actual)?;
        }
        Ok(())
    }
}

impl crate::FrameRenderer {
    /// Compares `scene` (the frame the host submits) and this renderer's state with the
    /// look-dev harness's frame: Desktop High quality, post on with auto exposure and AgX,
    /// depth prepass, sun shadows and IBL on, the forward fog off (the planet sky does the
    /// aerial perspective), physical light levels, a shared terrain draw, and the level's
    /// planet sky. Informational items (planet, clock, sky body, metered luminance) are
    /// listed without a verdict.
    #[must_use]
    pub fn host_checklist_report(&self, scene: &FrameScene<'_>) -> HostChecklist {
        let mut out = HostChecklist::default();
        let reference =
            FrameQuality::from_profile(&RenderProfile::from_preset(RenderProfilePreset::DesktopHigh));
        let q = scene.quality;
        out.same("quality.msaa_samples", q.msaa_samples, reference.msaa_samples);
        out.same("quality.hdr_target", q.hdr_target, reference.hdr_target);
        out.same(
            "quality.render_scale_permille",
            q.render_scale_permille,
            reference.render_scale_permille,
        );
        out.same("quality.taau", q.taau, reference.taau);
        out.same(
            "quality.physical_atmosphere",
            q.physical_atmosphere,
            reference.physical_atmosphere,
        );
        out.same(
            "quality.volumetric_fog",
            q.volumetric_fog,
            reference.volumetric_fog,
        );
        out.same(
            "quality.volumetric_clouds",
            q.volumetric_clouds,
            reference.volumetric_clouds,
        );
        out.same(
            "quality.cloud_reprojection",
            q.cloud_reprojection,
            reference.cloud_reprojection,
        );
        out.same(
            "quality.cloud_step_percent",
            q.cloud_step_percent,
            reference.cloud_step_percent,
        );
        out.same(
            "quality.cloud_cone_light",
            q.cloud_cone_light,
            reference.cloud_cone_light,
        );
        out.same(
            "quality.paged_sun_shadows",
            q.paged_sun_shadows,
            reference.paged_sun_shadows,
        );
        // A hand-built quality with the free-sliding cascades shimmers (ADR-1236).
        out.same(
            "quality.stable_sun_cascades",
            q.stable_sun_cascades,
            reference.stable_sun_cascades,
        );
        out.same(
            "quality.translucent_shadow_cascades",
            q.translucent_shadow_cascades,
            reference.translucent_shadow_cascades,
        );
        out.same(
            "quality.translucent_local_shadows",
            q.translucent_local_shadows,
            reference.translucent_local_shadows,
        );
        out.same(
            "quality.csm_cascade_count",
            q.csm_cascade_count,
            reference.csm_cascade_count,
        );
        out.same(
            "quality.csm_resolution",
            q.csm_resolution,
            reference.csm_resolution,
        );
        out.same("quality.shadow_filter", q.shadow_filter, reference.shadow_filter);
        out.same("quality.ssao", q.ssao, reference.ssao);
        out.same("quality.gi", q.gi, reference.gi);
        out.same(
            "quality.screen_space_gi",
            q.screen_space_gi,
            reference.screen_space_gi,
        );
        out.same("quality.baked_gi", q.baked_gi, reference.baked_gi);
        out.same("quality.ssr", q.ssr, reference.ssr);
        out.same(
            "quality.reflection_probes",
            q.reflection_probes,
            reference.reflection_probes,
        );
        out.same(
            "quality.parallax_occlusion",
            q.parallax_occlusion,
            reference.parallax_occlusion,
        );
        out.same("quality.gpu_driven", q.gpu_driven, reference.gpu_driven);
        out.same(
            "quality.terrain_budget",
            q.terrain_budget,
            reference.terrain_budget,
        );
        out.same(
            "quality.texture_streaming",
            q.texture_streaming,
            reference.texture_streaming,
        );
        // Texture memory: within the budget, and cooked (block-compressed) maps rather than a
        // level of PNGs. Differences, not errors: the frame draws, the machine may not survive.
        let textures = self.texture_memory();
        out.push(
            "material textures within the texture budget (MiB)",
            textures.resident_bytes / (1024 * 1024),
            format!("≤ {}", textures.checked_budget() / (1024 * 1024)),
            !textures.over_budget(),
        );
        out.push(
            "uncompressed (RGBA8) material textures",
            textures.uncompressed,
            format!(
                "≤ {} (cook PNG/JPEG to BC7/ASTC: `regia cook`)",
                textures.uncompressed_limit()
            ),
            !textures.over_uncompressed_limit(),
        );
        let wrong = &textures.wrong_platform;
        out.push(
            "textures cooked for another platform (CPU-decoded or unbound)",
            wrong.decoded + wrong.unbound,
            "0 (cook for this device: `regia cook --platform`)",
            wrong.decoded + wrong.unbound == 0,
        );
        out.same("scene.depth_prepass", scene.depth_prepass, true);
        out.same("scene.directional_shadows", scene.directional_shadows, true);
        out.same("scene.ibl", scene.ibl, true);
        out.same("scene.fog.enabled", scene.fog.enabled, false);
        out.same("scene.near ≤ 0.5 m", scene.near <= 0.5, true);
        out.same("scene.far finite", scene.far.is_finite(), true);
        match scene.post {
            Some(post) => {
                out.same("post.enabled", post.enabled, true);
                out.same("post.auto_exposure", post.auto_exposure, true);
                out.same("post.tonemap", post.tonemap, TonemapOperator::AgX);
                out.same("post.bloom", post.bloom, true);
                out.info(
                    "post.exposure band (min, max EV100)",
                    (post.auto_exposure_min_ev100, post.auto_exposure_max_ev100),
                );
                out.info("post.exposure_compensation", post.exposure_compensation);
                out.info("post.contrast / saturation", (post.contrast, post.saturation));
                out.info("post.grade", post.grade);
                out.info("post.lut", post.lut.as_ref().map(|l| l.size()));
            }
            None => out.push(
                "post",
                "None",
                "Some(RenderSettings::resolve_post + apply_level_look)",
                false,
            ),
        }
        // Physical light: an outdoor sun is thousands of lux. A pre-exposed host light
        // (a few lux, or 1.0) prints black under auto exposure.
        let lux = scene.directional.illuminance;
        out.push(
            "directional.illuminance (lux)",
            lux,
            "≥ 1000 (physical, not pre-exposed)",
            lux >= 1000.0,
        );
        out.same(
            "shared draw set (renderer.set_shared_draw)",
            self.has_shared_draw(),
            true,
        );
        let params = self.outdoor_params();
        // MSAA with terrain or a planet sky: the renderer draws neither (frame-breaking).
        let msaa_breaks = q.msaa_samples >= 4 && (self.has_shared_draw() || !params.is_earth());
        out.push_error(
            "MSAA with the shared terrain draw / planet sky",
            if msaa_breaks {
                format!(
                    "msaa_samples {}: terrain and planet sky not drawn",
                    q.msaa_samples
                )
            } else {
                "no".to_string()
            },
            "msaa_samples 1 (TAA)",
            !msaa_breaks,
        );
        // Texture memory problems are the three items above (differences, not errors).
        // Capability fallbacks (fallback_* codes) are listed as their own informational
        // section below: the frame is correct, a named fast path is off.
        for problem in self.frame_problems().iter().filter(|p| {
            p.code != crate::texture_budget::PROBLEM_TEXTURE_BUDGET
                && p.code != crate::texture_budget::PROBLEM_UNCOMPRESSED_TEXTURES
                && p.code != crate::texture_budget::PROBLEM_WRONG_PLATFORM_TEXTURES
                && !p.code.starts_with(FALLBACK_CODE_PREFIX)
        }) {
            out.push_error("last frame", problem.message.clone(), "no frame problems", false);
        }
        // The map's baked GI volume (`set_baked_gi`): informational, a map may have none.
        let baked = self.baked_gi_status();
        out.info(
            "baked GI volume (installed, lit the last frame, probes, error)",
            (baked.installed, baked.active, baked.probes, baked.error),
        );
        // Device capability fallbacks: one informational line per active fallback so a host
        // on a minimum-spec GPU (or a clamped profile) sees which fast paths are off.
        let fallbacks: Vec<_> = self
            .frame_problems()
            .iter()
            .filter(|p| p.code.starts_with(FALLBACK_CODE_PREFIX))
            .collect();
        out.info(
            "device capability fallbacks (optional features the adapter lacks)",
            if fallbacks.is_empty() {
                "none".to_string()
            } else {
                fallbacks
                    .iter()
                    .map(|p| p.code.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            },
        );
        // The sky irradiance the meshes are lit with (and its ground bounce) is the planet's,
        // at `outdoor.planet.sun_illuminance`; the meshes' sun is `directional`. A host whose
        // two suns differ lights the shaded side of every mesh for a different sun than the
        // sunlit side (no level clock: with one, the renderer derives the light itself).
        if params.time.is_none() && lux > 0.0 {
            let planet_lux = params.planet.sun_illuminance;
            out.push(
                "outdoor.planet.sun_illuminance = directional.illuminance (±5 %)",
                planet_lux,
                lux,
                (planet_lux - lux).abs() <= 0.05 * lux,
            );
        }
        // An airless map's shaded sides get only the ground bounce unless the sky body fills
        // them: DO27's Bore Twelve authors Jupiter-shine (`fill_lux` 1600) that its exported
        // level.ron does not carry (`lookdev dyn_light`: the shaded side 3.8 → 2.4 klx).
        let fill = params.sky_body.and_then(|b| b.fill_lux);
        out.push(
            "outdoor.sky_body on an airless planet (planet-shine fill)",
            params.sky_body.map(|b| (b.direction, b.fill_lux)),
            "Some(SkyBody { fill_lux: Some(lux), .. }) on airless maps with a planet in the sky",
            !params.planet.is_airless() || params.sky_body.is_some_and(|_| fill.is_none_or(|f| f > 0.0)),
        );
        out.info("outdoor.planet (Earth?)", params.is_earth());
        out.info("outdoor.time (level clock)", params.time.map(|t| t.hour));
        out.info(
            "outdoor.sky_body",
            params
                .sky_body
                .map(|b| (b.direction, b.angular_diameter_deg, b.fill_lux)),
        );
        out.info("outdoor.fog (froxel haze density)", params.fog.density);
        out.info("metered scene luminance (cd/m²)", self.metered_luminance());
        out
    }

    /// [`Self::host_checklist_report`] plus the map's own settings (`level`, the
    /// `LevelSettings` the host loaded): the post stack's auto-exposure band is the level
    /// look's (`apply_level_look`), the outdoor sun strength is the level's, and the sky body
    /// the level authors is the one set. Hosts that load DO27's content call this one.
    #[must_use]
    pub fn host_checklist_report_for_level(
        &self,
        scene: &FrameScene<'_>,
        level: &regia_scene::level::LevelSettings,
    ) -> HostChecklist {
        let mut out = self.host_checklist_report(scene);
        let look = &level.look;
        if let Some(post) = scene.post {
            let band = (post.auto_exposure_min_ev100, post.auto_exposure_max_ev100);
            let want = (look.min_ev100.unwrap_or(band.0), look.max_ev100.unwrap_or(band.1));
            out.push(
                "post exposure band = level.look (min, max EV100)",
                band,
                want,
                (band.0 - want.0).abs() < 1e-3 && (band.1 - want.1).abs() < 1e-3,
            );
        }
        let params = self.outdoor_params();
        if let Some(lux) = level.atmosphere.sun_illuminance_lux.filter(|l| *l > 0.0) {
            out.same(
                "outdoor.planet.sun_illuminance = level.atmosphere.sun_illuminance_lux",
                params.planet.sun_illuminance.to_bits() == lux.to_bits(),
                true,
            );
        }
        if level.atmosphere.sky_body.is_some() {
            out.same(
                "outdoor.sky_body = level.atmosphere.sky_body",
                params.sky_body == level.atmosphere.sky_body,
                true,
            );
        }
        out
    }
}
