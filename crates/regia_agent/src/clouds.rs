// SPDX-License-Identifier: MPL-2.0
//! `render.get_clouds` / `render.set_clouds`: the volumetric cloud layer
//! ([`CloudSettings`], book `agents/clouds.md`, guide topic `clouds`), with the cloud
//! transmittance map, its fog coupling and the overcast sky light (ADR-320, ADR-321).
//!
//! The renderer uses the app's [`CloudSettings`] resource when there is one; otherwise the open
//! level's `cloud_coverage` / `cloud_type` over the defaults. `set` writes the resource (starting
//! from those effective settings), so it then wins over the level. An airless planet still forces
//! the frame's coverage to 0 and skips the cloud pass, including a cirrus sheet on that resource.
//! `get` returns the resource (or the level) as stored; it does not apply that airless zero.
//!
//! Cloud genera and the look (ADR-1408, book `agents/cloud-types.md`): `genus` applies a
//! [`CloudGenus`] preset (heights, tiles, coverage, type, density and look); `diffusion`,
//! `erosion`, `breakup`, `breakup_scale_m` and the `cirrus_*` fields then change single values
//! (the fibrous ones, ADR-1509, included: `cirrus_fiber`, `cirrus_fiber_scale_m`,
//! `cirrus_shear_deg`, `cirrus_shear`, `cirrus_breakup`, `cirrus_hook`) and `cirrus_crisp`
//! (ADR-1516, book `agents/cirrus-crisp.md`).
//!
//! The air under the layer (ADR-5944): with `shadow_map` on, the aerial perspective and the
//! analytic haze read the cloud transmittance map along each view ray, so air under a cloud loses
//! the sun its surfaces lose; `get` reports it under `shadow_map.air`.
//!
//! Look override (ADR-1492): when a [`Weather`] drives the layer, the look fields given to `set`
//! are kept in the weather as an override applied over the genus look every step, so they stay
//! until `clear_look_override` is sent. `get` reports the pinned fields under `look_override`.

use regia_ecs::prelude::*;
use regia_render::{
    AERIAL_CLOUD_STEPS, CLOUD_SHADOW_MAP_STEPS, CLOUD_SHADOW_REFRESH_FRAMES, CloudGenus, CloudGenusLayer,
    CloudSettings, HAZE_CLOUD_STEPS, OutdoorParams,
};
use regia_vfx::{CloudLookOverride, Weather};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// The settings the renderer uses and where they come from (`resource`, `level`, `default`).
fn effective(world: &World) -> (CloudSettings, &'static str) {
    if let Some(settings) = world.get_resource::<CloudSettings>() {
        return (*settings, "resource");
    }
    if let Some(level) = world.get_resource::<regia_scene::level::LevelSettings>() {
        let mut params = OutdoorParams::default();
        params.apply_level(&level.atmosphere, false);
        return (params.clouds, "level");
    }
    (CloudSettings::default(), "default")
}

/// The pinned look fields (only the set ones) and whether any is set.
fn override_json(world: &World) -> Value {
    let pinned = world
        .get_resource::<Weather>()
        .map_or_else(CloudLookOverride::default, |w| w.look_override);
    let fields = match serde_json::to_value(pinned) {
        Ok(Value::Object(map)) => map.into_iter().filter(|(_, v)| !v.is_null()).collect(),
        _ => serde_json::Map::new(),
    };
    json!({ "active": !pinned.is_empty(), "fields": fields })
}

