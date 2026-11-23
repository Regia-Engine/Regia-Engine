// SPDX-License-Identifier: MPL-2.0
//! The wind near the ground (spec 99 § 3.3) and the state of wet or snowy surfaces (§ 3.4): pure
//! functions of the weather, its clock and the frame steps, so the same `dt` sequence gives the
//! same bits on one platform.
//!
//! The height profile is the wind profile power law (Peterson and Hennessey 1978). The gust signal
//! is Regia's own bounded sum of sines, not a turbulence model. The surface integrator is Regia's
//! own simple rates with the quantities that set them: radiation and wind drive the drying (the
//! two terms of Penman 1948, qualitatively), the melt is degree-day (Hock 2003).

use std::f64::consts::TAU;

use regia_core::math::det_powf_nonnegative;
use regia_scene::weather::{SceneWind, SceneWindMode, SurfaceWeather, SurfaceWeatherMode};

use super::{PrecipitationRates, SurfaceConfig, WeatherState, WindCoupling};

/// The surface wind's reference height (m).
const SURFACE_WIND_HEIGHT_M: f32 = 10.0;

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// Near-ground wind over the wind at `cloud_base_m`: the config's factor, else
/// `(10 m / cloud base)^(1/7)` (open terrain).
#[must_use]
pub fn surface_wind_factor(cloud_base_m: f32, config: &WindCoupling) -> f32 {
    config.surface_factor.unwrap_or_else(|| {
        let base = cloud_base_m.max(SURFACE_WIND_HEIGHT_M);
        // det_powf_nonnegative: the factor scales the wind written into SceneWind every frame, so its
        // bits must not follow the platform libm (fix-vfx1-determinism finding 6).
        det_powf_nonnegative(SURFACE_WIND_HEIGHT_M / base, 1.0 / 7.0)
    })
}

fn wave(clock_s: f64, period_s: f64, phase: f64) -> f64 {
    (TAU * clock_s / period_s + phase).sin()
}

/// The gust multiplier at `clock_s` for a strength `gamma` in `0..1`: never below 0, within
/// `gamma` of 1.
#[must_use]
pub fn gust_factor(clock_s: f64, gamma: f32) -> f32 {
    let signal =
        0.55 * wave(clock_s, 11.3, 0.0) + 0.3 * wave(clock_s, 4.7, 1.3) + 0.15 * wave(clock_s, 1.9, 2.9);
    1.0 + gamma * signal as f32
}

/// The gust's turn of the wind at `clock_s`, radians.
#[must_use]
pub fn gust_turn_rad(clock_s: f64, gamma: f32) -> f32 {
    0.17 * gamma * wave(clock_s, 17.1, 0.4) as f32
}

fn rotate(v: [f32; 2], angle_rad: f32) -> [f32; 2] {
    let (sin, cos) = angle_rad.sin_cos();
    [v[0] * cos - v[1] * sin, v[0] * sin + v[1] * cos]
}

/// The wind near the ground of `state` at the weather clock `clock_s`, under a cloud layer at
/// `cloud_base_m`, in [`SceneWindMode::Weather`]: `mean` is the cloud wind through the height
/// profile, `velocity` adds the gusts.
#[must_use]
pub fn surface_wind(
    state: &WeatherState,
    clock_s: f64,
    cloud_base_m: f32,
    config: &WindCoupling,
) -> SceneWind {
    let factor = surface_wind_factor(cloud_base_m, config);
    let aloft = [state.wind_m_per_s[0] * factor, state.wind_m_per_s[1] * factor];
    let mean = rotate(aloft, config.veer_deg.to_radians());
    let gamma = (state.gustiness * config.gust_scale).clamp(0.0, 1.0);
    let g = gust_factor(clock_s, gamma);
    let gusted = rotate([mean[0] * g, mean[1] * g], gust_turn_rad(clock_s, gamma));
    SceneWind {
        mode: SceneWindMode::Weather,
        velocity_m_per_s: gusted,
        mean_m_per_s: mean,
    }
}

