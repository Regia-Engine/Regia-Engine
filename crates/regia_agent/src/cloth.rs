// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface of the XPBD cloth solver (`docs/specs/23-agent-native.md`,
//! `docs/specs/55-cloth-and-soft-body.md` § 5, `regia_physics::cloth`).
//!
//! - `physics.cloth_stats` reports every solver in the world's [`ClothSet`] — counts, step
//!   totals, contacts, stretch, bounds and a state digest — so an agent can check that cloth
//!   settled, touched a collider or stayed deterministic without reading particle arrays.
//! - `physics.cloth_inspect` adds the spec 55 state (lifecycle, blend, LOD, self-collision,
//!   tearing, wind, pose error; volume for soft bodies) and an optional particle slice.
//! - `physics.cloth_set_params`, `physics.cloth_pin`, `physics.cloth_unpin`,
//!   `physics.cloth_wind`, `physics.cloth_step` and `physics.cloth_reset` drive a solver (class
//!   `play`).
//! - `physics.cloth_set_layers` puts particles on garment layers for one-sided self-collision
//!   (class `play`, ADR-2425).
//!
//! Targets are `{ name }` of a [`ClothSet`] solver or a [`SoftSet`] soft body. The methods are
//! extension methods: a host that adds `ClothPlugin` calls [`install_cloth_methods`] once to
//! register them on the world's [`AgentExtension`]. They are documented on
//! `book/agents/cloth.md` and `book/agents/soft-bodies.md`.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_physics::cloth::{
    ClothError, ClothPreset, ClothSet, ClothSolver, ClothTuning, Gust, Plasticity, SelfCollision, SoftSet,
    SoftSolver, StrainCap, TearParams, UnknownPreset, WindModel,
};

use regia_framework::cloth::{ClothGlueState, ClothOutput, ClothSim, ClothSourceState, SoftSim};
use regia_physics::cloth::maps::{MapEdit, apply_map_edit};

use crate::extension::AgentExtension;
use crate::protocol::{self, RpcError};
use crate::schema::{MethodSchema, ParamSchema, Permission};

/// Most particles `physics.cloth_inspect` returns in one slice.
pub const MAX_PARTICLE_SLICE: usize = 256;
/// Most particles one `physics.cloth_pin` / `physics.cloth_unpin` call names.
pub const MAX_PIN_LIST: usize = 1024;
/// Most fixed steps one `physics.cloth_step` call runs.
pub const MAX_STEP_COUNT: u64 = 600;

