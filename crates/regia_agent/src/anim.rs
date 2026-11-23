// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for animation (specs/17, specs/23 § 3): `anim.list`,
//! `anim.get_state` and `anim.set_param` let an agent see what every animator is doing (state
//! machine state, parameters, root motion, ragdoll blend) and drive a graph's parameters at
//! runtime — then verify the result with screenshots or `wait_until`. `anim.clip_report`
//! checks the clips themselves (key data, root motion per lap, compression error), and
//! `anim.ragdoll_set` / `anim.ragdoll_hit` drive a rig's ragdoll (modes, tuning, hits).
//!
//! Handlers are plain functions over `&mut World`; tests drive them without a server.

use std::collections::HashMap;

use regia_animation::{
    Animator, AnimatorSource, Gaze, GazeRig, GazeTarget, InstanceBindPose, ParamValue, RootMotionOutput,
    SkinnedMesh, SpringBones,
};
use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_framework::ragdoll::{Ragdoll, RagdollBodies, RagdollHit, RagdollMode};
use serde_json::{Map, Value, json};

use crate::protocol::RpcError;

fn value_json(value: ParamValue) -> (&'static str, Value) {
    match value {
        ParamValue::Float(v) => ("float", json!(v)),
        ParamValue::Int(v) => ("int", json!(v)),
        ParamValue::Bool(v) => ("bool", json!(v)),
        ParamValue::Vec2(v) => ("vec2", json!([v.x, v.y])),
        ParamValue::Vec3(v) => ("vec3", json!([v.x, v.y, v.z])),
        ParamValue::Trigger => ("trigger", json!(true)),
    }
}

fn vec3_json(v: Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn animator_of(world: &World, entity: Entity) -> Result<&Animator, RpcError> {
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "list animated rigs with anim.list",
        ));
    }
    world.get::<Animator>(entity).ok_or_else(|| {
        RpcError::not_found(
            format!("entity {} has no Animator", entity.to_bits()),
            "list animated rigs with anim.list",
        )
    })
}

/// The ragdoll summary of one rig (shared by `anim.list`, `anim.get_state` and
/// `anim.ragdoll_set`); `Null` when the rig has no [`Ragdoll`].
fn ragdoll_json(world: &World, entity: Entity) -> Value {
    world.get::<Ragdoll>(entity).map_or(Value::Null, |r| {
        let bodies = world.get::<RagdollBodies>(entity);
        json!({
            "mode": match r.mode {
                RagdollMode::Kinematic => "kinematic",
                RagdollMode::Powered => "powered",
                RagdollMode::Limp => "limp",
                RagdollMode::Active => "active",
            },
            "blend": r.blend,
            "weight": bodies.map_or(0.0, |b| b.weight),
            "bodies": bodies.map_or(0, |b| b.bodies.len()),
            "reacting": bodies.map_or(0, |b| b.bodies.iter().filter(|x| x.hit > 0.0).count()),
            "recovering": bodies.is_some_and(|b| b.recovering()),
            "settled": bodies.is_some_and(|b| b.settled()),
            "face_up": bodies.is_some_and(|b| b.face_up()),
            "frozen": bodies.is_some_and(|b| b.frozen()),
        })
    })
}

/// The state summary of one animator (shared by `anim.list` and `anim.get_state`).
fn state_json(world: &World, entity: Entity, animator: &Animator) -> Value {
    let mut parameters = Map::new();
    for (name, value) in animator.parameters.values() {
        let (kind, value) = value_json(value);
        parameters.insert(name, json!({ "type": kind, "value": value }));
    }
    for (name, set) in animator.parameters.triggers() {
        parameters.insert(name, json!({ "type": "trigger", "value": set }));
    }
    let (source, declared) = match &animator.source {
        AnimatorSource::Graph { graph, .. } => (
            "graph",
            graph
                .parameters
                .iter()
                .map(|decl| {
                    let (kind, default) = value_json(decl.1);
                    json!({ "name": decl.0, "type": kind, "default": default })
                })
                .collect::<Vec<_>>(),
        ),
        AnimatorSource::SingleClip { .. } => ("clip", Vec::new()),
    };
    let clip = match &animator.source {
        AnimatorSource::SingleClip { clip, looping } => {
            json!({
                "name": clip.name,
                "duration": clip.duration,
                "looping": looping,
                "morph_tracks": crate::morph::morph_tracks_json(clip),
                "node_tracks": clip.node_animation.as_ref().map_or(0, |a| a.nodes.iter().filter(|n| n.animated()).count()),
            })
        }
        AnimatorSource::Graph { .. } => Value::Null,
    };
    let root_motion = world.get::<RootMotionOutput>(entity).map_or(Value::Null, |r| {
        json!({
            "delta": vec3_json(r.delta),
            "yaw": r.rotation.to_euler(regia_core::glam::EulerRot::YXZ).0,
            "pending": vec3_json(r.pending_translation),
            "pending_seconds": r.pending_seconds,
        })
    });
    let ragdoll = ragdoll_json(world, entity);
    // The first state machine's running transition: mode (crossfade / inertialize /
    // inertialize_spring), progress, and the inertial offset still being decayed.
    let transition = animator.debug_snapshot().and_then(|debug| {
        debug.nodes.iter().find_map(|node| match &node.state {
            regia_animation::debug::NodeDebugState::StateMachine {
                transition,
                blend,
                inertial_offset,
                ..
            } => Some(match transition {
                Some((from, to, progress)) => json!({
                    "machine": node.path,
                    "from": from,
                    "to": to,
                    "progress": progress,
                    "blend": blend.map(|b| b.as_str()),
                    "inertial_offset": inertial_offset,
                }),
                None if *inertial_offset > 0.0 => json!({
                    "machine": node.path,
                    "blend": "inertialize",
                    "inertial_offset": inertial_offset,
                }),
                None => Value::Null,
            }),
            _ => None,
        })
    });
    // Before the first evaluation the graph has no instance: report the nodes at rest.
    let snapshot = animator.debug_snapshot().or_else(|| {
        animator.graph_def().map(|graph| {
            let rest = regia_animation::GraphInstance::from_node(&graph.root);
            regia_animation::debug::graph_debug(graph, &rest, &animator.parameters)
        })
    });
    let warps = snapshot.map_or_else(Vec::new, |debug| warps_json(&debug));
    json!({
        "entity": entity.to_bits(),
        "source": source,
        "state": animator.current_state(),
        "transition": transition.unwrap_or(Value::Null),
        "warps": warps,
        "clip": clip,
        "speed": animator.speed,
        "parameters": parameters,
        "declared": declared,
        "root_motion": root_motion,
        "ragdoll": ragdoll,
    })
}

/// The graph's locomotion warp nodes (`StrideWarp`, `OrientationWarp`; ADR-8540) from the debug
/// snapshot: one object per node, depth first, with its path and what it did on the latest
/// frame.
fn warps_json(debug: &regia_animation::debug::GraphDebug) -> Vec<Value> {
    use regia_animation::debug::NodeDebugState;
    debug
        .nodes
        .iter()
        .filter_map(|node| match &node.state {
            NodeDebugState::StrideWarp {
                mode,
                ratio,
                clip_speed,
                ground_speed,
                pelvis_drop,
            } => Some(json!({
                "path": node.path,
                "kind": "stride_warp",
                "active": node.active,
                "mode": match mode {
                    regia_animation::motion_matching::warp::StrideMode::Rate => "rate",
                    regia_animation::motion_matching::warp::StrideMode::Pose => "pose",
                },
                "ratio": ratio,
                "clip_speed": clip_speed,
                "ground_speed": ground_speed,
                "pelvis_drop": pelvis_drop,
            })),
            NodeDebugState::OrientationWarp { delta, clip_heading } => Some(json!({
                "path": node.path,
                "kind": "orientation_warp",
                "active": node.active,
                "delta": delta,
                "clip_heading": clip_heading,
            })),
            _ => None,
        })
        .collect()
}

