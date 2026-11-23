// SPDX-License-Identifier: MPL-2.0
//! Post-processing settings (`specs/13` § 5 step 12).

use regia_app::prelude::*;

/// Tonemap operator.
///
/// Reflected so a post volume can override it (`crate::post_volumes`); the type path is pinned
/// so saved levels keep naming it the same way.
#[derive(Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Default, Clone)]
#[type_path = "regia_render::post"]
pub enum TonemapOperator {
    /// AgX (default).
    #[default]
    AgX,
    /// ACES fitted.
    AcesFitted,
    /// Neutral curve.
    Neutral,
}

/// Per-view post-processing (`specs/13` § 9).
#[derive(Debug, Clone, PartialEq)]
pub struct PostSettings {
    /// Master enable for the post stack (goldens can disable for A/B).
    pub enabled: bool,
    /// Bloom threshold and intensity (energy-conserving dual filter when enabled).
    pub bloom: bool,
    /// Bloom mip count.
    pub bloom_mips: u32,
    /// Histogram-based auto exposure.
    pub auto_exposure: bool,
    /// Fixed exposure in EV100 when auto exposure is off.
    pub exposure_ev: f32,
    /// Tonemap operator.
    pub tonemap: TonemapOperator,
    /// FXAA pass after tonemap.
    pub fxaa: bool,
    /// TAA (desktop); jitter must be off for deterministic goldens.
    pub taa: bool,
    /// Vignette strength 0–1.
    pub vignette: f32,
    /// Film grain strength 0–1. Off for deterministic goldens.
    pub film_grain: f32,
    /// Aperture in f-stops when [`Self::use_physical_camera`] is set.
    pub aperture: f32,
    /// Shutter time in seconds when [`Self::use_physical_camera`] is set.
    pub shutter_seconds: f32,
    /// ISO sensitivity when [`Self::use_physical_camera`] is set.
    pub iso: f32,
    /// Derive EV100 from aperture, shutter and ISO instead of [`Self::exposure_ev`].
    pub use_physical_camera: bool,
    /// Bloom blend toward the blurred image (0 = off, 1 = only bloom). The default
    /// 0.04 is an energy-conserving lerp, not an additive glow, so a sunlit frame
    /// does not wash out.
    pub bloom_intensity: f32,
    /// Exposure compensation in EV added on top of auto (or fixed) exposure.
    /// Positive brightens.
    pub exposure_compensation: f32,
    /// Lowest EV100 auto exposure may pick (darkest scene it fully compensates).
    pub auto_exposure_min_ev100: f32,
    /// Highest EV100 auto exposure may pick (brightest scene it fully compensates).
    pub auto_exposure_max_ev100: f32,
    /// Display-space contrast of the default look (S-curve around mid grey; 1 = AgX base).
    pub contrast: f32,
    /// Saturation of the default look around Rec.709 luma (1 = AgX base).
    pub saturation: f32,
    /// White balance and lift / gamma / gain (neutral by default).
    pub grade: ColorGrade,
    /// What the auto-exposure meter averages and where it looks.
    pub meter: ExposureMeter,
    /// 3D LUT applied to the display image after the grade (`None` = identity).
    pub lut: Option<std::sync::Arc<crate::lut::ColorLut>>,
    /// Motion blur on the HDR before exposure (`None` = off, the default). A game sets it through
    /// a camera's [`crate::PhysicalCamera`]; it runs only on frames with the TAA velocity target.
    pub motion_blur: Option<crate::MotionBlur>,
    /// Depth of field on the HDR before exposure (`None` = off, the default). A game sets it
    /// through a camera's [`crate::PhysicalCamera`].
    pub depth_of_field: Option<crate::DepthOfField>,
    /// Local exposure before the tonemap (`None` = off, the default): a level look turns it on
    /// ([`apply_level_look`]).
    pub local_exposure: Option<LocalExposure>,
    /// Scale on the screen-space ambient occlusion strength, 0–4 (1 = unchanged, the default; 0
    /// removes the AO). A post volume sets it (`crate::post_volumes`, ADR-1464).
    pub ao_intensity: f32,
    /// Per-channel multiplier on the view's fog colour (`[1, 1, 1]` = unchanged, the default). A
    /// post volume sets it (`crate::post_volumes`, ADR-1464); see [`Self::tinted_fog`].
    pub fog_tint: [f32; 3],
    /// Ghost flare, lens dirt and chromatic aberration strengths ([`crate::LensEffects`],
    /// ADR-1863); [`crate::LensEffects::OFF`] (the default) records nothing. A camera's
    /// `LensEffects` component replaces it, post volumes override its fields, and the quality
    /// preset must allow the pass ([`crate::RenderProfile::lens_effects`]).
    pub lens_effects: crate::LensEffects,
    /// The auto-exposure band ([`Self::auto_exposure_min_ev100`]..[`Self::auto_exposure_max_ev100`])
    /// bounds the final exposure (ADR-7969). Off (the default) keeps the photographer's order:
    /// the meter is clamped to the band, then [`Self::exposure_compensation`] and the meter's
    /// bright-subject bias ([`ExposureMeter::bright_bias_ev`]) shift the result, so a frame of
    /// sunlit snow can settle under the band floor. On, both shifts come first and the band
    /// clamps last, so the band is the range of EV100 the frame can print at. A level look does
    /// not change it (`apply_level_look` rebuilds the meter, not this switch).
    pub band_bounds_final_exposure: bool,
}