/// What the surface state steps from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SurfaceInputs {
    /// What falls now.
    pub rates: PrecipitationRates,
    /// Air temperature near the ground, °C.
    pub temperature_c: f32,
    /// Sine of the sun's elevation (negative at night).
    pub sun_elevation_sin: f32,
    /// Fraction of the sky under cloud, `0..1`.
    pub cloud_coverage: f32,
    /// Near-ground wind speed without gusts (m/s).
    pub wind_speed_m_per_s: f32,
    /// Fresh snow depth per depth of water (`PrecipitationConfig::snow_ratio`).
    pub snow_ratio: f32,
    /// The project's top rain rate (`PrecipitationConfig::max_rain_mm_per_h`), for the ripple.
    pub max_rain_mm_per_h: f32,
}

impl SurfaceInputs {
    /// How fast surfaces dry relative to the configured times.
    #[must_use]
    pub fn drying_factor(&self) -> f32 {
        let e = self.sun_elevation_sin.clamp(0.0, 1.0);
        let c = self.cloud_coverage.clamp(0.0, 1.0);
        (0.25 + 0.75 * e * (1.0 - 0.7 * c))
            * (1.0 + self.wind_speed_m_per_s.max(0.0) / 5.0)
            * (1.0 + self.temperature_c / 20.0).clamp(0.1, 2.0)
    }
}

/// Steps `state` by `dt` seconds of game time. Does nothing in [`SurfaceWeatherMode::Manual`], or
/// for a `dt` of 0, negative or not finite.
pub fn step_surface_weather(
    state: &mut SurfaceWeather,
    inputs: &SurfaceInputs,
    config: &SurfaceConfig,
    dt: f32,
) {
    if state.mode == SurfaceWeatherMode::Manual || !dt.is_finite() || dt <= 0.0 {
        return;
    }
    let rain = inputs.rates.rain_mm_per_h;
    let snow = inputs.rates.snow_we_mm_per_h;
    let temperature = inputs.temperature_c;
    let e = inputs.sun_elevation_sin.clamp(0.0, 1.0);

    // Snow, kept as water equivalent so the mass is conserved. Wet flakes melt on contact.
    let settles = (1.0 - temperature / 2.0).clamp(0.0, 1.0);
    state.snow_we_mm += snow * settles / 3600.0 * dt;
    let melt_mm_per_h =
        config.melt_mm_per_c_day * config.melt_scale * temperature.max(0.0) * (1.0 + 0.5 * e) / 24.0;
    let melted_mm = (melt_mm_per_h / 3600.0 * dt).min(state.snow_we_mm);
    state.snow_we_mm = (state.snow_we_mm - melted_mm).max(0.0);
    let melt = melted_mm / dt * 3600.0;

    let dry = inputs.drying_factor();
    let water = rain + melt;
    if water > 0.0 {
        state.wetness += water / (config.wet_mm * 3600.0) * dt;
    } else {
        state.wetness -= dry / config.dry_time_s * dt;
    }
    state.wetness = state.wetness.clamp(0.0, 1.0);

    if water < config.drain_mm_per_h {
        state.puddles -= dry / config.puddle_dry_time_s * dt;
    } else if state.wetness >= 0.95 {
        state.puddles += (water - config.drain_mm_per_h) / (config.puddle_mm * 3600.0) * dt;
    }
    state.puddles = state.puddles.clamp(0.0, 1.0);

    state.snow_depth_m = state.snow_we_mm * inputs.snow_ratio / 1000.0;
    state.snow_cover = smoothstep(0.0, config.full_cover_m, state.snow_depth_m);
    state.rain_mm_per_h = rain;
    state.snow_we_mm_per_h = snow;
    state.ripple = (rain / inputs.max_rain_mm_per_h).clamp(0.0, 1.0);
}

#[cfg(test)]
mod tests {
    use super::super::{Weather, WeatherConfig};
    use super::*;

    const DT: f32 = 1.0 / 60.0;