/// `anim.list`: every entity with an `Animator` and its state summary.
pub fn list(world: &mut World) -> Result<Value, RpcError> {
    let mut query = world.query::<(Entity, &Animator)>();
    let world: &World = world;
    let mut rows: Vec<(Entity, Value)> = query
        .iter(world)
        .map(|(entity, animator)| (entity, state_json(world, entity, animator)))
        .collect();
    rows.sort_by_key(|(e, _)| e.to_bits());
    let animators: Vec<Value> = rows.into_iter().map(|(_, v)| v).collect();
    Ok(json!({ "count": animators.len(), "animators": animators }))
}

/// `anim.get_state`: one animator's state machine state, parameters (current values and the
/// graph's declarations), root motion and ragdoll blend.
pub fn get_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    let animator = animator_of(world, entity)?;
    Ok(state_json(world, entity, animator))
}

fn number(value: &Value, name: &str) -> Result<f32, RpcError> {
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .map(|v| v as f32)
        .ok_or_else(|| RpcError::invalid_params(format!("`value` for {name:?} must be a finite number")))
}

fn components(value: &Value, n: usize, name: &str) -> Result<Vec<f32>, RpcError> {
    let invalid =
        || RpcError::invalid_params(format!("`value` for {name:?} must be an array of {n} numbers"));
    let items = value.as_array().filter(|a| a.len() == n).ok_or_else(invalid)?;
    items
        .iter()
        .map(|v| {
            v.as_f64()
                .filter(|f| f.is_finite())
                .map(|f| f as f32)
                .ok_or_else(invalid)
        })
        .collect()
}

/// `anim.set_param`: set one graph parameter. The type comes from `type`, else from the
/// graph's declaration, else from the JSON value (number → float, boolean → bool, array of
/// 2/3 → vec2/vec3). `type: "trigger"` fires a trigger (the value is ignored).
pub fn set_param(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    let name = crate::params::opt_str(params, "name")?
        .filter(|n| !n.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`name` (the parameter name) is required"))?
        .to_string();
    let animator = animator_of(world, entity)?;
    let declared = animator
        .graph_def()
        .and_then(|g| g.parameters.iter().find(|d| d.0 == name))
        .map(|d| value_json(d.1).0);
    let explicit = crate::params::opt_str(params, "type")?;
    let value = params.get("value").cloned().unwrap_or(Value::Null);
    let kind = match (explicit, declared) {
        (Some(kind), _) => kind,
        (None, Some(kind)) => kind,
        (None, None) => match &value {
            Value::Number(_) => "float",
            Value::Bool(_) => "bool",
            Value::Array(a) if a.len() == 2 => "vec2",
            Value::Array(a) if a.len() == 3 => "vec3",
            _ => {
                return Err(RpcError::invalid_params(format!(
                    "cannot infer the type of {name:?}: pass `type` (float|int|bool|vec2|vec3|trigger)"
                )));
            }
        },
    };
    if let (Some(explicit), Some(declared)) = (explicit, declared)
        && explicit != declared
    {
        return Err(RpcError::invalid_params(format!(
            "{name:?} is declared {declared} by the graph, not {explicit}"
        )));
    }
    let new_value =
        match kind {
            "float" => ParamValue::Float(number(&value, &name)?),
            "int" => {
                let v = value
                    .as_i64()
                    .and_then(|v| i32::try_from(v).ok())
                    .ok_or_else(|| {
                        RpcError::invalid_params(format!("`value` for {name:?} must be an integer"))
                    })?;
                ParamValue::Int(v)
            }
            "bool" => ParamValue::Bool(value.as_bool().ok_or_else(|| {
                RpcError::invalid_params(format!("`value` for {name:?} must be a boolean"))
            })?),
            "vec2" => {
                let c = components(&value, 2, &name)?;
                ParamValue::Vec2(Vec2::new(c[0], c[1]))
            }
            "vec3" => {
                let c = components(&value, 3, &name)?;
                ParamValue::Vec3(Vec3::new(c[0], c[1], c[2]))
            }
            "trigger" => ParamValue::Trigger,
            other => {
                return Err(RpcError::invalid_params(format!(
                    "unknown parameter type {other:?} (float|int|bool|vec2|vec3|trigger)"
                )));
            }
        };
    let before = animator
        .parameters
        .get(&name)
        .map(|v| value_json(v).1)
        .unwrap_or(Value::Null);
    let Some(mut animator) = world.get_mut::<Animator>(entity) else {
        return Err(RpcError::internal(
            "the animator vanished while setting a parameter",
        ));
    };
    animator.set(&name, new_value);
    let after = animator
        .parameters
        .get(&name)
        .map(|v| value_json(v).1)
        .unwrap_or(Value::Null);
    Ok(json!({
        "entity": entity.to_bits(),
        "name": name,
        "type": kind,
        "before": before,
        "after": after,
    }))
}

fn ragdoll_of(world: &mut World, entity: Entity) -> Result<Mut<'_, Ragdoll>, RpcError> {
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "list animated rigs with anim.list",
        ));
    }
    world.get_mut::<Ragdoll>(entity).ok_or_else(|| {
        RpcError::not_found(
            format!("entity {} has no Ragdoll", entity.to_bits()),
            "add one with regia_framework::ragdoll::Ragdoll (RagdollPlugin); anim.ragdoll_set tunes it",
        )
    })
}

/// `anim.ragdoll_set`: switch a ragdoll's mode (`kinematic`, `powered`, `limp`, `active`) and tune it
/// (`blend`, `blend_time`, `strength`, `tracking_time`, `recover_snap`). Every field is
/// validated before any is applied; a limp → kinematic/powered switch is the get-up
/// (bodies re-pin to the animation, the rig snaps to where the body lies, the fallen pose
/// blends out over `blend_time`).
pub fn ragdoll_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    let mode = match params.get("mode") {
        None => None,
        Some(Value::String(mode)) => Some(match mode.as_str() {
            "kinematic" => RagdollMode::Kinematic,
            "powered" => RagdollMode::Powered,
            "limp" => RagdollMode::Limp,
            "active" => RagdollMode::Active,
            other => {
                return Err(RpcError::invalid_params(format!(
                    "unknown mode {other:?} (kinematic|powered|limp|active)"
                )));
            }
        }),
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`mode` is \"kinematic\", \"powered\", \"limp\" or \"active\"",
            ));
        }
    };
    let blend = params.get("blend").map(|v| finite(v, "blend")).transpose()?;
    let blend_time = params
        .get("blend_time")
        .map(|v| finite(v, "blend_time"))
        .transpose()?;
    let strength = params
        .get("strength")
        .map(|v| finite(v, "strength"))
        .transpose()?;
    let tracking_time = params
        .get("tracking_time")
        .map(|v| finite(v, "tracking_time"))
        .transpose()?;
    let recover_snap = match params.get("recover_snap") {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`recover_snap` must be a boolean"))?,
        ),
    };
    if mode.is_none()
        && blend.is_none()
        && blend_time.is_none()
        && strength.is_none()
        && tracking_time.is_none()
        && recover_snap.is_none()
    {
        return Err(RpcError::invalid_params(
            "nothing to set: pass `mode`, `blend`, `blend_time`, `strength`, `tracking_time` or `recover_snap`",
        ));
    }
    if let Some(b) = blend
        && !(0.0..=1.0).contains(&b)
    {
        return Err(RpcError::invalid_params("`blend` must be within 0..=1"));
    }
    if let Some(t) = blend_time
        && t < 0.0
    {
        return Err(RpcError::invalid_params("`blend_time` must be >= 0"));
    }
    if let Some(s) = strength
        && !(0.0..=1.0).contains(&s)
    {
        return Err(RpcError::invalid_params("`strength` must be within 0..=1"));
    }
    if let Some(t) = tracking_time
        && t <= 0.0
    {
        return Err(RpcError::invalid_params("`tracking_time` must be greater than 0"));
    }
    // Every field is valid: only now touch the ragdoll, so a rejected call changes nothing.
    let mut ragdoll = ragdoll_of(world, entity)?;
    if let Some(mode) = mode {
        ragdoll.mode = mode;
    }
    if let Some(b) = blend {
        ragdoll.blend = b;
    }
    if let Some(t) = blend_time {
        ragdoll.blend_time = t;
    }
    if let Some(s) = strength {
        ragdoll.strength = s;
    }
    if let Some(t) = tracking_time {
        ragdoll.tracking_time = t;
    }
    if let Some(s) = recover_snap {
        ragdoll.recover_snap = s;
    }
    let mut out = ragdoll_json(world, entity);
    out["entity"] = json!(entity.to_bits());
    Ok(out)
}

