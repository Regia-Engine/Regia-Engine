// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the showcase's crash fire lights and shock-arrival camera shake:
//! `vfx.fire_lights`, `vfx.spawn_crash_fires`, `camera.shock_shake` and
//! `camera.schedule_shock_shake`.
//!
//! Handlers are plain functions over `&mut World` (tests drive them without a server). A world
//! without the resource a method needs answers `UNSUPPORTED` naming the plugin to add.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::ground_shock::{GroundShock, ShakeShape, WaveKind};
use regia_framework::shock_shake::{ShockShake, ShockShakeSchedule};
use regia_vfx::lights::VfxFrameLights;
use regia_vfx::{CrashFires, FireLights, VfxSettings};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most fires `vfx.spawn_crash_fires` adds in one call.
const MAX_CRASH_FIRES: u64 = 200;
/// Most fires `vfx.fire_lights` lists.
const MAX_LISTED_FIRES: usize = 200;

fn unsupported(what: &str, plugin: &str) -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "F0016",
        format!("no {what} in this app"),
        format!("add {plugin} so this method has something to read"),
    )
}

fn number(params: &Value, field: &str) -> Result<Option<f64>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?;
            if !(n.is_finite() && (n as f32).is_finite()) {
                return Err(RpcError::invalid_params(format!("`{field}` must be finite")));
            }
            Ok(Some(n))
        }
    }
}

fn whole(params: &Value, field: &str) -> Result<Option<u64>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a non-negative integer"))),
    }
}

fn position(params: &Value, field: &str) -> Result<Vec3, RpcError> {
    let invalid = || RpcError::invalid_params(format!("`{field}` must be [x, y, z], three finite numbers"));
    let items = crate::params::opt_array(params, field)?
        .filter(|a| a.len() == 3)
        .ok_or_else(invalid)?;
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let n = item.as_f64().ok_or_else(invalid)? as f32;
        if !n.is_finite() {
            return Err(invalid());
        }
        *slot = n;
    }
    Ok(Vec3::from_array(out))
}

fn rgb(c: [f32; 3]) -> Value {
    json!([c[0], c[1], c[2]])
}

/// `vfx.fire_lights`: the fire clock, every live fire with its light at the current frame, and
/// how many particle lights the frame published under the budget.
pub fn fire_lights(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let Some(fires) = world.get_resource::<FireLights>() else {
        return Err(unsupported("fire lights", "regia_vfx::VfxPlugin"));
    };
    let frame = fires.frame();
    let listed: Vec<Value> = fires
        .fires()
        .iter()
        .take(MAX_LISTED_FIRES)
        .enumerate()
        .map(|(index, fire)| {
            let age = frame.saturating_sub(fire.start_frame);
            let started = frame >= fire.start_frame;
            let sample = fire.light.sample(age);
            json!({
                "index": index,
                "position": [fire.position.x, fire.position.y, fire.position.z],
                "radius": fire.radius,
                "seed": fire.light.seed,
                "start_frame": fire.start_frame,
                "lifetime_frames": fire.light.lifetime_frames,
                "remaining_frames": fire.start_frame.saturating_add(fire.light.lifetime_frames).saturating_sub(frame),
                "lit": started && sample.intensity > 0.0,
                "intensity": if started { sample.intensity } else { 0.0 },
                "color": rgb(sample.color),
            })
        })
        .collect();
    let total = fires.fires().len();
    let budget = world
        .get_resource::<VfxSettings>()
        .map(|s| s.profile.light_budget);
    let published = world.get_resource::<VfxFrameLights>().map(|l| l.lights.len());
    Ok(json!({
        "frame": frame,
        "count": total,
        "listed": listed.len(),
        "truncated": total > listed.len(),
        "light_budget": budget,
        "published_lights": published,
        "fires": listed,
    }))
}

/// `vfx.spawn_crash_fires { position, count?, scatter_radius?, seed?, start_frame?, replace? }`:
/// the burning wreck at `position` and `count` scattered fires around it.
pub fn spawn_crash_fires(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<FireLights>().is_none() {
        return Err(unsupported("fire lights", "regia_vfx::VfxPlugin"));
    }
    let center = position(params, "position")?;
    let count = whole(params, "count")?.unwrap_or(40);
    if count > MAX_CRASH_FIRES {
        return Err(RpcError::invalid_params(format!(
            "`count` must be at most {MAX_CRASH_FIRES}"
        )));
    }
    let scatter = number(params, "scatter_radius")?.unwrap_or(300.0);
    if !(0.0..=5000.0).contains(&scatter) {
        return Err(RpcError::invalid_params("`scatter_radius` must be in 0..=5000"));
    }
    let seed = whole(params, "seed")?.unwrap_or(0);
    let replace = match params.get("replace") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`replace` must be a boolean"))?,
    };
    let mut fires = world.resource_mut::<FireLights>();
    let start_frame = whole(params, "start_frame")?.unwrap_or(fires.frame());
    if replace {
        fires.clear();
    }
    let added = fires.spawn_crash(&CrashFires {
        center,
        count: count as u32,
        scatter_radius_m: scatter as f32,
        seed,
        start_frame,
    });
    Ok(json!({ "added": added, "total": fires.fires().len(), "start_frame": start_frame, "seed": seed }))
}

