// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the lockstep projectiles and the aim solver (ADR-8509,
//! `book/agents/det-projectiles.md`).
//!
//! - `detsim.projectile_simulate` flies one projectile of the given kind on the given world from a
//!   launch, against an optional reference geometry (ground, planes, boxes, body spheres), for a
//!   number of ticks, and answers the sampled path and the impact. Stateless: nothing is stored.
//! - `detsim.aim_solve` answers the drag-free launch angles for a target and, with `drag: true`, the
//!   pitch that hits it with the projectile's drag (a fixed number of secant steps).
//!
//! Both run `regia_detsim` code, the same functions a lockstep game calls, so two calls with the
//! same parameters answer the same numbers on every host. Error codes: `bad_params` (a field is
//! missing, has the wrong shape, or a projectile or world number is invalid; `data.field` names it)
//! and `steps_limit` (too many ticks, samples, iterations or probe ticks).

use regia_detsim::aim::{self, AimArc, DragAim};
use regia_detsim::ballistic::{LowG, map_g};
use regia_detsim::projectile::{
    self, HitBox, HitPlane, HitSphere, HitTarget, Impact, LinearScanHits, ProjectileParams, ProjectileState,
};
use regia_detsim::site_physics::WorldPhysics;
use regia_detsim::tick::TickRate;
use serde_json::{Map, Value, json};

use crate::protocol::{INVALID_PARAMS, RpcError};

/// Most ticks one `detsim.projectile_simulate` call flies (10 minutes at 60 Hz).
pub const MAX_STEPS: u64 = 36_000;
/// Most path samples one call answers.
pub const MAX_SAMPLES: u64 = 4_096;
/// Most secant iterations of `detsim.aim_solve`.
pub const MAX_ITERATIONS: u64 = 32;
/// Most shapes of one kind in a call's geometry.
pub const MAX_SHAPES: usize = 256;

fn bad(field: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(
        INVALID_PARAMS,
        "bad_params",
        message,
        "see book/agents/det-projectiles.md for the fields and their valid ranges",
    );
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("field".into(), Value::String(field.into()));
    }
    error
}

fn steps_limit(message: impl Into<String>) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        "steps_limit",
        message,
        "fly fewer ticks (at most 36000), sample less often (at most 4096 samples), or ask for fewer \
         iterations (at most 32); see book/agents/det-projectiles.md",
    )
}

fn object<'a>(params: &'a Value, key: &str) -> Result<Option<&'a Map<String, Value>>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err(bad(key, format!("`{key}` must be an object"))),
    }
}

fn number(map: &Map<String, Value>, field: &str, key: &str, default: f32) -> Result<f32, RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| bad(field, format!("`{field}` must be a number")))?;
            let n = n as f32;
            if n.is_finite() {
                Ok(n)
            } else {
                Err(bad(field, format!("`{field}` must be a finite number")))
            }
        }
    }
}

fn vec3(value: Option<&Value>, field: &str) -> Result<Option<[f32; 3]>, RpcError> {
    let Some(value) = value.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let items = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| bad(field, format!("`{field}` must be [x, y, z]")))?;
    let mut out = [0.0_f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let n = item.as_f64().map(|n| n as f32).filter(|n| n.is_finite());
        *slot = n.ok_or_else(|| bad(field, format!("`{field}` must hold three finite numbers")))?;
    }
    Ok(Some(out))
}

fn count(params: &Value, key: &str, default: u64) -> Result<u64, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value
            .as_u64()
            .ok_or_else(|| bad(key, format!("`{key}` must be a non-negative integer"))),
    }
}