    fn storm_rates(rain_mm_per_h: f32) -> PrecipitationRates {
        PrecipitationRates {
            rain_mm_per_h,
            ..PrecipitationRates::NONE
        }
    }

    fn inputs(rates: PrecipitationRates) -> SurfaceInputs {
        SurfaceInputs {
            rates,
            temperature_c: 15.0,
            sun_elevation_sin: 0.5,
            cloud_coverage: 0.5,
            wind_speed_m_per_s: 0.0,
            snow_ratio: 10.0,
            max_rain_mm_per_h: 50.0,
        }
    }

    fn run(state: &mut SurfaceWeather, inputs: &SurfaceInputs, seconds: f32) {
        let config = SurfaceConfig::default();
        for _ in 0..(seconds / DT).round() as u32 {
            step_surface_weather(state, inputs, &config, DT);
        }
    }

    /// Audit G finding 3 (render5 determinism): the melt rate multiplies by
    /// `(1 + 0.5 * sun_elevation_sin)`, and that sine is the renderer's sun, whose
    /// bits come from the software libm. The pinned end state locks the integration
    /// across the render/vfx boundary: a host-libm change in the sun path moves these
    /// bits on the reference host (see `regia_render::soft_math::tests`).
    #[test]
    fn melting_snowpack_bits_are_pinned() {
        let time = regia_render::TimeOfDay {
            hour: 18.0,
            day_of_year: 172,
            latitude_deg: 45.0,
        };
        let sun = super::super::sun_elevation_sin(Some(&time));
        assert_eq!(
            sun.to_bits(),
            0x3e90_15ab,
            "the 18:00 sun-elevation sine is the software-libm value"
        );
        let inputs = SurfaceInputs {
            rates: PrecipitationRates::NONE,
            temperature_c: 2.0,
            sun_elevation_sin: sun,
            cloud_coverage: 0.25,
            wind_speed_m_per_s: 3.0,
            snow_ratio: 10.0,
            max_rain_mm_per_h: 50.0,
        };
        let config = SurfaceConfig::default();
        let mut state = SurfaceWeather {
            snow_we_mm: 12.0,
            wetness: 0.5,
            ..SurfaceWeather::default()
        };
        for _ in 0..600 {
            step_surface_weather(&mut state, &inputs, &config, DT);
        }
        assert_eq!(
            state.snow_we_mm.to_bits(),
            0x413f_fb50,
            "melted snowpack after 600 x 1/60 s at 2 C under the 18:00 sun"
        );
    }

    /// § 11 test 4.
    #[test]
    fn the_height_profile_matches_the_power_law() {
        let config = WindCoupling::default();
        let f = surface_wind_factor(1500.0, &config);
        assert!((f - (10.0_f32 / 1500.0).powf(1.0 / 7.0)).abs() < 1.0e-4, "{f}");
        assert!((f - 0.4888).abs() < 1.0e-3);
        // Pinned to the deterministic power law's bits: the factor scales the wind written
        // into SceneWind every frame (fix-vfx1-determinism finding 6).
        assert_eq!(f.to_bits(), 0x3efa_43ca);
        assert_eq!(surface_wind_factor(5.0, &config), 1.0, "not above the wind aloft");
        let held = WindCoupling {
            surface_factor: Some(0.3),
            ..config
        };
        assert_eq!(surface_wind_factor(1500.0, &held), 0.3);
    }

