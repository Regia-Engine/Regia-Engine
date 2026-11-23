// SPDX-License-Identifier: MPL-2.0
//! Retargeting and crowds on the agent protocol (`book/agents/retarget-crowds.md`,
//! `docs/specs/85-retarget-crowds.md` § 4): `anim.retarget_check` reports how well one rig's
//! animation retargets onto another, `anim.crowd_status` reads the crowd counters and one agent.
//!
//! Both are read-only. Handlers are plain functions over `&mut World`; tests drive them without
//! a server.

use std::sync::Arc;

use regia_animation::retarget::Retargeter;
use regia_animation::{
    AnimationClip, Animator, AnimatorSource, CrowdAgentId, CrowdAnimation, RetargetSetup, SkinnedMesh,
};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

const GUIDE: &str = "see book/agents/retarget-crowds.md";

/// A coded error: `data.code` is the lowercase reason, the message keeps it as a `[code]` prefix.
fn coded(jsonrpc: i64, code: &str, message: impl std::fmt::Display) -> RpcError {
    RpcError::new(jsonrpc, code, format!("[{code}] {message}"), GUIDE)
}

fn skeleton_of(
    world: &World,
    params: &Value,
    key: &str,
) -> Result<(Entity, Arc<regia_animation::Skeleton>), RpcError> {
    let bits = params
        .get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` (a rig entity id) is required")))?;
    let entity = Entity::try_from_bits(bits)
        .ok_or_else(|| RpcError::invalid_params(format!("`{key}` is not an entity id")))?;
    let mesh = world
        .get_entity(entity)
        .ok()
        .and_then(|e| e.get::<SkinnedMesh>())
        .ok_or_else(|| {
            RpcError::not_found(
                format!("entity {bits} is not a skinned rig (no SkinnedMesh)"),
                "list animated rigs with anim.list",
            )
        })?;
    Ok((entity, Arc::clone(&mesh.skeleton)))
}

fn clip_of(world: &World, entity: Entity, name: &str) -> Result<Arc<AnimationClip>, RpcError> {
    let missing = || {
        coded(
            protocol::NOT_FOUND,
            "unknown_clip",
            format!("clip {name:?} is not played by entity {}", entity.to_bits()),
        )
    };
    let animator = world.get::<Animator>(entity).ok_or_else(missing)?;
    match &animator.source {
        AnimatorSource::Graph { clips, .. } => clips.get(name).cloned().ok_or_else(missing),
        AnimatorSource::SingleClip { clip, .. } if clip.name == name => Ok(Arc::clone(clip)),
        AnimatorSource::SingleClip { .. } => Err(missing()),
    }
}

/// `anim.retarget_check { source, target, clip?, setup? }`: build a retargeter from the source
/// rig's skeleton onto the target rig's (guessed humanoid roles, or the `.retarget.ron` text in
/// `setup`) and report the mapping and, with `clip`, the bake's quality (`RetargetReport`).
pub fn retarget_check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let (source_entity, source) = skeleton_of(world, params, "source")?;
    let (_, target) = skeleton_of(world, params, "target")?;
    let setup = match params.get("setup") {
        None | Some(Value::Null) => RetargetSetup::default(),
        Some(Value::String(text)) => {
            RetargetSetup::from_ron_str(text).map_err(|e| coded(protocol::INVALID_PARAMS, e.code(), &e))?
        }
        Some(_) => {
            return Err(coded(
                protocol::INVALID_PARAMS,
                "bad_setup",
                "`setup` must be .retarget.ron text",
            ));
        }
    };
    let clip = match params.get("clip") {
        None | Some(Value::Null) => None,
        Some(Value::String(name)) => Some(clip_of(world, source_entity, name)?),
        Some(_) => {
            return Err(coded(
                protocol::INVALID_PARAMS,
                "unknown_clip",
                "`clip` must be a clip name",
            ));
        }
    };
    let rt: Retargeter = setup
        .build(&source, &target)
        .map_err(|e| coded(protocol::INVALID_PARAMS, e.code(), &e))?;
    let report = rt.check(clip.as_deref());
    let mut out = serde_json::to_value(&report).map_err(|e| RpcError::internal(e.to_string()))?;
    out["ok"] = json!(report.ok());
    out["mapped_joints"] = json!(rt.mapped_joints());
    Ok(out)
}

/// `anim.crowd_status { agent? }`: the crowd's counters of the last step and its digest; with
/// `agent`, that agent's playback state.
pub fn crowd_status(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let crowd = world.get_resource::<CrowdAnimation>().ok_or_else(|| {
        RpcError::not_found(
            "no CrowdAnimation resource (the app has no crowd)",
            "add regia_animation::CrowdAnimationPlugin and agents",
        )
    })?;
    let crowd = &crowd.0;
    let stats = crowd.stats();
    let archetypes: Vec<Value> = crowd
        .archetypes()
        .iter()
        .map(|a| {
            json!({
                "name": a.name,
                "joints": a.skeleton.len(),
                "hip_height": a.hip_height(),
                "has_legs": a.has_legs(),
                "gaits": a.gaits().iter().enumerate().map(|(i, g)| json!({
                    "clip": g.clip.name,
                    "speed": g.speed,
                    "cycle": g.cycle,
                    "planted": a.planted_share(i),
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut out = json!({
        "agents": stats.agents.max(crowd.len()),
        "archetypes": archetypes,
        "step": stats.step,
        "evaluated_poses": stats.evaluated_poses,
        "shared_hits": stats.shared_hits,
        "updated": stats.updated,
        "skipped": stats.skipped,
        "per_tier": stats.per_tier,
        "locked_feet": stats.locked_feet,
        "digest": format!("{:016x}", crowd.digest()),
        "agent": Value::Null,
    });
    match params.get("agent") {
        None | Some(Value::Null) => {}
        Some(value) => {
            let index = value
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or_else(|| {
                    coded(
                        protocol::INVALID_PARAMS,
                        "bad_agent",
                        "`agent` must be an agent index",
                    )
                })?;
            let state = crowd.agent_state(CrowdAgentId(index)).ok_or_else(|| {
                RpcError::not_found(
                    format!("crowd agent {index} does not exist"),
                    "anim.crowd_status without `agent` gives the agent count",
                )
            })?;
            out["agent"] = json!({
                "id": index,
                "archetype": state.archetype,
                "tier": state.tier,
                "gait": state.gait,
                "weight": state.weight,
                "phase": state.phase,
                "rate": state.rate,
                "speed": state.speed,
                "yaw": state.yaw,
                "position": [state.position.x, state.position.y, state.position.z],
                "hip_ratio": state.hip_ratio,
                "feet_locked": state.feet_locked,
                "feet_planted": state.feet_planted,
                "evaluated": crowd.palette(CrowdAgentId(index)).is_some(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::components::spawn_skeleton;
    use regia_animation::demo_rig::{DemoGait, DemoRig, RigNaming, demo_humanoid, demo_locomotion};
    use regia_animation::{CrowdAgentDesc, CrowdArchetype, CrowdSettings};

    fn rig(world: &mut World, naming: RigNaming, size: f32, with_clips: bool) -> Entity {
        let skeleton = demo_humanoid(&DemoRig {
            naming,
            size,
            ..DemoRig::default()
        })
        .expect("rig");
        let clips = demo_locomotion(&skeleton, &DemoGait::standard());
        let binding = spawn_skeleton(world, Arc::new(skeleton));
        let mut entity = world.spawn(binding);
        if with_clips {
            let walk = clips.into_iter().nth(1).expect("walk");
            entity.insert(Animator::single(walk, true));
        }
        entity.id()
    }

    fn reason(err: &RpcError) -> String {
        err.data
            .as_ref()
            .and_then(|d| d.get("code"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned()
    }

    #[test]
    fn retarget_check_reports_mapping_and_clip_quality() {
        let mut world = World::new();
        let source = rig(&mut world, RigNaming::Plain, 1.0, true);
        let target = rig(&mut world, RigNaming::Biped, 0.8, false);
        let report = retarget_check(
            &mut world,
            &json!({ "source": source.to_bits(), "target": target.to_bits(), "clip": "walk" }),
        )
        .expect("checks");
        assert_eq!(report["ok"], json!(true), "{report}");
        assert_eq!(report["issues"], json!([]));
        let scale = report["scale"].as_f64().expect("scale");
        assert!((scale - 0.8).abs() < 1e-3, "{scale}");
        assert_eq!(report["clip"]["name"], "walk");
        let ratio = report["clip"]["speed_ratio"].as_f64().expect("ratio");
        assert!((ratio - 0.8).abs() < 0.01, "{ratio}");
        assert!(report["mapped"].as_array().is_some_and(|m| m.len() == 19));

        // A setup that drops root motion; no clip: mapping only.
        let setup = "(format_version: 1, root_motion: Off)";
        let report = retarget_check(
            &mut world,
            &json!({ "source": source.to_bits(), "target": target.to_bits(), "setup": setup }),
        )
        .expect("checks");
        assert_eq!(report["root_motion_scale"], Value::Null);
        assert_eq!(report["clip"], Value::Null);
    }

    #[test]
    fn retarget_check_errors_carry_reason_codes() {
        let mut world = World::new();
        let source = rig(&mut world, RigNaming::Plain, 1.0, true);
        let target = rig(&mut world, RigNaming::Game, 1.0, false);
        let call = |world: &mut World, extra: Value| {
            let mut params = json!({ "source": source.to_bits(), "target": target.to_bits() });
            if let (Some(p), Some(e)) = (params.as_object_mut(), extra.as_object()) {
                p.extend(e.clone());
            }
            retarget_check(world, &params).expect_err("fails")
        };
        let err = call(&mut world, json!({ "setup": "(format_version: 9)" }));
        assert!(err.message.starts_with("[bad_setup]"), "{}", err.message);
        assert_eq!(reason(&err), "bad_setup");
        let err = call(
            &mut world,
            json!({ "setup": "(format_version: 1, target: (roles: Explicit({ \"hips\": \"nope\" })))" }),
        );
        assert!(err.message.starts_with("[missing_joint]"), "{}", err.message);
        assert_eq!(reason(&err), "missing_joint");
        let err = call(
            &mut world,
            json!({ "setup": "(format_version: 1, target: (roles: Explicit({ \"tail\": \"root\" })))" }),
        );
        assert!(err.message.starts_with("[no_common_roles]"), "{}", err.message);
        assert_eq!(reason(&err), "no_common_roles");
        let err = call(&mut world, json!({ "clip": "swim" }));
        assert!(err.message.starts_with("[unknown_clip]"), "{}", err.message);
        assert_eq!(reason(&err), "unknown_clip");
        assert_eq!(err.code, RpcError::not_found("x", "y").code);
        // Spec 85 § 4: a `clip` that is not a string is F0002 with the same reason code.
        let err = call(&mut world, json!({ "clip": 3 }));
        assert!(err.message.starts_with("[unknown_clip]"), "{}", err.message);
        assert_eq!(reason(&err), "unknown_clip");
        assert_eq!(err.code, protocol::INVALID_PARAMS);
        let plain = world.spawn_empty().id();
        let err = retarget_check(
            &mut world,
            &json!({ "source": plain.to_bits(), "target": target.to_bits() }),
        )
        .expect_err("not a rig");
        assert_eq!(err.code, RpcError::not_found("x", "y").code);
        assert!(retarget_check(&mut world, &json!({ "target": target.to_bits() })).is_err());
    }

    #[test]
    fn crowd_status_reads_counters_and_one_agent() {
        let mut world = World::new();
        assert!(crowd_status(&mut world, &json!({})).is_err(), "no crowd resource");
        let skeleton = demo_humanoid(&DemoRig::default()).expect("rig");
        let clips = demo_locomotion(&skeleton, &DemoGait::standard());
        let mut crowd = CrowdAnimation::default();
        crowd.0.set_settings(CrowdSettings {
            chunk: 0,
            ..CrowdSettings::default()
        });
        let a = crowd
            .0
            .add_archetype(CrowdArchetype::new("plain", Arc::new(skeleton), clips).expect("archetype"));
        for i in 0..4 {
            crowd
                .0
                .add_agent(CrowdAgentDesc {
                    archetype: a,
                    velocity: regia_core::math::Vec3::new(0.0, 0.0, -1.3),
                    phase: i as f32 * 0.25,
                    ..CrowdAgentDesc::default()
                })
                .expect("agent");
        }
        crowd.0.step();
        world.insert_resource(crowd);
        let status = crowd_status(&mut world, &json!({ "agent": 2 })).expect("status");
        assert_eq!(status["agents"], json!(4));
        assert_eq!(status["step"], json!(1));
        assert_eq!(status["updated"], json!(4));
        assert_eq!(status["archetypes"][0]["gaits"].as_array().map(Vec::len), Some(3));
        assert_eq!(status["agent"]["id"], json!(2));
        assert_eq!(status["agent"]["evaluated"], json!(true));
        let rate = status["agent"]["rate"].as_f64().expect("rate");
        assert!((rate - 1.0).abs() < 1e-4, "{rate}");
        assert_eq!(status["digest"].as_str().map(str::len), Some(16));
        let err = crowd_status(&mut world, &json!({ "agent": "x" })).expect_err("bad agent");
        assert!(err.message.starts_with("[bad_agent]"));
        assert_eq!(reason(&err), "bad_agent");
        let err = crowd_status(&mut world, &json!({ "agent": 99 })).expect_err("no agent");
        assert_eq!(err.code, RpcError::not_found("x", "y").code);
    }
}
