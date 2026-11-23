// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the crash effects (`regia_vfx::crash`, ADR-1398 and ADR-1414,
//! `book/agents/crash-fx.md`): schedule a crash sized by its yield, list the scheduled crashes,
//! and measure an effect's size and timing with the CPU probe. The blast front's dust wall and
//! gust line, and `vfx.blast_gust`, are ADR-1497 (`book/agents/crash-blast-front.md`).

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_vfx::crash::{
    BlastGusts, CrashBurst, CrashMeasure, CrashScale, CrashSequence, CrashTier, GroundClass,
    blast_arrival_seconds, blast_front_reached, crash_effect, ensure_crash_effects, measure_crash_effect,
};
use regia_vfx::probe::{EmitterProbe, ProbeSettings, probe_effect};
use regia_vfx::{VfxMedium, VfxScene, VfxSettings};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most probe times one `vfx.crash_profile` call takes.
const MAX_PROBE_TIMES: usize = 64;
/// Latest probe time, seconds (the cap is measured at 180 s).
const MAX_PROBE_SECONDS: f64 = 240.0;
/// Latest detonation a `vfx.crash` call schedules, seconds.
const MAX_IMPACT_IN: f32 = 600.0;
/// Most gust sites one `vfx.crash` call takes (the nearest `CrashSequence::MAX_GUST_PUFFS` to
/// the camera fire).
const MAX_GUST_SITES: usize = 256;

fn no_scene() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "F0016",
        "no VFX scene in this app",
        "add regia_vfx::VfxPlugin so the crash effects have a scene",
    )
}

fn vec3(value: &Value, what: &str) -> Result<Vec3, RpcError> {
    let items = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| RpcError::invalid_params(format!("`{what}` must be [x, y, z]")))?;
    let mut out = [0.0f32; 3];
    for (o, v) in out.iter_mut().zip(items) {
        let f = v
            .as_f64()
            .filter(|f| f.is_finite())
            .ok_or_else(|| RpcError::invalid_params(format!("`{what}` must hold finite numbers")))?;
        *o = f as f32;
    }
    Ok(Vec3::from_array(out))
}

fn optional_vec3(params: &Value, field: &str) -> Result<Option<Vec3>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => vec3(v, field).map(Some),
    }
}

fn optional_f32(params: &Value, field: &str, default: f32, min: f32, max: f32) -> Result<f32, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_f64()
            .map(|f| f as f32)
            .filter(|f| f.is_finite() && (min..=max).contains(f))
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number in {min}..={max}"))),
    }
}

fn optional_bool(params: &Value, field: &str, default: bool) -> Result<bool, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be true or false"))),
    }
}

/// `gust_sites`: up to [`MAX_GUST_SITES`] `[x, y, z]` points (default none).
fn gust_sites_param(params: &Value) -> Result<Vec<Vec3>, RpcError> {
    match params.get("gust_sites") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => {
            let items = v
                .as_array()
                .filter(|a| a.len() <= MAX_GUST_SITES)
                .ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "`gust_sites` must hold at most {MAX_GUST_SITES} [x, y, z] points"
                    ))
                })?;
            items.iter().map(|p| vec3(p, "gust_sites")).collect()
        }
    }
}

fn ground_param(params: &Value) -> Result<GroundClass, RpcError> {
    match crate::params::opt_str(params, "ground")? {
        None => Ok(GroundClass::Mud),
        Some(name) => GroundClass::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("`ground` {name}: mud, snow or grass"))),
    }
}

fn scale_param(params: &Value) -> Result<CrashScale, RpcError> {
    optional_f32(
        params,
        "yield_t",
        CrashScale::VEHICLE.tonnes(),
        CrashScale::MIN_TONNES,
        CrashScale::MAX_TONNES,
    )
    .map(CrashScale::from_tonnes)
}

/// The tier from `params.tier`, else the app's VFX profile, else full.
fn tier_param(world: &World, params: &Value) -> Result<CrashTier, RpcError> {
    match crate::params::opt_str(params, "tier")? {
        Some(name) => CrashTier::from_name(name)
            .ok_or_else(|| RpcError::invalid_params(format!("`tier` {name}: full or lite"))),
        None => Ok(world
            .get_resource::<VfxSettings>()
            .map_or(CrashTier::Full, |s| CrashTier::for_profile(&s.profile))),
    }
}

