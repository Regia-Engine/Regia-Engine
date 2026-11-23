// SPDX-License-Identifier: MPL-2.0
//! `render.get_weather` / `render.set_weather`: the [`Weather`] resource that drives the
//! volumetric cloud layer (ADR-429, book `agents/weather.md`, guide topic `weather`).
//!
//! `set` creates the resource from the cloud layer the renderer uses when there is none (so
//! nothing moves until a field changes), starts a transition to the requested weather (or
//! switches at once with `transition_s` 0) and writes the cloud layer straight away, so
//! `render.get_clouds` shows the change before the next frame.

use regia_ecs::prelude::*;
use regia_render::CloudSettings;
use regia_vfx::weather::WeatherConfigStatus;
use regia_vfx::{PrecipitationKind, Weather, WeatherConfig, WeatherState, level_clouds};
use serde_json::{Value, json};

use crate::protocol::RpcError;
use regia_vfx::weather::PrecipitationRates;

/// How far the seam model looks for the farthest ground (m): past the tables' 32 km.
const SEAM_MODEL_FAR_M: f32 = 50_000.0;

/// The eye height the seam model's tables are built for (m).
const SEAM_MODEL_EYE_M: f32 = 2.0;

/// `aerial_tables` (ADR-3294): the profile's `aerial_from_tables` switch, the mode an
/// atmosphere frame of this profile takes, its cost in texel loads per pixel, and the horizon
/// seam both modes give for this sun and this precipitation (the CPU model of
/// `regia_render::horizon_seam`, Earth's air, the eye 2 m up, the farthest ground 50 km out).
fn aerial_tables_json(world: &World, extinction_per_m: f32) -> Value {
    let quality = world
        .get_resource::<regia_render::RenderSettings>()
        .map(|settings| regia_render::FrameQuality::from_profile(&settings.profile));
    let enabled = quality.as_ref().map(|q| q.aerial_from_tables);
    let mode = quality.as_ref().map_or(regia_render::TableAir::Off, |q| {
        regia_render::TableAir::for_frame(q, true)
    });
    let (aerial, past_range) = mode.aerial_fetches();
    let time = world
        .get_resource::<regia_render::TimeOfDay>()
        .copied()
        .unwrap_or_default();
    let sun = regia_render::sun_position(&time);
    let planet = regia_render::AtmosphereParameters::earth();
    let luts = regia_render::build_atmosphere_luts_for(&planet, sun.direction, SEAM_MODEL_EYE_M);
    let (color, lux) = regia_render::transmitted_sun(&luts, &planet);
    let fog = regia_render::FogSettings {
        sky_inscatter: true,
        ..regia_render::FogSettings::default()
    };
    let model = regia_render::horizon_seam::horizon_seam_model(
        &luts,
        &regia_render::horizon_seam::SeamInputs {
            to_sun: luts.sun_direction,
            sun_irradiance: color.map(|c| c * lux),
            extinction_per_m,
            far_m: SEAM_MODEL_FAR_M,
            fog: &fog,
            exposure_multiplier: 1.0,
        },
    );
    json!({
        "enabled": enabled,
        "frame_mode": mode.name(),
        "fetches_per_pixel": {
            "aerial": aerial,
            "aerial_past_range": past_range,
            "veil": if mode.is_on() { 4 } else { 0 },
        },
        "seam": {
            "off": model.off.step,
            "on": model.on.step,
            "far_m": model.far_m,
            "extinction_per_m": model.extinction_per_m,
            "sun_elevation_deg": sun.elevation_rad.to_degrees(),
        },
    })
}

/// The cloud layer the renderer uses: the app's resource, else the level's, else the defaults.
fn layer(world: &World) -> CloudSettings {
    world
        .get_resource::<CloudSettings>()
        .copied()
        .unwrap_or_else(|| level_clouds(world.get_resource::<regia_scene::level::LevelSettings>()))
}

