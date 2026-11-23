// SPDX-License-Identifier: MPL-2.0
//! The weather's effects on the agent protocol (spec 99 § 8, book
//! `agents/weather-precipitation.md`, guide topic `weather-precipitation`): the scene wind, the
//! drawn precipitation, the surface state and the occlusion view's readback.

use regia_ecs::prelude::*;
use regia_render::{CloudSettings, RuntimeRenderer};
use regia_scene::weather::{SceneWind, SceneWindMode, SurfaceWeather, SurfaceWeatherMode, WeatherAir};
use regia_vfx::weather::{
    PrecipitationRates, SurfaceConfig, WeatherConfig, sun_elevation_sin, surface_wind_factor,
};
use regia_vfx::{PrecipBudget, PrecipCounts, PrecipitationReport, PrecipitationSettings, Weather};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// The longest wind a call may set (m/s), as `render.set_foliage_wind`'s.
const MAX_WIND_M_PER_S: f32 = 40.0;
/// The most points `render.surface_exposure` takes.
const MAX_EXPOSURE_POINTS: usize = 256;

fn missing_renderer() -> RpcError {
    RpcError::not_found(
        "no RuntimeRenderer resource in this app",
        "add regia_render::RenderPlugin (a host driving FrameRenderer itself reads \
         FrameRenderer::precipitation_occlusion)",
    )
}

fn domain_error(code: i64, regia_code: &str, reason: &str, message: &str, hint: &str) -> RpcError {
    let mut error = RpcError::new(code, regia_code, message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn finite(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .map(|f| f as f32)
            .filter(|f| f.is_finite())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number"))),
    }
}

fn boolean(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be true or false"))),
    }
}

fn config(world: &World) -> WeatherConfig {
    world.get_resource::<WeatherConfig>().cloned().unwrap_or_default()
}

/// A wind's JSON name.
fn mode_name(mode: SceneWindMode) -> &'static str {
    match mode {
        SceneWindMode::Weather => "weather",
        SceneWindMode::Manual => "manual",
        SceneWindMode::Off => "off",
    }
}

fn cloud_base(world: &World) -> f32 {
    world
        .get_resource::<CloudSettings>()
        .copied()
        .unwrap_or_default()
        .altitude_bottom
}

fn wind_json(world: &World) -> Value {
    let wind = world.get_resource::<SceneWind>();
    let surface_factor = surface_wind_factor(cloud_base(world), &config(world).wind);
    let renderer = world
        .get_resource::<RuntimeRenderer>()
        .and_then(|renderer| renderer.0.foliage_wind());
    json!({
        "present": wind.is_some(),
        "mode": wind.map(|w| mode_name(w.mode)),
        "velocity_m_per_s": wind.map(|w| w.velocity_m_per_s),
        "mean_m_per_s": wind.map(|w| w.mean_m_per_s),
        "speed_m_per_s": wind.map(SceneWind::speed),
        "gust": wind.map(|w| {
            let mean = w.mean_speed();
            if mean > 0.0 { w.speed() / mean } else { 1.0 }
        }),
        "surface_factor": surface_factor,
        "renderer_foliage_wind": renderer,
    })
}

/// `render.scene_wind`.
pub(crate) fn scene_wind(world: &mut World) -> Result<Value, RpcError> {
    Ok(wind_json(world))
}

/// `render.set_scene_wind { mode, wind_m_per_s? }`: creates the [`SceneWind`] when absent.
pub(crate) fn set_scene_wind(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = match crate::params::opt_str(params, "mode")? {
        Some("weather") => SceneWindMode::Weather,
        Some("manual") => SceneWindMode::Manual,
        Some("off") => SceneWindMode::Off,
        _ => {
            return Err(RpcError::invalid_params(
                "`mode` must be \"weather\", \"manual\" or \"off\"",
            ));
        }
    };
    let wind = match params.get("wind_m_per_s") {
        None | Some(Value::Null) => None,
        Some(raw) => Some(
            raw.as_array()
                .filter(|a| a.len() == 2)
                .and_then(|a| Some([a[0].as_f64()? as f32, a[1].as_f64()? as f32]))
                .filter(|w| w.iter().all(|v| v.is_finite()))
                .ok_or_else(|| {
                    RpcError::invalid_params("`wind_m_per_s` must be [x, z], two finite numbers")
                })?,
        ),
    };
    if mode == SceneWindMode::Manual && wind.is_none() {
        return Err(RpcError::invalid_params(
            "`wind_m_per_s` is required with mode \"manual\"",
        ));
    }
    let before = wind_json(world);
    let mut next = world
        .get_resource::<SceneWind>()
        .copied()
        .unwrap_or(SceneWind::CALM);
    next.mode = mode;
    if let Some([x, z]) = wind {
        let speed = x.hypot(z);
        let scale = if speed > MAX_WIND_M_PER_S {
            MAX_WIND_M_PER_S / speed
        } else {
            1.0
        };
        next.velocity_m_per_s = [x * scale, z * scale];
        next.mean_m_per_s = next.velocity_m_per_s;
    }
    world.insert_resource(next);
    Ok(json!({ "before": before, "after": wind_json(world) }))
}