fn clouds_json(settings: &CloudSettings, source: &str) -> Value {
    json!({
        "source": source,
        "coverage": settings.coverage,
        "cloud_type": settings.cloud_type,
        "density": settings.density,
        "altitude_bottom": settings.altitude_bottom,
        "altitude_top": settings.altitude_top,
        "wind_m_per_s": settings.wind_m_per_s,
        "time_s": settings.time_s,
        "shadow_map": {
            "enabled": settings.shadow_map,
            "built": settings.wants_shadow_map(),
            "size": settings.shadow_map_texels(),
            "extent_m": settings.shadow_map_extent(),
            "texel_m": settings.shadow_map_extent() / settings.shadow_map_texels() as f32,
            "refresh_fraction": 1.0 / CLOUD_SHADOW_REFRESH_FRAMES as f32,
            "steps": CLOUD_SHADOW_MAP_STEPS,
            // The air under the layer (ADR-5944): the aerial perspective and the analytic haze
            // read the map on frames whose surfaces do.
            "air": {
                "shadowed": settings.shadow_map,
                "steps": AERIAL_CLOUD_STEPS,
                "haze_steps": HAZE_CLOUD_STEPS,
                "overcast": settings.shadow_map && settings.overcast_ibl,
            },
        },
        "fog_coupling": settings.fog_coupling,
        "overcast_ibl": settings.overcast_ibl,
        "shape_scale_m": settings.shape_scale_m,
        "detail_scale_m": settings.detail_scale_m,
        "genus": genus_of(settings),
        "genera": CloudGenus::NAMES,
        "look": {
            "diffusion": settings.look.diffusion,
            "erosion": settings.look.erosion,
            "breakup": settings.look.breakup,
            "breakup_scale_m": settings.look.breakup_scale_m,
            "cirrus": {
                "coverage": settings.look.cirrus.coverage,
                "altitude_m": settings.look.cirrus.altitude_m,
                "optical_depth": settings.look.cirrus.optical_depth,
                "scale_m": settings.look.cirrus.scale_m,
                "stretch": settings.look.cirrus.stretch,
                "fiber": settings.look.cirrus.fiber,
                "fiber_scale_m": settings.look.cirrus.fiber_scale_m,
                "shear_deg": settings.look.cirrus.shear_deg,
                "shear": settings.look.cirrus.shear,
                "breakup": settings.look.cirrus.breakup,
                "hook": settings.look.cirrus.hook,
                "crisp": settings.look.cirrus.crisp,
                "fibrous": settings.look.cirrus.is_fibrous(),
            },
        },
    })
}

/// The genus whose layer `settings` holds exactly, or null.
fn genus_of(settings: &CloudSettings) -> Value {
    let layer = CloudGenusLayer::of(settings);
    CloudGenus::ALL
        .into_iter()
        .find(|g| g.layer() == layer)
        .map_or(Value::Null, |g| json!(g.name()))
}

/// `render.get_clouds`: the cloud layer the renderer uses, where it comes from, and the cloud
/// transmittance map, fog coupling and overcast switches.
pub(crate) fn get(world: &mut World) -> Result<Value, RpcError> {
    let (settings, source) = effective(world);
    let mut out = clouds_json(&settings, source);
    out["look_override"] = override_json(world);
    Ok(out)
}

fn number(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite() && (*f as f32).is_finite())
            .map(|f| Some(f as f32))
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`{name}` must be a finite number within f32 range"))
            }),
    }
}

fn flag(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_bool()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be true or false"))),
    }
}

