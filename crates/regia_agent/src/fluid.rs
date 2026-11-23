// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the particle liquid (`regia_fluid`, ADR-2441, `book/agents/fluid.md`).
//!
//! - `fluid.create { preset?, bounds?, cell_size?, particle_cap?, seed?, whitewater? }`: a new
//!   simulation in the world's `FluidWorld` (inserted when absent), from a preset (`pour`,
//!   `dam_break`, `splash`, `stream`) or empty bounds.
//! - `fluid.add_emitter { id, kind?, shape?, center, half_extents?, radius?, velocity? }`: a fill,
//!   a source or a sink (`kind: "sink"`).
//! - `fluid.step { id, steps?, run? }`: fixed steps now; `run` sets whether `FluidPlugin` steps it
//!   every `FixedUpdate`.
//! - `fluid.stats { id }`: particles, volume and drift, divergence, PCG iterations, the digest and
//!   the whitewater counts.
//! - `fluid.bake { id, steps, whitewater?, path? }`: records the current frame and `steps` more into a
//!   `.fluidcache` (kept on the entry; written atomically to the project path when given).
//! - `fluid.play { id, frame?, path?, stop? }`: shows a cache frame instead of the live state (no
//!   solver runs), loads a `.fluidcache` from the project first when `path` is given.
//! - `fluid.destroy { id }`.
//! - `fluid.set_solids { id, solids }`: replaces the moving solids (kinematic boxes, spheres and
//!   capsules with a pose and velocities, ADR-2577); bodies with a `FluidBody` are set by
//!   `FluidPhysicsPlugin` every `FixedUpdate` instead.
//! - `fluid.solids { id }`: the moving solids, the pressure impulse each got in the last step, and the
//!   coupled physics bodies (`FluidCoupling`).
//!
//! Errors carry `data.code` `F0002` (bad parameters) or `F0003` (no such simulation or cache) and a
//! lowercase `data.reason` (`fluid_not_found`, `fluid_bad_param`, `fluid_bad_bounds`, ...).

use regia_app::prelude::*;
use regia_core::math::Quat;
use regia_core::math::Vec3;
use regia_fluid::cache::FluidCache;
use regia_fluid::coupling::FluidCoupling;
use regia_fluid::sim::{Emitter, EmitterKind, FluidParams, FluidSim, Shape};
use regia_fluid::solid::{MovingSolid, SolidMotion, SolidShape};
use regia_fluid::{FluidStats, FluidWorld, PointKind, presets};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most steps one `fluid.step` or `fluid.bake` call may run.
pub const MAX_STEPS_PER_CALL: u64 = 600;

/// Most `steps * (cells + particles)` one call may run: a big simulation steps in smaller batches,
/// so one request never holds the frame for long (`fluid_step_budget`).
pub const MAX_WORK_PER_CALL: u64 = 60_000_000;

fn check_budget(sim: &FluidSim, steps: u64) -> Result<(), RpcError> {
    let per_step = (sim.dims().cells() + sim.positions().len()) as u64;
    if steps.saturating_mul(per_step) > MAX_WORK_PER_CALL {
        return Err(bad(
            "fluid_step_budget",
            format!(
                "{steps} steps of {per_step} cells and particles is over one call's budget; step at most {} at a time",
                MAX_WORK_PER_CALL / per_step.max(1)
            ),
        ));
    }
    Ok(())
}

const HINT: &str = "see book/agents/fluid.md for the parameters and codes";

fn reasoned(code: i64, regia_code: &str, reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(code, regia_code, message, HINT);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn bad(reason: &str, message: impl Into<String>) -> RpcError {
    reasoned(protocol::INVALID_PARAMS, "F0002", reason, message)
}

fn missing(reason: &str, message: impl Into<String>) -> RpcError {
    reasoned(protocol::NOT_FOUND, "F0003", reason, message)
}

fn vec3(params: &Value, key: &str) -> Result<Option<Vec3>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(a)) if a.len() == 3 => {
            let mut out = [0.0f32; 3];
            for (slot, v) in out.iter_mut().zip(a) {
                let x = v
                    .as_f64()
                    .filter(|x| x.is_finite() && x.abs() <= 1.0e6)
                    .ok_or_else(|| bad("fluid_bad_param", format!("`{key}` must be three finite numbers")))?;
                *slot = x as f32;
            }
            Ok(Some(Vec3::from(out)))
        }
        Some(_) => Err(bad("fluid_bad_param", format!("`{key}` must be [x, y, z]"))),
    }
}

