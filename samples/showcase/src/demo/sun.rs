// SPDX-License-Identifier: MIT-0
//! The sun on tiers that draw no physical sky (ADR-3013). Only High, Medium and Ultra run the atmosphere that turns
//! the level's sun clock into a sun; Low, Deck and the phone tiers keep the scene's authored light, so without this
//! the time-of-day and weather sliders would change nothing on the tiers weak machines get.
//!
//! [`follow_clock`] writes the authored light from the same clock ([`regia_render::sun_position`]) and the weather's
//! cloud cover. The atmosphere's tiers ignore the authored light, so it runs on every tier, and the benchmark's
//! rows keep measuring the tier as it ships. The panel says what such a tier cannot draw ([`sky_note`]).

use regia::prelude::*;
use regia_render::{DirectionalLight, QualityTier, TimeOfDay, sun_position};
use regia_scene::level::LevelTimeOfDay;
use regia_vfx::{PrecipitationSettings, Weather};

use super::DemoState;

/// The sun's color right at the horizon.
const LOW_SUN_COLOR: [f32; 3] = [1.0, 0.55, 0.3];
/// How much of the sun full cloud cover takes away.
const OVERCAST_DIM: f32 = 0.7;
/// The sun stays at least this far above the horizon (radians) for its shadows; it has no light by then.
const LOWEST_SHADOW_ELEVATION: f32 = 0.03;

/// The authored light for a clock and a cloud cover.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PlainSun {
    /// Direction of light travel.
    pub direction: [f32; 3],
    /// Color.
    pub color: [f32; 3],
    /// Illuminance, lux.
    pub illuminance: f32,
}

