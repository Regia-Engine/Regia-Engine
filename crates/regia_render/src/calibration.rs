// SPDX-License-Identifier: MPL-2.0
//! Player display calibration: SDR black level and gamma, HDR paper white / peak / minimum
//! luminance (ADR-092, book `user-guide/display-calibration.md`).
//!
//! Calibration is a **display stage**: it runs on the finished frame (scene, VFX and UI) just
//! before present, so the whole picture is calibrated the way a TV's picture settings would be,
//! and the calibration screen's test patterns go through exactly the curve they measure.
//! [`DisplayCalibration::NEUTRAL`] is the identity: with it (and an SDR surface) the stage does
//! not run at all, so goldens and neutral screenshots are untouched by construction.
//!
//! ## The SDR curve
//!
//! Input and output are display-encoded values (`0..=1`, the sRGB-encoded image the tonemap and
//! UI produce). Two controls, in this order:
//!
//! 1. **Gamma** `g` (1.8–2.6, neutral 2.2): `v' = v^(g / 2.2)` — the image as it would look on a
//!    display decoding with gamma `g` instead of 2.2. Higher is darker mid-tones.
//! 2. **Black level** `s` (−1..1, neutral 0): a shadow curve on the lower half of the range.
//!    For `v < 0.5`, with `x = 2v` and `a = 2.5 s` (lift) or `0.9 s` (deepen), the display
//!    value is `v' = ½(x + a·x·(1 − x)²)`, equivalently `v' = v + a·v·(1 − 2v)²`. It keeps
//!    black at 0, mid grey (0.5) and everything above it unchanged, and meets the identity at
//!    mid grey with slope 1 (C¹), so it is a curve, not a flat gain. The slope at black is
//!    `1 + a` (up to 3.5×). At full lift, code 3 (`v = 3/255`) rises by about 3.38×. That is
//!    what makes the "barely visible" logo appear on a display that crushes its first code values.
//!
//! ## HDR
//!
//! On an HDR10 or scRGB surface the frame is rendered linear in **paper-white units** (1.0 =
//! [`DisplayCalibration::paper_white_nits`]); the post blit expands the scene's highlights above
//! [`HDR_HIGHLIGHT_KNEE`] up to [`DisplayCalibration::hdr_headroom`] (`peak / paper`) with a C¹
//! quadratic ([`hdr_highlight_expand`]), the UI stays at paper white. The display stage applies
//! the black-level curve, scales to nits, raises black to [`DisplayCalibration::min_nits`]
//! (`n' = min + n·(1 − min / peak)`), converts Rec.709 → Rec.2020 (ITU-R BT.2087) and encodes
//! SMPTE ST 2084 (PQ), or divides by 80 for scRGB (IEC 61966-2-2).
//!
//! ## Display simulation
//!
//! [`DisplayModel`] is a gain-offset-gamma display (Berns 1996) with a crush threshold and
//! reflected ambient light (the display model of Mantiuk, Daly & Kerofsky, "Display Adaptive
//! Tone Mapping", SIGGRAPH 2008). It powers the evidence sheet, the tests, and
//! [`DisplayModel::solve_black_level`], which emulates a player doing the logo test.

use regia_app::prelude::*;

pub use regia_platform::DisplayLuminance;

use crate::profile::RenderProfilePreset;

/// Gamma the content is mastered for; the neutral [`DisplayCalibration::gamma`].
pub const NEUTRAL_GAMMA: f32 = 2.2;
/// Player gamma range.
pub const GAMMA_MIN: f32 = 1.8;
/// Player gamma range.
pub const GAMMA_MAX: f32 = 2.6;
/// Shadow-gain coefficient `a` at black level +1 (slope at black `1 + a`).
pub const BLACK_LIFT_MAX: f32 = 2.5;
/// Shadow-gain coefficient magnitude at black level −1 (slope at black `1 − 0.9`).
pub const BLACK_DEEPEN_MAX: f32 = 0.9;
/// Default HDR paper white: ITU-R BT.2408 reference white for graphics (203 cd/m²).
pub const DEFAULT_PAPER_WHITE_NITS: f32 = 203.0;
/// Default HDR peak when the display does not report one.
pub const DEFAULT_PEAK_NITS: f32 = 1000.0;
/// Display-linear value (paper-white units) above which HDR expands highlights.
pub const HDR_HIGHLIGHT_KNEE: f32 = 0.5;
/// Paper-white range offered to players (cd/m²).
pub const PAPER_WHITE_RANGE: (f32, f32) = (80.0, 500.0);
/// Peak range offered to players (cd/m²).
pub const PEAK_RANGE: (f32, f32) = (200.0, 10_000.0);
/// Minimum-luminance range offered to players (cd/m²).
pub const MIN_NITS_RANGE: (f32, f32) = (0.0, 1.0);
/// Signal of the "barely visible" logo in the black-level test (≈ 9/255).
pub const LOGO_VISIBLE_SIGNAL: f32 = 0.035;
/// Signal of the "hidden" logo in the black-level test (≈ 3/255).
pub const LOGO_HIDDEN_SIGNAL: f32 = 0.012;