/// `rates` as the protocol shows them: an infinite visibility is JSON null.
pub(crate) fn rates_json(rates: &PrecipitationRates) -> Value {
    json!({
        "snow_fraction": rates.snow_fraction,
        "rain_mm_per_h": rates.rain_mm_per_h,
        "snow_we_mm_per_h": rates.snow_we_mm_per_h,
        "snow_depth_mm_per_h": rates.snow_depth_mm_per_h,
        "drop_lambda_per_mm": rates.drop_lambda_per_mm,
        "visibility_m": rates.visibility_m.is_finite().then_some(rates.visibility_m),
    })
}

fn settings(world: &World) -> PrecipitationSettings {
    world
        .get_resource::<PrecipitationSettings>()
        .copied()
        .unwrap_or_default()
}

fn precipitation_json(world: &World) -> Value {
    let settings = settings(world);
    let config = config(world);
    let rates = world
        .get_resource::<Weather>()
        .map_or(PrecipitationRates::NONE, |weather| weather.rates(&config));
    let quality = regia_vfx::precip::precipitation_tier(world.get_resource());
    let budget = PrecipBudget::for_quality(quality);
    let counts = PrecipCounts::of(&rates, &config, &settings, &budget);
    let report = world.get_resource::<PrecipitationReport>();
    let on = settings.enabled;
    json!({
        "enabled": on,
        "quality_tier": quality.name(),
        "kind": { "rain": rates.rain_mm_per_h > 0.0, "snow": rates.snow_we_mm_per_h > 0.0 },
        "rates": rates_json(&rates),
        "drawn": {
            "rain": report.map_or(0, |r| r.rain_drawn),
            "snow": report.map_or(0, |r| r.snow_drawn),
            "splashes": report.map_or(0, |r| r.splashes),
        },
        "wanted": {
            "rain": PrecipCounts::instances(counts.rain),
            "snow": PrecipCounts::instances(counts.snow),
            "splashes": PrecipCounts::instances(counts.splashes),
        },
        "budget": {
            "rain": budget.rain,
            "snow": budget.snow,
            "splashes": budget.splashes,
            "half_extent_m": budget.half_extent_m,
            "height_m": budget.height_m,
            "resolution": budget.resolution.name(),
        },
        "resolution": settings.resolution.unwrap_or(budget.resolution).name(),
        "resolution_override": settings.resolution.map(regia_vfx::PrecipResolution::name),
        "density_scale": settings.density_scale,
        "max_drops": settings.max_drops,
        "splashes": settings.splashes,
        "streak_exposure_s": settings.streak_exposure_s,
        "occlusion": report.and_then(|r| r.occlusion).map(|o| json!({
            "size_texels": o.size_texels,
            "extent_m": o.extent_m,
            "origin": o.origin,
            "tilt_deg": o.tilt_deg,
            "redraws": o.redraws,
            "last_redraw_frame": o.last_redraw_frame,
        })),
        "fog_extinction_per_m": world.get_resource::<WeatherAir>().map_or(0.0, |a| a.extinction_per_m),
    })
}

/// `render.precipitation`.
pub(crate) fn precipitation(world: &mut World) -> Result<Value, RpcError> {
    Ok(precipitation_json(world))
}