/// The projectile kind from `params.params` (missing fields: [`ProjectileParams::DEFAULT`]),
/// validated.
fn projectile_params(params: &Value) -> Result<ProjectileParams, RpcError> {
    let Some(map) = object(params, "params")? else {
        return Ok(ProjectileParams::DEFAULT);
    };
    let d = ProjectileParams::DEFAULT;
    let field = |key: &str, default: f32| number(map, key, key, default);
    let fuse_s = match map.get("fuse_s") {
        None | Some(Value::Null) => None,
        Some(_) => Some(number(map, "fuse_s", "fuse_s", 0.0)?),
    };
    let max_bounces = match map.get("max_bounces") {
        None | Some(Value::Null) => d.max_bounces,
        Some(value) => value
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .ok_or_else(|| bad("max_bounces", "`max_bounces` must be an integer 0..=65535"))?,
    };
    let p = ProjectileParams {
        gravity_scale: field("gravity_scale", d.gravity_scale)?,
        drag_per_m: field("drag_per_m", d.drag_per_m)?,
        linear_drag_per_s: field("linear_drag_per_s", d.linear_drag_per_s)?,
        wind_response: field("wind_response", d.wind_response)?,
        radius_m: field("radius_m", d.radius_m)?,
        lifetime_s: field("lifetime_s", d.lifetime_s)?,
        max_range_m: field("max_range_m", d.max_range_m)?,
        fuse_s,
        restitution: field("restitution", d.restitution)?,
        friction: field("friction", d.friction)?,
        max_bounces,
        stop_speed_m_s: field("stop_speed_m_s", d.stop_speed_m_s)?,
    };
    p.validate().map_err(|e| {
        let mut error = bad(e.field, format!("[{}] {e}", e.kind.code()));
        if let Some(Value::Object(data)) = error.data.as_mut() {
            data.insert("kind".into(), Value::String(e.kind.code().into()));
        }
        error
    })?;
    Ok(p)
}

/// The world from `params.world` (missing fields: [`WorldPhysics::EARTH`]), validated.
fn world(params: &Value) -> Result<WorldPhysics, RpcError> {
    let Some(map) = object(params, "world")? else {
        return Ok(WorldPhysics::EARTH);
    };
    let e = WorldPhysics::EARTH;
    let vacuum = match map.get("vacuum") {
        None | Some(Value::Null) => e.vacuum,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| bad("world.vacuum", "`world.vacuum` must be a boolean"))?,
    };
    let low_g = match map.get("low_g") {
        None | Some(Value::Null) => e.low_g,
        Some(Value::String(word)) if word == "governor" => LowG::Governor,
        Some(Value::String(word)) if word == "physical" => LowG::Physical,
        Some(other) => {
            return Err(bad(
                "world.low_g",
                format!("`world.low_g` is \"governor\" or \"physical\", not {other}"),
            ));
        }
    };
    let w = WorldPhysics {
        gravity_m_s2: number(map, "world.gravity_m_s2", "gravity_m_s2", e.gravity_m_s2)?,
        air_density_kg_m3: number(
            map,
            "world.air_density_kg_m3",
            "air_density_kg_m3",
            e.air_density_kg_m3,
        )?,
        vacuum,
        low_g,
    };
    w.validate()
        .map_err(|err| bad(&format!("world.{}", err.field), err.to_string()))?;
    Ok(w)
}

fn rate(params: &Value) -> Result<TickRate, RpcError> {
    let hz = count(params, "rate_hz", 60)?;
    u32::try_from(hz)
        .ok()
        .filter(|hz| (1..=1000).contains(hz))
        .and_then(TickRate::new)
        .ok_or_else(|| bad("rate_hz", "`rate_hz` must be 1..=1000"))
}

fn shapes<'a>(map: &'a Map<String, Value>, key: &str) -> Result<&'a [Value], RpcError> {
    match map.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) if items.len() <= MAX_SHAPES => Ok(items),
        Some(_) => Err(bad(
            &format!("geometry.{key}"),
            format!("`geometry.{key}` must be an array of at most {MAX_SHAPES} shapes"),
        )),
    }
}

