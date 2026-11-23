// SPDX-License-Identifier: MPL-2.0
//! `camera_body.*`: the camera as a physical actor (ADR-1502, `book/agents/camera-body.md`).
//!
//! - `camera_body.get` reads the spec and last frame's state.
//! - `camera_body.set` merges a spec.
//! - `camera_body.wake` samples the air wake at points.
//! - `camera_body.proximity` asks what comes within R of the loaded camera path: points, birds,
//!   rigid bodies, scatter.
//! - `camera_body.clearance` sweeps the path against the terrain and the scatter (trunks and
//!   crowns) and proposes lifted keys.
//! - `camera_body.lift_path` applies that lift to a shot of the loaded sequence.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_framework::bird::BirdFlock;
use regia_framework::camera_body::clearance::{
    ClearanceReport, ClearanceSpec, ClearanceWorld, Obstacle, check_clearance, lift_shot,
};
use regia_framework::camera_body::proximity::{ProximityQuery, ProximityTarget, proximity};
use regia_framework::camera_body::{CameraBody, CameraBodySpec};
use regia_framework::shots::{ShotPlayer, ShotSequence};
use regia_physics::{BodyKind, RigidBody};
use regia_scene::GlobalTransform;
use regia_terrain::Terrain;
use regia_terrain::scatter::runtime::RuntimeScatter;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Most explicit targets one proximity call takes.
const MAX_TARGETS: usize = 100_000;
/// Most points one wake call samples.
const MAX_POINTS: usize = 10_000;

fn parse<T: DeserializeOwned>(params: &Value, field: &str) -> Result<Option<T>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => serde_json::from_value(v.clone())
            .map(Some)
            .map_err(|e| RpcError::invalid_params(format!("`{field}`: {e}"))),
    }
}

fn flag(params: &Value, field: &str, default: bool) -> Result<bool, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(default),
        Some(v) => v
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a boolean"))),
    }
}

fn number(params: &Value, field: &str) -> Result<Option<f64>, RpcError> {
    match params.get(field).filter(|v| !v.is_null()) {
        None => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|f| f.is_finite())
            .map(Some)
            .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a finite number"))),
    }
}

fn to_json<T: serde::Serialize>(value: &T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|e| RpcError::internal(e.to_string()))
}

/// JSON merge patch (RFC 7386): objects merge key by key, `null` removes, anything else
/// replaces.
pub(crate) fn merge(base: &mut Value, patch: &Value) {
    match (base, patch) {
        (Value::Object(b), Value::Object(p)) => {
            for (k, v) in p {
                if v.is_null() {
                    b.remove(k);
                } else {
                    merge(b.entry(k.clone()).or_insert(Value::Null), v);
                }
            }
        }
        (b, p) => *b = p.clone(),
    }
}

fn no_body() -> RpcError {
    RpcError::not_found(
        "no camera body",
        "the host adds regia_framework::camera_body::CameraBodyPlugin (the showcase does); \
         camera_body.set creates one",
    )
}

fn no_path() -> RpcError {
    RpcError::not_found(
        "no camera path loaded",
        "load one with camera.set_path or render.sequence {\"action\": \"load\"} first",
    )
}

fn sequence(world: &World) -> Option<ShotSequence> {
    world.get_resource::<ShotPlayer>()?.sequence().cloned()
}

/// `camera_body.get`.
pub(crate) fn get(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let body = world.get_resource::<CameraBody>().ok_or_else(no_body)?;
    Ok(json!({
        "spec": to_json(body.spec())?,
        "state": to_json(body.report())?,
        "wake_active": body.wake_words().is_some(),
    }))
}

/// `camera_body.set`.
pub(crate) fn set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let current = world
        .get_resource::<CameraBody>()
        .map(|b| b.spec().clone())
        .unwrap_or_default();
    let mut doc = to_json(&current)?;
    if let Some(patch) = params.get("spec").filter(|v| !v.is_null()) {
        if !patch.is_object() {
            return Err(RpcError::invalid_params(
                "`spec` must be an object (merged onto the current spec)",
            ));
        }
        merge(&mut doc, patch);
    }
    if let Some(enabled) = params.get("enabled").filter(|v| !v.is_null()) {
        let enabled = enabled
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?;
        doc["enabled"] = json!(enabled);
    }
    let spec =
        CameraBodySpec::from_json(&doc.to_string()).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    match world.get_resource_mut::<CameraBody>() {
        Some(mut body) => body.set_spec(spec.clone()),
        None => {
            world.insert_resource(CameraBody::new(spec.clone()));
        }
    }
    let spec = world
        .get_resource::<CameraBody>()
        .map(|b| b.spec().clone())
        .unwrap_or(spec);
    Ok(json!({ "spec": to_json(&spec)? }))
}