/// The `physics.cloth_stats` schema row.
fn cloth_stats_schema() -> MethodSchema {
    MethodSchema {
        name: "physics.cloth_stats",
        tool: "physics_cloth_stats",
        description: "Cloth and rope solvers: particle and constraint counts, steps, contacts, max stretch, bounds, digest.",
        params: &[ParamSchema {
            name: "name",
            kind: "string",
            required: false,
            description: "report only the solver with this name (F0003 when there is none)",
        }],
        result: "{ plugin, solvers: [{ name, particles, pinned, distance_constraints, bend_constraints, colliders, steps, dropped_steps, contacts, max_stretch, bounds_min, bounds_max, digest }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

const TARGET: ParamSchema = ParamSchema {
    name: "target",
    kind: "object",
    required: true,
    description: "{ name } of a cloth solver or soft body (F0003 cloth_not_found when there is none)",
};

const TARGET_OPTIONAL: ParamSchema = ParamSchema {
    name: "target",
    kind: "object",
    required: false,
    description: "{ name } of a cloth solver or soft body; absent = every one",
};

fn play_schema(
    name: &'static str,
    tool: &'static str,
    description: &'static str,
    params: &'static [ParamSchema],
    result: &'static str,
) -> MethodSchema {
    MethodSchema {
        name,
        tool,
        description,
        params,
        result,
        mutating: true,
        permission: Permission::Play,
    }
}

fn cloth_inspect_schema() -> MethodSchema {
    MethodSchema {
        name: "physics.cloth_inspect",
        tool: "physics_cloth_inspect",
        description: "Cloth and soft-body solvers in depth: cloth_stats fields plus state, blend, LOD, self-collision, tearing, wind (own, global with the scene wind, and per instance the scene wind it received), pose error, volume, and a particle slice.",
        params: &[
            TARGET_OPTIONAL,
            ParamSchema {
                name: "particles",
                kind: "object",
                required: false,
                description: "{ from, count <= 256 }: include these particles' position, velocity and inv_mass",
            },
        ],
        result: "{ plugin, parallel, scene_wind: null | { mode, velocity, cloth }, solvers: [{ name, kind, ...cloth_stats fields, state, blend, lod, dropped_self_pairs, self_contacts, self_crossings, max_layer, layered, layer_gap, torn, wind, global_wind, max_pose_error, teleports, tethers, volume?, rest_volume?, pressure?, inverted_tets?, particles?, instance?: { entity, asset, section, section_vertices, section_triangles, hidden_triangles, scene_wind, error } }], failed: [{ entity, asset, error }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

/// Registers every `physics.cloth_*` method on the world's [`AgentExtension`] (creating the
/// resource when the app has none yet). Call it once after adding `ClothPlugin`.
pub fn install_cloth_methods(world: &mut World) {
    let mut extension = world
        .get_resource::<AgentExtension>()
        .cloned()
        .unwrap_or_default();
    extension.register(cloth_stats_schema(), cloth_stats);
    extension.register(cloth_inspect_schema(), cloth_inspect);
    extension.register(
        play_schema(
            "physics.cloth_set_params",
            "physics_cloth_set_params",
            "Change a cloth solver's parameters (tuning, config, blend, pressure, LOD) with a patch object.",
            &[
                TARGET,
                ParamSchema {
                    name: "patch",
                    kind: "object",
                    required: true,
                    description: "fields to change (`preset` applies a named starting point first; unknown_cloth_preset when the name is wrong); see book/agents/cloth.md for the keys",
                },
            ],
            "{ applied: [key], params }",
        ),
        cloth_set_params,
    );
    extension.register(
        play_schema(
            "physics.cloth_pin",
            "physics_cloth_pin",
            "Pin particles of a cloth solver (inverse mass 0).",
            &[
                TARGET,
                ParamSchema {
                    name: "particles",
                    kind: "array",
                    required: true,
                    description: "particle indices (at most 1024)",
                },
            ],
            "{ pinned }",
        ),
        cloth_pin,
    );
    extension.register(
        play_schema(
            "physics.cloth_unpin",
            "physics_cloth_unpin",
            "Free pinned particles of a cloth solver.",
            &[
                TARGET,
                ParamSchema {
                    name: "particles",
                    kind: "array",
                    required: true,
                    description: "particle indices (at most 1024)",
                },
                ParamSchema {
                    name: "inv_mass",
                    kind: "number",
                    required: false,
                    description: "inverse mass to give them (positive; default: the mean of the solver's free particles, or 1)",
                },
            ],
            "{ unpinned, pinned }",
        ),
        cloth_unpin,
    );
    extension.register(
        play_schema(
            "physics.cloth_wind",
            "physics_cloth_wind",
            "Set the wind of one solver (target) or the global cloth wind (no target).",
            &[
                TARGET_OPTIONAL,
                ParamSchema {
                    name: "velocity",
                    kind: "array",
                    required: false,
                    description: "[x, y, z] in m/s (required unless clear)",
                },
                ParamSchema {
                    name: "gust",
                    kind: "object",
                    required: false,
                    description: "{ amplitude 0..1, period_steps >= 1, seed } (target only)",
                },
                ParamSchema {
                    name: "clear",
                    kind: "boolean",
                    required: false,
                    description: "true: zero the wind and drop the gust",
                },
            ],
            "{ wind }",
        ),
        cloth_wind,
    );
    extension.register(
        play_schema(
            "physics.cloth_step",
            "physics_cloth_step",
            "Run fixed steps on one solver (or all), also while paused; returns the digest.",
            &[
                TARGET_OPTIONAL,
                ParamSchema {
                    name: "count",
                    kind: "number",
                    required: true,
                    description: "fixed steps, 1..=600",
                },
            ],
            "{ steps_run, digest }",
        ),
        cloth_step,
    );
    extension.register(
        play_schema(
            "physics.cloth_reset",
            "physics_cloth_reset",
            "Snap a solver to its pose (or its rest positions), zero velocities, enter settling.",
            &[TARGET],
            "{ state }",
        ),
        cloth_reset,
    );
    extension.register(
        MethodSchema {
            name: "physics.cloth_edit_map",
            tool: "physics_cloth_edit_map",
            description: "Edit a vertex map (reach, standoff, pose_pull, sim_mix, mass_scale, layer) of the cloth asset every ClothSim with that asset name uses; the solvers rebuild and settle.",
            params: &[
                ParamSchema {
                    name: "asset",
                    kind: "string",
                    required: true,
                    description: "the cloth asset's name (its .cloth.ron `name`)",
                },
                ParamSchema {
                    name: "map",
                    kind: "string",
                    required: true,
                    description: "reach | standoff | pose_pull | sim_mix | mass_scale | layer",
                },
                ParamSchema {
                    name: "edit",
                    kind: "object",
                    required: true,
                    description: "a map edit: { op: set | sphere | relax | ramp | flood | from_vertex_color, ... } (book/agents/cloth.md)",
                },
            ],
            result: "{ changed, instances, dirty }",
            mutating: true,
            permission: Permission::EditScene,
        },
        cloth_edit_map,
    );
    extension.register(
        MethodSchema {
            name: "physics.cloth_presets",
            tool: "physics_cloth_presets",
            description: "The named cloth starting points (light_fabric, heavy_fabric, stiff_fabric, elastic, chain, silk, denim, leather, rope_hemp) with their numbers; apply one with physics.cloth_set_params { patch: { preset } }.",
            params: &[],
            result: "{ presets: [{ name, substeps, iterations, damping, stretch_compliance, shear_compliance, bend_compliance, density, strain_cap, tether, wind_drag, wind_lift, gravity_scale, tear_strain }] }",
            mutating: false,
            permission: Permission::Read,
        },
        cloth_presets,
    );
    extension.register(
        play_schema(
            "physics.cloth_set_layers",
            "physics_cloth_set_layers",
            "Put cloth particles on garment layers (inner layers push outer layers, never the reverse) or change the gap kept between layers.",
            &[
                TARGET,
                ParamSchema {
                    name: "layer",
                    kind: "integer",
                    required: false,
                    description: "layer 0..=255 for `particles` or `range` (higher is farther out)",
                },
                ParamSchema {
                    name: "particles",
                    kind: "array",
                    required: false,
                    description: "particle indices (at most 1024)",
                },
                ParamSchema {
                    name: "range",
                    kind: "object",
                    required: false,
                    description: "{ from, count }: a run of particles (a vertex set)",
                },
                ParamSchema {
                    name: "clear",
                    kind: "boolean",
                    required: false,
                    description: "put every particle back on layer 0 first",
                },
                ParamSchema {
                    name: "layer_gap",
                    kind: "number",
                    required: false,
                    description: "extra distance kept across layers in meters; null restores the default (twice the self-collision radius)",
                },
            ],
            "{ max_layer, layered, layer_gap }",
        ),
        cloth_set_layers,
    );
    extension.register(cloth_health_schema(), cloth_health);
    world.insert_resource(extension);
}

fn solver_json(name: &str, solver: &ClothSolver) -> Value {
    let stats = solver.stats();
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in solver.particles() {
        for (axis, value) in p.position.to_array().into_iter().enumerate() {
            min[axis] = min[axis].min(value);
            max[axis] = max[axis].max(value);
        }
    }
    let bounds = |b: [f32; 3]| {
        if solver.particles().is_empty() {
            Value::Null
        } else {
            json!(b)
        }
    };
    json!({
        "name": name,
        "particles": stats.particles,
        "pinned": stats.pinned,
        "distance_constraints": stats.distance_constraints,
        "bend_constraints": stats.bend_constraints,
        "colliders": stats.colliders,
        "steps": stats.steps,
        "dropped_steps": stats.dropped_steps,
        "contacts": stats.contacts,
        "max_stretch": stats.max_stretch,
        "bounds_min": bounds(min),
        "bounds_max": bounds(max),
        // A string: 64-bit values do not survive JSON numbers in every client.
        "digest": format!("{:016x}", solver.state_digest()),
    })
}

fn cloth_stats(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let Some(set) = world.get_resource::<ClothSet>() else {
        return Ok(json!({ "plugin": false, "solvers": [] }));
    };
    let solvers: Vec<Value> = match params.get("name") {
        None | Some(Value::Null) => set
            .iter()
            .map(|(name, solver)| solver_json(name, solver))
            .collect(),
        Some(Value::String(name)) => {
            let solver = set.get(name).ok_or_else(|| {
                RpcError::not_found(
                    format!("no cloth solver named `{name}`"),
                    "call physics.cloth_stats without `name` to list the solvers",
                )
            })?;
            vec![solver_json(name, solver)]
        }
        Some(_) => return Err(RpcError::invalid_params("`name` must be a string")),
    };
    Ok(json!({ "plugin": true, "solvers": solvers }))
}

// ---------------------------------------------------------------------------
// Spec 55 methods
// ---------------------------------------------------------------------------

/// An error with `data.reason` (lowercase protocol code, `book/agents/cloth.md`).
fn reasoned(code: i64, regia_code: &str, reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(code, regia_code, message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

/// `F0002` with a reason.
fn bad(reason: &str, message: impl Into<String>) -> RpcError {
    reasoned(
        protocol::INVALID_PARAMS,
        "F0002",
        reason,
        message,
        "see book/agents/cloth.md for the parameters",
    )
}

/// `F0003` with a reason.
fn missing(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    reasoned(protocol::NOT_FOUND, "F0003", reason, message, hint)
}

/// A rejected solver call: `F0002` with the solver's error code as the reason.
fn refused(err: &ClothError) -> RpcError {
    bad(err.code(), err.to_string())
}

/// The solver name of the `target` param (`None` when absent and optional): `{ name }`, or
/// `{ entity }` of an entity with a `ClothSim` / `SoftSim` (its glue solver).
fn target_name(world: &World, params: &Value, required: bool) -> Result<Option<String>, RpcError> {
    match params.get("target") {
        None | Some(Value::Null) if required => Err(bad("cloth_bad_param", "`target` is required")),
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(target)) => match (target.get("name"), target.get("entity")) {
            (Some(Value::String(name)), _) => Ok(Some(name.clone())),
            (None, Some(bits)) => {
                let entity = bits
                    .as_u64()
                    .and_then(Entity::try_from_bits)
                    .ok_or_else(|| bad("cloth_bad_param", "`target.entity` must be an entity id"))?;
                world
                    .get_entity(entity)
                    .ok()
                    .and_then(|e| e.get::<ClothGlueState>())
                    .map(|state| Some(state.solver().to_string()))
                    .ok_or_else(|| {
                        missing(
                            "cloth_not_found",
                            format!("entity {} has no built ClothSim or SoftSim", entity.to_bits()),
                            "target a solver by { name } (physics.cloth_stats lists them), or an entity with a ClothSim",
                        )
                    })
            }
            _ => Err(bad("cloth_bad_param", "`target` must be { name } or { entity }")),
        },
        Some(_) => Err(bad("cloth_bad_param", "`target` must be an object")),
    }
}

fn plugin_present(world: &World) -> bool {
    world.contains_resource::<ClothSet>() || world.contains_resource::<SoftSet>()
}

fn require_plugin(world: &World) -> Result<(), RpcError> {
    if plugin_present(world) {
        Ok(())
    } else {
        Err(missing(
            "cloth_plugin_missing",
            "this app has no cloth plugin",
            "add regia_physics::cloth::ClothPlugin and call install_cloth_methods",
        ))
    }
}

fn not_found(name: &str) -> RpcError {
    missing(
        "cloth_not_found",
        format!("no cloth solver or soft body named `{name}`"),
        "call physics.cloth_stats or physics.cloth_inspect without a target to list them",
    )
}

/// Which set holds the solver named `name`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Cloth,
    Soft,
}

fn kind_of(world: &World, name: &str) -> Result<Kind, RpcError> {
    if world
        .get_resource::<ClothSet>()
        .is_some_and(|s| s.get(name).is_some())
    {
        Ok(Kind::Cloth)
    } else if world
        .get_resource::<SoftSet>()
        .is_some_and(|s| s.get(name).is_some())
    {
        Ok(Kind::Soft)
    } else {
        Err(not_found(name))
    }
}

/// The particle store of the cloth solver or soft body named `name`.
fn solver_mut<'w>(world: &'w mut World, name: &str) -> Result<&'w mut ClothSolver, RpcError> {
    match kind_of(world, name)? {
        Kind::Cloth => world
            .resource_mut::<ClothSet>()
            .into_inner()
            .get_mut(name)
            .ok_or_else(|| not_found(name)),
        Kind::Soft => world
            .resource_mut::<SoftSet>()
            .into_inner()
            .get_mut(name)
            .map(SoftSolver::cloth_mut)
            .ok_or_else(|| not_found(name)),
    }
}

fn vec3_json(v: Vec3) -> Value {
    json!(v.to_array())
}

fn vec3_param(value: &Value, field: &str) -> Result<Vec3, RpcError> {
    let items = value
        .as_array()
        .filter(|a| a.len() == 3)
        .ok_or_else(|| bad("cloth_bad_param", format!("`{field}` must be [x, y, z]")))?;
    let mut out = [0.0f32; 3];
    for (slot, item) in out.iter_mut().zip(items) {
        let n = item
            .as_f64()
            .ok_or_else(|| bad("cloth_bad_param", format!("`{field}` must hold numbers")))?
            as f32;
        if !n.is_finite() {
            return Err(bad("cloth_non_finite", format!("`{field}` must be finite")));
        }
        *slot = n;
    }
    Ok(Vec3::from_array(out))
}

fn number(value: &Value, field: &str) -> Result<f32, RpcError> {
    let n = value
        .as_f64()
        .ok_or_else(|| bad("cloth_bad_param", format!("`{field}` must be a number")))? as f32;
    if n.is_finite() {
        Ok(n)
    } else {
        Err(bad("cloth_non_finite", format!("`{field}` must be finite")))
    }
}

fn unsigned(value: &Value, field: &str) -> Result<u64, RpcError> {
    value.as_u64().ok_or_else(|| {
        bad(
            "cloth_bad_param",
            format!("`{field}` must be a non-negative integer"),
        )
    })
}

fn unsigned32(value: &Value, field: &str) -> Result<u32, RpcError> {
    u32::try_from(unsigned(value, field)?).map_err(|_| {
        bad(
            "cloth_bad_param",
            format!("`{field}` must fit in a 32-bit unsigned integer"),
        )
    })
}

fn self_collision_json(mode: SelfCollision) -> Value {
    match mode.radius() {
        Some(radius) => json!({ "mode": mode.name(), "radius": radius }),
        None => json!({ "mode": mode.name() }),
    }
}

fn wind_model_json(model: WindModel) -> Value {
    match model {
        WindModel::Push { gain } => json!({ "mode": "push", "gain": gain }),
        WindModel::Aerodynamic { drag, lift } => json!({ "mode": "aerodynamic", "drag": drag, "lift": lift }),
    }
}

/// The settings `physics.cloth_set_params` can change, as JSON.
fn params_json(solver: &ClothSolver) -> Value {
    let config = solver.config();
    let t = solver.tuning();
    let (stretch_compliance, bend_compliance) = solver.link_compliances();
    json!({
        "damping": config.damping,
        "substeps": config.substeps,
        "iterations": config.iterations,
        "collision_thickness": config.thickness,
        "gravity_scale": t.gravity_scale,
        "gravity_override": t.gravity_override.map(vec3_json),
        "stretch_compliance": stretch_compliance,
        "bend_compliance": bend_compliance,
        "pull_compliance": t.pull_compliance,
        "pull_scale": t.pull_scale,
        "inertia_scale": t.inertia_scale,
        "strain_cap": t.strain_cap.map(|c| json!({ "max": c.max, "min": c.min })),
        "tether": t.tether,
        "self_collision": self_collision_json(t.self_collision),
        "self_friction": t.self_friction,
        "exclude_rings": t.exclude_rings,
        "wind_model": wind_model_json(t.wind_model),
        "air_density": t.air_density,
        "teleport_distance": t.teleport_distance,
        "tear": t.tear.map(|x| json!({ "strain": x.strain, "max_per_step": x.max_per_step })),
        "blend": solver.blend(),
        "lod": solver.lod_level(),
    })
}

fn inspect_json(
    name: &str,
    kind: &str,
    solver: &ClothSolver,
    soft: Option<&SoftSolver>,
    slice: Option<(usize, usize)>,
) -> Value {
    let mut out = solver_json(name, solver);
    let d = solver.diagnostics();
    if let Value::Object(map) = &mut out {
        map.insert("kind".into(), json!(kind));
        map.insert("state".into(), json!(d.state.name()));
        map.insert("blend".into(), json!(d.blend));
        map.insert("lod".into(), json!(d.lod));
        map.insert("dropped_self_pairs".into(), json!(d.dropped_self_pairs));
        map.insert("self_contacts".into(), json!(d.self_contacts));
        map.insert("self_crossings".into(), json!(solver.self_crossings()));
        map.insert("max_layer".into(), json!(solver.max_layer()));
        map.insert(
            "layered".into(),
            json!(solver.layers().iter().filter(|&&l| l != 0).count()),
        );
        map.insert("layer_gap".into(), json!(solver.layer_gap()));
        map.insert("torn".into(), json!(d.torn));
        map.insert("wind".into(), vec3_json(d.wind));
        map.insert("global_wind".into(), vec3_json(solver.global_wind()));
        map.insert("max_pose_error".into(), json!(d.max_pose_error));
        map.insert("teleports".into(), json!(d.teleports));
        map.insert("tethers".into(), json!(d.tethers));
        if let Some(soft) = soft {
            map.insert("model".into(), json!(soft.model_name()));
            map.insert("volume".into(), json!(soft.volume()));
            map.insert("rest_volume".into(), json!(soft.rest_volume()));
            map.insert("pressure".into(), json!(soft.pressure()));
            map.insert("inverted_tets".into(), json!(soft.inverted_tets()));
            map.insert("plasticity".into(), plasticity_json(soft.plasticity()));
            map.insert("plastic_strain".into(), json!(soft.max_plastic_strain()));
        }
        if let Some((from, count)) = slice {
            let particles: Vec<Value> = solver
                .particles()
                .iter()
                .enumerate()
                .skip(from)
                .take(count)
                .map(|(index, p)| {
                    json!({
                        "index": index,
                        "position": vec3_json(p.position),
                        "velocity": vec3_json(p.velocity),
                        "inv_mass": p.inv_mass,
                    })
                })
                .collect();
            map.insert("particles".into(), Value::Array(particles));
        }
    }
    out
}

/// The game-world side of the instances: `{ entity, asset, section, section_vertices,
/// section_triangles, hidden_triangles, error }` by solver name, and the instances that have no
/// solver (a load or build that failed).
fn instances(world: &mut World) -> (BTreeMap<String, Value>, Vec<Value>) {
    type Instance = (
        Entity,
        &'static ClothGlueState,
        Option<&'static ClothSim>,
        Option<&'static SoftSim>,
        Option<&'static ClothOutput>,
    );
    let mut query = world.query::<Instance>();
    let found: Vec<_> = query
        .iter(world)
        .map(|(entity, state, cloth, soft, output)| {
            let asset = cloth
                .map(|c| c.asset.name.clone())
                .or_else(|| soft.map(|s| s.asset.name.clone()));
            let section = cloth
                .and_then(|c| c.section)
                .or_else(|| soft.and_then(|s| s.section));
            let hidden = output.map_or(0, |o| o.hidden_triangles);
            (
                entity,
                state.solver().to_string(),
                state.last_error,
                asset,
                section,
                (hidden, state.scene_wind()),
            )
        })
        .collect();
    let mut by_solver = BTreeMap::new();
    let mut failed = Vec::new();
    for (entity, solver, error, asset, section, (hidden, scene_wind)) in found {
        let mesh = section
            .and_then(|e| world.get::<regia_render::MeshRenderer>(e))
            .map(|renderer| renderer.mesh.clone());
        let built = world
            .get_resource::<ClothSet>()
            .is_some_and(|s| s.get(&solver).is_some())
            || world
                .get_resource::<SoftSet>()
                .is_some_and(|s| s.get(&solver).is_some());
        if !built {
            failed.push(json!({ "entity": entity.to_bits(), "asset": asset, "error": error }));
            continue;
        }
        by_solver.insert(
            solver,
            json!({
                "entity": entity.to_bits(),
                "asset": asset,
                "section": section.map(Entity::to_bits),
                "section_vertices": mesh.as_ref().map_or(0, |m| m.vertices.len()),
                "section_triangles": mesh.as_ref().map_or(0, |m| m.indices.len() / 3),
                "hidden_triangles": hidden,
                "scene_wind": vec3_json(scene_wind),
                "error": error,
            }),
        );
    }
    let mut query = world.query::<(Entity, &ClothSourceState)>();
    failed.extend(
        query
            .iter(world)
            .filter(|(_, s)| s.error.is_some())
            .map(|(entity, s)| json!({ "entity": entity.to_bits(), "asset": s.asset, "error": s.error })),
    );
    (by_solver, failed)
}

fn cloth_inspect(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !plugin_present(world) {
        return Ok(json!({ "plugin": false, "solvers": [] }));
    }
    let target = target_name(world, params, false)?;
    let slice = match params.get("particles") {
        None | Some(Value::Null) => None,
        Some(Value::Object(p)) => {
            let from = usize::try_from(p.get("from").map_or(Ok(0), |v| unsigned(v, "particles.from"))?)
                .unwrap_or(usize::MAX);
            let count = p
                .get("count")
                .map_or(Ok(MAX_PARTICLE_SLICE as u64), |v| unsigned(v, "particles.count"))?;
            let count = usize::try_from(count).unwrap_or(usize::MAX);
            if count > MAX_PARTICLE_SLICE {
                return Err(bad(
                    "cloth_bad_param",
                    format!("`particles.count` is at most {MAX_PARTICLE_SLICE}"),
                ));
            }
            Some((from, count))
        }
        Some(_) => return Err(bad("cloth_bad_param", "`particles` must be { from, count }")),
    };
    if let Some(name) = &target {
        kind_of(world, name)?;
    }
    let (mut instances, failed) = instances(world);
    let wanted = |n: &str| target.as_deref().is_none_or(|t| t == n);
    let check_slice = |solver: &ClothSolver| match slice {
        Some((from, _)) if from >= solver.particles().len() && target.is_some() => Err(bad(
            "cloth_particle_out_of_range",
            format!("`particles.from` {from} is out of range"),
        )),
        _ => Ok(()),
    };
    let mut solvers = Vec::new();
    if let Some(set) = world.get_resource::<ClothSet>() {
        for (name, solver) in set.iter().filter(|(n, _)| wanted(n)) {
            check_slice(solver)?;
            let mut json = inspect_json(name, "cloth", solver, None, slice);
            attach_instance(&mut json, &mut instances, name);
            solvers.push(json);
        }
    }
    if let Some(set) = world.get_resource::<SoftSet>() {
        for (name, soft) in set.iter().filter(|(n, _)| wanted(n)) {
            check_slice(soft.cloth())?;
            let mut json = inspect_json(name, "soft", soft.cloth(), Some(soft), slice);
            attach_instance(&mut json, &mut instances, name);
            solvers.push(json);
        }
    }
    let parallel = world.get_resource::<ClothSet>().is_some_and(ClothSet::parallel);
    let scene_wind = world.get_resource::<regia_scene::weather::SceneWind>().map(|w| {
        json!({
            "mode": match w.mode {
                regia_scene::weather::SceneWindMode::Weather => "weather",
                regia_scene::weather::SceneWindMode::Manual => "manual",
                regia_scene::weather::SceneWindMode::Off => "off",
            },
            "velocity": w.velocity3(),
            "cloth": regia_framework::cloth::cloth_scene_wind(w).map(vec3_json),
        })
    });
    Ok(json!({
        "plugin": true,
        "parallel": parallel,
        "scene_wind": scene_wind,
        "solvers": solvers,
        "failed": failed,
    }))
}

fn attach_instance(solver: &mut Value, instances: &mut BTreeMap<String, Value>, name: &str) {
    if let (Value::Object(map), Some(instance)) = (solver, instances.remove(name)) {
        map.insert("instance".into(), instance);
    }
}

fn plasticity_json(p: Option<Plasticity>) -> Value {
    p.map_or(
        Value::Null,
        |p| json!({ "yield": p.yield_threshold, "creep": p.creep, "max": p.max }),
    )
}

/// `plasticity`: `null` or `{ yield, creep, max }` (each defaulting to 0.1, 1 and 1).
fn plasticity_param(value: &Value) -> Result<Option<Plasticity>, RpcError> {
    match value {
        Value::Null => Ok(None),
        Value::Object(p) => {
            let field = |f: &str, default: f32| {
                p.get(f)
                    .map_or(Ok(default), |v| number(v, &format!("plasticity.{f}")))
            };
            let plasticity = Plasticity {
                yield_threshold: field("yield", 0.1)?,
                creep: field("creep", 1.0)?,
                max: field("max", 1.0)?,
            };
            plasticity.validate().map(Some).map_err(|e| refused(&e))
        }
        _ => Err(bad(
            "cloth_bad_param",
            "`plasticity` must be { yield, creep, max } or null",
        )),
    }
}

/// `lod_distances`: three ascending positive distances in meters.
fn lod_distances_param(value: &Value) -> Result<[f32; 3], RpcError> {
    let d = vec3_param(value, "lod_distances")?.to_array();
    if d[0] <= 0.0 || d[0] >= d[1] || d[1] >= d[2] {
        return Err(bad(
            "cloth_bad_param",
            "`lod_distances` must be three ascending positive distances [d1, d2, d_off]",
        ));
    }
    Ok(d)
}

/// The entity of the `ClothSim` / `SoftSim` instance whose glue built solver `name`.
fn instance_of(world: &mut World, name: &str) -> Option<Entity> {
    let mut query = world.query::<(Entity, &ClothGlueState)>();
    query
        .iter(world)
        .find(|(_, state)| state.solver() == name)
        .map(|(entity, _)| entity)
}

/// The distance-LOD thresholds of the instance, if it has a `ClothSim` or `SoftSim`.
fn instance_lod_distances(world: &World, entity: Entity) -> Option<[f32; 3]> {
    let e = world.get_entity(entity).ok()?;
    e.get::<ClothSim>()
        .map(|s| s.lod_distances)
        .or_else(|| e.get::<SoftSim>().map(|s| s.lod_distances))
}

/// Applies `patch` to copies of the solver's settings; nothing is written unless every key is
/// valid.
fn cloth_set_params(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_plugin(world)?;
    let name = target_name(world, params, true)?.unwrap_or_default();
    let patch = match params.get("patch") {
        Some(Value::Object(patch)) => patch.clone(),
        _ => return Err(bad("cloth_bad_param", "`patch` must be an object")),
    };
    let kind = kind_of(world, &name)?;
    let solver = solver_mut(world, &name)?;
    let mut config = *solver.config();
    let mut tuning: ClothTuning = *solver.tuning();
    let mut blend = None;
    let mut pressure = None;
    let mut lod = None;
    let mut stretch_compliance = None;
    let mut bend_compliance = None;
    let mut lod_distances = None;
    let mut plasticity = None;
    let mut applied = Vec::new();
    // A preset is the base: it goes in first, so the other keys of the same patch refine it.
    let mut preset = None;
    if let Some(value) = patch.get("preset") {
        if kind == Kind::Soft {
            return Err(bad(
                "cloth_bad_param",
                "`preset` applies to cloth solvers, not soft bodies",
            ));
        }
        let name = value
            .as_str()
            .ok_or_else(|| bad("cloth_bad_param", "`preset` must be a string"))?;
        let chosen = ClothPreset::from_name(name).map_err(|e| bad(UnknownPreset::CODE, e.to_string()))?;
        chosen.apply(&mut tuning, &mut config);
        preset = Some(chosen);
    }
    for (key, value) in &patch {
        match key.as_str() {
            "preset" => {}
            "damping" => config.damping = number(value, key)?,
            "substeps" => config.substeps = unsigned32(value, key)?,
            "iterations" => config.iterations = unsigned32(value, key)?,
            "collision_thickness" => config.thickness = number(value, key)?,
            "gravity_scale" => tuning.gravity_scale = number(value, key)?,
            "gravity_override" => {
                tuning.gravity_override = match value {
                    Value::Null => None,
                    v => Some(vec3_param(v, key).map_err(|_| {
                        bad("cloth_bad_param", "`gravity_override` must be [x, y, z] or null")
                    })?),
                }
            }
            "stretch_compliance" | "bend_compliance" => {
                let c = number(value, key)?;
                if c < 0.0 {
                    return Err(bad("cloth_bad_param", format!("`{key}` must be at least 0")));
                }
                if key == "stretch_compliance" {
                    stretch_compliance = Some(c);
                } else {
                    bend_compliance = Some(c);
                }
            }
            "shear_compliance" => {
                return Err(bad(
                    "cloth_bad_param",
                    "`shear_compliance` is cook time only: a built solver does not know its shear links (ADR-2250)",
                ));
            }
            "friction" => {
                return Err(bad(
                    "cloth_bad_param",
                    "`friction` is reserved: cloth contacts use the collider's friction (ADR-2250)",
                ));
            }
            "lod_distances" => lod_distances = Some(lod_distances_param(value)?),
            "plasticity" => {
                let shape_match = kind == Kind::Soft
                    && world
                        .get_resource::<SoftSet>()
                        .and_then(|set| set.get(&name))
                        .is_some_and(|soft| soft.model_name() == "shape_match");
                if !shape_match {
                    return Err(bad(
                        "cloth_bad_param",
                        "`plasticity` applies to shape-matching soft bodies only",
                    ));
                }
                plasticity = Some(plasticity_param(value)?);
            }
            "pull_compliance" => tuning.pull_compliance = number(value, key)?,
            "pull_scale" => tuning.pull_scale = number(value, key)?,
            "inertia_scale" => tuning.inertia_scale = number(value, key)?,
            "strain_cap" => {
                tuning.strain_cap = match value {
                    Value::Null => None,
                    Value::Object(c) => Some(StrainCap {
                        max: number(c.get("max").unwrap_or(&Value::Null), "strain_cap.max")?,
                        min: c.get("min").map_or(Ok(0.0), |v| number(v, "strain_cap.min"))?,
                    }),
                    _ => {
                        return Err(bad(
                            "cloth_bad_param",
                            "`strain_cap` must be { max, min? } or null",
                        ));
                    }
                }
            }
            "tether" => {
                tuning.tether = match value {
                    Value::Null => None,
                    v => Some(number(v, key)?),
                }
            }
            "self_collision" => {
                let mode = crate::params::opt_str(value, "mode")?.unwrap_or("");
                let radius = || {
                    number(
                        value.get("radius").unwrap_or(&Value::Null),
                        "self_collision.radius",
                    )
                };
                tuning.self_collision = match mode {
                    "off" => SelfCollision::Off,
                    "spheres" => SelfCollision::Spheres { radius: radius()? },
                    "vertex_triangle" => SelfCollision::VertexTriangle { radius: radius()? },
                    _ => {
                        return Err(bad(
                            "cloth_bad_param",
                            "`self_collision.mode` is off, spheres or vertex_triangle",
                        ));
                    }
                }
            }
            "self_friction" => tuning.self_friction = number(value, key)?,
            "exclude_rings" => tuning.exclude_rings = unsigned32(value, key)?,
            "wind_model" => {
                let field = |f: &str, default: f32| value.get(f).map_or(Ok(default), |v| number(v, f));
                tuning.wind_model = match crate::params::opt_str(value, "mode")? {
                    Some("push") => WindModel::Push {
                        gain: field("gain", 1.0)?,
                    },
                    Some("aerodynamic") => WindModel::Aerodynamic {
                        drag: field("drag", 1.0)?,
                        lift: field("lift", 0.3)?,
                    },
                    _ => return Err(bad("cloth_bad_param", "`wind_model.mode` is push or aerodynamic")),
                }
            }
            "air_density" => tuning.air_density = number(value, key)?,
            "teleport_distance" => tuning.teleport_distance = number(value, key)?,
            "tear" => {
                tuning.tear = match value {
                    Value::Null => None,
                    Value::Object(t) => Some(TearParams {
                        strain: t.get("strain").map_or(Ok(2.0), |v| number(v, "tear.strain"))?,
                        max_per_step: t
                            .get("max_per_step")
                            .map_or(Ok(4), |v| unsigned32(v, "tear.max_per_step"))?,
                    }),
                    _ => {
                        return Err(bad(
                            "cloth_bad_param",
                            "`tear` must be { strain, max_per_step } or null",
                        ));
                    }
                }
            }
            "blend" => blend = Some(number(value, key)?),
            "pressure" => {
                if kind != Kind::Soft
                    || world
                        .get_resource::<SoftSet>()
                        .and_then(|set| set.get(&name))
                        .and_then(SoftSolver::pressure)
                        .is_none()
                {
                    return Err(bad(
                        "cloth_bad_param",
                        "`pressure` applies to pressure soft bodies only",
                    ));
                }
                pressure = Some(number(value, key)?);
            }
            "lod" => {
                let level = unsigned(value, key)?;
                if level > u64::from(regia_physics::cloth::LOD_OFF) {
                    return Err(bad("cloth_bad_param", "`lod` is 0, 1, 2 or 3 (off)"));
                }
                lod = Some(level as u8);
            }
            other => return Err(bad("cloth_bad_param", format!("unknown parameter `{other}`"))),
        }
        applied.push(key.clone());
    }
    config.validate().map_err(|e| refused(&e))?;
    tuning.validate().map_err(|e| refused(&e))?;
    let instance = instance_of(world, &name);
    if lod_distances.is_some() && instance.is_none_or(|e| instance_lod_distances(world, e).is_none()) {
        return Err(bad(
            "cloth_bad_param",
            "`lod_distances` needs a ClothSim or SoftSim instance (a solver built by code has none)",
        ));
    }
    if let Some(b) = blend
        && !(0.0..=1.0).contains(&b)
    {
        return Err(refused(&ClothError::BadBlend));
    }
    // Every value is valid: apply.
    let solver = solver_mut(world, &name)?;
    if let Some(preset) = preset {
        // Also sets the link compliances; the config and tuning below then add the patch.
        solver.apply_preset(preset).map_err(|e| refused(&e))?;
    }
    solver.set_config(config).map_err(|e| refused(&e))?;
    solver.set_tuning(tuning).map_err(|e| refused(&e))?;
    if let Some(b) = blend {
        solver.set_blend(b).map_err(|e| refused(&e))?;
    }
    if let Some(level) = lod {
        solver.set_lod_level(level);
    }
    solver
        .set_link_compliances(stretch_compliance, bend_compliance)
        .map_err(|e| refused(&e))?;
    if let (Some(d), Some(entity)) = (lod_distances, instance)
        && let Ok(mut e) = world.get_entity_mut(entity)
    {
        if let Some(mut sim) = e.get_mut::<ClothSim>() {
            sim.lod_distances = d;
        } else if let Some(mut sim) = e.get_mut::<SoftSim>() {
            sim.lod_distances = d;
        }
    }
    if let Some(p) = plasticity
        && let Some(mut set) = world.get_resource_mut::<SoftSet>()
        && let Some(soft) = set.get_mut(&name)
    {
        soft.set_plasticity(p).map_err(|e| refused(&e))?;
    }
    if let Some(k) = pressure
        && let Some(mut set) = world.get_resource_mut::<SoftSet>()
        && let Some(soft) = set.get_mut(&name)
    {
        soft.set_pressure(k).map_err(|e| refused(&e))?;
    }
    let mut result = {
        let solver = solver_mut(world, &name)?;
        params_json(solver)
    };
    // A soft body's stored pressure is a setting, so the echo includes it whether or not this
    // patch named it (spec 55 § 5; the same value `physics.cloth_inspect` reports).
    if kind == Kind::Soft
        && let Some(stored) = world
            .get_resource::<SoftSet>()
            .and_then(|set| set.get(&name))
            .map(|soft| soft.pressure())
        && let Value::Object(map) = &mut result
    {
        map.insert("pressure".into(), json!(stored));
    }
    if let Value::Object(map) = &mut result {
        map.insert(
            "lod_distances".into(),
            json!(instance.and_then(|e| instance_lod_distances(world, e))),
        );
        if kind == Kind::Soft {
            let p = world
                .get_resource::<SoftSet>()
                .and_then(|set| set.get(&name))
                .and_then(SoftSolver::plasticity);
            map.insert("plasticity".into(), plasticity_json(p));
        }
    }
    Ok(json!({ "applied": applied, "params": result }))
}

/// `physics.cloth_presets`: the preset table. Needs no `ClothPlugin`; it is static data.
fn cloth_presets(_world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let presets: Vec<Value> = ClothPreset::ALL
        .iter()
        .map(|p| {
            let r = p.row();
            json!({
                "name": p.name(),
                "substeps": r.substeps,
                "iterations": r.iterations,
                "damping": r.damping,
                "stretch_compliance": r.stretch_compliance,
                "shear_compliance": r.shear_compliance,
                "bend_compliance": r.bend_compliance,
                "density": r.density,
                "strain_cap": r.strain_cap.map(|(max, min)| json!({ "max": max, "min": min })),
                "tether": r.tether,
                "wind_drag": r.wind_drag,
                "wind_lift": r.wind_lift,
                "gravity_scale": r.gravity_scale,
                "tear_strain": r.tear_strain,
            })
        })
        .collect();
    Ok(json!({ "presets": presets }))
}

/// The `particles` list of a pin call: integers, at most [`MAX_PIN_LIST`], all in range.
fn particle_list(params: &Value, len: usize) -> Result<Vec<usize>, RpcError> {
    let list = params
        .get("particles")
        .and_then(Value::as_array)
        .ok_or_else(|| bad("cloth_bad_param", "`particles` must be an array of indices"))?;
    if list.len() > MAX_PIN_LIST {
        return Err(bad(
            "cloth_bad_param",
            format!("at most {MAX_PIN_LIST} particles per call"),
        ));
    }
    list.iter()
        .map(|v| {
            // `try_from`: on 32-bit targets a wide index must not wrap onto a valid particle.
            let i = usize::try_from(unsigned(v, "particles")?).unwrap_or(usize::MAX);
            if i < len {
                Ok(i)
            } else {
                Err(bad(
                    "cloth_particle_out_of_range",
                    format!("particle {i} is out of range ({len} particles)"),
                ))
            }
        })
        .collect()
}

fn cloth_pin(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_plugin(world)?;
    let name = target_name(world, params, true)?.unwrap_or_default();
    let solver = solver_mut(world, &name)?;
    let list = particle_list(params, solver.particles().len())?;
    for i in list {
        solver.pin(i).map_err(|e| refused(&e))?;
    }
    Ok(json!({ "pinned": solver.stats().pinned }))
}

fn cloth_unpin(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_plugin(world)?;
    let name = target_name(world, params, true)?.unwrap_or_default();
    let solver = solver_mut(world, &name)?;
    let list = particle_list(params, solver.particles().len())?;
    let inv_mass = match params.get("inv_mass") {
        None | Some(Value::Null) => {
            let free: Vec<f32> = solver
                .particles()
                .iter()
                .map(|p| p.inv_mass)
                .filter(|&w| w > 0.0)
                .collect();
            if free.is_empty() {
                1.0
            } else {
                free.iter().sum::<f32>() / free.len() as f32
            }
        }
        Some(v) => number(v, "inv_mass")?,
    };
    if inv_mass <= 0.0 {
        return Err(bad("cloth_bad_param", "`inv_mass` must be positive"));
    }
    let mut unpinned = 0;
    for i in list {
        if solver.particles()[i].inv_mass == 0.0 {
            unpinned += 1;
        }
        solver.unpin(i, inv_mass).map_err(|e| refused(&e))?;
    }
    Ok(json!({ "unpinned": unpinned, "pinned": solver.stats().pinned }))
}

fn cloth_wind(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_plugin(world)?;
    let target = target_name(world, params, false)?;
    let clear = crate::params::opt_bool(params, "clear")?.unwrap_or(false);
    let velocity = if clear {
        Vec3::ZERO
    } else {
        let v = params
            .get("velocity")
            .ok_or_else(|| bad("cloth_bad_param", "`velocity` is required unless `clear` is true"))?;
        vec3_param(v, "velocity")?
    };
    let gust = match params.get("gust") {
        None | Some(Value::Null) => None,
        Some(Value::Object(g)) => Some(Gust {
            amplitude: number(g.get("amplitude").unwrap_or(&Value::Null), "gust.amplitude")?,
            period_steps: unsigned32(g.get("period_steps").unwrap_or(&Value::Null), "gust.period_steps")?,
            seed: g.get("seed").map_or(Ok(0), |v| unsigned32(v, "gust.seed"))?,
        }),
        Some(_) => {
            return Err(bad(
                "cloth_bad_param",
                "`gust` must be { amplitude, period_steps, seed }",
            ));
        }
    };
    match target {
        Some(name) => {
            let solver = solver_mut(world, &name)?;
            if let Some(g) = &gust {
                g.validate().map_err(|e| refused(&e))?;
            }
            solver.set_wind(velocity).map_err(|e| refused(&e))?;
            if clear || gust.is_some() {
                solver.set_gust(gust).map_err(|e| refused(&e))?;
            }
        }
        None => {
            if gust.is_some() {
                return Err(bad("cloth_bad_param", "`gust` needs a `target`"));
            }
            let mut set = world.get_resource_mut::<ClothSet>().ok_or_else(|| {
                missing(
                    "cloth_plugin_missing",
                    "this app has no ClothSet",
                    "add ClothPlugin",
                )
            })?;
            set.set_global_wind(velocity).map_err(|e| refused(&e))?;
        }
    }
    Ok(json!({ "wind": vec3_json(velocity) }))
}

fn cloth_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_plugin(world)?;
    let target = target_name(world, params, false)?;
    let count = crate::params::opt_u64(params, "count")?
        .filter(|&c| c >= 1)
        .ok_or_else(|| bad("cloth_bad_param", "`count` must be an integer from 1 to 600"))?;
    if count > MAX_STEP_COUNT {
        return Err(bad(
            "cloth_step_count_too_large",
            format!("`count` {count} is over {MAX_STEP_COUNT}"),
        ));
    }
    let count = count as u32;
    let digest = match target {
        Some(name) => {
            let solver = solver_mut(world, &name)?;
            solver.step_fixed(count);
            solver.state_digest()
        }
        None => {
            // FNV-1a over every solver's digest, cloth solvers then soft bodies, in insertion
            // order.
            let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
            let mut feed = |d: u64| {
                for byte in d.to_le_bytes() {
                    hash ^= u64::from(byte);
                    hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
                }
            };
            if let Some(mut set) = world.get_resource_mut::<ClothSet>() {
                for (_, solver) in set.iter_mut() {
                    solver.step_fixed(count);
                    feed(solver.state_digest());
                }
            }
            if let Some(mut set) = world.get_resource_mut::<SoftSet>() {
                let names: Vec<String> = set.iter().map(|(n, _)| n.to_owned()).collect();
                for name in names {
                    if let Some(soft) = set.get_mut(&name) {
                        soft.cloth_mut().step_fixed(count);
                        feed(soft.state_digest());
                    }
                }
            }
            hash
        }
    };
    Ok(json!({ "steps_run": count, "digest": format!("{digest:016x}") }))
}

fn cloth_reset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_plugin(world)?;
    let name = target_name(world, params, true)?.unwrap_or_default();
    let solver = solver_mut(world, &name)?;
    solver.reset_to_pose();
    Ok(json!({ "state": solver.state().name() }))
}

