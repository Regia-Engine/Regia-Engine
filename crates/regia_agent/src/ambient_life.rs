// SPDX-License-Identifier: MPL-2.0
//! Ambient life on the agent protocol (ADR-3024, book `agents/ambient-life.md`, guide topic
//! `ambient-life`): `render.ambient_life` reads what lives and why, `render.set_ambient_life`
//! turns it on and picks the kinds, the density, the season and the seed, and
//! `render.set_ambient_birds` places a flock or a soaring bird (at the same point and with the
//! same sound names as `audio.set_ambient_spot { preset: birds }`, so the calls follow the birds).

use regia_ecs::prelude::*;
use regia_framework::ambient_bird_calls::AmbientBirdCalls;
use regia_scene::Transform;
use regia_vfx::ambient::{
    AmbientBudget, AmbientCounts, AutoAmbientBirds, BirdFlockSim, MAX_BIRDS, ambient_tier,
};
use regia_vfx::{AmbientBirds, AmbientLifeHandle, AmbientLifeReport, AmbientLifeSettings, BirdMode, Season};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// The most sound names a flock takes.
const MAX_CALL_SOUNDS: usize = 16;

/// `F0002` with `data.reason`.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
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
            .filter(|f| f.is_finite())
            .map(|f| Some(f as f32))
            .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number"))),
    }
}

fn in_range(params: &Value, name: &str, lo: f32, hi: f32) -> Result<Option<f32>, RpcError> {
    let value = finite(params, name)?;
    if value.is_some_and(|v| !(lo..=hi).contains(&v)) {
        return Err(RpcError::invalid_params(format!(
            "`{name}` must be in {lo}..{hi}"
        )));
    }
    Ok(value)
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

fn whole(params: &Value, name: &str, lo: u64, hi: u64) -> Result<Option<u32>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .filter(|n| (lo..=hi).contains(n))
            .and_then(|n| u32::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| {
                RpcError::invalid_params(format!("`{name}` must be a whole number in {lo}..{hi}"))
            }),
    }
}

fn settings(world: &World) -> AmbientLifeSettings {
    world
        .get_resource::<AmbientLifeSettings>()
        .copied()
        .unwrap_or_default()
}

fn counts_json(counts: &AmbientCounts) -> Value {
    json!({
        "motes": AmbientCounts::instances(counts.motes),
        "swarm_members": AmbientCounts::instances(counts.swarm_members),
        "butterflies": AmbientCounts::instances(counts.butterflies),
        "fireflies": AmbientCounts::instances(counts.fireflies),
        "leaves": AmbientCounts::instances(counts.leaves),
        "birds": AmbientCounts::instances(counts.birds),
    })
}

fn budget_json(budget: &AmbientBudget) -> Value {
    json!({
        "motes": budget.motes,
        "swarm_members": budget.swarm_members,
        "butterflies": budget.butterflies,
        "fireflies": budget.fireflies,
        "leaves": budget.leaves,
        "birds": budget.birds,
    })
}

fn flock_json(entity: Entity, config: &AmbientBirds, sim: Option<&BirdFlockSim>, auto: bool) -> Value {
    let visible = sim.map_or(0.0, BirdFlockSim::visible);
    let positions: Vec<Value> = sim
        .map(|sim| {
            (0..sim.len())
                .filter(|&i| BirdFlockSim::alpha(i, visible) > 0.0)
                .filter_map(|i| sim.position(i))
                .map(|p| json!([p.x, p.y, p.z]))
                .collect()
        })
        .unwrap_or_default();
    json!({
        "entity": entity.to_bits(),
        "mode": config.mode.name(),
        "auto": auto,
        "anchor": sim.map(|s| { let a = s.anchor(); json!([a.x, a.y, a.z]) }),
        "birds": sim.map_or(config.count as usize, BirdFlockSim::len),
        "visible": visible,
        "radius_m": config.radius_m,
        "height_m": config.height_m,
        "wingspan_m": config.wingspan_m,
        "speed_m_per_s": config.speed_m_per_s,
        "call_sounds": config.call_sounds,
        "call_reach_m": config.call_reach_m(),
        "seed": config.seed,
        "steps": sim.map_or(0, BirdFlockSim::steps),
        "positions": positions,
    })
}

