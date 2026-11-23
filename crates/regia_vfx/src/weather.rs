// SPDX-License-Identifier: MPL-2.0
//! Weather: the [`Weather`] resource drives the volumetric cloud layer (ADR-429).
//!
//! A [`WeatherState`] is one weather: cloud coverage, cloud type, density, wind and
//! precipitation. [`Weather`] holds the current state, an optional eased transition to a target
//! state, the weather clock and the wind drift integrated over that clock. Every `Update`
//! [`drive_cloud_weather`] steps it by the game's [`Time`] (so it pauses and scales with the game)
//! and writes the result into the renderer's [`CloudSettings`] resource. Coverage, type,
//! density, wind, the clock and the drift are always written. A genus preset also writes
//! heights, noise tiles and look (see **Cloud genera** below). `shadow_map`, `fog_coupling`
//! and `overcast_ibl` are left as they are.
//!
//! - **Wind.** The drift is integrated (`drift += wind × dt`) and handed to the renderer as
//!   `CloudSettings::drift_offset_m`, so a wind change carries the clouds on from where they are
//!   rather than jumping by `Δwind × time` (which also kept the cloud history and the cloud
//!   transmittance map valid across gusts).
//! - **Transitions.** Values ease with a smoothstep. While a transition runs, coverage and cloud
//!   type are written in steps of 1/256 and density in 1/64-octave steps: the weather map is
//!   re-thresholded on the CPU only when a step is crossed (at most 256 times over a full
//!   clear-to-overcast change), and the values written at rest are exact.
//! - **Weather cuts.** A transition moves the layer a little each frame, which the renderer's
//!   reprojection follows. A change larger than `regia_render::CLOUD_WEATHER_CUT_COVERAGE` (and
//!   friends) in one frame, such as [`Weather::set_now`] to a different state, is a weather cut:
//!   the renderer marches the clouds and their shadow map whole that frame.
//! - **Cloud genera** (ADR-1408). The `fair_cumulus`, `cumulus`, `stratocumulus` and `cirrus`
//!   presets carry a [`CloudGenus`]: besides coverage, type and density they write the layer's
//!   heights, noise tiles and look (diffuse light, erosion, edge breakup, the cirrus sheet). The
//!   older presets carry none and leave those renderer fields alone, as before. A transition
//!   between two genera blends their layers; to or from a preset without one, the genus's layer
//!   holds for the whole transition.
//! - **Precipitation** (spec 99, ADR-1847). `precipitation` with the temperature and the
//!   [`PrecipitationKind`] gives the rates ([`Weather::rates`], [`PrecipitationRates`]). While the
//!   project's effects are on (`config/weather.ron`, [`WeatherConfig`]) they draw rain and snow
//!   ([`crate::precip`]), thicken the air ([`regia_scene::weather::WeatherAir`]) and wet or cover
//!   the surfaces ([`regia_scene::weather::SurfaceWeather`]), and the weather's wind reaches the
//!   ground as [`regia_scene::weather::SceneWind`]. With the effects off (the default) it stays a
//!   value games read.
//!
//! Agent: `render.get_weather` / `render.set_weather` (book `agents/weather.md`),
//! `render.scene_wind`, `render.surface_weather` and friends (book
//! `agents/weather-precipitation.md`).

mod config;
mod drivers;
mod model;
mod surface;

pub use config::{
    PrecipitationConfig, SurfaceConfig, VisibilityConfig, WEATHER_CONFIG_FORMAT_VERSION, WEATHER_CONFIG_PATH,
    WeatherConfig, WeatherConfigError, WeatherConfigStatus, WeatherEffects, WeatherPresetDef, WindCoupling,
    weather_config_migration_sources, weather_config_schema_fingerprint,
};
pub use drivers::{
    drive_scene_wind, drive_surface_weather, drive_weather_air, load_weather_config, medium_is_airless,
    scene_wind_surface_mean, start_level_weather, sun_elevation_sin,
};
pub use model::{
    PrecipitationKind, PrecipitationRates, RAIN_CLASSES, SNOW_CLASSES, SNOW_FALL_SPEEDS_M_PER_S,
    rain_class_diameters_mm, rain_count_factor, rain_fall_speed_m_per_s, snow_count_factor,
};
pub use surface::{
    SurfaceInputs, gust_factor, gust_turn_rad, step_surface_weather, surface_wind, surface_wind_factor,
};

use regia_app::prelude::*;
use regia_render::{CloudGenus, CloudGenusLayer, CloudLook, CloudSettings, OutdoorParams};

/// The preset names [`WeatherState::preset`] accepts. The last four before the precipitation
/// presets are cloud genera ([`CloudGenus::NAMES`], ADR-1408).
pub const WEATHER_PRESETS: [&str; 12] = [
    "clear",
    "fair",
    "cloudy",
    "overcast",
    "storm",
    "fair_cumulus",
    "cumulus",
    "stratocumulus",
    "cirrus",
    "rain",
    "snow",
    "blizzard",
];

/// Coverage and cloud-type step written while a transition runs.
pub const WEATHER_VALUE_STEP: f32 = 1.0 / 256.0;

/// Density step written while a transition runs, in octaves (a ratio of `2^(1/64)` ≈ 1.011).
pub const WEATHER_DENSITY_OCTAVE_STEP: f32 = 1.0 / 64.0;

/// One weather: what the sky does when it is not changing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WeatherState {
    /// Fraction of the sky the cloud layer covers, `0..1`.
    pub coverage: f32,
    /// Cloud type: 0 stratus, 0.5 cumulus, 1 towering cumulus.
    pub cloud_type: f32,
    /// Extinction inside a full-density cloud (1/metres).
    pub density: f32,
    /// Horizontal wind at cloud height (metres/second) on world X and Z.
    pub wind_m_per_s: [f32; 2],
    /// Rain or snow intensity, `0..1`: the input of [`Self::rates`].
    pub precipitation: f32,
    /// The cloud genus whose heights, noise tiles and look this weather writes (ADR-1408).
    /// `None` leaves those renderer fields as they are.
    pub genus: Option<CloudGenus>,
    /// Air temperature near the ground, °C.
    pub temperature_c: f32,
    /// What falls; [`PrecipitationKind::Auto`] goes by the temperature.
    pub precipitation_kind: PrecipitationKind,
    /// The gust amplitude of the scene wind, `0..1`.
    pub gustiness: f32,
}

impl Default for WeatherState {
    fn default() -> Self {
        Self::FAIR
    }
}