/// `physics.cloth_edit_map`: applies a `MapEdit` to LOD 0 of the asset named `asset` and
/// gives every `ClothSim` using it the edited copy (one shared `Arc`), which rebuilds their
/// solvers. The asset is marked dirty; the source `.cloth.ron` is rewritten by the editor's
/// save (`regia_physics::cloth::maps::store_maps`).
/// `physics.cloth_set_layers`: everything is checked before the solver changes.
fn cloth_set_layers(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    require_plugin(world)?;
    let name = target_name(world, params, true)?.unwrap_or_default();
    let gap = match params.get("layer_gap") {
        None => None,
        Some(Value::Null) => Some(None),
        Some(v) => {
            let g = number(v, "layer_gap")?;
            if g < 0.0 {
                return Err(bad("cloth_bad_param", "`layer_gap` must not be negative"));
            }
            Some(Some(g))
        }
    };
    let clear = match params.get("clear") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(bad("cloth_bad_param", "`clear` must be a boolean")),
    };
    let has_list = params.get("particles").is_some_and(|v| !v.is_null());
    let has_range = params.get("range").is_some_and(|v| !v.is_null());
    if has_list && has_range {
        return Err(bad("cloth_bad_param", "give `particles` or `range`, not both"));
    }
    if !(has_list || has_range || clear || gap.is_some()) {
        return Err(bad(
            "cloth_bad_param",
            "give `particles` or `range` with `layer`, `clear` or `layer_gap`",
        ));
    }
    let layer = if has_list || has_range {
        let raw = params.get("layer").ok_or_else(|| {
            bad(
                "cloth_bad_param",
                "`layer` is required with `particles` or `range`",
            )
        })?;
        u8::try_from(unsigned(raw, "layer")?)
            .map_err(|_| bad("cloth_bad_param", "`layer` must be in 0..=255"))?
    } else {
        0
    };
    let solver = solver_mut(world, &name)?;
    let len = solver.particles().len();
    let targets: Vec<usize> = if has_list {
        particle_list(params, len)?
    } else if has_range {
        let range = params
            .get("range")
            .and_then(Value::as_object)
            .ok_or_else(|| bad("cloth_bad_param", "`range` must be { from, count }"))?;
        let field = |key: &str| -> Result<usize, RpcError> {
            let v = range
                .get(key)
                .ok_or_else(|| bad("cloth_bad_param", format!("`range.{key}` is required")))?;
            Ok(usize::try_from(unsigned(v, key)?).unwrap_or(usize::MAX))
        };
        let (from, count) = (field("from")?, field("count")?);
        match from.checked_add(count) {
            Some(end) if end <= len => (from..end).collect(),
            _ => {
                return Err(bad(
                    "cloth_particle_out_of_range",
                    format!("`range` {from}+{count} passes the last particle ({len} particles)"),
                ));
            }
        }
    } else {
        Vec::new()
    };
    let mut layers: Vec<u8> = if clear {
        Vec::new()
    } else {
        solver.layers().to_vec()
    };
    if !targets.is_empty() {
        layers.resize(len, 0);
        for i in targets {
            layers[i] = layer;
        }
    } else if !layers.is_empty() {
        layers.resize(len, 0);
    }
    solver.set_layers(&layers).map_err(|e| refused(&e))?;
    if let Some(gap) = gap {
        solver.set_layer_gap(gap).map_err(|e| refused(&e))?;
    }
    Ok(json!({
        "max_layer": solver.layers().iter().copied().max().unwrap_or(0),
        "layered": solver.layers().iter().filter(|&&l| l != 0).count(),
        "layer_gap": solver.layer_gap(),
    }))
}