/// Player display calibration, applied by the display stage after the tonemap, VFX and UI.
///
/// Persisted by the framework with the graphics settings (`graphics.calibration`). Insert or
/// change the resource to change the picture; [`Self::NEUTRAL`] is the identity.
///
/// ```
/// use regia_render::calibration::DisplayCalibration;
///
/// let neutral = DisplayCalibration::NEUTRAL;
/// assert_eq!(neutral.apply_sdr([0.1, 0.5, 0.9]), [0.1, 0.5, 0.9]);
/// let lifted = DisplayCalibration { black_level: 0.5, ..neutral };
/// assert!(lifted.apply_sdr([0.05; 3])[0] > 0.05, "shadows lift");
/// assert_eq!(lifted.apply_sdr([0.5; 3]), [0.5; 3], "mid grey stays");
/// ```
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct DisplayCalibration {
    /// Black level, −1..1 (0 neutral). Positive lifts shadows, negative deepens them; black,
    /// mid grey and white never move.
    pub black_level: f32,
    /// Display gamma 1.8–2.6 (2.2 neutral). SDR only. Higher is darker mid-tones.
    pub gamma: f32,
    /// HDR: luminance of UI and diffuse white, cd/m².
    pub paper_white_nits: f32,
    /// HDR: brightest luminance the display shows, cd/m² (from the display when it reports one).
    pub peak_nits: f32,
    /// HDR: the display's black, cd/m² (0 = true black).
    pub min_nits: f32,
}

impl Default for DisplayCalibration {
    fn default() -> Self {
        Self::NEUTRAL
    }
}

/// Which family of screen the game runs on, for [`DisplayCalibration::platform_default`].
///
/// Also a resource: a shell that knows better (a Deck detected through Steam) inserts it, and
/// the framework's settings use it for the out-of-the-box calibration.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayPlatform {
    /// Desktop monitor or TV: neutral.
    Desktop,
    /// Phone or tablet: often viewed outdoors; a modest shadow lift.
    Mobile,
    /// Steam Deck LCD panel (~400 cd/m², 1000:1 at best): a small shadow lift.
    Deck,
}

impl DisplayPlatform {
    /// The platform family of the running machine: Android is [`Self::Mobile`]; a Steam Deck
    /// (Steam sets `SteamDeck=1` in the game's environment there) is [`Self::Deck`].
    #[must_use]
    pub fn current() -> Self {
        let deck = regia_core::brand::env_var("SteamDeck").is_ok_and(|v| v == "1");
        Self::detect(deck.then_some(RenderProfilePreset::Deck))
    }

    /// The platform family for this build and render profile preset.
    #[must_use]
    pub fn detect(preset: Option<RenderProfilePreset>) -> Self {
        if cfg!(target_os = "android") {
            return Self::Mobile;
        }
        match preset {
            Some(RenderProfilePreset::Deck) => Self::Deck,
            Some(RenderProfilePreset::MobileLow | RenderProfilePreset::MobileHigh) => Self::Mobile,
            _ => Self::Desktop,
        }
    }
}

impl DisplayCalibration {
    /// The identity calibration (what goldens and neutral screenshots use).
    pub const NEUTRAL: Self = Self {
        black_level: 0.0,
        gamma: NEUTRAL_GAMMA,
        paper_white_nits: DEFAULT_PAPER_WHITE_NITS,
        peak_nits: DEFAULT_PEAK_NITS,
        min_nits: 0.0,
    };