/// Local exposure: an edge-aware tone adjustment of the exposed HDR image before bloom and the
/// tonemap.
///
/// The log luminance is split into a base layer (its edge-preserving blur, read from a
/// bilateral grid: Chen, Paris and Durand, "Real-time edge-aware image processing with the
/// bilateral grid", SIGGRAPH 2007) and the detail on top of it (Durand and Dorsey, "Fast
/// bilateral filtering for the display of high-dynamic-range images", SIGGRAPH 2002). The base
/// is scaled toward middle grey, by [`Self::shadow_contrast`] below it and
/// [`Self::highlight_contrast`] above it; the detail is scaled by [`Self::detail_strength`].
/// With every factor 1 the image is unchanged.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LocalExposure {
    /// Base-layer contrast below middle grey, 0–1 (1 = unchanged; 0.75 lifts a region 4 EV
    /// under middle grey by 1 EV).
    pub shadow_contrast: f32,
    /// Base-layer contrast above middle grey, 0–1 (1 = unchanged).
    pub highlight_contrast: f32,
    /// Detail kept on top of the base layer (1 = unchanged, above 1 sharpens texture).
    pub detail_strength: f32,
}

impl Default for LocalExposure {
    fn default() -> Self {
        Self {
            shadow_contrast: 1.0,
            highlight_contrast: 1.0,
            detail_strength: 1.0,
        }
    }
}

impl LocalExposure {
    /// Factors clamped to their working ranges (non-finite values become 1).
    #[must_use]
    pub fn sanitized(self) -> Self {
        let clamp = |v: f32, hi: f32| if v.is_finite() { v.clamp(0.0, hi) } else { 1.0 };
        Self {
            shadow_contrast: clamp(self.shadow_contrast, 1.0),
            highlight_contrast: clamp(self.highlight_contrast, 1.0),
            detail_strength: clamp(self.detail_strength, 4.0),
        }
    }

    /// Every factor is 1: the pass would not change the image.
    #[must_use]
    pub fn is_identity(&self) -> bool {
        *self == Self::default()
    }
}

/// Colour grade of a view (`specs/13` § 5 step 12).
///
/// White balance scales the scene-referred image per channel before the tonemap (a von
/// Kries adaptation from the Planckian white at [`Self::white_balance_kelvin`] to D65);
/// lift / gamma / gain shape the display image after it:
/// `out = (gain · (x + lift · (1 − x)))^(1 / gamma)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColorGrade {
    /// Colour temperature the camera is balanced for, kelvin; 6500 is neutral.
    pub white_balance_kelvin: f32,
    /// Green–magenta tint, −1..1 (positive is magenta).
    pub tint: f32,
    /// Shadow offset per channel (0 is neutral).
    pub lift: [f32; 3],
    /// Mid-tone power per channel (1 is neutral; above 1 brightens).
    pub gamma: [f32; 3],
    /// Highlight scale per channel (1 is neutral).
    pub gain: [f32; 3],
}

impl Default for ColorGrade {
    fn default() -> Self {
        Self {
            white_balance_kelvin: 6500.0,
            tint: 0.0,
            lift: [0.0; 3],
            gamma: [1.0; 3],
            gain: [1.0; 3],
        }
    }
}

impl ColorGrade {
    /// Per-channel multipliers that map the Planckian white at the grade's temperature
    /// (with its tint) to neutral, normalised to keep luminance. `[1, 1, 1]` at 6500 K
    /// and no tint.
    ///
    /// Planckian chromaticity from Kim et al.'s cubic fit (Kang et al., "Design of advanced
    /// color temperature control system for HDTV applications", J. Korean Phys. Soc. 41, 2002),
    /// xy → XYZ → linear Rec.709.
    #[must_use]
    pub fn white_balance_gains(&self) -> [f32; 3] {
        let white = planckian_rgb(self.white_balance_kelvin);
        let d65 = planckian_rgb(6500.0);
        let tint = self.tint.clamp(-1.0, 1.0);
        let mut gains = [0.0; 3];
        for c in 0..3 {
            gains[c] = d65[c] / white[c].max(1.0e-4);
        }
        // Tint moves along green–magenta: positive removes green.
        gains[1] *= 1.0 - 0.25 * tint;
        let luma = 0.2126 * gains[0] + 0.7152 * gains[1] + 0.0722 * gains[2];
        gains.map(|g| g / luma.max(1.0e-4))
    }

    /// No change to the image.
    #[must_use]
    pub fn is_neutral(&self) -> bool {
        *self == Self::default()
    }
}

