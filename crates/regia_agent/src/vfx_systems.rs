// SPDX-License-Identifier: MPL-2.0
//! The generic particle-system methods (ADR-2642): what is loaded and alive (`vfx.systems`), what
//! a system is made of (`vfx.describe`), playing one from a name or a project asset (`vfx.spawn`)
//! and changing a live instance or a loaded emitter (`vfx.set_param`). `.vfx.ron` v7 (ADR-2800)
//! adds each emitter's shape, kill and render readout to `vfx.describe` and `vfx.spawn_probe`:
//! the spawn positions a seed gives, without spawning anything.
//!
//! Refusals are `F0002` with `data.reason` (`missing_param`, `bad_source`, `path_escapes`,
//! `asset_invalid`, `unknown_param`, `value_out_of_range`); a name or id that does not exist is
//! `F0003`; a world without [`regia_vfx::VfxPlugin`] is `F0016`. See `book/agents/vfx.md`.

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_vfx::VfxScene;
use regia_vfx::format::{
    EmitterDef, KillShape, KillSide, RenderMode, RibbonLink, RibbonTexture, ShapeDef, SpawnMode, VfxSystemDef,
};
use regia_vfx::scene::VfxLibrary;
use regia_vfx::sim::{LodInput, VfxInstance};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// GPU budget per live particle behind `gpu_ms_estimate`: spec 26 § 6 allows 2 ms for 100 000 on
/// Desktop High. It is the budget's rate, not a measurement of this frame.
const GPU_MS_PER_PARTICLE: f32 = 2.0 / 100_000.0;

const HINT: &str = "see book/agents/vfx.md";

/// `F0002` with `data.reason`.
fn refused(reason: &str, message: impl Into<String>) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, HINT);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn no_scene(method: &str) -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "F0016",
        "no VFX scene in this app",
        format!("add regia_vfx::VfxPlugin so {method} has a scene"),
    )
}

fn string_param<'a>(params: &'a Value, field: &str) -> Result<&'a str, RpcError> {
    params
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| refused("missing_param", format!("`{field}` (a string) is required")))
}

/// A finite `[x, y, z]`.
fn vec3_value(value: &Value, field: &str) -> Result<Vec3, RpcError> {
    let bad = || {
        refused(
            "value_out_of_range",
            format!("`{field}` must be three finite numbers"),
        )
    };
    let items = value.as_array().ok_or_else(bad)?;
    let [x, y, z] = items.as_slice() else {
        return Err(bad());
    };
    let number = |v: &Value| v.as_f64().map(|n| n as f32).ok_or_else(bad);
    let v = Vec3::new(number(x)?, number(y)?, number(z)?);
    if v.is_finite() { Ok(v) } else { Err(bad()) }
}

fn finite_number(value: &Value, field: &str) -> Result<f32, RpcError> {
    value
        .as_f64()
        .map(|n| n as f32)
        .filter(|n| n.is_finite())
        .ok_or_else(|| refused("value_out_of_range", format!("`{field}` must be a finite number")))
}

fn instance_json(instance: &VfxInstance, def: &VfxSystemDef) -> Value {
    let live = instance.live_particles();
    let (_, _, forward) = instance.axes();
    json!({
        "id": instance.id(),
        "system": def.name,
        "live": live,
        "budget": def.system.budget,
        "active": instance.is_active(),
        "age": instance.age(),
        "origin": instance.origin().to_array(),
        "forward": forward.to_array(),
        "scale": instance.scale(),
        "intensity": instance.intensity(),
        "lod_scale": instance.lod_scale(),
        "gpu_ms_estimate": live as f32 * GPU_MS_PER_PARTICLE,
    })
}

/// The GPU pool's counters, read back now (blocks on the device). `None` without a GPU context.
fn gpu_stats(world: &World) -> Option<Value> {
    let handle = world.get_resource::<regia_vfx::runtime::VfxGpuHandle>()?;
    let gpu = handle.lock()?;
    let stats = gpu.read_pool_stats_now()?;
    Some(json!({
        "alive": stats.alive,
        "dropped": stats.dropped,
        "events_fired": stats.events_fired,
        "events_overflow": stats.events_overflow,
        "event_capacity": gpu.event_capacity(),
        "children_spawned": stats.children_spawned,
        "children_dropped": stats.children_dropped,
        "child_capacity": gpu.child_capacity(),
    }))
}

/// `vfx.systems`: the loaded systems, the live instances with their particle counts and a cost
/// estimate, and with `gpu: true` the pool's counters read back from the GPU.
pub fn systems(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let with_gpu = match params.get("gpu") {
        None | Some(Value::Null) => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| refused("value_out_of_range", "`gpu` must be a boolean"))?,
    };
    let scene = world
        .get_resource::<VfxScene>()
        .ok_or_else(|| no_scene("vfx.systems"))?;
    let library = scene.library();
    let systems: Vec<Value> = library
        .defs()
        .iter()
        .enumerate()
        .map(|(index, def)| {
            let playing = || scene.instances().filter(move |i| i.def_index() == index as u32);
            json!({
                "name": def.name,
                "emitters": def.emitters.len(),
                "budget": def.system.budget,
                "has_events": def.has_events(),
                "instances": playing().count(),
                "live": playing().map(VfxInstance::live_particles).sum::<u32>(),
            })
        })
        .collect();
    let instances: Vec<Value> = scene
        .instances()
        .map(|i| instance_json(i, library.def(i.def_index())))
        .collect();
    let live = scene.estimated_live();
    let wind = scene.wind().to_array();
    let gpu = if with_gpu { gpu_stats(world) } else { None };
    Ok(json!({
        "systems": systems,
        "instances": instances,
        "live": live,
        "gpu_ms_estimate": live as f32 * GPU_MS_PER_PARTICLE,
        "wind": wind,
        "gpu": gpu,
    }))
}

/// `vfx.describe`: one loaded system: a summary per emitter and the whole definition.
pub fn describe(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = string_param(params, "system")?;
    let scene = world
        .get_resource::<VfxScene>()
        .ok_or_else(|| no_scene("vfx.describe"))?;
    let index = scene
        .library()
        .index(name)
        .ok_or_else(|| RpcError::not_found(format!("no vfx system `{name}` is loaded"), "use vfx.systems"))?;
    let def = scene.library().def(index);
    let emitters: Vec<Value> = def
        .emitters
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mode = match e.spawn.mode {
                SpawnMode::Rate(_) => "rate",
                SpawnMode::Burst(_) => "burst",
                SpawnMode::PerDistance { .. } => "per_distance",
                SpawnMode::Event => "event",
            };
            json!({
                "name": e.name,
                "spawn_mode": mode,
                "lifetime": [e.spawn.lifetime.min, e.spawn.lifetime.max],
                "wind_response": e.update.wind_response,
                "wind_drag": e.update.wind_drag,
                "drag": e.update.drag,
                "events": e.update.events.len(),
                "event_tail_s": def.event_tail(i),
                "shape": shape_json(scene.library(), e),
                "kill": kill_json(e),
                "render": render_json(scene, e),
            })
        })
        .collect();
    let full = serde_json::to_value(def).map_err(|err| RpcError::internal(err.to_string()))?;
    let mesh_draws = mesh_draws_json(world);
    Ok(json!({
        "name": def.name,
        "version": def.version,
        "budget": def.system.budget,
        "has_events": def.has_events(),
        "max_lifetime": def
            .emitters
            .iter()
            .map(|e| e.spawn.lifetime.max)
            .fold(0.0f32, f32::max),
        "emitters": emitters,
        "mesh_draws": mesh_draws,
        "definition": full,
    }))
}