/// `render.set_clouds`: changes the given fields (others keep their value) and inserts the
/// result as the app's [`CloudSettings`] resource. `shadow_map_size` is clamped to 64..2048
/// texels and `shadow_map_extent_m` to 1..200 km, as the renderer does.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (before, source) = effective(world);
    let mut after = before;
    match params.get("genus") {
        None | Some(Value::Null) => {}
        Some(Value::String(name)) => {
            let genus = CloudGenus::from_name(name).ok_or_else(|| {
                RpcError::invalid_params(format!(
                    "unknown cloud genus `{name}`; one of {}",
                    CloudGenus::NAMES.join(", ")
                ))
            })?;
            after = after.with_genus(genus);
        }
        Some(_) => return Err(RpcError::invalid_params("`genus` must be a string")),
    }
    let clear = flag(params, "clear_look_override")?.unwrap_or(false);
    if clear && let Some(weather) = world.get_resource::<Weather>() {
        let mut cleared = weather.clone();
        cleared.look_override = CloudLookOverride::default();
        let mut stepped = after;
        cleared.apply(&mut stepped);
        after.look = stepped.look;
    }
    let mut look = after.look;
    let mut pinned = CloudLookOverride::default();
    if let Some(v) = number(params, "diffusion")? {
        look.diffusion = v;
        pinned.diffusion = Some(v);
    }
    if let Some(v) = number(params, "erosion")? {
        look.erosion = v;
        pinned.erosion = Some(v);
    }
    if let Some(v) = number(params, "breakup")? {
        look.breakup = v;
        pinned.breakup = Some(v);
    }
    if let Some(v) = number(params, "breakup_scale_m")? {
        look.breakup_scale_m = v;
        pinned.breakup_scale_m = Some(v);
    }
    if let Some(v) = number(params, "cirrus_coverage")? {
        look.cirrus.coverage = v;
        pinned.cirrus_coverage = Some(v);
    }
    if let Some(v) = number(params, "cirrus_altitude_m")? {
        look.cirrus.altitude_m = v;
        pinned.cirrus_altitude_m = Some(v);
    }
    if let Some(v) = number(params, "cirrus_optical_depth")? {
        look.cirrus.optical_depth = v;
        pinned.cirrus_optical_depth = Some(v);
    }
    if let Some(v) = number(params, "cirrus_scale_m")? {
        look.cirrus.scale_m = v;
        pinned.cirrus_scale_m = Some(v);
    }
    if let Some(v) = number(params, "cirrus_stretch")? {
        look.cirrus.stretch = v;
        pinned.cirrus_stretch = Some(v);
    }
    if let Some(v) = number(params, "cirrus_fiber")? {
        look.cirrus.fiber = v;
        pinned.cirrus_fiber = Some(v);
    }
    if let Some(v) = number(params, "cirrus_fiber_scale_m")? {
        look.cirrus.fiber_scale_m = v;
        pinned.cirrus_fiber_scale_m = Some(v);
    }
    if let Some(v) = number(params, "cirrus_shear_deg")? {
        look.cirrus.shear_deg = v;
        pinned.cirrus_shear_deg = Some(v);
    }
    if let Some(v) = number(params, "cirrus_shear")? {
        look.cirrus.shear = v;
        pinned.cirrus_shear = Some(v);
    }
    if let Some(v) = number(params, "cirrus_breakup")? {
        look.cirrus.breakup = v;
        pinned.cirrus_breakup = Some(v);
    }
    if let Some(v) = number(params, "cirrus_hook")? {
        look.cirrus.hook = v;
        pinned.cirrus_hook = Some(v);
    }
    if let Some(v) = number(params, "cirrus_crisp")? {
        look.cirrus.crisp = v;
        pinned.cirrus_crisp = Some(v);
    }
    after.look = look.sanitized();
    if let Some(v) = number(params, "coverage")? {
        after.coverage = v.clamp(0.0, 1.0);
    }
    if let Some(v) = number(params, "cloud_type")? {
        after.cloud_type = v.clamp(0.0, 1.0);
    }
    if let Some(v) = number(params, "density")? {
        after.density = v.max(0.0);
    }
    if let Some(on) = flag(params, "shadow_map")? {
        after.shadow_map = on;
    }
    if let Some(size) = number(params, "shadow_map_size")? {
        after.shadow_map_size = (size.max(0.0) as u32).clamp(64, 2048);
    }
    if let Some(extent) = number(params, "shadow_map_extent_m")? {
        after.shadow_map_extent_m = extent.clamp(1_000.0, 200_000.0);
    }
    if let Some(on) = flag(params, "fog_coupling")? {
        after.fog_coupling = on;
    }
    if let Some(on) = flag(params, "overcast_ibl")? {
        after.overcast_ibl = on;
    }
    // Every parameter is valid: only now change the pinned look (a refused call changes nothing).
    if let Some(mut weather) = world.get_resource_mut::<Weather>() {
        let kept = if clear {
            CloudLookOverride::default()
        } else {
            weather.look_override
        };
        weather.look_override = kept.merged(pinned);
    }
    world.insert_resource(after);
    Ok(json!({
        "before": clouds_json(&before, source),
        "after": clouds_json(&after, "resource"),
        "look_override": override_json(world),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_reports_the_map_switches_off_by_default() {
        let mut world = World::new();
        let got = get(&mut world).expect("get");
        assert_eq!(got["source"], "default");
        assert_eq!(got["shadow_map"]["enabled"], false);
        assert_eq!(got["shadow_map"]["size"], 512);
        assert_eq!(got["shadow_map"]["extent_m"], 20_000.0);
        assert_eq!(got["shadow_map"]["refresh_fraction"], 0.0625);
        // ADR-5944: the air follows the map switch, so it is off too.
        assert_eq!(got["shadow_map"]["air"]["shadowed"], false);
        assert_eq!(got["shadow_map"]["air"]["overcast"], false);
        assert_eq!(got["shadow_map"]["air"]["steps"], json!([8, 4, 2]));
        assert_eq!(got["shadow_map"]["air"]["haze_steps"], 4);
        assert_eq!(got["fog_coupling"], false);
        assert_eq!(got["overcast_ibl"], false);
    }

    #[test]
    fn set_writes_the_resource_and_clamps() {
        let mut world = World::new();
        let out = set(
            &mut world,
            &json!({
                "coverage": 0.6,
                "shadow_map": true,
                "shadow_map_size": 100000,
                "shadow_map_extent_m": 30000,
                "fog_coupling": true,
                "overcast_ibl": true,
            }),
        )
        .expect("set");
        assert_eq!(out["before"]["shadow_map"]["enabled"], false);
        assert_eq!(out["after"]["shadow_map"]["enabled"], true);
        assert_eq!(out["after"]["shadow_map"]["size"], 2048);
        // ADR-5944: with the map on the air reads it, and with the overcast sky light on as
        // well its multiple scattering follows that light.
        assert_eq!(out["before"]["shadow_map"]["air"]["shadowed"], false);
        assert_eq!(out["after"]["shadow_map"]["air"]["shadowed"], true);
        assert_eq!(out["after"]["shadow_map"]["air"]["overcast"], true);
        let settings = *world.get_resource::<CloudSettings>().expect("inserted");
        assert!(settings.shadow_map && settings.fog_coupling && settings.overcast_ibl);
        assert!((settings.coverage - 0.6).abs() < 1e-6);
        assert!((settings.shadow_map_extent_m - 30_000.0).abs() < 1e-3);
        let got = get(&mut world).expect("get");
        assert_eq!(got["source"], "resource");
        // One switch alone keeps the others.
        set(&mut world, &json!({ "shadow_map": false })).expect("set");
        assert!(world.get_resource::<CloudSettings>().expect("kept").fog_coupling);
        assert!(!world.get_resource::<CloudSettings>().expect("kept").shadow_map);
        // The overcast sky light alone (no map) leaves the air as it was.
        let got = get(&mut world).expect("get");
        assert_eq!(got["shadow_map"]["air"]["shadowed"], false);
        assert_eq!(got["shadow_map"]["air"]["overcast"], false);
        assert!(set(&mut world, &json!({ "shadow_map": 3 })).is_err());
        assert!(set(&mut world, &json!({ "fog_coupling": "yes" })).is_err());
        assert!(set(&mut world, &json!({ "coverage": "lots" })).is_err());
    }

    /// `genus` applies a preset layer, single look fields then override it (clamped), and an
    /// unknown genus is refused without touching the resource.
    #[test]
    fn set_genus_and_look_fields() {
        let mut world = World::new();
        let got = get(&mut world).expect("get");
        assert_eq!(got["genus"], Value::Null);
        assert_eq!(got["look"]["diffusion"], 0.0);
        assert_eq!(got["look"]["erosion"], 0.5);
        assert_eq!(got["genera"].as_array().map(Vec::len), Some(4));
        let out = set(&mut world, &json!({ "genus": "stratocumulus" })).expect("set");
        assert_eq!(out["after"]["genus"], "stratocumulus");
        assert_eq!(out["after"]["altitude_bottom"], 900.0);
        let s = *world.get_resource::<CloudSettings>().expect("inserted");
        assert_eq!(CloudGenusLayer::of(&s), CloudGenus::Stratocumulus.layer());
        let out = set(
            &mut world,
            &json!({ "genus": "cirrus", "cirrus_coverage": 0.8, "cirrus_altitude_m": 1.0e6, "erosion": 3.0 }),
        )
        .expect("set");
        assert_eq!(
            out["after"]["genus"],
            Value::Null,
            "overridden: no longer the exact preset"
        );
        let s = *world.get_resource::<CloudSettings>().expect("kept");
        assert!((s.look.cirrus.coverage - 0.8).abs() < 1e-6);
        assert_eq!(s.look.cirrus.altitude_m, 20_000.0);
        assert_eq!(s.look.erosion, 1.0);
        assert!(set(&mut world, &json!({ "genus": "nimbus" })).is_err());
        assert!(set(&mut world, &json!({ "genus": 4 })).is_err());
        assert!(set(&mut world, &json!({ "diffusion": "lots" })).is_err());
        let kept = *world.get_resource::<CloudSettings>().expect("kept");
        assert_eq!(kept, s, "a refused call changes nothing");
    }

    /// With a `Weather` driving the layer the look fields persist over weather steps; `get`
    /// reports them; `clear_look_override` brings the genus look back.
    #[test]
    fn set_look_persists_over_weather_steps_until_cleared() {
        use regia_vfx::WeatherState;
        let mut world = World::new();
        let cirrus = WeatherState::preset("cirrus").expect("cirrus");
        world.insert_resource(Weather::new(cirrus));
        let mut clouds = CloudSettings::default();
        world.resource_mut::<Weather>().apply(&mut clouds);
        world.insert_resource(clouds);
        let genus_look = clouds.look;
        let out = set(
            &mut world,
            &json!({ "cirrus_coverage": 0.65, "cirrus_optical_depth": 0.11, "cirrus_scale_m": 1500 }),
        )
        .expect("set");
        assert_eq!(out["look_override"]["active"], true);
        for _ in 0..60 {
            let mut weather = world.resource_mut::<Weather>().clone();
            weather.step(1.0 / 60.0);
            let mut next = *world.resource::<CloudSettings>();
            weather.apply(&mut next);
            world.insert_resource(weather);
            world.insert_resource(next);
        }
        let got = get(&mut world).expect("get");
        assert_eq!(
            got["look"]["cirrus"]["coverage"].as_f64(),
            Some(f64::from(0.65_f32))
        );
        assert_eq!(got["look"]["cirrus"]["scale_m"], 1500.0);
        assert_eq!(got["look_override"]["active"], true);
        assert_eq!(got["look_override"]["fields"]["cirrus_scale_m"], 1500.0);
        assert!(got["look_override"]["fields"].get("erosion").is_none());
        // A second set merges: the earlier pins stay.
        set(&mut world, &json!({ "erosion": 0.3 })).expect("set");
        let pinned = world.resource::<Weather>().look_override;
        assert_eq!(pinned.erosion, Some(0.3));
        assert_eq!(pinned.cirrus_scale_m, Some(1500.0));
        let out = set(&mut world, &json!({ "clear_look_override": true })).expect("clear");
        assert_eq!(out["look_override"]["active"], false);
        assert_eq!(world.resource::<CloudSettings>().look, genus_look);
        assert!(set(&mut world, &json!({ "clear_look_override": 1 })).is_err());
    }

    /// ADR-1509: the fibrous cirrus fields are set, clamped, pinned over the weather's genus
    /// look, and reported by `get` (all zero and `fibrous: false` by default).
    #[test]
    fn set_and_get_the_fibrous_cirrus_fields() {
        use regia_vfx::WeatherState;
        let mut world = World::new();
        let got = get(&mut world).expect("get");
        assert_eq!(got["look"]["cirrus"]["fiber"], 0.0);
        assert_eq!(got["look"]["cirrus"]["fiber_scale_m"], 150.0);
        assert_eq!(got["look"]["cirrus"]["fibrous"], false);
        world.insert_resource(Weather::new(WeatherState::preset("cirrus").expect("cirrus")));
        let out = set(
            &mut world,
            &json!({
                "cirrus_fiber": 0.8,
                "cirrus_fiber_scale_m": 300,
                "cirrus_shear_deg": -15,
                "cirrus_shear": 3.0,
                "cirrus_breakup": 0.5,
                "cirrus_hook": 0.6,
            }),
        )
        .expect("set");
        let c = &out["after"]["look"]["cirrus"];
        assert_eq!(c["fiber"].as_f64(), Some(f64::from(0.8_f32)));
        assert_eq!(c["fiber_scale_m"], 300.0);
        assert_eq!(c["shear_deg"], -15.0);
        assert_eq!(c["shear"], 1.0, "clamped");
        assert_eq!(c["fibrous"], true);
        let fields = &out["look_override"]["fields"];
        assert_eq!(fields["cirrus_hook"].as_f64(), Some(f64::from(0.6_f32)));
        assert_eq!(fields["cirrus_breakup"], 0.5);
        let pinned = world.resource::<Weather>().look_override;
        assert_eq!(pinned.cirrus_shear, Some(3.0));
        let s = *world.resource::<CloudSettings>();
        assert_eq!(s.look.cirrus.shear, 1.0);
        assert!(set(&mut world, &json!({ "cirrus_hook": "lots" })).is_err());
        assert_eq!(
            *world.resource::<CloudSettings>(),
            s,
            "a refused call changes nothing"
        );
        // Clearing the override brings the smooth genus trails back.
        set(&mut world, &json!({ "clear_look_override": true })).expect("clear");
        let got = get(&mut world).expect("get");
        assert_eq!(got["look"]["cirrus"]["fibrous"], false);
    }

    /// ADR-1516: `cirrus_crisp` is 0 by default, set, clamped, pinned, reported, refused when
    /// not a number, and cleared with the override.
    #[test]
    fn set_and_get_the_crisp_cirrus_field() {
        use regia_vfx::WeatherState;
        let mut world = World::new();
        let got = get(&mut world).expect("get");
        assert_eq!(got["look"]["cirrus"]["crisp"], 0.0);
        world.insert_resource(Weather::new(WeatherState::preset("cirrus").expect("cirrus")));
        let out = set(&mut world, &json!({ "cirrus_crisp": 2.5, "cirrus_fiber": 1.0 })).expect("set");
        let c = &out["after"]["look"]["cirrus"];
        assert_eq!(c["crisp"], 1.0, "clamped");
        assert_eq!(c["fibrous"], true);
        assert_eq!(out["look_override"]["fields"]["cirrus_crisp"], 2.5);
        assert_eq!(world.resource::<Weather>().look_override.cirrus_crisp, Some(2.5));
        let s = *world.resource::<CloudSettings>();
        assert!(set(&mut world, &json!({ "cirrus_crisp": "sharp" })).is_err());
        assert_eq!(
            *world.resource::<CloudSettings>(),
            s,
            "a refused call changes nothing"
        );
        set(&mut world, &json!({ "clear_look_override": true })).expect("clear");
        let got = get(&mut world).expect("get");
        assert_eq!(got["look"]["cirrus"]["crisp"], 0.0);
        assert_eq!(got["look"]["cirrus"]["fibrous"], false);
    }

    /// Without a `Weather` there is nothing to overwrite: the resource holds the values and
    /// `look_override` reports inactive.
    #[test]
    fn set_look_without_weather_reports_no_override() {
        let mut world = World::new();
        let out = set(&mut world, &json!({ "cirrus_coverage": 0.4 })).expect("set");
        assert_eq!(out["look_override"]["active"], false);
        let got = get(&mut world).expect("get");
        assert_eq!(
            got["look"]["cirrus"]["coverage"].as_f64(),
            Some(f64::from(0.4_f32))
        );
    }

    /// A finite f64 that overflows f32 is refused. It is not stored as infinity on the look
    /// override, and a refused call leaves the cloud resource and the pins unchanged. Shear 3
    /// stays a raw pin (the rendered look still clamps it).
    #[test]
    fn set_rejects_f32_overflow_without_storing_infinity() {
        use regia_vfx::WeatherState;
        let mut world = World::new();
        world.insert_resource(Weather::new(WeatherState::preset("cirrus").expect("cirrus")));
        let err = set(&mut world, &json!({ "cirrus_crisp": 1.0e308 })).expect_err("overflow");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        let pinned = world.resource::<Weather>().look_override;
        assert!(pinned.is_finite(), "{pinned:?}");
        assert_eq!(pinned.cirrus_crisp, None);
        assert!(world.get_resource::<CloudSettings>().is_none());

        set(&mut world, &json!({ "cirrus_shear": 3.0, "cirrus_crisp": 2.5 })).expect("finite");
        assert_eq!(world.resource::<Weather>().look_override.cirrus_shear, Some(3.0));
        assert_eq!(world.resource::<Weather>().look_override.cirrus_crisp, Some(2.5));
        assert_eq!(world.resource::<CloudSettings>().look.cirrus.shear, 1.0);
        let kept = *world.resource::<CloudSettings>();
        let err = set(&mut world, &json!({ "diffusion": 1.0e39 })).expect_err("diffusion");
        assert_eq!(err.code, crate::protocol::INVALID_PARAMS);
        assert_eq!(*world.resource::<CloudSettings>(), kept);
        let pinned = world.resource::<Weather>().look_override;
        assert_eq!(pinned.cirrus_crisp, Some(2.5));
        assert_eq!(pinned.cirrus_shear, Some(3.0));
        assert!(pinned.diffusion.is_none());
        assert!(pinned.is_finite());
    }
}
