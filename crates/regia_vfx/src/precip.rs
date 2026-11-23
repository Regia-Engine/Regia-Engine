// SPDX-License-Identifier: MPL-2.0
//! Rain and snow drawn as stateless instanced drops in a wrapped box around the eye (spec 99 § 5,
//! ADR-1847). No simulation buffer and no CPU work per drop: the CPU integrates one fall distance
//! per size class and one wind drift ([`PrecipIntegrator`]), and each vertex rebuilds its drop
//! from its index, so a camera cut shows a full field on its first frame and the same frames draw
//! the same drops.
//!
//! The wrapped box follows Tariq 2007 and the camera-attached volume of Wang and Wade 2004; the
//! streak is the exposure-time model of Garg and Nayar 2006; subpixel streaks keep their coverage
//! as in Persson 2012. The code is Regia's own.
//!
//! [`rain_drop`], [`snow_flake`] and [`splash`] are the CPU mirror of the vertex stage, for the
//! tests and the agent checks. [`PrecipitationGpu`] is the draw, owned by the
//! [`crate::runtime::VfxRuntimePlugin`] layer. The agent reads [`PrecipitationReport`].

mod gpu;
pub(crate) use gpu::contract_rows;
mod system;

pub use gpu::{PrecipFill, PrecipTarget, PrecipView, PrecipitationGpu, precip_shader_source};
pub use system::{PrecipState, PrecipitationHandle, precipitation_tier, step_precipitation};

use regia_app::prelude::*;
use regia_render::{PrecipOcclusionInfo, QualityTier};

use crate::weather::{
    PrecipitationRates, RAIN_CLASSES, SNOW_CLASSES, SNOW_FALL_SPEEDS_M_PER_S, WeatherConfig,
    rain_class_diameters_mm, rain_count_factor, rain_fall_speed_m_per_s, snow_count_factor,
};

/// Forward push of the box center along the horizontal view direction, as a part of `H`.
const CENTER_SHIFT: f32 = 0.35;
/// The part of each half-extent alpha fades over.
const EDGE_FADE: f32 = 0.1;
/// The part of the drawn count alpha fades over.
const COUNT_FADE: f32 = 0.05;
/// The fastest eye the streaks follow (m/s); faster is a clamp, a jump this size a camera cut.
pub const MAX_EYE_SPEED_M_PER_S: f32 = 50.0;
/// The bound of a flake's sway (m).
pub const SNOW_SWAY_M: f32 = 0.15;
/// A splash's life (s).
pub const SPLASH_LIFE_S: f32 = 0.3;

/// The project's precipitation switches, seeded from `config/weather.ron`.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct PrecipitationSettings {
    /// Draw whatever falls (`effects.precipitation`, default false).
    pub enabled: bool,
    /// Multiplies the drawn counts, `0..4`.
    pub density_scale: f32,
    /// Caps the drops of each kind; `None` is the tier budget.
    pub max_drops: Option<u32>,
    /// Draw splashes (on the tiers with a splash budget).
    pub splashes: bool,
    /// Ask the renderer for the occlusion view.
    pub occlusion: bool,
    /// The shutter streaks are stretched by, seconds.
    pub streak_exposure_s: f32,
    /// The resolution the drops draw at; `None` is the tier's ([`PrecipBudget::resolution`]).
    /// Set at run time (`render.set_precipitation { resolution }`), never from the config file.
    pub resolution: Option<PrecipResolution>,
}

/// The resolution rain and snow draw at (spec 99 § 5.7, ADR-3249). `Full` draws them in the frame's
/// own pass, as before; `Half` and `Quarter` draw them into a smaller premultiplied target of
/// their own and composite it over the frame. A streak thinner than the target's pixel widens to
/// one pixel and pays in coverage, so the smaller targets fill fewer pixels but soften and widen
/// thin rain.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum PrecipResolution {
    /// The frame's resolution, in its own pass.
    #[default]
    Full,
    /// Half the frame's width and height.
    Half,
    /// A quarter of the frame's width and height.
    Quarter,
}

impl PrecipResolution {
    /// Every choice, finest first.
    pub const ALL: [Self; 3] = [Self::Full, Self::Half, Self::Quarter];

    /// The frame-size divisor (1, 2 or 4).
    #[must_use]
    pub fn divisor(self) -> u32 {
        match self {
            Self::Full => 1,
            Self::Half => 2,
            Self::Quarter => 4,
        }
    }

