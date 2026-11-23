// SPDX-License-Identifier: MPL-2.0
//! The weather's effects in an app (spec 99 § 11): opt-in per project, the fog term, the scene
//! wind and the surface state on game time, airless levels and the project's config.

use regia_app::prelude::*;
use regia_render::{CloudSettings, ExtractedFrame};
use regia_scene::level::{AtmospherePreset, LevelSettings};
use regia_scene::weather::{SceneWind, SceneWindMode, SurfaceWeather, SurfaceWeatherMode, WeatherAir};
use regia_vfx::weather::{WeatherConfigStatus, WeatherPlugin};
use regia_vfx::{PrecipitationSettings, Weather, WeatherConfig, WeatherState};

const STEP_MS: u64 = 100;

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_millis(STEP_MS),
    ));
    app.add_plugins((regia_app::MinimalPlugins, WeatherPlugin));
    app
}

fn all_on() -> WeatherConfig {
    WeatherConfig::from_ron("(effects: (precipitation: true, surface: true, scene_wind: true))")
        .expect("config")
}

/// An app with the `overcast` weather and no config inserts none of the effects' resources.
#[test]
fn with_no_config_nothing_turns_on() {
    let mut app = app();
    app.insert_resource(Weather::new(WeatherState::OVERCAST));
    for _ in 0..5 {
        app.update();
    }
    let world = app.world();
    assert!(world.get_resource::<SceneWind>().is_none());
    assert!(world.get_resource::<SurfaceWeather>().is_none());
    assert!(world.get_resource::<WeatherAir>().is_none());
    assert!(!world.resource::<PrecipitationSettings>().enabled);
    assert_eq!(*world.resource::<WeatherConfig>(), WeatherConfig::default());
    assert_eq!(
        *world.resource::<WeatherConfigStatus>(),
        WeatherConfigStatus::default()
    );
}

/// § 11 opt-in, the renderer's side: with `overcast` and no config the extracted frame carries the
/// dry surface bit for bit, no precipitation extinction and no scene-wind override, and no drop or
/// splash is drawn. These are the inputs of the frame without the feature (the dry-frame bytes are
/// held by `regia_render`'s `surface_weather` and `visbuffer_surface_weather` tests).
#[test]
fn with_no_config_the_renderer_gets_the_frame_without_the_feature() {
    let mut app = app();
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(regia_render::RenderSettings::default());
    app.insert_resource(Weather::new(WeatherState::OVERCAST));
    app.add_systems(Update, regia_render::extract_frame);
    for _ in 0..5 {
        app.update();
    }
    let outdoor = &app.world().resource::<ExtractedFrame>().outdoor;
    assert_eq!(outdoor.surface_weather, regia_render::SurfaceWeatherFrame::DRY);
    assert_eq!(outdoor.precip_extinction_per_m.to_bits(), 0.0f32.to_bits());
    assert_eq!(outdoor.scene_wind, None, "no SceneWind, no foliage override");
    let drawn = app
        .world()
        .get_resource::<regia_vfx::PrecipitationReport>()
        .map_or((0, 0, 0), |r| (r.rain_drawn, r.snow_drawn, r.splashes));
    assert_eq!(drawn, (0, 0, 0), "nothing falls without the effect");
}