fn state_json(state: &WeatherState) -> Value {
    json!({
        "coverage": state.coverage,
        "cloud_type": state.cloud_type,
        "density": state.density,
        "wind_m_per_s": state.wind_m_per_s,
        "precipitation": state.precipitation,
        "genus": state.genus.map(|g| g.name()),
        "temperature_c": state.temperature_c,
        "precipitation_kind": state.precipitation_kind.name(),
        "gustiness": state.gustiness,
    })
}

/// `config`: where the project's weather config came from and what went wrong with it.
fn config_json(status: Option<&WeatherConfigStatus>) -> Value {
    json!({
        "path": status.and_then(|s| s.path.as_deref()),
        "format_version": status.and_then(|s| s.format_version),
        "error": status.and_then(|s| s.error.as_ref()).map(|(code, message)| {
            json!({ "code": code, "message": message })
        }),
    })
}

fn weather_json(
    weather: Option<&Weather>,
    clouds: &CloudSettings,
    config: &WeatherConfig,
    status: Option<&WeatherConfigStatus>,
) -> Value {
    let presets = config.preset_names();
    let layer = json!({
        "coverage": clouds.coverage,
        "cloud_type": clouds.cloud_type,
        "density": clouds.density,
        "wind_m_per_s": clouds.wind_m_per_s,
        "drift_m": clouds.drift_m(),
    });
    match weather {
        None => json!({
            "active": false,
            "current": state_json(&WeatherState::from_clouds(clouds)),
            "cloud_layer": layer,
            "presets": presets,
            "rates": crate::weather_effects::rates_json(&PrecipitationRates::NONE),
            "config": config_json(status),
        }),
        Some(weather) => json!({
            "active": true,
            "current": state_json(&weather.current()),
            "target": state_json(&weather.target()),
            "transitioning": weather.transitioning(),
            "progress": weather.progress(),
            "remaining_s": weather.remaining_s(),
            "clock_s": weather.clock_s(),
            "drift_m": weather.drift_m(),
            "paused": weather.paused,
            "cloud_layer": layer,
            "presets": presets,
            "rates": crate::weather_effects::rates_json(&weather.rates(config)),
            "config": config_json(status),
        }),
    }
}

/// `render.get_weather`: whether a [`Weather`] drives the clouds, the weather now and its
/// target, the transition, the clock and wind drift, and the values the cloud layer holds.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let clouds = layer(world);
    let config = project_config(world);
    let mut out = weather_json(
        world.get_resource::<Weather>(),
        &clouds,
        &config,
        world.get_resource::<WeatherConfigStatus>(),
    );
    let extinction = world
        .get_resource::<Weather>()
        .map_or(0.0, |weather| weather.rates(&config).extinction_per_m());
    out["aerial_tables"] = aerial_tables_json(world, extinction);
    Ok(out)
}

fn project_config(world: &World) -> WeatherConfig {
    world.get_resource::<WeatherConfig>().cloned().unwrap_or_default()
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(|f| Some(f as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number"))),
    }
}

fn wind(params: &Value) -> Result<Option<[f32; 2]>, RpcError> {
    let bad = || RpcError::invalid_params("`wind_m_per_s` must be [x, z], two finite numbers");
    match params.get("wind_m_per_s") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.len() == 2 => {
            let axis = |v: &Value| v.as_f64().filter(|f| f.is_finite()).map(|f| f as f32);
            match (axis(&items[0]), axis(&items[1])) {
                (Some(x), Some(z)) => Ok(Some([x, z])),
                _ => Err(bad()),
            }
        }
        Some(_) => Err(bad()),
    }
}