fn ambient_json(world: &mut World) -> Value {
    let settings = settings(world);
    let report = world.get_resource::<AmbientLifeReport>().copied();
    let quality = report.map_or_else(|| ambient_tier(world.get_resource()), |r| r.quality);
    let budget = AmbientBudget::for_quality(quality);
    let mut query = world.query::<(
        Entity,
        &AmbientBirds,
        Option<&BirdFlockSim>,
        Has<AutoAmbientBirds>,
    )>();
    let flocks: Vec<Value> = query
        .iter(world)
        .map(|(entity, config, sim, auto)| flock_json(entity, config, sim, auto))
        .collect();
    let calls = world
        .get_resource::<AmbientBirdCalls>()
        .copied()
        .unwrap_or_default();
    let sun_view = world
        .get_resource::<AmbientLifeHandle>()
        .and_then(|handle| handle.state.lock().ok().map(|state| state.wants_sun_view))
        .unwrap_or(false);
    let sun_view_tilt_deg = world
        .get_resource::<regia_render::RuntimeRenderer>()
        .and_then(|renderer| renderer.0.sun_occlusion())
        .map(|view| view.tilt_deg);
    json!({
        "enabled": settings.enabled,
        "active": report.is_some_and(|r| r.active),
        "quality_tier": quality.name(),
        "season": report.map(|r| r.season.name()),
        "season_setting": settings.season.map_or("auto", Season::name),
        "kinds": {
            "motes": settings.motes,
            "insects": settings.insects,
            "birds": settings.birds,
            "leaves": settings.leaves,
        },
        "density_scale": settings.density_scale,
        "seed": settings.seed,
        "auto_birds": settings.auto_birds,
        "conditions": report.map(|r| json!({
            "sun_elevation_sin": r.conditions.sun_elevation_sin,
            "rain_mm_per_h": r.conditions.rain_mm_per_h,
            "snow_mm_per_h": r.conditions.snow_mm_per_h,
            "wind_m_per_s": r.conditions.wind_m_per_s,
            "cloud_coverage": r.conditions.cloud_coverage,
        })),
        "activity": report.map(|r| json!({
            "motes": r.activity.motes,
            "swarms": r.activity.swarms,
            "butterflies": r.activity.butterflies,
            "fireflies": r.activity.fireflies,
            "leaves": r.activity.leaves,
            "birds": r.activity.birds,
        })),
        "drawn": counts_json(&report.map(|r| r.counts).unwrap_or_default()),
        "birds_drawn": report.map_or(0, |r| r.birds_drawn),
        "sun_view": sun_view,
        "sun_view_tilt_deg": sun_view_tilt_deg,
        "budget": budget_json(&budget),
        "max_birds": MAX_BIRDS,
        "flocks": flocks,
        "calls": { "bound": calls.bound, "unbound": calls.unbound },
    })
}

/// `render.ambient_life`.
pub(crate) fn ambient_life(world: &mut World) -> Result<Value, RpcError> {
    Ok(ambient_json(world))
}