/// `render.set_precipitation`.
pub(crate) fn set_precipitation(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = boolean(params, "enabled")?;
    let density_scale = finite(params, "density_scale")?;
    if density_scale.is_some_and(|v| !(0.0..=4.0).contains(&v)) {
        return Err(RpcError::invalid_params("`density_scale` must be in 0..4"));
    }
    let max_drops = match params.get("max_drops") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .ok_or_else(|| RpcError::invalid_params("`max_drops` must be a count"))?,
        ),
    };
    let splashes = boolean(params, "splashes")?;
    let occlusion = boolean(params, "occlusion")?;
    let streak = finite(params, "streak_exposure_s")?;
    if streak.is_some_and(|v| v <= 0.0) {
        return Err(RpcError::invalid_params("`streak_exposure_s` must be positive"));
    }
    // `resolution`: "full", "half" or "quarter"; null returns to the tier's (ADR-3249).
    let resolution = match params.get("resolution") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_str()
                .and_then(regia_vfx::PrecipResolution::from_name)
                .ok_or_else(|| {
                    RpcError::invalid_params("`resolution` must be \"full\", \"half\" or \"quarter\"")
                })?,
        ),
    };
    let before = precipitation_json(world);
    let mut next = settings(world);
    next.enabled = enabled.unwrap_or(next.enabled);
    next.density_scale = density_scale.unwrap_or(next.density_scale);
    if params.get("max_drops").is_some() {
        next.max_drops = max_drops;
    }
    next.splashes = splashes.unwrap_or(next.splashes);
    next.occlusion = occlusion.unwrap_or(next.occlusion);
    next.streak_exposure_s = streak.unwrap_or(next.streak_exposure_s);
    if params.get("resolution").is_some() {
        next.resolution = resolution;
    }
    world.insert_resource(next);
    Ok(json!({ "before": before, "after": precipitation_json(world) }))
}

fn drying_factor(world: &World) -> Option<f32> {
    let weather = world.get_resource::<Weather>()?;
    let config = config(world);
    let current = weather.current();
    let wind = world
        .get_resource::<SceneWind>()
        .map_or(0.0, SceneWind::mean_speed);
    Some(
        regia_vfx::weather::SurfaceInputs {
            rates: weather.rates(&config),
            temperature_c: current.temperature_c,
            sun_elevation_sin: sun_elevation_sin(world.get_resource()),
            cloud_coverage: current.coverage,
            wind_speed_m_per_s: wind,
            snow_ratio: config.precipitation.snow_ratio,
            max_rain_mm_per_h: config.precipitation.max_rain_mm_per_h,
        }
        .drying_factor(),
    )
}

fn surface_json(world: &World) -> Value {
    let surface = world.get_resource::<SurfaceWeather>();
    let frame = world.get_resource::<regia_render::ExtractedFrame>();
    let mode = surface.map(|s| match s.mode {
        SurfaceWeatherMode::Simulated => "simulated",
        SurfaceWeatherMode::Manual => "manual",
    });
    json!({
        "present": surface.is_some(),
        "mode": mode,
        "wetness": surface.map(|s| s.wetness),
        "puddles": surface.map(|s| s.puddles),
        "snow_we_mm": surface.map(|s| s.snow_we_mm),
        "snow_depth_m": surface.map(|s| s.snow_depth_m),
        "snow_cover": surface.map(|s| s.snow_cover),
        "ripple": surface.map(|s| s.ripple),
        "rain_mm_per_h": surface.map(|s| s.rain_mm_per_h),
        "snow_we_mm_per_h": surface.map(|s| s.snow_we_mm_per_h),
        "drying_factor": drying_factor(world),
        "frame": frame.map(|f| json!({
            "wetness": f.outdoor.surface_weather.wetness,
            "puddles": f.outdoor.surface_weather.puddles,
            "snow_cover": f.outdoor.surface_weather.snow_cover,
            "ripple": f.outdoor.surface_weather.ripple,
        })),
        "terrain_variant_on": world.get_resource::<RuntimeRenderer>().is_some_and(|r| r.0.surface_weather_variant_on()),
    })
}

/// `render.surface_weather`, with each terrain's weather response (`terrain_layers`).
pub(crate) fn surface_weather(world: &mut World) -> Result<Value, RpcError> {
    let mut out = surface_json(world);
    let terrains = terrain_layers_json(world);
    if let Value::Object(map) = &mut out {
        map.insert("terrain_layers".into(), terrains);
    }
    Ok(out)
}

/// Every terrain's weather response from its `.terrain.layers.ron` v4 (spec 99 § 7.3): the
/// stack's `puddles` and, per layer, the porosity it gets (its own, or the one its scalar
/// roughness gives: `porosity_from` says which) and its snow scale. Sorted by entity.
fn terrain_layers_json(world: &mut World) -> Value {
    let mut query = world.query::<(Entity, &regia_terrain::Terrain)>();
    let mut terrains: Vec<(u64, Value)> = query
        .iter(world)
        .map(|(entity, terrain)| {
            let stack = &terrain.layers;
            let layers: Vec<Value> = stack
                .layers
                .iter()
                .map(|layer| {
                    let weather = layer.weather.unwrap_or_default();
                    let derived = ((layer.roughness - 0.25) / 0.5).clamp(0.0, 1.0);
                    let source = weather.porosity_source(layer.orm_texture.is_some());
                    // `orm` (v5, ADR-3367): per texel, from the ORM map's blue channel; no
                    // single number.
                    let porosity = match source {
                        regia_terrain::layers::PorositySource::Orm => Value::Null,
                        _ => json!(weather.porosity.unwrap_or(derived)),
                    };
                    json!({
                        "name": layer.name,
                        "porosity": porosity,
                        "porosity_from": source.as_str(),
                        "snow": weather.snow,
                    })
                })
                .collect();
            (
                entity.to_bits(),
                json!({
                    "entity": entity.to_bits(),
                    "puddles": stack.puddles,
                    "overrides": stack.has_weather_overrides(),
                    "layers": layers,
                }),
            )
        })
        .collect();
    terrains.sort_by_key(|(bits, _)| *bits);
    Value::Array(terrains.into_iter().map(|(_, v)| v).collect())
}