fn number(params: &Value, key: &str) -> Result<Option<f64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_f64()
            .filter(|x| x.is_finite())
            .map(Some)
            .ok_or_else(|| bad("fluid_bad_param", format!("`{key}` must be a finite number"))),
    }
}

fn integer(params: &Value, key: &str) -> Result<Option<u64>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| {
            bad(
                "fluid_bad_param",
                format!("`{key}` must be a non-negative integer"),
            )
        }),
    }
}

fn boolean(params: &Value, key: &str) -> Result<Option<bool>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(bad("fluid_bad_param", format!("`{key}` must be true or false"))),
    }
}

fn string<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, RpcError> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(bad("fluid_bad_param", format!("`{key}` must be a string"))),
    }
}

fn id_of(params: &Value) -> Result<u32, RpcError> {
    integer(params, "id")?
        .and_then(|v| u32::try_from(v).ok())
        .ok_or_else(|| {
            bad(
                "fluid_bad_param",
                "`id` (a fluid id from fluid.create) is required",
            )
        })
}

fn not_found(id: u32) -> RpcError {
    missing(
        "fluid_not_found",
        format!("no fluid simulation {id} (fluid.create makes one)"),
    )
}

fn fluid_error(e: &regia_fluid::FluidError) -> RpcError {
    bad(e.code(), e.to_string())
}

fn cache_error(e: &regia_fluid::CacheError) -> RpcError {
    bad(e.code(), e.to_string())
}

fn stats_json(id: u32, stats: &FluidStats, digest: u64) -> Value {
    json!({
        "id": id,
        "steps": stats.steps,
        "particles": stats.particles,
        "volume": stats.volume,
        "nominal_volume": stats.nominal_volume,
        "volume_drift": stats.volume_drift,
        "divergence": stats.divergence_max,
        "pcg_iterations": stats.pcg_iterations,
        "pcg_residual": stats.pcg_residual,
        "pcg_converged": stats.pcg_converged,
        "substeps": stats.substeps,
        "liquid_cells": stats.liquid_cells,
        "dropped_non_finite": stats.dropped_non_finite,
        "foam": stats.foam,
        "spray": stats.spray,
        "bubbles": stats.bubbles,
        "whitewater_generated": stats.whitewater_generated,
        "moving_solids": stats.moving_solids,
        "wetted_faces": stats.wetted_faces,
        // Hex text: a u64 does not survive every JSON reader as a number.
        "digest": format!("{digest:016x}"),
    })
}

/// `fluid.create`.
pub fn create(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let preset = string(params, "preset")?;
    let mut p = FluidParams::default();
    if let Some(name) = preset {
        let Some((lo, hi, dx)) = presets::default_bounds(name) else {
            return Err(bad(
                "fluid_unknown_preset",
                format!("unknown preset {name:?} (one of {})", presets::PRESETS.join(", ")),
            ));
        };
        (p.bounds_min, p.bounds_max, p.cell_size) = (lo, hi, dx);
    }
    match params.get("bounds") {
        None | Some(Value::Null) if preset.is_none() => {
            return Err(bad(
                "fluid_bad_bounds",
                "`bounds` { min, max } is required without a preset",
            ));
        }
        None | Some(Value::Null) => {}
        Some(b @ Value::Object(_)) => {
            let (Some(lo), Some(hi)) = (vec3(b, "min")?, vec3(b, "max")?) else {
                return Err(bad(
                    "fluid_bad_bounds",
                    "`bounds` needs `min` and `max` ([x, y, z] each)",
                ));
            };
            (p.bounds_min, p.bounds_max) = (lo, hi);
        }
        Some(_) => {
            return Err(bad(
                "fluid_bad_bounds",
                "`bounds` must be { min: [x, y, z], max: [x, y, z] }",
            ));
        }
    }
    if let Some(dx) = number(params, "cell_size")? {
        p.cell_size = dx as f32;
    }
    if let Some(cap) = integer(params, "particle_cap")? {
        p.particle_cap = usize::try_from(cap).unwrap_or(usize::MAX);
    }
    if let Some(seed) = integer(params, "seed")? {
        p.seed = seed;
    }
    if let Some(ww) = boolean(params, "whitewater")? {
        p.whitewater.enabled = ww;
    }
    let sim = match preset {
        Some(name) => presets::build(name, p),
        None => FluidSim::new(p),
    }
    .map_err(|e| fluid_error(&e))?;
    let d = *sim.dims();
    let particle_cap = sim.params().particle_cap;
    let mut fluids = world.get_resource_or_insert_with(FluidWorld::default);
    let id = fluids.insert(sim, preset.map(str::to_string)).ok_or_else(|| {
        bad(
            "fluid_too_many_sims",
            format!("at most {} simulations", regia_fluid::plugin::MAX_SIMS),
        )
    })?;
    Ok(json!({
        "id": id,
        "preset": preset,
        "cells": [d.nx, d.ny, d.nz],
        "cell_size": d.dx,
        "particle_cap": particle_cap,
    }))
}