    /// Lower-case name (`full`, `half`, `quarter`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Half => "half",
            Self::Quarter => "quarter",
        }
    }

    /// The choice named `name` ([`Self::name`]).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.name() == name)
    }

    /// The reduced target's size for a `width` × `height` frame (at least one pixel each way).
    #[must_use]
    pub fn target_size(self, width: u32, height: u32) -> (u32, u32) {
        let d = self.divisor();
        (width.div_ceil(d).max(1), height.div_ceil(d).max(1))
    }
}

impl Default for PrecipitationSettings {
    fn default() -> Self {
        Self::from_config(&WeatherConfig::default())
    }
}

impl PrecipitationSettings {
    /// The settings a project starts with.
    #[must_use]
    pub fn from_config(config: &WeatherConfig) -> Self {
        let p = &config.precipitation;
        Self {
            enabled: config.effects.precipitation,
            density_scale: p.density_scale,
            max_drops: None,
            splashes: p.splashes,
            occlusion: true,
            streak_exposure_s: p.streak_exposure_s,
            resolution: None,
        }
    }
}

/// What a tier may draw (§ 5.7).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrecipBudget {
    /// Rain streaks at the heaviest rain.
    pub rain: u32,
    /// Flakes in the heaviest snow.
    pub snow: u32,
    /// Splashes at the heaviest rain.
    pub splashes: u32,
    /// Half the box's horizontal size `H` (m).
    pub half_extent_m: f32,
    /// Half the box's height `V` (m).
    pub height_m: f32,
    /// The occlusion view: `(texels, extent m)`; `None` where the tier has none.
    pub occlusion: Option<(u32, f32)>,
    /// The resolution the drops draw at. `Full` on every tier (ADR-3249): the reduced choices are
    /// there for the lead's device measurement, and a tier moves only by ADR.
    pub resolution: PrecipResolution,
}

impl PrecipBudget {
    /// The budget of `tier`.
    #[must_use]
    pub fn for_quality(tier: QualityTier) -> Self {
        let row = |rain, snow, splashes, half_extent_m, height_m, occlusion| Self {
            rain,
            snow,
            splashes,
            half_extent_m,
            height_m,
            occlusion,
            resolution: PrecipResolution::Full,
        };
        match tier {
            QualityTier::MobileLow => row(2_000, 1_500, 0, 12.0, 10.0, None),
            QualityTier::MobileHigh => row(6_000, 4_000, 256, 12.0, 10.0, Some((256, 32.0))),
            QualityTier::Deck => row(8_000, 5_000, 256, 14.0, 10.0, Some((256, 48.0))),
            QualityTier::Low | QualityTier::VeryLow => row(12_000, 8_000, 512, 16.0, 12.0, Some((512, 64.0))),
            QualityTier::Medium => row(24_000, 16_000, 1_024, 18.0, 12.0, Some((512, 64.0))),
            QualityTier::High => row(48_000, 30_000, 2_048, 18.0, 12.0, Some((512, 64.0))),
            QualityTier::Ultra => row(96_000, 60_000, 4_096, 22.0, 14.0, Some((1024, 96.0))),
        }
    }

    /// The box's size `B = (2H, 2V, 2H)` (m).
    #[must_use]
    pub fn box_size(&self) -> [f32; 3] {
        [
            2.0 * self.half_extent_m,
            2.0 * self.height_m,
            2.0 * self.half_extent_m,
        ]
    }
}

/// The drawn counts: `N` as a real number, so a rate change fades drops in and out.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PrecipCounts {
    /// Rain streaks.
    pub rain: f32,
    /// Flakes.
    pub snow: f32,
    /// Splashes.
    pub splashes: f32,
}

impl PrecipCounts {
    /// The counts `rates` draw under `budget` and `settings`.
    #[must_use]
    pub fn of(
        rates: &PrecipitationRates,
        config: &WeatherConfig,
        settings: &PrecipitationSettings,
        budget: &PrecipBudget,
    ) -> Self {
        let scale = settings.density_scale.clamp(0.0, 4.0);
        let cap = |budget: u32| settings.max_drops.map_or(budget, |max| budget.min(max)) as f32;
        let share = rain_count_factor(rates, config);
        let rain = (cap(budget.rain) * share * scale).min(budget.rain as f32 * 4.0);
        let snow = cap(budget.snow) * snow_count_factor(rates, config) * scale;
        let splash_rate = (rates.rain_mm_per_h / config.precipitation.max_rain_mm_per_h).clamp(0.0, 1.0);
        let splashes = if settings.splashes {
            cap(budget.splashes) * splash_rate * scale
        } else {
            0.0
        };
        Self { rain, snow, splashes }
    }

    /// Instances of a real count: the fade's last drop rounds up.
    #[must_use]
    pub fn instances(count: f32) -> u32 {
        count.ceil().max(0.0) as u32
    }
}