/// The effect names `crash` fires, in order (the contact puff first when asked).
fn planned_effects(crash: &CrashBurst) -> Vec<String> {
    use regia_vfx::crash::{column_effect, crash_fire_sites, fireball_effect, shell_effect};
    let s = crash.scale;
    let mut out = Vec::new();
    if crash.contact {
        out.push(crash.ground.contact_effect().to_string());
    }
    for (on, name) in [
        (crash.fireball, fireball_effect(s)),
        (crash.shell, shell_effect(s)),
        (crash.dust, crash.ground.dust_effect(s)),
        (crash.ejecta, crash.ground.ejecta_effect(s)),
        (crash.column, column_effect(s)),
    ] {
        if on {
            out.push(name);
        }
    }
    if crash.fires {
        let mut sizes: Vec<&str> = crash_fire_sites(s, crash.seed)
            .iter()
            .map(|site| site.size.effect())
            .collect();
        sizes.sort_unstable();
        sizes.dedup();
        out.extend(sizes.into_iter().map(str::to_string));
    }
    out
}

/// `vfx.crash`: spawns an entity with a [`CrashSequence`] for a crash at `position` sized by
/// `yield_t`, registering the crash effects for the scale, tier and wind first. The VFX plugin
/// fires the contact puff and the detonation on its clock.
pub fn crash(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let tier = tier_param(world, params)?;
    let position = vec3(
        params
            .get("position")
            .ok_or_else(|| RpcError::invalid_params("`position` ([x, y, z]) is required"))?,
        "position",
    )?;
    let normal = optional_vec3(params, "normal")?.unwrap_or(Vec3::Y);
    if normal.length_squared() < 1e-8 {
        return Err(RpcError::invalid_params("`normal` must not be zero"));
    }
    let wind = optional_vec3(params, "wind")?;
    let defaults = CrashBurst::default();
    let contact_lead = optional_f32(params, "contact_lead", defaults.contact_lead, 0.0, 1.0)?;
    let burst = CrashBurst {
        position,
        normal,
        ground: ground_param(params)?,
        scale: scale_param(params)?,
        seed: crate::params::opt_u64(params, "seed")?.unwrap_or(defaults.seed),
        fireball: optional_bool(params, "fireball", true)?,
        shell: optional_bool(params, "shell", false)?,
        dust: optional_bool(params, "dust", true)?,
        ejecta: optional_bool(params, "ejecta", true)?,
        column: optional_bool(params, "column", true)?,
        fires: optional_bool(params, "fires", true)?,
        contact: optional_bool(params, "contact", true)?,
        contact_lead,
        contact_downrange: None,
        gust: optional_bool(params, "gust", true)?,
    };
    let gust_sites = gust_sites_param(params)?;
    let impact_in = optional_f32(params, "impact_in", contact_lead, 0.0, MAX_IMPACT_IN)?;
    {
        let mut scene = world.get_resource_mut::<VfxScene>().ok_or_else(no_scene)?;
        ensure_crash_effects(&mut scene, burst.scale, tier, wind);
    }
    let fire_lights = optional_bool(params, "fire_lights", true)?;
    let mut sequence = CrashSequence::new(burst, impact_in);
    sequence.fire_lights = fire_lights;
    let gust_count = gust_sites.len().min(CrashSequence::MAX_GUST_PUFFS);
    sequence.gust_sites = gust_sites;
    let entity = world.spawn((Name::new("crash"), sequence)).id();
    let s = burst.scale;
    let crater = s.crater();
    let camera = world
        .get_resource::<regia_vfx::VfxView>()
        .map(|v| v.camera.eye.distance(position));
    Ok(json!({
        "entity": entity.to_bits(),
        "tier": tier.name(),
        "ground": burst.ground.name(),
        "yield_t": s.tonnes(),
        "impact_in": impact_in,
        "contact_lead": contact_lead,
        "fire_lights": fire_lights && burst.fires,
        "effects": planned_effects(&burst),
        "fireball_radius": s.fireball_radius(),
        "column_height": s.column_height(),
        "blast_reach": s.blast_reach(),
        "wall_reach": s.wall_reach(),
        "wall_rings": if burst.dust { s.wall_rings(tier).len() } else { 0 },
        "gust": burst.gust,
        "gust_sites": if burst.gust { gust_count } else { 0 },
        "camera_distance": camera,
        "camera_arrival_in": camera.map(|d| f64::from(impact_in) + blast_arrival_seconds(d)),
        "camera_wind": camera.map(|d| s.blast_wind(d).peak_wind),
        "fire_count": if burst.fires { s.fire_count() } else { 0 },
        "crater": {
            "diameter": crater.diameter,
            "depth": crater.depth,
            "rim_height": crater.rim_height,
            "ejecta_blanket_radius": crater.ejecta_blanket_radius,
            "scorch_radius": crater.scorch_radius,
        },
    }))
}

