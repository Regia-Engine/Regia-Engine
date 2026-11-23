// SPDX-License-Identifier: MPL-2.0
//! What falls: the rates, drop sizes and fall speeds of a weather (spec 99 § 3.2). Pure functions
//! of a [`WeatherState`] and the project's [`WeatherConfig`].
//!
//! Drop sizes are the Marshall and Palmer (1948) exponential distribution; fall speeds are the
//! Atlas, Srivastava and Sekhon (1973) fit; snow aggregates fall at 0.5-1.5 m/s (Locatelli and
//! Hobbs 1974). The rain-to-snow band sits near the observed transition over land (Dai 2008). The
//! `p²` rate curve and the class split are Regia's own.

use regia_core::math::{det_exp, det_ln, det_powf_nonnegative};
use serde::{Deserialize, Serialize};

use super::{WeatherConfig, WeatherState};

/// Size classes of the drawn rain, equal-count quantiles of the visible tail.
pub const RAIN_CLASSES: usize = 8;
/// Size classes of the drawn snow.
pub const SNOW_CLASSES: usize = 4;
/// Fall speed of each snow class (m/s).
pub const SNOW_FALL_SPEEDS_M_PER_S: [f32; SNOW_CLASSES] = [0.6, 0.85, 1.1, 1.4];

/// `N0` of the Marshall-Palmer distribution, per m³ per mm.
const MARSHALL_PALMER_N0: f32 = 8000.0;
/// `Λ = 4.1 R^-0.21`, per mm.
const MARSHALL_PALMER_A: f32 = 4.1;
const MARSHALL_PALMER_B: f32 = -0.21;
/// The largest drop drawn (mm): bigger ones break up.
const MAX_DROP_MM: f32 = 6.0;
/// Koschmieder's constant at a 2 % contrast threshold.
const KOSCHMIEDER: f32 = 3.912;

/// What falls from the sky.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrecipitationKind {
    /// By the temperature ([`crate::weather::PrecipitationConfig::snow_all_below_c`] and
    /// [`crate::weather::PrecipitationConfig::rain_all_above_c`]).
    #[default]
    Auto,
    /// Always rain.
    Rain,
    /// Always snow.
    Snow,
    /// Half and half.
    Sleet,
}

impl PrecipitationKind {
    /// The name the agent protocol uses.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Rain => "rain",
            Self::Snow => "snow",
            Self::Sleet => "sleet",
        }
    }

    /// The kind of an agent-protocol name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "rain" => Some(Self::Rain),
            "snow" => Some(Self::Snow),
            "sleet" => Some(Self::Sleet),
            _ => None,
        }
    }
}

/// What falls now, in physical units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PrecipitationRates {
    /// 0 rain .. 1 snow.
    pub snow_fraction: f32,
    /// Liquid reaching the ground, mm/h.
    pub rain_mm_per_h: f32,
    /// Snow as water equivalent, mm/h.
    pub snow_we_mm_per_h: f32,
    /// Fresh snow depth, mm/h (water equivalent × snow ratio).
    pub snow_depth_mm_per_h: f32,
    /// The rain's size distribution slope, per mm (0 when no rain falls).
    pub drop_lambda_per_mm: f32,
    /// Horizontal visibility from the precipitation alone, m ([`f32::INFINITY`] when none falls).
    pub visibility_m: f32,
}

impl Default for PrecipitationRates {
    fn default() -> Self {
        Self::NONE
    }
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn lambda_of(rain_mm_per_h: f32) -> f32 {
    if rain_mm_per_h > 0.0 {
        // det_powf_nonnegative / det_ln / det_exp: the settled weather is replay-visible (the class
        // diameters and fall speeds feed the precipitation uniform), so it must not follow
        // the platform libm (fix-vfx1-determinism finding 7).
        MARSHALL_PALMER_A * det_powf_nonnegative(rain_mm_per_h, MARSHALL_PALMER_B)
    } else {
        0.0
    }
}

fn visibility_of(extinction_per_m: f32) -> f32 {
    if extinction_per_m > 0.0 {
        KOSCHMIEDER / extinction_per_m
    } else {
        f32::INFINITY
    }
}

impl PrecipitationRates {
    /// Nothing falls.
    pub const NONE: Self = Self {
        snow_fraction: 0.0,
        rain_mm_per_h: 0.0,
        snow_we_mm_per_h: 0.0,
        snow_depth_mm_per_h: 0.0,
        drop_lambda_per_mm: 0.0,
        visibility_m: f32::INFINITY,
    };