/// The reference geometry from `params.geometry`.
fn geometry(params: &Value) -> Result<LinearScanHits, RpcError> {
    let mut hits = LinearScanHits::default();
    let Some(map) = object(params, "geometry")? else {
        return Ok(hits);
    };
    if map.get("ground_y").is_some_and(|v| !v.is_null()) {
        hits.planes.push(HitPlane::ground(number(
            map,
            "geometry.ground_y",
            "ground_y",
            0.0,
        )?));
    }
    for plane in shapes(map, "planes")? {
        let normal = vec3(plane.get("normal"), "geometry.planes.normal")?
            .ok_or_else(|| bad("geometry.planes.normal", "a plane needs `normal`"))?;
        let len =
            regia_detsim::math::sqrt(normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2]);
        if len < 1.0e-6 {
            return Err(bad(
                "geometry.planes.normal",
                "a plane's `normal` must not be zero",
            ));
        }
        let offset = plane
            .as_object()
            .map_or(Ok(0.0), |p| number(p, "geometry.planes.offset", "offset", 0.0))?;
        hits.planes.push(HitPlane {
            normal: [normal[0] / len, normal[1] / len, normal[2] / len],
            offset,
        });
    }
    for b in shapes(map, "boxes")? {
        let min = vec3(b.get("min"), "geometry.boxes.min")?;
        let max = vec3(b.get("max"), "geometry.boxes.max")?;
        let (Some(min), Some(max)) = (min, max) else {
            return Err(bad("geometry.boxes", "a box needs `min` and `max`"));
        };
        hits.boxes.push(HitBox { min, max });
    }
    for s in shapes(map, "spheres")? {
        let center = vec3(s.get("center"), "geometry.spheres.center")?
            .ok_or_else(|| bad("geometry.spheres.center", "a sphere needs `center`"))?;
        let fields = s
            .as_object()
            .ok_or_else(|| bad("geometry.spheres", "a sphere is an object"))?;
        let radius_m = number(fields, "geometry.spheres.radius_m", "radius_m", 0.5)?;
        let body = fields
            .get("body")
            .and_then(Value::as_u64)
            .and_then(|b| u32::try_from(b).ok())
            .ok_or_else(|| bad("geometry.spheres.body", "a sphere needs `body`, an integer id"))?;
        hits.spheres.push(HitSphere {
            center,
            radius_m,
            body,
        });
    }
    Ok(hits)
}

fn target_json(target: Option<HitTarget>) -> Value {
    match target {
        None => Value::Null,
        Some(HitTarget::World) => json!("world"),
        Some(HitTarget::Body(body)) => json!({ "body": body }),
    }
}

fn impact_json(impact: &Impact, tick: u64) -> Value {
    json!({
        "tick": tick,
        "reason": impact.reason.name(),
        "point": impact.point,
        "normal": impact.normal,
        "target": target_json(impact.target),
        "speed": impact.speed,
        "age_ticks": impact.age_ticks,
    })
}

/// `detsim.projectile_simulate`: fly one projectile and answer its sampled path and its impact.
pub fn projectile_simulate(params: &Value) -> Result<Value, RpcError> {
    let kind = projectile_params(params)?;
    let world = world(params)?;
    let dt = rate(params)?.dt();
    let launch = object(params, "launch")?.ok_or_else(|| bad("launch", "`launch` {pos, vel} is required"))?;
    let pos = vec3(launch.get("pos"), "launch.pos")?.unwrap_or([0.0; 3]);
    let vel = vec3(launch.get("vel"), "launch.vel")?
        .ok_or_else(|| bad("launch.vel", "`launch.vel` [x, y, z] is required"))?;
    let owner = match launch.get("owner") {
        None | Some(Value::Null) => None,
        Some(v) => Some(
            v.as_u64()
                .and_then(|o| u32::try_from(o).ok())
                .ok_or_else(|| bad("launch.owner", "`launch.owner` must be a body id"))?,
        ),
    };
    let wind = vec3(params.get("wind"), "wind")?.unwrap_or([0.0; 3]);
    let steps = count(params, "steps", 600)?;
    let every = count(params, "sample_every", 1)?.max(1);
    if steps == 0 || steps > MAX_STEPS {
        return Err(steps_limit(format!(
            "`steps` is {steps}; fly 1..={MAX_STEPS} ticks"
        )));
    }
    if steps / every > MAX_SAMPLES {
        return Err(steps_limit(format!(
            "{steps} ticks sampled every {every} is more than {MAX_SAMPLES} samples; raise `sample_every`"
        )));
    }
    let mut hits = geometry(params)?;
    let mut state = ProjectileState::launch(pos, vel, owner);
    let mut samples = Vec::new();
    let mut impact = Value::Null;
    let mut ticks = 0_u64;
    for tick in 1..=steps {
        let ended = projectile::step(&kind, &world, wind, &mut state, dt, &mut hits);
        ticks = tick;
        if tick.is_multiple_of(every) || ended.is_some() || tick == steps {
            samples.push(json!({
                "tick": tick,
                "pos": state.pos,
                "vel": state.vel,
                "bounces": state.bounces,
                "resting": state.resting,
            }));
        }
        if let Some(end) = ended {
            impact = impact_json(&end, tick);
            break;
        }
    }
    Ok(json!({
        "ticks": ticks,
        "dt": dt,
        "alive": state.alive,
        "travelled_m": state.travelled_m,
        "samples": samples,
        "impact": impact,
    }))
}