fn cloth_edit_map(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let asset_name = params
        .get("asset")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("cloth_bad_param", "`asset` (string) is required"))?
        .to_string();
    let map = params
        .get("map")
        .and_then(Value::as_str)
        .ok_or_else(|| bad("cloth_bad_param", "`map` (string) is required"))?
        .to_string();
    let edit: MapEdit = serde_json::from_value(params.get("edit").cloned().unwrap_or(Value::Null))
        .map_err(|e| bad("cloth_bad_param", format!("`edit`: {e}")))?;
    let users = cloth_asset_users(world, &asset_name);
    let Some((_, first)) = users.first() else {
        return Err(missing(
            "cloth_not_found",
            format!("no ClothSim uses a cloth asset named {asset_name:?}"),
            "physics.cloth_stats lists the solvers; scene queries list ClothSim entities",
        ));
    };
    let (edited, changed) = edit_cloth_map(first, &map, &edit)?;
    let shared = std::sync::Arc::new(edited);
    let entities: Vec<Entity> = users.iter().map(|(e, _)| *e).collect();
    share_cloth_asset(world, &entities, &shared);
    Ok(json!({ "changed": changed, "instances": users.len(), "dirty": shared.dirty }))
}

/// The map-edit core `physics.cloth_edit_map` and the editor's cloth paint mode share (spec 55
/// § 3.4, § 6): `edit` applied to map `map` of LOD 0 of a copy of `base` through
/// `regia_physics::cloth::maps::apply_map_edit`. Returns the edited copy (marked dirty) and how
/// many LOD 0 values changed.
///
/// # Errors
/// `F0002` with the edit's reason (`cloth_bad_param`, `cloth_particle_out_of_range`, ...);
/// `base` is never changed.
pub fn edit_cloth_map(
    base: &regia_physics::cloth::ClothAsset,
    map: &str,
    edit: &MapEdit,
) -> Result<(regia_physics::cloth::ClothAsset, usize), RpcError> {
    let mut edited = base.clone();
    let changed = apply_map_edit(&mut edited, 0, map, edit).map_err(|e| refused(&e))?;
    Ok((edited, changed))
}

/// Every `ClothSim` whose asset is named `name`, with that asset, sorted by entity bits (the
/// instances `physics.cloth_edit_map` edits).
pub fn cloth_asset_users(
    world: &mut World,
    name: &str,
) -> Vec<(Entity, std::sync::Arc<regia_physics::cloth::ClothAsset>)> {
    let mut query = world.query::<(Entity, &ClothSim)>();
    let mut users: Vec<(Entity, std::sync::Arc<regia_physics::cloth::ClothAsset>)> = query
        .iter(world)
        .filter(|(_, sim)| sim.asset.name == name)
        .map(|(e, sim)| (e, sim.asset.clone()))
        .collect();
    users.sort_by_key(|(e, _)| e.to_bits());
    users
}