/// The per-frame inputs of the draw, built on the CPU (no per-drop data).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrecipFrame {
    /// World-space eye.
    pub eye: [f64; 3],
    /// The eye's velocity, clamped, zero on a camera cut (m/s).
    pub eye_velocity_m_per_s: [f32; 3],
    /// Horizontal direction of view, unit, on world X and Z.
    pub forward_xz: [f32; 2],
    /// The weather clock (s).
    pub clock_s: f64,
    /// What falls.
    pub rates: PrecipitationRates,
    /// The wind carrying the drops (m/s).
    pub wind_m_per_s: [f32; 3],
    /// Integrated fall of each rain class, wrapped to the box (m).
    pub rain_offsets_m: [f32; RAIN_CLASSES],
    /// Fall speed of each rain class (m/s).
    pub rain_speeds_m_per_s: [f32; RAIN_CLASSES],
    /// Diameter of each rain class (mm).
    pub rain_diameters_mm: [f32; RAIN_CLASSES],
    /// Integrated fall of each snow class, wrapped to the box (m).
    pub snow_offsets_m: [f32; SNOW_CLASSES],
    /// Integrated wind drift on X and Z, wrapped to the box (m).
    pub drift_m: [f32; 2],
    /// `eye mod B`, computed in `f64` so precision holds far from the origin (m).
    pub eye_mod_m: [f32; 3],
    /// The drawn counts.
    pub counts: PrecipCounts,
    /// The tier's budget (the box size).
    pub budget: PrecipBudget,
    /// The streak shutter (s).
    pub streak_exposure_s: f32,
    /// The tier.
    pub quality: QualityTier,
}

impl PrecipFrame {
    /// A frame with nothing to draw.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.counts.rain <= 0.0 && self.counts.snow <= 0.0
    }

    /// The box center relative to the eye (m).
    #[must_use]
    pub fn center_shift_m(&self) -> [f32; 3] {
        let push = CENTER_SHIFT * self.budget.half_extent_m;
        [self.forward_xz[0] * push, 0.0, self.forward_xz[1] * push]
    }
}

/// One fall distance per class and one drift, in `f64`, each wrapped by the box size so the
/// uploaded `f32` keeps its precision, whatever the clock. A rate change moves a class's speed and
/// its offset carries on from where it is (the cloud drift rule, ADR-429).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PrecipIntegrator {
    rain_fall_m: [f64; RAIN_CLASSES],
    snow_fall_m: [f64; SNOW_CLASSES],
    drift_m: [f64; 2],
    last_rain_speed: [f32; RAIN_CLASSES],
    last_wind: [f32; 2],
    box_size_m: [f64; 3],
}

impl PrecipIntegrator {
    /// Rain class speeds of `rates`.
    fn rain_speeds(rates: &PrecipitationRates, config: &WeatherConfig) -> [f32; RAIN_CLASSES] {
        let mut diameters = [0.0; RAIN_CLASSES];
        rain_class_diameters_mm(rates, config, &mut diameters);
        diameters.map(rain_fall_speed_m_per_s)
    }

    /// Advances by `dt` seconds: the trapezoid over the step, as `Weather::step` does, wrapped.
    pub fn step(
        &mut self,
        rates: &PrecipitationRates,
        config: &WeatherConfig,
        wind_m_per_s: [f32; 2],
        budget: &PrecipBudget,
        dt: f32,
    ) {
        self.box_size_m = budget.box_size().map(f64::from);
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let dt = f64::from(dt);
        let wrap_y = self.box_size_m[1];
        let speeds = Self::rain_speeds(rates, config);
        for (k, fall) in self.rain_fall_m.iter_mut().enumerate() {
            let mean = 0.5 * (f64::from(self.last_rain_speed[k]) + f64::from(speeds[k]));
            *fall = (*fall + mean * dt).rem_euclid(wrap_y);
        }
        self.last_rain_speed = speeds;
        for (k, fall) in self.snow_fall_m.iter_mut().enumerate() {
            *fall = (*fall + f64::from(SNOW_FALL_SPEEDS_M_PER_S[k]) * dt).rem_euclid(wrap_y);
        }
        for (axis, (drift, wind)) in self.drift_m.iter_mut().zip(wind_m_per_s).enumerate() {
            let mean = 0.5 * (f64::from(self.last_wind[axis]) + f64::from(wind));
            *drift = (*drift + mean * dt).rem_euclid(self.box_size_m[axis * 2]);
        }
        self.last_wind = wind_m_per_s;
    }