/// `camera_body.wake`.
pub(crate) fn wake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let points: Vec<[f32; 3]> = parse(params, "points")?
        .ok_or_else(|| RpcError::invalid_params("`points` is required: [[x, y, z], ...]"))?;
    if points.len() > MAX_POINTS {
        return Err(RpcError::invalid_params(format!("at most {MAX_POINTS} points")));
    }
    let time = number(params, "time")?;
    let body = world.get_resource::<CameraBody>().ok_or_else(no_body)?;
    let state = match time {
        Some(t) => {
            let seq = sequence(world).ok_or_else(no_path)?;
            body.sample_path(&seq, t)
        }
        None => body.state(),
    };
    let shape = body.spec().wake;
    let samples: Vec<Value> = points
        .iter()
        .map(|p| {
            let s = state.map(|b| shape.sample(&b, Vec3::from_array(*p))).unwrap_or_default();
            json!({
                "gust": s.gust.to_array(),
                "gust_m_s": s.gust.length(),
                "peak_m_s": s.peak_m_s,
                "since_front_s": if s.since_front_s.is_finite() { json!(s.since_front_s) } else { Value::Null },
                "lateral_m": s.lateral_m,
                "behind_m": s.behind_m,
            })
        })
        .collect();
    Ok(json!({
        "time": time.unwrap_or_else(|| body.time()),
        "body": state.map(|b| json!({ "position": b.position.to_array(), "velocity": b.velocity.to_array() })),
        "samples": samples,
    }))
}

/// Every dynamic rigid body as a proximity target (`body:<entity bits>`).
fn body_targets(world: &mut World, out: &mut Vec<ProximityTarget>) {
    let mut q = world.query::<(Entity, &RigidBody, &GlobalTransform)>();
    let mut found: Vec<ProximityTarget> = q
        .iter(world)
        .filter(|(_, r, _)| r.kind == BodyKind::Dynamic)
        .map(|(e, _, t)| ProximityTarget {
            id: format!("body:{}", e.to_bits()),
            position: t.translation().to_array(),
            radius_m: 0.0,
        })
        .collect();
    found.sort_by(|a, b| a.id.cmp(&b.id));
    out.extend(found);
}

/// Every resident scatter instance's bounding sphere (`scatter:<index>`).
fn scatter_spheres(world: &mut World) -> Vec<([f32; 3], f32)> {
    world
        .get_resource_mut::<RuntimeScatter>()
        .map(|mut s| s.field_mut().instance_spheres())
        .unwrap_or_default()
}

/// `camera_body.proximity`.
pub(crate) fn proximity_call(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let seq = sequence(world).ok_or_else(no_path)?;
    let mut query = ProximityQuery {
        body_radius_m: world
            .get_resource::<CameraBody>()
            .map_or(0.0, |b| b.spec().wake.radius_m),
        ..ProximityQuery::default()
    };
    if let Some(r) = number(params, "radius_m")? {
        if r < 0.0 {
            return Err(RpcError::invalid_params("`radius_m` must be at least zero"));
        }
        query.radius_m = r as f32;
    }
    if let Some(v) = number(params, "from_s")? {
        query.from_s = v;
    }
    if let Some(v) = number(params, "to_s")? {
        query.to_s = v;
    }
    if let Some(v) = number(params, "step_s")? {
        if v <= 0.0 {
            return Err(RpcError::invalid_params("`step_s` must be greater than zero"));
        }
        query.step_s = v.max(1.0e-4);
    }
    let mut targets: Vec<ProximityTarget> = parse(params, "targets")?.unwrap_or_default();
    if targets.len() > MAX_TARGETS {
        return Err(RpcError::invalid_params(format!("at most {MAX_TARGETS} targets")));
    }
    if flag(params, "include_birds", true)?
        && let Some(flock) = world.get_resource::<BirdFlock>()
    {
        targets.extend(flock.reports().iter().map(|r| ProximityTarget {
            id: format!("bird:{}", r.name),
            position: r.position,
            radius_m: 0.0,
        }));
    }
    if flag(params, "include_bodies", true)? {
        body_targets(world, &mut targets);
    }
    if flag(params, "include_scatter", false)? {
        targets.extend(
            scatter_spheres(world)
                .into_iter()
                .enumerate()
                .map(|(i, (c, r))| ProximityTarget {
                    id: format!("scatter:{i}"),
                    position: c,
                    radius_m: r,
                }),
        );
    }
    let hits = proximity(&seq, &targets, &query);
    Ok(json!({ "targets": targets.len(), "hits": to_json(&hits)? }))
}