/// `render.set_ambient_life`.
pub(crate) fn set_ambient_life(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let enabled = boolean(params, "enabled")?;
    let motes = boolean(params, "motes")?;
    let insects = boolean(params, "insects")?;
    let birds = boolean(params, "birds")?;
    let leaves = boolean(params, "leaves")?;
    let auto_birds = boolean(params, "auto_birds")?;
    let density = in_range(params, "density_scale", 0.0, 4.0)?;
    let seed = whole(params, "seed", 0, u64::from(u32::MAX))?;
    let season = match params.get("season") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) if name == "auto" => Some(None),
        Some(Value::String(name)) => match Season::parse(name) {
            Some(season) => Some(Some(season)),
            None => {
                return Err(refused(
                    "ambient_unknown_season",
                    format!("unknown season {name:?}"),
                    "use auto, spring, summer, autumn (or fall) or winter",
                ));
            }
        },
        Some(_) => return Err(RpcError::invalid_params("`season` must be a string")),
    };
    let before = ambient_json(world);
    let mut next = settings(world);
    next.enabled = enabled.unwrap_or(next.enabled);
    next.motes = motes.unwrap_or(next.motes);
    next.insects = insects.unwrap_or(next.insects);
    next.birds = birds.unwrap_or(next.birds);
    next.leaves = leaves.unwrap_or(next.leaves);
    next.auto_birds = auto_birds.unwrap_or(next.auto_birds);
    next.density_scale = density.unwrap_or(next.density_scale);
    next.seed = seed.unwrap_or(next.seed);
    if let Some(season) = season {
        next.season = season;
    }
    world.insert_resource(next);
    Ok(json!({ "before": before, "after": ambient_json(world) }))
}