    /// The precipitation's extinction `σ_p` (1/m), 0 when nothing falls.
    #[must_use]
    pub fn extinction_per_m(&self) -> f32 {
        if self.visibility_m.is_finite() && self.visibility_m > 0.0 {
            KOSCHMIEDER / self.visibility_m
        } else {
            0.0
        }
    }

    /// `true` when nothing falls.
    #[must_use]
    pub fn is_none(&self) -> bool {
        self.rain_mm_per_h <= 0.0 && self.snow_we_mm_per_h <= 0.0
    }

    /// `a` blended toward `b` by `t`, field by field; the slope comes from the blended rain rate
    /// and the visibility from the blended extinction.
    #[must_use]
    pub fn blend(a: &Self, b: &Self, t: f32, config: &WeatherConfig) -> Self {
        let t = t.clamp(0.0, 1.0);
        let mix = |x: f32, y: f32| x + (y - x) * t;
        let rain = mix(a.rain_mm_per_h, b.rain_mm_per_h);
        let snow = mix(a.snow_we_mm_per_h, b.snow_we_mm_per_h);
        Self {
            snow_fraction: mix(a.snow_fraction, b.snow_fraction),
            rain_mm_per_h: rain,
            snow_we_mm_per_h: snow,
            snow_depth_mm_per_h: snow * config.precipitation.snow_ratio,
            drop_lambda_per_mm: lambda_of(rain),
            visibility_m: visibility_of(mix(a.extinction_per_m(), b.extinction_per_m())),
        }
    }
}

impl WeatherState {
    /// The fraction of the precipitation that falls as snow, `0..1`.
    #[must_use]
    pub fn snow_fraction(&self, config: &WeatherConfig) -> f32 {
        match self.precipitation_kind {
            PrecipitationKind::Rain => 0.0,
            PrecipitationKind::Snow => 1.0,
            PrecipitationKind::Sleet => 0.5,
            PrecipitationKind::Auto => {
                let band = &config.precipitation;
                1.0 - smoothstep(band.snow_all_below_c, band.rain_all_above_c, self.temperature_c)
            }
        }
    }