/// `anim.ragdoll_hit`: apply an impulse to the bone nearest `joint` with a decaying partial
/// physics weight on that bone and its children (the hit reaction). Defaults: `reaction` 1,
/// `recover` 1 s.
pub fn ragdoll_hit(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    let joint = crate::params::opt_str(params, "joint")?
        .filter(|j| !j.is_empty())
        .ok_or_else(|| RpcError::invalid_params("`joint` (a skeleton joint name) is required"))?
        .to_string();
    let impulse = params
        .get("impulse")
        .map(|v| components(v, 3, "impulse"))
        .transpose()?
        .ok_or_else(|| RpcError::invalid_params("`impulse` is [x, y, z] in kg·m/s"))?;
    let reaction = match params.get("reaction") {
        None => 1.0,
        Some(v) => {
            let r = finite(v, "reaction")?;
            if !(0.0..=1.0).contains(&r) {
                return Err(RpcError::invalid_params("`reaction` must be within 0..=1"));
            }
            r
        }
    };
    let recover = match params.get("recover") {
        None => 1.0,
        Some(v) => {
            let r = finite(v, "recover")?;
            if r <= 0.0 {
                return Err(RpcError::invalid_params("`recover` must be greater than 0"));
            }
            r
        }
    };
    // A rig the physics has not built bodies for yet still takes the message (it lands on
    // the next build); a joint the skeleton does not know can never land.
    ragdoll_of(world, entity)?;
    if let Some(binding) = world.get::<SkinnedMesh>(entity)
        && binding.skeleton.joint_index(&joint).is_none()
    {
        return Err(RpcError::invalid_params(format!(
            "joint {joint:?} is not in the rig's skeleton"
        )));
    }
    world.write_message(RagdollHit {
        rig: entity,
        joint,
        impulse: Vec3::new(impulse[0], impulse[1], impulse[2]),
        reaction,
        recover,
    });
    let mut out = ragdoll_json(world, entity);
    out["entity"] = json!(entity.to_bits());
    Ok(out)
}

fn spring_json(entity: Entity, springs: &SpringBones) -> Value {
    let setup = springs.setup();
    let chains: Vec<Value> = setup
        .chains
        .iter()
        .map(|c| {
            let positions: Vec<Value> = springs
                .chain_positions(&c.name)
                .into_iter()
                .map(vec3_json)
                .collect();
            json!({
                "name": c.name,
                "joints": c.joints,
                "enabled": c.enabled,
                "bound": springs.chain_bound(&c.name),
                "stiffness": c.stiffness,
                "damping": c.damping,
                "gravity_dir": vec3_json(c.gravity_dir),
                "gravity_power": c.gravity_power,
                "hit_radius": c.hit_radius,
                "colliders": c.colliders,
                "positions": positions,
            })
        })
        .collect();
    json!({
        "entity": entity.to_bits(),
        "enabled": springs.enabled,
        "frozen": springs.frozen(),
        "reduced": springs.reduced(),
        "step_hz": setup.step_hz,
        "steps_last_frame": springs.steps_last_frame(),
        "problems": springs.problems(),
        "colliders": setup.colliders.iter().map(|c| json!({ "name": c.name, "joint": c.joint })).collect::<Vec<_>>(),
        "chains": chains,
    })
}

fn springs_of(world: &mut World, entity: Entity) -> Result<Mut<'_, SpringBones>, RpcError> {
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "list spring rigs with anim.spring_list",
        ));
    }
    world.get_mut::<SpringBones>(entity).ok_or_else(|| {
        RpcError::not_found(
            format!("entity {} has no SpringBones", entity.to_bits()),
            "list spring rigs with anim.spring_list",
        )
    })
}

fn gaze_json(entity: Entity, gaze: &Gaze) -> Value {
    let result = gaze.result();
    let target = match gaze.target {
        GazeTarget::None => Value::Null,
        GazeTarget::Point(p) => json!({ "point": vec3_json(p) }),
        GazeTarget::Entity(e) => json!({ "entity": e.to_bits() }),
    };
    let eye = |k: usize| {
        let c = result.channels[k];
        json!({
            "yaw": result.angles[k].0,
            "pitch": result.angles[k].1,
            "goal_yaw": result.goal[k].0,
            "goal_pitch": result.goal[k].1,
            "saturated": result.saturated[k],
            "channels": { "up": c.up, "down": c.down, "in": c.inward, "out": c.outward },
        })
    };
    json!({
        "entity": entity.to_bits(),
        "target": target,
        "weight": gaze.weight,
        "valid": result.valid,
        "error": result.error,
        "convergence": gaze.rig.convergence,
        "saccade_speed": gaze.rig.saccade_speed,
        "min_distance": gaze.rig.min_distance,
        "left": eye(0),
        "right": eye(1),
        "lids": gaze.rig.lids.len(),
        "morphs": gaze.morphs.entries.len(),
        "head_follow": gaze.rig.head.is_some(),
    })
}

fn gaze_of(world: &World, entity: Entity) -> Result<&Gaze, RpcError> {
    animator_of(world, entity)?;
    world.get::<Gaze>(entity).ok_or_else(|| {
        RpcError::not_found(
            format!("entity {} has no Gaze", entity.to_bits()),
            "add one with anim.gaze_set (it finds the eye joints by name)",
        )
    })
}

/// `anim.spring_list`: every rig with `SpringBones`: LOD state, binding problems, every
/// chain's parameters and simulated joint positions (world space).
pub fn spring_list(world: &mut World) -> Result<Value, RpcError> {
    let mut query = world.query::<(Entity, &SpringBones)>();
    let mut rows: Vec<(u64, Value)> = query
        .iter(world)
        .map(|(entity, springs)| (entity.to_bits(), spring_json(entity, springs)))
        .collect();
    rows.sort_by_key(|(bits, _)| *bits);
    let rigs: Vec<Value> = rows.into_iter().map(|(_, v)| v).collect();
    Ok(json!({ "count": rigs.len(), "rigs": rigs }))
}

fn unit_range(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    let Some(value) = params.get(name) else {
        return Ok(None);
    };
    let v = number(value, name)?;
    if !(0.0..=1.0).contains(&v) {
        return Err(RpcError::invalid_params(format!("`{name}` must be within 0..=1")));
    }
    Ok(Some(v))
}

fn non_negative(params: &Value, name: &str) -> Result<Option<f32>, RpcError> {
    let Some(value) = params.get(name) else {
        return Ok(None);
    };
    let v = number(value, name)?;
    if v < 0.0 {
        return Err(RpcError::invalid_params(format!("`{name}` must be >= 0")));
    }
    Ok(Some(v))
}