    /// § 11 test 4: over 10⁵ frames the gust stays inside its band and its per-frame bound.
    #[test]
    fn gusts_stay_bounded_and_the_clock_gives_the_same_bits() {
        let config = WindCoupling::default();
        let state = WeatherState::STORM;
        let gamma = state.gustiness;
        let mut last: Option<SceneWind> = None;
        let mut clock = 0.0_f64;
        for frame in 0..100_000 {
            clock += f64::from(DT);
            let wind = surface_wind(&state, clock, 1500.0, &config);
            let mean = wind.mean_speed();
            let ratio = wind.speed() / mean;
            assert!(
                ratio >= 1.0 - gamma - 1.0e-5 && ratio <= 1.0 + gamma + 1.0e-5,
                "frame {frame}: {ratio}"
            );
            if let Some(last) = last {
                let step = (wind.velocity_m_per_s[0] - last.velocity_m_per_s[0])
                    .hypot(wind.velocity_m_per_s[1] - last.velocity_m_per_s[1]);
                assert!(step <= 1.35 * gamma * mean * DT + 1.0e-5, "frame {frame}: {step}");
            }
            last = Some(wind);
            assert_eq!(wind, surface_wind(&state, clock, 1500.0, &config));
        }
    }

    /// § 11 test 4: `g ≥ 0` at the strongest gust, so the wind never reverses.
    #[test]
    fn the_strongest_gust_never_reverses_the_wind() {
        let mut clock = 0.0_f64;
        for _ in 0..200_000 {
            clock += 0.01;
            assert!(gust_factor(clock, 1.0) >= 0.0, "{clock}");
            assert!(gust_turn_rad(clock, 1.0).abs() <= 0.17 + 1.0e-6);
        }
        assert_eq!(gust_factor(123.4, 0.0), 1.0);
        assert_eq!(gust_turn_rad(123.4, 0.0), 0.0);
    }

    /// § 11 test 4: a wind transition carries on without a jump.
    #[test]
    fn a_wind_transition_does_not_jump() {
        let config = WindCoupling::default();
        let mut weather = Weather::new(WeatherState::FAIR);
        weather.transition_to(WeatherState::STORM, 5.0);
        let mut last = surface_wind(&weather.current(), weather.clock_s(), 1500.0, &config);
        for frame in 0..400 {
            weather.step(DT);
            let wind = surface_wind(&weather.current(), weather.clock_s(), 1500.0, &config);
            let step = (wind.velocity_m_per_s[0] - last.velocity_m_per_s[0])
                .hypot(wind.velocity_m_per_s[1] - last.velocity_m_per_s[1]);
            assert!(step < 0.2, "frame {frame}: {step}");
            last = wind;
        }
    }

    #[test]
    fn veer_turns_the_mean() {
        let config = WindCoupling {
            veer_deg: 90.0,
            surface_factor: Some(1.0),
            gust_scale: 0.0,
            ..WindCoupling::default()
        };
        let state = WeatherState {
            wind_m_per_s: [4.0, 0.0],
            ..WeatherState::FAIR
        };
        let wind = surface_wind(&state, 3.0, 1500.0, &config);
        assert!(wind.mean_m_per_s[0].abs() < 1.0e-5);
        assert!((wind.mean_m_per_s[1] - 4.0).abs() < 1.0e-5);
        assert_eq!(wind.velocity_m_per_s, wind.mean_m_per_s, "no gusts at scale 0");
    }

    /// § 11 test 5.
    #[test]
    fn dry_stays_dry() {
        let mut state = SurfaceWeather::DRY;
        run(&mut state, &inputs(PrecipitationRates::NONE), 60.0);
        assert_eq!(state, SurfaceWeather::DRY);
    }

    /// § 11 test 5: 5 mm/h wets fully in `wet_mm / 5 · 3600` s, within a frame.
    #[test]
    fn wetting_takes_the_stated_time() {
        let mut state = SurfaceWeather::DRY;
        let wet = inputs(storm_rates(5.0));
        let seconds = SurfaceConfig::default().wet_mm / 5.0 * 3600.0;
        run(&mut state, &wet, seconds - 2.0 * DT);
        assert!(state.wetness < 1.0, "{}", state.wetness);
        run(&mut state, &wet, 3.0 * DT);
        assert_eq!(state.wetness, 1.0);
    }