/// Linear Rec.709 colour of a Planckian radiator at `kelvin`, unit Y. The published fit
/// coefficients are evaluated in `f64`.
fn planckian_rgb(kelvin: f32) -> [f32; 3] {
    let t = f64::from(kelvin.clamp(1667.0, 25000.0));
    let (t2, t3) = (t * t, t * t * t);
    let x = if t <= 4000.0 {
        -0.266_123_9e9 / t3 - 0.234_358_0e6 / t2 + 0.877_695_6e3 / t + 0.179_910
    } else {
        -3.025_846_9e9 / t3 + 2.107_037_9e6 / t2 + 0.222_634_7e3 / t + 0.240_390
    };
    let (x2, x3) = (x * x, x * x * x);
    let y = if t <= 2222.0 {
        -1.106_381_4 * x3 - 1.348_110_20 * x2 + 2.185_558_32 * x - 0.202_196_83
    } else if t <= 4000.0 {
        -0.954_947_6 * x3 - 1.374_185_93 * x2 + 2.091_370_15 * x - 0.167_488_67
    } else {
        3.081_758_0 * x3 - 5.873_386_70 * x2 + 3.751_129_97 * x - 0.370_014_83
    };
    let (cx, cy, cz) = (x / y, 1.0, (1.0 - x - y) / y);
    [
        (3.240_454_2 * cx - 1.537_138_5 * cy - 0.498_531_4 * cz) as f32,
        (-0.969_266_0 * cx + 1.876_010_8 * cy + 0.041_556_0 * cz) as f32,
        (0.055_643_4 * cx - 0.204_025_9 * cy + 1.057_225_2 * cz) as f32,
    ]
}

/// Estimated correlated colour temperature (kelvin) of a linear Rec.709 colour: the
/// temperature of the Planckian radiator (`planckian_rgb`) whose unit-luma colour is closest
/// to `rgb`'s. Used to report the temperature of indirect "shadow" light — a blue sky-lit
/// shadow reads hot (≈ 25 000 K), a turbid-sky shadow reads lower. The estimate is clamped to
/// the fit's 1667–25 000 K range and assumes the colour sits near the Planckian locus.
#[must_use]
pub fn correlated_color_temperature(rgb: [f32; 3]) -> f32 {
    let y = 0.2126 * rgb[0] + 0.7152 * rgb[1] + 0.0722 * rgb[2];
    let target = [
        rgb[0] / y.max(1.0e-4),
        rgb[1] / y.max(1.0e-4),
        rgb[2] / y.max(1.0e-4),
    ];
    let distance = |k: f32| {
        let p = planckian_rgb(k);
        let d = [p[0] - target[0], p[1] - target[1], p[2] - target[2]];
        d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
    };
    // Ternary search over the fit's clamp range; the distance to the Planckian locus is
    // unimodal in temperature for colours near the locus (sky and shadow light).
    let (mut lo, mut hi) = (1667.0_f32, 25_000.0_f32);
    for _ in 0..48 {
        let m1 = lo + (hi - lo) / 3.0;
        let m2 = hi - (hi - lo) / 3.0;
        if distance(m1) < distance(m2) {
            hi = m2;
        } else {
            lo = m1;
        }
    }
    (lo + hi) * 0.5
}

/// The auto-exposure meter (percentile-trimmed log average, `readback`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExposureMeter {
    /// Fraction of the darkest pixels ignored (0–1).
    pub low_percentile: f32,
    /// Fraction of pixels from the dark end above which the brightest are ignored (0–1).
    pub high_percentile: f32,
    /// 0 weights every pixel the same; 1 weights the frame centre up to 9× (a Gaussian
    /// over the middle third), the way a centre-weighted camera meter reads a subject.
    pub center_weight: f32,
    /// Bright-subject compensation threshold, cd/m²: a metered pixel at or above it (and
    /// below 16× it, which leaves out emissive fire, the sun and glints) counts as bright
    /// subject — sunlit snow or ice. `0` turns the compensation off.
    pub bright_threshold: f32,
    /// EV added (brighter) when bright subject fills the metered frame, scaled by
    /// [`bright_coverage_weight`] of its coverage: the photographer's +1 EV for snow,
    /// applied only as far as the frame is snow, so a cockpit or a smoke plume keeps the
    /// plain meter.
    pub bright_bias_ev: f32,
}