/// `anim.spring_set`: tune one chain (`stiffness`, `damping`, `gravity_power`, `gravity_dir`,
/// `hit_radius`, `enabled`), or, with no `chain`, switch the whole component with `enabled`.
/// Every field is validated before any is applied; the simulation state is kept.
pub fn spring_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    let stiffness = unit_range(params, "stiffness")?;
    let damping = unit_range(params, "damping")?;
    let gravity_power = params
        .get("gravity_power")
        .map(|v| number(v, "gravity_power"))
        .transpose()?;
    let hit_radius = non_negative(params, "hit_radius")?;
    let gravity_dir = params
        .get("gravity_dir")
        .map(|v| components(v, 3, "gravity_dir"))
        .transpose()?
        .map(|c| Vec3::new(c[0], c[1], c[2]));
    let enabled = match params.get("enabled") {
        None => None,
        Some(v) => Some(
            v.as_bool()
                .ok_or_else(|| RpcError::invalid_params("`enabled` must be a boolean"))?,
        ),
    };
    let chain_name = crate::params::opt_str(params, "chain")?;
    let mut springs = springs_of(world, entity)?;
    match chain_name {
        None => {
            if stiffness.is_some()
                || damping.is_some()
                || gravity_power.is_some()
                || hit_radius.is_some()
                || gravity_dir.is_some()
            {
                return Err(RpcError::invalid_params(
                    "name the `chain` to tune (omit it only to switch the component with `enabled`)",
                ));
            }
            let Some(enabled) = enabled else {
                return Err(RpcError::invalid_params(
                    "nothing to set: pass `chain` and fields, or `enabled`",
                ));
            };
            springs.enabled = enabled;
        }
        Some(name) => {
            let Some(index) = springs.setup().chains.iter().position(|c| c.name == name) else {
                return Err(RpcError::not_found(
                    format!("no spring chain named {name:?}"),
                    "chain names are in anim.spring_list",
                ));
            };
            let chain = &mut springs.setup_mut().chains[index];
            if let Some(v) = stiffness {
                chain.stiffness = v;
            }
            if let Some(v) = damping {
                chain.damping = v;
            }
            if let Some(v) = gravity_power {
                chain.gravity_power = v;
            }
            if let Some(v) = hit_radius {
                chain.hit_radius = v;
            }
            if let Some(v) = gravity_dir {
                chain.gravity_dir = v;
            }
            if let Some(v) = enabled {
                chain.enabled = v;
            }
        }
    }
    Ok(spring_json(entity, &springs))
}

/// `anim.spring_reset`: forget the simulation of a rig; its chains re-seed from the animated
/// pose next frame (after a teleport or a cut).
pub fn spring_reset(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    let mut springs = springs_of(world, entity)?;
    springs.reset();
    Ok(json!({ "entity": entity.to_bits(), "reset": true }))
}

/// `anim.gaze_get`: the eye angles, limits state, channels and target of a rig's [`Gaze`].
pub fn gaze_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    let gaze = gaze_of(world, entity)?;
    Ok(gaze_json(entity, gaze))
}

fn finite(value: &Value, name: &str) -> Result<f32, RpcError> {
    value
        .as_f64()
        .filter(|v| v.is_finite())
        .map(|v| v as f32)
        .ok_or_else(|| RpcError::invalid_params(format!("`{name}` must be a finite number")))
}

/// `anim.gaze_set`: set a rig's gaze target and tuning. Creates the [`Gaze`] (eye and lid joints
/// found by name) when the rig has none. `target` is `[x, y, z]` (a world point), an entity id,
/// or `null` (eyes return to rest).
pub fn gaze_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    animator_of(world, entity)?;
    let target = match params.get("target") {
        None => None,
        Some(Value::Null) => Some(GazeTarget::None),
        Some(Value::Number(n)) => {
            let bits = n
                .as_u64()
                .and_then(Entity::try_from_bits)
                .ok_or_else(|| RpcError::invalid_params("`target` entity id is not valid"))?;
            if world.get_entity(bits).is_err() {
                return Err(RpcError::not_found(
                    format!("target entity {} does not exist", bits.to_bits()),
                    "use scene.query_entities",
                ));
            }
            Some(GazeTarget::Entity(bits))
        }
        Some(Value::Array(_)) => {
            let c = components(&params["target"], 3, "target")?;
            Some(GazeTarget::Point(Vec3::new(c[0], c[1], c[2])))
        }
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`target` is [x, y, z], an entity id, or null",
            ));
        }
    };
    let weight = params.get("weight").map(|v| finite(v, "weight")).transpose()?;
    let convergence = params
        .get("convergence")
        .map(|v| finite(v, "convergence"))
        .transpose()?;
    let speed = params
        .get("saccade_speed")
        .map(|v| finite(v, "saccade_speed"))
        .transpose()?;
    // Every parameter is valid: only now create the gaze, so a rejected call leaves no trace.
    if world.get::<Gaze>(entity).is_none() {
        let binding = world.get::<SkinnedMesh>(entity).ok_or_else(|| {
            RpcError::not_found(
                format!("entity {} has no SkinnedMesh", entity.to_bits()),
                "gaze needs a skinned rig",
            )
        })?;
        let rig = GazeRig::from_skeleton(&binding.skeleton).ok_or_else(|| {
            RpcError::invalid_params("no left/right eye joints found by name (eye_l, LeftEye, ...)")
        })?;
        world.entity_mut(entity).insert(Gaze::new(rig));
    }
    let Some(mut gaze) = world.get_mut::<Gaze>(entity) else {
        return Err(RpcError::internal("gaze vanished while setting it"));
    };
    if let Some(target) = target {
        gaze.target = target;
    }
    if let Some(w) = weight {
        gaze.weight = w.clamp(0.0, 1.0);
    }
    if let Some(c) = convergence {
        gaze.rig.convergence = c.clamp(0.0, 1.0);
    }
    if let Some(s) = speed {
        gaze.rig.saccade_speed = s;
    }
    Ok(gaze_json(entity, &gaze))
}

fn bind_pose_json(world: &World, entity: Entity) -> Result<Value, RpcError> {
    let binding = world.get::<SkinnedMesh>(entity).ok_or_else(|| {
        RpcError::not_found(
            format!("entity {} has no SkinnedMesh", entity.to_bits()),
            "list animated rigs with anim.list",
        )
    })?;
    Ok(match world.get::<InstanceBindPose>(entity) {
        None => json!({
            "entity": entity.to_bits(),
            "overridden": false,
            "joints": binding.skeleton.len(),
        }),
        Some(instance) => {
            let (lo, hi) = instance.ratio_range();
            let changed: Vec<Value> = instance
                .length_ratio
                .iter()
                .enumerate()
                .filter(|(_, r)| (**r - 1.0).abs() > 1e-4)
                .map(|(i, r)| {
                    json!({
                        "joint": binding.skeleton.joint_names.get(i).cloned().unwrap_or_default(),
                        "ratio": r,
                    })
                })
                .collect();
            json!({
                "entity": entity.to_bits(),
                "overridden": true,
                "joints": instance.len(),
                "matches_skeleton": instance.len() == binding.skeleton.len(),
                "ratio_min": lo,
                "ratio_max": hi,
                "changed": changed,
            })
        }
    })
}

/// `anim.bind_pose_get`: whether a rig has its own bind pose and how it differs from the shared
/// skeleton (per-joint bone length ratios).
pub fn bind_pose_get(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    animator_of(world, entity)?;
    bind_pose_json(world, entity)
}

/// `anim.bind_pose_set`: give a rig its own proportions (`scale` for every bone, `bone_scales`
/// per joint name) or remove them (`clear`). The shared skeleton asset is not touched.
pub fn bind_pose_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    animator_of(world, entity)?;
    let skeleton = world
        .get::<SkinnedMesh>(entity)
        .map(|b| b.skeleton.clone())
        .ok_or_else(|| {
            RpcError::not_found(
                format!("entity {} has no SkinnedMesh", entity.to_bits()),
                "list animated rigs with anim.list",
            )
        })?;
    if crate::params::opt_bool(params, "clear")? == Some(true) {
        world.entity_mut(entity).remove::<InstanceBindPose>();
        return bind_pose_json(world, entity);
    }
    let positive = |v: &Value, name: &str| {
        finite(v, name).and_then(|f| {
            if f > 0.0 {
                Ok(f)
            } else {
                Err(RpcError::invalid_params(format!(
                    "`{name}` must be greater than 0 (0 collapses the bones, a negative factor mirrors them)"
                )))
            }
        })
    };
    let uniform = params.get("scale").map(|v| positive(v, "scale")).transpose()?;
    let bones = match params.get("bone_scales") {
        None | Some(Value::Null) => HashMap::new(),
        Some(Value::Object(map)) => map
            .iter()
            .map(|(k, v)| positive(v, "bone_scales").map(|f| (k.clone(), f)))
            .collect::<Result<HashMap<_, _>, _>>()?,
        Some(_) => {
            return Err(RpcError::invalid_params(
                "`bone_scales` is { joint name: factor }",
            ));
        }
    };
    if uniform.is_none() && bones.is_empty() {
        return Err(RpcError::invalid_params(
            "pass `scale`, `bone_scales` or `clear: true`",
        ));
    }
    let factor = uniform.unwrap_or(1.0);
    let mut combined = HashMap::new();
    for name in &skeleton.joint_names {
        combined.insert(name.clone(), factor * bones.get(name).copied().unwrap_or(1.0));
    }
    if let Some(unknown) = bones.keys().find(|k| !combined.contains_key(*k)) {
        return Err(RpcError::invalid_params(format!(
            "joint {unknown:?} is not in the skeleton"
        )));
    }
    let instance = InstanceBindPose::from_bone_scales(&skeleton, &combined)
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    world.entity_mut(entity).insert(instance);
    bind_pose_json(world, entity)
}