impl WeatherState {
    /// No clouds, a light breeze.
    pub const CLEAR: Self = Self {
        coverage: 0.0,
        cloud_type: 0.5,
        density: 0.04,
        wind_m_per_s: [3.0, 1.0],
        precipitation: 0.0,
        genus: None,
        temperature_c: 15.0,
        precipitation_kind: PrecipitationKind::Auto,
        gustiness: 0.15,
    };
    /// Scattered fair-weather cumulus.
    pub const FAIR: Self = Self {
        coverage: 0.3,
        cloud_type: 0.5,
        density: 0.04,
        wind_m_per_s: [6.0, 2.0],
        precipitation: 0.0,
        genus: None,
        temperature_c: 15.0,
        precipitation_kind: PrecipitationKind::Auto,
        gustiness: 0.2,
    };
    /// Broken cumulus, more than half the sky.
    pub const CLOUDY: Self = Self {
        coverage: 0.6,
        cloud_type: 0.6,
        density: 0.05,
        wind_m_per_s: [8.0, 3.0],
        precipitation: 0.0,
        genus: None,
        temperature_c: 12.0,
        precipitation_kind: PrecipitationKind::Auto,
        gustiness: 0.25,
    };
    /// A low grey stratus deck with drizzle.
    pub const OVERCAST: Self = Self {
        coverage: 0.95,
        cloud_type: 0.15,
        density: 0.06,
        wind_m_per_s: [10.0, 3.0],
        precipitation: 0.2,
        genus: None,
        temperature_c: 8.0,
        precipitation_kind: PrecipitationKind::Auto,
        gustiness: 0.2,
    };
    /// Towering, dense storm cells in a strong wind, heavy rain.
    pub const STORM: Self = Self {
        coverage: 1.0,
        cloud_type: 0.95,
        density: 0.09,
        wind_m_per_s: [18.0, 6.0],
        precipitation: 1.0,
        genus: None,
        temperature_c: 10.0,
        precipitation_kind: PrecipitationKind::Auto,
        gustiness: 0.45,
    };
    /// A grey deck with moderate rain, about 6 mm/h.
    pub const RAIN: Self = Self {
        coverage: 0.9,
        cloud_type: 0.3,
        density: 0.07,
        wind_m_per_s: [9.0, 3.0],
        precipitation: 0.35,
        genus: None,
        temperature_c: 9.0,
        precipitation_kind: PrecipitationKind::Auto,
        gustiness: 0.3,
    };
    /// Steady snow below freezing, about 1.2 cm/h.
    pub const SNOW: Self = Self {
        coverage: 0.95,
        cloud_type: 0.15,
        density: 0.06,
        wind_m_per_s: [5.0, 2.0],
        precipitation: 0.5,
        genus: None,
        temperature_c: -3.0,
        precipitation_kind: PrecipitationKind::Auto,
        gustiness: 0.2,
    };
    /// Heavy snow in a strong, gusty wind, about 5 cm/h.
    pub const BLIZZARD: Self = Self {
        coverage: 1.0,
        cloud_type: 0.2,
        density: 0.08,
        wind_m_per_s: [20.0, 8.0],
        precipitation: 1.0,
        genus: None,
        temperature_c: -8.0,
        precipitation_kind: PrecipitationKind::Auto,
        gustiness: 0.6,
    };

    /// The named preset ([`WEATHER_PRESETS`]: `clear`, `fair`, `cloudy`, `overcast`, `storm`, a
    /// cloud genus `fair_cumulus`, `cumulus`, `stratocumulus`, `cirrus`, or `rain`, `snow`,
    /// `blizzard`), or `None`.
    #[must_use]
    pub fn preset(name: &str) -> Option<Self> {
        match name {
            "clear" => Some(Self::CLEAR),
            "fair" => Some(Self::FAIR),
            "cloudy" => Some(Self::CLOUDY),
            "overcast" => Some(Self::OVERCAST),
            "storm" => Some(Self::STORM),
            "rain" => Some(Self::RAIN),
            "snow" => Some(Self::SNOW),
            "blizzard" => Some(Self::BLIZZARD),
            _ => CloudGenus::from_name(name).map(Self::of_genus),
        }
    }

    /// The weather of a cloud genus: its layer's coverage, type and density, a wind for its
    /// height (light for fair cumulus, a breeze for the rest), no precipitation.
    #[must_use]
    pub fn of_genus(genus: CloudGenus) -> Self {
        let layer = genus.layer();
        let wind_m_per_s = match genus {
            CloudGenus::FairCumulus => [5.0, 1.5],
            CloudGenus::Cumulus => [7.0, 2.5],
            CloudGenus::Stratocumulus => [8.0, 2.0],
            CloudGenus::Cirrus => [6.0, 2.0],
        };
        Self {
            coverage: layer.coverage,
            cloud_type: layer.cloud_type,
            density: layer.density,
            wind_m_per_s,
            precipitation: 0.0,
            genus: Some(genus),
            temperature_c: 12.0,
            precipitation_kind: PrecipitationKind::Auto,
            gustiness: 0.2,
        }
    }

    /// The weather a cloud layer shows now (no precipitation).
    #[must_use]
    pub fn from_clouds(clouds: &CloudSettings) -> Self {
        Self {
            coverage: clouds.coverage,
            cloud_type: clouds.cloud_type,
            density: clouds.density,
            wind_m_per_s: clouds.wind_m_per_s,
            precipitation: 0.0,
            genus: None,
            temperature_c: Self::FAIR.temperature_c,
            precipitation_kind: PrecipitationKind::Auto,
            gustiness: Self::FAIR.gustiness,
        }
        .sanitized()
    }

    /// Coverage, type, precipitation and gustiness clamped to `0..1`, density to at least 0, the
    /// temperature to `-60..60`; a non-finite value takes [`Self::FAIR`]'s.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let pick = |v: f32, fallback: f32| if v.is_finite() { v } else { fallback };
        let fair = Self::FAIR;
        Self {
            coverage: pick(self.coverage, fair.coverage).clamp(0.0, 1.0),
            cloud_type: pick(self.cloud_type, fair.cloud_type).clamp(0.0, 1.0),
            density: pick(self.density, fair.density).max(0.0),
            wind_m_per_s: [pick(self.wind_m_per_s[0], 0.0), pick(self.wind_m_per_s[1], 0.0)],
            precipitation: pick(self.precipitation, 0.0).clamp(0.0, 1.0),
            genus: self.genus,
            temperature_c: pick(self.temperature_c, fair.temperature_c).clamp(-60.0, 60.0),
            precipitation_kind: self.precipitation_kind,
            gustiness: pick(self.gustiness, fair.gustiness).clamp(0.0, 1.0),
        }
    }

    /// `a` blended toward `b` by `t` (`0..1`; NaN reads as 0), every number linearly; the genus
    /// is `b`'s once `t` reaches 1, before that `b`'s if it has one, else `a`'s; the precipitation
    /// kind is `b`'s once `t` reaches 1, else `a`'s.
    #[must_use]
    pub fn lerp(a: Self, b: Self, t: f32) -> Self {
        // Regression: tests/tqs_sweep.rs::weather_lerp_hits_both_ends_clamps_and_treats_nan_as_the_start.
        let t = if t.is_nan() { 0.0 } else { t.clamp(0.0, 1.0) };
        let mix = |x: f32, y: f32| x + (y - x) * t;
        Self {
            coverage: mix(a.coverage, b.coverage),
            cloud_type: mix(a.cloud_type, b.cloud_type),
            density: mix(a.density, b.density),
            wind_m_per_s: [
                mix(a.wind_m_per_s[0], b.wind_m_per_s[0]),
                mix(a.wind_m_per_s[1], b.wind_m_per_s[1]),
            ],
            precipitation: mix(a.precipitation, b.precipitation),
            genus: if t >= 1.0 { b.genus } else { b.genus.or(a.genus) },
            temperature_c: mix(a.temperature_c, b.temperature_c),
            precipitation_kind: if t >= 1.0 {
                b.precipitation_kind
            } else {
                a.precipitation_kind
            },
            gustiness: mix(a.gustiness, b.gustiness),
        }
    }

    /// The state with coverage and type on [`WEATHER_VALUE_STEP`] and density on
    /// [`WEATHER_DENSITY_OCTAVE_STEP`] steps (what a running transition writes).
    #[must_use]
    pub fn quantized(self) -> Self {
        let step = |v: f32| (v / WEATHER_VALUE_STEP).round() * WEATHER_VALUE_STEP;
        let density = if self.density > 0.0 {
            let octaves =
                (self.density.log2() / WEATHER_DENSITY_OCTAVE_STEP).round() * WEATHER_DENSITY_OCTAVE_STEP;
            octaves.exp2()
        } else {
            0.0
        };
        Self {
            coverage: step(self.coverage).clamp(0.0, 1.0),
            cloud_type: step(self.cloud_type).clamp(0.0, 1.0),
            density,
            ..self
        }
    }
}

