// SPDX-License-Identifier: MPL-2.0
//! `render.get_exposure_settings` / `render.set_exposure_settings`: the live exposure and tonemap settings of the
//! post stack (`PostSettings`), which `level.set_look` does not reach: the tonemap operator, the
//! auto-exposure switch, the fixed EV, the physical-camera triple, bloom, vignette and grain.
//!
//! The map's own look (`level.get_look` / `level.set_look`, [`crate::look`]) is applied on top of
//! these values each frame; the two compose (the look's bias adds to `exposure_compensation`).

use regia_ecs::prelude::*;
use regia_render::{PostSettings, RenderSettings, TonemapOperator};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

/// Every setting `render.get_exposure_settings` reports and `render.set_exposure_settings` accepts, by wire name.
/// The coverage test checks this list against the schema, the guide topic and the book page.
pub(crate) const FIELDS: &[&str] = &[
    "auto_exposure",
    "exposure_ev",
    "exposure_compensation",
    "min_ev100",
    "max_ev100",
    "tonemap",
    "use_physical_camera",
    "aperture",
    "shutter_seconds",
    "iso",
    "contrast",
    "saturation",
    "bloom",
    "bloom_intensity",
    "vignette",
    "film_grain",
    "meter",
    "band_bounds_final_exposure",
];

/// The meter sub-fields (`meter` takes a partial object of these).
pub(crate) const METER_FIELDS: &[&str] = &[
    "low_percentile",
    "high_percentile",
    "center_weight",
    "bright_threshold",
    "bright_bias_ev",
];

/// The tonemap operator names `tonemap` accepts.
pub(crate) const TONEMAPS: &[&str] = &["agx", "aces_fitted", "neutral"];

fn tonemap_name(op: TonemapOperator) -> &'static str {
    match op {
        TonemapOperator::AgX => "agx",
        TonemapOperator::AcesFitted => "aces_fitted",
        TonemapOperator::Neutral => "neutral",
    }
}

fn parse_tonemap(name: &str) -> Option<TonemapOperator> {
    match name.to_ascii_lowercase().replace('-', "_").as_str() {
        "agx" => Some(TonemapOperator::AgX),
        "aces_fitted" | "aces" => Some(TonemapOperator::AcesFitted),
        "neutral" => Some(TonemapOperator::Neutral),
        _ => None,
    }
}

fn no_renderer() -> RpcError {
    RpcError::not_found(
        "no RenderSettings resource in this app",
        "the app runs without regia_render's RenderPlugin",
    )
}

fn post_json(post: &PostSettings) -> Value {
    json!({
        "auto_exposure": post.auto_exposure,
        "exposure_ev": post.exposure_ev,
        "exposure_compensation": post.exposure_compensation,
        "min_ev100": post.auto_exposure_min_ev100,
        "max_ev100": post.auto_exposure_max_ev100,
        "tonemap": tonemap_name(post.tonemap),
        "use_physical_camera": post.use_physical_camera,
        "aperture": post.aperture,
        "shutter_seconds": post.shutter_seconds,
        "iso": post.iso,
        "contrast": post.contrast,
        "saturation": post.saturation,
        "bloom": post.bloom,
        "bloom_intensity": post.bloom_intensity,
        "vignette": post.vignette,
        "film_grain": post.film_grain,
        "meter": {
            "low_percentile": post.meter.low_percentile,
            "high_percentile": post.meter.high_percentile,
            "center_weight": post.meter.center_weight,
            "bright_threshold": post.meter.bright_threshold,
            "bright_bias_ev": post.meter.bright_bias_ev,
        },
        "band_bounds_final_exposure": post.band_bounds_final_exposure,
    })
}

/// `render.get_exposure_settings`: the live exposure and tonemap settings, the tonemap names accepted and
/// the EV100 the physical camera triple gives.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let settings = world.get_resource::<RenderSettings>().ok_or_else(no_renderer)?;
    let post = &settings.post;
    let mut out = post_json(post);
    out["tonemaps"] = json!(TONEMAPS);
    out["camera_ev100"] = json!(regia_render::ev100_from_camera(
        post.aperture,
        post.shutter_seconds,
        post.iso
    ));
    Ok(out)
}