/// `render.set_weather`: starts a transition to a weather built from `preset` (else the current
/// target) with the given fields replaced, over `transition_s` seconds of game time (0 or absent:
/// at once, a weather cut). `paused` stops or resumes the weather clock. Creates the [`Weather`]
/// resource from the current cloud layer when there is none, and writes the cloud layer now.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let config = project_config(world);
    let preset = match params.get("preset") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(config.preset(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown weather preset `{name}`; one of {}",
                config.preset_names().join(", ")
            ))
        })?),
        Some(_) => return Err(RpcError::invalid_params("`preset` must be a string")),
    };
    let coverage = number(params, "coverage")?;
    let cloud_type = number(params, "cloud_type")?;
    let density = number(params, "density")?;
    let precipitation = number(params, "precipitation")?;
    let temperature_c = number(params, "temperature_c")?;
    let gustiness = number(params, "gustiness")?;
    let precipitation_kind = match params.get("precipitation_kind") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_str().and_then(PrecipitationKind::from_name).ok_or_else(|| {
            RpcError::invalid_params("`precipitation_kind` must be \"auto\", \"rain\", \"snow\" or \"sleet\"")
        })?),
    };
    let wind = wind(params)?;
    let transition_s = number(params, "transition_s")?.unwrap_or(0.0);
    if transition_s < 0.0 {
        return Err(RpcError::invalid_params("`transition_s` must be at least 0"));
    }
    let paused = match params.get("paused") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`paused` must be true or false"))?,
        ),
    };
    let tables = match params.get("aerial_from_tables") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`aerial_from_tables` must be true or false"))?,
        ),
    };
    if tables.is_some() && world.get_resource::<regia_render::RenderSettings>().is_none() {
        return Err(RpcError::not_found(
            "no RenderSettings resource in this app: `aerial_from_tables` is a render profile switch",
            "the app runs without regia_render's RenderPlugin",
        ));
    }
    let mut clouds = layer(world);
    let had = world.get_resource::<Weather>().cloned();
    let status = world.get_resource::<WeatherConfigStatus>().cloned();
    let before = weather_json(had.as_ref(), &clouds, &config, status.as_ref());
    let mut weather = had.unwrap_or_else(|| Weather::from_clouds(&clouds));
    let changes_weather = preset.is_some()
        || coverage.is_some()
        || cloud_type.is_some()
        || density.is_some()
        || precipitation.is_some()
        || temperature_c.is_some()
        || gustiness.is_some()
        || precipitation_kind.is_some()
        || wind.is_some();
    if changes_weather {
        let mut target = preset.unwrap_or_else(|| weather.target());
        if let Some(v) = coverage {
            target.coverage = v;
        }
        if let Some(v) = cloud_type {
            target.cloud_type = v;
        }
        if let Some(v) = density {
            target.density = v;
        }
        if let Some(v) = precipitation {
            target.precipitation = v;
        }
        if let Some(v) = temperature_c {
            target.temperature_c = v;
        }
        if let Some(v) = gustiness {
            target.gustiness = v;
        }
        if let Some(v) = precipitation_kind {
            target.precipitation_kind = v;
        }
        if let Some(v) = wind {
            target.wind_m_per_s = v;
        }
        weather.transition_to(target, transition_s);
    }
    if let Some(paused) = paused {
        weather.paused = paused;
    }
    weather.apply(&mut clouds);
    let after = weather_json(Some(&weather), &clouds, &config, status.as_ref());
    world.insert_resource(clouds);
    world.insert_resource(weather);
    let mut out = json!({ "before": before, "after": after });
    if let Some(on) = tables
        && let Some(mut settings) = world.get_resource_mut::<regia_render::RenderSettings>()
    {
        // ADR-3294: a runtime switch, which `RenderSettings::preset` ignores.
        if settings.profile.aerial_from_tables != on {
            settings.profile.aerial_from_tables = on;
        }
        out["aerial_from_tables"] = json!(on);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_without_weather_reports_the_cloud_layer() {
        let mut world = World::new();
        let got = get(&mut world).expect("get");
        assert_eq!(got["active"], false);
        assert_eq!(got["current"]["coverage"], 0.0);
        // Every preset, in order: the five weathers, then the cloud genera lookdev added
        // (ADR-1408).
        assert_eq!(got["presets"], json!(regia_vfx::WEATHER_PRESETS));
        assert_eq!(
            regia_vfx::WEATHER_PRESETS[..5],
            ["clear", "fair", "cloudy", "overcast", "storm"]
        );
        assert!(got["rates"]["visibility_m"].is_null());
        assert!(got["config"]["error"].is_null());
    }

    /// Spec 99 § 8: the extension is additive, and the new fields reach the transition.
    #[test]
    fn set_takes_the_temperature_kind_and_gust() {
        let mut world = World::new();
        let out = set(
            &mut world,
            &json!({ "preset": "rain", "temperature_c": -2.0, "precipitation_kind": "snow", "gustiness": 0.7 }),
        )
        .expect("set");
        let current = &out["after"]["current"];
        assert_eq!(current["temperature_c"], -2.0);
        assert_eq!(current["precipitation_kind"], "snow");
        assert!(
            current["gustiness"]
                .as_f64()
                .is_some_and(|g| (g - 0.7).abs() < 1.0e-6)
        );
        assert_eq!(out["after"]["rates"]["snow_fraction"], 1.0);
        assert!(
            out["after"]["rates"]["snow_we_mm_per_h"]
                .as_f64()
                .is_some_and(|r| r > 0.0)
        );
        let err = set(&mut world, &json!({ "precipitation_kind": "hail" })).expect_err("rejected");
        assert!(err.message.contains("precipitation_kind"));
    }

    /// Spec 99 § 8: a config preset resolves, and a config error shows.
    #[test]
    fn config_presets_and_errors_reach_the_protocol() {
        let mut world = World::new();
        world.insert_resource(
            WeatherConfig::from_ron(
                "(presets: [(name: \"monsoon\", base: \"storm\", temperature_c: Some(27.0))])",
            )
            .expect("config"),
        );
        world.insert_resource(WeatherConfigStatus {
            path: Some("config://weather.ron".to_owned()),
            format_version: Some(1),
            error: Some(("weather_config_invalid".to_owned(), "bad".to_owned())),
        });
        let out = set(&mut world, &json!({ "preset": "monsoon" })).expect("set");
        assert_eq!(out["after"]["current"]["temperature_c"], 27.0);
        assert!(
            out["after"]["presets"]
                .as_array()
                .is_some_and(|p| p.contains(&json!("monsoon")))
        );
        assert_eq!(out["after"]["config"]["error"]["code"], "weather_config_invalid");
    }

    #[test]
    fn set_preset_now_writes_the_cloud_layer() {
        let mut world = World::new();
        world.insert_resource(CloudSettings {
            shadow_map: true,
            ..CloudSettings::default()
        });
        let out = set(&mut world, &json!({ "preset": "storm" })).expect("set");
        assert_eq!(out["before"]["active"], false);
        assert_eq!(out["after"]["active"], true);
        assert_eq!(out["after"]["transitioning"], false);
        let clouds = *world.get_resource::<CloudSettings>().expect("layer");
        assert_eq!(clouds.coverage, WeatherState::STORM.coverage);
        assert_eq!(clouds.wind_m_per_s, WeatherState::STORM.wind_m_per_s);
        assert!(clouds.shadow_map, "the layer's own switches are kept");
        assert!(world.get_resource::<Weather>().is_some());
    }

    #[test]
    fn set_with_a_transition_eases_and_overrides_fields() {
        let mut world = World::new();
        let out = set(
            &mut world,
            &json!({
                "preset": "overcast",
                "coverage": 0.8,
                "wind_m_per_s": [5.0, -1.0],
                "transition_s": 30.0,
            }),
        )
        .expect("set");
        assert_eq!(out["after"]["transitioning"], true);
        let weather = world.get_resource::<Weather>().expect("weather");
        assert_eq!(weather.target().coverage, 0.8);
        assert_eq!(weather.target().cloud_type, WeatherState::OVERCAST.cloud_type);
        assert_eq!(weather.target().wind_m_per_s, [5.0, -1.0]);
        assert!((weather.remaining_s() - 30.0).abs() < 1.0e-6);
        // A second partial set keeps the pending target's other fields.
        set(&mut world, &json!({ "density": 0.07, "transition_s": 10.0 })).expect("set");
        let weather = world.get_resource::<Weather>().expect("weather");
        assert_eq!(weather.target().coverage, 0.8);
        assert_eq!(weather.target().density, 0.07);
        set(&mut world, &json!({ "paused": true })).expect("pause");
        assert!(world.get_resource::<Weather>().expect("weather").paused);
    }

    #[test]
    fn bad_params_are_rejected() {
        let mut world = World::new();
        for params in [
            json!({ "preset": "hail" }),
            json!({ "preset": 3 }),
            json!({ "coverage": "lots" }),
            json!({ "wind_m_per_s": [1.0] }),
            json!({ "wind_m_per_s": [1.0, "x"] }),
            json!({ "transition_s": -1.0 }),
            json!({ "paused": "yes" }),
        ] {
            let err = set(&mut world, &params).expect_err("rejected");
            assert_eq!(err.code, crate::protocol::INVALID_PARAMS, "{params}");
            assert_eq!(err.data.as_ref().map(|d| d["code"].clone()), Some(json!("F0002")));
        }
        assert!(
            world.get_resource::<Weather>().is_none(),
            "a rejected call changes nothing"
        );
    }

    /// ADR-3294: `aerial_tables` reports the switch, the mode, the cost and the seam model;
    /// `aerial_from_tables` turns the profile switch on and off and keeps the preset.
    #[test]
    fn aerial_tables_report_and_switch() {
        use regia_render::{RenderProfilePreset, RenderSettings};
        let mut world = World::new();
        let got = get(&mut world).expect("get");
        assert_eq!(got["aerial_tables"]["enabled"], Value::Null);
        assert_eq!(got["aerial_tables"]["frame_mode"], "off");
        assert_eq!(got["aerial_tables"]["fetches_per_pixel"]["veil"], 0);
        let err = set(&mut world, &json!({ "aerial_from_tables": true })).expect_err("no renderer");
        assert_eq!(err.code, crate::protocol::NOT_FOUND);
        let err = set(&mut world, &json!({ "aerial_from_tables": "on" })).expect_err("not a bool");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);

        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        let out = set(
            &mut world,
            &json!({ "aerial_from_tables": true, "preset": "storm" }),
        )
        .expect("set");
        assert_eq!(out["aerial_from_tables"], true);
        let settings = world.get_resource::<RenderSettings>().expect("settings");
        assert!(settings.profile.aerial_from_tables);
        assert_eq!(
            settings.preset(),
            Some(RenderProfilePreset::DesktopHigh),
            "a runtime switch"
        );
        let got = get(&mut world).expect("get");
        let tables = &got["aerial_tables"];
        assert_eq!(tables["enabled"], true);
        assert_eq!(tables["frame_mode"], "full");
        assert_eq!(tables["fetches_per_pixel"]["aerial"], 8);
        assert_eq!(tables["fetches_per_pixel"]["veil"], 4);
        let (off, on) = (
            tables["seam"]["off"].as_f64().expect("off"),
            tables["seam"]["on"].as_f64().expect("on"),
        );
        assert!(on < off, "the storm's seam: on {on} vs off {off}");
        assert!(tables["seam"]["extinction_per_m"].as_f64().expect("σ") > 0.0);

        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::MobileLow));
        set(&mut world, &json!({ "aerial_from_tables": true })).expect("set");
        let got = get(&mut world).expect("get");
        assert_eq!(got["aerial_tables"]["frame_mode"], "lite");
        assert_eq!(got["aerial_tables"]["fetches_per_pixel"]["aerial"], 4);
        set(&mut world, &json!({ "aerial_from_tables": false })).expect("off");
        let settings = world.get_resource::<RenderSettings>().expect("settings");
        assert!(!settings.profile.aerial_from_tables);
    }
}