/// `render.set_surface_weather`: creates the resource when absent. Values are clamped to their
/// ranges (snow depth is clamped at zero); inputs and derived water equivalent must fit in f32.
/// A value given without `mode` keeps the mode.
pub(crate) fn set_surface_weather(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let mode = match params.get("mode") {
        None | Some(Value::Null) => None,
        Some(Value::String(mode)) if mode == "simulated" => Some(SurfaceWeatherMode::Simulated),
        Some(Value::String(mode)) if mode == "manual" => Some(SurfaceWeatherMode::Manual),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`mode` must be \"simulated\" or \"manual\"",
            ));
        }
    };
    let wetness = finite(params, "wetness")?;
    let puddles = finite(params, "puddles")?;
    let depth = finite(params, "snow_depth_m")?;
    let cover = finite(params, "snow_cover")?;
    let before = surface_json(world);
    let config = config(world);
    let mut next = world
        .get_resource::<SurfaceWeather>()
        .copied()
        .unwrap_or(SurfaceWeather::DRY);
    next.mode = mode.unwrap_or(next.mode);
    if let Some(v) = wetness {
        next.wetness = v.clamp(0.0, 1.0);
    }
    if let Some(v) = puddles {
        next.puddles = v.clamp(0.0, 1.0);
    }
    if let Some(v) = depth {
        next.snow_depth_m = v.max(0.0);
        let water_mm = next.snow_depth_m * 1000.0 / config.precipitation.snow_ratio;
        if !water_mm.is_finite() || !(water_mm * config.precipitation.snow_ratio).is_finite() {
            return Err(RpcError::invalid_params(
                "`snow_depth_m` must keep snow water/depth conversions finite within f32 range",
            ));
        }
        next.snow_we_mm = water_mm;
        next.snow_cover = cover_of(next.snow_depth_m, &config.surface);
    }
    if let Some(v) = cover {
        next.snow_cover = v.clamp(0.0, 1.0);
    }
    world.insert_resource(next);
    Ok(json!({ "before": before, "after": surface_json(world) }))
}