    /// The frame drawn with the eye at `eye`, looking along `forward_xz`.
    #[allow(clippy::too_many_arguments)] // one call, every input named at the call site
    #[must_use]
    pub fn frame(
        &self,
        eye: [f64; 3],
        eye_velocity_m_per_s: [f32; 3],
        forward_xz: [f32; 2],
        clock_s: f64,
        rates: &PrecipitationRates,
        config: &WeatherConfig,
        settings: &PrecipitationSettings,
        wind_m_per_s: [f32; 3],
        quality: QualityTier,
    ) -> PrecipFrame {
        let mut budget = PrecipBudget::for_quality(quality);
        if let Some(resolution) = settings.resolution {
            budget.resolution = resolution;
        }
        let box_size = budget.box_size().map(f64::from);
        let mut rain_diameters_mm = [0.0; RAIN_CLASSES];
        rain_class_diameters_mm(rates, config, &mut rain_diameters_mm);
        PrecipFrame {
            eye,
            eye_velocity_m_per_s,
            forward_xz,
            clock_s,
            rates: *rates,
            wind_m_per_s,
            rain_offsets_m: self.rain_fall_m.map(|v| v.rem_euclid(box_size[1]) as f32),
            rain_speeds_m_per_s: rain_diameters_mm.map(rain_fall_speed_m_per_s),
            rain_diameters_mm,
            snow_offsets_m: self.snow_fall_m.map(|v| v.rem_euclid(box_size[1]) as f32),
            drift_m: [
                self.drift_m[0].rem_euclid(box_size[0]) as f32,
                self.drift_m[1].rem_euclid(box_size[2]) as f32,
            ],
            eye_mod_m: [
                eye[0].rem_euclid(box_size[0]) as f32,
                eye[1].rem_euclid(box_size[1]) as f32,
                eye[2].rem_euclid(box_size[2]) as f32,
            ],
            counts: PrecipCounts::of(rates, config, settings, &budget),
            budget,
            streak_exposure_s: settings.streak_exposure_s,
            quality,
        }
    }
}

/// What `render.precipitation` reads.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct PrecipitationReport {
    /// The tier the budget comes from.
    pub quality: QualityTier,
    /// Rain streaks drawn.
    pub rain_drawn: u32,
    /// Flakes drawn.
    pub snow_drawn: u32,
    /// Splashes drawn.
    pub splashes: u32,
    /// The tier's budget.
    pub budget: PrecipBudget,
    /// The occlusion view, when the renderer drew one.
    pub occlusion: Option<PrecipOcclusionInfo>,
}

/// A rebuilt drop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DropSample {
    /// Position relative to the eye (m): `f32`, so it keeps its precision far from the origin.
    pub position_m: [f32; 3],
    /// World velocity (m/s).
    pub velocity_m_per_s: [f32; 3],
    /// Diameter (mm).
    pub diameter_mm: f32,
    /// Coverage, `0..1`, before the thin-line rule: the box edge and count fades and the base
    /// alpha of the kind.
    pub alpha: f32,
}

/// A rebuilt splash.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SplashSample {
    /// World position on X and Z (m).
    pub xz_m: [f32; 2],
    /// Age over the life, `0..1`.
    pub phase: f32,
    /// Coverage, `0..1` (0 for an instance past the count).
    pub alpha: f32,
}

/// Regia's integer hash of a drop index: the same bits as `precip_hash4` in `precip.wgsl`.
fn hash_step(state: u32) -> u32 {
    let word = state.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let mixed = ((word >> ((word >> 28) + 4)) ^ word).wrapping_mul(277_803_737);
    (mixed >> 22) ^ mixed
}

fn unit(bits: u32) -> f32 {
    // 24 bits fit an `f32` exactly, here and in the shader.
    (bits >> 8) as f32 / 16_777_216.0
}