    /// What falls in this weather when it is settled ([`crate::weather::Weather::rates`] blends
    /// two of these during a transition).
    #[must_use]
    pub fn rates(&self, config: &WeatherConfig) -> PrecipitationRates {
        let s = self.snow_fraction(config);
        let p2 = self.precipitation * self.precipitation;
        let c = &config.precipitation;
        let rain = c.max_rain_mm_per_h * p2 * (1.0 - s);
        let snow = c.max_snow_we_mm_per_h * p2 * s;
        let v = &config.visibility;
        let extinction = KOSCHMIEDER * p2 * ((1.0 - s) / v.rain_visibility_m + s / v.snow_visibility_m);
        PrecipitationRates {
            snow_fraction: s,
            rain_mm_per_h: rain,
            snow_we_mm_per_h: snow,
            snow_depth_mm_per_h: snow * c.snow_ratio,
            drop_lambda_per_mm: lambda_of(rain),
            visibility_m: visibility_of(extinction),
        }
    }
}

/// The diameter of each drawn rain class (mm): equal-count quantiles of the distribution's tail
/// above the visible size, capped at 6 mm. All at the visible size when no rain falls.
pub fn rain_class_diameters_mm(
    rates: &PrecipitationRates,
    config: &WeatherConfig,
    out: &mut [f32; RAIN_CLASSES],
) {
    let visible = config.precipitation.visible_drop_mm;
    let lambda = rates.drop_lambda_per_mm;
    for (k, d) in out.iter_mut().enumerate() {
        *d = if lambda > 0.0 {
            let quantile = (k as f32 + 0.5) / RAIN_CLASSES as f32;
            (visible - det_ln(1.0 - quantile) / lambda).min(MAX_DROP_MM)
        } else {
            visible
        };
    }
}

/// The terminal fall speed of a rain drop of `diameter_mm` (m/s), in `[0, 9.65]`: the fit
/// rises toward 9.65 and correction terms below the f32 ulp of 9.65 read as exactly 9.65 at
/// absurd diameters (real classes stay strictly below it).
#[must_use]
pub fn rain_fall_speed_m_per_s(diameter_mm: f32) -> f32 {
    (9.65 - 10.3 * det_exp(-0.6 * diameter_mm)).max(0.0)
}

/// Drops above the visible size per unit volume, per m³.
fn visible_drops_per_m3(rain_mm_per_h: f32, visible_mm: f32) -> f32 {
    let lambda = lambda_of(rain_mm_per_h);
    if lambda > 0.0 {
        MARSHALL_PALMER_N0 / lambda * det_exp(-lambda * visible_mm)
    } else {
        0.0
    }
}

/// The share of the rain budget drawn: the drops above the visible size per unit volume,
/// normalized to 1 at the project's maximum rate.
#[must_use]
pub fn rain_count_factor(rates: &PrecipitationRates, config: &WeatherConfig) -> f32 {
    let c = &config.precipitation;
    let full = visible_drops_per_m3(c.max_rain_mm_per_h, c.visible_drop_mm);
    if full <= 0.0 {
        return 0.0;
    }
    (visible_drops_per_m3(rates.rain_mm_per_h, c.visible_drop_mm) / full).clamp(0.0, 1.0)
}

/// The share of the snow budget drawn: linear in the water equivalent rate.
#[must_use]
pub fn snow_count_factor(rates: &PrecipitationRates, config: &WeatherConfig) -> f32 {
    (rates.snow_we_mm_per_h / config.precipitation.max_snow_we_mm_per_h).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::super::{WEATHER_PRESETS, Weather};
    use super::*;

    fn config() -> WeatherConfig {
        WeatherConfig::default()
    }

    /// Pinned `to_bits` of the 50 mm/h class diameters (`class_diameters_grow_with_class_and_rate`).
    const STORM_CLASS_DIAMETER_BITS: [u32; RAIN_CLASSES] = [
        1_065_653_484,
        1_066_319_266,
        1_067_096_491,
        1_068_030_118,
        1_069_199_368,
        1_070_764_816,
        1_073_141_452,
        1_075_997_304,
    ];
    /// Pinned `to_bits` of `rain_fall_speed_m_per_s(3.0)` (`fall_speeds_match_the_fit`).
    const FALL_SPEED_3MM_BITS: u32 = 1_090_408_774;

    /// A drop of any diameter — however huge — falls inside the documented `[0, 9.65]`:
    /// the exponential underflows to 0 in the deterministic kernel instead of producing
    /// inf/NaN garbage (Muse review of fix-vfx1-determinism finding 3). At absurd diameters
    /// the fit's correction is far below the f32 ulp of 9.65, so the asymptote reads as
    /// exactly 9.65; real classes stay strictly below it.
    #[test]
    fn fall_speed_stays_in_range_for_huge_drops() {
        for d in [0.0f32, 3.0, 1217.0, f32::MAX] {
            let v = rain_fall_speed_m_per_s(d);
            assert!((0.0..=9.65).contains(&v), "diameter {d}: {v}");
        }
        assert_eq!(rain_fall_speed_m_per_s(1217.0), rain_fall_speed_m_per_s(f32::MAX));
        assert!(rain_fall_speed_m_per_s(3.0) < 9.65);
        assert_eq!(rain_fall_speed_m_per_s(0.0), 0.0);
    }

    /// § 11 test 1: the twelve presets are in range, the old presets' old fields did not move,
    /// `snow` is all snow and `rain` all rain.
    #[test]
    fn presets_are_in_range_and_the_old_ones_are_unchanged() {
        assert_eq!(WEATHER_PRESETS.len(), 12);
        for name in WEATHER_PRESETS {
            let state = WeatherState::preset(name).expect("preset");
            assert_eq!(state, state.sanitized(), "{name}");
            assert_eq!(state.precipitation_kind, PrecipitationKind::Auto, "{name}");
        }
        // name, coverage, type, density, wind, precipitation (ADR-429's values, bit for bit).
        let old = [
            ("clear", 0.0, 0.5, 0.04, [3.0, 1.0], 0.0),
            ("fair", 0.3, 0.5, 0.04, [6.0, 2.0], 0.0),
            ("cloudy", 0.6, 0.6, 0.05, [8.0, 3.0], 0.0),
            ("overcast", 0.95, 0.15, 0.06, [10.0, 3.0], 0.2),
            ("storm", 1.0, 0.95, 0.09, [18.0, 6.0], 1.0),
        ];
        for (name, coverage, cloud_type, density, wind, precipitation) in old {
            let s = WeatherState::preset(name).expect("preset");
            assert_eq!(
                (
                    s.coverage,
                    s.cloud_type,
                    s.density,
                    s.wind_m_per_s,
                    s.precipitation
                ),
                (coverage, cloud_type, density, wind, precipitation),
                "{name}"
            );
        }
        let c = config();
        assert_eq!(WeatherState::SNOW.rates(&c).snow_fraction, 1.0);
        assert_eq!(WeatherState::BLIZZARD.rates(&c).snow_fraction, 1.0);
        assert_eq!(WeatherState::RAIN.rates(&c).snow_fraction, 0.0);
    }

    /// § 11 test 2.
    #[test]
    fn snow_fraction_follows_the_kind_and_the_temperature() {
        let c = config();
        let at = |kind, t| {
            WeatherState {
                precipitation_kind: kind,
                temperature_c: t,
                ..WeatherState::STORM
            }
            .snow_fraction(&c)
        };
        assert_eq!(at(PrecipitationKind::Rain, -20.0), 0.0);
        assert_eq!(at(PrecipitationKind::Snow, 30.0), 1.0);
        assert_eq!(at(PrecipitationKind::Sleet, 30.0), 0.5);
        assert_eq!(at(PrecipitationKind::Auto, 0.0), 1.0);
        assert_eq!(at(PrecipitationKind::Auto, -5.0), 1.0);
        assert_eq!(at(PrecipitationKind::Auto, 2.5), 0.0);
        assert_eq!(at(PrecipitationKind::Auto, 9.0), 0.0);
        assert!((at(PrecipitationKind::Auto, 1.25) - 0.5).abs() < 1.0e-6);
        let mut last = 1.0;
        for k in 0..=100 {
            let s = at(PrecipitationKind::Auto, k as f32 * 0.025);
            assert!(s <= last, "monotone at {k}");
            last = s;
        }
    }

    /// § 11 test 2: rain to snow through `Auto` crosses the band with no step in any rate.
    #[test]
    fn a_rain_to_snow_transition_moves_every_rate_smoothly_and_lands_on_the_target() {
        let c = config();
        let mut weather = Weather::new(WeatherState::RAIN);
        let target = WeatherState {
            precipitation: 0.35,
            ..WeatherState::SNOW
        };
        weather.transition_to(target, 10.0);
        let mut last = weather.rates(&c);
        assert_eq!(last, WeatherState::RAIN.rates(&c));
        for frame in 0..610 {
            weather.step(1.0 / 60.0);
            let now = weather.rates(&c);
            assert!(
                (now.snow_fraction - last.snow_fraction).abs() < 0.01,
                "frame {frame}"
            );
            assert!(
                (now.rain_mm_per_h - last.rain_mm_per_h).abs() < 0.1,
                "frame {frame}"
            );
            assert!(
                (now.snow_we_mm_per_h - last.snow_we_mm_per_h).abs() < 0.02,
                "frame {frame}"
            );
            last = now;
        }
        assert!(!weather.transitioning());
        assert_eq!(last, target.rates(&c), "settles exactly on the target");
    }

    /// § 11 test 3.
    #[test]
    fn rates_match_the_spec_points() {
        let c = config();
        let rate = |p: f32| {
            WeatherState {
                precipitation: p,
                ..WeatherState::STORM
            }
            .rates(&c)
        };
        assert_eq!(rate(0.0), PrecipitationRates::NONE);
        assert!(rate(0.0).is_none());
        assert_eq!(rate(1.0).rain_mm_per_h, c.precipitation.max_rain_mm_per_h);
        let snow = WeatherState {
            precipitation: 1.0,
            ..WeatherState::BLIZZARD
        }
        .rates(&c);
        assert_eq!(snow.snow_we_mm_per_h, c.precipitation.max_snow_we_mm_per_h);
        assert_eq!(snow.snow_depth_mm_per_h, 50.0, "5 cm/h");
        assert!(WeatherState::OVERCAST.rates(&c).rain_mm_per_h < 2.5);
        let rain = WeatherState::RAIN.rates(&c).rain_mm_per_h;
        assert!((2.5..=7.6).contains(&rain), "{rain}");
        assert_eq!(WeatherState::STORM.rates(&c).rain_mm_per_h, 50.0);
        for r in [1.0_f32, 2.0, 6.1, 12.5, 50.0] {
            let p = (r / 50.0).sqrt();
            let rates = rate(p);
            let expect = 4.1 * r.powf(-0.21);
            assert!((rates.drop_lambda_per_mm - expect).abs() < 1.0e-4, "{r}");
        }
        for rates in [rate(0.3), rate(1.0), snow] {
            let sigma = rates.extinction_per_m();
            assert!((rates.visibility_m - 3.912 / sigma).abs() < 1.0e-2);
        }
        assert!(rate(0.0).visibility_m.is_infinite());
        assert_eq!(rate(0.0).extinction_per_m(), 0.0);
        assert!((rate(1.0).visibility_m - 3000.0).abs() < 1.0e-2);
        assert!((snow.visibility_m - 400.0).abs() < 1.0e-3);
    }

    /// § 11 test 3: class diameters grow with the class and the rate, stay under the cap.
    #[test]
    fn class_diameters_grow_with_class_and_rate() {
        let c = config();
        let classes = |p: f32| {
            let rates = WeatherState {
                precipitation: p,
                ..WeatherState::STORM
            }
            .rates(&c);
            let mut out = [0.0; RAIN_CLASSES];
            rain_class_diameters_mm(&rates, &c, &mut out);
            out
        };
        let light = classes(0.3);
        let heavy = classes(1.0);
        for k in 0..RAIN_CLASSES {
            assert!(light[k] >= c.precipitation.visible_drop_mm);
            assert!(heavy[k] <= 6.0);
            assert!(heavy[k] >= light[k], "class {k}");
            if k > 0 {
                assert!(light[k] > light[k - 1] && heavy[k] > heavy[k - 1], "class {k}");
            }
        }
        assert_eq!(classes(0.0), [c.precipitation.visible_drop_mm; RAIN_CLASSES]);
        // Pinned to the deterministic kernels' bits for the 50 mm/h row (finding 7).
        let bits: Vec<u32> = heavy.iter().map(|d| d.to_bits()).collect();
        assert_eq!(bits, STORM_CLASS_DIAMETER_BITS);
    }

    /// § 11 test 3: the Atlas fit.
    #[test]
    fn fall_speeds_match_the_fit() {
        for (d, expect) in [(0.5, 2.02), (1.0, 4.0), (2.0, 6.55), (4.0, 8.72)] {
            let v = rain_fall_speed_m_per_s(d);
            assert!((v - expect).abs() < 0.03, "{d} mm: {v}");
        }
        assert_eq!(rain_fall_speed_m_per_s(0.0), 0.0, "clamped at 0");
        // Pinned to the deterministic Atlas exponential's bits (finding 7).
        assert_eq!(rain_fall_speed_m_per_s(3.0).to_bits(), FALL_SPEED_3MM_BITS);
        for k in 0..600 {
            let v = rain_fall_speed_m_per_s(k as f32 * 0.01);
            assert!((0.0..9.65).contains(&v));
        }
    }

    /// § 11 test 3: monotone, 1 at the maximum, and the spec's sample points.
    #[test]
    fn the_drawn_count_follows_the_visible_drops() {
        let c = config();
        let factor = |r: f32| {
            let p = (r / 50.0).sqrt();
            rain_count_factor(
                &WeatherState {
                    precipitation: p,
                    ..WeatherState::STORM
                }
                .rates(&c),
                &c,
            )
        };
        assert_eq!(factor(50.0), 1.0);
        assert_eq!(factor(0.0), 0.0);
        for (r, expect) in [(2.0, 0.089), (6.1, 0.237), (12.5, 0.406)] {
            assert!((factor(r) - expect).abs() < 0.005, "{r}: {}", factor(r));
        }
        let mut last = 0.0;
        for k in 0..=50 {
            let f = factor(k as f32);
            assert!(f >= last, "{k}");
            last = f;
        }
        let snow = WeatherState::SNOW.rates(&c);
        assert_eq!(snow_count_factor(&snow, &c), 0.25);
    }

    /// § 11 test 3: `Λ(R) = 4.1 R^-0.21` to 1e-5, against an `f64` reference at the rain rate the
    /// rates report, over the whole precipitation range.
    #[test]
    fn lambda_matches_marshall_palmer_to_1e5() {
        let c = config();
        for k in 1..=200 {
            let rates = WeatherState {
                precipitation: k as f32 / 200.0,
                ..WeatherState::STORM
            }
            .rates(&c);
            let r = f64::from(rates.rain_mm_per_h);
            let expect = 4.1 * r.powf(-0.21);
            let got = f64::from(rates.drop_lambda_per_mm);
            assert!(
                (got - expect).abs() < 1.0e-5,
                "R {r} mm/h: {got} against {expect}"
            );
        }
    }

    /// § 3.1: the old presets' new fields, and every column of the three new presets.
    #[test]
    fn preset_temperatures_gusts_and_the_new_rows_match_the_table() {
        let old = [
            ("clear", 15.0, 0.15),
            ("fair", 15.0, 0.2),
            ("cloudy", 12.0, 0.25),
            ("overcast", 8.0, 0.2),
            ("storm", 10.0, 0.45),
            ("fair_cumulus", 12.0, 0.2),
            ("cumulus", 12.0, 0.2),
            ("stratocumulus", 12.0, 0.2),
            ("cirrus", 12.0, 0.2),
        ];
        for (name, temperature, gust) in old {
            let s = WeatherState::preset(name).expect("preset");
            assert_eq!((s.temperature_c, s.gustiness), (temperature, gust), "{name}");
        }
        // name, coverage, type, density, wind, precipitation, temperature, gust (§ 3.1 table).
        let new = [
            ("rain", 0.9, 0.3, 0.07, [9.0, 3.0], 0.35, 9.0, 0.3),
            ("snow", 0.95, 0.15, 0.06, [5.0, 2.0], 0.5, -3.0, 0.2),
            ("blizzard", 1.0, 0.2, 0.08, [20.0, 8.0], 1.0, -8.0, 0.6),
        ];
        for (name, coverage, cloud_type, density, wind, precipitation, temperature, gust) in new {
            let s = WeatherState::preset(name).expect("preset");
            assert_eq!(
                (
                    s.coverage,
                    s.cloud_type,
                    s.density,
                    s.wind_m_per_s,
                    s.precipitation,
                    s.temperature_c,
                    s.gustiness
                ),
                (
                    coverage,
                    cloud_type,
                    density,
                    wind,
                    precipitation,
                    temperature,
                    gust
                ),
                "{name}"
            );
            assert_eq!(s.genus, None, "{name}");
            assert_eq!(s.precipitation_kind, PrecipitationKind::Auto, "{name}");
        }
        let c = config();
        let snow = WeatherState::SNOW.rates(&c).snow_depth_mm_per_h;
        assert!((snow - 12.5).abs() < 1.0e-4, "about 1.2 cm/h of snow: {snow}");
    }

    /// § 3.2: the four snow classes fall at 0.6, 0.85, 1.1 and 1.4 m/s, inside the 0.5-1.5 m/s
    /// of aggregates.
    #[test]
    fn snow_classes_fall_at_the_stated_speeds() {
        assert_eq!(SNOW_FALL_SPEEDS_M_PER_S, [0.6, 0.85, 1.1, 1.4]);
        for v in SNOW_FALL_SPEEDS_M_PER_S {
            assert!((0.5..=1.5).contains(&v));
        }
    }

    /// § 3.1: `sanitized` clamps the temperature to -60..60 and the gust to 0..1; a non-finite
    /// value takes the fair weather's.
    #[test]
    fn sanitized_clamps_the_temperature_and_the_gust() {
        let s = |temperature_c: f32, gustiness: f32| {
            WeatherState {
                temperature_c,
                gustiness,
                ..WeatherState::STORM
            }
            .sanitized()
        };
        assert_eq!(
            (s(-80.0, -1.0).temperature_c, s(-80.0, -1.0).gustiness),
            (-60.0, 0.0)
        );
        assert_eq!((s(75.0, 3.0).temperature_c, s(75.0, 3.0).gustiness), (60.0, 1.0));
        let fair = WeatherState::FAIR;
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let got = s(bad, bad);
            assert_eq!(got.temperature_c, fair.temperature_c, "{bad}");
            assert_eq!(got.gustiness, fair.gustiness, "{bad}");
        }
        let kept = s(-3.5, 0.37);
        assert_eq!((kept.temperature_c, kept.gustiness), (-3.5, 0.37));
        assert_eq!(s(-3.5, 0.37).precipitation_kind, PrecipitationKind::Auto);
    }