/// Gives each listed `ClothSim` the shared `asset` (their solvers rebuild on the next fixed
/// step). Entities without a `ClothSim` are skipped.
pub fn share_cloth_asset(
    world: &mut World,
    entities: &[Entity],
    asset: &std::sync::Arc<regia_physics::cloth::ClothAsset>,
) {
    for entity in entities {
        if let Some(mut sim) = world.get_mut::<ClothSim>(*entity)
            && !std::sync::Arc::ptr_eq(&sim.asset, asset)
        {
            sim.asset = asset.clone();
        }
    }
}

// ---------------------------------------------------------------------------
// Runaway guard (ADR-3273)
// ---------------------------------------------------------------------------

fn cloth_health_schema() -> MethodSchema {
    MethodSchema {
        name: "physics.cloth_health",
        tool: "physics_cloth_health",
        description: "Cloth and soft-body runaway guard: max and peak strain, fault counts by kind, pose resets and the last fault (code, recovery, step).",
        params: &[TARGET_OPTIONAL],
        result: "{ plugin, solvers: [{ name, kind, guard: { enabled, max_strain, stiff_limit, extra_iterations, stiff_target }, max_strain, peak_strain, faults, non_finite_faults, runaway_faults, pose_resets, extra_iterations, stiff_projections, last_fault: { step, code, recovery, particle, strain } | null }] }",
        mutating: false,
        permission: Permission::Read,
    }
}

/// A float for JSON: non-finite values (an overflowing strain) become `null`.
fn finite_json(value: f32) -> Value {
    if value.is_finite() {
        json!(value)
    } else {
        Value::Null
    }
}

fn health_json(name: &str, kind: &str, solver: &ClothSolver) -> Value {
    let h = solver.health();
    let last_fault = h.last_fault.map_or(Value::Null, |f| {
        json!({
            "step": f.step,
            "code": f.code(),
            "recovery": f.recovery.name(),
            "particle": f.particle,
            "strain": finite_json(f.strain),
        })
    });
    json!({
        "name": name,
        "kind": kind,
        "guard": {
            "enabled": h.guard.enabled,
            "max_strain": h.guard.max_strain,
            "stiff_limit": h.guard.stiff_limit,
            "extra_iterations": h.guard.extra_iterations,
            "stiff_target": h.guard.stiff_target,
        },
        "max_strain": finite_json(h.max_strain),
        "peak_strain": finite_json(h.peak_strain),
        "faults": h.faults,
        "non_finite_faults": h.non_finite_faults,
        "runaway_faults": h.runaway_faults,
        "pose_resets": h.pose_resets,
        "extra_iterations": h.extra_iterations,
        "stiff_projections": h.stiff_projections,
        "last_fault": last_fault,
    })
}