/// Look values a scene or an agent pins over the genus layer's look (ADR-1492). Each `Some` field
/// replaces that value of the cloud look every step ([`Weather::apply`]); `None` leaves the
/// genus layer's (or the renderer's) value. All `None` (the default) changes nothing.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CloudLookOverride {
    /// Diffuse light strength ([`regia_render::CloudLook::diffusion`]).
    pub diffusion: Option<f32>,
    /// Detail erosion depth, `0..1`.
    pub erosion: Option<f32>,
    /// Edge breakup amplitude, `0..1`.
    pub breakup: Option<f32>,
    /// World size of one breakup tile (metres).
    pub breakup_scale_m: Option<f32>,
    /// Cirrus coverage, `0..1`.
    pub cirrus_coverage: Option<f32>,
    /// Cirrus height above the ground (metres).
    pub cirrus_altitude_m: Option<f32>,
    /// Cirrus vertical optical depth.
    pub cirrus_optical_depth: Option<f32>,
    /// Width of one cirrus streak tile (metres).
    pub cirrus_scale_m: Option<f32>,
    /// Cirrus streak length over width.
    pub cirrus_stretch: Option<f32>,
    /// Cirrus fiber strength, `0..1` (ADR-1509; 0 keeps the ADR-1470 trails).
    pub cirrus_fiber: Option<f32>,
    /// Cirrus fiber spacing across the shear (metres).
    pub cirrus_fiber_scale_m: Option<f32>,
    /// Cirrus shear direction from the wind (degrees).
    pub cirrus_shear_deg: Option<f32>,
    /// Cirrus wind shear amount, `0..1`: longer trails, tails hanging below the sheet.
    pub cirrus_shear: Option<f32>,
    /// Cirrus patchy breakup, `0..1`.
    pub cirrus_breakup: Option<f32>,
    /// Cirrus uncinus hook amount, `0..1`.
    pub cirrus_hook: Option<f32>,
    /// Crisp cirrus strands, `0..1` (ADR-1516; 0 keeps the ADR-1509 sheet).
    pub cirrus_crisp: Option<f32>,
}

impl CloudLookOverride {
    /// Whether no field is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// Every field finite (a `Some` that is not finite is a data error).
    #[must_use]
    pub fn is_finite(&self) -> bool {
        [
            self.diffusion,
            self.erosion,
            self.breakup,
            self.breakup_scale_m,
            self.cirrus_coverage,
            self.cirrus_altitude_m,
            self.cirrus_optical_depth,
            self.cirrus_scale_m,
            self.cirrus_stretch,
            self.cirrus_fiber,
            self.cirrus_fiber_scale_m,
            self.cirrus_shear_deg,
            self.cirrus_shear,
            self.cirrus_breakup,
            self.cirrus_hook,
            self.cirrus_crisp,
        ]
        .into_iter()
        .flatten()
        .all(f32::is_finite)
    }

    /// `other`'s set fields over this one's.
    #[must_use]
    pub fn merged(self, other: Self) -> Self {
        Self {
            diffusion: other.diffusion.or(self.diffusion),
            erosion: other.erosion.or(self.erosion),
            breakup: other.breakup.or(self.breakup),
            breakup_scale_m: other.breakup_scale_m.or(self.breakup_scale_m),
            cirrus_coverage: other.cirrus_coverage.or(self.cirrus_coverage),
            cirrus_altitude_m: other.cirrus_altitude_m.or(self.cirrus_altitude_m),
            cirrus_optical_depth: other.cirrus_optical_depth.or(self.cirrus_optical_depth),
            cirrus_scale_m: other.cirrus_scale_m.or(self.cirrus_scale_m),
            cirrus_stretch: other.cirrus_stretch.or(self.cirrus_stretch),
            cirrus_fiber: other.cirrus_fiber.or(self.cirrus_fiber),
            cirrus_fiber_scale_m: other.cirrus_fiber_scale_m.or(self.cirrus_fiber_scale_m),
            cirrus_shear_deg: other.cirrus_shear_deg.or(self.cirrus_shear_deg),
            cirrus_shear: other.cirrus_shear.or(self.cirrus_shear),
            cirrus_breakup: other.cirrus_breakup.or(self.cirrus_breakup),
            cirrus_hook: other.cirrus_hook.or(self.cirrus_hook),
            cirrus_crisp: other.cirrus_crisp.or(self.cirrus_crisp),
        }
    }

    /// `look` with the set fields replaced, then sanitized (an empty override returns `look`
    /// untouched, bit for bit).
    #[must_use]
    pub fn applied(&self, look: CloudLook) -> CloudLook {
        if self.is_empty() {
            return look;
        }
        let mut look = look;
        if let Some(v) = self.diffusion {
            look.diffusion = v;
        }
        if let Some(v) = self.erosion {
            look.erosion = v;
        }
        if let Some(v) = self.breakup {
            look.breakup = v;
        }
        if let Some(v) = self.breakup_scale_m {
            look.breakup_scale_m = v;
        }
        if let Some(v) = self.cirrus_coverage {
            look.cirrus.coverage = v;
        }
        if let Some(v) = self.cirrus_altitude_m {
            look.cirrus.altitude_m = v;
        }
        if let Some(v) = self.cirrus_optical_depth {
            look.cirrus.optical_depth = v;
        }
        if let Some(v) = self.cirrus_scale_m {
            look.cirrus.scale_m = v;
        }
        if let Some(v) = self.cirrus_stretch {
            look.cirrus.stretch = v;
        }
        if let Some(v) = self.cirrus_fiber {
            look.cirrus.fiber = v;
        }
        if let Some(v) = self.cirrus_fiber_scale_m {
            look.cirrus.fiber_scale_m = v;
        }
        if let Some(v) = self.cirrus_shear_deg {
            look.cirrus.shear_deg = v;
        }
        if let Some(v) = self.cirrus_shear {
            look.cirrus.shear = v;
        }
        if let Some(v) = self.cirrus_breakup {
            look.cirrus.breakup = v;
        }
        if let Some(v) = self.cirrus_hook {
            look.cirrus.hook = v;
        }
        if let Some(v) = self.cirrus_crisp {
            look.cirrus.crisp = v;
        }
        look.sanitized()
    }
}