/// An emitter's spawn shape (v7): its kind and, for a mesh shape, the mesh and whether it is
/// usable (`mesh_status`: `ok`, `missing_mesh` or `bad_mesh`).
fn shape_json(library: &VfxLibrary, e: &EmitterDef) -> Value {
    let kind = match &e.init.shape {
        ShapeDef::Point => "point",
        ShapeDef::Sphere { .. } => "sphere",
        ShapeDef::Box { .. } => "box",
        ShapeDef::Cone { .. } => "cone",
        ShapeDef::MeshSurface { .. } => "mesh_surface",
        ShapeDef::SkinnedMesh { .. } => "skinned_mesh",
    };
    let Some(mesh) = e.init.shape.mesh_name() else {
        return json!({ "kind": kind });
    };
    let max_per_frame = match &e.init.shape {
        ShapeDef::SkinnedMesh { max_per_frame, .. } => Some(*max_per_frame),
        _ => None,
    };
    match library.shape_mesh(&e.init.shape) {
        Ok(Some(found)) => json!({
            "kind": kind,
            "mesh": mesh,
            "mesh_status": "ok",
            "triangles": found.triangle_count(),
            "area": found.area(),
            "skinned": found.is_skinned(),
            "max_per_frame": max_per_frame,
        }),
        Ok(None) => json!({ "kind": kind, "mesh": mesh }),
        Err(err) => json!({
            "kind": kind,
            "mesh": mesh,
            "mesh_status": err.code(),
            "error": err.to_string(),
            "max_per_frame": max_per_frame,
        }),
    }
}

/// An emitter's kills (v7): the `kill_below` plane and the kill volume.
fn kill_json(e: &EmitterDef) -> Value {
    let volume = e.update.kill_volume.as_ref().map(|v| {
        let side = match v.kill {
            KillSide::Inside => "inside",
            KillSide::Outside => "outside",
        };
        let center = [v.center.0, v.center.1, v.center.2];
        match v.shape {
            KillShape::Box { half_extents: h } => {
                json!({ "shape": "box", "half_extents": [h.0, h.1, h.2], "center": center, "kill": side })
            }
            KillShape::Sphere { radius } => {
                json!({ "shape": "sphere", "radius": radius, "center": center, "kill": side })
            }
        }
    });
    json!({
        "below": e.update.kill_below,
        "volume": volume,
        "depth_collision": e.update.depth_collision.map(|d| if d.kill { "kill" } else { "slide" }),
    })
}

/// An emitter's render mode, with the ribbon settings for a ribbon (v7).
fn render_json(scene: &VfxScene, e: &EmitterDef) -> Value {
    let mode = match e.render.mode {
        RenderMode::Billboard => "billboard",
        RenderMode::Stretched => "stretched",
        RenderMode::Mesh => "mesh",
        RenderMode::Ribbon => "ribbon",
    };
    if e.render.mode == RenderMode::Mesh {
        return mesh_render_json(scene, e.render.mesh.as_deref(), e.render.lit);
    }
    if e.render.mode != RenderMode::Ribbon {
        return json!({ "mode": mode });
    }
    let r = &e.render.ribbon;
    let texture = match r.texture {
        RibbonTexture::None => json!("none"),
        RibbonTexture::Stretch => json!("stretch"),
        RibbonTexture::Tile(length) => json!({ "tile": length }),
    };
    json!({
        "mode": mode,
        "link": if r.link == RibbonLink::Emitter { "emitter" } else { "spawn_group" },
        "max_points": r.max_points,
        "texture": texture,
    })
}

/// A mesh particle's geometry (`.vfx.ron` v8 `render.mesh`): the built-in cube (`mesh_status`
/// `cube`, 36 indices), a registered mesh (`ok`, its triangles and indices) or a mesh that draws
/// nothing (`missing_mesh`: nothing registered under the name; `bad_mesh`: the GPU refused it at
/// the last upload), with the reason in `error`. `lit`: the mesh is shaded by the sun and
/// ambient (a `lit` emitter drawing a mesh asset, ADR-7261; the built-in cube is never lit).
fn mesh_render_json(scene: &VfxScene, mesh: Option<&str>, lit: bool) -> Value {
    let Some(name) = mesh else {
        return json!({ "mode": "mesh", "mesh": null, "mesh_status": "cube", "indices": 36, "lit": false });
    };
    if let Some((code, reason)) = scene.render_mesh_issues().get(name) {
        return json!({ "mode": "mesh", "mesh": name, "mesh_status": code, "error": reason, "lit": lit });
    }
    match scene.library().mesh(name) {
        Some(found) => json!({
            "mode": "mesh",
            "mesh": name,
            "mesh_status": "ok",
            "triangles": found.triangle_count(),
            "indices": found.triangle_count() * 3,
            "lit": lit,
        }),
        None => {
            let err = regia_vfx::mesh_shape::VfxMeshError::Missing(name.to_string());
            json!({ "mode": "mesh", "mesh": name, "mesh_status": err.code(), "error": err.to_string(), "lit": lit })
        }
    }
}

/// The GPU's mesh-asset draws of the last frame (ADR-7261): one per registered render mesh, with
/// its indices and the instances it covered (that mesh's live particles only). `None` without a
/// GPU context; empty with no registered render mesh.
fn mesh_draws_json(world: &World) -> Option<Value> {
    let handle = world.get_resource::<regia_vfx::runtime::VfxGpuHandle>()?;
    let gpu = handle.lock()?;
    let draws: Vec<Value> = gpu
        .read_render_mesh_draws()
        .into_iter()
        .map(|d| json!({ "mesh": d.mesh, "slot": d.slot, "indices": d.indices, "instances": d.instances }))
        .collect();
    Some(Value::Array(draws))
}

/// Most frames `vfx.spawn_probe` plays.
const PROBE_MAX_FRAMES: u64 = 600;
/// Most positions `vfx.spawn_probe` returns per emitter.
const PROBE_MAX_POSITIONS: u64 = 4096;

fn bounded(params: &Value, field: &str, default: u64, max: u64) -> Result<u64, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(default),
        Some(value) => value.as_u64().filter(|n| (1..=max).contains(n)).ok_or_else(|| {
            refused(
                "value_out_of_range",
                format!("`{field}` must be an integer in 1..={max}"),
            )
        }),
    }
}

/// `vfx.spawn_probe`: the spawn jobs `frames` frames of a loaded system give for `seed` at a
/// transform, sampled exactly as a spawned instance samples them (the library's meshes bound),
/// without spawning: per emitter, how many spawned, how many a mesh shape refused and why, and
/// the first `max` positions with their velocities.
pub fn spawn_probe(world: &World, params: &Value) -> Result<Value, RpcError> {
    let name = string_param(params, "system")?;
    let origin = match params.get("origin") {
        None | Some(Value::Null) => Vec3::ZERO,
        Some(value) => vec3_value(value, "origin")?,
    };
    let forward = match params.get("forward") {
        None | Some(Value::Null) => Vec3::Z,
        Some(value) => vec3_value(value, "forward")?,
    };
    if forward.length_squared() < 1e-8 {
        return Err(refused("value_out_of_range", "`forward` must not be zero"));
    }
    let seed = match params.get("seed") {
        None | Some(Value::Null) => 1,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| refused("value_out_of_range", "`seed` must be an unsigned integer"))?,
    };
    let frames = bounded(params, "frames", 1, PROBE_MAX_FRAMES)?;
    let max = bounded(params, "max", 256, PROBE_MAX_POSITIONS)? as usize;
    let scene = world
        .get_resource::<VfxScene>()
        .ok_or_else(|| no_scene("vfx.spawn_probe"))?;
    let library = scene.library();
    let index = library
        .index(name)
        .ok_or_else(|| RpcError::not_found(format!("no vfx system `{name}` is loaded"), "use vfx.systems"))?;
    let def = library.def(index);
    let mut instance = VfxInstance::new(index, seed, origin, forward);
    library.bind_meshes(&mut instance);
    let lod = LodInput { camera_pos: origin };
    let mut spawned = vec![0u64; def.emitters.len()];
    let mut positions: Vec<Vec<Value>> = vec![Vec::new(); def.emitters.len()];
    let mut jobs = Vec::new();
    for _ in 0..frames {
        instance.schedule(def, 1.0 / 60.0, &lod, u32::MAX);
        for (e, (count, list)) in spawned.iter_mut().zip(&mut positions).enumerate() {
            jobs.clear();
            instance.drain_emitter_jobs(e, &mut jobs);
            *count += jobs.len() as u64;
            for job in jobs.iter().take(max.saturating_sub(list.len())) {
                list.push(json!({ "position": job.position, "velocity": job.velocity }));
            }
        }
    }
    let emitters: Vec<Value> = def
        .emitters
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mesh_error = library.shape_mesh(&e.init.shape).err().map(|err| err.code());
            json!({
                "name": e.name,
                "spawned": spawned[i],
                "refused": instance.mesh_misses(i),
                "mesh_error": mesh_error,
                "shape": shape_json(library, e),
                "positions": std::mem::take(&mut positions[i]),
            })
        })
        .collect();
    Ok(json!({
        "system": def.name,
        "seed": seed,
        "frames": frames,
        "total": spawned.iter().sum::<u64>(),
        "emitters": emitters,
    }))
}