    /// Out-of-the-box calibration for a platform (before the player calibrates).
    #[must_use]
    pub fn platform_default(platform: DisplayPlatform) -> Self {
        match platform {
            DisplayPlatform::Desktop => Self::NEUTRAL,
            DisplayPlatform::Mobile => Self {
                black_level: 0.3,
                ..Self::NEUTRAL
            },
            DisplayPlatform::Deck => Self {
                black_level: 0.15,
                ..Self::NEUTRAL
            },
        }
    }

    /// This calibration with the peak and minimum the display reports, where it reports them.
    #[must_use]
    pub fn with_display(self, display: &DisplayLuminance) -> Self {
        Self {
            peak_nits: display.peak_nits.unwrap_or(self.peak_nits),
            min_nits: display.min_nits.unwrap_or(self.min_nits),
            ..self
        }
        .clamped()
    }

    /// Every field clamped into its player range (NaN becomes the neutral value).
    #[must_use]
    pub fn clamped(self) -> Self {
        let fix = |v: f32, lo: f32, hi: f32, neutral: f32| {
            if v.is_finite() { v.clamp(lo, hi) } else { neutral }
        };
        let paper = fix(
            self.paper_white_nits,
            PAPER_WHITE_RANGE.0,
            PAPER_WHITE_RANGE.1,
            DEFAULT_PAPER_WHITE_NITS,
        );
        let peak = fix(self.peak_nits, PEAK_RANGE.0, PEAK_RANGE.1, DEFAULT_PEAK_NITS).max(paper);
        Self {
            black_level: fix(self.black_level, -1.0, 1.0, 0.0),
            gamma: fix(self.gamma, GAMMA_MIN, GAMMA_MAX, NEUTRAL_GAMMA),
            paper_white_nits: paper,
            peak_nits: peak,
            min_nits: fix(self.min_nits, MIN_NITS_RANGE.0, MIN_NITS_RANGE.1, 0.0).min(peak * 0.01),
        }
    }

    /// True when the SDR controls are the identity (the display stage can be skipped on SDR).
    #[must_use]
    pub fn is_sdr_neutral(&self) -> bool {
        let c = self.clamped();
        c.black_level == 0.0 && c.gamma == NEUTRAL_GAMMA
    }

    /// The black-level curve coefficient `a` (slope at black is `1 + a`).
    #[must_use]
    pub fn shadow_gain(&self) -> f32 {
        let s = self.clamped().black_level;
        if s >= 0.0 {
            BLACK_LIFT_MAX * s
        } else {
            BLACK_DEEPEN_MAX * s
        }
    }

    /// Exponent the SDR gamma control applies to display-encoded values (`g / 2.2`).
    #[must_use]
    pub fn gamma_exponent(&self) -> f32 {
        self.clamped().gamma / NEUTRAL_GAMMA
    }

    /// HDR highlight headroom over paper white (`peak / paper`, ≥ 1).
    #[must_use]
    pub fn hdr_headroom(&self) -> f32 {
        let c = self.clamped();
        (c.peak_nits / c.paper_white_nits).max(1.0)
    }

    /// The SDR calibration of one display-encoded colour (gamma, then black level).
    #[must_use]
    pub fn apply_sdr(&self, display: [f32; 3]) -> [f32; 3] {
        if self.is_sdr_neutral() {
            return display;
        }
        let a = self.shadow_gain();
        let e = self.gamma_exponent();
        display.map(|v| black_level_curve(v.clamp(0.0, 1.0).powf(e), a))
    }