/// `vfx.crashes`: every scheduled crash and where its clock is.
pub fn crashes(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let camera = world.get_resource::<regia_vfx::VfxView>().map(|v| v.camera.eye);
    let wind_at_camera = camera.and_then(|eye| {
        world
            .get_resource::<BlastGusts>()
            .map(|g| g.wind_at(eye).to_array())
    });
    let mut query = world.query::<(Entity, &CrashSequence)>();
    let list: Vec<Value> = query
        .iter(world)
        .map(|(entity, seq)| {
            json!({
                "gust_puffs": seq.gust_puffs(),
                "gust_sites": seq.gust_sites.len(),
                "entity": entity.to_bits(),
                "yield_t": seq.crash.scale.tonnes(),
                "position": seq.crash.position.to_array(),
                "clock": seq.clock(),
                "impact_in": seq.impact_in,
                "contact_fired": seq.contact_fired(),
                "detonated": seq.detonated(),
                "front_radius": seq.front_radius(),
                "arrived": seq.arrived(),
                "instances": seq
                    .instances()
                    .iter()
                    .map(|(effect, id)| json!({ "effect": effect, "instance": id }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({ "count": list.len(), "crashes": list, "camera_wind": wind_at_camera }))
}

/// `vfx.blast_gust`: the blast wind at `position` (default: the `VfxView` camera) this frame,
/// from [`BlastGusts`], and for every scheduled crash when its front reaches the point, on the
/// one clock of ADR-1497 (`distance / 343`).
pub fn blast_gust(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let position = match optional_vec3(params, "position")? {
        Some(p) => p,
        None => world
            .get_resource::<regia_vfx::VfxView>()
            .map(|v| v.camera.eye)
            .ok_or_else(|| {
                RpcError::invalid_params("`position` ([x, y, z]) is required without a VFX view")
            })?,
    };
    let wind = world
        .get_resource::<BlastGusts>()
        .map_or(Vec3::ZERO, |g| g.wind_at(position));
    let mut query = world.query::<(Entity, &CrashSequence)>();
    let fronts: Vec<Value> = query
        .iter(world)
        .map(|(entity, seq)| {
            let crash = &seq.crash;
            let distance = crash.position.distance(position);
            let arrival = blast_arrival_seconds(distance);
            let air = crash.scale.blast_wind(distance);
            let arrival_in = match seq.since_detonation() {
                Some(elapsed) => arrival - elapsed,
                None => f64::from(seq.impact_in) - f64::from(seq.clock()) + arrival,
            };
            json!({
                "entity": entity.to_bits(),
                "yield_t": crash.scale.tonnes(),
                "origin": crash.position.to_array(),
                "distance": distance,
                "detonated": seq.detonated(),
                "front_radius": seq.front_radius(),
                "arrival_s": arrival,
                "arrival_in": arrival_in,
                "reached": seq.since_detonation().is_some_and(|e| blast_front_reached(e, distance)),
                "peak_wind": air.peak_wind,
                "overpressure_pa": air.overpressure_pa,
                "positive_phase": air.positive_phase,
            })
        })
        .collect();
    Ok(json!({
        "position": position.to_array(),
        "wind": wind.to_array(),
        "speed": wind.length(),
        "count": fronts.len(),
        "fronts": fronts,
    }))
}

fn emitter_json(e: &EmitterProbe) -> Value {
    json!({
        "name": e.name,
        "live": e.live,
        "visible": e.visible,
        "centroid": e.centroid.to_array(),
        "envelope_radius": e.envelope_radius,
        "front_p90": e.front_p90,
        "top": e.top,
        "height_p90": e.height_p90,
        "mean_speed": e.mean_speed,
        "mean_flat_speed": e.mean_flat_speed,
        "mean_rise": e.mean_rise,
        "mean_alpha": e.mean_alpha,
        "mean_luma": e.mean_luma,
        "max_size": e.max_size,
        "drift": e.drift.to_array(),
    })
}

fn measure_json(m: &CrashMeasure) -> Value {
    json!({
        "flash_end": m.flash_end,
        "fireball_peak_radius": m.fireball_peak_radius,
        "fireball_peak_time": m.fireball_peak_time,
        "fire_gone_at": m.fire_gone_at,
        "fireball_rise": m.fireball_rise,
        "soot_time": m.soot_time,
        "soot_luma": m.soot_luma,
        "soot_alpha": m.soot_alpha,
        "shell_radius": m.shell_radius,
        "shell_gone_at": m.shell_gone_at,
        "front_speed": m.front_speed,
        "front_error": m.front_error,
        "front_reach": m.front_reach,
        "lift_reach": m.lift_reach,
        "lift_leads_front": m.lift_leads_front,
        "wall_reach": m.wall_reach,
        "wall_lag": m.wall_lag,
        "wall_leads_front": m.wall_leads_front,
        "wall_height": m.wall_height,
        "surge_time": m.surge_time,
        "surge_front": m.surge_front,
        "surge_height": m.surge_height,
        "ejecta_top": m.ejecta_top,
        "ejecta_landing": m.ejecta_landing,
        "column_top_60s": m.column_top_60s,
        "column_width_60s": m.column_width_60s,
        "column_rise_30s": m.column_rise_30s,
        "column_drift_60s": m.column_drift_60s,
        "column_top_180s": m.column_top_180s,
        "contact_reach": m.contact_reach,
        "contact_gone_at": m.contact_gone_at,
    })
}

/// The full effect name for `name`: a scaled base name (`crash_fireball`, `crash_dust_snow`, …)
/// gets the `@<t>t` tag of `yield_t`; a tagged or scale-free name stays.
fn profile_name(name: &str, params: &Value) -> Result<String, RpcError> {
    let scaled = ["crash_fireball", "crash_shell", "crash_column"].contains(&name)
        || GroundClass::ALL.iter().any(|g| {
            name == format!("crash_dust_{}", g.name()) || name == format!("crash_ejecta_{}", g.name())
        });
    if scaled && !name.contains('@') {
        Ok(format!("{name}@{}", scale_param(params)?.tag()))
    } else {
        Ok(name.to_string())
    }
}

/// `vfx.crash_profile`: measures a crash effect with the CPU probe (no GPU, no scene needed):
/// the acceptance summary and, at each of `times`, every emitter's envelope.
pub fn crash_profile(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let base = params
        .get("effect")
        .and_then(Value::as_str)
        .ok_or_else(|| RpcError::invalid_params("`effect` (a crash effect name) is required"))?;
    let name = profile_name(base, params)?;
    let tier = tier_param(world, params)?;
    let mut medium = world
        .get_resource::<VfxScene>()
        .map_or_else(VfxMedium::earth, VfxScene::medium);
    if let Some(wind) = optional_vec3(params, "wind")? {
        medium.wind = wind;
    }
    let def = crash_effect(&name, tier, &medium).ok_or_else(|| {
        RpcError::invalid_params(format!(
            "`effect` {base}: crash_fireball, crash_shell, crash_column, crash_dust_<ground>, crash_ejecta_<ground>, crash_contact_<ground>, crash_ground_fire_<size> or crash_gust_snow"
        ))
    })?;
    let times: Vec<f32> = match params.get("times") {
        None | Some(Value::Null) => vec![0.1, 0.5, 1.0, 2.0, 4.0, 8.0, 15.0, 30.0, 60.0],
        Some(v) => {
            let items = v
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= MAX_PROBE_TIMES)
                .ok_or_else(|| {
                    RpcError::invalid_params(format!("`times` must hold 1..={MAX_PROBE_TIMES} seconds"))
                })?;
            items
                .iter()
                .map(|t| {
                    t.as_f64()
                        .filter(|t| t.is_finite() && (0.0..=MAX_PROBE_SECONDS).contains(t))
                        .map(|t| t as f32)
                        .ok_or_else(|| {
                            RpcError::invalid_params(format!(
                                "`times` entries must be 0..={MAX_PROBE_SECONDS} s"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    let snapshots = probe_effect(&def, Vec3::ZERO, Vec3::Y, ProbeSettings::default(), &times);
    let snapshots: Vec<Value> = snapshots
        .iter()
        .map(|s| {
            json!({
                "time": s.time,
                "emitters": s.emitters.iter().map(emitter_json).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "effect": name,
        "tier": tier.name(),
        "summary": measure_json(&measure_crash_effect(&def)),
        "snapshots": snapshots,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(regia_vfx::VfxPlugin);
        app.update();
        app
    }

    #[test]
    fn crash_schedules_a_sequence_and_validates() {
        let mut app = app();
        let out = crash(
            app.world_mut(),
            &json!({ "position": [700.0, 105.7, 5.0], "ground": "snow", "yield_t": 250.0,
                     "wind": [3.0, 0.0, 0.0], "tier": "lite", "impact_in": 0.0, "contact": false }),
        )
        .expect("crash");
        assert_eq!(out["tier"], "lite");
        assert_eq!(out["yield_t"], 250.0);
        let effects: Vec<&str> = out["effects"]
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        assert_eq!(
            &effects[..4],
            [
                "crash_fireball@250t",
                "crash_dust_snow@250t",
                "crash_ejecta_snow@250t",
                "crash_column@250t"
            ]
        );
        let d = out["crater"]["diameter"].as_f64().unwrap_or_default();
        assert!((40.0..=70.0).contains(&d), "{d}");
        app.update();
        let list = crashes(app.world_mut(), &json!({})).expect("list");
        assert_eq!(list["count"], 1);
        assert_eq!(list["crashes"][0]["detonated"], true);
        assert_eq!(list["crashes"][0]["contact_fired"], true);
        assert!(
            list["crashes"][0]["instances"]
                .as_array()
                .is_some_and(|a| a.len() > 4)
        );
        let small =
            crash(app.world_mut(), &json!({ "position": [0, 0, 0], "fires": false })).expect("vehicle");
        assert_eq!(small["yield_t"], 1.0);
        assert_eq!(small["fire_count"], 0);
        assert_eq!(small["effects"][0], "crash_contact_mud");
        for bad in [
            json!({}),
            json!({ "position": [0, 0] }),
            json!({ "position": [0, 0, 0], "ground": "lava" }),
            json!({ "position": [0, 0, 0], "tier": "ultra" }),
            json!({ "position": [0, 0, 0], "normal": [0, 0, 0] }),
            json!({ "position": [0, 0, 0], "yield_t": 5000.0 }),
            json!({ "position": [0, 0, 0], "impact_in": -1.0 }),
            json!({ "position": [0, 0, 0], "contact_lead": 2.0 }),
            json!({ "position": [0, 0, 0], "shell": "yes" }),
        ] {
            let err = crash(app.world_mut(), &bad).expect_err("invalid");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
        let err = crash(App::new().world_mut(), &json!({ "position": [0, 0, 0] })).expect_err("no scene");
        assert_eq!(err.code, protocol::UNSUPPORTED);
    }

    #[test]
    fn profile_reports_the_acceptance_summary() {
        let mut world = World::new();
        let out = crash_profile(
            &mut world,
            &json!({ "effect": "crash_fireball", "yield_t": 250.0, "times": [1.2] }),
        )
        .expect("profile");
        assert_eq!(out["effect"], "crash_fireball@250t");
        let r = out["summary"]["fireball_peak_radius"]
            .as_f64()
            .unwrap_or_default();
        assert!((75.0..=125.0).contains(&r), "{r}");
        assert_eq!(out["snapshots"].as_array().map(Vec::len), Some(1));
        let tagged = crash_profile(
            &mut world,
            &json!({ "effect": "crash_dust_mud@250t", "times": [2.0] }),
        )
        .expect("tagged");
        assert_eq!(tagged["effect"], "crash_dust_mud@250t");
        let fire = crash_profile(
            &mut world,
            &json!({ "effect": "crash_ground_fire_large", "times": [5.0] }),
        )
        .expect("fire");
        assert!(
            fire["snapshots"][0]["emitters"][0]["visible"]
                .as_u64()
                .unwrap_or_default()
                > 0
        );
        for bad in [
            json!({}),
            json!({ "effect": "explosion" }),
            json!({ "effect": "crash_fireball@250.0t" }),
            json!({ "effect": "crash_fireball", "times": [] }),
            json!({ "effect": "crash_fireball", "times": [999.0] }),
            json!({ "effect": "crash_fireball", "yield_t": 0.0 }),
        ] {
            let err = crash_profile(&mut world, &bad).expect_err("invalid");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
    }

    /// ADR-1497: `gust_sites` is validated, the result reports the wall and the camera's
    /// arrival, and `vfx.blast_gust` reports the wind and the front's arrival at a point.
    #[test]
    fn the_blast_front_surface_reports_and_validates() {
        let mut app = app();
        let eye = Vec3::new(-1752.0, 353.2, -2203.0);
        app.insert_resource(regia_vfx::VfxView {
            camera: regia_vfx::VfxCamera::looking_at(eye, Vec3::new(700.0, 105.7, 5.0)),
            ..regia_vfx::VfxView::default()
        });
        let out = crash(
            app.world_mut(),
            &json!({ "position": [700.0, 105.7, 5.0], "ground": "snow", "yield_t": 250.0,
                     "impact_in": 0.0, "fires": false, "tier": "lite",
                     "gust_sites": [[-1700.0, 350.0, -2150.0], [-1780.0, 356.0, -2240.0]] }),
        )
        .expect("crash");
        assert_eq!(out["gust_sites"], 2);
        assert_eq!(out["wall_rings"], 9);
        let reach = out["wall_reach"].as_f64().unwrap_or_default();
        assert!((300.0..=500.0).contains(&reach), "{reach}");
        let arrive = out["camera_arrival_in"].as_f64().unwrap_or_default();
        assert!((9.6..9.7).contains(&arrive), "{arrive}");
        let gust = blast_gust(app.world_mut(), &json!({})).expect("gust at the camera");
        assert_eq!(gust["count"], 1);
        assert_eq!(gust["fronts"][0]["reached"], false);
        let wind = gust["fronts"][0]["peak_wind"].as_f64().unwrap_or_default();
        assert!((3.0..=15.0).contains(&wind), "{wind}");
        let at = blast_gust(app.world_mut(), &json!({ "position": [700.0, 105.7, 105.0] })).expect("near");
        assert!((at["fronts"][0]["distance"].as_f64().unwrap_or_default() - 100.0).abs() < 1e-3);
        for bad in [
            json!({ "position": [0, 0, 0], "gust_sites": [[0, 0]] }),
            json!({ "position": [0, 0, 0], "gust_sites": "trees" }),
            json!({ "position": [0, 0, 0], "gust": "yes" }),
        ] {
            let err = crash(app.world_mut(), &bad).expect_err("invalid");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
        let err = blast_gust(&mut World::new(), &json!({})).expect_err("no view");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
    }

    /// ADR-1497, one clock: the framework's shock shake (what the showcase schedules with
    /// `camera.schedule_shock_shake`) lands on the step the crash sequence reports the front's
    /// arrival, at shot 4's camera and at other distances.
    #[test]
    fn the_scheduled_shock_shake_and_the_front_share_one_clock() {
        use regia_framework::shock_shake::ShockShake;
        use regia_vfx::crash::{CrashEventKind, advance_crash_sequence, blast_arrival_step};
        assert_eq!(
            regia_framework::ground_shock::AIR_SPEED_M_S,
            regia_vfx::crash::SHOCK_FRONT_SPEED
        );
        let site = Vec3::new(700.0, 105.7, 5.0);
        for listener in [
            Vec3::new(-1752.0, 353.2, -2203.0),
            site + Vec3::X * 343.0,
            site + Vec3::new(1_000.0, 50.0, -400.0),
        ] {
            let d = site.distance(listener);
            let mut scene = VfxScene::new(regia_vfx::scene::VfxLibrary::new());
            let mut seq = CrashSequence::new(
                CrashBurst {
                    position: site,
                    scale: CrashScale::SHOWCASE,
                    fires: false,
                    column: false,
                    ..CrashBurst::default()
                },
                0.25,
            );
            let mut detonation = None;
            let mut arrival = None;
            for frame in 0..1_200u64 {
                for kind in
                    advance_crash_sequence(&mut scene, &mut seq, 1.0 / 60.0, CrashTier::Lite, Some(listener))
                {
                    match kind {
                        CrashEventKind::Detonation => detonation = Some(frame),
                        CrashEventKind::BlastArrived { .. } => arrival = Some(frame),
                        _ => {}
                    }
                }
            }
            let detonation = detonation.expect("detonation");
            let shake = ShockShake::default().arrival_frame(d, 60.0, detonation);
            assert_eq!(arrival, Some(shake), "{d} m");
            assert_eq!(arrival, Some(detonation + blast_arrival_step(d, 60.0)), "{d} m");
        }
    }
}