/// The weather that drives the volumetric cloud layer (ADR-429). Insert it (or call agent
/// `render.set_weather`) and [`drive_cloud_weather`] keeps the [`CloudSettings`] resource in step
/// with it every frame. Without this resource the cloud layer is left alone.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct Weather {
    from: WeatherState,
    to: WeatherState,
    duration_s: f32,
    elapsed_s: f32,
    clock_s: f64,
    drift_m: [f64; 2],
    /// The clock and drift this weather started from.
    origin: WeatherClock,
    /// Stops the transition, the clock and the wind drift (the game's pause stops them too).
    pub paused: bool,
    /// Cloud look values pinned over the genus layer's look every step (ADR-1492); empty by
    /// default. Set by `sky.ron`'s `cloud_look` block and by agent `render.set_clouds`. Without a
    /// genus layer clearing it leaves the last written values in the cloud layer.
    pub look_override: CloudLookOverride,
}

/// Where a [`Weather`]'s clock and wind drift stand (see [`Weather::restart_clock`]).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WeatherClock {
    clock_s: f64,
    drift_m: [f64; 2],
}

impl Default for Weather {
    fn default() -> Self {
        Self::new(WeatherState::default())
    }
}

impl Weather {
    /// Settled in `state`, the clock and drift at zero.
    #[must_use]
    pub fn new(state: WeatherState) -> Self {
        let state = state.sanitized();
        Self {
            from: state,
            to: state,
            duration_s: 0.0,
            elapsed_s: 0.0,
            clock_s: 0.0,
            drift_m: [0.0, 0.0],
            origin: WeatherClock::default(),
            paused: false,
            look_override: CloudLookOverride::default(),
        }
    }

    /// Settled in the weather `clouds` shows, keeping its clock and wind drift, so taking over a
    /// layer moves nothing.
    #[must_use]
    pub fn from_clouds(clouds: &CloudSettings) -> Self {
        let drift = clouds.drift_m();
        let origin = WeatherClock {
            clock_s: f64::from(clouds.time_s.max(0.0)),
            drift_m: [f64::from(drift[0]), f64::from(drift[1])],
        };
        Self {
            clock_s: origin.clock_s,
            drift_m: origin.drift_m,
            origin,
            ..Self::new(WeatherState::from_clouds(clouds))
        }
    }

    /// Puts the clock and the wind drift back where this weather started and returns where they
    /// stood, so a stretch of time runs from the same cloud phase however long the app ran before
    /// it (a render-out job, ADR-1881). The weather itself (the transition) stays.
    pub fn restart_clock(&mut self) -> WeatherClock {
        let before = WeatherClock {
            clock_s: self.clock_s,
            drift_m: self.drift_m,
        };
        self.clock_s = self.origin.clock_s;
        self.drift_m = self.origin.drift_m;
        before
    }

    /// Ends the stretch [`Self::restart_clock`] began: the clock and drift continue from `before`
    /// plus what the stretch ran.
    pub fn resume_clock(&mut self, before: WeatherClock) {
        self.clock_s = before.clock_s + (self.clock_s - self.origin.clock_s);
        for (axis, drift) in self.drift_m.iter_mut().enumerate() {
            *drift = before.drift_m[axis] + (*drift - self.origin.drift_m[axis]);
        }
    }

    /// The weather now: the transition's eased blend (exact, not stepped; the target itself once
    /// settled).
    #[must_use]
    pub fn current(&self) -> WeatherState {
        let t = self.progress();
        if t >= 1.0 {
            return self.to;
        }
        WeatherState::lerp(self.from, self.to, t * t * (3.0 - 2.0 * t))
    }

    /// The weather the transition ends in (the current one when settled).
    #[must_use]
    pub fn target(&self) -> WeatherState {
        self.to
    }

    /// What falls now. While a transition runs, the rates of the two end states blended by the
    /// eased progress, field by field, with the slope and the visibility recomputed from the
    /// blend: a change of kind moves every rate continuously and settles exactly on the target's.
    /// Every consumer reads this, not `current().rates()`.
    #[must_use]
    pub fn rates(&self, config: &WeatherConfig) -> PrecipitationRates {
        let t = self.progress();
        let to = self.to.rates(config);
        if t >= 1.0 {
            return to;
        }
        PrecipitationRates::blend(&self.from.rates(config), &to, t * t * (3.0 - 2.0 * t), config)
    }

    /// How far the transition is, `0..1`; 1 when settled.
    #[must_use]
    pub fn progress(&self) -> f32 {
        if self.duration_s <= 0.0 {
            return 1.0;
        }
        (self.elapsed_s / self.duration_s).clamp(0.0, 1.0)
    }

    /// Seconds of weather time left in the transition (0 when settled).
    #[must_use]
    pub fn remaining_s(&self) -> f32 {
        (self.duration_s - self.elapsed_s).max(0.0)
    }

    /// Whether a transition is still running.
    #[must_use]
    pub fn transitioning(&self) -> bool {
        self.progress() < 1.0
    }

    /// Seconds of weather time stepped so far.
    #[must_use]
    pub fn clock_s(&self) -> f64 {
        self.clock_s
    }

    /// Wind drift integrated over the clock (metres, world X and Z).
    #[must_use]
    pub fn drift_m(&self) -> [f64; 2] {
        self.drift_m
    }

    /// Starts an eased transition from the weather now to `target` over `seconds` of game time.
    /// `seconds` of 0 or less (or not finite) switches at once ([`Self::set_now`]).
    pub fn transition_to(&mut self, target: WeatherState, seconds: f32) {
        let target = target.sanitized();
        self.from = self.current();
        self.to = target;
        self.elapsed_s = 0.0;
        self.duration_s = if seconds.is_finite() {
            seconds.max(0.0)
        } else {
            0.0
        };
        if self.duration_s <= 0.0 {
            self.from = target;
        }
    }

    /// Switches to `state` at once. A change larger than a transition step is a weather cut:
    /// the renderer re-marches the clouds whole on the next frame.
    pub fn set_now(&mut self, state: WeatherState) {
        self.transition_to(state, 0.0);
    }

    /// Advances the transition, the clock and the wind drift by `dt` seconds (nothing while
    /// [`Self::paused`], or for a negative or non-finite `dt`).
    pub fn step(&mut self, dt: f32) {
        if self.paused || !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let before = self.current().wind_m_per_s;
        let duration = self.duration_s.max(0.0);
        self.elapsed_s = (self.elapsed_s + dt).min(duration);
        // Steps that add up to the duration land on it (120 steps of 1/60 s sum to 2 s less a
        // few f32 ulps): finish then, not one frame late.
        if duration - self.elapsed_s <= duration * 8.0 * f32::EPSILON {
            self.elapsed_s = duration;
        }
        let after = self.current().wind_m_per_s;
        let dt = f64::from(dt);
        // Trapezoid over the step: a steady wind drifts exactly `wind × dt`.
        for (axis, drift) in self.drift_m.iter_mut().enumerate() {
            *drift += 0.5 * (f64::from(before[axis]) + f64::from(after[axis])) * dt;
        }
        self.clock_s += dt;
    }