fn number(params: &Map<String, Value>, key: &str, lo: f64, hi: f64) -> Result<Option<f32>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match value.as_f64() {
            Some(v) if (lo..=hi).contains(&v) => Ok(Some(v as f32)),
            _ => Err(RpcError::invalid_params(format!(
                "`{key}` must be a number from {lo} to {hi}"
            ))),
        },
    }
}

fn boolean(params: &Map<String, Value>, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(RpcError::invalid_params(format!("`{key}` must be a boolean"))),
    }
}

/// `render.set_exposure_settings { ...fields }`: sets the named fields (any subset), validated before any
/// is applied. Returns `{ before, after }`. At least one field is required.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let map = params
        .as_object()
        .ok_or_else(|| RpcError::invalid_params("params must be an object"))?;
    for key in map.keys() {
        if !FIELDS.contains(&key.as_str()) {
            return Err(RpcError::invalid_params(format!(
                "unknown field `{key}`; one of {}",
                FIELDS.join(", ")
            )));
        }
    }
    if map.values().all(Value::is_null) {
        return Err(RpcError::invalid_params(format!(
            "give at least one field: {}",
            FIELDS.join(", ")
        )));
    }
    let auto_exposure = boolean(map, "auto_exposure")?;
    let exposure_ev = number(map, "exposure_ev", -20.0, 30.0)?;
    let compensation = number(map, "exposure_compensation", -16.0, 16.0)?;
    let min_ev = number(map, "min_ev100", -20.0, 30.0)?;
    let max_ev = number(map, "max_ev100", -20.0, 30.0)?;
    let tonemap = match map.get("tonemap") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_str().and_then(parse_tonemap).ok_or_else(|| {
            RpcError::invalid_params(format!("`tonemap` must be one of {}", TONEMAPS.join(", ")))
        })?),
    };
    let physical = boolean(map, "use_physical_camera")?;
    let aperture = number(map, "aperture", 0.5, 128.0)?;
    let shutter = number(map, "shutter_seconds", 1.0e-6, 3600.0)?;
    let iso = number(map, "iso", 1.0, 1.0e6)?;
    let contrast = number(map, "contrast", 0.05, 8.0)?;
    let saturation = number(map, "saturation", 0.0, 8.0)?;
    let bloom = boolean(map, "bloom")?;
    let bloom_intensity = number(map, "bloom_intensity", 0.0, 1.0)?;
    let vignette = number(map, "vignette", 0.0, 1.0)?;
    let film_grain = number(map, "film_grain", 0.0, 1.0)?;
    let band_bounds_final = boolean(map, "band_bounds_final_exposure")?;
    let meter = match map.get("meter") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => {
            for key in m.keys() {
                if !METER_FIELDS.contains(&key.as_str()) {
                    return Err(RpcError::invalid_params(format!(
                        "unknown meter field `{key}`; one of {}",
                        METER_FIELDS.join(", ")
                    )));
                }
            }
            Some((
                number(m, "low_percentile", 0.0, 1.0)?,
                number(m, "high_percentile", 0.0, 1.0)?,
                number(m, "center_weight", 0.0, 1.0)?,
                number(m, "bright_threshold", 0.0, 1.0e6)?,
                number(m, "bright_bias_ev", -4.0, 4.0)?,
            ))
        }
        Some(_) => return Err(RpcError::invalid_params("`meter` must be an object")),
    };

    let mut settings = world
        .get_resource_mut::<RenderSettings>()
        .ok_or_else(no_renderer)?;
    let post = &mut settings.post;
    let before = post_json(post);
    let (lo, hi) = (
        min_ev.unwrap_or(post.auto_exposure_min_ev100),
        max_ev.unwrap_or(post.auto_exposure_max_ev100),
    );
    if lo > hi {
        return Err(RpcError::invalid_params(format!(
            "`min_ev100` ({lo}) must not exceed `max_ev100` ({hi})"
        )));
    }
    post.auto_exposure_min_ev100 = lo;
    post.auto_exposure_max_ev100 = hi;
    if let Some(v) = auto_exposure {
        post.auto_exposure = v;
    }
    if let Some(v) = exposure_ev {
        post.exposure_ev = v;
    }
    if let Some(v) = compensation {
        post.exposure_compensation = v;
    }
    if let Some(v) = tonemap {
        post.tonemap = v;
    }
    if let Some(v) = physical {
        post.use_physical_camera = v;
    }
    if let Some(v) = aperture {
        post.aperture = v;
    }
    if let Some(v) = shutter {
        post.shutter_seconds = v;
    }
    if let Some(v) = iso {
        post.iso = v;
    }
    if let Some(v) = contrast {
        post.contrast = v;
    }
    if let Some(v) = saturation {
        post.saturation = v;
    }
    if let Some(v) = bloom {
        post.bloom = v;
    }
    if let Some(v) = bloom_intensity {
        post.bloom_intensity = v;
    }
    if let Some(v) = vignette {
        post.vignette = v;
    }
    if let Some(v) = film_grain {
        post.film_grain = v;
    }
    if let Some(v) = band_bounds_final {
        post.band_bounds_final_exposure = v;
    }
    if let Some((low, high, center, threshold, bias)) = meter {
        if let Some(v) = low {
            post.meter.low_percentile = v;
        }
        if let Some(v) = high {
            post.meter.high_percentile = v;
        }
        if let Some(v) = center {
            post.meter.center_weight = v;
        }
        if let Some(v) = threshold {
            post.meter.bright_threshold = v;
        }
        if let Some(v) = bias {
            post.meter.bright_bias_ev = v;
        }
    }
    let after = post_json(post);
    Ok(json!({ "before": before, "after": after }))
}