/// `physics.cloth_health`: the guard state of every solver and soft body (or the `target`).
fn cloth_health(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if !plugin_present(world) {
        return Ok(json!({ "plugin": false, "solvers": [] }));
    }
    let target = target_name(world, params, false)?;
    if let Some(name) = &target {
        kind_of(world, name)?;
    }
    let wanted = |n: &str| target.as_deref().is_none_or(|t| t == n);
    let mut solvers = Vec::new();
    if let Some(set) = world.get_resource::<ClothSet>() {
        solvers.extend(
            set.iter()
                .filter(|(n, _)| wanted(n))
                .map(|(name, solver)| health_json(name, "cloth", solver)),
        );
    }
    if let Some(set) = world.get_resource::<SoftSet>() {
        solvers.extend(
            set.iter()
                .filter(|(n, _)| wanted(n))
                .map(|(name, soft)| health_json(name, "soft", soft.cloth())),
        );
    }
    Ok(json!({ "plugin": true, "solvers": solvers }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Vec3;
    use regia_physics::cloth::{ClothCollider, ClothConfig, ClothRope, ClothShape};

    #[test]
    fn pin_index_beyond_32_bits_is_out_of_range() {
        let wide = json!({ "particles": [4_294_967_296_u64] });
        assert!(particle_list(&wide, 4).is_err(), "must not wrap onto particle 0");
        assert_eq!(
            particle_list(&json!({ "particles": [3] }), 4).expect("ok"),
            vec![3]
        );
    }

    fn world_with_rope() -> World {
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
        let rope = solver
            .add_rope(&ClothRope {
                start: Vec3::new(0.0, 2.0, 0.0),
                end: Vec3::new(1.0, 2.0, 0.0),
                segments: 4,
                particle_mass: 0.1,
                stretch_compliance: 0.0,
                bend_compliance: 0.1,
            })
            .expect("rope");
        solver.pin(rope.first).expect("pin");
        solver
            .add_collider(ClothCollider {
                shape: ClothShape::Plane {
                    normal: Vec3::Y,
                    offset: 0.0,
                },
                friction: 0.5,
            })
            .expect("floor");
        solver.step(1.0 / 60.0).expect("step");
        let mut set = ClothSet::new();
        set.insert("rope", solver);
        set.insert("empty", ClothSolver::new(ClothConfig::default()).expect("config"));
        let mut world = World::new();
        world.insert_resource(set);
        install_cloth_methods(&mut world);
        world
    }

    fn call(world: &mut World, params: Value) -> Result<Value, RpcError> {
        let handler = world
            .resource::<AgentExtension>()
            .method("physics.cloth_stats")
            .map(|(_, handler)| handler)
            .expect("registered");
        handler(world, &params)
    }

    #[test]
    fn install_registers_a_read_only_method() {
        let mut world = World::new();
        install_cloth_methods(&mut world);
        let extension = world.resource::<AgentExtension>();
        let (schema, _) = extension.method("physics.cloth_stats").expect("registered");
        assert!(!schema.mutating);
        assert_eq!(schema.permission.name(), "read");
        assert_eq!(schema.tool, "physics_cloth_stats");
    }

    #[test]
    fn reports_every_solver_in_insertion_order() {
        let mut world = world_with_rope();
        let result = call(&mut world, json!({})).expect("ok");
        assert_eq!(result["plugin"], true);
        let solvers = result["solvers"].as_array().expect("array");
        assert_eq!(solvers.len(), 2);
        let rope = &solvers[0];
        assert_eq!(rope["name"], "rope");
        assert_eq!(rope["particles"], 5);
        assert_eq!(rope["pinned"], 1);
        assert_eq!(rope["distance_constraints"], 7);
        assert_eq!(rope["bend_constraints"], 0);
        assert_eq!(rope["colliders"], 1);
        assert_eq!(rope["steps"], 1);
        assert_eq!(rope["dropped_steps"], 0);
        assert_eq!(rope["bounds_max"][1], 2.0, "the pinned end is the top");
        let digest = rope["digest"].as_str().expect("string");
        assert_eq!(digest.len(), 16);
        assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()));
        let empty = &solvers[1];
        assert_eq!(empty["particles"], 0);
        assert!(empty["bounds_min"].is_null());
    }

    #[test]
    fn filters_by_name_and_reports_unknown_names() {
        let mut world = world_with_rope();
        let result = call(&mut world, json!({ "name": "rope" })).expect("ok");
        assert_eq!(result["solvers"].as_array().map(Vec::len), Some(1));
        let err = call(&mut world, json!({ "name": "cape" })).expect_err("unknown");
        assert_eq!(err.data.as_ref().map(|d| d["code"].clone()), Some(json!("F0003")));
        let err = call(&mut world, json!({ "name": 3 })).expect_err("not a string");
        assert_eq!(err.data.as_ref().map(|d| d["code"].clone()), Some(json!("F0002")));
    }

    #[test]
    fn answers_without_the_plugin() {
        let mut world = World::new();
        install_cloth_methods(&mut world);
        let result = call(&mut world, json!({})).expect("ok");
        assert_eq!(result, json!({ "plugin": false, "solvers": [] }));
    }

    // -----------------------------------------------------------------------
    // Spec 55 methods
    // -----------------------------------------------------------------------

    use regia_physics::cloth::{ClothGrid, SoftModel, SoftParams, SoftSet, SoftSolver};

    fn call_method(world: &mut World, method: &str, params: Value) -> Result<Value, RpcError> {
        let handler = world
            .resource::<AgentExtension>()
            .method(method)
            .map(|(_, handler)| handler)
            .expect("registered");
        handler(world, &params)
    }

    fn reason(err: &RpcError) -> (String, String) {
        let data = err.data.as_ref().expect("data");
        (
            data["code"].as_str().unwrap_or_default().to_owned(),
            data["reason"].as_str().unwrap_or_default().to_owned(),
        )
    }

    fn flag() -> ClothSolver {
        let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
        solver
            .add_grid(&ClothGrid {
                origin: Vec3::new(0.0, 2.0, 0.0),
                u_axis: Vec3::X,
                v_axis: Vec3::NEG_Y,
                columns: 6,
                rows: 6,
                spacing: 0.1,
                particle_mass: 0.05,
                stretch_compliance: 0.0,
                shear_compliance: 1e-4,
                bend_compliance: 1e-2,
            })
            .expect("grid");
        for r in 0..6 {
            solver.pin(r * 6).expect("pin");
        }
        solver
    }

    fn tetra() -> SoftSolver {
        let p = [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z];
        SoftSolver::new(
            &p,
            SoftModel::Tetra {
                tets: vec![[0, 1, 2, 3]],
                volume_compliance: 0.0,
            },
            SoftParams::default(),
        )
        .expect("tet")
    }

    fn pressure_ball() -> SoftSolver {
        SoftSolver::new(
            &[Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z],
            SoftModel::Pressure {
                triangles: vec![[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]],
                pressure: 1.0,
                volume_compliance: 0.0,
            },
            SoftParams::default(),
        )
        .expect("closed surface")
    }

    #[test]
    fn set_params_echoes_stored_pressure_when_the_patch_omits_it() {
        let mut soft = SoftSet::default();
        soft.insert("ball", pressure_ball());
        let mut world = World::new();
        world.insert_resource(soft);
        install_cloth_methods(&mut world);
        let set = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": { "name": "ball" }, "patch": { "pressure": 1.5 } }),
        )
        .expect("pressure");
        assert_eq!(set["params"]["pressure"], json!(1.5));
        let damped = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": { "name": "ball" }, "patch": { "damping": 0.01 } }),
        )
        .expect("damping");
        let damping = damped["params"]["damping"].as_f64().expect("damping");
        assert!((damping - 0.01).abs() < 1.0e-5, "{damping}");
        let pressure = damped["params"]["pressure"].as_f64().expect("pressure");
        assert!((pressure - 1.5).abs() < 1.0e-5, "{pressure}");
    }

    fn world_with_flag() -> World {
        let mut set = ClothSet::new();
        set.insert("flag", flag());
        let mut soft = SoftSet::default();
        soft.insert("jelly", tetra());
        let mut world = World::new();
        world.insert_resource(set);
        world.insert_resource(soft);
        install_cloth_methods(&mut world);
        world
    }

    #[test]
    fn install_registers_every_spec55_method_with_its_class() {
        let mut world = World::new();
        install_cloth_methods(&mut world);
        let extension = world.resource::<AgentExtension>();
        for (method, permission) in [
            ("physics.cloth_stats", "read"),
            ("physics.cloth_inspect", "read"),
            ("physics.cloth_set_params", "play"),
            ("physics.cloth_pin", "play"),
            ("physics.cloth_unpin", "play"),
            ("physics.cloth_wind", "play"),
            ("physics.cloth_step", "play"),
            ("physics.cloth_reset", "play"),
            ("physics.cloth_presets", "read"),
            ("physics.cloth_set_layers", "play"),
            ("physics.cloth_health", "read"),
        ] {
            let (schema, _) = extension.method(method).expect(method);
            assert_eq!(schema.permission.name(), permission, "{method}");
            assert_eq!(schema.mutating, permission == "play", "{method}");
            assert_eq!(schema.tool, method.replace('.', "_"), "{method}");
        }
    }

    #[test]
    fn presets_lists_every_row_without_the_plugin() {
        let mut world = World::new();
        install_cloth_methods(&mut world);
        let r = call_method(&mut world, "physics.cloth_presets", json!({})).expect("presets");
        let list = r["presets"].as_array().expect("array");
        assert_eq!(list.len(), ClothPreset::ALL.len());
        for (entry, preset) in list.iter().zip(ClothPreset::ALL) {
            assert_eq!(entry["name"], json!(preset.name()));
            assert_eq!(entry["density"], json!(preset.row().density));
            assert!(entry["strain_cap"]["max"].as_f64().is_some_and(|m| m >= 1.0));
        }
        assert_eq!(list[0]["name"], "light_fabric");
    }

    #[test]
    fn edit_map_is_registered_as_edit_scene() {
        let mut world = World::new();
        install_cloth_methods(&mut world);
        let extension = world.resource::<AgentExtension>();
        let (schema, _) = extension.method("physics.cloth_edit_map").expect("registered");
        assert_eq!(schema.permission.name(), "edit_scene");
        assert!(schema.mutating);
        assert_eq!(schema.tool, "physics_cloth_edit_map");
    }

    /// An app with the cloth glue and one `ClothSim` (a 4 x 4 sheet, top row pinned) after a
    /// few fixed steps.
    fn app_with_cloth_sim() -> (App, Entity) {
        use regia_physics::cloth::asset::{ClothAsset, ClothSource, SimMeshInput};
        let mut mesh = SimMeshInput::default();
        for r in 0..4u32 {
            for c in 0..4u32 {
                mesh.positions
                    .push(Vec3::new(c as f32 * 0.1, -(r as f32) * 0.1, 0.0));
            }
        }
        for r in 0..3u32 {
            for c in 0..3u32 {
                let i = r * 4 + c;
                mesh.triangles.push([i, i + 4, i + 1]);
                mesh.triangles.push([i + 1, i + 4, i + 5]);
            }
        }
        let source = ClothSource::from_ron_str(
            r#"(format_version: 1, name: "banner", sim_mesh: (source: "b.glb", node: "B"),
                maps: { "reach": Sparse(default: 1.0, entries: [(0, 0.0), (1, 0.0), (2, 0.0), (3, 0.0)]) })"#,
        )
        .expect("parses");
        let asset = std::sync::Arc::new(ClothAsset::cook(&source, &mesh, None, &[]).expect("cooks"));
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .insert_resource(TimeUpdateStrategy::ManualDuration(
                std::time::Duration::from_secs_f64(1.0 / 60.0),
            ));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed);
        app.add_plugins(regia_framework::cloth::ClothGluePlugin);
        let entity = app
            .world_mut()
            .spawn((
                ClothSim::new(asset),
                regia_scene::Transform::default(),
                regia_scene::GlobalTransform::default(),
            ))
            .id();
        for _ in 0..12 {
            app.update();
        }
        install_cloth_methods(app.world_mut());
        (app, entity)
    }

    #[test]
    fn inspect_shows_the_section_and_instances_that_failed() {
        let (mut app, entity) = app_with_cloth_sim();
        let material = std::sync::Arc::new(regia_render::CpuMaterial::default());
        let empty = std::sync::Arc::new(regia_render::CpuMesh {
            vertices: Vec::new(),
            indices: Vec::new(),
        });
        let section = app
            .world_mut()
            .spawn((
                regia_render::MeshRenderer::new(empty, material),
                regia_scene::Transform::default(),
                regia_scene::GlobalTransform::default(),
            ))
            .id();
        app.world_mut().get_mut::<ClothSim>(entity).expect("sim").section = Some(section);
        app.world_mut().spawn(ClothSourceState {
            asset: "missing.cloth.ron".into(),
            id: None,
            generation: 0,
            error: Some("cloth_asset_invalid".into()),
        });
        for _ in 0..3 {
            app.update();
        }
        let result = call_method(app.world_mut(), "physics.cloth_inspect", json!({})).expect("inspect");
        let instance = &result["solvers"][0]["instance"];
        assert_eq!(instance["entity"], json!(entity.to_bits()));
        assert_eq!(instance["asset"], json!("banner"));
        assert_eq!(instance["section"], json!(section.to_bits()));
        assert_eq!(instance["section_vertices"], json!(16));
        assert_eq!(instance["section_triangles"], json!(18));
        assert_eq!(instance["hidden_triangles"], json!(0));
        assert_eq!(result["failed"][0]["asset"], json!("missing.cloth.ron"));
        assert_eq!(result["failed"][0]["error"], json!("cloth_asset_invalid"));
    }

    #[test]
    fn entity_targets_resolve_through_cloth_sim() {
        let (mut app, entity) = app_with_cloth_sim();
        let result = call_method(
            app.world_mut(),
            "physics.cloth_inspect",
            json!({ "target": { "entity": entity.to_bits() } }),
        )
        .expect("inspect");
        let solvers = result["solvers"].as_array().expect("solvers");
        assert_eq!(solvers.len(), 1);
        assert_eq!(solvers[0]["name"], json!(format!("cloth:{}", entity.to_bits())));
        let stepped = call_method(
            app.world_mut(),
            "physics.cloth_step",
            json!({ "target": { "entity": entity.to_bits() }, "count": 5 }),
        )
        .expect("step");
        assert_eq!(stepped["steps_run"], json!(5));
    }

    #[test]
    fn edit_map_edits_the_asset_of_every_instance_and_rebuilds() {
        let (mut app, entity) = app_with_cloth_sim();
        let before = app.world().get::<ClothSim>(entity).expect("sim").asset.clone();
        let result = call_method(
            app.world_mut(),
            "physics.cloth_edit_map",
            json!({ "asset": "banner", "map": "pose_pull",
                    "edit": { "op": "sphere", "center": [0.0, 0.0, 0.0], "radius": 0.15, "value": 1.0,
                              "falloff": "hard", "strength": 1.0 } }),
        )
        .expect("edit");
        assert_eq!(result["instances"], json!(1));
        assert_eq!(result["dirty"], json!(true));
        let after = app.world().get::<ClothSim>(entity).expect("sim").asset.clone();
        assert!(!std::sync::Arc::ptr_eq(&before, &after));
        let pull = &after.lod0().maps.pose_pull;
        // Vertices 0, 1 (x = 0.1), 4 (y = -0.1) and the diagonal 5 are within 0.15 m of the origin.
        assert_eq!(pull[0], 1.0);
        assert_eq!(pull[1], 1.0);
        assert_eq!(pull[4], 1.0);
        assert_eq!(pull[5], 1.0);
        assert_eq!(pull[2], 0.0);
        assert_eq!(pull[6], 0.0);
        // The same edit through the API gives an equal map.
        let mut direct = (*before).clone();
        apply_map_edit(
            &mut direct,
            0,
            "pose_pull",
            &MapEdit::Sphere {
                center: [0.0, 0.0, 0.0],
                radius: 0.15,
                value: 1.0,
                falloff: regia_physics::cloth::maps::Falloff::Hard,
                strength: 1.0,
            },
        )
        .expect("direct");
        assert_eq!(direct.lod0().maps, after.lod0().maps);
        app.update();
        app.update();
        let name = format!("cloth:{}", entity.to_bits());
        let steps = app
            .world()
            .resource::<ClothSet>()
            .get(&name)
            .expect("solver")
            .stats()
            .steps;
        assert!(steps < 12, "rebuilt after the edit: {steps}");

        let err = call_method(
            app.world_mut(),
            "physics.cloth_edit_map",
            json!({ "asset": "banner", "map": "pose_pull", "edit": { "op": "set", "vertices": [[0, 2.0]] } }),
        )
        .expect_err("out of range");
        assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));
        let err = call_method(
            app.world_mut(),
            "physics.cloth_edit_map",
            json!({ "asset": "nope", "map": "reach", "edit": { "op": "set", "vertices": [] } }),
        )
        .expect_err("unknown asset");
        assert_eq!(reason(&err), ("F0003".into(), "cloth_not_found".into()));
        let err = call_method(
            app.world_mut(),
            "physics.cloth_edit_map",
            json!({ "asset": "banner", "map": "reach", "edit": { "op": "paint" } }),
        )
        .expect_err("unknown op");
        assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));
    }

    #[test]
    fn inspect_reports_state_soft_volume_and_a_particle_slice() {
        let mut world = world_with_flag();
        let result = call_method(
            &mut world,
            "physics.cloth_inspect",
            json!({ "particles": { "from": 2, "count": 3 } }),
        )
        .expect("ok");
        assert_eq!(result["plugin"], true);
        let solvers = result["solvers"].as_array().expect("array");
        assert_eq!(solvers.len(), 2);
        let flag = &solvers[0];
        assert_eq!(flag["kind"], "cloth");
        assert_eq!(flag["state"], "running");
        assert_eq!(flag["blend"], 1.0);
        assert_eq!(flag["lod"], 0);
        assert_eq!(flag["pinned"], 6);
        let slice = flag["particles"].as_array().expect("slice");
        assert_eq!(slice.len(), 3);
        assert_eq!(slice[0]["index"], 2);
        let jelly = &solvers[1];
        assert_eq!(jelly["kind"], "soft");
        assert_eq!(jelly["model"], "tetra");
        let rest = jelly["rest_volume"].as_f64().expect("number");
        assert!((rest - 1.0 / 6.0).abs() < 1e-6, "{rest}");
        let one = call_method(
            &mut world,
            "physics.cloth_inspect",
            json!({ "target": { "name": "flag" } }),
        )
        .expect("ok");
        assert_eq!(one["solvers"].as_array().map(Vec::len), Some(1));
        let err = call_method(
            &mut world,
            "physics.cloth_inspect",
            json!({ "particles": { "from": 0, "count": 300 } }),
        )
        .expect_err("too many");
        assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));
        let err = call_method(
            &mut world,
            "physics.cloth_inspect",
            json!({ "target": { "name": "cape" } }),
        )
        .expect_err("unknown");
        assert_eq!(reason(&err), ("F0003".into(), "cloth_not_found".into()));
        let err = call_method(
            &mut world,
            "physics.cloth_inspect",
            json!({ "target": { "entity": 4 } }),
        )
        .expect_err("entity");
        assert_eq!(reason(&err), ("F0003".into(), "cloth_not_found".into()));
    }

    #[test]
    fn step_limits_and_digest_match_manual_stepping() {
        let mut world = world_with_flag();
        let mut manual = flag();
        manual.step_fixed(30);
        let result = call_method(
            &mut world,
            "physics.cloth_step",
            json!({ "target": { "name": "flag" }, "count": 30 }),
        )
        .expect("ok");
        assert_eq!(result["steps_run"], 30);
        assert_eq!(result["digest"], format!("{:016x}", manual.state_digest()));
        let err =
            call_method(&mut world, "physics.cloth_step", json!({ "count": 601 })).expect_err("too many");
        assert_eq!(
            reason(&err),
            ("F0002".into(), "cloth_step_count_too_large".into())
        );
        let err = call_method(&mut world, "physics.cloth_step", json!({ "count": 0 })).expect_err("zero");
        assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));
        let all = call_method(&mut world, "physics.cloth_step", json!({ "count": 2 })).expect("all");
        assert_eq!(all["digest"].as_str().map(str::len), Some(16));
    }

    /// Spec 55 § 5: `physics.cloth_pin` and `physics.cloth_unpin` take at most 1,024 particles
    /// per call; a longer list is `cloth_bad_param` and changes nothing.
    #[test]
    fn pin_lists_are_capped_at_1024_particles() {
        let mut world = world_with_flag();
        let target = json!({ "name": "flag" });
        let pinned = |world: &mut World| {
            call_method(world, "physics.cloth_stats", json!({ "name": "flag" })).expect("stats")["solvers"][0]
                ["pinned"]
                .as_u64()
                .expect("pinned")
        };
        let before = pinned(&mut world);
        for method in ["physics.cloth_pin", "physics.cloth_unpin"] {
            let err = call_method(
                &mut world,
                method,
                json!({ "target": target, "particles": vec![1; 1025] }),
            )
            .expect_err("too many");
            assert_eq!(
                reason(&err),
                ("F0002".into(), "cloth_bad_param".into()),
                "{method}"
            );
        }
        assert_eq!(pinned(&mut world), before, "a refused list pins nothing");
        let r = call_method(
            &mut world,
            "physics.cloth_pin",
            json!({ "target": target, "particles": vec![1; 1024] }),
        )
        .expect("exactly the cap");
        assert_eq!(r["pinned"].as_u64(), Some(before + 1), "particle 1 was free");
    }

    #[test]
    fn pin_unpin_wind_reset_and_set_params_drive_the_solver() {
        let mut world = world_with_flag();
        let target = json!({ "name": "flag" });
        let r = call_method(
            &mut world,
            "physics.cloth_pin",
            json!({ "target": target, "particles": [1, 2] }),
        )
        .expect("pin");
        assert_eq!(r["pinned"], 8);
        let r = call_method(
            &mut world,
            "physics.cloth_unpin",
            json!({ "target": target, "particles": [1, 2, 6] }),
        )
        .expect("unpin");
        assert_eq!(r["unpinned"], 3);
        assert_eq!(r["pinned"], 5);
        let err = call_method(
            &mut world,
            "physics.cloth_pin",
            json!({ "target": target, "particles": [99] }),
        )
        .expect_err("range");
        assert_eq!(
            reason(&err),
            ("F0002".into(), "cloth_particle_out_of_range".into())
        );

        // Wind pushes the flag downwind.
        call_method(
            &mut world,
            "physics.cloth_pin",
            json!({ "target": target, "particles": [6] }),
        )
        .expect("pin");
        call_method(
            &mut world,
            "physics.cloth_step",
            json!({ "target": target, "count": 60 }),
        )
        .expect("settle");
        let before = world.resource::<ClothSet>().get("flag").expect("flag").centroid();
        let r = call_method(
            &mut world,
            "physics.cloth_wind",
            json!({ "target": target, "velocity": [0.0, 0.0, 12.0] }),
        )
        .expect("wind");
        assert_eq!(r["wind"], json!([0.0, 0.0, 12.0]));
        call_method(
            &mut world,
            "physics.cloth_step",
            json!({ "target": target, "count": 120 }),
        )
        .expect("blow");
        let after = world.resource::<ClothSet>().get("flag").expect("flag").centroid();
        assert!(after.z - before.z > 0.05, "moved {}", after.z - before.z);
        call_method(
            &mut world,
            "physics.cloth_wind",
            json!({ "velocity": [1.0, 0.0, 0.0] }),
        )
        .expect("global");
        assert_eq!(world.resource::<ClothSet>().global_wind(), Vec3::X);
        call_method(
            &mut world,
            "physics.cloth_wind",
            json!({ "target": target, "clear": true }),
        )
        .expect("clear");

        // Parameters: applied as a whole, or not at all.
        let r = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": target, "patch": { "damping": 0.4, "self_collision": { "mode": "spheres", "radius": 0.02 }, "lod": 1 } }),
        )
        .expect("params");
        assert_eq!(
            r["params"]["damping"].as_f64().map(|d| (d - 0.4).abs() < 1e-6),
            Some(true)
        );
        assert_eq!(r["params"]["self_collision"]["mode"], "spheres");
        assert_eq!(r["params"]["lod"], 1);
        let err = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": target, "patch": { "damping": 0.9, "inertia_scale": 2.0 } }),
        )
        .expect_err("range");
        assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));
        let damping = world
            .resource::<ClothSet>()
            .get("flag")
            .expect("flag")
            .config()
            .damping;
        assert!((damping - 0.4).abs() < 1e-6, "rejected patch changed nothing");
        let err = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": target, "patch": { "colour": 1 } }),
        )
        .expect_err("unknown");
        assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));

        // Presets: a patch with `preset` applies the row first, other keys refine it.
        let r = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": target, "patch": { "preset": "silk", "damping": 1.5 } }),
        )
        .expect("preset");
        assert_eq!(r["applied"], json!(["damping", "preset"]));
        let silk = ClothPreset::Silk.row();
        assert_eq!(r["params"]["substeps"], json!(silk.substeps));
        assert!((r["params"]["damping"].as_f64().unwrap_or(0.0) - 1.5).abs() < 1e-6);
        assert!(
            (r["params"]["wind_model"]["lift"].as_f64().unwrap_or(0.0) - f64::from(silk.wind_lift)).abs()
                < 1e-6
        );
        let before = world
            .resource::<ClothSet>()
            .get("flag")
            .expect("flag")
            .state_digest();
        let err = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": target, "patch": { "damping": 0.2, "preset": "tweed" } }),
        )
        .expect_err("unknown preset");
        assert_eq!(reason(&err), ("F0002".into(), "unknown_cloth_preset".into()));
        let err = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": target, "patch": { "preset": 3 } }),
        )
        .expect_err("not a string");
        assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));
        let solver = world.resource::<ClothSet>().get("flag").expect("flag");
        assert_eq!(solver.state_digest(), before);
        assert!(
            (solver.config().damping - 1.5).abs() < 1e-6,
            "rejected patch changed nothing"
        );
        let err = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": { "name": "jelly" }, "patch": { "preset": "silk" } }),
        )
        .expect_err("soft body");
        assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));

        let r = call_method(&mut world, "physics.cloth_reset", json!({ "target": target })).expect("reset");
        assert_eq!(r["state"], "settling");
        let r = call_method(
            &mut world,
            "physics.cloth_reset",
            json!({ "target": { "name": "jelly" } }),
        )
        .expect("reset soft");
        assert_eq!(r["state"], "settling");
    }

    #[test]
    fn pressure_patch_rejects_other_models_without_partial_writes() {
        let mut world = world_with_flag();
        let positions = [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z];
        world.resource_mut::<SoftSet>().insert(
            "shape",
            SoftSolver::new(
                &positions,
                SoftModel::ShapeMatch {
                    clusters: vec![vec![0, 1, 2, 3]],
                    stiffness: 0.5,
                    beta: 0.0,
                },
                SoftParams::default(),
            )
            .expect("shape"),
        );
        for name in ["jelly", "shape"] {
            let before = world.resource::<SoftSet>().get(name).expect("soft");
            let before_params = params_json(before.cloth());
            let before_digest = before.state_digest();
            let err = call_method(
                &mut world,
                "physics.cloth_set_params",
                json!({ "target": { "name": name }, "patch": {
                    "damping": 0.05, "gravity_scale": 0.5, "blend": 0.5, "lod": 1, "pressure": 1.0
                } }),
            )
            .expect_err("only pressure models accept pressure");
            assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));
            let after = world.resource::<SoftSet>().get(name).expect("soft");
            assert_eq!(
                params_json(after.cloth()),
                before_params,
                "{name}: rejected patch changed settings"
            );
            assert_eq!(after.state_digest(), before_digest);
        }
    }

    #[test]
    fn pressure_patch_clamps_and_reports_stored_value() {
        let mut world = world_with_flag();
        world.resource_mut::<SoftSet>().insert(
            "balloon",
            SoftSolver::new(
                &[Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z],
                SoftModel::Pressure {
                    triangles: vec![[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]],
                    pressure: 1.0,
                    volume_compliance: 0.0,
                },
                SoftParams::default(),
            )
            .expect("closed surface"),
        );
        // Spec 55 section 2.8 and the agent guide document the solver's clamping contract.
        for (requested, expected) in [
            (-1.0_f32, 0.05_f32),
            (100.0, 8.0),
            (0.05, 0.05),
            (8.0, 8.0),
            (1.5, 1.5),
        ] {
            let result = call_method(
                &mut world,
                "physics.cloth_set_params",
                json!({ "target": { "name": "balloon" }, "patch": { "pressure": requested } }),
            )
            .expect("finite pressure is clamped");
            let stored = world
                .resource::<SoftSet>()
                .get("balloon")
                .expect("soft")
                .pressure();
            assert_eq!(stored, Some(expected));
            assert_eq!(
                result["params"]["pressure"],
                json!(stored),
                "response must report stored pressure"
            );
        }
    }

    #[test]
    fn oversized_u32_patch_fields_are_rejected_without_writes() {
        let mut world = world_with_flag();
        for value in [u64::from(u32::MAX) + 1, 5_000_000_000, u64::MAX] {
            for patch in [
                json!({ "damping": 0.05, "tear": { "strain": 2.0, "max_per_step": value } }),
                json!({ "damping": 0.05, "substeps": value }),
                json!({ "damping": 0.05, "iterations": value }),
                json!({ "damping": 0.05, "exclude_rings": value }),
            ] {
                let before = world.resource::<ClothSet>().get("flag").expect("flag");
                let before_params = params_json(before);
                let before_digest = before.state_digest();
                let err = call_method(
                    &mut world,
                    "physics.cloth_set_params",
                    json!({ "target": { "name": "flag" }, "patch": patch }),
                )
                .expect_err("integer does not fit u32");
                assert_eq!(
                    reason(&err),
                    ("F0002".into(), "cloth_bad_param".into()),
                    "{patch}"
                );
                let after = world.resource::<ClothSet>().get("flag").expect("flag");
                assert_eq!(params_json(after), before_params);
                assert_eq!(after.state_digest(), before_digest);
            }
        }
        let accepted = call_method(
            &mut world,
            "physics.cloth_set_params",
            json!({ "target": { "name": "flag" }, "patch": { "tear": { "max_per_step": u32::MAX } } }),
        )
        .expect("u32 boundary is representable");
        assert_eq!(accepted["params"]["tear"]["max_per_step"], u32::MAX);
    }

    #[test]
    fn oversized_gust_integers_leave_wind_and_gust_unchanged() {
        for key in ["period_steps", "seed"] {
            for value in [u64::from(u32::MAX) + 1, 5_000_000_000, u64::MAX] {
                let mut world = world_with_flag();
                call_method(
                    &mut world,
                    "physics.cloth_wind",
                    json!({ "target": { "name": "flag" }, "velocity": [1, 2, 3],
                        "gust": { "amplitude": 0.5, "period_steps": 7, "seed": 23 } }),
                )
                .expect("initial wind");
                let mut reference = world.resource::<ClothSet>().get("flag").expect("flag").clone();
                let mut params = json!({ "target": { "name": "flag" }, "velocity": [9, 0, 0],
                    "gust": { "amplitude": 0.5, "period_steps": 7, "seed": 23 } });
                params["gust"][key] = json!(value);
                let err = call_method(&mut world, "physics.cloth_wind", params)
                    .expect_err("integer does not fit u32");
                assert_eq!(
                    reason(&err),
                    ("F0002".into(), "cloth_bad_param".into()),
                    "{key}={value}"
                );
                let mut set = world.resource_mut::<ClothSet>();
                let after = set.get_mut("flag").expect("flag");
                assert_eq!(after.wind(), reference.wind());
                // Gust has no getter: fixed steps must still match the original gust exactly.
                after.step_fixed(20);
                reference.step_fixed(20);
                assert_eq!(after.state_digest(), reference.state_digest());
                assert_eq!(after.diagnostics().wind, reference.diagnostics().wind);
            }
        }
        let mut world = world_with_flag();
        call_method(
            &mut world,
            "physics.cloth_wind",
            json!({ "target": { "name": "flag" }, "velocity": [0, 0, 0],
                "gust": { "amplitude": 0.5, "period_steps": u32::MAX, "seed": u32::MAX } }),
        )
        .expect("u32 boundaries are representable");
    }

    #[test]
    fn play_methods_without_the_plugin_say_so() {
        let mut world = World::new();
        install_cloth_methods(&mut world);
        let read = call_method(&mut world, "physics.cloth_inspect", json!({})).expect("ok");
        assert_eq!(read, json!({ "plugin": false, "solvers": [] }));
        for (method, params) in [
            ("physics.cloth_step", json!({ "count": 1 })),
            ("physics.cloth_reset", json!({ "target": { "name": "flag" } })),
            ("physics.cloth_wind", json!({ "velocity": [1, 0, 0] })),
        ] {
            let err = call_method(&mut world, method, params).expect_err(method);
            assert_eq!(
                reason(&err),
                ("F0003".into(), "cloth_plugin_missing".into()),
                "{method}"
            );
        }
    }

    /// ADR-2425: `physics.cloth_set_layers` puts a vertex set on a layer, checks everything
    /// before it changes the solver, and `physics.cloth_inspect` reports the layers, the gap,
    /// vertex-triangle crossings and whether the set steps in parallel.
    #[test]
    fn set_layers_assigns_checks_and_shows_in_inspect() {
        let mut world = world_with_flag();
        let call = |world: &mut World, params: Value| call_method(world, "physics.cloth_set_layers", params);
        let r = call(
            &mut world,
            json!({ "target": { "name": "flag" }, "range": { "from": 18, "count": 18 }, "layer": 1 }),
        )
        .expect("range");
        assert_eq!(r, json!({ "max_layer": 1, "layered": 18, "layer_gap": null }));
        let r = call(
            &mut world,
            json!({ "target": { "name": "flag" }, "particles": [0, 1], "layer": 2, "layer_gap": 0.03 }),
        )
        .expect("list");
        assert_eq!(r["max_layer"], 2);
        assert_eq!(r["layered"], 20);
        assert!((r["layer_gap"].as_f64().expect("gap") - 0.03).abs() < 1e-6);

        let digest = world
            .resource::<ClothSet>()
            .get("flag")
            .expect("flag")
            .state_digest();
        for (params, code) in [
            (
                json!({ "target": { "name": "flag" }, "range": { "from": 30, "count": 7 }, "layer": 1 }),
                "cloth_particle_out_of_range",
            ),
            (
                json!({ "target": { "name": "flag" }, "particles": [36], "layer": 1 }),
                "cloth_particle_out_of_range",
            ),
            (
                json!({ "target": { "name": "flag" }, "particles": [0], "layer": 256 }),
                "cloth_bad_param",
            ),
            (
                json!({ "target": { "name": "flag" }, "particles": [0] }),
                "cloth_bad_param",
            ),
            (
                json!({ "target": { "name": "flag" }, "particles": [0], "range": { "from": 0, "count": 1 }, "layer": 1 }),
                "cloth_bad_param",
            ),
            (
                json!({ "target": { "name": "flag" }, "layer_gap": -1.0 }),
                "cloth_bad_param",
            ),
            (json!({ "target": { "name": "flag" } }), "cloth_bad_param"),
            (
                json!({ "target": { "name": "flag" }, "clear": 1 }),
                "cloth_bad_param",
            ),
        ] {
            let err = call(&mut world, params.clone()).expect_err("refused");
            assert_eq!(reason(&err).1, code, "{params}");
        }
        let err =
            call(&mut world, json!({ "target": { "name": "nope" }, "clear": true })).expect_err("unknown");
        assert_eq!(reason(&err), ("F0003".into(), "cloth_not_found".into()));
        let after = world
            .resource::<ClothSet>()
            .get("flag")
            .expect("flag")
            .state_digest();
        assert_eq!(digest, after, "refused calls change nothing");

        let inspect = call_method(
            &mut world,
            "physics.cloth_inspect",
            json!({ "target": { "name": "flag" } }),
        )
        .expect("inspect");
        assert_eq!(inspect["parallel"], false);
        let flag = &inspect["solvers"][0];
        assert_eq!(flag["max_layer"], 2);
        assert_eq!(flag["layered"], 20);
        assert_eq!(flag["self_crossings"], 0);

        let r = call(
            &mut world,
            json!({ "target": { "name": "flag" }, "clear": true, "layer_gap": null }),
        )
        .expect("clear");
        assert_eq!(r, json!({ "max_layer": 0, "layered": 0, "layer_gap": null }));
        let solver = world.resource::<ClothSet>().get("flag").expect("flag");
        assert!(solver.layers().is_empty());

        world.resource_mut::<ClothSet>().set_parallel(true);
        let inspect = call_method(&mut world, "physics.cloth_inspect", json!({})).expect("inspect");
        assert_eq!(inspect["parallel"], true);
    }

    /// ADR-3273: `physics.cloth_health` reports the guard of every solver; a fault shows its
    /// code and recovery; unknown targets and a missing plugin answer like the other methods.
    #[test]
    fn health_reports_guard_faults() {
        let mut world = world_with_rope();
        let r = call_method(&mut world, "physics.cloth_health", json!({})).expect("health");
        assert_eq!(r["plugin"], true);
        let rope = &r["solvers"][0];
        assert_eq!(rope["name"], "rope");
        assert_eq!(rope["kind"], "cloth");
        assert_eq!(rope["guard"]["enabled"], true);
        assert_eq!(
            rope["guard"]["max_strain"],
            json!(regia_physics::cloth::DEFAULT_GUARD_STRAIN)
        );
        assert_eq!(
            rope["guard"]["stiff_target"],
            json!(regia_physics::cloth::DEFAULT_STIFF_TARGET)
        );
        assert_eq!(rope["faults"], 0);
        assert!(rope["last_fault"].is_null());
        assert!(rope["max_strain"].as_f64().is_some_and(|s| s > 0.9 && s < 1.1));

        // A particle far enough out that the next step overflows to NaN: the guard restores.
        {
            let mut set = world.resource_mut::<ClothSet>();
            let solver = set.get_mut("rope").expect("rope");
            solver.set_position(3, Vec3::splat(1.0e38)).expect("finite");
            solver.step(1.0 / 60.0).expect("step");
        }
        let r = call_method(
            &mut world,
            "physics.cloth_health",
            json!({ "target": { "name": "rope" } }),
        )
        .expect("health");
        let solvers = r["solvers"].as_array().expect("array");
        assert_eq!(solvers.len(), 1);
        let rope = &solvers[0];
        assert_eq!(rope["faults"], 1);
        assert_eq!(rope["non_finite_faults"], 1);
        assert_eq!(rope["runaway_faults"], 0);
        assert_eq!(rope["last_fault"]["code"], "cloth_fault_non_finite");
        assert_eq!(rope["last_fault"]["recovery"], "last_good");
        assert_eq!(rope["last_fault"]["step"], 1);

        let err = call_method(
            &mut world,
            "physics.cloth_health",
            json!({ "target": { "name": "cape" } }),
        )
        .expect_err("unknown");
        assert_eq!(reason(&err), ("F0003".into(), "cloth_not_found".into()));
        let err = call_method(&mut world, "physics.cloth_health", json!({ "target": 3 })).expect_err("bad");
        assert_eq!(reason(&err), ("F0002".into(), "cloth_bad_param".into()));

        let mut bare = World::new();
        install_cloth_methods(&mut bare);
        let r = call_method(&mut bare, "physics.cloth_health", json!({})).expect("no plugin");
        assert_eq!(r, json!({ "plugin": false, "solvers": [] }));
        let (schema, _) = bare
            .resource::<AgentExtension>()
            .method("physics.cloth_health")
            .expect("registered");
        assert!(!schema.mutating);
        assert_eq!(schema.permission.name(), "read");
    }

    /// `physics.cloth_inspect` shows the scene wind (ADR-2884): `scene_wind` at the top, each
    /// solver's `global_wind`, and per instance the wind it received after `wind_scale`.
    #[test]
    fn inspect_shows_the_scene_wind_each_cloth_received() {
        let (mut app, entity) = app_with_cloth_sim();
        let quiet = call_method(app.world_mut(), "physics.cloth_inspect", json!({})).expect("inspect");
        assert_eq!(quiet["scene_wind"], Value::Null);
        assert_eq!(
            quiet["solvers"][0]["instance"]["scene_wind"],
            json!([0.0, 0.0, 0.0])
        );
        app.insert_resource(regia_scene::weather::SceneWind::manual([3.0, -1.0]));
        for _ in 0..3 {
            app.update();
        }
        let windy = call_method(
            app.world_mut(),
            "physics.cloth_inspect",
            json!({ "target": { "entity": entity.to_bits() } }),
        )
        .expect("inspect");
        assert_eq!(windy["scene_wind"]["mode"], json!("manual"));
        assert_eq!(windy["scene_wind"]["cloth"], json!([3.0, 0.0, -1.0]));
        assert_eq!(
            windy["solvers"][0]["instance"]["scene_wind"],
            json!([3.0, 0.0, -1.0])
        );
        assert_eq!(windy["solvers"][0]["global_wind"], json!([3.0, 0.0, -1.0]));
    }

    // -----------------------------------------------------------------------
    // ADR-9070: the tuning fields `physics.cloth_set_params` gained
    // -----------------------------------------------------------------------

    fn set_params(world: &mut World, name: &str, patch: Value) -> Result<Value, RpcError> {
        call_method(
            world,
            "physics.cloth_set_params",
            json!({ "target": { "name": name }, "patch": patch }),
        )
    }

    /// Steps `name` 60 fixed steps through `physics.cloth_step` and returns its centroid.
    fn step_centroid(world: &mut World, name: &str) -> Vec3 {
        call_method(
            world,
            "physics.cloth_step",
            json!({ "target": { "name": name }, "count": 60 }),
        )
        .expect("step");
        solver_mut(world, name).expect("solver").centroid()
    }

    /// Each new cloth field moves the flag away from a control flag that got no patch.
    #[test]
    fn every_new_cloth_tuning_field_changes_the_sim() {
        let control = step_centroid(&mut world_with_flag(), "flag");
        // gravity_override: gravity across the sheet (+z) swings the flag out of its plane.
        let mut world = world_with_flag();
        let r =
            set_params(&mut world, "flag", json!({ "gravity_override": [0.0, 0.0, 8.0] })).expect("override");
        assert_eq!(r["applied"], json!(["gravity_override"]));
        assert_eq!(r["params"]["gravity_override"], json!([0.0, 0.0, 8.0]));
        let swung = step_centroid(&mut world, "flag");
        assert!(
            swung.z > control.z + 0.05,
            "override {swung} vs control {control}"
        );
        // ... and null gives the config gravity back.
        let mut world = world_with_flag();
        set_params(&mut world, "flag", json!({ "gravity_override": [0.0, 0.0, 8.0] })).expect("override");
        let r = set_params(&mut world, "flag", json!({ "gravity_override": null })).expect("cleared");
        assert!(r["params"]["gravity_override"].is_null());
        assert_eq!(
            step_centroid(&mut world, "flag"),
            control,
            "cleared override is the control"
        );

        // stretch_compliance: softer links let the flag sag lower.
        let mut world = world_with_flag();
        let r = set_params(&mut world, "flag", json!({ "stretch_compliance": 1e-2 })).expect("stretch");
        assert_eq!(r["params"]["stretch_compliance"], json!(1e-2_f32));
        let sagged = step_centroid(&mut world, "flag");
        assert!(
            sagged.y < control.y - 1e-3,
            "stretch {sagged} vs control {control}"
        );

        // bend_compliance: a sheet hanging in its own plane never bends, so load it across the
        // plane; a stiff bend (0) and a soft one (1) then fold it differently.
        let swing = |bend: f32| {
            let mut world = world_with_flag();
            let r = set_params(
                &mut world,
                "flag",
                json!({ "gravity_override": [0.0, 0.0, 8.0], "bend_compliance": bend }),
            )
            .expect("bend");
            assert_eq!(r["params"]["bend_compliance"], json!(bend));
            step_centroid(&mut world, "flag")
        };
        let (stiff, soft) = (swing(0.0), swing(1.0));
        assert!(
            stiff.distance(soft) > 1e-4,
            "stiff bend {stiff} vs soft bend {soft}"
        );

        // Bad values are refused with no write.
        let mut world = world_with_flag();
        for patch in [
            json!({ "gravity_override": [1.0, 2.0] }),
            json!({ "gravity_override": "down" }),
            json!({ "stretch_compliance": -1.0 }),
            json!({ "bend_compliance": -1.0 }),
            json!({ "shear_compliance": 1e-3 }),
            json!({ "friction": 0.5 }),
        ] {
            let err = set_params(&mut world, "flag", patch.clone()).expect_err("refused");
            assert_eq!(reason(&err).1, "cloth_bad_param", "{patch}");
        }
        assert_eq!(
            step_centroid(&mut world, "flag"),
            control,
            "refused patches wrote nothing"
        );
    }

    /// `lod_distances` writes the instance's thresholds; the distance LOD follows them.
    #[test]
    fn lod_distances_patch_moves_the_distance_lod() {
        let (mut app, entity) = app_with_cloth_sim();
        let solver = app
            .world()
            .get::<ClothGlueState>(entity)
            .expect("glue")
            .solver()
            .to_string();
        let eye = Vec3::new(0.0, 0.0, 20.0);
        app.world_mut().spawn((
            regia_render::Camera::default(),
            regia_scene::Transform::from_translation(eye),
            regia_scene::GlobalTransform(regia_core::math::Affine3A::from_translation(eye)),
        ));
        for _ in 0..6 {
            app.update();
        }
        let level = |app: &App| app.world().get::<ClothGlueState>(entity).expect("glue").lod();
        assert_eq!(level(&app), 1, "20 m is past the default 15 m");
        let r = set_params(
            app.world_mut(),
            &solver,
            json!({ "lod_distances": [100.0, 200.0, 300.0] }),
        )
        .expect("lod distances");
        assert_eq!(r["params"]["lod_distances"], json!([100.0, 200.0, 300.0]));
        assert_eq!(
            app.world().get::<ClothSim>(entity).expect("sim").lod_distances,
            [100.0, 200.0, 300.0]
        );
        for _ in 0..6 {
            app.update();
        }
        assert_eq!(level(&app), 0, "20 m is near with the patched thresholds");
        for bad in [
            json!([30.0, 20.0, 60.0]),
            json!([1.0, 2.0]),
            json!([-1.0, 2.0, 3.0]),
        ] {
            let err = set_params(app.world_mut(), &solver, json!({ "lod_distances": bad.clone() }))
                .expect_err("refused");
            assert_eq!(reason(&err).1, "cloth_bad_param", "{bad}");
        }
        // A solver with no instance (made by code) has no thresholds to patch.
        let mut world = world_with_flag();
        let err = set_params(&mut world, "flag", json!({ "lod_distances": [1.0, 2.0, 3.0] }))
            .expect_err("no instance");
        assert_eq!(reason(&err).1, "cloth_bad_param");
    }

    fn jelly_cube() -> SoftSolver {
        let mut rest = Vec::new();
        for x in [-0.15f32, 0.15] {
            for y in [-0.15f32, 0.15] {
                for z in [-0.15f32, 0.15] {
                    rest.push(Vec3::new(x, y, z));
                }
            }
        }
        SoftSolver::new(
            &rest,
            SoftModel::ShapeMatch {
                clusters: vec![(0..8).collect()],
                stiffness: 0.5,
                beta: 0.0,
            },
            SoftParams {
                config: ClothConfig {
                    gravity: Vec3::ZERO,
                    ..ClothConfig::default()
                },
                ..SoftParams::default()
            },
        )
        .expect("cube")
    }

    /// Squashes the cube to half height, holds it one second, lets go and runs four seconds
    /// through the agent; returns the height.
    fn squash_through_agent(world: &mut World, name: &str) -> f32 {
        let squash = Vec3::new(std::f32::consts::SQRT_2, 0.5, std::f32::consts::SQRT_2);
        {
            let solver = solver_mut(world, name).expect("solver");
            for i in 0..8 {
                let p = solver.particles()[i].position;
                solver.pin(i).expect("pin");
                solver.set_position(i, p * squash).expect("squash");
            }
        }
        call_method(
            world,
            "physics.cloth_step",
            json!({ "target": { "name": name }, "count": 60 }),
        )
        .expect("hold");
        call_method(
            world,
            "physics.cloth_unpin",
            json!({ "target": { "name": name }, "particles": [0, 1, 2, 3, 4, 5, 6, 7], "inv_mass": 100.0 }),
        )
        .expect("let go");
        for _ in 0..4 {
            call_method(
                world,
                "physics.cloth_step",
                json!({ "target": { "name": name }, "count": 60 }),
            )
            .expect("released");
        }
        let solver = solver_mut(world, name).expect("solver");
        let ys = solver.particles().iter().map(|p| p.position.y);
        let (lo, hi) = ys.fold((f32::MAX, f32::MIN), |(lo, hi), y| (lo.min(y), hi.max(y)));
        hi - lo
    }

    /// `plasticity` on a shape-matching soft body: past the yield the body keeps its squash; the
    /// elastic control springs back; other models refuse it.
    #[test]
    fn plasticity_patch_keeps_a_squash_and_the_elastic_control_returns() {
        let mut world = world_with_flag();
        world.resource_mut::<SoftSet>().insert("elastic", jelly_cube());
        world.resource_mut::<SoftSet>().insert("putty", jelly_cube());
        let r = set_params(
            &mut world,
            "putty",
            json!({ "plasticity": { "yield": 0.1, "creep": 20.0, "max": 1.0 } }),
        )
        .expect("plasticity");
        assert_eq!(
            r["params"]["plasticity"],
            json!({ "yield": 0.1_f32, "creep": 20.0, "max": 1.0 })
        );
        let back = squash_through_agent(&mut world, "elastic");
        let kept = squash_through_agent(&mut world, "putty");
        assert!((back - 0.3).abs() < 0.003, "elastic height {back}");
        assert!((kept - 0.15).abs() < 0.03, "plastic height {kept}");
        let inspect = call_method(
            &mut world,
            "physics.cloth_inspect",
            json!({ "target": { "name": "putty" } }),
        )
        .expect("inspect");
        let strain = inspect["solvers"][0]["plastic_strain"].as_f64().expect("strain");
        assert!(strain > 0.5, "inspect reports the plastic strain: {strain}");
        // Off again, and refused on a tetra body or for a bad value.
        let r = set_params(&mut world, "putty", json!({ "plasticity": null })).expect("off");
        assert!(r["params"]["plasticity"].is_null());
        for (name, patch, code) in [
            (
                "jelly",
                json!({ "plasticity": { "yield": 0.1, "creep": 1.0, "max": 1.0 } }),
                "cloth_bad_param",
            ),
            (
                "putty",
                json!({ "plasticity": { "yield": -0.1, "creep": 1.0, "max": 1.0 } }),
                "cloth_negative",
            ),
            ("putty", json!({ "plasticity": 3 }), "cloth_bad_param"),
            (
                "flag",
                json!({ "plasticity": { "yield": 0.1, "creep": 1.0, "max": 1.0 } }),
                "cloth_bad_param",
            ),
        ] {
            let err = set_params(&mut world, name, patch.clone()).expect_err("refused");
            assert_eq!(reason(&err).1, code, "{name}: {patch}");
        }
        assert!(
            world
                .resource::<SoftSet>()
                .get("putty")
                .and_then(SoftSolver::plasticity)
                .is_none(),
            "a refused patch changed nothing"
        );
    }
}