/// Whether `path` stays inside the project: relative, no `..`, no root or prefix.
fn confined(path: &str) -> bool {
    !path.is_empty()
        && std::path::Path::new(path)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Reads and parses a project-relative `.vfx.ron`, refusing what leaves the project (a symlink
/// included).
fn load_asset(world: &World, path: &str) -> Result<VfxSystemDef, RpcError> {
    if !confined(path) {
        return Err(refused(
            "path_escapes",
            format!("`{path}` must be relative to the project, without `..`"),
        ));
    }
    // ADR-8798: the shared root (`AgentSettings::project_dir` counts when no asset server is set).
    let root = crate::req_path::project_root(world);
    let missing = || RpcError::not_found(format!("cannot read `{path}`"), "check the path with assets.list");
    let resolved = root.join(path).canonicalize().map_err(|_| missing())?;
    let inside = root
        .canonicalize()
        .is_ok_and(|project| resolved.starts_with(project));
    if !inside {
        return Err(refused(
            "path_escapes",
            format!("`{path}` leads outside the project"),
        ));
    }
    let text = std::fs::read_to_string(resolved).map_err(|_| missing())?;
    VfxSystemDef::from_ron(&text).map_err(|err| refused("asset_invalid", format!("`{path}`: {err}")))
}

/// `vfx.spawn`: plays a loaded `system`, or loads the project-relative `asset` first (replacing a
/// loaded definition of the same name), at `origin` along `forward`.
pub fn spawn(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<VfxScene>().is_none() {
        return Err(no_scene("vfx.spawn"));
    }
    let origin = vec3_value(
        params
            .get("origin")
            .ok_or_else(|| refused("missing_param", "`origin` ([x, y, z]) is required"))?,
        "origin",
    )?;
    let forward = match params.get("forward") {
        None | Some(Value::Null) => Vec3::Z,
        Some(value) => vec3_value(value, "forward")?,
    };
    if forward.length_squared() < 1e-8 {
        return Err(refused("value_out_of_range", "`forward` must not be zero"));
    }
    let seed = match params.get("seed") {
        None | Some(Value::Null) => 1,
        Some(value) => value
            .as_u64()
            .ok_or_else(|| refused("value_out_of_range", "`seed` must be an unsigned integer"))?,
    };
    let scale = params
        .get("scale")
        .map(|v| finite_number(v, "scale"))
        .transpose()?;
    let intensity = params
        .get("intensity")
        .map(|v| finite_number(v, "intensity"))
        .transpose()?;
    let asset = crate::params::opt_str(params, "asset")?;
    let system = crate::params::opt_str(params, "system")?;
    let loaded = match (asset, system) {
        (Some(path), None) => Some(load_asset(world, path)?),
        (None, Some(_)) => None,
        _ => {
            return Err(refused("bad_source", "give exactly one of `asset` and `system`"));
        }
    };
    let mut scene = world
        .get_resource_mut::<VfxScene>()
        .ok_or_else(|| no_scene("vfx.spawn"))?;
    let name = match loaded {
        Some(def) => {
            let name = def.name.clone();
            scene
                .library_mut()
                .upsert(def)
                .map_err(|err| refused("asset_invalid", err.to_string()))?;
            name
        }
        None => system.unwrap_or_default().to_string(),
    };
    let loaded_name = scene.library().index(&scene.resolve(&name)).is_some();
    let id = scene.spawn(&name, origin, forward, seed).ok_or_else(|| {
        if loaded_name {
            refused(
                "refused_by_cap",
                format!("the per-system cap of `{name}` refused the spawn (vfx.significance)"),
            )
        } else {
            RpcError::not_found(format!("no vfx system `{name}` is loaded"), "use vfx.systems")
        }
    })?;
    if let Some(instance) = scene.instance_mut(id) {
        if let Some(scale) = scale {
            instance.set_scale(scale);
        }
        if let Some(intensity) = intensity {
            instance.set_intensity(intensity);
        }
    }
    Ok(json!({ "id": id, "system": name, "plays": scene.resolve(&name), "asset": asset }))
}

/// One live instance's parameter: `(before, after)`.
fn set_instance_param(
    scene: &mut VfxScene,
    id: u64,
    param: &str,
    value: &Value,
) -> Result<(Value, Value), RpcError> {
    let instance = scene
        .instance_mut(id)
        .ok_or_else(|| RpcError::not_found(format!("no live vfx instance {id}"), "use vfx.systems"))?;
    match param {
        "scale" => {
            let before = instance.scale();
            instance.set_scale(finite_number(value, param)?);
            Ok((json!(before), json!(instance.scale())))
        }
        "intensity" => {
            let before = instance.intensity();
            instance.set_intensity(finite_number(value, param)?);
            Ok((json!(before), json!(instance.intensity())))
        }
        "origin" => {
            let before = instance.origin();
            let (_, _, forward) = instance.axes();
            instance.set_transform(vec3_value(value, param)?, forward, 0.0);
            Ok((json!(before.to_array()), json!(instance.origin().to_array())))
        }
        "forward" => {
            let (_, _, before) = instance.axes();
            let next = vec3_value(value, param)?;
            if next.length_squared() < 1e-8 {
                return Err(refused("value_out_of_range", "`forward` must not be zero"));
            }
            instance.set_transform(instance.origin(), next, 0.0);
            let (_, _, after) = instance.axes();
            Ok((json!(before.to_array()), json!(after.to_array())))
        }
        other => Err(refused(
            "unknown_param",
            format!("instance parameter `{other}`: scale, intensity, origin or forward"),
        )),
    }
}

/// One loaded emitter's parameter: validates the changed definition, then replaces it (running
/// instances pick it up on their next update). `(before, after)`.
fn set_emitter_param(
    scene: &mut VfxScene,
    system: &str,
    emitter: &str,
    param: &str,
    value: &Value,
) -> Result<(Value, Value), RpcError> {
    let index = scene.library().index(system).ok_or_else(|| {
        RpcError::not_found(format!("no vfx system `{system}` is loaded"), "use vfx.systems")
    })?;
    let mut def = scene.library().def(index).clone();
    let target = def
        .emitters
        .iter_mut()
        .find(|e| e.name == emitter)
        .ok_or_else(|| {
            RpcError::not_found(
                format!("no emitter `{emitter}` in `{system}`"),
                "use vfx.describe",
            )
        })?;
    let next = finite_number(value, param)?;
    let field: &mut f32 = match param {
        "wind_response" => &mut target.update.wind_response,
        "wind_drag" => &mut target.update.wind_drag,
        "drag" => &mut target.update.drag,
        "count_scale" => &mut target.spawn.count_scale,
        "alpha_scale" => &mut target.render.alpha_scale,
        "emissive" => &mut target.render.emissive,
        other => {
            return Err(refused(
                "unknown_param",
                format!(
                    "emitter parameter `{other}`: wind_response, wind_drag, drag, count_scale, alpha_scale or emissive"
                ),
            ));
        }
    };
    let before = *field;
    *field = next;
    scene
        .library_mut()
        .upsert(def)
        .map_err(|err| refused("value_out_of_range", err.to_string()))?;
    Ok((json!(before), json!(next)))
}

/// `vfx.set_param`: changes a live `instance` (scale, intensity, origin, forward) or a loaded
/// `system` + `emitter` (wind_response, wind_drag, drag, count_scale, alpha_scale, emissive).
/// Returns `{ param, before, after }`.
pub fn set_param(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let param = string_param(params, "param")?.to_string();
    let value = params
        .get("value")
        .ok_or_else(|| refused("missing_param", "`value` is required"))?;
    let mut scene = world
        .get_resource_mut::<VfxScene>()
        .ok_or_else(|| no_scene("vfx.set_param"))?;
    let (before, after) = match (params.get("instance"), params.get("system")) {
        (Some(id), None) => {
            let id = id
                .as_u64()
                .ok_or_else(|| refused("value_out_of_range", "`instance` must be an id from vfx.systems"))?;
            set_instance_param(&mut scene, id, &param, value)?
        }
        (None, Some(system)) => {
            let system = system
                .as_str()
                .ok_or_else(|| refused("missing_param", "`system` must be a string"))?;
            let emitter = string_param(params, "emitter")?;
            set_emitter_param(&mut scene, system, emitter, &param, value)?
        }
        _ => {
            return Err(refused(
                "bad_source",
                "give exactly one of `instance` and `system`",
            ));
        }
    };
    Ok(json!({ "param": param, "before": before, "after": after }))
}

// -- Scene-wide significance (ADR-3275, book/agents/vfx.md "Scene budget and significance")

fn caps_json(caps: &regia_vfx::significance::SignificanceCaps) -> Value {
    json!({
        "active": caps.is_active(),
        "max_instances": caps.max_instances,
        "full_instances": caps.full_instances,
        "reduced_rate": caps.reduced_rate,
        "max_spawns_per_frame": caps.max_spawns_per_frame,
        "max_live": caps.max_live,
        "pause_offscreen": caps.pause_offscreen,
        "warmup_s": caps.warmup_s,
        "warmup_max_particles": caps.warmup_max_particles,
        "hysteresis": caps.hysteresis,
        "min_dwell_frames": caps.min_dwell_frames,
        "offscreen_margin": caps.offscreen_margin,
        "per_system": caps.per_system.iter().map(|c| json!({
            "system": c.system,
            "max_instances": c.max_instances,
            "steal": c.steal.name(),
        })).collect::<Vec<_>>(),
    })
}

/// Most ranked rows `vfx.significance` returns.
const SIGNIFICANCE_LIMIT_MAX: u64 = 1000;

/// `vfx.significance`: the caps, the last ranking's counts by state, refused and stolen spawns,
/// and the live instances most significant first (`limit` rows, default 64). Without caps the
/// list still shows each instance's score (`state` null: nothing is ranked).
pub fn significance(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let limit = match params.get("limit") {
        None | Some(Value::Null) => 64,
        Some(value) => value
            .as_u64()
            .filter(|n| (1..=SIGNIFICANCE_LIMIT_MAX).contains(n))
            .ok_or_else(|| refused("value_out_of_range", "`limit` must be an integer in 1..=1000"))?,
    };
    let scene = world
        .get_resource::<VfxScene>()
        .ok_or_else(|| no_scene("vfx.significance"))?;
    let sig = scene.significance();
    let stats = sig.stats();
    let enabled = sig.caps().is_active();
    let mut rows: Vec<(f32, u64, Value)> = scene
        .instances()
        .map(|instance| {
            let def = scene.library().def(instance.def_index());
            let (score_now, distance_now, visible_now) = sig.score_now(instance, def);
            let entry = enabled
                .then(|| sig.entries().find(|e| e.id == instance.id()))
                .flatten();
            let (score, distance, visible) = entry.map_or((score_now, distance_now, visible_now), |e| {
                (e.score, e.distance, e.visible)
            });
            let state = entry.and_then(|e| e.state).map(|s| s.name());
            let row = json!({
                "id": instance.id(),
                "system": def.name,
                "state": state,
                "finishing": !instance.is_active(),
                "score": score,
                "distance": distance,
                "visible": visible,
                "priority": sig.priority_of(instance.id(), &def.name),
                "age": instance.age(),
                "live": instance.live_particles(),
                "frames_in_state": entry.map(|e| e.frames_in_state),
            });
            (score, instance.id(), row)
        })
        .collect();
    rows.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let total = rows.len();
    let ranked: Vec<Value> = rows
        .into_iter()
        .take(limit as usize)
        .enumerate()
        .map(|(rank, (_, _, mut row))| {
            row["rank"] = json!(rank);
            row
        })
        .collect();
    Ok(json!({
        "enabled": enabled,
        "caps": caps_json(sig.caps()),
        "counts": {
            "full": stats.full,
            "reduced": stats.reduced,
            "paused": stats.paused,
            "culled": stats.culled,
            "finishing": stats.finishing,
        },
        "rejected_spawns": stats.rejected_spawns,
        "rejected_total": stats.rejected_total,
        "stolen_total": stats.stolen_total,
        "transitions": stats.transitions,
        "transitions_total": stats.transitions_total,
        "spawned": stats.spawned,
        "warmup_particles": stats.warmup_particles,
        "spawn_cap_reached": stats.spawn_cap_reached,
        "system_priority": sig.system_priorities().iter().map(|(name, p)| json!({ "system": name, "priority": p })).collect::<Vec<_>>(),
        "instance_priority": sig.instance_priorities().iter().map(|(id, p)| json!({ "instance": id, "priority": p })).collect::<Vec<_>>(),
        "instances": total,
        "ranked": ranked,
    }))
}

/// An optional cap: absent keeps it, `null` clears it, an integer `>= 0` sets it.
fn optional_cap(caps: &Value, field: &str, slot: &mut Option<u32>) -> Result<(), RpcError> {
    match caps.get(field) {
        None => Ok(()),
        Some(Value::Null) => {
            *slot = None;
            Ok(())
        }
        Some(value) => {
            let n = value
                .as_u64()
                .filter(|&n| n <= u64::from(u32::MAX))
                .ok_or_else(|| {
                    refused(
                        "value_out_of_range",
                        format!("`caps.{field}` must be an integer >= 0 or null"),
                    )
                })?;
            *slot = Some(n as u32);
            Ok(())
        }
    }
}

/// A number field of `caps` in `lo..=hi`, when present.
fn ranged_cap(caps: &Value, field: &str, lo: f32, hi: f32, slot: &mut f32) -> Result<(), RpcError> {
    if let Some(value) = caps.get(field) {
        let v = finite_number(value, field)?;
        if !(lo..=hi).contains(&v) {
            return Err(refused(
                "value_out_of_range",
                format!("`caps.{field}` must be in {lo}..={hi}"),
            ));
        }
        *slot = v;
    }
    Ok(())
}

/// The fields `caps` may hold.
const CAP_FIELDS: [&str; 11] = [
    "max_instances",
    "full_instances",
    "reduced_rate",
    "max_spawns_per_frame",
    "max_live",
    "pause_offscreen",
    "warmup_s",
    "warmup_max_particles",
    "hysteresis",
    "min_dwell_frames",
    "offscreen_margin",
];

fn apply_caps(caps: &mut regia_vfx::significance::SignificanceCaps, patch: &Value) -> Result<(), RpcError> {
    let object = patch
        .as_object()
        .ok_or_else(|| refused("value_out_of_range", "`caps` must be an object"))?;
    if let Some(unknown) = object.keys().find(|k| !CAP_FIELDS.contains(&k.as_str())) {
        return Err(refused("unknown_cap", format!("no cap named `{unknown}`")));
    }
    optional_cap(patch, "max_instances", &mut caps.max_instances)?;
    optional_cap(patch, "full_instances", &mut caps.full_instances)?;
    optional_cap(patch, "max_spawns_per_frame", &mut caps.max_spawns_per_frame)?;
    optional_cap(patch, "max_live", &mut caps.max_live)?;
    ranged_cap(patch, "reduced_rate", 0.0, 1.0, &mut caps.reduced_rate)?;
    ranged_cap(patch, "warmup_s", 0.0, 10.0, &mut caps.warmup_s)?;
    ranged_cap(patch, "hysteresis", 0.0, 10.0, &mut caps.hysteresis)?;
    ranged_cap(patch, "offscreen_margin", 0.0, 1000.0, &mut caps.offscreen_margin)?;
    for (field, slot) in [
        ("warmup_max_particles", &mut caps.warmup_max_particles),
        ("min_dwell_frames", &mut caps.min_dwell_frames),
    ] {
        if let Some(value) = patch.get(field) {
            *slot = value.as_u64().filter(|&n| n <= 100_000).ok_or_else(|| {
                refused(
                    "value_out_of_range",
                    format!("`caps.{field}` must be an integer in 0..=100000"),
                )
            })? as u32;
        }
    }
    if let Some(value) = patch.get("pause_offscreen") {
        caps.pause_offscreen = value
            .as_bool()
            .ok_or_else(|| refused("value_out_of_range", "`caps.pause_offscreen` must be a boolean"))?;
    }
    Ok(())
}

/// `vfx.significance_set`: applies, in this order, a `tier`'s caps (or `"off"`: no caps), a
/// `caps` patch, a `system_cap` (`{ system, max_instances | null, steal }`) and a `priority`
/// override (`{ system | instance, value | null }`). Returns `{ before, caps }`.
pub fn significance_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_vfx::significance::{SignificanceCaps, StealPolicy, SystemCap};
    let known = ["tier", "caps", "system_cap", "priority"];
    if !known.iter().any(|k| params.get(*k).is_some()) {
        return Err(refused(
            "missing_param",
            "give at least one of `tier`, `caps`, `system_cap`, `priority`",
        ));
    }
    let scene = world
        .get_resource::<VfxScene>()
        .ok_or_else(|| no_scene("vfx.significance_set"))?;
    let before = scene.significance().caps().clone();
    let mut caps = before.clone();
    if let Some(tier) = params.get("tier") {
        let name = tier
            .as_str()
            .ok_or_else(|| refused("unknown_tier", "`tier` must be a quality tier name or \"off\""))?;
        caps = if name == "off" {
            SignificanceCaps::default()
        } else {
            let tier = regia_render::QualityTier::from_name(name).ok_or_else(|| {
                refused(
                    "unknown_tier",
                    format!("no quality tier `{name}` (MobileLow .. Ultra, or off)"),
                )
            })?;
            regia_vfx::quality::significance_caps(tier)
        };
    }
    if let Some(patch) = params.get("caps") {
        apply_caps(&mut caps, patch)?;
    }
    if let Some(cap) = params.get("system_cap") {
        let system = string_param(cap, "system")?.to_string();
        match cap.get("max_instances") {
            Some(Value::Null) => {
                caps.remove_system_cap(&system);
            }
            Some(value) => {
                let max = value
                    .as_u64()
                    .filter(|&n| n <= u64::from(u32::MAX))
                    .ok_or_else(|| {
                        refused(
                            "value_out_of_range",
                            "`system_cap.max_instances` must be an integer >= 0 or null",
                        )
                    })?;
                let steal = match cap.get("steal") {
                    None | Some(Value::Null) => StealPolicy::default(),
                    Some(value) => value.as_str().and_then(StealPolicy::from_name).ok_or_else(|| {
                        refused(
                            "unknown_steal_policy",
                            "`system_cap.steal` must be reject, oldest or least_significant",
                        )
                    })?,
                };
                caps.set_system_cap(SystemCap {
                    system,
                    max_instances: max as u32,
                    steal,
                });
            }
            None => {
                return Err(refused(
                    "missing_param",
                    "`system_cap.max_instances` is required (null removes the cap)",
                ));
            }
        }
    }
    let priority = match params.get("priority") {
        None => None,
        Some(p) => {
            let value = match p.get("value") {
                None => {
                    return Err(refused(
                        "missing_param",
                        "`priority.value` is required (null removes it)",
                    ));
                }
                Some(Value::Null) => None,
                Some(v) => Some(finite_number(v, "priority.value")?)
                    .filter(|v| *v >= 0.0)
                    .map(Some)
                    .ok_or_else(|| {
                        refused(
                            "value_out_of_range",
                            "`priority.value` must be a finite number >= 0",
                        )
                    })?,
            };
            let target = match (p.get("system"), p.get("instance")) {
                (Some(system), None) => {
                    let system = system
                        .as_str()
                        .ok_or_else(|| refused("missing_param", "`priority.system` must be a string"))?;
                    if scene.library().index(system).is_none() {
                        return Err(RpcError::not_found(
                            format!("no vfx system `{system}` is loaded"),
                            "use vfx.systems",
                        ));
                    }
                    Ok(system.to_string())
                }
                (None, Some(id)) => {
                    let id = id.as_u64().ok_or_else(|| {
                        refused(
                            "value_out_of_range",
                            "`priority.instance` must be an id from vfx.systems",
                        )
                    })?;
                    if scene.instance(id).is_none() && value.is_some() {
                        return Err(RpcError::not_found(
                            format!("no live vfx instance {id}"),
                            "use vfx.systems",
                        ));
                    }
                    Err(id)
                }
                _ => {
                    return Err(refused(
                        "bad_source",
                        "give exactly one of `priority.system` and `priority.instance`",
                    ));
                }
            };
            Some((target, value))
        }
    };
    let mut scene = world
        .get_resource_mut::<VfxScene>()
        .ok_or_else(|| no_scene("vfx.significance_set"))?;
    let sig = scene.significance_mut();
    sig.set_caps(caps);
    match priority {
        Some((Ok(system), value)) => {
            sig.set_system_priority(&system, value);
        }
        Some((Err(id), value)) => {
            sig.set_instance_priority(id, value);
        }
        None => {}
    }
    Ok(json!({ "before": caps_json(&before), "caps": caps_json(sig.caps()) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_vfx::format::VFX_FORMAT_VERSION;
    use regia_vfx::scene::VfxLibrary;
    use regia_vfx::sim::LodInput;

    fn effect(name: &str) -> String {
        format!(
            r#"(
    version: {VFX_FORMAT_VERSION},
    name: "{name}",
    system: (budget: 500),
    emitters: [
        (
            name: "sparks",
            spawn: (mode: Rate(600.0), lifetime: (min: 1.0, max: 1.0)),
            init: (size: (min: 0.1, max: 0.2)),
            update: (wind_response: 0.5, events: [(on: Death, child: "embers", count: 2, probability: 0.5)]),
            render: (mode: Billboard),
        ),
        (
            name: "embers",
            spawn: (mode: Event, lifetime: (min: 0.5, max: 2.0)),
            init: (size: (min: 0.1, max: 0.2)),
            render: (mode: Billboard),
        ),
    ],
)"#
        )
    }

    fn app() -> App {
        let mut app = App::new();
        let mut library = VfxLibrary::new();
        library
            .add(VfxSystemDef::from_ron(&effect("sparks")).expect("effect"))
            .expect("add");
        app.insert_resource(VfxScene::new(library));
        app
    }

    fn step(app: &mut App, frames: u32) {
        let mut scene = app.world_mut().resource_mut::<VfxScene>();
        let lod = LodInput {
            camera_pos: Vec3::ZERO,
        };
        for _ in 0..frames {
            scene.update(1.0 / 60.0, &lod, 100_000);
        }
    }

    /// ADR-8798: `load_asset` reads `AgentSettings::project_dir` when no asset server is set,
    /// and a symlink in the project that leads out is `path_escapes`.
    #[test]
    fn load_asset_reads_the_agent_project_and_refuses_a_symlink_out() {
        let base = regia_test_utils::unique_temp_dir("vfx-load-root");
        let project = base.join("project");
        let outside = base.join("outside");
        std::fs::create_dir_all(&project).expect("project");
        std::fs::create_dir_all(&outside).expect("outside");
        let mut world = World::new();
        world.insert_resource(crate::AgentSettings {
            project_dir: Some(project.clone()),
        });
        let missing = load_asset(&world, "sparks.vfx.ron").expect_err("missing");
        assert!(missing.message.contains("cannot read"), "{missing:?}");
        std::fs::write(project.join("sparks.vfx.ron"), effect("sparks")).expect("asset");
        let loaded = load_asset(&world, "sparks.vfx.ron").expect("project asset");
        assert_eq!(loaded.name, "sparks");
        #[cfg(unix)]
        {
            std::fs::write(outside.join("secret.vfx.ron"), effect("secret")).expect("outside asset");
            std::os::unix::fs::symlink(&outside, project.join("escape")).expect("dir link");
            let error = load_asset(&world, "escape/secret.vfx.ron").expect_err("symlink out");
            assert_eq!(reason(&error), "path_escapes");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d["reason"].as_str())
            .unwrap_or_default()
            .to_string()
    }

    /// The brief's check: an agent spawn is listed by `vfx.systems` with its particle count, and
    /// the count reaches the system's row and the cost estimate.
    #[test]
    fn spawn_then_systems_lists_the_instance() {
        let mut app = app();
        let spawned = spawn(
            app.world_mut(),
            &json!({ "system": "sparks", "origin": [1.0, 2.0, 3.0], "forward": [0.0, 1.0, 0.0], "seed": 9, "scale": 2.0 }),
        )
        .expect("spawn");
        let id = spawned["id"].as_u64().expect("id");
        assert_eq!(spawned["system"], "sparks");
        step(&mut app, 30);
        let out = systems(app.world_mut(), &json!({})).expect("systems");
        assert_eq!(out["systems"][0]["name"], "sparks");
        assert_eq!(out["systems"][0]["instances"], 1);
        assert_eq!(out["systems"][0]["has_events"], true);
        let row = &out["instances"][0];
        assert_eq!(row["id"].as_u64(), Some(id));
        assert_eq!(row["origin"], json!([1.0, 2.0, 3.0]));
        assert_eq!(row["scale"], 2.0);
        let live = row["live"].as_u64().expect("live");
        assert!(live >= 290, "half a second at 600/s: {live}");
        assert_eq!(out["live"].as_u64(), Some(live));
        assert_eq!(out["systems"][0]["live"].as_u64(), Some(live));
        assert!(row["gpu_ms_estimate"].as_f64().expect("cost") > 0.0);
        assert!(out["gpu"].is_null(), "no GPU context and not asked");
    }

    /// A v7 system: a mesh-surface emitter with a kill volume, a skinned emitter whose mesh is
    /// not registered, and a ribbon.
    fn v7_app() -> App {
        let text = format!(
            r#"(
    version: {VFX_FORMAT_VERSION},
    name: "shapes",
    emitters: [
        (
            name: "surface",
            spawn: (mode: Burst(12), lifetime: (min: 1.0, max: 1.0)),
            init: (shape: MeshSurface(mesh: "quad"), velocity: (mode: Normal(speed: (min: 1.0, max: 1.0))), size: (min: 0.1, max: 0.1)),
            update: (kill_below: Some(-1.0), kill_volume: Some((shape: Sphere(radius: 3.0), kill: Outside))),
            render: (mode: Billboard),
        ),
        (
            name: "skin",
            spawn: (mode: Rate(60.0), lifetime: (min: 1.0, max: 1.0)),
            init: (shape: SkinnedMesh(mesh: "hero", max_per_frame: 8), size: (min: 0.1, max: 0.1)),
            render: (mode: Billboard),
        ),
        (
            name: "trail",
            spawn: (mode: Rate(30.0), lifetime: (min: 1.0, max: 1.0)),
            init: (size: (min: 0.1, max: 0.1)),
            render: (mode: Ribbon, ribbon: (link: SpawnGroup, max_points: 32, texture: Tile(0.5))),
        ),
        (
            name: "cube",
            spawn: (mode: Burst(2), lifetime: (min: 1.0, max: 1.0)),
            init: (size: (min: 0.1, max: 0.1)),
            render: (mode: Mesh),
        ),
        (
            name: "rock",
            spawn: (mode: Burst(2), lifetime: (min: 1.0, max: 1.0)),
            init: (size: (min: 0.1, max: 0.1)),
            render: (mode: Mesh, lit: true, mesh: Some("quad")),
        ),
        (
            name: "lost",
            spawn: (mode: Burst(2), lifetime: (min: 1.0, max: 1.0)),
            init: (size: (min: 0.1, max: 0.1)),
            render: (mode: Mesh, mesh: Some("nowhere")),
        ),
    ],
)"#
        );
        let mut library = VfxLibrary::new();
        library.add_mesh(
            "quad",
            regia_vfx::mesh_shape::VfxMesh::from_triangles(
                &[[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [1.0, 1.0, 0.0], [0.0, 1.0, 0.0]],
                &[0, 1, 2, 0, 2, 3],
            )
            .expect("quad"),
        );
        library
            .add(VfxSystemDef::from_ron(&text).expect("v7 effect"))
            .expect("add");
        let mut app = App::new();
        app.insert_resource(VfxScene::new(library));
        app
    }

    #[test]
    fn describe_reads_out_shapes_kills_and_ribbons() {
        let mut app = v7_app();
        let out = describe(app.world_mut(), &json!({ "system": "shapes" })).expect("describe");
        let surface = &out["emitters"][0];
        assert_eq!(surface["shape"]["kind"], "mesh_surface");
        assert_eq!(surface["shape"]["mesh_status"], "ok");
        assert_eq!(surface["shape"]["triangles"], 2);
        assert_eq!(surface["kill"]["below"], -1.0);
        assert_eq!(surface["kill"]["volume"]["shape"], "sphere");
        assert_eq!(surface["kill"]["volume"]["kill"], "outside");
        let skin = &out["emitters"][1];
        assert_eq!(skin["shape"]["mesh_status"], "missing_mesh");
        assert_eq!(skin["shape"]["max_per_frame"], 8);
        let trail = &out["emitters"][2]["render"];
        assert_eq!(
            (trail["mode"].as_str(), trail["link"].as_str()),
            (Some("ribbon"), Some("spawn_group"))
        );
        assert_eq!(trail["texture"]["tile"], 0.5);
        // `.vfx.ron` v8 render meshes (ADR-7210): the cube, a registered mesh, a missing one.
        let cube = &out["emitters"][3]["render"];
        assert_eq!(
            (cube["mesh_status"].as_str(), cube["indices"].as_u64()),
            (Some("cube"), Some(36))
        );
        assert_eq!(cube["lit"], false, "the built-in cube is never lit");
        let rock = &out["emitters"][4]["render"];
        assert_eq!(rock["mesh"], "quad");
        assert_eq!(
            rock["lit"], true,
            "a lit emitter's mesh asset is shaded (ADR-7261)"
        );
        assert_eq!(
            (rock["mesh_status"].as_str(), rock["indices"].as_u64()),
            (Some("ok"), Some(6))
        );
        let lost = &out["emitters"][5]["render"];
        assert_eq!(lost["mesh_status"], "missing_mesh");
        assert_eq!(lost["lit"], false);
        assert_eq!(out["mesh_draws"], Value::Null, "no GPU context, no draws");
        assert!(
            lost["error"].as_str().is_some_and(|e| e.contains("nowhere")),
            "{lost}"
        );
    }

    #[test]
    fn spawn_probe_lists_positions_for_a_seed_without_spawning() {
        let mut app = v7_app();
        let live_id = app
            .world_mut()
            .resource_mut::<VfxScene>()
            .spawn("shapes", Vec3::ZERO, Vec3::Z, 99)
            .expect("an existing live instance");
        let entities_before = app.world().entities().len();
        let systems_before = systems(app.world_mut(), &json!({})).expect("systems before");
        let params =
            json!({ "system": "shapes", "seed": 4, "origin": [10.0, 0.0, 0.0], "frames": 3, "max": 5 });
        let out = spawn_probe(app.world(), &params).expect("probe");
        let again = spawn_probe(app.world(), &params).expect("probe again");
        assert_eq!(out, again, "same seed, same positions");
        assert_eq!(app.world().entities().len(), entities_before);
        assert_eq!(
            systems(app.world_mut(), &json!({})).expect("systems after"),
            systems_before,
            "probing changes no observable VFX state"
        );
        let surface = &out["emitters"][0];
        assert_eq!(surface["spawned"], 12);
        assert_eq!(surface["positions"].as_array().map(Vec::len), Some(5));
        for row in surface["positions"].as_array().expect("rows") {
            let p = &row["position"];
            let x = p[0].as_f64().expect("x") - 10.0;
            assert!(
                (-1.0..=0.0).contains(&x) && p[2].as_f64() == Some(0.0),
                "{p} is on the quad"
            );
            assert_eq!(row["velocity"], json!([0.0, 0.0, 1.0]), "along the face normal");
        }
        let skin = &out["emitters"][1];
        assert_eq!(
            (skin["spawned"].as_u64(), skin["mesh_error"].as_str()),
            (Some(0), Some("missing_mesh"))
        );
        assert!(skin["refused"].as_u64().expect("refused") >= 2);
        assert_eq!(
            app.world().resource::<VfxScene>().live_instances(),
            1,
            "only the existing instance remains"
        );
        assert_eq!(
            app.world_mut()
                .resource_mut::<VfxScene>()
                .spawn("shapes", Vec3::ZERO, Vec3::Z, 100),
            Some(live_id + 1),
            "probing consumes no live instance ids"
        );
        for (params, want) in [
            (json!({ "system": "shapes", "frames": 0 }), "value_out_of_range"),
            (json!({ "system": "shapes", "max": 5000 }), "value_out_of_range"),
            (
                json!({ "system": "shapes", "forward": [0.0, 0.0, 0.0] }),
                "value_out_of_range",
            ),
            (json!({}), "missing_param"),
        ] {
            let err = spawn_probe(app.world(), &params).expect_err("refused");
            assert_eq!(reason(&err), want, "{params}");
        }
        let err = spawn_probe(app.world(), &json!({ "system": "nope" })).expect_err("unknown");
        assert_eq!(err.code, protocol::NOT_FOUND);
    }

    #[test]
    fn describe_reports_emitters_and_the_definition() {
        let mut app = app();
        let out = describe(app.world_mut(), &json!({ "system": "sparks" })).expect("describe");
        assert_eq!(out["version"].as_u64(), Some(u64::from(VFX_FORMAT_VERSION)));
        assert_eq!(out["emitters"][0]["spawn_mode"], "rate");
        assert_eq!(out["emitters"][1]["spawn_mode"], "event");
        assert_eq!(out["emitters"][0]["wind_response"], 0.5);
        assert_eq!(out["emitters"][0]["event_tail_s"], 2.0);
        assert_eq!(
            out["definition"]["emitters"][0]["update"]["events"][0]["count"],
            2
        );
        let err = describe(app.world_mut(), &json!({ "system": "nope" })).expect_err("unknown");
        assert_eq!(err.code, protocol::NOT_FOUND);
    }

    /// A project asset loads and plays by path; a path that leaves the project and a broken file
    /// are refused with their reasons.
    #[test]
    fn spawn_loads_a_project_asset_and_refuses_what_it_should() {
        let dir = regia_test_utils::unique_temp_dir("vfx-agent-spawn");
        std::fs::write(dir.join("fx.vfx.ron"), effect("from_disk")).expect("write");
        std::fs::write(dir.join("bad.vfx.ron"), "( version: 1, name: ").expect("write");
        let mut app = app();
        let server = regia_asset::AssetServer::open(regia_asset::AssetServerSettings {
            project_root: dir.clone(),
            platform: regia_asset_pipeline::host_platform().to_string(),
            content_dir: None,
        })
        .expect("opens");
        app.insert_resource(server);
        let out = spawn(
            app.world_mut(),
            &json!({ "asset": "fx.vfx.ron", "origin": [0.0, 0.0, 0.0], "intensity": 0.5 }),
        )
        .expect("spawn from disk");
        assert_eq!(out["system"], "from_disk");
        let listed = systems(app.world_mut(), &json!({})).expect("systems");
        assert_eq!(listed["instances"][0]["system"], "from_disk");
        assert_eq!(listed["instances"][0]["intensity"], 0.5);

        let origin = json!([0.0, 0.0, 0.0]);
        for (params, want) in [
            (
                json!({ "asset": "../fx.vfx.ron", "origin": origin }),
                "path_escapes",
            ),
            (
                json!({ "asset": "/etc/passwd", "origin": origin }),
                "path_escapes",
            ),
            (
                json!({ "asset": "bad.vfx.ron", "origin": origin }),
                "asset_invalid",
            ),
            (json!({ "origin": origin }), "bad_source"),
            (
                json!({ "asset": "fx.vfx.ron", "system": "x", "origin": origin }),
                "bad_source",
            ),
            (json!({ "system": "sparks" }), "missing_param"),
            (
                json!({ "system": "sparks", "origin": [0.0, 1.0] }),
                "value_out_of_range",
            ),
            (
                json!({ "system": "sparks", "origin": origin, "forward": [0.0, 0.0, 0.0] }),
                "value_out_of_range",
            ),
        ] {
            let err = spawn(app.world_mut(), &params).expect_err("refused");
            assert_eq!(reason(&err), want, "{params}");
        }
        let err = spawn(
            app.world_mut(),
            &json!({ "asset": "missing.vfx.ron", "origin": origin }),
        )
        .expect_err("missing");
        assert_eq!(err.code, protocol::NOT_FOUND);
        let err =
            spawn(app.world_mut(), &json!({ "system": "nope", "origin": origin })).expect_err("unknown");
        assert_eq!(err.code, protocol::NOT_FOUND);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn set_param_changes_instances_and_emitters() {
        let mut app = app();
        let spawned = spawn(
            app.world_mut(),
            &json!({ "system": "sparks", "origin": [0.0, 0.0, 0.0] }),
        )
        .expect("spawn");
        let id = spawned["id"].clone();
        let out = set_param(
            app.world_mut(),
            &json!({ "instance": id, "param": "intensity", "value": 0.25 }),
        )
        .expect("intensity");
        assert_eq!(
            (out["before"].as_f64(), out["after"].as_f64()),
            (Some(1.0), Some(0.25))
        );
        let out = set_param(
            app.world_mut(),
            &json!({ "instance": id, "param": "origin", "value": [4.0, 5.0, 6.0] }),
        )
        .expect("origin");
        assert_eq!(out["after"], json!([4.0, 5.0, 6.0]));

        let out = set_param(
            app.world_mut(),
            &json!({ "system": "sparks", "emitter": "sparks", "param": "wind_response", "value": 1.0 }),
        )
        .expect("wind");
        assert_eq!(
            (out["before"].as_f64(), out["after"].as_f64()),
            (Some(0.5), Some(1.0))
        );
        let read = describe(app.world_mut(), &json!({ "system": "sparks" })).expect("describe");
        assert_eq!(read["emitters"][0]["wind_response"], 1.0);

        for (params, want) in [
            (
                json!({ "system": "sparks", "emitter": "sparks", "param": "wind_response", "value": 3.0 }),
                "value_out_of_range",
            ),
            (
                json!({ "system": "sparks", "emitter": "sparks", "param": "sparkle", "value": 1.0 }),
                "unknown_param",
            ),
            (
                json!({ "instance": id, "param": "sparkle", "value": 1.0 }),
                "unknown_param",
            ),
            (json!({ "param": "scale", "value": 1.0 }), "bad_source"),
            (json!({ "instance": id, "param": "scale" }), "missing_param"),
            // deep-zai-vfx findings 1 and 2: the upsert re-runs the loader's validation, so a
            // negative scale and an event child whose worst-case scaled count exceeds the packed
            // 24-bit link field are refused here too, not just at file load.
            (
                json!({ "system": "sparks", "emitter": "sparks", "param": "drag", "value": -0.5 }),
                "value_out_of_range",
            ),
            (
                json!({ "system": "sparks", "emitter": "embers", "param": "count_scale", "value": -1.0 }),
                "value_out_of_range",
            ),
            (
                json!({ "system": "sparks", "emitter": "embers", "param": "count_scale", "value": 1e9 }),
                "value_out_of_range",
            ),
        ] {
            let err = set_param(app.world_mut(), &params).expect_err("refused");
            assert_eq!(reason(&err), want, "{params}");
        }
        // A refused value left the definition as it was.
        let read = describe(app.world_mut(), &json!({ "system": "sparks" })).expect("describe");
        assert_eq!(read["emitters"][0]["wind_response"], 1.0);
        // A count_scale the loader accepts still applies to the event child.
        let out = set_param(
            app.world_mut(),
            &json!({ "system": "sparks", "emitter": "embers", "param": "count_scale", "value": 2.0 }),
        )
        .expect("legal count_scale");
        assert_eq!(
            (out["before"].as_f64(), out["after"].as_f64()),
            (Some(1.0), Some(2.0))
        );
        let err = set_param(
            app.world_mut(),
            &json!({ "instance": 999, "param": "scale", "value": 1.0 }),
        )
        .expect_err("gone");
        assert_eq!(err.code, protocol::NOT_FOUND);
    }

    #[test]
    fn a_world_without_the_plugin_answers_unsupported() {
        let mut bare = App::new();
        for out in [
            systems(bare.world_mut(), &json!({})),
            describe(bare.world_mut(), &json!({ "system": "x" })),
            spawn(
                bare.world_mut(),
                &json!({ "system": "x", "origin": [0.0, 0.0, 0.0] }),
            ),
            set_param(
                bare.world_mut(),
                &json!({ "instance": 1, "param": "scale", "value": 1.0 }),
            ),
        ] {
            assert_eq!(out.expect_err("no scene").code, protocol::UNSUPPORTED);
        }
    }

    /// `vfx.significance_set` applies a tier and a priority; `vfx.significance` reports the caps,
    /// the counts and the ranked list, most significant first, with the override on top.
    #[test]
    fn significance_set_then_significance_ranks_the_scene() {
        let mut app = app();
        let off = significance(app.world_mut(), &json!({})).expect("report");
        assert_eq!(off["enabled"], false);
        let mut ids = Vec::new();
        for i in 0..40u32 {
            let out = spawn(
                app.world_mut(),
                &json!({ "system": "sparks", "origin": [0.0, 0.0, -(2.0 + i as f32)], "seed": i }),
            )
            .expect("spawn");
            ids.push(out["id"].as_u64().expect("id"));
        }
        // Without caps the list still ranks by score, nothing has a state.
        let off = significance(app.world_mut(), &json!({ "limit": 3 })).expect("report");
        assert_eq!(off["ranked"].as_array().map(Vec::len), Some(3));
        assert_eq!(off["ranked"][0]["id"].as_u64(), Some(ids[0]));
        assert!(off["ranked"][0]["state"].is_null());

        let set = significance_set(
            app.world_mut(),
            &json!({ "tier": "Low", "priority": { "instance": ids[39], "value": 1000.0 } }),
        )
        .expect("set");
        assert_eq!(set["before"]["active"], false);
        assert_eq!(set["caps"]["max_instances"], 32);
        step(&mut app, 4);
        let out = significance(app.world_mut(), &json!({ "limit": 1000 })).expect("report");
        assert_eq!(out["enabled"], true);
        assert_eq!(
            out["counts"]["full"].as_u64().unwrap_or(0) + out["counts"]["reduced"].as_u64().unwrap_or(0),
            32
        );
        assert_eq!(out["counts"]["culled"], 8);
        assert_eq!(
            out["ranked"][0]["id"].as_u64(),
            Some(ids[39]),
            "the override ranks first"
        );
        assert_eq!(out["ranked"][0]["state"], "full");
        assert_eq!(out["ranked"][39]["state"], "culled");
        assert_eq!(out["instance_priority"][0]["priority"], 1000.0);

        // A per-system cap with a steal policy, then a refused spawn through vfx.spawn.
        significance_set(
            app.world_mut(),
            &json!({ "system_cap": { "system": "sparks", "max_instances": 40, "steal": "reject" } }),
        )
        .expect("cap");
        let err = spawn(
            app.world_mut(),
            &json!({ "system": "sparks", "origin": [0.0, 0.0, 0.0] }),
        )
        .expect_err("over the cap");
        assert_eq!(reason(&err), "refused_by_cap");
        step(&mut app, 1);
        let out = significance(app.world_mut(), &json!({})).expect("report");
        assert_eq!(out["rejected_spawns"], 1);
        assert_eq!(out["caps"]["per_system"][0]["steal"], "reject");

        let off = significance_set(app.world_mut(), &json!({ "tier": "off" })).expect("off");
        assert_eq!(off["caps"]["active"], false);
    }

    #[test]
    fn significance_set_refuses_what_it_should() {
        let mut app = app();
        for (params, want) in [
            (json!({}), "missing_param"),
            (json!({ "tier": "Potato" }), "unknown_tier"),
            (json!({ "tier": 3 }), "unknown_tier"),
            (json!({ "caps": { "sparkle": 1 } }), "unknown_cap"),
            (json!({ "caps": { "reduced_rate": 2.0 } }), "value_out_of_range"),
            (json!({ "caps": { "max_instances": -1 } }), "value_out_of_range"),
            (json!({ "caps": { "pause_offscreen": 1 } }), "value_out_of_range"),
            (json!({ "caps": 5 }), "value_out_of_range"),
            (
                json!({ "system_cap": { "system": "sparks", "max_instances": 2, "steal": "newest" } }),
                "unknown_steal_policy",
            ),
            (json!({ "system_cap": { "system": "sparks" } }), "missing_param"),
            (json!({ "priority": { "system": "sparks" } }), "missing_param"),
            (json!({ "priority": { "value": 2.0 } }), "bad_source"),
            (
                json!({ "priority": { "system": "sparks", "value": -2.0 } }),
                "value_out_of_range",
            ),
        ] {
            let err = significance_set(app.world_mut(), &params).expect_err("refused");
            assert_eq!(reason(&err), want, "{params}");
        }
        let err = significance(app.world_mut(), &json!({ "limit": 0 })).expect_err("limit");
        assert_eq!(reason(&err), "value_out_of_range");
        let err = significance_set(
            app.world_mut(),
            &json!({ "priority": { "system": "nope", "value": 2.0 } }),
        )
        .expect_err("unknown system");
        assert_eq!(err.code, protocol::NOT_FOUND);
        let err = significance_set(
            app.world_mut(),
            &json!({ "priority": { "instance": 999, "value": 2.0 } }),
        )
        .expect_err("unknown instance");
        assert_eq!(err.code, protocol::NOT_FOUND);
        // Nothing refused changed the caps.
        let out = significance(app.world_mut(), &json!({})).expect("report");
        assert_eq!(out["caps"]["active"], false);
        let mut bare = App::new();
        for out in [
            significance(bare.world_mut(), &json!({})),
            significance_set(bare.world_mut(), &json!({ "tier": "Low" })),
        ] {
            assert_eq!(out.expect_err("no scene").code, protocol::UNSUPPORTED);
        }
    }
}