    /// § 11 test 5.
    #[test]
    fn drying_follows_the_sun_and_the_wind() {
        let overcast = SurfaceInputs {
            sun_elevation_sin: 1.0,
            cloud_coverage: 1.0,
            ..inputs(PrecipitationRates::NONE)
        };
        let clear = SurfaceInputs {
            cloud_coverage: 0.0,
            ..overcast
        };
        let windy = SurfaceInputs {
            wind_speed_m_per_s: 5.0,
            ..overcast
        };
        assert!(clear.drying_factor() > overcast.drying_factor());
        assert_eq!(windy.drying_factor(), 2.0 * overcast.drying_factor());
        let mut slow = SurfaceWeather {
            wetness: 1.0,
            ..SurfaceWeather::DRY
        };
        let mut fast = slow;
        run(&mut slow, &overcast, 120.0);
        run(&mut fast, &clear, 120.0);
        assert!(fast.wetness < slow.wetness && slow.wetness < 1.0);
    }

    /// § 11 test 5.
    #[test]
    fn puddles_need_real_rain_and_a_wet_surface() {
        let mut drizzle = SurfaceWeather::DRY;
        run(&mut drizzle, &inputs(storm_rates(2.0)), 900.0);
        assert_eq!(drizzle.wetness, 1.0);
        assert_eq!(drizzle.puddles, 0.0, "drizzle wets and does not pool");
        let mut rain = SurfaceWeather::DRY;
        let heavy = inputs(storm_rates(30.0));
        // 30 mm/h wets in 24 s; before that nothing pools.
        run(&mut rain, &heavy, 20.0);
        assert!(rain.wetness < 0.95);
        assert_eq!(rain.puddles, 0.0);
        run(&mut rain, &heavy, 600.0);
        assert!(rain.puddles > 0.5, "{}", rain.puddles);
        let before = rain.puddles;
        run(&mut rain, &inputs(PrecipitationRates::NONE), 300.0);
        assert!(rain.puddles < before, "drains when the rain stops");
    }

    /// The rain preset (about 6 mm/h) is not full of puddles at half an hour. It fills in about 85 minutes.
    #[test]
    fn rain_preset_puddles_fill_in_about_eighty_five_minutes() {
        let rates = Weather::new(WeatherState::RAIN).rates(&WeatherConfig::default());
        assert!(
            (rates.rain_mm_per_h - 6.125).abs() < 1e-3,
            "{}",
            rates.rain_mm_per_h
        );
        let falling = SurfaceInputs {
            temperature_c: WeatherState::RAIN.temperature_c,
            ..inputs(rates)
        };
        let mut state = SurfaceWeather::DRY;
        run(&mut state, &falling, 1800.0);
        assert!(state.puddles < 0.5, "half an hour is not full: {}", state.puddles);
        run(&mut state, &falling, 3300.0);
        assert_eq!(state.puddles, 1.0, "full by about 85 minutes");
    }

    /// § 11 test 5.
    #[test]
    fn snow_settles_in_the_cold_melts_in_the_warm_and_meltwater_wets() {
        let snow = PrecipitationRates {
            snow_we_mm_per_h: 2.0,
            ..PrecipitationRates::NONE
        };
        let mut cold = SurfaceWeather::DRY;
        let freezing = SurfaceInputs {
            temperature_c: -3.0,
            ..inputs(snow)
        };
        run(&mut cold, &freezing, 1800.0);
        assert!((cold.snow_we_mm - 1.0).abs() < 1.0e-2, "{}", cold.snow_we_mm);
        assert!((cold.snow_depth_m - 0.01).abs() < 1.0e-4);
        assert!(cold.snow_cover > 0.0 && cold.snow_cover < 1.0);
        assert_eq!(cold.wetness, 0.0, "no melt at -3 °C");
        let mut warm = SurfaceWeather::DRY;
        let thaw = SurfaceInputs {
            temperature_c: 3.0,
            ..inputs(snow)
        };
        run(&mut warm, &thaw, 1800.0);
        assert_eq!(warm.snow_we_mm, 0.0, "wet flakes melt on contact");

        // A settled cover melts in the warm, faster in the sun, and wets the ground.
        let config = SurfaceConfig {
            melt_scale: 100.0,
            ..SurfaceConfig::default()
        };
        let start = cold;
        let melt = |sun: f32| {
            let mut s = start;
            let thaw = SurfaceInputs {
                temperature_c: 5.0,
                sun_elevation_sin: sun,
                ..inputs(PrecipitationRates::NONE)
            };
            for _ in 0..20 {
                step_surface_weather(&mut s, &thaw, &config, 1.0);
            }
            s
        };
        let (shade, sun) = (melt(0.0), melt(1.0));
        assert!(sun.snow_we_mm < shade.snow_we_mm && shade.snow_we_mm < start.snow_we_mm);
        assert!(shade.wetness > 0.0, "meltwater wets");
    }