/// A file that fails keeps the defaults and leaves its error for `render.get_weather`.
#[test]
fn a_bad_file_keeps_the_defaults_and_leaves_its_error() {
    let dir = regia_test_utils::unique_temp_dir("weather-bad-config");
    std::fs::write(dir.join("weather.ron"), "(nonsense: 1)").expect("write");
    let mut vfs = regia_platform::Vfs::new();
    vfs.mount(
        regia_platform::MountPoint::Config,
        regia_platform::DirMount::read_only(&dir),
    );
    let mut app = app();
    app.insert_resource(vfs);
    app.update();
    assert_eq!(*app.world().resource::<WeatherConfig>(), WeatherConfig::default());
    let status = app.world().resource::<WeatherConfigStatus>();
    assert_eq!(status.path.as_deref(), Some("config://weather.ron"));
    let code = status.error.as_ref().map(|(code, _)| code.as_str());
    assert_eq!(code, Some("weather_config_invalid"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// § 11 test 6: the air takes the extinction only with the effect and the coupling on.
#[test]
fn the_fog_term_follows_the_effect_and_the_coupling() {
    let mut app = app();
    app.insert_resource(all_on());
    app.insert_resource(Weather::new(WeatherState::STORM));
    app.update();
    app.update();
    let config = all_on();
    let want = WeatherState::STORM.rates(&config).extinction_per_m();
    assert_eq!(app.world().resource::<WeatherAir>().extinction_per_m, want);

    // Off again: the resource this system wrote goes away.
    app.world_mut().resource_mut::<PrecipitationSettings>().enabled = false;
    app.update();
    assert!(app.world().get_resource::<WeatherAir>().is_none());

    let mut app = self::app();
    let mut uncoupled = all_on();
    uncoupled.visibility.couple_fog = false;
    app.insert_resource(uncoupled);
    app.insert_resource(Weather::new(WeatherState::STORM));
    app.update();
    app.update();
    assert!(app.world().get_resource::<WeatherAir>().is_none());
}

/// A host's own `WeatherAir` stays while the effect is off.
#[test]
fn a_hosts_air_is_left_alone() {
    let mut app = app();
    app.insert_resource(Weather::new(WeatherState::STORM));
    app.insert_resource(WeatherAir {
        extinction_per_m: 0.5,
    });
    app.update();
    app.update();
    assert_eq!(app.world().resource::<WeatherAir>().extinction_per_m, 0.5);
}

/// § 11 test 4: inserted once, written every frame in `Weather` mode, held in `Manual`, and a
/// paused weather holds it.
#[test]
fn the_scene_wind_follows_the_weather_on_game_time() {
    let mut app = app();
    app.insert_resource(all_on());
    app.insert_resource(Weather::new(WeatherState::STORM));
    app.update();
    app.update();
    let first = *app.world().resource::<SceneWind>();
    assert_eq!(first.mode, SceneWindMode::Weather);
    assert!(first.mean_speed() > 0.0);
    let expected = 0.4888 * WeatherState::STORM.wind_m_per_s[0];
    assert!((first.mean_m_per_s[0] - expected).abs() < 0.01, "{first:?}");
    for _ in 0..3 {
        app.update();
    }
    let later = *app.world().resource::<SceneWind>();
    assert_ne!(later.velocity_m_per_s, first.velocity_m_per_s, "the gusts run");
    app.world_mut().resource_mut::<Weather>().paused = true;
    app.update();
    let held = *app.world().resource::<SceneWind>();
    app.update();
    app.update();
    assert_eq!(
        *app.world().resource::<SceneWind>(),
        held,
        "a paused weather holds"
    );
    app.world_mut().resource_mut::<Weather>().paused = false;
    app.world_mut().insert_resource(SceneWind::manual([1.0, 2.0]));
    app.update();
    assert_eq!(
        *app.world().resource::<SceneWind>(),
        SceneWind::manual([1.0, 2.0])
    );
}

/// A wind the height factor of a genus' cloud base follows.
#[test]
fn the_height_profile_reads_the_cloud_base() {
    let mut app = app();
    app.insert_resource(all_on());
    app.insert_resource(CloudSettings {
        altitude_bottom: 640.0,
        ..CloudSettings::default()
    });
    app.insert_resource(Weather::new(WeatherState {
        gustiness: 0.0,
        ..WeatherState::STORM
    }));
    app.update();
    app.update();
    let wind = *app.world().resource::<SceneWind>();
    let factor = (10.0_f32 / 640.0).powf(1.0 / 7.0);
    let want = WeatherState::STORM.wind_m_per_s[0] * factor;
    assert!((wind.mean_m_per_s[0] - want).abs() < 1.0e-3, "{wind:?}");
}

/// § 11 test 5: the surface wets on game time, starts dry, and `Weather::set_now` does not wet it
/// at once.
#[test]
fn the_surface_wets_at_its_rate() {
    let mut app = app();
    app.insert_resource(all_on());
    app.insert_resource(Weather::new(WeatherState::STORM));
    app.update();
    app.update();
    assert!(app.world().resource::<SurfaceWeather>().wetness < 0.5);
    let start = app.world().resource::<SurfaceWeather>().wetness;
    // 50 mm/h over wet_mm 0.2 wets in 14.4 s; ten 100 ms frames add about 0.07.
    for _ in 0..10 {
        app.update();
    }
    let wet = app.world().resource::<SurfaceWeather>();
    assert!(
        wet.wetness - start > 0.05 && wet.wetness - start < 0.09,
        "{wet:?}"
    );
    assert_eq!(wet.ripple, 1.0);
    assert_eq!(wet.rain_mm_per_h, 50.0);
}

/// § 3.5: without air nothing falls, nothing dries and the scene wind reads as off.
#[test]
fn an_airless_level_holds_the_surface() {
    let mut app = app();
    let mut level = LevelSettings::default();
    level.atmosphere.preset = AtmospherePreset::Airless;
    app.insert_resource(level);
    app.insert_resource(all_on());
    app.insert_resource(Weather::new(WeatherState::STORM));
    app.insert_resource(SurfaceWeather {
        wetness: 0.4,
        mode: SurfaceWeatherMode::Simulated,
        ..SurfaceWeather::DRY
    });
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(app.world().resource::<SurfaceWeather>().wetness, 0.4);
    assert_eq!(app.world().resource::<WeatherAir>().extinction_per_m, 0.0);
    assert_eq!(
        app.world().resource::<SceneWind>().mode,
        SceneWindMode::Weather,
        "the stored mode is kept"
    );
}

/// `start` names the weather a level opens with when there is none.
#[test]
fn a_level_starts_in_the_configs_preset() {
    let mut app = app();
    app.insert_resource(WeatherConfig::from_ron("(start: Some(\"rain\"))").expect("config"));
    app.update();
    assert!(
        app.world().get_resource::<Weather>().is_none(),
        "no level, no weather"
    );
    app.insert_resource(LevelSettings::default());
    app.update();
    let weather = app.world().resource::<Weather>();
    assert_eq!(weather.current().precipitation, WeatherState::RAIN.precipitation);
}

/// § 3.2 and § 11 test 6: the extraction carries the fog term as its own value and leaves the
/// host's fog alone, on or off.
#[test]
fn the_extraction_keeps_the_fog_term_apart_from_the_hosts_fog() {
    let mut app = App::new();
    app.add_plugins(regia_app::MinimalPlugins);
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(regia_render::RenderSettings::default());
    app.insert_resource(WeatherAir {
        extinction_per_m: 0.01,
    });
    app.add_systems(Update, regia_render::extract_frame);
    app.update();
    let settings = app.world().resource::<regia_render::RenderSettings>().fog;
    let frame = app.world().resource::<ExtractedFrame>();
    assert_eq!(frame.outdoor.precip_extinction_per_m, 0.01);
    assert_eq!(frame.fog, settings, "the host's fog is never modified");
    assert_eq!(frame.outdoor.scene_wind, None, "no SceneWind, no override");
    app.world_mut().insert_resource(SceneWind {
        mode: SceneWindMode::Off,
        ..SceneWind::manual([1.0, 2.0])
    });
    app.update();
    assert_eq!(
        app.world().resource::<ExtractedFrame>().outdoor.scene_wind,
        Some(None)
    );
    app.world_mut().insert_resource(SceneWind::manual([1.0, 2.0]));
    app.update();
    assert_eq!(
        app.world().resource::<ExtractedFrame>().outdoor.scene_wind,
        Some(Some([1.0, 2.0]))
    );
}

/// The drivers' cost on the CPU (spec 99 § 12, ≤ 0.02 ms a frame with the precipitation step):
/// `cargo test --release -p regia_vfx --test weather_effects bench -- --ignored --nocapture`.
#[test]
#[ignore = "prints numbers"]
fn bench_the_drivers() {
    let mut app = app();
    app.insert_resource(all_on());
    app.insert_resource(Weather::new(WeatherState::STORM));
    app.update();
    let started = std::time::Instant::now();
    for _ in 0..10_000 {
        app.update();
    }
    println!(
        "{:.4} ms an app update (drivers and the empty schedule)",
        started.elapsed().as_secs_f64() * 1000.0 / 10_000.0
    );
}

/// The precipitation slants with the scene wind (ADR-3249, spec 99 § 3.3): the drops are carried
/// by whatever `SceneWind` holds (a host's or an agent's manual wind too), and `Off` stills them.
#[test]
fn the_precipitation_slants_with_the_scene_wind() {
    let mut app = app();
    app.insert_resource(all_on());
    app.insert_resource(Weather::new(WeatherState::STORM));
    app.insert_resource(regia_vfx::PrecipitationHandle::default());
    app.update();
    app.update();
    let carried = |app: &App| {
        let handle = app.world().resource::<regia_vfx::PrecipitationHandle>();
        handle
            .state
            .lock()
            .map(|s| s.wind_m_per_s)
            .unwrap_or([f32::NAN; 3])
    };
    let weather_wind = *app.world().resource::<SceneWind>();
    assert_eq!(carried(&app), weather_wind.velocity3(), "the weather's own wind");
    app.world_mut().insert_resource(SceneWind::manual([6.0, -2.0]));
    app.update();
    assert_eq!(carried(&app), [6.0, 0.0, -2.0], "a manual scene wind");
    let mut off = SceneWind::manual([6.0, -2.0]);
    off.mode = SceneWindMode::Off;
    app.world_mut().insert_resource(off);
    app.update();
    assert_eq!(carried(&app), [0.0; 3], "an `Off` scene wind");
}

/// § 3.5: on an airless level nothing falls and nothing is carried: the precipitation reports no
/// drops and its state is inactive with no wind, where the same storm in air draws rain.
#[test]
fn an_airless_level_draws_no_precipitation() {
    let run = |preset: AtmospherePreset| {
        let mut app = app();
        let mut level = LevelSettings::default();
        level.atmosphere.preset = preset;
        app.insert_resource(level);
        app.insert_resource(all_on());
        app.insert_resource(Weather::new(WeatherState::STORM));
        app.insert_resource(regia_vfx::PrecipitationHandle::default());
        for _ in 0..4 {
            app.update();
        }
        let report = *app.world().resource::<regia_vfx::PrecipitationReport>();
        let state = app
            .world()
            .resource::<regia_vfx::PrecipitationHandle>()
            .state
            .lock()
            .map(|s| (s.active, s.wind_m_per_s, s.rates))
            .expect("precipitation state");
        (report, state)
    };
    let (air, (air_active, air_wind, air_rates)) = run(AtmospherePreset::default());
    assert!(air.rain_drawn > 0, "the control: a storm in air rains");
    assert!(air_active && air_rates.rain_mm_per_h > 0.0);
    assert_ne!(air_wind, [0.0; 3], "the control: the rain is carried");
    let (airless, (active, wind, rates)) = run(AtmospherePreset::Airless);
    assert_eq!(
        (airless.rain_drawn, airless.snow_drawn, airless.splashes),
        (0, 0, 0)
    );
    assert!(!active, "no precipitation without air");
    assert_eq!(wind, [0.0; 3], "nothing carries it");
    assert!(rates.is_none());
}

/// § 3.5 and § 3.3: the extraction treats the scene wind as off on an airless level (the
/// renderer's foliage wind is `None`) and keeps the stored resource, so it applies again in air.
#[test]
fn the_extraction_turns_the_scene_wind_off_without_air() {
    let mut app = App::new();
    app.add_plugins(regia_app::MinimalPlugins);
    app.init_resource::<ExtractedFrame>();
    app.insert_resource(regia_render::RenderSettings::default());
    app.insert_resource(SceneWind::manual([3.0, -1.0]));
    let mut level = LevelSettings::default();
    level.atmosphere.preset = AtmospherePreset::Airless;
    app.insert_resource(level);
    app.add_systems(Update, regia_render::extract_frame);
    app.update();
    assert_eq!(
        app.world().resource::<ExtractedFrame>().outdoor.scene_wind,
        Some(None),
        "no air, no foliage wind"
    );
    assert_eq!(
        *app.world().resource::<SceneWind>(),
        SceneWind::manual([3.0, -1.0]),
        "the stored wind is kept"
    );
    app.world_mut().resource_mut::<LevelSettings>().atmosphere.preset = AtmospherePreset::default();
    app.update();
    assert_eq!(
        app.world().resource::<ExtractedFrame>().outdoor.scene_wind,
        Some(Some([3.0, -1.0])),
        "in air the wind applies again"
    );
}

/// § 5.5: splashes draw only with the occlusion view (the shader culls every splash without it),
/// so the report's drawn splashes are 0 until the renderer reports a view, then the weather's.
#[test]
fn splashes_are_reported_drawn_only_with_the_occlusion_view() {
    let mut app = app();
    app.insert_resource(all_on());
    app.insert_resource(Weather::new(WeatherState::STORM));
    for _ in 0..3 {
        app.update();
    }
    let report = *app.world().resource::<regia_vfx::PrecipitationReport>();
    assert!(report.budget.splashes > 0, "the tier has a splash budget");
    assert!(report.rain_drawn > 0, "the storm rains");
    assert_eq!(report.occlusion, None, "no renderer, no occlusion view");
    assert_eq!(report.splashes, 0, "no view, no splash drawn");
    // The renderer's side reports a view (regia_vfx::runtime writes it after each frame).
    app.world_mut()
        .resource_mut::<regia_vfx::PrecipitationReport>()
        .occlusion = Some(regia_render::PrecipOcclusionInfo {
        size_texels: 512,
        extent_m: 64.0,
        origin: [0.0, 0.0],
        tilt_deg: 0.0,
        redraws: 1,
        last_redraw_frame: 1,
        tile: 0,
    });
    app.update();
    let report = *app.world().resource::<regia_vfx::PrecipitationReport>();
    assert!(report.splashes > 0, "with the view the storm splashes");
    assert!(report.splashes <= report.budget.splashes);
}