#[cfg(test)]
mod tests {
    use regia_render::RenderProfilePreset;

    use super::*;

    fn world() -> World {
        let mut world = World::new();
        world.insert_resource(RenderSettings::from_profile(RenderProfilePreset::DesktopHigh));
        world
    }

    #[test]
    fn no_renderer_is_an_error() {
        let mut world = World::new();
        assert!(get(&mut world).is_err());
        assert!(set(&mut world, &json!({ "bloom": false })).is_err());
    }

    #[test]
    fn get_reports_every_field() -> Result<(), RpcError> {
        let mut world = world();
        let got = get(&mut world)?;
        for field in FIELDS {
            assert!(got.get(*field).is_some(), "get_exposure lacks `{field}`");
        }
        for field in METER_FIELDS {
            assert!(got["meter"].get(*field).is_some(), "meter lacks `{field}`");
        }
        assert_eq!(got["tonemap"], "agx");
        assert_eq!(got["tonemaps"].as_array().map(Vec::len), Some(TONEMAPS.len()));
        Ok(())
    }

    #[test]
    fn set_changes_the_named_fields_only() -> Result<(), RpcError> {
        let mut world = world();
        let out = set(
            &mut world,
            &json!({
                "tonemap": "ACES-Fitted",
                "auto_exposure": false,
                "exposure_ev": 12.5,
                "exposure_compensation": 0.5,
                "min_ev100": 9.0,
                "max_ev100": 11.0,
                "use_physical_camera": true,
                "aperture": 8.0,
                "shutter_seconds": 0.01,
                "iso": 200.0,
                "contrast": 1.5,
                "saturation": 1.1,
                "bloom": false,
                "bloom_intensity": 0.1,
                "vignette": 0.2,
                "film_grain": 0.05,
                "meter": { "center_weight": 0.5, "bright_bias_ev": 1.0 },
            }),
        )?;
        assert_eq!(out["before"]["tonemap"], "agx");
        assert_eq!(out["after"]["tonemap"], "aces_fitted");
        let post = &world.resource::<RenderSettings>().post;
        assert_eq!(post.tonemap, TonemapOperator::AcesFitted);
        assert!(!post.auto_exposure && post.use_physical_camera && !post.bloom);
        assert_eq!(post.exposure_ev, 12.5);
        assert_eq!(post.auto_exposure_min_ev100, 9.0);
        assert_eq!(post.auto_exposure_max_ev100, 11.0);
        assert_eq!(post.iso, 200.0);
        assert_eq!(post.meter.center_weight, 0.5);
        assert_eq!(post.meter.bright_bias_ev, 1.0);
        // An untouched meter field keeps its value.
        assert_eq!(
            out["after"]["meter"]["low_percentile"],
            out["before"]["meter"]["low_percentile"]
        );
        Ok(())
    }