    /// § 11 test 5: mass is conserved through accumulation and melt.
    #[test]
    fn melting_conserves_the_water() {
        let mut state = SurfaceWeather {
            snow_we_mm: 3.0,
            ..SurfaceWeather::DRY
        };
        let warm = SurfaceInputs {
            temperature_c: 20.0,
            ..inputs(PrecipitationRates::NONE)
        };
        let config = SurfaceConfig {
            melt_scale: 200.0,
            ..SurfaceConfig::default()
        };
        step_surface_weather(&mut state, &warm, &config, 100_000.0);
        assert_eq!(state.snow_we_mm, 0.0, "never below zero");
    }

    /// § 11 test 5: manual and bad steps hold.
    #[test]
    fn manual_and_bad_steps_hold() {
        let wet = inputs(storm_rates(20.0));
        let config = SurfaceConfig::default();
        let mut manual = SurfaceWeather {
            mode: SurfaceWeatherMode::Manual,
            wetness: 0.4,
            ..SurfaceWeather::DRY
        };
        let held = manual;
        step_surface_weather(&mut manual, &wet, &config, 1.0);
        assert_eq!(manual, held);
        let mut state = SurfaceWeather::DRY;
        for dt in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            step_surface_weather(&mut state, &wet, &config, dt);
            assert_eq!(state, SurfaceWeather::DRY, "{dt}");
        }
    }

    /// § 11 test 5: 10⁶ steps through every kind of weather stay in range, and the same `dt`
    /// sequence gives the same bits.
    #[test]
    fn a_long_run_stays_in_range_and_is_repeatable() {
        let config = WeatherConfig::default();
        let run_once = || {
            let mut state = SurfaceWeather::DRY;
            let mut weather = Weather::new(WeatherState::RAIN);
            let presets = [
                WeatherState::STORM,
                WeatherState::SNOW,
                WeatherState::BLIZZARD,
                WeatherState::FAIR,
            ];
            for step in 0..1_000_000_u32 {
                if step.is_multiple_of(100_000) {
                    weather.transition_to(presets[(step / 100_000) as usize % presets.len()], 60.0);
                }
                // A varying step, the same each run.
                let dt = 1.0 / 60.0 + (step % 7) as f32 * 1.0e-4;
                weather.step(dt);
                let current = weather.current();
                let inputs = SurfaceInputs {
                    rates: weather.rates(&config),
                    temperature_c: current.temperature_c,
                    sun_elevation_sin: 0.4,
                    cloud_coverage: current.coverage,
                    wind_speed_m_per_s: 3.0,
                    snow_ratio: config.precipitation.snow_ratio,
                    max_rain_mm_per_h: config.precipitation.max_rain_mm_per_h,
                };
                step_surface_weather(&mut state, &inputs, &config.surface, dt);
                assert!((0.0..=1.0).contains(&state.wetness));
                assert!((0.0..=1.0).contains(&state.puddles));
                assert!((0.0..=1.0).contains(&state.snow_cover));
                assert!((0.0..=1.0).contains(&state.ripple));
                assert!(state.snow_we_mm.is_finite() && state.snow_we_mm >= 0.0);
            }
            state
        };
        assert_eq!(run_once(), run_once());
    }
}