/// The world as the clearance guard sees it: the terrains' height maps and the scatter's
/// bounding spheres as trees (crown = the sphere, trunk = a thin post from the ground to its
/// centre).
struct WorldClearance<'w> {
    terrains: Vec<&'w Terrain>,
    trees: Vec<([f32; 3], f32)>,
}

impl WorldClearance<'_> {
    fn terrain_at(&self, x: f32, z: f32) -> Option<&Terrain> {
        self.terrains.iter().copied().find(|t| {
            // The map's square, moved by its grid origin (ADR-8179).
            let ([x0, z0], [x1, z1]) = (t.map.world_min(), t.map.world_max());
            (x0..=x1).contains(&x) && (z0..=z1).contains(&z)
        })
    }
}

impl ClearanceWorld for WorldClearance<'_> {
    fn ground_height(&self, x: f32, z: f32) -> Option<f32> {
        self.terrain_at(x, z).map(|t| t.map.sample_bilinear(x, z))
    }

    fn raycast(&self, origin: Vec3, direction: Vec3, max_distance: f32) -> Option<f32> {
        self.terrains
            .iter()
            .filter_map(|t| regia_terrain::raycast_terrain(&t.map, origin, direction, max_distance))
            .map(|hit| hit.distance(origin))
            .reduce(f32::min)
    }

    fn obstacles(&self, lo: Vec3, hi: Vec3, out: &mut Vec<Obstacle>) {
        for (i, (c, r)) in self.trees.iter().enumerate() {
            let centre = Vec3::from_array(*c);
            if (centre - Vec3::splat(*r)).cmpgt(hi).any() || (centre + Vec3::splat(*r)).cmplt(lo).any() {
                continue;
            }
            let ground = self.ground_height(c[0], c[2]).unwrap_or(c[1] - r);
            out.push(Obstacle {
                id: format!("scatter:{i}"),
                base: [c[0], ground, c[2]],
                trunk_radius_m: (0.05 * r).max(0.1),
                trunk_height_m: (c[1] - ground).max(0.0),
                crown_center: *c,
                crown_radius_m: *r,
            });
        }
    }
}

fn clearance_spec(world: &World, params: &Value) -> Result<ClearanceSpec, RpcError> {
    let mut spec = ClearanceSpec {
        body_radius_m: world
            .get_resource::<CameraBody>()
            .map_or(ClearanceSpec::default().body_radius_m, |b| b.spec().wake.radius_m),
        ..ClearanceSpec::default()
    };
    for (field, slot) in [
        ("clearance_m", &mut spec.clearance_m),
        ("body_radius_m", &mut spec.body_radius_m),
    ] {
        if let Some(v) = number(params, field)? {
            if v < 0.0 {
                return Err(RpcError::invalid_params(format!(
                    "`{field}` must be at least zero"
                )));
            }
            *slot = v as f32;
        }
    }
    if let Some(v) = number(params, "step_s")? {
        if v <= 0.0 {
            return Err(RpcError::invalid_params("`step_s` must be greater than zero"));
        }
        spec.step_s = v.max(1.0e-4);
    }
    if let Some(v) = number(params, "from_s")? {
        spec.from_s = v;
    }
    if let Some(v) = number(params, "to_s")? {
        spec.to_s = v;
    }
    spec.ground = flag(params, "ground", true)?;
    spec.obstacles = flag(params, "obstacles", true)?;
    Ok(spec)
}