/// The `contact.<foot>` curves of a clip for `anim.clip_report` (ADR-8308): per curve the foot
/// joint, the key count, the share of the clip the foot is planted (curve at or above 0.5, sampled
/// at the clip's frames) and the number of touchdowns (rising edges).
fn contact_curves_report(clip: &regia_animation::AnimationClip) -> Vec<Value> {
    use regia_animation::foot_ik::contact::{contact_curves, contact_frame_times};
    let times = contact_frame_times(clip);
    contact_curves(clip)
        .map(|(joint, curve)| {
            let planted: Vec<bool> = times.iter().map(|&t| curve.sample(t) >= 0.5).collect();
            let share = if planted.is_empty() {
                0.0
            } else {
                planted.iter().filter(|&&p| p).count() as f64 / planted.len() as f64
            };
            let touchdowns = planted.windows(2).filter(|w| !w[0] && w[1]).count();
            json!({
                "name": regia_animation::foot_ik::contact::contact_curve_name(joint),
                "joint": joint,
                "keys": curve.key_count(),
                "planted_share": share,
                "touchdowns": touchdowns,
            })
        })
        .collect()
}

/// `anim.clip_report`: health and compression report for the clips one animator plays
/// (ADR-871): key-data problems with stable issue codes, the root motion one loop of each clip
/// adds, and the error the cook's compression leaves against the given tolerances, measured
/// on the rig's skeleton.
pub fn clip_report(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    use regia_animation::check::{check_clip, loop_seam, root_motion_per_lap};
    use regia_animation::compress::{
        CompressionSettings, compress_clip, measure_compression_error, raw_joint_bytes,
    };

    let entity = crate::scene::entity_from(params)?;
    let mut settings = CompressionSettings::default();
    for (key, slot) in [
        ("position_tolerance", &mut settings.position_tolerance),
        ("rotation_tolerance", &mut settings.rotation_tolerance),
        ("scale_tolerance", &mut settings.scale_tolerance),
        ("propagated_tolerance", &mut settings.propagated_tolerance),
    ] {
        if let Some(value) = params.get(key) {
            *slot = value
                .as_f64()
                .filter(|v| {
                    v.is_finite()
                        && *v >= crate::contract::CLIP_REPORT_TOLERANCE_MIN
                        && *v <= crate::contract::CLIP_REPORT_TOLERANCE_MAX
                })
                .ok_or_else(|| {
                    RpcError::invalid_params(format!(
                        "`{key}` must be a positive number in {}..={}",
                        crate::contract::CLIP_REPORT_TOLERANCE_MIN,
                        crate::contract::CLIP_REPORT_TOLERANCE_MAX
                    ))
                })? as f32;
        }
    }
    let measure = crate::params::opt_bool(params, "compression")?.unwrap_or(true);
    let only = crate::params::opt_str(params, "clip")?;
    let animator = animator_of(world, entity)?;
    let skeleton = world
        .get::<SkinnedMesh>(entity)
        .map(|mesh| std::sync::Arc::clone(&mesh.skeleton));
    let mut clips: Vec<std::sync::Arc<regia_animation::AnimationClip>> = match &animator.source {
        AnimatorSource::Graph { clips, .. } => clips.names().filter_map(|n| clips.get(n).cloned()).collect(),
        AnimatorSource::SingleClip { clip, .. } => vec![std::sync::Arc::clone(clip)],
    };
    clips.sort_by(|a, b| a.name.cmp(&b.name));
    if let Some(name) = only {
        clips.retain(|c| c.name == name);
        if clips.is_empty() {
            return Err(RpcError::not_found(
                format!("clip {name:?} is not played by entity {}", entity.to_bits()),
                "omit `clip` to report every clip of the animator",
            ));
        }
    }
    let skeleton = skeleton.as_deref();
    let mut issue_count = 0usize;
    let reports: Vec<Value> = clips
        .iter()
        .map(|clip| {
            let issues = check_clip(clip, skeleton);
            issue_count += issues.len();
            let root_motion = clip.root_motion.as_ref().map_or(Value::Null, |rm| {
                let lap = root_motion_per_lap(clip);
                json!({
                    "mode": format!("{:?}", rm.mode),
                    "joint": rm.joint,
                    "per_lap": lap.map_or(Value::Null, |(t, _)| vec3_json(t)),
                    "yaw_per_lap": lap.map_or(Value::Null, |(_, yaw)| json!(yaw)),
                })
            });
            let compression = if measure {
                let compressed = compress_clip(clip, skeleton, &settings);
                let error = measure_compression_error(clip, &compressed, skeleton);
                let raw = raw_joint_bytes(clip);
                let packed = compressed.compressed_size();
                json!({
                    "max_position": error.max_position,
                    "max_rotation": error.max_rotation,
                    "max_scale": error.max_scale,
                    "max_object_position": error.max_object_position,
                    "worst_joint": error.worst_joint,
                    "worst_joint_name": error.worst_joint.and_then(|j| {
                        skeleton.and_then(|s| s.joint_names.get(j as usize).cloned())
                    }),
                    "worst_time": error.worst_time,
                    "samples": error.samples,
                    "within_tolerance": error.within(&settings, 5e-5),
                    "raw_bytes": raw,
                    "compressed_bytes": packed,
                    "ratio": if raw > 0 { json!(packed as f64 / raw as f64) } else { Value::Null },
                })
            } else {
                Value::Null
            };
            let seam = loop_seam(clip);
            json!({
                "name": clip.name,
                "duration": clip.duration,
                "tracks": clip.tracks.len(),
                "node_animation": node_animation_report(clip),
                "notifies": clip.notifies.len(),
                "contact_curves": contact_curves_report(clip),
                "root_motion": root_motion,
                "loop_seam": {
                    "max_rotation": seam.max_rotation,
                    "max_translation": seam.max_translation,
                    "joint": seam.joint,
                    "joint_name": seam.joint.and_then(|j| {
                        skeleton.and_then(|s| s.joint_names.get(j as usize).cloned())
                    }),
                },
                "issues": issues
                    .iter()
                    .map(|i| json!({ "code": i.code, "detail": i.detail }))
                    .collect::<Vec<_>>(),
                "compression": compression,
            })
        })
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "skeleton": skeleton.map(|s| s.len()),
        "tolerances": {
            "position": settings.position_tolerance,
            "rotation": settings.rotation_tolerance,
            "scale": settings.scale_tolerance,
            "propagated": settings.propagated_tolerance,
        },
        "issue_count": issue_count,
        "clips": reports,
    }))
}