    /// § 3.1: `lerp` blends the temperature and the gust linearly; the kind is `b`'s only once
    /// `t` reaches 1.
    #[test]
    fn lerp_blends_the_temperature_and_gust_and_takes_the_kind_at_the_end() {
        let a = WeatherState {
            temperature_c: 10.0,
            gustiness: 0.2,
            precipitation_kind: PrecipitationKind::Rain,
            ..WeatherState::RAIN
        };
        let b = WeatherState {
            temperature_c: -6.0,
            gustiness: 0.6,
            precipitation_kind: PrecipitationKind::Snow,
            ..WeatherState::SNOW
        };
        assert_eq!(WeatherState::lerp(a, b, 0.0), a);
        let quarter = WeatherState::lerp(a, b, 0.25);
        assert!((quarter.temperature_c - 6.0).abs() < 1.0e-6);
        assert!((quarter.gustiness - 0.3).abs() < 1.0e-6);
        assert_eq!(quarter.precipitation_kind, PrecipitationKind::Rain);
        assert_eq!(
            WeatherState::lerp(a, b, 0.999).precipitation_kind,
            PrecipitationKind::Rain
        );
        let end = WeatherState::lerp(a, b, 1.0);
        assert_eq!(end.precipitation_kind, PrecipitationKind::Snow);
        assert!((end.temperature_c + 6.0).abs() < 1.0e-6);
        assert!((end.gustiness - 0.6).abs() < 1.0e-6);
        let mut last = a.temperature_c;
        for k in 1..=100 {
            let t = WeatherState::lerp(a, b, k as f32 / 100.0).temperature_c;
            assert!(t <= last, "monotone at {k}");
            last = t;
        }
    }

    #[test]
    fn kind_names_round_trip() {
        for kind in [
            PrecipitationKind::Auto,
            PrecipitationKind::Rain,
            PrecipitationKind::Snow,
            PrecipitationKind::Sleet,
        ] {
            assert_eq!(PrecipitationKind::from_name(kind.name()), Some(kind));
        }
        assert_eq!(PrecipitationKind::from_name("hail"), None);
    }
}