/// `fluid.add_emitter`.
pub fn add_emitter(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_of(params)?;
    let center =
        vec3(params, "center")?.ok_or_else(|| bad("fluid_bad_param", "`center` [x, y, z] is required"))?;
    let shape = match string(params, "shape")?.unwrap_or("box") {
        "box" => Shape::Box {
            center,
            half_extents: vec3(params, "half_extents")?.unwrap_or(Vec3::splat(0.1)),
        },
        "sphere" => Shape::Sphere {
            center,
            radius: number(params, "radius")?.unwrap_or(0.1) as f32,
        },
        other => {
            return Err(bad(
                "fluid_bad_param",
                format!("`shape` {other:?} is not box or sphere"),
            ));
        }
    };
    let velocity = vec3(params, "velocity")?.unwrap_or(Vec3::ZERO);
    let kind = string(params, "kind")?.unwrap_or("fill");
    let Some(mut fluids) = world.get_resource_mut::<FluidWorld>() else {
        return Err(not_found(id));
    };
    let entry = fluids.get_mut(id).ok_or_else(|| not_found(id))?;
    let index = match kind {
        "fill" | "source" => entry.sim.add_emitter(Emitter {
            shape,
            velocity,
            kind: if kind == "fill" {
                EmitterKind::Fill
            } else {
                EmitterKind::Source
            },
            done: false,
        }),
        "sink" => entry.sim.add_sink(shape),
        other => {
            return Err(bad(
                "fluid_bad_param",
                format!("`kind` {other:?} is not fill, source or sink"),
            ));
        }
    }
    .map_err(|e| fluid_error(&e))?;
    entry.revision += 1;
    Ok(json!({ "id": id, "kind": kind, "index": index }))
}

/// `fluid.step`.
pub fn step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_of(params)?;
    let steps = integer(params, "steps")?.unwrap_or(1);
    if steps > MAX_STEPS_PER_CALL {
        return Err(bad(
            "fluid_bad_param",
            format!("`steps` must be at most {MAX_STEPS_PER_CALL}"),
        ));
    }
    let run = boolean(params, "run")?;
    let Some(mut fluids) = world.get_resource_mut::<FluidWorld>() else {
        return Err(not_found(id));
    };
    let entry = fluids.get_mut(id).ok_or_else(|| not_found(id))?;
    check_budget(&entry.sim, steps)?;
    for _ in 0..steps {
        entry.sim.step();
    }
    if steps > 0 {
        entry.playing = None;
        entry.revision += 1;
    }
    if let Some(run) = run {
        entry.running = run;
    }
    let mut out = stats_json(id, &entry.sim.stats(), entry.sim.state_digest());
    out["running"] = json!(entry.running);
    Ok(out)
}

/// `fluid.stats`.
pub fn stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_of(params)?;
    let fluids = world.get_resource::<FluidWorld>().ok_or_else(|| not_found(id))?;
    let entry = fluids.get(id).ok_or_else(|| not_found(id))?;
    let mut out = stats_json(id, &entry.sim.stats(), entry.sim.state_digest());
    out["running"] = json!(entry.running);
    out["playing"] = json!(entry.playing);
    out["cache_frames"] = json!(entry.cache.as_ref().map(FluidCache::frame_count));
    out["emitters"] = json!(entry.sim.emitters().len());
    out["sinks"] = json!(entry.sim.sinks().len());
    Ok(out)
}