fn report_json(report: &ClearanceReport) -> Result<Value, RpcError> {
    let finite = |v: f32| if v.is_finite() { json!(v) } else { Value::Null };
    Ok(json!({
        "clear": report.is_clear(),
        "samples": report.samples,
        "min_ground_gap_m": finite(report.min_ground_gap_m),
        "min_ground_s": report.min_ground_s,
        "min_obstacle_gap_m": finite(report.min_obstacle_gap_m),
        "obstacles_checked": report.obstacles_checked,
        "violations": to_json(&report.violations)?,
    }))
}

fn shot_index(seq: &ShotSequence, params: &Value) -> Result<usize, RpcError> {
    match params.get("shot").filter(|v| !v.is_null()) {
        None if seq.shots.len() == 1 => Ok(0),
        None => Err(RpcError::invalid_params(
            "the sequence has several shots: name one with `shot`",
        )),
        Some(v) => {
            let name = v
                .as_str()
                .ok_or_else(|| RpcError::invalid_params("`shot` must be a shot name"))?;
            seq.shot_index(name).ok_or_else(|| {
                RpcError::not_found(format!("no shot named `{name}`"), "render.sequence lists them")
            })
        }
    }
}

fn lift_options(params: &Value) -> Result<(f32, u32), RpcError> {
    let margin = number(params, "margin_m")?.unwrap_or(1.0);
    if margin < 0.0 {
        return Err(RpcError::invalid_params("`margin_m` must be at least zero"));
    }
    let iterations = match params.get("iterations").filter(|v| !v.is_null()) {
        None => 4,
        Some(v) => v
            .as_u64()
            .filter(|n| (1..=16).contains(n))
            .ok_or_else(|| RpcError::invalid_params("`iterations` must be a whole number from 1 to 16"))?
            as u32,
    };
    Ok((margin as f32, iterations))
}

fn with_clearance_world<R>(world: &mut World, f: impl FnOnce(&WorldClearance<'_>) -> R) -> R {
    let trees = scatter_spheres(world);
    let terrains: Vec<&Terrain> = world.iter_entities().filter_map(|e| e.get::<Terrain>()).collect();
    f(&WorldClearance { terrains, trees })
}

/// `camera_body.clearance`.
pub(crate) fn clearance(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let seq = sequence(world).ok_or_else(no_path)?;
    let spec = clearance_spec(world, params)?;
    let propose = flag(params, "propose_lift", false)?;
    let (margin, iterations) = lift_options(params)?;
    let index = if propose {
        Some(shot_index(&seq, params)?)
    } else {
        None
    };
    let (report, lifted) = with_clearance_world(world, |cw| {
        let report = check_clearance(&seq, cw, &spec);
        let lifted = index.map(|i| {
            let (lifted, after) = lift_shot(&seq, i, cw, &spec, margin, iterations);
            (lifted.shots[i].camera.path.keys.clone(), after.is_clear())
        });
        (report, lifted)
    });
    let mut out = report_json(&report)?;
    if let Some((keys, clear)) = lifted {
        out["lifted_keys"] = to_json(&keys)?;
        out["lifted_clear"] = json!(clear);
    }
    Ok(out)
}

/// `camera_body.lift_path`.
pub(crate) fn lift_path(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let seq = sequence(world).ok_or_else(no_path)?;
    let spec = clearance_spec(world, params)?;
    let (margin, iterations) = lift_options(params)?;
    let index = shot_index(&seq, params)?;
    let dry_run = flag(params, "dry_run", false)?;
    let (lifted, before, after) = with_clearance_world(world, |cw| {
        let before = check_clearance(&seq, cw, &spec);
        let (lifted, after) = lift_shot(&seq, index, cw, &spec, margin, iterations);
        (lifted, before, after)
    });
    let changed = lifted != seq;
    if changed
        && !dry_run
        && let Some(mut player) = world.get_resource_mut::<ShotPlayer>()
    {
        player.replace_sequence(lifted.clone());
    }
    // Keys that rose, by index.
    let raised: Vec<Value> = seq.shots[index]
        .camera
        .path
        .keys
        .iter()
        .zip(&lifted.shots[index].camera.path.keys)
        .enumerate()
        .filter(|(_, (a, b))| a.position != b.position)
        .map(|(i, (a, b))| json!({ "key": i, "time": a.time, "lift_m": b.position[1] - a.position[1] }))
        .collect();
    Ok(json!({
        "applied": changed && !dry_run,
        "shot": seq.shots[index].name,
        "raised": raised,
        "before": report_json(&before)?,
        "after": report_json(&after)?,
    }))
}