fn cover_of(depth_m: f32, config: &SurfaceConfig) -> f32 {
    let t = (depth_m / config.full_cover_m).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn parse_points(params: &Value) -> Result<Vec<[f32; 3]>, RpcError> {
    let bad = || RpcError::invalid_params("`points` must be a list of [x, y, z], three finite numbers each");
    let items = params.get("points").and_then(Value::as_array).ok_or_else(bad)?;
    if items.len() > MAX_EXPOSURE_POINTS {
        return Err(RpcError::invalid_params(format!(
            "`points` holds at most {MAX_EXPOSURE_POINTS} points"
        )));
    }
    items
        .iter()
        .map(|item| {
            item.as_array()
                .filter(|a| a.len() == 3)
                .and_then(|a| {
                    Some([
                        a[0].as_f64()? as f32,
                        a[1].as_f64()? as f32,
                        a[2].as_f64()? as f32,
                    ])
                })
                .filter(|p| p.iter().all(|v| v.is_finite()))
                .ok_or_else(bad)
        })
        .collect()
}

/// `render.surface_exposure { points }`: the first call schedules a one-shot copy of the occlusion
/// tile and answers `ready: false`; a call after the next frame answers with the map's values.
pub(crate) fn surface_exposure(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let points = parse_points(params)?;
    let mut renderer = world
        .get_resource_mut::<RuntimeRenderer>()
        .ok_or_else(missing_renderer)?;
    if renderer.0.precipitation_occlusion().is_none() {
        return Err(domain_error(
            protocol::INVALID_PARAMS,
            "F0002",
            "occlusion_off",
            "the frame has no precipitation occlusion view",
            "turn it on: render.set_precipitation { enabled: true } with rain or snow falling, or a \
             wet SurfaceWeather, on a tier that has one",
        ));
    }
    let Some(map) = renderer.0.take_precip_occlusion_readback() else {
        renderer.0.request_precip_occlusion_readback();
        return Ok(json!({ "ready": false, "map_frame": null, "points": [] }));
    };
    let answers: Vec<Value> = points
        .iter()
        .map(|&p| {
            let (exposure, cover_y) = map.exposure_at(p);
            json!({ "p": p, "exposure": exposure, "cover_y": cover_y })
        })
        .collect();
    Ok(json!({ "ready": true, "map_frame": map.frame, "points": answers }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        World::new()
    }

    #[test]
    fn scene_wind_defaults_and_set_and_clamp() {
        let mut w = world();
        let got = scene_wind(&mut w).expect("read");
        assert_eq!(got["present"], false);
        assert!(
            got["surface_factor"]
                .as_f64()
                .is_some_and(|f| (f - 0.4888).abs() < 1.0e-3)
        );
        let out = set_scene_wind(
            &mut w,
            &json!({ "mode": "manual", "wind_m_per_s": [300.0, 400.0] }),
        )
        .expect("set");
        assert_eq!(out["before"]["present"], false);
        assert_eq!(out["after"]["mode"], "manual");
        let wind = *w.resource::<SceneWind>();
        assert!((wind.speed() - 40.0).abs() < 1.0e-3, "clamped: {}", wind.speed());
        set_scene_wind(&mut w, &json!({ "mode": "off" })).expect("off");
        assert_eq!(w.resource::<SceneWind>().mode, SceneWindMode::Off);
        assert_eq!(w.resource::<SceneWind>().active(), None);
    }

    #[test]
    fn scene_wind_bad_params_name_the_field() {
        let mut w = world();
        for (params, field) in [
            (json!({}), "mode"),
            (json!({ "mode": "storm" }), "mode"),
            (json!({ "mode": "manual" }), "wind_m_per_s"),
            (json!({ "mode": "manual", "wind_m_per_s": [1.0] }), "wind_m_per_s"),
            (
                json!({ "mode": "manual", "wind_m_per_s": [1.0, "x"] }),
                "wind_m_per_s",
            ),
        ] {
            let err = set_scene_wind(&mut w, &params).expect_err("rejected");
            assert_eq!(err.code, protocol::INVALID_PARAMS);
            assert!(err.message.contains(field), "{}", err.message);
        }
        assert!(w.get_resource::<SceneWind>().is_none());
    }

    #[test]
    fn precipitation_reads_and_sets() {
        let mut w = world();
        w.insert_resource(Weather::new(regia_vfx::WeatherState::STORM));
        let got = precipitation(&mut w).expect("read");
        assert_eq!(got["enabled"], false, "off by default");
        assert_eq!(got["kind"]["rain"], true);
        assert_eq!(got["wanted"]["rain"], 48_000);
        assert_eq!(got["budget"]["rain"], 48_000);
        assert!(got["occlusion"].is_null());
        assert_eq!(
            got["rates"]["visibility_m"].as_f64().map(f64::round),
            Some(3000.0)
        );
        let out = set_precipitation(
            &mut w,
            &json!({ "enabled": true, "density_scale": 0.5, "max_drops": 1000, "splashes": false }),
        )
        .expect("set");
        assert_eq!(out["after"]["enabled"], true);
        assert_eq!(out["after"]["wanted"]["rain"], 500);
        assert_eq!(out["after"]["max_drops"], 1000);
        assert_eq!(out["after"]["splashes"], false);
    }

    /// The resolution choice (ADR-3249): the tier's `full` by default, a run-time override, and
    /// null back to the tier's.
    #[test]
    fn precipitation_resolution_sets_and_resets() {
        let mut w = world();
        let got = precipitation(&mut w).expect("read");
        assert_eq!(got["resolution"], "full");
        assert_eq!(got["budget"]["resolution"], "full");
        assert!(got["resolution_override"].is_null());
        let out = set_precipitation(&mut w, &json!({ "resolution": "quarter" })).expect("set");
        assert_eq!(out["after"]["resolution"], "quarter");
        assert_eq!(out["after"]["resolution_override"], "quarter");
        assert_eq!(
            out["after"]["budget"]["resolution"], "full",
            "the tier's own stays"
        );
        let kept = set_precipitation(&mut w, &json!({ "density_scale": 1.0 })).expect("set");
        assert_eq!(kept["after"]["resolution"], "quarter", "other params keep it");
        let back = set_precipitation(&mut w, &json!({ "resolution": null })).expect("reset");
        assert_eq!(back["after"]["resolution"], "full");
        assert!(back["after"]["resolution_override"].is_null());
    }

    #[test]
    fn precipitation_reports_infinite_visibility_as_null() {
        let mut w = world();
        let got = precipitation(&mut w).expect("read");
        assert!(got["rates"]["visibility_m"].is_null());
        assert_eq!(got["kind"]["rain"], false);
    }

    #[test]
    fn precipitation_bad_params_name_the_field() {
        let mut w = world();
        for (params, field) in [
            (json!({ "enabled": "yes" }), "enabled"),
            (json!({ "density_scale": 9.0 }), "density_scale"),
            (json!({ "max_drops": -1 }), "max_drops"),
            (json!({ "splashes": 1 }), "splashes"),
            (json!({ "occlusion": 1 }), "occlusion"),
            (json!({ "streak_exposure_s": 0.0 }), "streak_exposure_s"),
            (json!({ "resolution": "eighth" }), "resolution"),
            (json!({ "resolution": 2 }), "resolution"),
        ] {
            let err = set_precipitation(&mut w, &params).expect_err("rejected");
            assert!(err.message.contains(field), "{}", err.message);
        }
        assert!(w.get_resource::<PrecipitationSettings>().is_none());
    }

    #[test]
    fn audit_surface_weather_rejects_overflow_without_mutation() {
        let mut w = world();
        set_surface_weather(&mut w, &json!({ "mode": "manual", "snow_depth_m": 0.03 }))
            .expect("initial surface");
        let before = surface_json(&w);
        for params in [
            json!({ "snow_depth_m": 1e308, "wetness": 0.5, "mode": "simulated" }),
            json!({ "snow_depth_m": f32::MAX }),
            json!({ "wetness": 1e308 }),
            json!({ "puddles": -1e308 }),
            json!({ "snow_cover": 1e308 }),
        ] {
            let err = set_surface_weather(&mut w, &params).expect_err("overflow");
            assert_eq!(err.code, protocol::INVALID_PARAMS);
            assert_eq!(surface_json(&w), before, "failed call must be atomic: {params}");
        }
        let mut absent = world();
        assert!(set_surface_weather(&mut absent, &json!({ "snow_depth_m": 1e308 })).is_err());
        assert!(!absent.contains_resource::<SurfaceWeather>());
    }

    #[test]
    fn audit_surface_weather_rejects_inverse_conversion_overflow() {
        let mut w = world();
        let mut cfg = WeatherConfig::default();
        cfg.precipitation.snow_ratio = 25.0;
        w.insert_resource(cfg.clone());
        w.insert_resource(SurfaceWeather::DRY);
        // The water equivalent fits in f32, but recovering depth can overflow through rounding.
        let err = set_surface_weather(&mut w, &json!({ "snow_depth_m": f32::MAX / 1000.0 }))
            .expect_err("unsafe inverse conversion");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert_eq!(*w.resource::<SurfaceWeather>(), SurfaceWeather::DRY);

        set_surface_weather(&mut w, &json!({ "snow_depth_m": 0.03 })).expect("ordinary depth");
        regia_vfx::weather::step_surface_weather(
            &mut w.resource_mut::<SurfaceWeather>(),
            &regia_vfx::weather::SurfaceInputs {
                rates: PrecipitationRates::NONE,
                temperature_c: -10.0,
                sun_elevation_sin: -1.0,
                cloud_coverage: 0.0,
                wind_speed_m_per_s: 0.0,
                snow_ratio: cfg.precipitation.snow_ratio,
                max_rain_mm_per_h: cfg.precipitation.max_rain_mm_per_h,
            },
            &cfg.surface,
            1.0,
        );
        let surface = w.resource::<SurfaceWeather>();
        assert!((surface.snow_depth_m - 0.03).abs() < 1e-6);
        assert!(surface.snow_we_mm.is_finite());
    }

    #[test]
    fn surface_weather_sets_clamps_and_keeps_the_mode() {
        let mut w = world();
        assert_eq!(surface_weather(&mut w).expect("read")["present"], false);
        let out = set_surface_weather(
            &mut w,
            &json!({ "mode": "manual", "wetness": 2.0, "puddles": -1.0 }),
        )
        .expect("set");
        assert_eq!(out["before"]["present"], false);
        assert_eq!(out["after"]["mode"], "manual");
        assert_eq!(out["after"]["wetness"], 1.0);
        assert_eq!(out["after"]["puddles"], 0.0);
        set_surface_weather(&mut w, &json!({ "snow_depth_m": 0.03 })).expect("depth");
        let surface = *w.resource::<SurfaceWeather>();
        assert_eq!(surface.mode, SurfaceWeatherMode::Manual, "the mode is kept");
        assert!(
            (surface.snow_we_mm - 3.0).abs() < 1.0e-5,
            "{}",
            surface.snow_we_mm
        );
        assert_eq!(surface.snow_cover, 1.0);
        set_surface_weather(&mut w, &json!({ "snow_cover": 0.4 })).expect("cover");
        assert_eq!(w.resource::<SurfaceWeather>().snow_cover, 0.4);
    }

    /// `render.surface_weather` lists each terrain's weather response: the derived porosity
    /// where a layer names none, the layer's own where it does, its snow scale and the stack's
    /// puddles; no terrain is an empty list.
    #[test]
    fn surface_weather_reports_the_terrain_layers() {
        use regia_terrain::{
            CdlodConfig, LayerDesc, LayerWeather, MinMaxPyramid, Terrain, TerrainLayers, TerrainMap,
        };
        let mut w = world();
        assert_eq!(
            surface_weather(&mut w).expect("read")["terrain_layers"],
            json!([])
        );
        let map = TerrainMap::from_heights(5, 5, vec![0.0; 25], 16.0, 10.0, 0.0).expect("map");
        let minmax = MinMaxPyramid::build(&map, 1).expect("pyramid");
        let rock = LayerDesc {
            name: "rock".into(),
            roughness: 0.5,
            weather: Some(LayerWeather {
                porosity: Some(0.1),
                snow: 0.25,
                porosity_from_orm: false,
            }),
            ..LayerDesc::ground()
        };
        w.spawn(Terrain {
            map,
            cdlod: CdlodConfig::default(),
            root_level: 1,
            minmax,
            layers: TerrainLayers {
                layers: vec![LayerDesc::ground(), rock],
                puddles: false,
                ..TerrainLayers::default()
            },
            holes: None,
            water: None,
        });
        let got = surface_weather(&mut w).expect("read");
        let terrain = &got["terrain_layers"][0];
        assert_eq!(terrain["puddles"], false);
        assert_eq!(terrain["overrides"], true);
        let ground = &terrain["layers"][0];
        assert_eq!(ground["porosity_from"], "roughness");
        assert!(
            (ground["porosity"].as_f64().expect("number") - 1.0).abs() < 1e-6,
            "0.95 → 1"
        );
        assert_eq!(ground["snow"], 1.0);
        let rock = &terrain["layers"][1];
        assert_eq!(rock["porosity_from"], "layer");
        assert!((rock["porosity"].as_f64().expect("number") - 0.1).abs() < 1e-6);
        assert_eq!(rock["snow"], 0.25);
    }

    /// A v5 layer that reads its porosity from its ORM map's blue channel reports
    /// `porosity_from: "orm"` and no single porosity; without an ORM map it falls back to the
    /// roughness (ADR-3367).
    #[test]
    fn surface_weather_reports_orm_porosity() {
        use regia_terrain::{
            CdlodConfig, LayerDesc, LayerWeather, MinMaxPyramid, Terrain, TerrainLayers, TerrainMap,
        };
        let mut w = world();
        let map = TerrainMap::from_heights(5, 5, vec![0.0; 25], 16.0, 10.0, 0.0).expect("map");
        let minmax = MinMaxPyramid::build(&map, 1).expect("pyramid");
        let from_orm = Some(LayerWeather {
            porosity_from_orm: true,
            ..LayerWeather::default()
        });
        let mud = LayerDesc {
            name: "mud".into(),
            orm_texture: Some("mud_orm.png".into()),
            weather: from_orm,
            ..LayerDesc::ground()
        };
        let bare = LayerDesc {
            name: "bare".into(),
            weather: from_orm,
            ..LayerDesc::ground()
        };
        w.spawn(Terrain {
            map,
            cdlod: CdlodConfig::default(),
            root_level: 1,
            minmax,
            layers: TerrainLayers {
                layers: vec![mud, bare],
                ..TerrainLayers::default()
            },
            holes: None,
            water: None,
        });
        let got = surface_weather(&mut w).expect("read");
        let terrain = &got["terrain_layers"][0];
        assert_eq!(terrain["overrides"], true);
        assert_eq!(terrain["layers"][0]["porosity_from"], "orm");
        assert!(terrain["layers"][0]["porosity"].is_null());
        assert_eq!(terrain["layers"][1]["porosity_from"], "roughness");
        assert!((terrain["layers"][1]["porosity"].as_f64().expect("number") - 1.0).abs() < 1e-6);
    }

    #[test]
    fn surface_weather_bad_params_name_the_field() {
        let mut w = world();
        for (params, field) in [
            (json!({ "mode": "wet" }), "mode"),
            (json!({ "wetness": "lots" }), "wetness"),
            (json!({ "puddles": [] }), "puddles"),
            (json!({ "snow_depth_m": "x" }), "snow_depth_m"),
            (json!({ "snow_cover": {} }), "snow_cover"),
        ] {
            let err = set_surface_weather(&mut w, &params).expect_err("rejected");
            assert!(err.message.contains(field), "{}", err.message);
        }
        assert!(w.get_resource::<SurfaceWeather>().is_none());
    }

    #[test]
    fn surface_exposure_needs_a_renderer_and_good_points() {
        let mut w = world();
        let err = surface_exposure(&mut w, &json!({ "points": [[0, 0, 0]] })).expect_err("no renderer");
        assert_eq!(err.code, protocol::NOT_FOUND);
        for params in [
            json!({}),
            json!({ "points": [[0, 0]] }),
            json!({ "points": [["x", 0, 0]] }),
            json!({ "points": vec![[0, 0, 0]; 257] }),
        ] {
            let err = surface_exposure(&mut w, &params).expect_err("rejected");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{params}");
            assert!(err.message.contains("points"), "{}", err.message);
        }
    }

    /// Spec 99 § 11 test 11: `occlusion_off`, then `ready: false`, then values.
    #[test]
    fn surface_exposure_reads_the_tile_a_frame_later() {
        use regia_render::{
            DirectionalLightUniform, FrameQuality, LabMaterial, LabMesh, LabScene, LabVertex,
            SurfaceWeatherFrame,
        };
        let Ok(device) = crate::seeing::headless_render_device() else {
            return;
        };
        let up = [0.0, 1.0, 0.0];
        let quad = |y: f32, e: f32| LabMesh {
            vertices: [[-e, y, -e], [e, y, -e], [e, y, e], [-e, y, e]]
                .into_iter()
                .map(|p| LabVertex::from_pos_normal_uv(p, up, [0.0, 0.0]))
                .collect(),
            indices: vec![0, 2, 1, 0, 3, 2],
            ..LabMesh::unskinned()
        };
        let lab = LabScene {
            width: 32,
            height: 32,
            eye: [6.0, 6.0, 6.0],
            target: [0.0, 0.0, 0.0],
            fov_y: 1.0,
            near: 0.1,
            far: 80.0,
            depth_prepass: false,
            directional_shadows: false,
            ibl: false,
            exposure_ev: 12.0,
            ambient_lux: 8_000.0,
            double_sided: true,
            post: None,
            directional: DirectionalLightUniform::new([0.0, -1.0, 0.0], [1.0; 3], 20_000.0),
            local_lights: Vec::new(),
            materials: vec![LabMaterial::default()],
            meshes: vec![quad(0.0, 20.0), quad(3.0, 2.0)],
        };
        let mut w = world();
        w.insert_resource(RuntimeRenderer::default());
        let params = json!({ "points": [[0.0, 0.0, 0.0], [8.0, 0.0, 0.0]] });
        let frame = |w: &mut World| {
            let mut renderer = w.resource_mut::<RuntimeRenderer>();
            renderer.0.set_surface_weather(SurfaceWeatherFrame {
                wetness: 1.0,
                ..SurfaceWeatherFrame::DRY
            });
            renderer
                .0
                .set_precipitation_occlusion_budget(Some((256, 32.0)), 16_384);
            let scene = lab.as_frame_scene_with_quality(FrameQuality::LAB);
            assert!(renderer.0.render_offscreen(&device, &scene).is_ok());
        };
        assert!(surface_exposure(&mut w, &params).is_err(), "no view yet");
        frame(&mut w);
        let first = surface_exposure(&mut w, &params).expect("schedules");
        assert_eq!(first["ready"], false);
        frame(&mut w);
        let second = surface_exposure(&mut w, &params).expect("answers");
        assert_eq!(second["ready"], true);
        assert_eq!(second["points"][0]["exposure"], 0.0, "under the slab");
        assert!(
            second["points"][0]["cover_y"]
                .as_f64()
                .is_some_and(|y| (y - 3.0).abs() < 0.1)
        );
        assert_eq!(second["points"][1]["exposure"], 1.0, "beside it");
        assert!(second["points"][1]["cover_y"].is_null());
    }

    #[test]
    fn surface_exposure_answers_occlusion_off_without_a_view() {
        let mut w = world();
        w.insert_resource(RuntimeRenderer::default());
        let err = surface_exposure(&mut w, &json!({ "points": [[0, 0, 0]] })).expect_err("off");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        assert_eq!(
            err.data.as_ref().map(|d| d["reason"].clone()),
            Some(json!("occlusion_off"))
        );
    }
}