/// Four values in `[0, 1)` from `index`.
#[must_use]
pub fn hash4(index: u32) -> [f32; 4] {
    let a = hash_step(index);
    let b = hash_step(a);
    let c = hash_step(b);
    let d = hash_step(c);
    [unit(a), unit(b), unit(c), unit(d)]
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge1 <= edge0 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn fract(x: f32) -> f32 {
    x - x.floor()
}

/// A drop's place in the wrapped box: `(eye-relative position, edge fade)`.
fn wrapped(seed: [f32; 4], offset: [f32; 3], frame: &PrecipFrame) -> ([f32; 3], f32) {
    let b = frame.budget.box_size();
    let half = [b[0] * 0.5, b[1] * 0.5, b[2] * 0.5];
    let shift = frame.center_shift_m();
    let mut position = [0.0; 3];
    let mut edge = 0.0_f32;
    for axis in 0..3 {
        let x = b[axis] * seed[axis] + offset[axis];
        let c = frame.eye_mod_m[axis] + shift[axis];
        let local = b[axis] * fract((x - c) / b[axis]) - half[axis];
        position[axis] = shift[axis] + local;
        edge = edge.max(local.abs() / half[axis]);
    }
    let fade = 1.0 - smoothstep(1.0 - EDGE_FADE, 1.0, edge);
    (position, fade)
}

fn count_fade(index: u32, count: f32) -> f32 {
    1.0 - smoothstep((1.0 - COUNT_FADE) * count, count, index as f32)
}

/// CPU mirror of the rain vertex stage: drop `index` of this frame.
#[must_use]
pub fn rain_drop(index: u32, frame: &PrecipFrame) -> DropSample {
    let k = index as usize % RAIN_CLASSES;
    let seed = hash4(index);
    let offset = [frame.drift_m[0], -frame.rain_offsets_m[k], frame.drift_m[1]];
    let (position_m, fade) = wrapped(seed, offset, frame);
    let diameter_mm = frame.rain_diameters_mm[k];
    let wind = frame.wind_m_per_s;
    DropSample {
        position_m,
        velocity_m_per_s: [wind[0], wind[1] - frame.rain_speeds_m_per_s[k], wind[2]],
        diameter_mm,
        alpha: fade * count_fade(index, frame.counts.rain) * rain_base_alpha(diameter_mm),
    }
}

/// The base alpha of a streak of `diameter_mm`.
#[must_use]
pub fn rain_base_alpha(diameter_mm: f32) -> f32 {
    (0.25 * diameter_mm / 2.0).clamp(0.1, 0.5)
}

/// The clock the shader sees: wrapped to a day, so an `f32` keeps a millisecond's precision.
#[must_use]
pub fn wrapped_clock_s(clock_s: f64) -> f64 {
    clock_s.rem_euclid(86_400.0)
}

/// A flake's sway at `clock_s`: bounded by [`SNOW_SWAY_M`] and continuous.
#[must_use]
pub fn snow_sway(seed: [f32; 4], clock_s: f64) -> [f32; 2] {
    let period = 1.5 + 1.5 * seed[3];
    let phase = seed[0] * std::f32::consts::TAU;
    let angle = (wrapped_clock_s(clock_s) as f32 / period) * std::f32::consts::TAU + phase;
    [SNOW_SWAY_M * angle.sin(), SNOW_SWAY_M * angle.cos()]
}

/// CPU mirror of the snow vertex stage: flake `index` of this frame.
#[must_use]
pub fn snow_flake(index: u32, frame: &PrecipFrame) -> DropSample {
    let k = index as usize % SNOW_CLASSES;
    let seed = hash4(index);
    let offset = [frame.drift_m[0], -frame.snow_offsets_m[k], frame.drift_m[1]];
    let (mut position_m, fade) = wrapped(seed, offset, frame);
    let sway = snow_sway(seed, frame.clock_s);
    position_m[0] += sway[0];
    position_m[2] += sway[1];
    let wind = frame.wind_m_per_s;
    DropSample {
        position_m,
        velocity_m_per_s: [wind[0], wind[1] - SNOW_FALL_SPEEDS_M_PER_S[k], wind[2]],
        diameter_mm: 3.0 + 7.0 * seed[3],
        alpha: fade * count_fade(index, frame.counts.snow) * 0.9,
    }
}

/// CPU mirror of the splash vertex stage: splash `index` of this frame. Each life cycle takes a
/// new place in a disk of radius `0.5 H`, in world space, so it does not slide with the camera.
#[must_use]
pub fn splash(index: u32, frame: &PrecipFrame) -> SplashSample {
    let start = hash4(index ^ 0x5bd1_e995)[3];
    let age = wrapped_clock_s(frame.clock_s) / f64::from(SPLASH_LIFE_S) + f64::from(start);
    let cycle = age.floor();
    let phase = (age - cycle) as f32;
    let seed = hash4(index ^ (cycle as u32).wrapping_mul(0x9e37_79b1));
    let radius = 0.5 * frame.budget.half_extent_m * seed[0].sqrt();
    let angle = seed[1] * std::f32::consts::TAU;
    // Snapped to world space, so a splash does not slide with the camera while it lives.
    let center = [frame.eye[0].floor() as f32, frame.eye[2].floor() as f32];
    SplashSample {
        xz_m: [center[0] + radius * angle.cos(), center[1] + radius * angle.sin()],
        phase,
        alpha: (1.0 - phase) * count_fade(index, frame.counts.splashes) * 0.5,
    }
}

#[cfg(test)]
mod tests;