fn smoothstep(low: f32, high: f32, x: f32) -> f32 {
    let t = ((x - low) / (high - low)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The sun for `clock` under `coverage` (`0..=1`), from the scene's authored color and strength: it fades out at the
/// horizon, warms as it gets low, and cloud cover dims it.
#[must_use]
pub fn plain_sun(clock: &LevelTimeOfDay, coverage: f32, color: [f32; 3], lux: f32) -> PlainSun {
    let sun = sun_position(&TimeOfDay {
        hour: clock.hour,
        day_of_year: clock.day_of_year,
        latitude_deg: clock.latitude_deg,
    });
    let daylight = smoothstep(-0.02, 0.2, sun.elevation_rad);
    let high = smoothstep(0.0, 0.5, sun.elevation_rad);
    let mut direction = sun.travel_direction();
    if sun.elevation_rad < LOWEST_SHADOW_ELEVATION {
        // At night the light has no strength: keep it coming from above so its shadow pass stays sane.
        let across = direction[0].hypot(direction[2]).max(1e-4);
        let (sin, cos) = LOWEST_SHADOW_ELEVATION.sin_cos();
        direction = [direction[0] / across * cos, -sin, direction[2] / across * cos];
    }
    PlainSun {
        direction,
        color: std::array::from_fn(|i| LOW_SUN_COLOR[i] + (color[i] - LOW_SUN_COLOR[i]) * high),
        illuminance: lux * daylight * (1.0 - OVERCAST_DIM * coverage.clamp(0.0, 1.0)),
    }
}

/// The lowest sun the panel's hour slider reaches, radians above the horizon (ADR-5419). The forest has no night
/// lighting: about half an hour after sunset its ground renders black on every tier, so the player's clock runs from
/// sunrise to sunset. Agent calls (filming, tests) keep the whole day.
pub const PLAYER_MIN_SUN_ELEVATION: f32 = 0.0;

/// The densest cloud cover the panel offers (ADR-5419). The renderer keeps the sun's hard shadows under full cover, so
/// the player's sky runs from clear to cloudy (the `cloudy` preset's cover); agent calls keep the whole range.
pub const PLAYER_MAX_COVERAGE: f32 = 0.6;

/// The weather presets the panel offers: every one within [`PLAYER_MAX_COVERAGE`] and without rain or snow.
pub const PLAYER_WEATHER_PRESETS: [&str; 3] = ["clear", "fair", "cloudy"];

fn elevation_at(hour: f32, day_of_year: u16, latitude_deg: f32) -> f32 {
    sun_position(&TimeOfDay {
        hour,
        day_of_year,
        latitude_deg,
    })
    .elevation_rad
}

/// The local solar hours from sunrise to sunset of a day (the sun at least [`PLAYER_MIN_SUN_ELEVATION`] high), or
/// `None` when the sun never gets that high (polar night). `(0, 24)` under the midnight sun.
#[must_use]
pub fn daylight_hours(day_of_year: u16, latitude_deg: f32) -> Option<(f32, f32)> {
    // `sun_position` is symmetric about solar noon (12:00) and the sun sinks monotonically from noon to midnight,
    // so one bisection on the afternoon finds sunset, and sunrise mirrors it.
    let up = |hour: f32| elevation_at(hour, day_of_year, latitude_deg) >= PLAYER_MIN_SUN_ELEVATION;
    if !up(12.0) {
        return None;
    }
    if up(24.0) {
        return Some((0.0, 24.0));
    }
    let (mut lit, mut dark) = (12.0_f32, 24.0_f32);
    for _ in 0..32 {
        let mid = 0.5 * (lit + dark);
        if up(mid) {
            lit = mid;
        } else {
            dark = mid;
        }
    }
    Some((24.0 - lit, lit))
}

/// The hour the panel's clock uses for `hour` on `clock`'s day and latitude: held between sunrise and sunset
/// ([`daylight_hours`]), or solar noon in a polar night.
#[must_use]
pub fn player_hour(clock: &LevelTimeOfDay, hour: f32) -> f32 {
    match daylight_hours(clock.day_of_year, clock.latitude_deg) {
        Some((rise, set)) => hour.clamp(rise, set),
        None => 12.0,
    }
}

/// Whether a tier draws the physical sky, clouds and sun (its profile's `physical_atmosphere`).
#[must_use]
pub const fn draws_sky(tier: QualityTier) -> bool {
    matches!(tier, QualityTier::Medium | QualityTier::High | QualityTier::Ultra)
}

/// What the sliders do on a tier without the physical sky, for the panel and `demo.state`.
#[must_use]
pub const fn sky_note(tier: QualityTier) -> Option<&'static str> {
    if draws_sky(tier) {
        None
    } else {
        Some(
            "This tier draws a fixed sky: the time of day moves the sun's light and shadows and cloud cover dims it. \
             Medium and up draw the sky and the clouds.",
        )
    }
}

/// `Update`, after the commands: the authored light follows the clock and the weather's current cloud cover.
pub fn follow_clock(
    state: Res<DemoState>,
    weather: Option<Res<Weather>>,
    mut authored: Local<Option<([f32; 3], f32)>>,
    mut lights: Query<&mut DirectionalLight>,
) {
    let Some(mut light) = lights.iter_mut().next() else {
        return;
    };
    let (color, lux) = *authored.get_or_insert((light.color, light.illuminance));
    let coverage = weather.map_or(state.weather.coverage, |w| w.current().coverage);
    let sun = plain_sun(&state.time_of_day, coverage, color, lux);
    if light.direction != sun.direction || light.color != sun.color || light.illuminance != sun.illuminance {
        light.direction = sun.direction;
        light.color = sun.color;
        light.illuminance = sun.illuminance;
    }
}

/// `Update`: once, turns on the rain and snow the weather sliders promise. Weather effects are off unless a project
/// turns them on (`config/weather.ron`); after this first frame `render.set_precipitation` still has the last word.
pub fn draw_precipitation_once(mut done: Local<bool>, settings: Option<ResMut<PrecipitationSettings>>) {
    if *done {
        return;
    }
    if let Some(mut settings) = settings {
        settings.enabled = true;
        *done = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_render::RenderSettings;

    fn at(hour: f32) -> LevelTimeOfDay {
        LevelTimeOfDay {
            hour,
            day_of_year: 80,
            latitude_deg: 46.0,
        }
    }

    const WHITE: [f32; 3] = [1.0; 3];

    #[test]
    fn the_sun_crosses_the_sky_and_is_gone_at_night() {
        let morning = plain_sun(&at(8.0), 0.0, WHITE, 100.0);
        let noon = plain_sun(&at(12.0), 0.0, WHITE, 100.0);
        let afternoon = plain_sun(&at(16.0), 0.0, WHITE, 100.0);
        let night = plain_sun(&at(23.0), 0.0, WHITE, 100.0);
        assert!(
            morning.direction[0] * afternoon.direction[0] < 0.0,
            "the light comes from the other side: {morning:?} {afternoon:?}"
        );
        assert!(noon.direction[1] < morning.direction[1], "noon is steeper");
        assert!((noon.illuminance - 100.0).abs() < 1e-3);
        assert_eq!(night.illuminance, 0.0);
        assert!(night.direction[1] < 0.0, "a night light still points down");
        let length = night.direction.iter().map(|c| c * c).sum::<f32>().sqrt();
        assert!((length - 1.0).abs() < 1e-3, "{length}");
    }

    #[test]
    fn clouds_dim_the_sun_and_a_low_sun_is_warm() {
        let clear = plain_sun(&at(12.0), 0.0, WHITE, 100.0);
        let overcast = plain_sun(&at(12.0), 1.0, WHITE, 100.0);
        assert!((overcast.illuminance - 30.0).abs() < 1e-3);
        assert_eq!(clear.color, WHITE);
        let low = plain_sun(&at(6.6), 0.0, WHITE, 100.0);
        assert!(low.color[2] < low.color[0], "{:?}", low.color);
        assert!(low.illuminance > 0.0 && low.illuminance < 100.0);
    }

    #[test]
    fn the_players_clock_runs_from_sunrise_to_sunset() {
        // The forest's own clock (day 80, latitude 46): the ground went black after about 18:30.
        let (rise, set) = daylight_hours(80, 46.0).expect("an equinox day");
        assert!(
            (5.5..6.5).contains(&rise) && (17.5..18.5).contains(&set),
            "{rise} {set}"
        );
        assert!((rise + set - 24.0).abs() < 1e-3, "symmetric about noon");
        for hour in [rise, set] {
            let elevation = elevation_at(hour, 80, 46.0);
            assert!(
                // Sunrise mirrors sunset, so its elevation may round a hair below the limit.
                (PLAYER_MIN_SUN_ELEVATION - 1e-4..PLAYER_MIN_SUN_ELEVATION + 0.01).contains(&elevation),
                "{hour}: {elevation}"
            );
        }
        let clock = at(8.0);
        assert_eq!(player_hour(&clock, 23.0), set);
        assert_eq!(player_hour(&clock, 18.75), set, "past 18:30 is held at sunset");
        assert_eq!(player_hour(&clock, 2.0), rise);
        assert_eq!(player_hour(&clock, 12.0), 12.0);
        assert!(plain_sun(&at(set), 0.0, WHITE, 100.0).illuminance > 0.0);
        // Summer days are longer, winter days shorter.
        let (_, summer) = daylight_hours(172, 46.0).expect("summer");
        let (_, winter) = daylight_hours(355, 46.0).expect("winter");
        assert!(summer > set + 1.0 && winter < set - 1.0, "{summer} {winter}");
        // Midnight sun and polar night.
        assert_eq!(daylight_hours(172, 80.0), Some((0.0, 24.0)));
        assert_eq!(daylight_hours(355, 80.0), None);
        let polar = LevelTimeOfDay {
            hour: 3.0,
            day_of_year: 355,
            latitude_deg: 80.0,
        };
        assert_eq!(player_hour(&polar, 3.0), 12.0);
    }

    #[test]
    fn the_players_presets_stay_within_the_cover_and_bring_no_rain() {
        for name in PLAYER_WEATHER_PRESETS {
            let state = regia_vfx::WeatherState::preset(name).expect(name);
            assert!(state.coverage <= PLAYER_MAX_COVERAGE, "{name}");
            assert_eq!(state.precipitation, 0.0, "{name}");
        }
        let overcast = regia_vfx::WeatherState::preset("overcast").expect("overcast");
        assert!(
            overcast.coverage > PLAYER_MAX_COVERAGE,
            "the overcast preset is outside"
        );
    }

    #[test]
    fn the_tier_list_matches_the_profiles() {
        for tier in QualityTier::ALL {
            let mut settings = RenderSettings::default();
            tier.apply_for_gpu(&mut settings, None);
            assert_eq!(
                draws_sky(tier),
                settings.profile.physical_atmosphere,
                "{}",
                tier.name()
            );
            assert_eq!(sky_note(tier).is_none(), draws_sky(tier), "{}", tier.name());
        }
    }
}