/// How much of [`ExposureMeter::bright_bias_ev`] a frame gets for a bright-subject
/// `coverage` (fraction of the metered pixels): none below 25 %, all above 60 %,
/// smoothstep between.
#[must_use]
pub fn bright_coverage_weight(coverage: f32) -> f32 {
    let t = ((coverage - 0.25) / (0.60 - 0.25)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl Default for ExposureMeter {
    fn default() -> Self {
        Self {
            low_percentile: crate::readback::METER_LOW_PERCENTILE as f32,
            high_percentile: crate::readback::METER_HIGH_PERCENTILE as f32,
            center_weight: 0.0,
            bright_threshold: 0.0,
            bright_bias_ev: 0.0,
        }
    }
}

/// Applies a map's [`LevelLook`](regia_scene::level::LevelLook) to a view's post
/// settings: the bias adds to the compensation; every field the level sets replaces
/// the view's, every `None` keeps it.
pub fn apply_level_look(post: &mut PostSettings, look: &regia_scene::level::LevelLook) {
    post.exposure_compensation += look.exposure_bias_ev;
    if let Some(ev) = look.min_ev100 {
        post.auto_exposure_min_ev100 = ev;
    }
    if let Some(ev) = look.max_ev100 {
        post.auto_exposure_max_ev100 = ev;
    }
    // A profile without auto exposure (Mobile Low) holds the fixed `exposure_ev`: the map's
    // band centre, so a map tuned with a band does not print at the engine default.
    if let (Some(lo), Some(hi)) = (look.min_ev100, look.max_ev100) {
        post.exposure_ev = 0.5 * (lo + hi);
    }
    if let Some(meter) = look.meter {
        post.meter = ExposureMeter {
            low_percentile: meter.low_percentile.clamp(0.0, 1.0),
            high_percentile: meter.high_percentile.clamp(0.0, 1.0),
            center_weight: meter.center_weight.clamp(0.0, 1.0),
            bright_threshold: if meter.bright_threshold_cd_m2.is_finite() {
                meter.bright_threshold_cd_m2.max(0.0)
            } else {
                0.0
            },
            bright_bias_ev: if meter.bright_bias_ev.is_finite() {
                meter.bright_bias_ev.clamp(-4.0, 4.0)
            } else {
                0.0
            },
        };
    }
    if let Some(contrast) = look.contrast {
        post.contrast = contrast.max(0.05);
    }
    if let Some(saturation) = look.saturation {
        post.saturation = saturation.max(0.0);
    }
    if let Some(local) = look.local_exposure {
        post.local_exposure = Some(
            LocalExposure {
                shadow_contrast: local.shadow_contrast,
                highlight_contrast: local.highlight_contrast,
                detail_strength: local.detail_strength,
            }
            .sanitized(),
        );
    }
    if let Some(grade) = look.grade {
        post.grade = ColorGrade {
            white_balance_kelvin: grade.white_balance_kelvin,
            tint: grade.tint,
            lift: grade.lift,
            gamma: grade.gamma.map(|g| g.max(0.05)),
            gain: grade.gain.map(|g| g.max(0.0)),
        };
    }
}

/// The default look's contrast. Mirrored as `DEFAULT_LOOK_CONTRAST` in `utils.wgsl`
/// and the terrain shaders (the no-post paths).
pub const DEFAULT_LOOK_CONTRAST: f32 = 1.2;
/// The default look's saturation. Mirrored as `DEFAULT_LOOK_SATURATION`.
pub const DEFAULT_LOOK_SATURATION: f32 = 1.4;
/// Default bloom blend (see [`PostSettings::bloom_intensity`]).
pub const DEFAULT_BLOOM_INTENSITY: f32 = 0.04;

impl Default for PostSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            bloom: true,
            bloom_mips: 5,
            auto_exposure: true,
            exposure_ev: 0.0,
            tonemap: TonemapOperator::AgX,
            fxaa: false,
            taa: false,
            vignette: 0.0,
            film_grain: 0.0,
            aperture: 16.0,
            shutter_seconds: 1.0 / 125.0,
            iso: 100.0,
            use_physical_camera: false,
            bloom_intensity: DEFAULT_BLOOM_INTENSITY,
            exposure_compensation: 0.0,
            auto_exposure_min_ev100: -2.0,
            auto_exposure_max_ev100: 15.0,
            contrast: DEFAULT_LOOK_CONTRAST,
            saturation: DEFAULT_LOOK_SATURATION,
            grade: ColorGrade::default(),
            meter: ExposureMeter::default(),
            lut: None,
            motion_blur: None,
            depth_of_field: None,
            local_exposure: None,
            ao_intensity: 1.0,
            fog_tint: [1.0; 3],
            lens_effects: crate::LensEffects::OFF,
            band_bounds_final_exposure: false,
        }
    }
}

/// EV100 from a physical camera (`N` aperture, `t` shutter seconds, `iso`).
///
/// `EV100 = log2(N² / t) − log2(ISO / 100)`, ISO 2720 as used by Filament's public
/// exposure notes.
#[must_use]
pub fn ev100_from_camera(aperture: f32, shutter_seconds: f32, iso: f32) -> f32 {
    let n = aperture.max(0.1);
    let t = shutter_seconds.max(1.0e-6);
    let s = iso.max(1.0);
    (n * n / t).log2() - (s / 100.0).log2()
}

/// Linear exposure multiplier for an EV100 value: `1 / (1.2 · 2^EV100)`.
#[must_use]
pub fn exposure_from_ev100(ev100: f32) -> f32 {
    1.0 / (1.2 * ev100.exp2())
}

/// Pre-exposure of the linear HDR target for a frame whose post stack multiplies by
/// `exposure`: the power of two nearest to it, at most 1.
///
/// Every pass that writes scene radiance into the HDR target multiplies it by this scale,
/// and every pass that reads it back as radiance divides it out (the post exposure, the
/// meter, the HDR capture). Half floats then hold up to 65504 × the display white instead
/// of 65504 cd/m², so the physical sun (~1.6·10⁹ cd/m²) and its specular highlights no
/// longer clip. A power of two scales every finite half exactly, so a frame in which
/// nothing clipped and nothing fell below the half-float normal range is unchanged bit for
/// bit, and an exposure near 1 (EV100 ≤ 0.2, the lab frames) gets exactly 1.
#[must_use]
pub fn pre_exposure_scale(exposure: f32) -> f32 {
    if !(exposure.is_finite() && exposure > 0.0) {
        return 1.0;
    }
    exposure.log2().round().clamp(-60.0, 0.0).exp2()
}