/// The showcase blast law with any of the optional overrides applied.
fn blast_from(params: &Value) -> Result<ShockShake, RpcError> {
    let mut blast = ShockShake::showcase();
    let positive = |name: &str, current: f32, allow_zero: bool| -> Result<f32, RpcError> {
        match number(params, name)? {
            None => Ok(current),
            Some(n) if n > 0.0 || (allow_zero && n == 0.0) => Ok(n as f32),
            Some(_) => Err(RpcError::invalid_params(format!("`{name}` must be positive"))),
        }
    };
    blast.energy = positive("energy", blast.energy, true)?;
    blast.speed_m_s = positive("speed_m_s", blast.speed_m_s, false)?;
    blast.reference_distance_m = positive("reference_distance_m", blast.reference_distance_m, false)?;
    blast.max_amplitude = positive("max_amplitude", blast.max_amplitude, true)?;
    blast.duration_s = positive("duration_s", blast.duration_s, false)?;
    Ok(blast)
}

/// The ground-shock law: the `shake_profile` ("calibrated", the default, or "legacy" for the
/// ADR-1457 strengths), then `yield_kt` (default: the pod impact's 0.35 kt), `mount_q` and
/// `ceiling_deg` applied over it.
fn ground_from(params: &Value) -> Result<GroundShock, RpcError> {
    let mut ground = match params.get("shake_profile") {
        None | Some(Value::Null) => GroundShock::default(),
        Some(Value::String(name)) if name == "calibrated" => GroundShock::default(),
        Some(Value::String(name)) if name == "legacy" => GroundShock::legacy(),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`shake_profile` must be \"calibrated\" or \"legacy\"",
            ));
        }
    };
    if let Some(n) = number(params, "yield_kt")? {
        if n <= 0.0 {
            return Err(RpcError::invalid_params("`yield_kt` must be positive"));
        }
        ground.yield_kt = n as f32;
    }
    if let Some(n) = number(params, "mount_q")? {
        if n <= 0.0 {
            return Err(RpcError::invalid_params("`mount_q` must be positive"));
        }
        ground.mount_q = n as f32;
    }
    if let Some(n) = number(params, "ceiling_deg")? {
        if !(n > 0.0 && n <= 20.0) {
            return Err(RpcError::invalid_params("`ceiling_deg` must be in (0, 20]"));
        }
        ground.visibility.ceiling_rad = n.to_radians() as f32;
    }
    Ok(ground)
}

/// Whether the ground shock is wanted: `ground` defaults to true.
fn ground_wanted(params: &Value) -> Result<bool, RpcError> {
    match params.get("ground") {
        None | Some(Value::Null) => Ok(true),
        Some(value) => value
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`ground` must be true or false")),
    }
}

fn ground_json(ground: &GroundShock, distance: f32, fixed_hz: f32, blast_frame: u64) -> Value {
    let p = ground.predict(distance);
    let frame = |delay: f64| {
        let steps = delay * f64::from(fixed_hz);
        blast_frame.saturating_add((steps - 1.0e-9).ceil().max(0.0) as u64)
    };
    json!({
        "yield_kt": ground.yield_kt,
        "mount_q": ground.mount_q,
        "air_shape": match ground.air_shape {
            ShakeShape::Oscillation => "oscillation",
            ShakeShape::Impulse => "impulse",
        },
        "ppv_mm_s": p.ppv_mm_s,
        "ground_displacement_mm": p.ground_displacement_mm,
        "overpressure_pa": p.overpressure_pa,
        "seismic_delay_s": p.seismic_delay_s,
        "seismic_arrival_frame": frame(ground.seismic_delay_f64(distance)),
        "seismic_angle_deg": p.seismic_angle_rad.to_degrees(),
        "air_delay_s": p.air_delay_s,
        "air_arrival_frame": frame(ground.air_delay_f64(distance)),
        "air_angle_deg": p.air_angle_rad.to_degrees(),
    })
}