/// `render.set_ambient_birds`.
pub(crate) fn set_ambient_birds(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if boolean(params, "clear")? == Some(true) {
        let mut query = world.query_filtered::<Entity, (With<AmbientBirds>, Without<AutoAmbientBirds>)>();
        let doomed: Vec<Entity> = query.iter(world).collect();
        for entity in &doomed {
            world.despawn(*entity);
        }
        return Ok(json!({ "cleared": doomed.len() }));
    }
    let position = match crate::params::opt_array(params, "position")? {
        Some(items) if items.len() == 3 => {
            let mut p = [0.0f32; 3];
            for (slot, item) in p.iter_mut().zip(items) {
                *slot = item
                    .as_f64()
                    .filter(|v| v.is_finite())
                    .ok_or_else(|| RpcError::invalid_params("`position` must be three finite numbers"))?
                    as f32;
            }
            p
        }
        _ => {
            return Err(RpcError::invalid_params(
                "`position` [x, y, z] is required (or clear: true)",
            ));
        }
    };
    let mode = match params.get("preset") {
        None => Some(BirdMode::Flock),
        Some(preset) => preset.as_str().and_then(BirdMode::parse),
    };
    let Some(mode) = mode else {
        return Err(refused(
            "ambient_unknown_preset",
            format!("unknown bird preset {}", params["preset"]),
            "use flock, soaring or single",
        ));
    };
    let mut config = AmbientBirds::preset(mode);
    if let Some(count) = whole(params, "count", 1, MAX_BIRDS as u64)? {
        config.count = count;
    }
    if let Some(v) = in_range(params, "radius_m", 2.0, 2000.0)? {
        config.radius_m = v;
    }
    if let Some(v) = in_range(params, "height_m", 1.0, 2000.0)? {
        config.height_m = v;
    }
    if let Some(v) = in_range(params, "wingspan_m", 0.05, 4.0)? {
        config.wingspan_m = v;
    }
    if let Some(v) = in_range(params, "speed_m_per_s", 1.0, 40.0)? {
        config.speed_m_per_s = v;
    }
    if let Some(seed) = whole(params, "seed", 0, u64::from(u32::MAX))? {
        config.seed = seed;
    }
    match params.get("call_sounds") {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) if items.len() <= MAX_CALL_SOUNDS => {
            config.call_sounds = items
                .iter()
                .map(|item| {
                    item.as_str()
                        .filter(|s| !s.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| RpcError::invalid_params("`call_sounds` must be sound names"))
                })
                .collect::<Result<_, _>>()?;
        }
        Some(_) => {
            return Err(RpcError::invalid_params(format!(
                "`call_sounds` must be a list of at most {MAX_CALL_SOUNDS} sound names"
            )));
        }
    }
    let anchor = regia_core::math::Vec3::from_array(position);
    let entity = world
        .spawn((config.clone(), Transform::from_translation(anchor)))
        .id();
    let flock = flock_json(entity, &config, None, false);
    Ok(json!({ "entity": entity.to_bits(), "flock": flock }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_app::prelude::*;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(regia_app::TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(50),
        ));
        app.add_plugins((regia_app::MinimalPlugins, regia_vfx::ambient::AmbientLifePlugin));
        app.insert_resource(regia_render::TimeOfDay {
            hour: 15.0,
            day_of_year: 200,
            latitude_deg: 45.0,
        });
        app
    }

    #[test]
    fn ambient_life_reads_sets_and_places_birds() {
        let mut app = app();
        let off = ambient_life(app.world_mut()).expect("read");
        assert_eq!(off["enabled"], false);
        assert_eq!(off["active"], false);
        let out = set_ambient_life(
            app.world_mut(),
            &json!({ "enabled": true, "season": "autumn", "density_scale": 2.0, "seed": 4 }),
        )
        .expect("set");
        assert_eq!(out["before"]["enabled"], false);
        assert_eq!(out["after"]["enabled"], true);
        assert_eq!(out["after"]["season_setting"], "autumn");
        let placed = set_ambient_birds(
            app.world_mut(),
            &json!({ "position": [10.0, 0.0, -40.0], "preset": "flock", "count": 8, "call_sounds": ["bird_a"] }),
        )
        .expect("place");
        assert_eq!(placed["flock"]["mode"], "flock");
        for _ in 0..10 {
            app.update();
        }
        let now = ambient_life(app.world_mut()).expect("read");
        assert_eq!(now["active"], true);
        assert_eq!(now["season"], "autumn");
        let flocks = now["flocks"].as_array().expect("flocks");
        assert_eq!(flocks.len(), 1, "the placed flock, no automatic birds");
        assert_eq!(flocks[0]["birds"], 8);
        assert_eq!(flocks[0]["call_sounds"][0], "bird_a");
        assert!(!flocks[0]["positions"].as_array().expect("positions").is_empty());
        assert!(now["drawn"]["leaves"].as_u64().unwrap_or(0) > 0);
        assert!(now["sun_view"].is_boolean());
        assert!(now["sun_view_tilt_deg"].is_null(), "no frame drew it");
        let cleared = set_ambient_birds(app.world_mut(), &json!({ "clear": true })).expect("clear");
        assert_eq!(cleared["cleared"], 1);
    }

    #[test]
    fn ambient_life_refuses_bad_params() {
        let mut app = app();
        let reason = |err: RpcError| err.data.unwrap_or_default()["reason"].clone();
        let err = set_ambient_life(app.world_mut(), &json!({ "season": "monsoon" })).expect_err("season");
        assert_eq!(reason(err), "ambient_unknown_season");
        assert!(set_ambient_life(app.world_mut(), &json!({ "density_scale": 9.0 })).is_err());
        assert!(set_ambient_life(app.world_mut(), &json!({ "enabled": "yes" })).is_err());
        let err = set_ambient_birds(
            app.world_mut(),
            &json!({ "position": [0.0, 0.0, 0.0], "preset": 5 }),
        )
        .expect_err("a preset that is not a name");
        assert_eq!(reason(err), "ambient_unknown_preset");
        let err = set_ambient_birds(
            app.world_mut(),
            &json!({ "position": [0.0, 0.0, 0.0], "preset": "dragon" }),
        )
        .expect_err("preset");
        assert_eq!(reason(err), "ambient_unknown_preset");
        assert!(set_ambient_birds(app.world_mut(), &json!({ "preset": "flock" })).is_err());
        assert!(
            set_ambient_birds(
                app.world_mut(),
                &json!({ "position": [0.0, 0.0, 0.0], "count": 0 })
            )
            .is_err()
        );
        assert!(
            set_ambient_birds(
                app.world_mut(),
                &json!({ "position": [0.0, 0.0, 0.0], "call_sounds": [""] })
            )
            .is_err()
        );
    }
}