/// EV100 whose [`exposure_from_ev100`] maps `average_luminance` (cd/m²) to middle
/// grey (0.18): `log2(L / 0.216)` — the reflected-light meter equation
/// `EV100 = log2(L · S / K)` with `S = 100` and `K ≈ 21.6`.
#[must_use]
pub fn ev100_from_luminance(average_luminance: f32) -> f32 {
    (average_luminance.max(1.0e-6) / (0.18 * 1.2)).log2()
}

/// Middle-grey auto exposure from an average scene luminance.
#[must_use]
pub fn exposure_from_luminance(average_luminance: f32) -> f32 {
    0.18 / average_luminance.max(1.0e-4)
}

impl PostSettings {
    /// Deterministic settings for golden-image tests (`specs/13` § 12).
    #[must_use]
    pub fn golden_deterministic() -> Self {
        Self {
            enabled: true,
            bloom: false,
            bloom_mips: 0,
            auto_exposure: false,
            exposure_ev: 0.0,
            tonemap: TonemapOperator::AgX,
            fxaa: false,
            taa: false,
            vignette: 0.0,
            film_grain: 0.0,
            aperture: 16.0,
            shutter_seconds: 1.0 / 125.0,
            iso: 100.0,
            use_physical_camera: false,
            bloom_intensity: DEFAULT_BLOOM_INTENSITY,
            exposure_compensation: 0.0,
            auto_exposure_min_ev100: -2.0,
            auto_exposure_max_ev100: 15.0,
            contrast: DEFAULT_LOOK_CONTRAST,
            saturation: DEFAULT_LOOK_SATURATION,
            grade: ColorGrade::default(),
            meter: ExposureMeter::default(),
            lut: None,
            motion_blur: None,
            depth_of_field: None,
            local_exposure: None,
            ao_intensity: 1.0,
            fog_tint: [1.0; 3],
            lens_effects: crate::LensEffects::OFF,
            band_bounds_final_exposure: false,
        }
    }

    /// The view's fog with [`Self::fog_tint`] applied to its colour. The neutral tint returns
    /// `fog` unchanged (bit for bit); a tint channel that is not finite or negative counts as 1.
    #[must_use]
    pub fn tinted_fog(&self, fog: crate::FogSettings) -> crate::FogSettings {
        if self.fog_tint == [1.0; 3] {
            return fog;
        }
        let tint = self
            .fog_tint
            .map(|t| if t.is_finite() && t >= 0.0 { t } else { 1.0 });
        crate::FogSettings {
            color: [
                fog.color[0] * tint[0],
                fog.color[1] * tint[1],
                fog.color[2] * tint[2],
            ],
            ..fog
        }
    }

    /// [`Self::ao_intensity`] clamped to 0–4 (not finite counts as 1): the factor the SSAO pass
    /// multiplies its strength by.
    #[must_use]
    pub fn ao_intensity_scale(&self) -> f32 {
        if self.ao_intensity.is_finite() {
            self.ao_intensity.clamp(0.0, 4.0)
        } else {
            1.0
        }
    }

    /// Linear exposure multiplier from EV100 or the physical camera, ignoring auto exposure.
    /// [`Self::exposure_compensation`] applies.
    #[must_use]
    pub fn exposure_scale(&self) -> f32 {
        let ev = if self.use_physical_camera {
            ev100_from_camera(self.aperture, self.shutter_seconds, self.iso)
        } else {
            self.exposure_ev
        };
        exposure_from_ev100(ev - self.exposure_compensation)
    }

    /// EV100 auto exposure uses before its first histogram sample (two frames after a
    /// renderer starts): [`Self::exposure_ev`] when the game set one, otherwise the
    /// bright end of the range ([`Self::auto_exposure_max_ev100`], a sunny day). The
    /// default `exposure_ev` of 0 used to apply as-is, printing the first frames of an
    /// outdoor scene pure white.
    ///
    /// With [`Self::band_bounds_final_exposure`] the result is clamped to the band as well.
    #[must_use]
    pub fn auto_exposure_initial_ev100(&self) -> f32 {
        let ev = if self.exposure_ev != 0.0 {
            self.exposure_ev
        } else {
            self.auto_exposure_max_ev100
        };
        let ev = ev - self.exposure_compensation;
        if self.band_bounds_final_exposure {
            let (lo, hi) = self.auto_exposure_band();
            ev.clamp(lo, hi)
        } else {
            ev
        }
    }

    /// The auto-exposure band, ordered: `(min, max)` of [`Self::auto_exposure_min_ev100`] and
    /// [`Self::auto_exposure_max_ev100`] (a crossed pair is read the other way round).
    #[must_use]
    pub fn auto_exposure_band(&self) -> (f32, f32) {
        (
            self.auto_exposure_min_ev100.min(self.auto_exposure_max_ev100),
            self.auto_exposure_max_ev100.max(self.auto_exposure_min_ev100),
        )
    }