    /// Every `PostSettings` field is either settable here or named as reached another way. The
    /// exhaustive destructuring fails to compile when a field is added, so the new field has to
    /// be classified (and, if it is exposure or tonemap, documented) before this builds.
    #[test]
    fn every_post_field_is_classified() {
        let PostSettings {
            enabled: _,         // render.set_profile { post }
            bloom: _,           // set_exposure bloom
            bloom_mips: _,      // the profile owns it
            auto_exposure: _,   // set_exposure auto_exposure
            exposure_ev: _,     // set_exposure exposure_ev
            tonemap: _,         // set_exposure tonemap
            fxaa: _,            // the profile's anti-aliasing (render.set_profile)
            taa: _,             // the profile's anti-aliasing (render.set_profile)
            vignette: _,        // set_exposure vignette
            film_grain: _,      // set_exposure film_grain
            aperture: _,        // set_exposure aperture
            shutter_seconds: _, // set_exposure shutter_seconds
            iso: _,             // set_exposure iso
            use_physical_camera: _,
            bloom_intensity: _,
            exposure_compensation: _,
            auto_exposure_min_ev100: _, // set_exposure min_ev100
            auto_exposure_max_ev100: _, // set_exposure max_ev100
            contrast: _,
            saturation: _,
            grade: _,          // level.set_look grade
            meter: _,          // set_exposure meter
            lut: _,            // level.set_look lut
            motion_blur: _,    // the camera's PhysicalCamera
            depth_of_field: _, // the camera's PhysicalCamera
            local_exposure: _, // level.set_look local_exposure
            ao_intensity: _,   // a PostVolume's overrides (render.set_post_volume)
            fog_tint: _,       // a PostVolume's overrides (render.set_post_volume)
            lens_effects: _,   // render.set_lens_effects and PostVolume overrides (ADR-1863)
            // set_exposure band_bounds_final_exposure (ADR-7969)
            band_bounds_final_exposure: _,
        } = PostSettings::default();
    }

    #[test]
    fn schema_guide_and_book_name_every_field() {
        let set = crate::schema::METHODS
            .iter()
            .find(|m| m.name == "render.set_exposure_settings")
            .expect("render.set_exposure_settings is in the schema"); // invariant: test-only (cfg(test))
        let guide = crate::discovery::guide(&json!({ "topic": "look" }))
            .expect("look topic exists") // invariant: test-only (cfg(test))
            .to_string();
        let book = include_str!("../../../book/agents/exposure.md");
        for field in FIELDS {
            assert!(
                set.params.iter().any(|p| p.name == *field),
                "render.set_exposure_settings schema lacks `{field}`"
            );
            assert!(guide.contains(field), "the look guide topic lacks `{field}`");
            assert!(
                book.contains(&format!("`{field}`")),
                "book/agents/exposure.md lacks `{field}`"
            );
        }
        for field in METER_FIELDS {
            assert!(
                book.contains(&format!("`{field}`")),
                "book page lacks meter `{field}`"
            );
            assert!(
                guide.contains(field),
                "the look guide topic lacks meter `{field}`"
            );
        }
        for name in TONEMAPS {
            assert!(book.contains(name), "book page lacks tonemap `{name}`");
        }
    }