/// `detsim.aim_solve`: the drag-free launch angles for `delta`, and with `drag: true` the pitch
/// that hits it with the projectile's drag.
pub fn aim_solve(params: &Value) -> Result<Value, RpcError> {
    let kind = projectile_params(params)?;
    let world = world(params)?;
    let delta =
        vec3(params.get("delta"), "delta")?.ok_or_else(|| bad("delta", "`delta` [x, y, z] is required"))?;
    let speed_error = || bad("speed", "`speed` must be a positive number (m/s)");
    let speed = params
        .get("speed")
        .and_then(Value::as_f64)
        .ok_or_else(speed_error)? as f32;
    if !speed.is_finite() || speed <= 0.0 {
        return Err(speed_error());
    }
    // A wrong JSON type is refused like a wrong word (ADR-2622), never read as the default.
    let arc = match params.get("arc") {
        None | Some(Value::Null) => AimArc::Low,
        Some(Value::String(word)) if word == "low" => AimArc::Low,
        Some(Value::String(word)) if word == "high" => AimArc::High,
        Some(other) => return Err(bad("arc", format!("`arc` is \"low\" or \"high\", not {other}"))),
    };
    let drag = match params.get("drag") {
        None | Some(Value::Null) => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| bad("drag", "`drag` must be a boolean"))?,
    };
    let iterations = count(params, "iterations", 8)?;
    if iterations > MAX_ITERATIONS {
        return Err(steps_limit(format!(
            "`iterations` is {iterations}; at most {MAX_ITERATIONS}"
        )));
    }
    let max_ticks = count(params, "max_ticks", 3600)?;
    if max_ticks == 0 || max_ticks > MAX_STEPS {
        return Err(steps_limit(format!(
            "`max_ticks` is {max_ticks}; 1..={MAX_STEPS}"
        )));
    }
    let tolerance_m = match params.get("tolerance_m") {
        None | Some(Value::Null) => DragAim::DEFAULT.tolerance_m,
        Some(value) => match value.as_f64() {
            Some(t) if t.is_finite() && t >= 0.0 => t as f32,
            _ => return Err(bad("tolerance_m", "`tolerance_m` must be a number of 0 or more")),
        },
    };
    let opts = DragAim {
        dt: rate(params)?.dt(),
        max_ticks: max_ticks as u32,
        arc,
        tolerance_m,
    };
    let g = map_g(world.gravity_m_s2) * kind.gravity_scale;
    let angles = aim::launch_angles(delta, speed, g);
    let time = |pitch: f32| aim::time_of_flight(delta, speed, pitch, g);
    let solution = if drag {
        aim::solve_with_drag(&kind, &world, delta, speed, iterations as u8, &opts).map(|s| {
            json!({
                "pitch": s.pitch,
                "yaw": s.yaw,
                "velocity": s.velocity,
                "time_s": s.time_s,
                "miss_m": s.miss_m,
                "probes": s.probes,
            })
        })
    } else {
        None
    };
    Ok(json!({
        "reachable": angles.is_some(),
        "low": angles.map(|a| a.0),
        "high": angles.map(|a| a.1),
        "time_low": angles.and_then(|a| time(a.0)),
        "time_high": angles.and_then(|a| time(a.1)),
        "yaw": aim::yaw_to(delta),
        "gravity": g,
        "solution": solution,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code(error: &RpcError) -> Option<&str> {
        error.data.as_ref().and_then(|d| d["code"].as_str())
    }

    #[test]
    fn simulate_answers_the_same_path_and_a_ground_impact() {
        let call = json!({
            "params": { "radius_m": 0.05, "restitution": 0.5, "max_bounces": 2 },
            "world": { "gravity_m_s2": 9.81, "air_density_kg_m3": 1.2 },
            "launch": { "pos": [0.0, 1.0, 0.0], "vel": [20.0, 15.0, 0.0] },
            "geometry": { "ground_y": 0.0 },
            "steps": 2000,
            "sample_every": 10
        });
        let a = projectile_simulate(&call).expect("flies");
        let b = projectile_simulate(&call).expect("flies");
        assert_eq!(a, b);
        assert_eq!(a["impact"]["reason"], "hit");
        assert_eq!(a["impact"]["target"], "world");
        assert_eq!(a["alive"], false);
        assert!(a["samples"].as_array().is_some_and(|s| s.len() > 5));
    }

    #[test]
    fn bad_params_and_steps_limit_have_their_codes() {
        let launch = json!({ "vel": [1.0, 1.0, 0.0] });
        for (call, field) in [
            (
                json!({ "launch": launch, "params": { "restitution": 2.0 } }),
                "restitution",
            ),
            (json!({ "launch": launch, "params": { "fuse_s": 0.0 } }), "fuse_s"),
            (
                json!({ "launch": launch, "world": { "gravity_m_s2": -1.0 } }),
                "world.gravity_m_s2",
            ),
            (json!({ "launch": { "vel": [1.0, 2.0] } }), "launch.vel"),
            (json!({}), "launch"),
        ] {
            let error = projectile_simulate(&call).expect_err("refused");
            assert_eq!(code(&error), Some("bad_params"), "{call}");
            assert_eq!(error.data.as_ref().and_then(|d| d["field"].as_str()), Some(field));
        }
        for call in [
            json!({ "launch": launch, "steps": 0 }),
            json!({ "launch": launch, "steps": 36_001 }),
            json!({ "launch": launch, "steps": 36_000 }),
        ] {
            let error = projectile_simulate(&call).expect_err("refused");
            assert_eq!(code(&error), Some("steps_limit"), "{call}");
        }
        let error = aim_solve(&json!({ "delta": [10.0, 0.0, 0.0], "speed": 30.0, "iterations": 33 }))
            .expect_err("refused");
        assert_eq!(code(&error), Some("steps_limit"));
        let error = aim_solve(&json!({ "delta": [10.0, 0.0, 0.0], "speed": 0.0 })).expect_err("refused");
        assert_eq!(code(&error), Some("bad_params"));
        // A wrong JSON type for an optional field is refused, never read as its default (ADR-2622).
        for (field, value) in [
            ("arc", json!(1)),
            ("arc", json!(true)),
            ("tolerance_m", json!("0.1")),
            ("tolerance_m", json!(false)),
            ("speed", json!("30")),
        ] {
            let mut call = json!({ "delta": [10.0, 0.0, 0.0], "speed": 30.0 });
            call[field] = value;
            let error = aim_solve(&call).expect_err("refused");
            assert_eq!(code(&error), Some("bad_params"), "{call}");
            let named = error.data.as_ref().and_then(|d| d["field"].as_str());
            assert_eq!(named, Some(field), "{call}");
        }
        for call in [
            json!({ "delta": [10.0, 0.0, 0.0], "speed": 30.0, "arc": null, "tolerance_m": null }),
            json!({ "delta": [10.0, 0.0, 0.0], "speed": 30.0, "arc": "high", "tolerance_m": 0.5 }),
        ] {
            aim_solve(&call).expect("a null or well-typed optional field is read");
        }
        let call = json!({ "delta": [10.0, 0.0, 0.0], "speed": 30.0, "world": { "low_g": 1 } });
        let error = aim_solve(&call).expect_err("a wrong-typed world.low_g is refused");
        assert_eq!(code(&error), Some("bad_params"));
        let named = error.data.as_ref().and_then(|d| d["field"].as_str());
        assert_eq!(named, Some("world.low_g"));
    }

    #[test]
    fn aim_solve_answers_both_arcs_and_a_drag_solution() {
        let answer = aim_solve(&json!({
            "delta": [150.0, 5.0, 40.0],
            "speed": 80.0,
            "params": { "drag_per_m": 0.004 },
            "drag": true,
            "iterations": 8
        }))
        .expect("solves");
        assert_eq!(answer["reachable"], true);
        let low = answer["low"].as_f64().expect("low");
        let high = answer["high"].as_f64().expect("high");
        assert!(low < high);
        let pitch = answer["solution"]["pitch"].as_f64().expect("a drag solution");
        assert!(pitch > low, "drag needs more pitch on the low arc");
        let far = aim_solve(&json!({ "delta": [5000.0, 0.0, 0.0], "speed": 80.0 })).expect("answers");
        assert_eq!(far["reachable"], false);
        assert_eq!(far["low"], Value::Null);
    }
}