fn node_animation_report(clip: &regia_animation::AnimationClip) -> Value {
    let Some(animation) = &clip.node_animation else {
        return Value::Null;
    };
    let validation_error = animation.validate().err();
    let mut buffer = Vec::new();
    let samples: Vec<_> = [0.0, clip.duration * 0.5, clip.duration]
        .into_iter()
        .map(|time| {
            let error = animation.sample(time, &mut buffer).err();
            json!({ "time": time, "error": error, "nodes": buffer.iter().map(|node| json!({
            "index": node.index, "is_joint": node.is_joint,
            "local": node.local.to_cols_array(), "world": node.world.to_cols_array(),
            "delta": node.delta.map(|m| m.to_cols_array()),
        })).collect::<Vec<_>>() })
        })
        .collect();
    json!({ "version": animation.version, "validation_error": validation_error,
        "nodes": animation.nodes.iter().map(|node| json!({
            "index": node.node.index, "name": node.node.name, "parent_slot": node.parent,
            "is_joint": node.is_joint, "animated": node.animated(),
            "translation_keys": node.translations.as_ref().map_or(0, |c| c.times.len()),
            "rotation_keys": node.rotations.as_ref().map_or(0, |c| c.times.len()),
            "scale_keys": node.scales.as_ref().map_or(0, |c| c.times.len()),
        })).collect::<Vec<_>>(), "samples": samples })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::graph::AnimGraph;
    use regia_animation::{AnimationClip, ClipLibrary};

    const GRAPH: &str = r#"(
        format_version: 1,
        parameters: [("Speed", Float(0.0)), ("Jump", Trigger), ("Gear", Int(1))],
        root: StateMachine((
            initial: "Idle",
            states: [
                ("Idle", Clip((clip: "idle", looping: true, speed: 1.0, sync_group: None))),
                ("Run", Clip((clip: "idle", looping: true, speed: 1.0, sync_group: None))),
            ],
            transitions: [(
                from: Some("Idle"), to: "Run",
                conditions: [Float(param: "Speed", op: Gt, value: 1.0)],
                duration: 0.0, curve: Linear, interruptible: true,
            )],
        )),
    )"#;

    fn world_with_graph() -> Result<(App, Entity), String> {
        let graph = AnimGraph::from_ron_str(GRAPH).map_err(|e| e.to_string())?;
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("idle", 1.0));
        let mut animator = Animator::graph(graph, clips);
        animator.parameters = regia_animation::ParameterSet::from_decls(
            &animator
                .graph_def()
                .map(|g| g.parameters.clone())
                .unwrap_or_default(),
        );
        let mut app = App::new();
        let entity = app.world_mut().spawn(animator).id();
        Ok((app, entity))
    }

    #[test]
    fn set_param_infers_declared_types_and_reports_before_after() {
        let (mut app, e) = world_with_graph().expect("test graph parses");
        let world = app.world_mut();
        let out = set_param(
            world,
            &json!({ "entity": e.to_bits(), "name": "Speed", "value": 3.5 }),
        )
        .expect("set Speed");
        assert_eq!(out["type"], "float");
        assert_eq!(out["before"], json!(0.0));
        assert_eq!(out["after"], json!(3.5));
        let out = set_param(
            world,
            &json!({ "entity": e.to_bits(), "name": "Gear", "value": 3 }),
        )
        .expect("int");
        assert_eq!(out["type"], "int");
        let out = set_param(world, &json!({ "entity": e.to_bits(), "name": "Jump" })).expect("trigger");
        assert_eq!(out["after"], json!(true));
        let err = set_param(
            world,
            &json!({ "entity": e.to_bits(), "name": "Speed", "type": "bool", "value": true }),
        );
        assert!(err.is_err(), "type mismatch with the declaration is rejected");
        let err = set_param(
            world,
            &json!({ "entity": e.to_bits(), "name": "Mystery", "value": "x" }),
        );
        assert!(err.is_err(), "uninferable type");

        let state = get_state(world, &json!({ "entity": e.to_bits() })).expect("state");
        assert_eq!(state["source"], "graph");
        assert_eq!(state["parameters"]["Speed"]["value"], json!(3.5));
        assert_eq!(state["parameters"]["Jump"]["type"], "trigger");
        assert_eq!(state["declared"].as_array().map(Vec::len), Some(3));
        let listed = list(world).expect("list");
        assert_eq!(listed["count"], 1);
    }

    #[test]
    fn get_state_lists_the_warp_nodes() {
        let text = r#"(
            format_version: 4,
            parameters: [("GroundSpeed", Float(1.3)), ("Heading", Float(0.0))],
            root: OrientationWarp((
                inner: StrideWarp((
                    inner: Clip((clip: "idle", looping: true, speed: 1.0, sync_group: None)),
                    ground_speed_param: "GroundSpeed",
                    settings: (strength: 1.0, min_speed: 0.1, ratio_min: 0.8, ratio_max: 1.25,
                        response: 12.0, mode: Rate),
                )),
                heading_param: "Heading",
                pelvis: "hips",
            )),
        )"#;
        let graph = AnimGraph::from_ron_str(text).expect("v4 warp graph");
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("idle", 1.0));
        let mut app = App::new();
        let entity = app.world_mut().spawn(Animator::graph(graph, clips)).id();
        let out = get_state(app.world_mut(), &json!({ "entity": entity.to_bits() })).expect("state");
        let warps = out["warps"].as_array().expect("warps list");
        assert_eq!(warps.len(), 2, "{out}");
        assert_eq!(warps[0]["kind"], "orientation_warp");
        assert_eq!(warps[0]["path"], "root");
        assert_eq!(warps[0]["delta"], json!(0.0));
        assert_eq!(warps[1]["kind"], "stride_warp");
        assert_eq!(warps[1]["path"], "root/inner");
        assert_eq!(warps[1]["mode"], "rate");
        assert_eq!(warps[1]["ratio"], json!(1.0));
    }

    #[test]
    fn missing_entities_and_animators_are_not_found() {
        let mut app = App::new();
        let plain = app.world_mut().spawn_empty().id();
        let err = get_state(app.world_mut(), &json!({ "entity": plain.to_bits() })).expect_err("no animator");
        assert!(err.message.contains("no Animator"));
        assert!(get_state(app.world_mut(), &json!({})).is_err());
    }

    #[test]
    fn ragdoll_set_and_hit_drive_the_ragdoll() {
        use regia_framework::ragdoll::{Ragdoll, RagdollProfile};
        let (mut app, rig) = world_with_face();
        let world = app.world_mut();
        world
            .entity_mut(rig)
            .insert(Ragdoll::new(RagdollProfile::default()));
        let id = rig.to_bits();

        // Set: mode and tuning, reported back with the shared ragdoll summary.
        let out = ragdoll_set(
            world,
            &json!({ "entity": id, "mode": "limp", "blend": 0.5, "strength": 0.8 }),
        )
        .expect("set");
        assert_eq!(out["entity"], json!(id));
        assert_eq!(out["mode"], "limp");
        assert_eq!(out["blend"], json!(0.5));
        assert_eq!(out["bodies"], json!(0), "no plugin ran: nothing is built");
        assert_eq!(out["recovering"], json!(false));
        let ragdoll = world.get::<Ragdoll>(rig).expect("ragdoll");
        assert_eq!(ragdoll.mode, RagdollMode::Limp);
        assert!((ragdoll.strength - 0.8).abs() < 1e-6);
        assert!(ragdoll.recover_snap);

        // A rejected call changes nothing.
        for bad in [
            json!({ "entity": id, "mode": "dead" }),
            json!({ "entity": id, "blend": 2.0 }),
            json!({ "entity": id, "strength": -0.1 }),
            json!({ "entity": id, "tracking_time": 0.0 }),
            json!({ "entity": id, "blend_time": -1.0 }),
            json!({ "entity": id, "recover_snap": "yes" }),
            json!({ "entity": id }),
        ] {
            assert!(ragdoll_set(world, &bad).is_err(), "{bad}");
        }
        assert_eq!(
            world.get::<Ragdoll>(rig).expect("ragdoll").mode,
            RagdollMode::Limp
        );

        // Hits are validated against the skeleton.
        let out = ragdoll_hit(
            world,
            &json!({ "entity": id, "joint": "root", "impulse": [0.0, 0.0, -8.0], "reaction": 0.5, "recover": 2.0 }),
        )
        .expect("hit");
        assert_eq!(out["mode"], "limp");
        assert!(
            ragdoll_hit(
                world,
                &json!({ "entity": id, "joint": "tail", "impulse": [0, 0, -8] })
            )
            .is_err(),
            "unknown joint"
        );
        assert!(
            ragdoll_hit(world, &json!({ "entity": id, "joint": "root" })).is_err(),
            "impulse required"
        );
        assert!(
            ragdoll_hit(
                world,
                &json!({ "entity": id, "joint": "root", "impulse": [0, 0] })
            )
            .is_err(),
            "impulse needs three components"
        );
        assert!(
            ragdoll_hit(
                world,
                &json!({ "entity": id, "joint": "root", "impulse": [0, 0, "fast"] })
            )
            .is_err(),
            "impulse must be numbers"
        );
        assert!(
            ragdoll_hit(
                world,
                &json!({ "entity": id, "joint": "root", "impulse": [0, 0, -8], "reaction": 2.0 })
            )
            .is_err(),
            "reaction is 0..=1"
        );

        // The state summary carries the ragdoll.
        let state = get_state(world, &json!({ "entity": id })).expect("state");
        assert_eq!(state["ragdoll"]["mode"], "limp");

        // Entities without a ragdoll are not found.
        let plain = world.spawn_empty().id();
        assert!(ragdoll_set(world, &json!({ "entity": plain.to_bits(), "mode": "limp" })).is_err());
        assert!(
            ragdoll_hit(
                world,
                &json!({ "entity": plain.to_bits(), "joint": "root", "impulse": [1, 0, 0] })
            )
            .is_err()
        );
        assert!(ragdoll_set(world, &json!({ "entity": 4294967310_u64, "mode": "limp" })).is_err());
    }

    /// Spec 93 § 4 / § 10: `anim.ragdoll_set` takes `mode: "active"`, and the ragdoll summary of
    /// `anim.ragdoll_set`, `anim.get_state` and `anim.list` carries `settled`, `face_up` and
    /// `frozen` (all false for a ragdoll that has not been built).
    #[test]
    fn ragdoll_summary_has_the_settle_fields_and_active_mode() {
        use regia_framework::ragdoll::{Ragdoll, RagdollProfile};
        let (mut app, rig) = world_with_face();
        let world = app.world_mut();
        world
            .entity_mut(rig)
            .insert(Ragdoll::new(RagdollProfile::default()));
        let id = rig.to_bits();
        let out = ragdoll_set(world, &json!({ "entity": id, "mode": "active" })).expect("set");
        assert_eq!(out["mode"], "active");
        assert_eq!(
            world.get::<Ragdoll>(rig).expect("ragdoll").mode,
            RagdollMode::Active
        );
        let state = get_state(world, &json!({ "entity": id })).expect("state");
        let listed = list(world).expect("list");
        let row = listed["animators"]
            .as_array()
            .and_then(|a| a.iter().find(|r| r["ragdoll"].is_object()))
            .cloned()
            .expect("the rig is listed with its ragdoll");
        for summary in [&out, &state["ragdoll"], &row["ragdoll"]] {
            assert_eq!(summary["mode"], "active", "{summary}");
            for field in ["settled", "face_up", "frozen"] {
                assert_eq!(summary[field], json!(false), "{field}: {summary}");
            }
        }
    }

    #[test]
    fn spring_methods_list_tune_and_reset() {
        use regia_animation::spring::{SpringBoneSetup, SpringChain};
        let mut app = App::new();
        let setup = SpringBoneSetup {
            chains: vec![SpringChain {
                name: "hair".into(),
                joints: vec!["a".into(), "b".into()],
                ..SpringChain::default()
            }],
            ..SpringBoneSetup::default()
        };
        let e = app.world_mut().spawn(SpringBones::new(setup)).id();
        let world = app.world_mut();
        let listed = spring_list(world).expect("list");
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["rigs"][0]["chains"][0]["name"], "hair");
        let out = spring_set(
            world,
            &json!({ "entity": e.to_bits(), "chain": "hair", "stiffness": 0.9, "gravity_dir": [0, -1, 0], "gravity_power": 4.0 }),
        )
        .expect("tune");
        assert_eq!(out["chains"][0]["gravity_power"], json!(4.0));
        assert!((out["chains"][0]["stiffness"].as_f64().unwrap_or(0.0) - 0.9).abs() < 1e-6);
        let bad = spring_set(
            world,
            &json!({ "entity": e.to_bits(), "chain": "hair", "stiffness": 2.0 }),
        );
        assert!(bad.is_err(), "out-of-range stiffness is rejected");
        assert!(
            spring_set(
                world,
                &json!({ "entity": e.to_bits(), "chain": "nope", "damping": 0.1 })
            )
            .is_err()
        );
        assert!(spring_set(world, &json!({ "entity": e.to_bits(), "damping": 0.1 })).is_err());
        let off = spring_set(world, &json!({ "entity": e.to_bits(), "enabled": false })).expect("switch");
        assert_eq!(off["enabled"], json!(false));
        assert_eq!(
            spring_reset(world, &json!({ "entity": e.to_bits() })).expect("reset")["reset"],
            json!(true)
        );
        let plain = world.spawn_empty().id();
        assert!(spring_reset(world, &json!({ "entity": plain.to_bits() })).is_err());
    }

    fn world_with_face() -> (App, Entity) {
        use regia_animation::pose::translated;
        let skeleton = std::sync::Arc::new(
            regia_animation::Skeleton::from_local_bind(
                vec!["root".into(), "eye_l".into(), "eye_r".into()],
                vec![None, Some(0), Some(0)],
                vec![
                    translated(Vec3::new(0.0, 1.6, 0.0)),
                    translated(Vec3::new(-0.03, 0.05, -0.08)),
                    translated(Vec3::new(0.03, 0.05, -0.08)),
                ],
            )
            .expect("skeleton"),
        );
        let mut app = App::new();
        let binding = regia_animation::components::spawn_skeleton(app.world_mut(), skeleton);
        let animator = Animator::single(regia_animation::AnimationClip::empty("still", 1.0), true);
        let rig = app.world_mut().spawn((animator, binding)).id();
        (app, rig)
    }

    #[test]
    fn gaze_set_creates_the_gaze_and_get_reports_it() {
        let (mut app, rig) = world_with_face();
        let world = app.world_mut();
        let id = rig.to_bits();
        assert!(gaze_get(world, &json!({ "entity": id })).is_err(), "no gaze yet");
        let out = gaze_set(
            world,
            &json!({ "entity": id, "target": [1.0, 1.7, -2.0], "weight": 0.5, "convergence": 0.25 }),
        )
        .expect("set");
        // The point is stored as f32: 1.7 comes back as the f64 of 1.7f32.
        assert_eq!(out["target"]["point"], json!([1.0, f64::from(1.7f32), -2.0]));
        assert_eq!(out["weight"], json!(0.5));
        assert_eq!(out["convergence"], json!(0.25));
        let other = world.spawn_empty().id();
        let out = gaze_set(world, &json!({ "entity": id, "target": other.to_bits() })).expect("entity");
        assert_eq!(out["target"]["entity"], json!(other.to_bits()));
        let out = gaze_set(world, &json!({ "entity": id, "target": null })).expect("rest");
        assert_eq!(out["target"], Value::Null);
        assert!(gaze_set(world, &json!({ "entity": id, "target": "me" })).is_err());
        assert!(gaze_set(world, &json!({ "entity": id, "weight": "x" })).is_err());
        let got = gaze_get(world, &json!({ "entity": id })).expect("get");
        assert_eq!(got["left"]["channels"]["up"], json!(0.0));
        assert_eq!(got["valid"], json!(false), "not solved before the first frame");
    }

    #[test]
    fn gaze_set_needs_eyes_by_name() {
        let mut app = App::new();
        let skeleton = std::sync::Arc::new(
            regia_animation::Skeleton::from_local_bind(
                vec!["root".into()],
                vec![None],
                vec![Default::default()],
            )
            .expect("skeleton"),
        );
        let binding = regia_animation::components::spawn_skeleton(app.world_mut(), skeleton);
        let animator = Animator::single(regia_animation::AnimationClip::empty("still", 1.0), true);
        let rig = app.world_mut().spawn((animator, binding)).id();
        let err = gaze_set(app.world_mut(), &json!({ "entity": rig.to_bits() })).expect_err("no eyes");
        assert!(err.message.contains("eye"));
    }

    #[test]
    fn bind_pose_set_scales_bones_and_clears() {
        let (mut app, rig) = world_with_face();
        let world = app.world_mut();
        let id = rig.to_bits();
        let out = bind_pose_get(world, &json!({ "entity": id })).expect("get");
        assert_eq!(out["overridden"], json!(false));
        let out = bind_pose_set(
            world,
            &json!({ "entity": id, "scale": 1.2, "bone_scales": { "eye_l": 0.5 } }),
        )
        .expect("set");
        assert_eq!(out["overridden"], json!(true));
        assert!((out["ratio_max"].as_f64().unwrap_or(0.0) - 1.2).abs() < 1e-5);
        assert!((out["ratio_min"].as_f64().unwrap_or(0.0) - 0.6).abs() < 1e-5);
        assert_eq!(out["changed"].as_array().map(Vec::len), Some(3));
        assert!(bind_pose_set(world, &json!({ "entity": id, "bone_scales": { "tail": 2.0 } })).is_err());
        assert!(bind_pose_set(world, &json!({ "entity": id })).is_err());
        let out = bind_pose_set(world, &json!({ "entity": id, "clear": true })).expect("clear");
        assert_eq!(out["overridden"], json!(false));
    }

    /// Review w11 #9: a rejected `anim.gaze_set` leaves no `Gaze` behind, and `anim.bind_pose_set`
    /// refuses factors that collapse (0) or mirror (negative) the bones, even when the uniform
    /// and per-bone factors would multiply to a positive one.
    #[test]
    fn rejected_gaze_and_bind_pose_calls_change_nothing() {
        let (mut app, rig) = world_with_face();
        let world = app.world_mut();
        let id = rig.to_bits();
        for bad in [
            json!({ "entity": id, "weight": "x" }),
            json!({ "entity": id, "convergence": null }),
            json!({ "entity": id, "saccade_speed": "fast" }),
            json!({ "entity": id, "target": "me" }),
        ] {
            assert!(gaze_set(world, &bad).is_err(), "{bad}");
            assert!(world.get::<Gaze>(rig).is_none(), "{bad} left a Gaze behind");
        }
        for bad in [
            json!({ "entity": id, "scale": 0.0 }),
            json!({ "entity": id, "scale": -1.0 }),
            json!({ "entity": id, "bone_scales": { "eye_l": 0.0 } }),
            json!({ "entity": id, "scale": -1.0, "bone_scales": { "eye_l": -1.0 } }),
        ] {
            assert!(bind_pose_set(world, &bad).is_err(), "{bad}");
            assert!(world.get::<InstanceBindPose>(rig).is_none(), "{bad}");
        }
    }

    #[test]
    fn clip_report_lists_issues_root_motion_and_compression_error() {
        use regia_animation::clip::{JointTrack, Notify, RootMotionMode, RootMotionTrack, TrackCurve};
        let mut walk = AnimationClip::empty("walk", 1.0);
        let times: Vec<f32> = (0..=30).map(|k| k as f32 / 30.0).collect();
        walk.tracks.push(JointTrack {
            joint: 0,
            translations: TrackCurve::Animated {
                values: times
                    .iter()
                    .map(|t| Vec3::new(0.0, 0.05 * (t * 6.0).sin(), -1.2 * t))
                    .collect(),
                times,
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(regia_core::math::Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        walk.root_motion = Some(RootMotionTrack {
            joint: 0,
            mode: RootMotionMode::XZ,
        });
        let mut broken = AnimationClip::empty("broken", 1.0);
        broken.notifies.push(Notify {
            time: 5.0,
            name: "never".into(),
            payload: None,
        });
        let mut clips = ClipLibrary::new();
        clips.insert(walk);
        clips.insert(broken);
        let graph = AnimGraph::from_ron_str(GRAPH).expect("test graph parses");
        let mut app = App::new();
        let skeleton = std::sync::Arc::new(
            regia_animation::Skeleton::from_local_bind(
                vec!["root".into()],
                vec![None],
                vec![regia_animation::pose::translated(Vec3::ZERO)],
            )
            .expect("valid skeleton"),
        );
        let e = app
            .world_mut()
            .spawn((
                Animator::graph(graph, clips),
                SkinnedMesh {
                    skeleton,
                    joints: Vec::new(),
                },
            ))
            .id();
        let world = app.world_mut();
        let out = clip_report(world, &json!({ "entity": e.to_bits() })).expect("report");
        assert_eq!(out["skeleton"], json!(1));
        assert_eq!(out["issue_count"], json!(1));
        let clips = out["clips"].as_array().expect("clips");
        assert_eq!(clips.len(), 2);
        assert_eq!(clips[0]["name"], "broken", "sorted by name");
        assert_eq!(clips[0]["issues"][0]["code"], "notify_outside_clip");
        let walk_report = &clips[1];
        assert_eq!(walk_report["root_motion"]["mode"], "XZ");
        // The extracted travel is not a seam; the small vertical bob ends near where it began.
        assert!(
            walk_report["loop_seam"]["max_translation"]
                .as_f64()
                .is_some_and(|m| m < 0.05)
        );
        let lap_z = walk_report["root_motion"]["per_lap"][2].as_f64().unwrap_or(0.0);
        assert!((lap_z + 1.2).abs() < 1e-4, "{lap_z}");
        assert_eq!(walk_report["compression"]["within_tolerance"], json!(true));
        assert!(
            walk_report["compression"]["max_position"]
                .as_f64()
                .is_some_and(|m| m <= 0.005 + 1e-4)
        );
        // One clip, tighter tolerance, no compression pass.
        let out = clip_report(
            world,
            &json!({ "entity": e.to_bits(), "clip": "walk", "compression": false }),
        )
        .expect("one clip");
        assert_eq!(out["clips"].as_array().map(Vec::len), Some(1));
        assert!(out["clips"][0]["compression"].is_null());
        let missing = clip_report(world, &json!({ "entity": e.to_bits(), "clip": "nope" }));
        assert!(missing.is_err());
        let bad = clip_report(
            world,
            &json!({ "entity": e.to_bits(), "position_tolerance": -1.0 }),
        );
        assert!(bad.is_err());
        // Zero is inside the old `0..=1000` contract range and outside the handler. Both
        // layers refuse it, and the same four keys accept a positive tolerance.
        let gate = crate::contract::contract("anim.clip_report");
        for key in [
            "position_tolerance",
            "rotation_tolerance",
            "scale_tolerance",
            "propagated_tolerance",
        ] {
            let zero = json!({ "entity": e.to_bits(), key: 0.0 });
            assert!(
                crate::contract::check_params(gate, &zero).is_err(),
                "{key} zero passed the gate"
            );
            assert!(
                clip_report(world, &zero).is_err(),
                "{key} zero passed the handler"
            );
            let ok = json!({ "entity": e.to_bits(), key: 0.005 });
            assert!(crate::contract::check_params(gate, &ok).is_ok(), "{key}");
            assert!(clip_report(world, &ok).is_ok(), "{key}");
        }
    }

    /// `anim.clip_report` lists a clip's cooked contact curves (ADR-8308).
    #[test]
    fn clip_report_lists_contact_curves() {
        use regia_animation::clip::NamedCurve;
        use regia_animation::foot_ik::contact::{contact_curve, contact_frame_times};
        let mut walk = AnimationClip::empty("walk", 1.0);
        let times = contact_frame_times(&walk);
        let labels: Vec<bool> = (0..times.len()).map(|i| i < 18).collect();
        walk.curves.push(NamedCurve {
            name: "contact.foot_l".into(),
            curve: contact_curve(&times, &labels),
        });
        walk.curves.push(NamedCurve {
            name: "FootPlant_L".into(),
            curve: regia_animation::clip::TrackCurve::Static(1.0),
        });
        let mut app = App::new();
        let e = app.world_mut().spawn(Animator::single(walk, true)).id();
        let out = clip_report(
            app.world_mut(),
            &json!({ "entity": e.to_bits(), "compression": false }),
        )
        .expect("report");
        let curves = out["clips"][0]["contact_curves"]
            .as_array()
            .expect("contact curves");
        assert_eq!(curves.len(), 1, "only contact.* curves: {curves:?}");
        assert_eq!(curves[0]["name"], "contact.foot_l");
        assert_eq!(curves[0]["joint"], "foot_l");
        assert_eq!(curves[0]["touchdowns"], json!(0));
        let share = curves[0]["planted_share"].as_f64().expect("share");
        assert!((share - 18.0 / 31.0).abs() < 1e-9, "{share}");
    }
}