    /// The values written into the cloud layer: [`Self::current`], stepped
    /// ([`WeatherState::quantized`]) while a transition runs. A stepped value never lies past the
    /// transition's start or target (ADR-5667): near an end that is not on the step grid, it is
    /// that end, so the first value the renderer sees in the target's step is the target itself.
    #[must_use]
    pub fn cloud_values(&self) -> WeatherState {
        let now = self.current();
        if self.transitioning() {
            let stepped = now.quantized();
            WeatherState {
                coverage: between(stepped.coverage, self.from.coverage, self.to.coverage),
                cloud_type: between(stepped.cloud_type, self.from.cloud_type, self.to.cloud_type),
                density: between(stepped.density, self.from.density, self.to.density),
                ..stepped
            }
        } else {
            now
        }
    }

    /// The genus layer the cloud layer takes now (ADR-1408): between two genera their layers
    /// blended by the eased progress, else the one genus's layer, else `None` (keep the
    /// renderer's heights, tiles and look).
    #[must_use]
    pub fn genus_layer(&self) -> Option<CloudGenusLayer> {
        let to = self.to.genus.map(CloudGenus::layer);
        if !self.transitioning() {
            return to;
        }
        let from = self.from.genus.map(CloudGenus::layer);
        match (from, to) {
            (Some(a), Some(b)) => {
                let t = self.progress();
                Some(CloudGenusLayer::lerp(a, b, t * t * (3.0 - 2.0 * t)))
            }
            (a, b) => b.or(a),
        }
    }

    /// Writes the weather into `clouds`: coverage, type, density, wind, the clock as `time_s`
    /// and the integrated drift as `drift_offset_m` (so `clouds.drift_m()` is [`Self::drift_m`]),
    /// and with a genus ([`Self::genus_layer`]) its heights, noise tiles and look. Every other
    /// field is kept. Returns whether anything changed.
    pub fn apply(&self, clouds: &mut CloudSettings) -> bool {
        let values = self.cloud_values();
        let time_s = self.clock_s as f32;
        let wind = values.wind_m_per_s;
        let offset = [
            (self.drift_m[0] - f64::from(wind[0] * time_s)) as f32,
            (self.drift_m[1] - f64::from(wind[1] * time_s)) as f32,
        ];
        let mut next = CloudSettings {
            coverage: values.coverage,
            cloud_type: values.cloud_type,
            density: values.density,
            wind_m_per_s: wind,
            time_s,
            drift_offset_m: offset,
            ..*clouds
        };
        if let Some(layer) = self.genus_layer() {
            layer.apply_shape(&mut next);
        }
        next.look = self.look_override.applied(next.look);
        if next == *clouds {
            return false;
        }
        *clouds = next;
        true
    }
}

/// `value` clamped to the segment between `a` and `b` (either order). A NaN end drops out (`min`
/// and `max` return the other end); with both ends NaN `value` is returned as it is. Never panics.
fn between(value: f32, a: f32, b: f32) -> f32 {
    let (lo, hi) = (a.min(b), a.max(b));
    if lo <= hi { value.clamp(lo, hi) } else { value }
}

/// The cloud layer the renderer would use without a [`CloudSettings`] resource: the open
/// level's coverage and type over the defaults (as the extraction does), else the defaults.
#[must_use]
pub fn level_clouds(level: Option<&regia_scene::level::LevelSettings>) -> CloudSettings {
    let Some(level) = level else {
        return CloudSettings::default();
    };
    let mut params = OutdoorParams::default();
    params.apply_level(&level.atmosphere, false);
    params.clouds
}

/// `Update`: steps the [`Weather`] by the game's frame time and writes it into the
/// [`CloudSettings`] resource, inserting one (from the level's layer) when there is none.
/// Does nothing without a [`Weather`] resource.
pub fn drive_cloud_weather(
    time: Option<Res<Time>>,
    weather: Option<ResMut<Weather>>,
    clouds: Option<ResMut<CloudSettings>>,
    level: Option<Res<regia_scene::level::LevelSettings>>,
    mut commands: Commands,
) {
    let _span = tracing::debug_span!("drive_cloud_weather").entered();
    let Some(mut weather) = weather else {
        return;
    };
    let dt = time.map_or(0.0, |t| t.delta_secs());
    weather.step(dt);
    match clouds {
        Some(mut clouds) => {
            let mut next = *clouds;
            if weather.apply(&mut next) {
                *clouds = next;
            }
        }
        None => {
            let mut next = level_clouds(level.as_deref());
            weather.apply(&mut next);
            commands.insert_resource(next);
        }
    }
}

/// Installs [`drive_cloud_weather`] and the three weather effect drivers, and reads the project's
/// [`WeatherConfig`] at `Startup` (added by [`crate::VfxPlugin`]).
pub struct WeatherPlugin;