    /// Applies [`Self::apply_sdr`] in place to sRGB-encoded RGBA8 pixels (CPU path of the
    /// display stage: offscreen captures and the evidence tool).
    pub fn apply_sdr_rgba8(&self, rgba: &mut [u8]) {
        if self.is_sdr_neutral() {
            return;
        }
        let table: Vec<u8> = (0..=255u8)
            .map(|c| {
                let v = self.apply_sdr([f32::from(c) / 255.0; 3])[0];
                (v * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect();
        for px in rgba.as_chunks_mut::<4>().0 {
            for c in &mut px[..3] {
                *c = table[usize::from(*c)];
            }
        }
    }

    /// Absolute luminance (cd/m², Rec.709 primaries) the HDR display stage sends for a
    /// linear colour in paper-white units (the black-level curve, paper white, then the
    /// minimum-luminance floor). Mirror of `display.wgsl`.
    #[must_use]
    pub fn hdr_nits(&self, linear_paper: [f32; 3]) -> [f32; 3] {
        let c = self.clamped();
        let a = self.shadow_gain();
        linear_paper.map(|l| {
            let l = l.max(0.0);
            let curved = if a != 0.0 && l < decode_srgb(0.5) {
                decode_srgb(black_level_curve(encode_srgb(l), a))
            } else {
                l
            };
            let nits = (curved * c.paper_white_nits).min(c.peak_nits);
            c.min_nits + nits * (1.0 - c.min_nits / c.peak_nits)
        })
    }

    /// The display-stage uniform words for this calibration (`display.wgsl` `cal`).
    #[must_use]
    pub(crate) fn uniform_words(&self) -> [[f32; 4]; 2] {
        let c = self.clamped();
        [
            [self.shadow_gain(), self.gamma_exponent(), 0.0, 0.0],
            [c.paper_white_nits, c.min_nits, c.peak_nits, 0.0],
        ]
    }
}

/// `PreUpdate`, once: reads [`DisplayLuminance`] from the platform and, while the calibration
/// is still the platform default, takes the display's peak and minimum into it.
pub(crate) fn query_display_luminance(
    mut commands: Commands,
    luminance: Option<Res<DisplayLuminance>>,
    platform: Option<Res<DisplayPlatform>>,
    calibration: Option<ResMut<DisplayCalibration>>,
) {
    if luminance.is_some() {
        return;
    }
    let report = regia_platform::display_luminance();
    commands.insert_resource(report);
    let default =
        DisplayCalibration::platform_default(platform.map_or_else(DisplayPlatform::current, |p| *p));
    if let Some(mut calibration) = calibration
        && *calibration == default
    {
        *calibration = default.with_display(&report);
    }
}

/// The black-level shadow curve on a display-encoded value (see the module docs).
///
/// `a = 0` is the identity; `v ≥ 0.5` is never changed; `f(0) = 0`.
#[must_use]
pub fn black_level_curve(v: f32, a: f32) -> f32 {
    if a == 0.0 || v >= 0.5 {
        return v;
    }
    let x = (2.0 * v).max(0.0);
    let one_minus = 1.0 - x;
    0.5 * (x + a * x * one_minus * one_minus)
}

/// HDR highlight expansion on a display-linear value: identity up to [`HDR_HIGHLIGHT_KNEE`],
/// then `k + (1 − k)·t + (H − 1)·t²` with `t = (L − k)/(1 − k)`, reaching `headroom` at `L = 1`
/// with a continuous slope at the knee. Mirror of the post blit in `post.wgsl`.
#[must_use]
pub fn hdr_highlight_expand(l: f32, headroom: f32) -> f32 {
    let k = HDR_HIGHLIGHT_KNEE;
    if headroom <= 1.0 || l <= k {
        return l;
    }
    let t = ((l - k) / (1.0 - k)).min(1.0);
    k + (1.0 - k) * t + (headroom - 1.0) * t * t
}

/// Linear Rec.709 → linear Rec.2020 (ITU-R BT.2087, the 709-in-2020 matrix).
#[must_use]
pub fn rec709_to_rec2020(rgb: [f32; 3]) -> [f32; 3] {
    const M: [[f32; 3]; 3] = [
        [0.627_404, 0.329_283, 0.043_313],
        [0.069_097, 0.919_540, 0.011_362],
        [0.016_391, 0.088_013, 0.895_595],
    ];
    std::array::from_fn(|r| M[r][0] * rgb[0] + M[r][1] * rgb[1] + M[r][2] * rgb[2])
}

/// sRGB transfer function (IEC 61966-2-1), linear → encoded.
#[must_use]
pub fn encode_srgb(linear: f32) -> f32 {
    let l = linear.max(0.0);
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

/// sRGB transfer function (IEC 61966-2-1), encoded → linear.
#[must_use]
pub fn decode_srgb(encoded: f32) -> f32 {
    let e = encoded.max(0.0);
    if e <= 0.040_45 {
        e / 12.92
    } else {
        ((e + 0.055) / 1.055).powf(2.4)
    }
}

/// A simulated display for evidence and tests (see the module docs).
///
/// Luminance of a signal `v`: `black + (white − black)·max(0, (v − crush)/(1 − crush))^gamma
/// + ambient`, where `ambient` is the light the screen reflects.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DisplayModel {
    /// Short name for sheets and logs.
    pub name: &'static str,
    /// Peak white, cd/m².
    pub white_nits: f32,
    /// Emitted black, cd/m².
    pub black_nits: f32,
    /// Electro-optical gamma.
    pub gamma: f32,
    /// Signal at and below which the display shows black (crushed code values).
    pub crush: f32,
    /// Reflected ambient light, cd/m².
    pub ambient_nits: f32,
}

impl DisplayModel {
    /// A good monitor in a dim room: the look the content is judged on.
    pub const REFERENCE: Self = Self {
        name: "reference monitor",
        white_nits: 160.0,
        black_nits: 0.16,
        gamma: 2.2,
        crush: 0.0,
        ambient_nits: 0.3,
    };
    /// A dim laptop panel in an office: raised black, low contrast, darker gamma.
    pub const DIM_LAPTOP: Self = Self {
        name: "dim laptop",
        white_nits: 110.0,
        black_nits: 1.1,
        gamma: 2.5,
        crush: 0.0,
        ambient_nits: 2.5,
    };
    /// A TV whose processing crushes the first code values to black.
    pub const TV_CRUSHED_BLACKS: Self = Self {
        name: "TV, crushed blacks",
        white_nits: 250.0,
        black_nits: 0.05,
        gamma: 2.4,
        crush: 0.06,
        ambient_nits: 0.3,
    };
    /// A phone outdoors: bright panel, strong reflected daylight.
    pub const PHONE_OUTDOOR: Self = Self {
        name: "phone outdoors",
        white_nits: 650.0,
        black_nits: 0.5,
        gamma: 2.2,
        crush: 0.0,
        ambient_nits: 70.0,
    };
    /// The three problem displays of the evidence sheet.
    pub const PROBLEM_DISPLAYS: [Self; 3] = [Self::DIM_LAPTOP, Self::TV_CRUSHED_BLACKS, Self::PHONE_OUTDOOR];

    /// Luminance the viewer receives for a signal (cd/m², including reflections).
    #[must_use]
    pub fn luminance(&self, signal: f32) -> f32 {
        let s = signal.clamp(0.0, 1.0);
        let crushed = ((s - self.crush) / (1.0 - self.crush)).max(0.0);
        self.black_nits + (self.white_nits - self.black_nits) * crushed.powf(self.gamma) + self.ambient_nits
    }

    /// The luminance as a fraction of the brightest thing the viewer sees on this screen
    /// (white plus reflections): what an adapted eye compares against. For sheets, encode it
    /// with [`encode_srgb`].
    #[must_use]
    pub fn relative(&self, signal: f32) -> f32 {
        self.luminance(signal) / (self.white_nits + self.ambient_nits)
    }

    /// Smallest luminance difference visible at luminance `l` (Weber fraction 2 % with a
    /// 1 cd/m² dark floor, a coarse threshold-versus-intensity model).
    #[must_use]
    pub fn threshold(l: f32) -> f32 {
        0.02 * (l + 1.0)
    }

    /// True when `a` and `b` (signals) are told apart on this display.
    #[must_use]
    pub fn distinguishable(&self, a: f32, b: f32) -> bool {
        let (la, lb) = (self.luminance(a), self.luminance(b));
        (la - lb).abs() > Self::threshold(la.min(lb))
    }

    /// Emulates a player doing the black-level logo test on this display: the smallest black
    /// level (in slider steps of 0.05) at which the "barely visible" logo shows against black,
    /// keeping the rest of `base`. Returns `base` unchanged when the logo already shows.
    #[must_use]
    pub fn solve_black_level(&self, base: DisplayCalibration) -> DisplayCalibration {
        for step in 0..=20 {
            let candidate = DisplayCalibration {
                black_level: step as f32 * 0.05,
                ..base
            };
            let logo = candidate.apply_sdr([LOGO_VISIBLE_SIGNAL; 3])[0];
            let black = candidate.apply_sdr([0.0; 3])[0];
            if self.distinguishable(logo, black) {
                return candidate;
            }
        }
        DisplayCalibration {
            black_level: 1.0,
            ..base
        }
    }

    /// Emulates a player doing the gamma (contrast) test on this display: the gamma (slider
    /// steps of 0.05) at which a solid square at the 2.2 mid-grey code value matches the
    /// average of 1-pixel black/white lines, keeping the rest of `base`.
    #[must_use]
    pub fn solve_gamma(&self, base: DisplayCalibration) -> DisplayCalibration {
        let stripes = 0.5 * (self.luminance(1.0) + self.luminance(0.0));
        let solid_signal = 0.5_f32.powf(1.0 / NEUTRAL_GAMMA);
        let steps = ((GAMMA_MAX - GAMMA_MIN) / 0.05).round() as i32;
        (0..=steps)
            .map(|i| DisplayCalibration {
                gamma: GAMMA_MIN + i as f32 * 0.05,
                ..base
            })
            .min_by(|a, b| {
                let err = |c: &DisplayCalibration| {
                    (self.luminance(c.apply_sdr([solid_signal; 3])[0]) - stripes).abs()
                };
                err(a).total_cmp(&err(b))
            })
            .unwrap_or(base)
    }

    /// Emulates a player running the whole SDR calibration screen: the contrast (gamma) test,
    /// then the black-level logo test.
    #[must_use]
    pub fn solve(&self, base: DisplayCalibration) -> DisplayCalibration {
        self.solve_black_level(self.solve_gamma(base))
    }

    /// Fraction of shadow detail that survives on this display, against the reference
    /// display showing the neutral image: over horizontally and vertically adjacent pixel
    /// pairs in the shadows of `neutral` (luma below 0.3) that are visible on the reference
    /// display, the fraction still visible here after `calibration`. sRGB-encoded RGBA8.
    #[must_use]
    pub fn shadow_detail_retained(
        &self,
        calibration: &DisplayCalibration,
        neutral: &[u8],
        width: usize,
        height: usize,
    ) -> f32 {
        let luma = |px: &[u8]| {
            (0.2126 * f32::from(px[0]) + 0.7152 * f32::from(px[1]) + 0.0722 * f32::from(px[2])) / 255.0
        };
        let at = |x: usize, y: usize| luma(&neutral[(y * width + x) * 4..(y * width + x) * 4 + 4]);
        let cal = |v: f32| calibration.apply_sdr([v; 3])[0];
        let (mut seen, mut kept) = (0u32, 0u32);
        for y in 0..height.saturating_sub(1) {
            for x in 0..width.saturating_sub(1) {
                let a = at(x, y);
                if a >= 0.3 {
                    continue;
                }
                for b in [at(x + 1, y), at(x, y + 1)] {
                    if !Self::REFERENCE.distinguishable(a, b) {
                        continue;
                    }
                    seen += 1;
                    if self.distinguishable(cal(a), cal(b)) {
                        kept += 1;
                    }
                }
            }
        }
        if seen == 0 { 1.0 } else { kept as f32 / seen as f32 }
    }

    /// The image as perceived on this display, re-encoded for viewing on a normal monitor
    /// (sRGB RGBA8, each channel through [`Self::relative`]). For evidence sheets.
    pub fn simulate_rgba8(&self, rgba: &mut [u8]) {
        let table: Vec<u8> = (0..=255u8)
            .map(|c| {
                let rel = self.relative(f32::from(c) / 255.0);
                (encode_srgb(rel) * 255.0).round().clamp(0.0, 255.0) as u8
            })
            .collect();
        for px in rgba.as_chunks_mut::<4>().0 {
            for c in &mut px[..3] {
                *c = table[usize::from(*c)];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neutral_is_the_identity() {
        let n = DisplayCalibration::NEUTRAL;
        assert!(n.is_sdr_neutral());
        for i in 0..=255u8 {
            let v = f32::from(i) / 255.0;
            assert_eq!(n.apply_sdr([v; 3])[0], v);
        }
        let mut px = vec![1, 2, 3, 4, 200, 100, 50, 255];
        let before = px.clone();
        n.apply_sdr_rgba8(&mut px);
        assert_eq!(px, before);
    }

    #[test]
    fn black_level_keeps_black_mid_grey_and_white() {
        for s in [-1.0, -0.4, 0.3, 1.0] {
            let c = DisplayCalibration {
                black_level: s,
                ..DisplayCalibration::NEUTRAL
            };
            assert_eq!(c.apply_sdr([0.0; 3])[0], 0.0, "black stays black at {s}");
            assert!((c.apply_sdr([0.5; 3])[0] - 0.5).abs() < 1.0e-6, "mid gray at {s}");
            assert_eq!(c.apply_sdr([1.0; 3])[0], 1.0, "white at {s}");
            assert_eq!(c.apply_sdr([0.7; 3])[0], 0.7, "upper half untouched at {s}");
        }
    }

    #[test]
    fn black_level_is_a_monotonic_curve_not_a_gain() {
        let lift = DisplayCalibration {
            black_level: 1.0,
            ..DisplayCalibration::NEUTRAL
        };
        let mut last = -1.0;
        for i in 0..=1000 {
            let v = i as f32 / 1000.0;
            let y = lift.apply_sdr([v; 3])[0];
            assert!(y >= last, "monotonic at {v}");
            last = y;
        }
        // The closed form includes the one-half: v' = ½(x + a·x·(1 − x)²), x = 2v.
        assert!(
            (black_level_curve(0.25, 2.5) - 0.40625).abs() < 1.0e-6,
            "quarter grey at full lift"
        );
        let code3 = 3.0 / 255.0;
        let ratio = black_level_curve(code3, 2.5) / code3;
        assert!(
            (ratio - 3.38).abs() < 0.02,
            "code 3 at full lift is about 3.38×, got {ratio}"
        );
        // Gain falls from 3.5x near black to 1x at mid grey.
        let g_dark = lift.apply_sdr([0.01; 3])[0] / 0.01;
        let g_mid = lift.apply_sdr([0.4; 3])[0] / 0.4;
        assert!(g_dark > 3.0 && g_mid < 1.2, "{g_dark} {g_mid}");
        // C1 at mid grey: the slope just below 0.5 is about 1.
        let slope = (lift.apply_sdr([0.5; 3])[0] - lift.apply_sdr([0.49; 3])[0]) / 0.01;
        assert!((slope - 1.0).abs() < 0.05, "slope at mid gray {slope}");
        let deepen = DisplayCalibration {
            black_level: -1.0,
            ..DisplayCalibration::NEUTRAL
        };
        assert!(deepen.apply_sdr([0.1; 3])[0] < 0.1);
    }

    #[test]
    fn gamma_moves_mid_tones_only_by_the_display_ratio() {
        let dark = DisplayCalibration {
            gamma: 2.6,
            ..DisplayCalibration::NEUTRAL
        };
        let bright = DisplayCalibration {
            gamma: 1.8,
            ..DisplayCalibration::NEUTRAL
        };
        let v = 0.5;
        assert!(dark.apply_sdr([v; 3])[0] < v);
        assert!(bright.apply_sdr([v; 3])[0] > v);
        // The image on a 2.2 display after gamma 2.6 equals the image on a 2.6 display.
        let shown = dark.apply_sdr([v; 3])[0].powf(2.2);
        assert!((shown - v.powf(2.6)).abs() < 1.0e-5);
        assert_eq!(dark.apply_sdr([1.0; 3])[0], 1.0);
    }

    #[test]
    fn clamping_keeps_players_in_range() {
        let wild = DisplayCalibration {
            black_level: 7.0,
            gamma: f32::NAN,
            paper_white_nits: 5.0,
            peak_nits: 50.0,
            min_nits: 3.0,
        }
        .clamped();
        assert_eq!(wild.black_level, 1.0);
        assert_eq!(wild.gamma, NEUTRAL_GAMMA);
        assert_eq!(wild.paper_white_nits, PAPER_WHITE_RANGE.0);
        assert!(wild.peak_nits >= wild.paper_white_nits);
        assert!(wild.min_nits <= 0.01 * wild.peak_nits);
    }

    #[test]
    fn hdr_paper_white_peak_and_floor() {
        let c = DisplayCalibration::NEUTRAL;
        let white = c.hdr_nits([1.0; 3])[0];
        assert!(
            (white - DEFAULT_PAPER_WHITE_NITS).abs() < 1.0e-3,
            "UI white at paper white"
        );
        let peak = c.hdr_nits([c.hdr_headroom(); 3])[0];
        assert!((peak - DEFAULT_PEAK_NITS).abs() < 1.0e-2);
        assert_eq!(c.hdr_nits([0.0; 3])[0], 0.0);
        let floor = DisplayCalibration { min_nits: 0.5, ..c };
        assert!((floor.hdr_nits([0.0; 3])[0] - 0.5).abs() < 1.0e-6);
        assert!((floor.hdr_nits([c.hdr_headroom(); 3])[0] - DEFAULT_PEAK_NITS).abs() < 1.0e-2);
    }

    #[test]
    fn highlight_expansion_is_c1_and_reaches_headroom() {
        let h = 5.0;
        assert_eq!(hdr_highlight_expand(0.3, h), 0.3);
        assert!((hdr_highlight_expand(1.0, h) - h).abs() < 1.0e-5);
        let e = 1.0e-3;
        let slope = (hdr_highlight_expand(HDR_HIGHLIGHT_KNEE + e, h)
            - hdr_highlight_expand(HDR_HIGHLIGHT_KNEE, h))
            / e;
        assert!((slope - 1.0).abs() < 0.02, "slope at the knee {slope}");
        assert_eq!(hdr_highlight_expand(0.9, 1.0), 0.9, "no headroom, no expansion");
    }

    #[test]
    fn rec2020_keeps_white() {
        let w = rec709_to_rec2020([1.0; 3]);
        for c in w {
            assert!((c - 1.0).abs() < 1.0e-3, "{w:?}");
        }
    }

    #[test]
    fn srgb_round_trips() {
        for i in 0..=255u8 {
            let v = f32::from(i) / 255.0;
            assert!((encode_srgb(decode_srgb(v)) - v).abs() < 1.0e-5);
        }
    }

    #[test]
    fn logo_test_fixes_the_crushed_tv_and_leaves_the_reference_alone() {
        let reference = DisplayModel::REFERENCE.solve_black_level(DisplayCalibration::NEUTRAL);
        assert_eq!(
            reference.black_level, 0.0,
            "logo visible on a good display at neutral"
        );
        assert!(
            !DisplayModel::REFERENCE.distinguishable(LOGO_HIDDEN_SIGNAL, 0.0),
            "the hidden logo stays hidden on a good display"
        );
        let tv = DisplayModel::TV_CRUSHED_BLACKS;
        assert!(
            !tv.distinguishable(LOGO_VISIBLE_SIGNAL, 0.0),
            "the TV crushes the logo"
        );
        let fixed = tv.solve_black_level(DisplayCalibration::NEUTRAL);
        assert!(fixed.black_level > 0.0 && fixed.black_level < 1.0, "{fixed:?}");
        let logo = fixed.apply_sdr([LOGO_VISIBLE_SIGNAL; 3])[0];
        assert!(tv.distinguishable(logo, 0.0));
    }

    #[test]
    fn gamma_test_finds_the_display_gamma() {
        // On a gamma-2.2 display with no flare the stripes match at neutral.
        let ideal = DisplayModel {
            black_nits: 0.0,
            ambient_nits: 0.0,
            ..DisplayModel::REFERENCE
        };
        assert!((ideal.solve_gamma(DisplayCalibration::NEUTRAL).gamma - NEUTRAL_GAMMA).abs() < 0.03);
        // A darker 2.5 display asks for a brighter (lower) setting.
        let dark = DisplayModel { gamma: 2.5, ..ideal };
        assert!(dark.solve_gamma(DisplayCalibration::NEUTRAL).gamma < 2.0);
        assert!(
            DisplayModel::REFERENCE
                .solve(DisplayCalibration::NEUTRAL)
                .black_level
                < 0.1
        );
    }

    #[test]
    fn calibration_recovers_shadow_detail_on_problem_displays() {
        // A shadow ramp with fine texture: 0.02..0.25 display with ±1 code value noise.
        let (w, h) = (64usize, 16usize);
        let mut img = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let base = 5.0 + 60.0 * x as f32 / w as f32;
                let v = (base + if (x + y) % 2 == 0 { 2.0 } else { -2.0 }).round() as u8;
                img[(y * w + x) * 4..(y * w + x) * 4 + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        for display in DisplayModel::PROBLEM_DISPLAYS {
            let before = display.shadow_detail_retained(&DisplayCalibration::NEUTRAL, &img, w, h);
            let cal = display.solve_black_level(DisplayCalibration::NEUTRAL);
            let after = display.shadow_detail_retained(&cal, &img, w, h);
            assert!(
                after > before,
                "{}: detail {before} -> {after} at black level {}",
                display.name,
                cal.black_level
            );
        }
    }

    #[test]
    fn platform_defaults() {
        assert!(DisplayCalibration::platform_default(DisplayPlatform::Desktop).is_sdr_neutral());
        assert!(DisplayCalibration::platform_default(DisplayPlatform::Mobile).black_level > 0.0);
        assert!(DisplayCalibration::platform_default(DisplayPlatform::Deck).black_level > 0.0);
        if !cfg!(target_os = "android") {
            assert_eq!(
                DisplayPlatform::detect(Some(RenderProfilePreset::Deck)),
                DisplayPlatform::Deck
            );
            assert_eq!(DisplayPlatform::detect(None), DisplayPlatform::Desktop);
        }
    }
}