    /// EV100 auto exposure picks for a metered average luminance (cd/m²): keyed so
    /// the average lands on middle grey, clamped to
    /// [`Self::auto_exposure_min_ev100`]..[`Self::auto_exposure_max_ev100`], then
    /// [`Self::exposure_compensation`] subtracted. With
    /// [`Self::band_bounds_final_exposure`] the compensation is subtracted first and the
    /// band clamps last, so the result never leaves the band.
    ///
    /// The clamp is what keeps a snowfield white: a frame of sunlit snow meters
    /// above EV100 16, and an unclamped meter would print it middle grey. The
    /// default ceiling (EV100 15, the "sunny 16" exposure) is what a photographer
    /// uses for any subject in full sun.
    #[must_use]
    pub fn auto_exposure_ev100(&self, average_luminance: f32) -> f32 {
        self.auto_exposure_ev100_with_coverage(average_luminance, 0.0)
    }

    /// [`Self::auto_exposure_ev100`] for a frame whose metered pixels are `bright_coverage`
    /// bright subject: the meter's [`ExposureMeter::bright_bias_ev`] is subtracted in
    /// proportion to [`bright_coverage_weight`] (nothing when the threshold is 0).
    ///
    /// The order follows [`Self::band_bounds_final_exposure`]: off (the default),
    /// `clamp(metered, band) - compensation - bias`; on, `clamp(metered - compensation -
    /// bias, band)`. When the meter and the shifted value both lie inside the band, the two agree bit
    /// for bit.
    #[must_use]
    pub fn auto_exposure_ev100_with_coverage(&self, average_luminance: f32, bright_coverage: f32) -> f32 {
        let (lo, hi) = self.auto_exposure_band();
        let bright = if self.meter.bright_threshold > 0.0 {
            self.meter.bright_bias_ev * bright_coverage_weight(bright_coverage)
        } else {
            0.0
        };
        let metered = ev100_from_luminance(average_luminance);
        if self.band_bounds_final_exposure {
            (metered - self.exposure_compensation - bright).clamp(lo, hi)
        } else {
            metered.clamp(lo, hi) - self.exposure_compensation - bright
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bright_compensation_scales_with_coverage() {
        let mut post = PostSettings {
            auto_exposure_min_ev100: 8.0,
            auto_exposure_max_ev100: 14.0,
            ..PostSettings::default()
        };
        let plain = post.auto_exposure_ev100(500.0);
        // Off (threshold 0): coverage changes nothing.
        assert_eq!(post.auto_exposure_ev100_with_coverage(500.0, 0.9), plain);
        post.meter.bright_threshold = 400.0;
        post.meter.bright_bias_ev = 1.0;
        // A cockpit or smoke frame (little bright subject) keeps the plain meter; a snowfield
        // gets the whole +1 EV; half coverage gets part of it.
        assert_eq!(post.auto_exposure_ev100_with_coverage(500.0, 0.1), plain);
        assert!((post.auto_exposure_ev100_with_coverage(500.0, 0.8) - (plain - 1.0)).abs() < 1e-5);
        let half = post.auto_exposure_ev100_with_coverage(500.0, 0.425);
        assert!((half - (plain - 0.5)).abs() < 1e-5, "{half} vs {plain}");
        assert_eq!(bright_coverage_weight(0.0), 0.0);
        assert_eq!(bright_coverage_weight(1.0), 1.0);

        // ADR-7969: a full-snow frame that meters exactly at the band floor (EV100 8).
        let floor_luminance = 0.18 * 1.2 * 8.0_f32.exp2();
        assert!(!post.band_bounds_final_exposure, "off by default");
        // Switch off (today's order): the +1 EV comes after the clamp, 1 EV under the floor.
        let off = post.auto_exposure_ev100_with_coverage(floor_luminance, 0.8);
        assert!((off - 7.0).abs() < 1e-4, "switch off: {off}");
        // Switch on: the band bounds the final exposure, so the frame stays on the floor.
        post.band_bounds_final_exposure = true;
        let on = post.auto_exposure_ev100_with_coverage(floor_luminance, 0.8);
        assert!((on - 8.0).abs() < 1e-4, "switch on: {on}");
        // Switch on: compensation alone cannot push it out of the band either.
        post.meter.bright_threshold = 0.0;
        post.exposure_compensation = 1.0;
        let compensated = post.auto_exposure_ev100_with_coverage(floor_luminance, 0.8);
        assert!((compensated - 8.0).abs() < 1e-4, "compensation: {compensated}");
        post.exposure_compensation = -3.0;
        let ceiling = post.auto_exposure_ev100(0.18 * 1.2 * 13.0_f32.exp2());
        assert!((ceiling - 14.0).abs() < 1e-4, "negative compensation: {ceiling}");
        // Inside the band the switch changes nothing: 10 + 3 = 13 either way.
        let inside = post.auto_exposure_ev100(0.18 * 1.2 * 10.0_f32.exp2());
        post.band_bounds_final_exposure = false;
        assert_eq!(inside, post.auto_exposure_ev100(0.18 * 1.2 * 10.0_f32.exp2()));
    }

    /// The DO27 Europa repro (ADR-7969): band 10.5–11.5, bright threshold 250 cd/m², +1 EV snow
    /// bias and +1 EV compensation on a frame of sunlit ice that meters above the band.
    #[test]
    fn band_bounds_final_exposure_keeps_sunlit_ice_in_the_band() {
        let mut post = PostSettings {
            auto_exposure_min_ev100: 10.5,
            auto_exposure_max_ev100: 11.5,
            exposure_compensation: 1.0,
            ..PostSettings::default()
        };
        post.meter.bright_threshold = 250.0;
        post.meter.bright_bias_ev = 1.0;
        for metered in [11.5_f32, 12.0, 12.5, 14.0, 17.0] {
            let luminance = 0.18 * 1.2 * metered.exp2();
            post.band_bounds_final_exposure = false;
            let off = post.auto_exposure_ev100_with_coverage(luminance, 1.0);
            assert!((off - 9.5).abs() < 1e-4, "EV {metered} off: {off} (today's 9.5)");
            post.band_bounds_final_exposure = true;
            let on = post.auto_exposure_ev100_with_coverage(luminance, 1.0);
            let want = (metered - 2.0).clamp(10.5, 11.5);
            assert!((on - want).abs() < 1e-4, "EV {metered} on: {on} vs {want}");
            assert!((10.5..=11.5).contains(&on), "EV {metered}: {on} left the band");
        }
        // Before the first histogram the held EV is in the band too.
        post.band_bounds_final_exposure = false;
        assert!((post.auto_exposure_initial_ev100() - 10.5).abs() < 1e-4);
        post.exposure_compensation = 2.0;
        assert!((post.auto_exposure_initial_ev100() - 9.5).abs() < 1e-4);
        post.band_bounds_final_exposure = true;
        assert!((post.auto_exposure_initial_ev100() - 10.5).abs() < 1e-4);
    }

    /// Applying a level look rebuilds the meter wholesale; the switch lives on `PostSettings`
    /// and keeps its value either way (ADR-7969: no level-file field until the v0.6 format bump).
    #[test]
    fn a_level_look_keeps_the_band_switch() {
        use regia_scene::level::{LevelLook, LevelMeter};
        for switch in [false, true] {
            let mut post = PostSettings {
                band_bounds_final_exposure: switch,
                ..PostSettings::default()
            };
            let look = LevelLook {
                min_ev100: Some(10.5),
                max_ev100: Some(11.5),
                exposure_bias_ev: 1.0,
                meter: Some(LevelMeter {
                    bright_threshold_cd_m2: 250.0,
                    bright_bias_ev: 1.0,
                    ..LevelMeter::default()
                }),
                ..LevelLook::default()
            };
            apply_level_look(&mut post, &look);
            apply_level_look(&mut post, &LevelLook::default());
            assert_eq!(post.band_bounds_final_exposure, switch);
            assert_eq!(post.meter.bright_bias_ev, 1.0);
        }
    }

    #[test]
    fn pre_exposure_is_a_power_of_two_near_the_exposure() {
        // Lab frames (EV100 0) keep an exact 1: their HDR target is unchanged.
        assert_eq!(pre_exposure_scale(exposure_from_ev100(0.0)), 1.0);
        assert_eq!(pre_exposure_scale(4.0), 1.0);
        assert_eq!(pre_exposure_scale(f32::NAN), 1.0);
        assert_eq!(pre_exposure_scale(0.0), 1.0);
        for ev in [3.0f32, 9.5, 14.0, 15.0, 17.0] {
            let exposure = exposure_from_ev100(ev);
            let p = pre_exposure_scale(exposure);
            assert_eq!(p.log2().fract(), 0.0, "EV {ev}: {p} is a power of two");
            assert!(
                (p / exposure).log2().abs() <= 0.5 + 1e-4,
                "EV {ev}: {p} vs {exposure}"
            );
        }
        // The physical sun at a sunny exposure fits a half float once pre-exposed.
        let sun = 1.6e9 * pre_exposure_scale(exposure_from_ev100(15.0));
        assert!(sun < 65504.0, "{sun}");
    }

    #[test]
    fn sunny_16_is_near_ev100_15() {
        let ev = ev100_from_camera(16.0, 1.0 / 125.0, 100.0);
        assert!((ev - 15.0).abs() < 0.15, "ev {ev}");
        let exposure = exposure_from_ev100(0.0);
        assert!((exposure - 1.0 / 1.2).abs() < 1.0e-5);
        let mut camera = PostSettings {
            use_physical_camera: true,
            ..PostSettings::default()
        };
        let sunny = camera.exposure_scale();
        camera.use_physical_camera = false;
        camera.exposure_ev = 0.0;
        assert!(sunny < camera.exposure_scale());
    }

    #[test]
    fn white_balance_is_neutral_at_d65_and_warms_below_it() {
        let neutral = ColorGrade::default().white_balance_gains();
        for g in neutral {
            assert!((g - 1.0).abs() < 1.0e-3, "{neutral:?}");
        }
        // Balanced for a warm 3200 K lamp: the image is corrected toward blue.
        let tungsten = ColorGrade {
            white_balance_kelvin: 3200.0,
            ..ColorGrade::default()
        }
        .white_balance_gains();
        assert!(tungsten[2] > tungsten[0] * 1.5, "{tungsten:?}");
        // Balanced for 9000 K shade: corrected toward red (a warmer print).
        let shade = ColorGrade {
            white_balance_kelvin: 9000.0,
            ..ColorGrade::default()
        }
        .white_balance_gains();
        assert!(shade[0] > shade[2], "{shade:?}");
        let luma = 0.2126 * shade[0] + 0.7152 * shade[1] + 0.0722 * shade[2];
        assert!((luma - 1.0).abs() < 1.0e-3, "luminance kept, {luma}");
    }

    #[test]
    fn correlated_color_temperature_inverts_the_planckian_fit() {
        for &k in &[2000.0_f32, 5000.0, 6500.0, 10_000.0, 20_000.0] {
            let roundtrip = correlated_color_temperature(planckian_rgb(k));
            assert!(
                (roundtrip - k).abs() / k < 0.05,
                "{k} K read back as {roundtrip} K"
            );
        }
        // A blue cast reads hotter than a warm cast at the same luminance.
        let blue = correlated_color_temperature([0.7, 0.85, 1.4]);
        let warm = correlated_color_temperature([1.4, 0.85, 0.6]);
        assert!(blue > warm + 2000.0, "blue {blue} K vs warm {warm} K");
    }

    #[test]
    fn a_level_look_turns_local_exposure_on_clamped() {
        use regia_scene::level::{LevelLocalExposure, LevelLook};
        let mut post = PostSettings::default();
        apply_level_look(&mut post, &LevelLook::default());
        assert_eq!(post.local_exposure, None, "off unless the level asks");
        let look = LevelLook {
            local_exposure: Some(LevelLocalExposure {
                shadow_contrast: 0.75,
                highlight_contrast: 1.5,
                detail_strength: f32::NAN,
            }),
            ..LevelLook::default()
        };
        apply_level_look(&mut post, &look);
        assert_eq!(
            post.local_exposure,
            Some(LocalExposure {
                shadow_contrast: 0.75,
                highlight_contrast: 1.0,
                detail_strength: 1.0,
            })
        );
        assert!(LocalExposure::default().is_identity());
    }

    #[test]
    fn a_level_look_overrides_only_what_it_sets() {
        use regia_scene::level::{LevelGrade, LevelLook, LevelMeter};
        let mut post = PostSettings {
            contrast: 1.0,
            exposure_compensation: 0.25,
            ..PostSettings::default()
        };
        apply_level_look(&mut post, &LevelLook::default());
        assert_eq!(post.contrast, 1.0, "a silent level keeps the view's look");
        assert_eq!(post.exposure_compensation, 0.25);
        let look = LevelLook {
            exposure_bias_ev: 0.5,
            max_ev100: Some(14.0),
            meter: Some(LevelMeter {
                center_weight: 0.7,
                ..LevelMeter::default()
            }),
            saturation: Some(1.1),
            grade: Some(LevelGrade {
                white_balance_kelvin: 5600.0,
                ..LevelGrade::default()
            }),
            ..LevelLook::default()
        };
        apply_level_look(&mut post, &look);
        assert_eq!(post.exposure_compensation, 0.75);
        assert_eq!(post.auto_exposure_max_ev100, 14.0);
        assert_eq!(post.auto_exposure_min_ev100, -2.0);
        assert_eq!(post.meter.center_weight, 0.7);
        assert_eq!(post.saturation, 1.1);
        assert_eq!(post.contrast, 1.0);
        assert_eq!(post.grade.white_balance_kelvin, 5600.0);
        let fixed_ev = post.exposure_ev;
        assert_eq!(
            fixed_ev,
            PostSettings::default().exposure_ev,
            "a one-sided band keeps the fixed EV"
        );
    }

    #[test]
    fn a_level_exposure_band_also_sets_the_fixed_exposure() {
        use regia_scene::level::LevelLook;
        // Mobile Low runs without auto exposure: before, a DO27 map tuned with an EV100 band
        // printed at the engine's default EV, blown to white.
        let mut post = PostSettings {
            auto_exposure: false,
            ..PostSettings::default()
        };
        let before = post.exposure_scale();
        apply_level_look(
            &mut post,
            &LevelLook {
                min_ev100: Some(11.1),
                max_ev100: Some(11.3),
                ..LevelLook::default()
            },
        );
        assert!((post.exposure_ev - 11.2).abs() < 1.0e-5, "{}", post.exposure_ev);
        assert!((post.exposure_scale() - exposure_from_ev100(11.2)).abs() < 1.0e-9);
        assert_ne!(post.exposure_scale(), before);
    }

    #[test]
    fn auto_exposure_keys_middle_grey_and_clamps_daylight() {
        let post = PostSettings::default();
        // A dim interior (40 cd/m² average) is fully compensated: its average lands on 0.18.
        let ev = post.auto_exposure_ev100(40.0);
        assert!((exposure_from_ev100(ev) * 40.0 - 0.18).abs() < 1.0e-3, "ev {ev}");
        // Sunlit snow (~12 000 cd/m²) stops at the sunny-16 ceiling, so it stays bright.
        let snow = post.auto_exposure_ev100(12_000.0);
        assert!((snow - 15.0).abs() < 1.0e-4, "snow ev {snow}");
        assert!(
            exposure_from_ev100(snow) * 12_000.0 > 0.25,
            "snow must print brighter than gray"
        );
        // Compensation brightens.
        let brighter = PostSettings {
            exposure_compensation: 1.0,
            ..PostSettings::default()
        };
        assert!((brighter.auto_exposure_ev100(40.0) - (ev - 1.0)).abs() < 1.0e-4);
        assert_eq!(ev100_from_luminance(0.216).round(), 0.0);
    }
}