    /// ADR-7969 end to end through the dispatcher: `band_bounds_final_exposure` reads back off,
    /// turns on, changes the EV auto exposure picks for a band-floor snow frame, survives a map
    /// look and rejects a non-boolean without changing anything.
    #[test]
    fn band_switch_round_trips_through_the_dispatcher() {
        let mut world = world();
        // The dispatcher stamps authored changes with the frame number (`agent.changes`).
        world.init_resource::<regia_app::Time>();
        let call = |world: &mut World, method: &str, params: Value| {
            crate::dispatch::call_in_process(world, method, params).expect("synchronous method") // invariant: test-only (cfg(test))
        };
        let got = call(&mut world, "render.get_exposure_settings", json!({})).expect("get through protocol"); // invariant: test-only (cfg(test))
        assert_eq!(got["band_bounds_final_exposure"], false, "off by default");
        let set = call(
            &mut world,
            "render.set_exposure_settings",
            json!({
                "band_bounds_final_exposure": true,
                "min_ev100": 10.5,
                "max_ev100": 11.5,
                "exposure_compensation": 1.0,
                "meter": { "bright_threshold": 250.0, "bright_bias_ev": 1.0 },
            }),
        )
        .expect("set through protocol"); // invariant: test-only (cfg(test))
        assert_eq!(set["before"]["band_bounds_final_exposure"], false);
        assert_eq!(set["after"]["band_bounds_final_exposure"], true);
        let post = world.resource::<RenderSettings>().post.clone();
        assert!(post.band_bounds_final_exposure);
        // A frame of sunlit ice metering EV100 12.5: in the band (10.5), not today's 9.5.
        let ev = post.auto_exposure_ev100_with_coverage(0.18 * 1.2 * 12.5_f32.exp2(), 1.0);
        assert!((ev - 10.5).abs() < 1e-4, "{ev}");
        // The map's look rebuilds the meter; the switch stays.
        let mut looked = post.clone();
        regia_render::apply_level_look(
            &mut looked,
            &regia_scene::level::LevelLook {
                meter: Some(regia_scene::level::LevelMeter::default()),
                ..regia_scene::level::LevelLook::default()
            },
        );
        assert!(looked.band_bounds_final_exposure);
        // Wrong type: an error, nothing applied.
        let err = call(
            &mut world,
            "render.set_exposure_settings",
            json!({ "band_bounds_final_exposure": "yes", "bloom": false }),
        )
        .expect_err("non-boolean rejected"); // invariant: test-only (cfg(test))
        assert!(
            err.message.contains("band_bounds_final_exposure"),
            "{}",
            err.message
        );
        assert_eq!(world.resource::<RenderSettings>().post, post);
        let off = call(
            &mut world,
            "render.set_exposure_settings",
            json!({ "band_bounds_final_exposure": false }),
        )
        .expect("set through protocol"); // invariant: test-only (cfg(test))
        assert_eq!(off["after"]["band_bounds_final_exposure"], false);
        assert!(!world.resource::<RenderSettings>().post.band_bounds_final_exposure);
    }

    #[test]
    fn bad_input_changes_nothing() -> Result<(), RpcError> {
        let mut world = world();
        let before = get(&mut world)?;
        for bad in [
            json!({}),
            json!({ "tonemap": "reinhard" }),
            json!({ "tonemap": 3 }),
            json!({ "nonsense": 1 }),
            json!({ "bloom": 1 }),
            json!({ "vignette": 2.0 }),
            json!({ "exposure_ev": "high" }),
            json!({ "meter": 1 }),
            json!({ "meter": { "bogus": 1 } }),
            json!({ "bloom": false, "min_ev100": 12.0, "max_ev100": 3.0 }),
            json!({ "bloom": false, "min_ev100": 20.0 }),
        ] {
            assert!(set(&mut world, &bad).is_err(), "accepted {bad}");
        }
        assert_eq!(get(&mut world)?, before);
        Ok(())
    }
}