fn blast_json(blast: &ShockShake) -> Value {
    json!({
        "energy": blast.energy,
        "speed_m_s": blast.speed_m_s,
        "reference_distance_m": blast.reference_distance_m,
        "max_amplitude": blast.max_amplitude,
        "duration_s": blast.duration_s,
    })
}

/// `camera.shock_shake { distance_m?, blast_frame?, ...overrides }`: the shock schedule (step,
/// rate, pending, fired) and, with `distance_m`, what a blast at that distance would do.
pub fn shock_shake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(schedule) = world.get_resource::<ShockShakeSchedule>() else {
        return Err(unsupported(
            "shock shake schedule",
            "regia_framework::shock_shake::ShockShakePlugin",
        ));
    };
    let blast = blast_from(params)?;
    let ground = ground_from(params)?;
    let prediction = match number(params, "distance_m")? {
        None => Value::Null,
        Some(d) => {
            let d = d as f32;
            let blast_frame = whole(params, "blast_frame")?.unwrap_or(schedule.frame());
            let shake = blast.to_shake(d);
            json!({
                "distance_m": d,
                "delay_s": blast.arrival_delay_s(d),
                "arrival_frame": blast.arrival_frame(d, schedule.fixed_hz(), blast_frame),
                "amplitude": blast.amplitude(d),
                "ground": ground_json(&ground, d, schedule.fixed_hz(), blast_frame),
                "shake": shake.map(|(_, s)| json!({
                    "amplitude": s.amplitude,
                    "frequency_hz": s.frequency_hz,
                    "duration_secs": s.duration_secs,
                    "falloff": s.falloff,
                })),
            })
        }
    };
    let pending: Vec<Value> = schedule
        .pending()
        .iter()
        .map(|p| {
            json!({
                "blast_frame": p.blast_frame,
                "distance_m": p.distance_m,
                "arrival_frame": p.arrival_frame,
                "frames_left": p.arrival_frame.saturating_sub(schedule.frame()),
                "amplitude": p.shake.amplitude,
            })
        })
        .collect();
    let ground_pending: Vec<Value> = schedule
        .ground_pending()
        .iter()
        .map(|w| {
            json!({
                "wave": match w.kind {
                    WaveKind::Seismic => "seismic",
                    WaveKind::Airblast => "airblast",
                },
                "blast_frame": w.blast_frame,
                "distance_m": w.distance_m,
                "arrival_frame": w.arrival_frame,
                "frames_left": w.arrival_frame.saturating_sub(schedule.frame()),
                "angle_deg": w.shock.angle_rad.to_degrees(),
                "seed": w.shock.seed,
            })
        })
        .collect();
    Ok(json!({
        "frame": schedule.frame(),
        "fixed_hz": schedule.fixed_hz(),
        "fired": schedule.fired(),
        "ground_fired": schedule.ground_fired(),
        "pending": pending,
        "ground_pending": ground_pending,
        "blast": blast_json(&blast),
        "prediction": prediction,
    }))
}