impl Plugin for WeatherPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, load_weather_config).add_systems(
            Update,
            (
                start_level_weather,
                drive_cloud_weather,
                drive_scene_wind,
                drive_surface_weather,
                drive_weather_air,
                crate::precip::step_precipitation,
            )
                .chain(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32, tol: f32) -> bool {
        (a - b).abs() <= tol
    }

    #[test]
    fn a_restarted_clock_runs_from_the_origin_and_resumes_where_it_left() {
        let clouds = CloudSettings {
            time_s: 30.0,
            ..CloudSettings::default()
        };
        let mut weather = Weather::from_clouds(&clouds);
        let start = (weather.clock_s, weather.drift_m);
        weather.step(4.0);
        let before = (weather.clock_s, weather.drift_m);
        let saved = weather.restart_clock();
        assert_eq!((weather.clock_s, weather.drift_m), start);
        weather.step(2.0);
        let once = (weather.clock_s, weather.drift_m);
        weather.resume_clock(saved);
        assert!((weather.clock_s - (before.0 + 2.0)).abs() < 1.0e-9);
        // A second stretch from another point of the clock runs the same phases as the first.
        weather.step(1.5);
        let saved = weather.restart_clock();
        weather.step(2.0);
        assert_eq!((weather.clock_s, weather.drift_m), once);
        weather.resume_clock(saved);
        assert!((weather.clock_s - (before.0 + 5.5)).abs() < 1.0e-9);
    }

    #[test]
    fn presets_resolve_and_are_sane() {
        for name in WEATHER_PRESETS {
            let state = WeatherState::preset(name).expect("preset");
            assert_eq!(state, state.sanitized(), "{name} is in range");
        }
        assert!(WeatherState::preset("hail").is_none());
        let clear = WeatherState::preset("clear").expect("clear");
        let storm = WeatherState::preset("storm").expect("storm");
        assert!(clear.coverage < storm.coverage && clear.density < storm.density);
    }

    /// Frame steps that add up to the duration end the transition on the last of them, though
    /// their f32 sum falls a few ulps short.
    #[test]
    fn a_transition_ends_on_the_frame_its_duration_runs_out() {
        let fair = WeatherState::preset("clear").expect("clear");
        let storm = WeatherState::preset("storm").expect("storm");
        let mut weather = Weather::new(fair);
        weather.transition_to(storm, 2.0);
        for _ in 0..119 {
            weather.step(1.0 / 60.0);
        }
        assert!(weather.transitioning(), "one frame left");
        weather.step(1.0 / 60.0);
        assert!(!weather.transitioning());
        assert_eq!(weather.current(), storm.sanitized());
    }

    /// A transition eases from the start to the target: half way it is half way (smoothstep
    /// is symmetric), it ends exactly on the target, and it does not overshoot.
    #[test]
    fn transition_eases_to_the_target() {
        let mut weather = Weather::new(WeatherState::CLEAR);
        weather.transition_to(WeatherState::OVERCAST, 10.0);
        assert!(weather.transitioning());
        assert_eq!(weather.current(), WeatherState::CLEAR);
        let mut last = 0.0;
        for k in 0..600 {
            weather.step(1.0 / 60.0);
            let c = weather.current().coverage;
            assert!(c >= last - 1.0e-6, "coverage falls at frame {k}");
            assert!(c <= WeatherState::OVERCAST.coverage + 1.0e-6);
            last = c;
            if k == 299 {
                assert!(
                    close(c, 0.5 * WeatherState::OVERCAST.coverage, 1.0e-3),
                    "half way: {c}"
                );
            }
        }
        // 600 frames of 1/60 s are 10 s within f32 rounding; one more frame settles it.
        weather.step(1.0 / 60.0);
        assert!(!weather.transitioning());
        assert_eq!(weather.current(), WeatherState::OVERCAST);
        assert_eq!(weather.cloud_values(), WeatherState::OVERCAST, "exact at rest");
        assert!((weather.clock_s() - 601.0 / 60.0).abs() < 1.0e-3);
    }

    /// The stepped values never leave the segment between the start and the target (ADR-5667):
    /// the last eased frame of a 25 % to 60 % second used to round to 154/256 (past 0.6) and then
    /// settle on 0.6, which shares that 1/256 step, so the renderer kept the map of the overshoot.
    /// Coverage rises and type and density fall here, so both ends of the clamp are checked.
    #[test]
    fn stepped_values_stay_between_the_start_and_the_target() {
        let from = WeatherState {
            coverage: 0.25,
            cloud_type: 0.7,
            density: 1.3,
            ..WeatherState::FAIR
        };
        let to = WeatherState {
            coverage: 0.6,
            cloud_type: 0.3,
            density: 0.37,
            ..from
        };
        for (seconds, dt) in [(1.0, 1.0 / 60.0), (2.0, 1.0 / 60.0), (1.0, 1.0 / 144.0)] {
            let mut weather = Weather::new(from);
            weather.transition_to(to, seconds);
            let mut frame = 0;
            while weather.transitioning() {
                weather.step(dt);
                frame += 1;
                let v = weather.cloud_values();
                let inside = |value: f32, a: f32, b: f32| a.min(b) <= value && value <= a.max(b);
                assert!(
                    inside(v.coverage, from.coverage, to.coverage),
                    "{seconds} s at {dt}: frame {frame} writes coverage {} outside {}..{}",
                    v.coverage,
                    from.coverage,
                    to.coverage
                );
                assert!(
                    inside(v.cloud_type, from.cloud_type, to.cloud_type),
                    "{seconds} s at {dt}: frame {frame} writes type {}",
                    v.cloud_type
                );
                assert!(
                    inside(v.density, from.density, to.density),
                    "{seconds} s at {dt}: frame {frame} writes density {}",
                    v.density
                );
                assert!(frame < 10_000, "the transition ends");
            }
            assert_eq!(weather.cloud_values().coverage, to.coverage, "exact at rest");
        }
    }

    /// Changing the wind at t = 1000 s carries the clouds on: the drift the cloud layer sees moves
    /// by about `wind × dt` each frame, never by `Δwind × t` (40 km here), and equals the
    /// integrated drift.
    #[test]
    fn wind_change_keeps_the_drift_continuous() {
        let mut weather = Weather::new(WeatherState {
            wind_m_per_s: [40.0, 0.0],
            ..WeatherState::FAIR
        });
        let mut clouds = CloudSettings::coverage_layer(0.3);
        let dt = 1.0 / 60.0;
        for _ in 0..60_000 {
            weather.step(dt);
        }
        weather.apply(&mut clouds);
        let mut prev = clouds.drift_m();
        assert!((f64::from(prev[0]) - weather.drift_m()[0]).abs() < 0.5);
        weather.transition_to(
            WeatherState {
                wind_m_per_s: [0.0, 40.0],
                ..WeatherState::FAIR
            },
            2.0,
        );
        for frame in 0..240 {
            weather.step(dt);
            weather.apply(&mut clouds);
            let now = clouds.drift_m();
            let moved = ((now[0] - prev[0]).powi(2) + (now[1] - prev[1]).powi(2)).sqrt();
            assert!(
                moved <= 40.0 * dt + 0.2,
                "frame {frame}: the clouds jumped {moved} m"
            );
            let integrated = weather.drift_m();
            assert!((f64::from(now[0]) - integrated[0]).abs() < 0.5);
            assert!((f64::from(now[1]) - integrated[1]).abs() < 0.5);
            prev = now;
        }
        assert_eq!(clouds.wind_m_per_s, [0.0, 40.0]);
    }

    /// A 10 s clear-to-overcast transition at 60 fps: no frame is a weather cut for the renderer,
    /// and the weather map (coverage and type) changes at most once per 1/256 step.
    #[test]
    fn transition_steps_bound_the_weather_rebuilds() {
        let mut weather = Weather::new(WeatherState::CLEAR);
        weather.transition_to(WeatherState::OVERCAST, 10.0);
        let mut clouds = CloudSettings::default();
        weather.apply(&mut clouds);
        let mut prev = clouds;
        let mut rebuilds = 0;
        for _ in 0..700 {
            weather.step(1.0 / 60.0);
            weather.apply(&mut clouds);
            assert!(
                !regia_render::cloud_weather_cut(prev.weather_sample(), clouds.weather_sample()),
                "a transition frame is a weather cut: {:?} -> {:?}",
                prev.weather_sample(),
                clouds.weather_sample()
            );
            if (prev.coverage, prev.cloud_type) != (clouds.coverage, clouds.cloud_type) {
                rebuilds += 1;
            }
            prev = clouds;
        }
        let steps = ((WeatherState::OVERCAST.coverage - WeatherState::CLEAR.coverage) / WEATHER_VALUE_STEP)
            .ceil() as u32
            + ((WeatherState::CLEAR.cloud_type - WeatherState::OVERCAST.cloud_type) / WEATHER_VALUE_STEP)
                .ceil() as u32
            + 1;
        assert!(
            rebuilds <= steps,
            "{rebuilds} weather map changes for {steps} steps"
        );
        assert_eq!(clouds.coverage, WeatherState::OVERCAST.coverage, "exact at rest");
    }

    /// Switching at once to a different weather is a cut; the renderer's own fields survive.
    #[test]
    fn set_now_is_a_cut_and_keeps_the_layer_switches() {
        let mut weather = Weather::new(WeatherState::FAIR);
        let mut clouds = CloudSettings {
            shadow_map: true,
            fog_coupling: true,
            altitude_bottom: 1800.0,
            ..CloudSettings::default()
        };
        weather.apply(&mut clouds);
        let before = clouds;
        weather.set_now(WeatherState::STORM);
        weather.step(1.0 / 60.0);
        assert!(weather.apply(&mut clouds));
        assert!(regia_render::cloud_weather_cut(
            before.weather_sample(),
            clouds.weather_sample()
        ));
        assert!(clouds.shadow_map && clouds.fog_coupling);
        assert_eq!(clouds.altitude_bottom, 1800.0);
        assert_eq!(clouds.coverage, WeatherState::STORM.coverage);
    }

    /// The genus presets (ADR-1408) resolve and write their layer's heights, tiles and look; the
    /// older presets still leave those renderer fields alone.
    #[test]
    fn genus_presets_write_their_layer() {
        for name in regia_render::CloudGenus::NAMES {
            let state = WeatherState::preset(name).expect("genus preset");
            assert!(WEATHER_PRESETS.contains(&name));
            let genus = state.genus.expect("carries its genus");
            assert_eq!(genus.name(), name);
            let mut clouds = CloudSettings {
                altitude_bottom: 1800.0,
                shadow_map: true,
                ..CloudSettings::default()
            };
            Weather::new(state).apply(&mut clouds);
            let layer = genus.layer();
            assert_eq!(clouds.altitude_bottom, layer.altitude_bottom, "{name}");
            assert_eq!(clouds.altitude_top, layer.altitude_top, "{name}");
            assert_eq!(clouds.look, layer.look, "{name}");
            assert_eq!(clouds.coverage, layer.coverage, "{name}");
            assert!(clouds.shadow_map, "{name} keeps the switches");
        }
        let mut clouds = CloudSettings {
            altitude_bottom: 1800.0,
            ..CloudSettings::default()
        };
        Weather::new(WeatherState::CLOUDY).apply(&mut clouds);
        assert_eq!(clouds.altitude_bottom, 1800.0);
        assert_eq!(clouds.look, regia_render::CloudLook::LEGACY);
    }

    /// Cirrus, started from the renderer's defaults, writes the genus layer. Rain leaves it.
    #[test]
    fn cirrus_rewrites_default_heights_and_rain_leaves_them() {
        let mut clouds = CloudSettings::default();
        let switches = (clouds.shadow_map, clouds.fog_coupling, clouds.overcast_ibl);
        let defaults = (
            clouds.altitude_bottom,
            clouds.altitude_top,
            clouds.shape_scale_m,
            clouds.detail_scale_m,
        );
        let cirrus = WeatherState::preset("cirrus").expect("cirrus");
        Weather::new(cirrus).apply(&mut clouds);
        let layer = cirrus.genus.expect("genus").layer();
        assert_eq!(clouds.altitude_bottom, layer.altitude_bottom);
        assert_eq!(clouds.altitude_top, layer.altitude_top);
        assert_eq!(clouds.shape_scale_m, layer.shape_scale_m);
        assert_eq!(clouds.detail_scale_m, layer.detail_scale_m);
        assert_ne!(clouds.altitude_bottom, defaults.0);
        assert_eq!(clouds.altitude_bottom, 1300.0);
        assert_eq!(clouds.shape_scale_m, 1500.0);
        assert_eq!(
            (clouds.shadow_map, clouds.fog_coupling, clouds.overcast_ibl),
            switches
        );

        let mut rain = CloudSettings::default();
        let before = (
            rain.altitude_bottom,
            rain.shape_scale_m,
            rain.detail_scale_m,
            rain.look,
        );
        Weather::new(WeatherState::RAIN).apply(&mut rain);
        assert_eq!(
            (
                rain.altitude_bottom,
                rain.shape_scale_m,
                rain.detail_scale_m,
                rain.look
            ),
            before
        );
        assert_eq!(rain.altitude_bottom, 1500.0);
        assert_eq!(rain.shape_scale_m, 2600.0);
    }

    /// Stratocumulus to cumulus over 10 s: the cloud top rises without going back, ends exactly
    /// on the cumulus layer, and each frame moves it by a small part of the change.
    #[test]
    fn genus_transition_blends_the_layers() {
        let sc = WeatherState::preset("stratocumulus").expect("sc");
        let cu = WeatherState::preset("cumulus").expect("cu");
        let mut weather = Weather::new(sc);
        let mut clouds = CloudSettings::default();
        weather.apply(&mut clouds);
        weather.transition_to(cu, 10.0);
        let mut last = clouds.altitude_top;
        let span = cu.genus.expect("cu").layer().altitude_top - last;
        for frame in 0..620 {
            weather.step(1.0 / 60.0);
            weather.apply(&mut clouds);
            assert!(clouds.altitude_top >= last - 1.0e-3, "frame {frame} falls");
            assert!(clouds.altitude_top - last <= span * 0.01, "frame {frame} jumps");
            last = clouds.altitude_top;
        }
        assert_eq!(
            regia_render::CloudGenusLayer::of(&clouds),
            regia_render::CloudGenus::Cumulus.layer()
        );
        // To a preset without a genus the cumulus layer holds, then stays.
        weather.transition_to(WeatherState::FAIR, 2.0);
        for _ in 0..200 {
            weather.step(1.0 / 60.0);
            weather.apply(&mut clouds);
            assert_eq!(
                clouds.altitude_top,
                regia_render::CloudGenus::Cumulus.layer().altitude_top
            );
        }
        assert_eq!(clouds.coverage, WeatherState::FAIR.coverage);
    }

    /// Paused weather and a zero or bad frame time move nothing.
    #[test]
    fn paused_weather_holds() {
        let mut weather = Weather::new(WeatherState::FAIR);
        weather.transition_to(WeatherState::STORM, 5.0);
        weather.paused = true;
        weather.step(1.0);
        assert_eq!(weather.clock_s(), 0.0);
        assert_eq!(weather.progress(), 0.0);
        weather.paused = false;
        weather.step(f32::NAN);
        weather.step(-1.0);
        assert_eq!(weather.clock_s(), 0.0);
        weather.step(2.5);
        assert!(close(weather.progress(), 0.5, 1.0e-6));
        assert!(close(weather.remaining_s(), 2.5, 1.0e-6));
    }

    /// Taking over a layer from its settings moves nothing: the drift and values match.
    #[test]
    fn from_clouds_takes_over_without_a_jump() {
        let clouds = CloudSettings {
            wind_m_per_s: [7.0, -2.0],
            time_s: 300.0,
            ..CloudSettings::coverage_layer(0.45)
        };
        let weather = Weather::from_clouds(&clouds);
        let mut copy = clouds;
        weather.apply(&mut copy);
        assert_eq!(copy.coverage, clouds.coverage);
        let (a, b) = (copy.drift_m(), clouds.drift_m());
        assert!(
            close(a[0], b[0], 1.0e-2) && close(a[1], b[1], 1.0e-2),
            "{a:?} vs {b:?}"
        );
    }

    /// In an app: inserting the resource makes the cloud layer follow it frame by frame on the
    /// game clock.
    #[test]
    fn plugin_writes_the_cloud_layer_on_game_time() {
        let mut app = App::new();
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(100),
        ));
        app.add_plugins((regia_app::MinimalPlugins, WeatherPlugin));
        app.update();
        assert!(
            app.world().get_resource::<CloudSettings>().is_none(),
            "no weather, no cloud layer written"
        );
        let mut weather = Weather::new(WeatherState::CLEAR);
        weather.transition_to(WeatherState::CLOUDY, 1.0);
        app.insert_resource(weather);
        for _ in 0..15 {
            app.update();
        }
        let clouds = *app.world().resource::<CloudSettings>();
        assert_eq!(clouds.coverage, WeatherState::CLOUDY.coverage);
        assert!(
            clouds.time_s > 1.0,
            "the weather clock runs on game time: {}",
            clouds.time_s
        );
        let weather = app.world().resource::<Weather>();
        assert!(!weather.transitioning());
    }

    fn cirrus_override() -> CloudLookOverride {
        CloudLookOverride {
            cirrus_coverage: Some(0.65),
            cirrus_optical_depth: Some(0.11),
            cirrus_scale_m: Some(1500.0),
            ..CloudLookOverride::default()
        }
    }

    /// An empty override leaves the stepped cloud layer bit for bit as before ADR-1492.
    #[test]
    fn an_empty_override_changes_nothing() {
        let mut plain = Weather::new(WeatherState::preset("cirrus").expect("cirrus"));
        let mut empty = plain.clone();
        empty.look_override = CloudLookOverride::default();
        let (mut a, mut b) = (CloudSettings::default(), CloudSettings::default());
        for _ in 0..30 {
            plain.step(1.0 / 60.0);
            empty.step(1.0 / 60.0);
            plain.apply(&mut a);
            empty.apply(&mut b);
            assert_eq!(a, b);
        }
        assert_eq!(a.look.cirrus, CloudGenus::Cirrus.layer().look.cirrus);
    }

    /// The override survives every weather step over the genus look; clearing it brings the
    /// genus look back on the next step.
    #[test]
    fn the_override_persists_over_steps_and_clearing_restores_the_genus_look() {
        let mut weather = Weather::new(WeatherState::preset("cirrus").expect("cirrus"));
        weather.look_override = cirrus_override();
        let mut clouds = CloudSettings::default();
        let genus_look = CloudGenus::Cirrus.layer().look;
        for _ in 0..120 {
            weather.step(1.0 / 60.0);
            weather.apply(&mut clouds);
            assert_eq!(clouds.look.cirrus.coverage, 0.65);
            assert_eq!(clouds.look.cirrus.optical_depth, 0.11);
            assert_eq!(clouds.look.cirrus.scale_m, 1500.0);
            // Fields the override leaves alone keep the genus layer's value.
            assert_eq!(clouds.look.cirrus.stretch, genus_look.cirrus.stretch);
            assert_eq!(clouds.look.erosion, genus_look.erosion);
        }
        weather.look_override = CloudLookOverride::default();
        weather.step(1.0 / 60.0);
        weather.apply(&mut clouds);
        assert_eq!(clouds.look, genus_look);
    }

    /// The override stays through a weather transition and values are sanitized.
    #[test]
    fn the_override_survives_a_transition_and_is_clamped() {
        let mut weather = Weather::new(WeatherState::preset("cirrus").expect("cirrus"));
        weather.look_override = CloudLookOverride {
            cirrus_coverage: Some(7.0),
            ..CloudLookOverride::default()
        };
        weather.transition_to(WeatherState::STORM, 2.0);
        let mut clouds = CloudSettings::default();
        for _ in 0..150 {
            weather.step(1.0 / 60.0);
            weather.apply(&mut clouds);
            assert_eq!(clouds.look.cirrus.coverage, 1.0);
        }
    }

    /// ADR-1509: the fibrous cirrus fields pin like the others; unset ones keep the genus
    /// layer's (zero, the ADR-1470 trails), and they are clamped.
    #[test]
    fn the_fibrous_cirrus_fields_pin_and_clamp() {
        let mut weather = Weather::new(WeatherState::preset("cirrus").expect("cirrus"));
        weather.look_override = CloudLookOverride {
            cirrus_fiber: Some(0.8),
            cirrus_fiber_scale_m: Some(300.0),
            cirrus_shear: Some(4.0),
            cirrus_hook: Some(0.5),
            ..CloudLookOverride::default()
        };
        let mut clouds = CloudSettings::default();
        for _ in 0..30 {
            weather.step(1.0 / 60.0);
            weather.apply(&mut clouds);
        }
        let c = clouds.look.cirrus;
        assert_eq!(
            (c.fiber, c.fiber_scale_m, c.shear, c.hook),
            (0.8, 300.0, 1.0, 0.5)
        );
        assert_eq!(
            (c.breakup, c.shear_deg),
            (0.0, 0.0),
            "unset fields keep the genus value"
        );
        assert!(c.is_fibrous());
        assert!(!CloudGenus::Cirrus.layer().look.cirrus.is_fibrous());
        let merged = weather.look_override.merged(CloudLookOverride {
            cirrus_breakup: Some(0.6),
            cirrus_shear_deg: Some(-20.0),
            ..CloudLookOverride::default()
        });
        assert_eq!(merged.cirrus_fiber, Some(0.8));
        assert_eq!(merged.cirrus_breakup, Some(0.6));
        assert_eq!(merged.cirrus_shear_deg, Some(-20.0));
        assert!(
            !CloudLookOverride {
                cirrus_hook: Some(f32::INFINITY),
                ..Default::default()
            }
            .is_finite()
        );
    }

    /// ADR-1516: `cirrus_crisp` pins and clamps like the other cirrus fields, merges, and alone
    /// switches the fibrous sheet on; unset it keeps the genus layer's 0.
    #[test]
    fn the_crisp_cirrus_field_pins_and_clamps() {
        let mut weather = Weather::new(WeatherState::preset("cirrus").expect("cirrus"));
        let mut clouds = CloudSettings::default();
        weather.step(1.0 / 60.0);
        weather.apply(&mut clouds);
        assert_eq!(clouds.look.cirrus.crisp, 0.0, "off unless pinned");
        weather.look_override = CloudLookOverride {
            cirrus_crisp: Some(3.0),
            ..CloudLookOverride::default()
        };
        for _ in 0..30 {
            weather.step(1.0 / 60.0);
            weather.apply(&mut clouds);
        }
        let c = clouds.look.cirrus;
        assert_eq!(c.crisp, 1.0, "clamped to 0..1");
        assert_eq!((c.fiber, c.shear, c.hook, c.breakup), (0.0, 0.0, 0.0, 0.0));
        assert!(c.is_fibrous());
        let merged = weather.look_override.merged(CloudLookOverride {
            cirrus_fiber: Some(1.0),
            ..CloudLookOverride::default()
        });
        assert_eq!((merged.cirrus_crisp, merged.cirrus_fiber), (Some(3.0), Some(1.0)));
        assert!(
            !CloudLookOverride {
                cirrus_crisp: Some(f32::NAN),
                ..Default::default()
            }
            .is_finite()
        );
    }

    #[test]
    fn merged_keeps_earlier_fields_and_new_ones_win() {
        let base = cirrus_override();
        let next = CloudLookOverride {
            cirrus_coverage: Some(0.3),
            erosion: Some(0.2),
            ..CloudLookOverride::default()
        };
        let both = base.merged(next);
        assert_eq!(both.cirrus_coverage, Some(0.3));
        assert_eq!(both.cirrus_scale_m, Some(1500.0));
        assert_eq!(both.erosion, Some(0.2));
        assert!(!both.is_empty() && CloudLookOverride::default().is_empty());
        assert!(
            !CloudLookOverride {
                erosion: Some(f32::NAN),
                ..Default::default()
            }
            .is_finite()
        );
    }
}
