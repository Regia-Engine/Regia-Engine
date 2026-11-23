// SPDX-License-Identifier: MPL-2.0
//! The `Update` systems that fill the `regia_scene::weather` resources from the [`Weather`], and
//! the `Startup` read of `config://weather.ron`. Each effect runs only when the project turned it
//! on (or a host or an agent inserted its resource), so a host with a `Weather` for its clouds
//! alone renders the same frames.

use regia_app::prelude::*;
use regia_platform::Vfs;
use regia_render::{CloudSettings, TimeOfDay, sun_position};
use regia_scene::level::LevelSettings;
use regia_scene::weather::{SceneWind, SceneWindMode, SurfaceWeather, WeatherAir};

use super::{
    SurfaceInputs, WEATHER_CONFIG_PATH, Weather, WeatherConfig, WeatherConfigStatus, step_surface_weather,
    surface_wind,
};
use crate::medium::{VfxAtmosphere, VfxMedium};
use crate::precip::PrecipitationSettings;

/// `true` when the level has no air: nothing falls, nothing dries and there is no wind.
#[must_use]
pub fn medium_is_airless(level: Option<&LevelSettings>) -> bool {
    level.is_some_and(|level| VfxMedium::from_level(level).atmosphere == VfxAtmosphere::Airless)
}

/// Sine of the sun's elevation at `time` (solar noon without a clock).
#[must_use]
pub fn sun_elevation_sin(time: Option<&TimeOfDay>) -> f32 {
    sun_position(&time.copied().unwrap_or_default()).direction[1]
}

/// The surface wind's mean speed without a [`SceneWind`]: the weather's own.
#[must_use]
pub fn scene_wind_surface_mean(
    weather: &Weather,
    clouds: Option<&CloudSettings>,
    config: &WeatherConfig,
) -> SceneWind {
    let base = clouds.map_or(CloudSettings::default().altitude_bottom, |c| c.altitude_bottom);
    surface_wind(&weather.current(), weather.clock_s(), base, &config.wind)
}

/// `Startup`: reads `config://weather.ron` into a [`WeatherConfig`] unless the host inserted one.
/// A missing file, or an app without a `Vfs`, gives the defaults. A file that fails is logged once,
/// keeps the defaults and leaves its error in [`WeatherConfigStatus`] for `render.get_weather`.
/// Also seeds [`PrecipitationSettings`] from the effects.
pub fn load_weather_config(world: &mut World) {
    let mut status = WeatherConfigStatus::default();
    if !world.contains_resource::<WeatherConfig>() {
        let loaded = world.get_resource::<Vfs>().map(WeatherConfig::load);
        let config = match loaded {
            None | Some(Ok(None)) => WeatherConfig::default(),
            Some(Ok(Some(config))) => {
                status.path = Some(format!("config://{WEATHER_CONFIG_PATH}"));
                status.format_version = Some(config.format_version);
                config
            }
            Some(Err(err)) => {
                tracing::error!(target: "regia_vfx", "config/weather.ron refused: {err}");
                status.path = Some(format!("config://{WEATHER_CONFIG_PATH}"));
                status.error = Some((err.code().to_owned(), err.to_string()));
                WeatherConfig::default()
            }
        };
        world.insert_resource(config);
    }
    world.insert_resource(status);
    if !world.contains_resource::<PrecipitationSettings>() {
        let settings = PrecipitationSettings::from_config(world.resource::<WeatherConfig>());
        world.insert_resource(settings);
    }
}

/// `Update`: when a level opens without a [`Weather`], starts the config's `start` preset.
pub fn start_level_weather(
    config: Option<Res<WeatherConfig>>,
    weather: Option<Res<Weather>>,
    level: Option<Res<LevelSettings>>,
    mut commands: Commands,
) {
    let (Some(config), None, Some(level)) = (config, weather, level) else {
        return;
    };
    if !level.is_changed() {
        return;
    }
    if let Some(state) = config.start.as_deref().and_then(|name| config.preset(name)) {
        commands.insert_resource(Weather::new(state));
    }
}