fn cache_path(world: &World, params: &Value) -> Result<Option<std::path::PathBuf>, RpcError> {
    let Some(rel) = string(params, "path")? else {
        return Ok(None);
    };
    if !rel.ends_with(".fluidcache") {
        return Err(bad("fluid_bad_param", "`path` must end in .fluidcache"));
    }
    crate::req_path::project_path(world, "path", rel).map(Some)
}

fn write_atomic(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension(format!("fluidcache.tmp{}", std::process::id()));
    // ADR-10066: a symlink planted at the temp name is unlinked, never written through.
    crate::req_path::write_new_temp(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// `fluid.bake`.
pub fn bake(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_of(params)?;
    let steps = integer(params, "steps")?.ok_or_else(|| bad("fluid_bad_param", "`steps` is required"))?;
    if steps == 0 || steps > MAX_STEPS_PER_CALL {
        return Err(bad(
            "fluid_bad_param",
            format!("`steps` must be in 1..={MAX_STEPS_PER_CALL}"),
        ));
    }
    let whitewater = boolean(params, "whitewater")?.unwrap_or(true);
    let path = cache_path(world, params)?;
    let Some(mut fluids) = world.get_resource_mut::<FluidWorld>() else {
        return Err(not_found(id));
    };
    let entry = fluids.get_mut(id).ok_or_else(|| not_found(id))?;
    check_budget(&entry.sim, steps)?;
    let mut cache = FluidCache::for_sim(&entry.sim, whitewater);
    cache.record(&entry.sim).map_err(|e| cache_error(&e))?;
    for _ in 0..steps {
        entry.sim.step();
        cache.record(&entry.sim).map_err(|e| cache_error(&e))?;
    }
    let bytes = cache.to_bytes();
    let frames = cache.frame_count();
    entry.cache = Some(cache);
    entry.playing = None;
    entry.revision += 1;
    let digest = entry.sim.state_digest();
    if let Some(path) = &path {
        write_atomic(path, &bytes)
            .map_err(|e| bad("fluid_write_failed", format!("could not write the cache: {e}")))?;
    }
    Ok(json!({
        "id": id,
        "frames": frames,
        "bytes": bytes.len(),
        "whitewater": whitewater,
        "written": path.is_some(),
        "digest": format!("{digest:016x}"),
    }))
}

/// `fluid.play`.
pub fn play(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_of(params)?;
    let stop = boolean(params, "stop")?.unwrap_or(false);
    let frame = integer(params, "frame")?.unwrap_or(0);
    let loaded = match cache_path(world, params)? {
        Some(path) => {
            let meta = std::fs::metadata(&path)
                .map_err(|e| missing("fluid_no_cache", format!("cannot read the cache file: {e}")))?;
            if meta.len() > 1 << 30 {
                return Err(bad("fluidcache_too_large", "a .fluidcache over 1 GiB is refused"));
            }
            let bytes = std::fs::read(&path)
                .map_err(|e| missing("fluid_no_cache", format!("cannot read the cache file: {e}")))?;
            Some(FluidCache::from_bytes(&bytes).map_err(|e| cache_error(&e))?)
        }
        None => None,
    };
    let Some(mut fluids) = world.get_resource_mut::<FluidWorld>() else {
        return Err(not_found(id));
    };
    let entry = fluids.get_mut(id).ok_or_else(|| not_found(id))?;
    if let Some(cache) = loaded {
        entry.cache = Some(cache);
    }
    if stop {
        entry.playing = None;
        entry.revision += 1;
        return Ok(json!({ "id": id, "playing": Value::Null }));
    }
    let Some(cache) = entry.cache.as_ref() else {
        return Err(missing("fluid_no_cache", "no baked cache (fluid.bake makes one)"));
    };
    let count = cache.frame_count();
    let index = usize::try_from(frame).unwrap_or(usize::MAX);
    if index >= count {
        return Err(bad(
            "fluid_frame_out_of_range",
            format!("frame {frame} is past the end ({count} frames)"),
        ));
    }
    let shown = cache.play(index).map_err(|e| cache_error(&e))?;
    entry.playing = Some(index);
    entry.revision += 1;
    Ok(json!({
        "id": id,
        "playing": index,
        "frames": count,
        "step": shown.step,
        "points": shown.len(),
        "liquid": shown.count(PointKind::Liquid),
        "foam": shown.count(PointKind::Foam),
        "spray": shown.count(PointKind::Spray),
        "bubbles": shown.count(PointKind::Bubble),
    }))
}

/// `fluid.destroy`.
pub fn destroy(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_of(params)?;
    let removed = world
        .get_resource_mut::<FluidWorld>()
        .is_some_and(|mut f| f.remove(id));
    if !removed {
        return Err(not_found(id));
    }
    Ok(json!({ "id": id, "destroyed": true }))
}

/// Most solids one `fluid.set_solids` call takes (the solver's shape cap).
pub const MAX_SOLIDS_PER_CALL: usize = regia_fluid::sim::MAX_SHAPES;

fn solid_from(index: usize, v: &Value) -> Result<MovingSolid, RpcError> {
    let bad_solid = |what: &str| bad("fluid_bad_solid", format!("solids[{index}]: {what}"));
    if !v.is_object() {
        return Err(bad_solid("must be an object"));
    }
    let center = vec3(v, "center")?.ok_or_else(|| bad_solid("`center` [x, y, z] is required"))?;
    let shape = match string(v, "shape")?.unwrap_or("box") {
        "box" => SolidShape::Box {
            half_extents: vec3(v, "half_extents")?.unwrap_or(Vec3::splat(0.1)),
        },
        "sphere" => SolidShape::Sphere {
            radius: number(v, "radius")?.unwrap_or(0.1) as f32,
        },
        "capsule" => SolidShape::Capsule {
            half_height: number(v, "half_height")?.unwrap_or(0.1) as f32,
            radius: number(v, "radius")?.unwrap_or(0.05) as f32,
        },
        other => {
            return Err(bad_solid(&format!(
                "`shape` {other:?} is not box, sphere or capsule"
            )));
        }
    };
    let rotation = match v.get("rotation") {
        None | Some(Value::Null) => Quat::IDENTITY,
        Some(Value::Array(a)) if a.len() == 4 => {
            let mut q = [0.0f32; 4];
            for (slot, x) in q.iter_mut().zip(a) {
                *slot = x
                    .as_f64()
                    .filter(|x| x.is_finite() && x.abs() <= 2.0)
                    .ok_or_else(|| bad_solid("`rotation` must be four finite numbers [x, y, z, w]"))?
                    as f32;
            }
            Quat::from_array(q).normalize()
        }
        Some(_) => return Err(bad_solid("`rotation` must be a quaternion [x, y, z, w]")),
    };
    let motion = match string(v, "motion")?.unwrap_or("kinematic") {
        "kinematic" => SolidMotion::Kinematic,
        "dynamic" => SolidMotion::Dynamic,
        other => {
            return Err(bad_solid(&format!(
                "`motion` {other:?} is not kinematic or dynamic"
            )));
        }
    };
    let solid = MovingSolid {
        shape,
        position: center,
        rotation,
        linear_velocity: vec3(v, "velocity")?.unwrap_or(Vec3::ZERO),
        angular_velocity: vec3(v, "angular_velocity")?.unwrap_or(Vec3::ZERO),
        motion,
        tag: integer(v, "tag")?.unwrap_or(index as u64),
    };
    if !solid.is_valid() {
        return Err(bad_solid(
            "shape sizes must be in [0, 1000] m and speeds at most 1000 (m/s, rad/s)",
        ));
    }
    Ok(solid)
}

fn shape_json(shape: &SolidShape) -> Value {
    match *shape {
        SolidShape::Box { half_extents } => {
            json!({ "shape": "box", "half_extents": half_extents.to_array() })
        }
        SolidShape::Sphere { radius } => json!({ "shape": "sphere", "radius": radius }),
        SolidShape::Capsule { half_height, radius } => {
            json!({ "shape": "capsule", "half_height": half_height, "radius": radius })
        }
    }
}

fn motion_name(m: SolidMotion) -> &'static str {
    match m {
        SolidMotion::Kinematic => "kinematic",
        SolidMotion::Dynamic => "dynamic",
    }
}

/// `fluid.set_solids`.
pub fn set_solids(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_of(params)?;
    let list = match params.get("solids") {
        Some(Value::Array(a)) => a,
        _ => {
            return Err(bad(
                "fluid_bad_param",
                "`solids` (an array of { shape, center, ... }, empty to clear) is required",
            ));
        }
    };
    if list.len() > MAX_SOLIDS_PER_CALL {
        return Err(bad(
            "fluid_too_many_shapes",
            format!("at most {MAX_SOLIDS_PER_CALL} moving solids"),
        ));
    }
    let solids = list
        .iter()
        .enumerate()
        .map(|(i, v)| solid_from(i, v))
        .collect::<Result<Vec<_>, _>>()?;
    let Some(mut fluids) = world.get_resource_mut::<FluidWorld>() else {
        return Err(not_found(id));
    };
    let entry = fluids.get_mut(id).ok_or_else(|| not_found(id))?;
    let count = solids.len();
    entry.sim.set_moving_solids(solids).map_err(|e| fluid_error(&e))?;
    entry.revision += 1;
    Ok(json!({ "id": id, "solids": count }))
}

/// `fluid.solids`.
pub fn solids(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let id = id_of(params)?;
    let fluids = world.get_resource::<FluidWorld>().ok_or_else(|| not_found(id))?;
    let entry = fluids.get(id).ok_or_else(|| not_found(id))?;
    let impulses = entry.sim.solid_impulses();
    let list: Vec<Value> = entry
        .sim
        .moving_solids()
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let mut out = shape_json(&s.shape);
            out["index"] = json!(i);
            out["center"] = json!(s.position.to_array());
            out["rotation"] = json!(s.rotation.to_array());
            out["velocity"] = json!(s.linear_velocity.to_array());
            out["angular_velocity"] = json!(s.angular_velocity.to_array());
            out["motion"] = json!(motion_name(s.motion));
            out["tag"] = json!(s.tag);
            // The impulse of the last step, when the list has not changed since.
            let last = impulses.get(i).filter(|im| im.tag == s.tag);
            out["impulse"] = json!(last.map(|im| im.linear.to_array()));
            out["angular_impulse"] = json!(last.map(|im| im.angular.to_array()));
            out["wetted_faces"] = json!(last.map(|im| im.wetted_faces));
            out
        })
        .collect();
    let coupling = world.get_resource::<FluidCoupling>();
    let bodies: Vec<Value> = coupling
        .map(|c| {
            c.bodies
                .iter()
                .filter(|b| b.fluid == id)
                .map(|b| {
                    json!({
                        "entity": b.entity.to_bits(),
                        "motion": motion_name(b.motion),
                        "supported": b.solid.is_some(),
                        "two_way": b.two_way,
                        "simulated": b.simulated,
                        "applied": b.applied,
                        "impulse": b.impulse.to_array(),
                        "angular_impulse": b.angular_impulse.to_array(),
                        "wetted_faces": b.wetted_faces,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "id": id,
        "steps": entry.sim.steps(),
        "solids": list,
        "coupled_bodies": bodies,
        "coupling_installed": coupling.is_some(),
        "coupling_steps": coupling.map_or(0, |c| c.steps),
        "skipped_bodies": coupling.map_or(0, |c| c.skipped),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn world() -> World {
        World::new()
    }

    fn reason(e: &RpcError) -> Option<&str> {
        e.data.as_ref()?.get("reason")?.as_str()
    }

    /// Create, emit, step, stats, bake, play and destroy, end to end through the handlers.
    #[test]
    fn fluid_methods_round_trip() {
        let mut w = world();
        let created =
            create(&mut w, &json!({ "preset": "dam_break", "particle_cap": 4000 })).expect("create");
        let id = created["id"].as_u64().expect("id");
        assert_eq!(created["cells"], json!([64, 24, 4]));
        let added = add_emitter(
            &mut w,
            &json!({ "id": id, "kind": "sink", "shape": "box", "center": [1.5, 0.1, 0.05], "half_extents": [0.1, 0.1, 0.05] }),
        )
        .expect("sink");
        assert_eq!(added["kind"], "sink");
        let stepped = step(&mut w, &json!({ "id": id, "steps": 5 })).expect("step");
        assert_eq!(stepped["steps"], 5);
        assert!(stepped["particles"].as_u64().is_some_and(|n| n > 0 && n <= 4000));
        let s = stats(&mut w, &json!({ "id": id })).expect("stats");
        assert_eq!(s["digest"], stepped["digest"]);
        assert_eq!(s["sinks"], 1);
        let baked = bake(&mut w, &json!({ "id": id, "steps": 4 })).expect("bake");
        assert_eq!(baked["frames"], 5);
        let shown = play(&mut w, &json!({ "id": id, "frame": 2 })).expect("play");
        assert_eq!(shown["step"], 7);
        let past = play(&mut w, &json!({ "id": id, "frame": 99 })).expect_err("past the end");
        assert_eq!(reason(&past), Some("fluid_frame_out_of_range"));
        destroy(&mut w, &json!({ "id": id })).expect("destroy");
        let gone = stats(&mut w, &json!({ "id": id })).expect_err("gone");
        assert_eq!(reason(&gone), Some("fluid_not_found"));
    }

    /// `fluid.bake { path }` writes a `.fluidcache` into the project; `fluid.play { path }` on another
    /// simulation loads it and replays the same frame without stepping.
    #[test]
    fn fluid_bake_writes_and_play_loads_a_project_file() {
        let root = regia_test_utils::unique_temp_dir("agent-fluid-bake");
        let mut w = world();
        w.insert_resource(crate::AgentSettings {
            project_dir: Some(root.clone()),
        });
        let a =
            create(&mut w, &json!({ "preset": "dam_break", "particle_cap": 2000 })).expect("a")["id"].clone();
        step(&mut w, &json!({ "id": a, "steps": 3 })).expect("step");
        let baked = bake(
            &mut w,
            &json!({ "id": a, "steps": 3, "path": "fluid/dam.fluidcache" }),
        )
        .expect("bake");
        assert_eq!(baked["written"], true);
        let file = root.join("fluid/dam.fluidcache");
        let bytes = std::fs::read(&file).expect("cache file");
        assert_eq!(bytes.len() as u64, baked["bytes"].as_u64().expect("bytes"));
        let shown_a = play(&mut w, &json!({ "id": a, "frame": 3 })).expect("play a");
        let b = create(
            &mut w,
            &json!({ "bounds": { "min": [0, 0, 0], "max": [0.2, 0.2, 0.2] }, "cell_size": 0.1 }),
        )
        .expect("b")["id"]
            .clone();
        let shown_b = play(
            &mut w,
            &json!({ "id": b, "frame": 3, "path": "fluid/dam.fluidcache" }),
        )
        .expect("play b");
        for key in ["step", "liquid", "foam", "spray", "bubbles", "frames"] {
            assert_eq!(shown_a[key], shown_b[key], "{key}");
        }
        let stats_b = stats(&mut w, &json!({ "id": b })).expect("stats b");
        assert_eq!(stats_b["steps"], 0, "playback ran the solver");
        assert_eq!(stats_b["playing"], 3);
        std::fs::write(&file, &bytes[..bytes.len() / 2]).expect("truncate");
        let e = play(&mut w, &json!({ "id": b, "path": "fluid/dam.fluidcache" })).expect_err("truncated");
        assert_eq!(reason(&e), Some("fluidcache_truncated"));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The same preset twice gives the same digest (the agent sees the solver's determinism).
    #[test]
    fn fluid_digest_is_repeatable() {
        let mut w = world();
        let a =
            create(&mut w, &json!({ "preset": "splash", "particle_cap": 3000 })).expect("a")["id"].clone();
        let b =
            create(&mut w, &json!({ "preset": "splash", "particle_cap": 3000 })).expect("b")["id"].clone();
        let da = step(&mut w, &json!({ "id": a, "steps": 3 })).expect("a")["digest"].clone();
        let db = step(&mut w, &json!({ "id": b, "steps": 3 })).expect("b")["digest"].clone();
        assert_eq!(da, db);
    }

    /// Bad requests answer with their documented reasons.
    #[test]
    fn fluid_errors_have_reasons() {
        let mut w = world();
        let e = create(&mut w, &json!({})).expect_err("no bounds");
        assert_eq!(reason(&e), Some("fluid_bad_bounds"));
        let e = create(&mut w, &json!({ "preset": "lava" })).expect_err("preset");
        assert_eq!(reason(&e), Some("fluid_unknown_preset"));
        let e = create(
            &mut w,
            &json!({ "bounds": { "min": [0, 0, 0], "max": [100, 100, 100] }, "cell_size": 0.01 }),
        )
        .expect_err("huge");
        assert_eq!(reason(&e), Some("fluid_too_many_cells"));
        let e = step(&mut w, &json!({ "id": 9, "steps": 1 })).expect_err("missing");
        assert_eq!(reason(&e), Some("fluid_not_found"));
        let id = create(
            &mut w,
            &json!({ "bounds": { "min": [0, 0, 0], "max": [0.5, 0.5, 0.5] }, "cell_size": 0.1 }),
        )
        .expect("empty")["id"]
            .clone();
        let e = play(&mut w, &json!({ "id": id })).expect_err("no cache");
        assert_eq!(reason(&e), Some("fluid_no_cache"));
        let e = add_emitter(&mut w, &json!({ "id": id, "center": [0, 0, 0], "kind": "drain" }))
            .expect_err("kind");
        assert_eq!(reason(&e), Some("fluid_bad_param"));
        let e = bake(
            &mut w,
            &json!({ "id": id, "steps": 2, "path": "../out.fluidcache" }),
        )
        .expect_err("path");
        assert_eq!(reason(&e), Some("path_outside_project"));
    }

    /// `fluid.set_solids` puts kinematic solids into a simulation; a moving one shows its impulse in
    /// `fluid.solids` after a step, the stats count it, and bad solids answer their reasons.
    #[test]
    fn fluid_moving_solids_round_trip() {
        let mut w = world();
        let made = create(&mut w, &json!({ "preset": "dam_break", "whitewater": false })).expect("create");
        let id = made["id"].clone();
        let set = set_solids(
            &mut w,
            &json!({ "id": id, "solids": [
                { "shape": "box", "center": [0.3, 0.1, 0.05], "half_extents": [0.03, 0.08, 0.2], "velocity": [0.5, 0, 0] },
                { "shape": "capsule", "center": [1.0, 0.1, 0.05], "half_height": 0.05, "radius": 0.03, "motion": "dynamic" }
            ] }),
        )
        .expect("set");
        assert_eq!(set["solids"], 2);
        step(&mut w, &json!({ "id": id, "steps": 3 })).expect("step");
        let s = solids(&mut w, &json!({ "id": id })).expect("solids");
        assert_eq!(s["solids"].as_array().map(Vec::len), Some(2));
        assert_eq!(s["solids"][0]["shape"], "box");
        assert_eq!(s["solids"][1]["motion"], "dynamic");
        assert!(s["solids"][0]["impulse"].is_array(), "{s}");
        assert_eq!(s["coupling_installed"], false);
        // The box moved 3 steps at 0.5 m/s.
        let x = s["solids"][0]["center"][0].as_f64().unwrap_or(0.0);
        assert!((x - (0.3 + 0.5 * 3.0 / 60.0)).abs() < 1e-4, "{x}");
        let st = stats(&mut w, &json!({ "id": id })).expect("stats");
        assert_eq!(st["moving_solids"], 2);
        let e = set_solids(
            &mut w,
            &json!({ "id": id, "solids": [{ "shape": "cone", "center": [0, 0, 0] }] }),
        )
        .expect_err("cone");
        assert_eq!(reason(&e), Some("fluid_bad_solid"));
        let e = set_solids(
            &mut w,
            &json!({ "id": id, "solids": [{ "center": [0, 0, 0], "velocity": [1e5, 0, 0] }] }),
        )
        .expect_err("fast");
        assert_eq!(reason(&e), Some("fluid_bad_solid"));
        let e = set_solids(&mut w, &json!({ "id": id })).expect_err("missing");
        assert_eq!(reason(&e), Some("fluid_bad_param"));
        let e = set_solids(&mut w, &json!({ "id": 99, "solids": [] })).expect_err("gone");
        assert_eq!(reason(&e), Some("fluid_not_found"));
        // An empty list clears them.
        set_solids(&mut w, &json!({ "id": id, "solids": [] })).expect("clear");
        let s = solids(&mut w, &json!({ "id": id })).expect("solids");
        assert_eq!(s["solids"].as_array().map(Vec::len), Some(0));
    }
}