/// `camera.schedule_shock_shake { distance_m, blast_frame?, ...overrides }`: schedules the shake
/// of a blast `distance_m` away. `scheduled: false` when the shock is negligible.
pub fn schedule_shock_shake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<ShockShakeSchedule>().is_none() {
        return Err(unsupported(
            "shock shake schedule",
            "regia_framework::shock_shake::ShockShakePlugin",
        ));
    }
    let distance = number(params, "distance_m")?
        .ok_or_else(|| RpcError::invalid_params("`distance_m` is required"))? as f32;
    if distance < 0.0 {
        return Err(RpcError::invalid_params("`distance_m` must not be negative"));
    }
    let blast = blast_from(params)?;
    let ground = ground_from(params)?;
    let wanted = ground_wanted(params)?;
    let mut schedule = world.resource_mut::<ShockShakeSchedule>();
    let blast_frame = whole(params, "blast_frame")?.unwrap_or(schedule.frame());
    let arrival = schedule.schedule(&blast, distance, blast_frame);
    let (seismic, air) = if wanted {
        schedule.schedule_ground(&ground, distance, blast_frame)
    } else {
        (None, None)
    };
    let ground_report = if wanted {
        ground_json(&ground, distance, schedule.fixed_hz(), blast_frame)
    } else {
        Value::Null
    };
    Ok(json!({
        "ground_scheduled": seismic.is_some() || air.is_some(),
        "seismic_arrival_frame": seismic,
        "air_arrival_frame": air,
        "ground": ground_report,
        "scheduled": arrival.is_some(),
        "blast_frame": blast_frame,
        "arrival_frame": arrival,
        "delay_s": blast.arrival_delay_s(distance),
        "amplitude": blast.amplitude(distance),
        "pending": schedule.pending().len(),
        "ground_pending": schedule.ground_pending().len(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.insert_resource(FireLights::default());
        app.insert_resource(ShockShakeSchedule::default());
        app
    }

    #[test]
    fn missing_resources_are_unsupported() {
        let mut empty = App::new();
        for err in [
            fire_lights(empty.world_mut(), &json!({})).expect_err("fires"),
            spawn_crash_fires(empty.world_mut(), &json!({ "position": [0, 0, 0] })).expect_err("spawn"),
            shock_shake(empty.world_mut(), &json!({})).expect_err("shake"),
            schedule_shock_shake(empty.world_mut(), &json!({ "distance_m": 1 })).expect_err("schedule"),
        ] {
            assert_eq!(err.code, protocol::UNSUPPORTED);
        }
    }

    #[test]
    fn crash_fires_spawn_and_list() {
        let mut app = app();
        let out = spawn_crash_fires(
            app.world_mut(),
            &json!({ "position": [10, 2, -5], "count": 12, "scatter_radius": 80, "seed": 9 }),
        )
        .expect("spawn");
        assert_eq!(out["added"], 13);
        assert_eq!(out["total"], 13);
        let listed = fire_lights(app.world_mut(), &json!({})).expect("list");
        assert_eq!(listed["count"], 13);
        assert_eq!(listed["fires"][0]["lit"], true, "the wreck is lit at once");
        assert_eq!(listed["truncated"], false);
        let replaced = spawn_crash_fires(
            app.world_mut(),
            &json!({ "position": [0, 0, 0], "count": 0, "replace": true }),
        )
        .expect("replace");
        assert_eq!(replaced["total"], 1);
    }

    #[test]
    fn crash_fire_params_are_validated() {
        let mut app = app();
        for bad in [
            json!({}),
            json!({ "position": [0, 0] }),
            json!({ "position": [0, 0, 0], "count": 1000 }),
            json!({ "position": [0, 0, 0], "count": -1 }),
            json!({ "position": [0, 0, 0], "scatter_radius": -1 }),
            json!({ "position": [0, 0, 0], "replace": "yes" }),
        ] {
            let err = spawn_crash_fires(app.world_mut(), &bad).expect_err("invalid");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
    }

    #[test]
    fn shock_shake_predicts_and_schedules() {
        let mut app = app();
        let predicted = shock_shake(app.world_mut(), &json!({ "distance_m": 343 })).expect("predict");
        assert_eq!(predicted["prediction"]["delay_s"], 1.0);
        assert_eq!(predicted["prediction"]["arrival_frame"], 60);
        let doubled =
            shock_shake(app.world_mut(), &json!({ "distance_m": 686, "speed_m_s": 686 })).expect("speed");
        assert_eq!(doubled["prediction"]["delay_s"], 1.0);
        let out = schedule_shock_shake(app.world_mut(), &json!({ "distance_m": 3430, "blast_frame": 5 }))
            .expect("schedule");
        assert_eq!(out["scheduled"], true);
        assert_eq!(out["arrival_frame"], 605);
        let state = shock_shake(app.world_mut(), &json!({})).expect("state");
        assert_eq!(state["pending"][0]["arrival_frame"], 605);
        assert_eq!(state["prediction"], Value::Null);
        let none = schedule_shock_shake(app.world_mut(), &json!({ "distance_m": 1.0e9 })).expect("far");
        assert_eq!(none["scheduled"], false);
    }

    #[test]
    fn shock_shake_params_are_validated() {
        let mut app = app();
        for bad in [
            json!({}),
            json!({ "distance_m": -1 }),
            json!({ "distance_m": "far" }),
            json!({ "distance_m": 10, "speed_m_s": 0 }),
            json!({ "distance_m": 10, "duration_s": -2 }),
            json!({ "distance_m": 10, "yield_kt": 0 }),
            json!({ "distance_m": 10, "ground": "yes" }),
        ] {
            let err = schedule_shock_shake(app.world_mut(), &bad).expect_err("invalid");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
    }

    #[test]
    fn the_shake_profile_picks_calibrated_or_legacy_strengths() {
        let mut app = app();
        let calibrated = shock_shake(app.world_mut(), &json!({ "distance_m": 3300 })).expect("calibrated");
        let legacy = shock_shake(
            app.world_mut(),
            &json!({ "distance_m": 3300, "shake_profile": "legacy" }),
        )
        .expect("legacy");
        let c = &calibrated["prediction"]["ground"];
        let l = &legacy["prediction"]["ground"];
        assert_eq!(c["air_shape"], "impulse");
        assert_eq!(l["air_shape"], "oscillation");
        // `mount_q` is an f32. 0.8 is not exact in that format, so compare with a tolerance.
        let mount = c["mount_q"].as_f64().expect("mount_q");
        assert!((mount - 0.8).abs() < 1.0e-5, "{mount}");
        assert_eq!(l["mount_q"].as_f64().expect("legacy mount_q"), 10.0);
        let ratio = l["seismic_angle_deg"].as_f64().expect("l") / c["seismic_angle_deg"].as_f64().expect("c");
        assert!((ratio - 10.0 / 0.8).abs() < 0.01, "{ratio}");
        let tuned = shock_shake(app.world_mut(), &json!({ "distance_m": 3300, "mount_q": 10 })).expect("q");
        assert_eq!(
            tuned["prediction"]["ground"]["seismic_angle_deg"],
            l["seismic_angle_deg"]
        );
        let capped =
            shock_shake(app.world_mut(), &json!({ "distance_m": 50, "ceiling_deg": 1 })).expect("cap");
        let air = capped["prediction"]["ground"]["air_angle_deg"]
            .as_f64()
            .expect("air");
        assert!((air - 1.0).abs() < 1.0e-4, "{air}");
        for bad in [
            json!({ "distance_m": 10, "shake_profile": "wild" }),
            json!({ "distance_m": 10, "shake_profile": 3 }),
            json!({ "distance_m": 10, "mount_q": 0 }),
            json!({ "distance_m": 10, "ceiling_deg": 0 }),
            json!({ "distance_m": 10, "ceiling_deg": 45 }),
        ] {
            let err = schedule_shock_shake(app.world_mut(), &bad).expect_err("invalid");
            assert_eq!(err.code, protocol::INVALID_PARAMS, "{bad}");
        }
    }

    #[test]
    fn the_ground_shock_is_scheduled_with_the_blast_and_can_be_left_out() {
        let mut app = app();
        let out = schedule_shock_shake(app.world_mut(), &json!({ "distance_m": 3300, "blast_frame": 10 }))
            .expect("schedule");
        assert_eq!(out["ground_scheduled"], true);
        assert_eq!(out["seismic_arrival_frame"], 46, "10 + 0.6 s");
        assert_eq!(out["air_arrival_frame"], 588, "10 + 9.62 s");
        assert_eq!(out["ground_pending"], 2);
        let seismic = out["ground"]["seismic_angle_deg"].as_f64().expect("deg");
        assert!((0.005..0.03).contains(&seismic), "calibrated: {seismic}");
        let legacy = schedule_shock_shake(
            app.world_mut(),
            &json!({ "distance_m": 3300, "blast_frame": 10, "shake_profile": "legacy" }),
        )
        .expect("legacy");
        let old = legacy["ground"]["seismic_angle_deg"].as_f64().expect("deg");
        assert!((0.05..0.2).contains(&old), "legacy: {old}");
        let tail = shock_shake(app.world_mut(), &json!({})).expect("tail");
        assert_eq!(tail["ground_pending"].as_array().map(Vec::len), Some(4));
        app.world_mut().resource_mut::<ShockShakeSchedule>().clear();
        let out = schedule_shock_shake(app.world_mut(), &json!({ "distance_m": 3300, "blast_frame": 10 }))
            .expect("again");
        assert_eq!(out["ground_pending"], 2);
        let state = shock_shake(app.world_mut(), &json!({})).expect("state");
        assert_eq!(state["ground_pending"][0]["wave"], "seismic");
        assert_eq!(state["ground_pending"][1]["wave"], "airblast");
        let predicted = shock_shake(
            app.world_mut(),
            &json!({ "distance_m": 300, "yield_kt": 0.5, "shake_profile": "legacy" }),
        )
        .expect("predict");
        assert!(
            predicted["prediction"]["ground"]["seismic_angle_deg"]
                .as_f64()
                .expect("deg")
                > 1.0
        );
        let off = schedule_shock_shake(app.world_mut(), &json!({ "distance_m": 3300, "ground": false }))
            .expect("off");
        assert_eq!(off["ground_scheduled"], false);
        assert_eq!(off["ground"], Value::Null);
        assert_eq!(off["ground_pending"], 2, "nothing added");
        assert_eq!(off["scheduled"], true, "the airblast translation still is");
    }
}