/// `Update`, after [`super::drive_cloud_weather`]: inserts the [`SceneWind`] the first time a
/// [`Weather`] exists while the project's `effects.scene_wind` is on (a host or an agent may insert
/// it at any time instead), and writes it every frame while its mode is `Weather`.
pub fn drive_scene_wind(
    weather: Option<Res<Weather>>,
    config: Option<Res<WeatherConfig>>,
    clouds: Option<Res<CloudSettings>>,
    wind: Option<ResMut<SceneWind>>,
    mut inserted: Local<bool>,
    mut commands: Commands,
) {
    let (Some(weather), Some(config)) = (weather, config) else {
        return;
    };
    let _span = tracing::trace_span!("drive_scene_wind").entered();
    let next = scene_wind_surface_mean(&weather, clouds.as_deref(), &config);
    match wind {
        Some(mut wind) => {
            *inserted = true;
            if wind.mode == SceneWindMode::Weather && *wind != next {
                *wind = next;
            }
        }
        None if config.effects.scene_wind && !*inserted => {
            *inserted = true;
            commands.insert_resource(SceneWind {
                mode: config.wind.mode,
                ..next
            });
        }
        None => {}
    }
}

/// `Update`, after [`drive_scene_wind`]: steps the [`SurfaceWeather`] by the game's frame time while
/// the weather runs. Inserts it dry the first time a [`Weather`] exists while `effects.surface` is
/// on (a host or an agent may insert it at any time instead). Holds without air.
#[allow(clippy::too_many_arguments)] // ECS systems pull their inputs as parameters
pub fn drive_surface_weather(
    time: Option<Res<Time>>,
    weather: Option<Res<Weather>>,
    config: Option<Res<WeatherConfig>>,
    tod: Option<Res<TimeOfDay>>,
    wind: Option<Res<SceneWind>>,
    clouds: Option<Res<CloudSettings>>,
    level: Option<Res<LevelSettings>>,
    surface: Option<ResMut<SurfaceWeather>>,
    mut inserted: Local<bool>,
    mut commands: Commands,
) {
    let (Some(weather), Some(config)) = (weather, config) else {
        return;
    };
    let Some(mut surface) = surface else {
        if config.effects.surface && !*inserted {
            *inserted = true;
            commands.insert_resource(SurfaceWeather::DRY);
        }
        return;
    };
    *inserted = true;
    if weather.paused || medium_is_airless(level.as_deref()) {
        return;
    }
    let _span = tracing::trace_span!("drive_surface_weather").entered();
    let dt = time.map_or(0.0, |t| t.delta_secs());
    let current = weather.current();
    let wind_speed = wind.as_deref().map_or_else(
        || scene_wind_surface_mean(&weather, clouds.as_deref(), &config).mean_speed(),
        |wind| {
            if wind.mode == SceneWindMode::Off {
                0.0
            } else {
                wind.mean_speed()
            }
        },
    );
    let inputs = SurfaceInputs {
        rates: weather.rates(&config),
        temperature_c: current.temperature_c,
        sun_elevation_sin: sun_elevation_sin(tod.as_deref()),
        cloud_coverage: current.coverage,
        wind_speed_m_per_s: wind_speed,
        snow_ratio: config.precipitation.snow_ratio,
        max_rain_mm_per_h: config.precipitation.max_rain_mm_per_h,
    };
    let mut next = *surface;
    step_surface_weather(&mut next, &inputs, &config.surface, dt);
    if next != *surface {
        *surface = next;
    }
}

/// `Update`, after [`drive_surface_weather`]: while the precipitation effect and the config's
/// `visibility.couple_fog` are on, writes the extinction of what falls into [`WeatherAir`]. Turning
/// either off removes the resource this system wrote, so a host's own `WeatherAir` is left alone.
pub fn drive_weather_air(
    weather: Option<Res<Weather>>,
    config: Option<Res<WeatherConfig>>,
    settings: Option<Res<PrecipitationSettings>>,
    level: Option<Res<LevelSettings>>,
    air: Option<ResMut<WeatherAir>>,
    mut wrote: Local<bool>,
    mut commands: Commands,
) {
    let (Some(weather), Some(config)) = (weather, config) else {
        return;
    };
    let on = config.visibility.couple_fog && settings.is_some_and(|s| s.enabled);
    if !on {
        if std::mem::take(&mut *wrote) {
            commands.remove_resource::<WeatherAir>();
        }
        return;
    }
    let extinction_per_m = if medium_is_airless(level.as_deref()) {
        0.0
    } else {
        weather.rates(&config).extinction_per_m()
    };
    let next = WeatherAir { extinction_per_m };
    match air {
        Some(mut air) => {
            if *air != next {
                *air = next;
            }
        }
        None => commands.insert_resource(next),
    }
    *wrote = true;
}
