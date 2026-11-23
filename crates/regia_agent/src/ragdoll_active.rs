// SPDX-License-Identifier: MPL-2.0
//! Active ragdolls on the agent protocol (spec 93 § 10, book/agents/ragdoll-active.md, ADR-2498).
//!
//! - `ragdoll.state { entity }` (read): mode, phase (`animated`, `active`, `falling`, `passive`, `getting_up`),
//!   group strength and hit multipliers, center of mass and its offset from the support polygon, the balance lean,
//!   motor counts, and the chosen get-up clip.
//! - `ragdoll.active_set { entity, strength?, gains? }`: sets the authored group strength (one number for every
//!   group, or `{spine?, arms?, legs?}`) and per-joint motor gains (`[{joint, kp?, kd?, max_torque?, group?,
//!   balance?}]`; a joint without a motor gets one). Every field is checked before anything changes. Switch the
//!   ragdoll on with `anim.ragdoll_set {mode: "active"}`. `recovery` (`"full"`, `"off"` or an object of fields over
//!   the current values) turns stepping, landing and planting on moving bodies on or off (ADR-2963).
//! - `ragdoll.push { entity, impulse, joint? }`: a one-step impulse (kg·m/s) on one ragdoll body (default: the heaviest),
//!   for push-recovery checks; drops are a rig transform in the air and `anim.ragdoll_set {mode: "active"}`.

use regia_app::prelude::FixedTime;
use regia_ecs::prelude::*;
use regia_framework::ragdoll::{
    Ragdoll, RagdollBalanceRole, RagdollBodies, RagdollGroup, RagdollGroupStrength, RagdollMode,
    RagdollMotor, RagdollRecovery,
};
use serde_json::{Value, json};

use crate::anim_montage::{invalid, missing};
use crate::protocol::RpcError;

fn vec3(v: regia_core::math::Vec3) -> Value {
    json!([v.x, v.y, v.z])
}

fn mode_name(mode: RagdollMode) -> &'static str {
    match mode {
        RagdollMode::Kinematic => "kinematic",
        RagdollMode::Powered => "powered",
        RagdollMode::Limp => "limp",
        RagdollMode::Active => "active",
    }
}

fn strength_json(s: &RagdollGroupStrength) -> Value {
    json!({ "spine": s.spine, "arms": s.arms, "legs": s.legs })
}

fn motor_json(m: &RagdollMotor) -> Value {
    json!({
        "joint": m.joint,
        "group": m.group.name(),
        "kp": m.kp,
        "kd": m.kd,
        "max_torque": m.max_torque,
        "balance": m.balance.name(),
    })
}

fn recovery_json(r: &RagdollRecovery) -> Value {
    json!({
        "max_steps": r.max_steps,
        "trigger_margin": r.trigger_margin,
        "overshoot": r.overshoot,
        "max_step": r.max_step,
        "min_width": r.min_width,
        "swing_time": r.swing_time,
        "lift": r.lift,
        "landing": r.landing,
        "pre_bend": r.pre_bend,
        "crouch": r.crouch,
        "absorb_time": r.absorb_time,
        "max_air_time": r.max_air_time,
        "moving_support": r.moving_support,
    })
}

fn ragdoll_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "list animated rigs with anim.list",
        ));
    }
    if world.get::<Ragdoll>(entity).is_none() {
        return Err(missing(
            "ragdoll_not_found",
            format!("entity {} has no Ragdoll", entity.to_bits()),
            "add one with regia_framework::ragdoll::Ragdoll (RagdollPlugin)",
        ));
    }
    Ok(entity)
}

/// The `ragdoll.state` result of one rig.
fn state_json(world: &World, entity: Entity) -> Value {
    let Some(ragdoll) = world.get::<Ragdoll>(entity) else {
        return Value::Null;
    };
    let active = &ragdoll.profile.active;
    let dt = world
        .get_resource::<FixedTime>()
        .map_or(1.0 / 60.0, |f| f.step().as_secs_f32().max(1e-4));
    let mut out = json!({
        "entity": entity.to_bits(),
        "mode": mode_name(ragdoll.mode),
        "drives": active.drives(),
        "strength": strength_json(&active.strength),
        "motors": active.motors.iter().map(motor_json).collect::<Vec<_>>(),
        "get_up_clips": active.get_up.iter().map(|g| g.clip.clone()).collect::<Vec<_>>(),
        "recovery": recovery_json(&active.recovery),
        "built": false,
    });
    if let Some(bodies) = world.get::<RagdollBodies>(entity) {
        let s = bodies.active();
        let hit: serde_json::Map<String, Value> = RagdollGroup::ALL
            .into_iter()
            .map(|g| (g.name().to_string(), json!(s.hit_strength(g, dt))))
            .collect();
        out["built"] = json!(true);
        out["phase"] = json!(s.phase().name());
        out["hit_strength"] = Value::Object(hit);
        out["center_of_mass"] = vec3(s.center_of_mass());
        out["com_offset"] = vec3(s.com_offset());
        out["support_distance"] = s.support_distance().map_or(Value::Null, |d| json!(d));
        out["capture_distance"] = s.capture_distance().map_or(Value::Null, |d| json!(d));
        out["support_points"] = json!(s.support_points());
        out["planted_feet"] = json!(s.planted_feet());
        out["tilt"] = vec3(s.tilt());
        out["driven_motors"] = json!(s.driven_motors());
        out["peak_torque"] = json!(s.peak_torque());
        out["settled"] = json!(bodies.settled());
        out["facing"] = s.facing().map_or(Value::Null, |f| json!(f.name()));
        out["get_up_clip"] = s.get_up_clip().map_or(Value::Null, |c| json!(c));
        out["step_phase"] = json!(s.step_phase().name());
        out["steps"] = json!(s.steps());
        out["total_steps"] = json!(s.total_steps());
        out["swing_target"] = s.swing_target().map_or(Value::Null, vec3);
        out["support_body"] = s.support_body().map_or(Value::Null, |e| json!(e.to_bits()));
        out["fall_cause"] = s.fall_cause().map_or(Value::Null, |c| json!(c.name()));
    }
    out
}

/// The `recovery` field of `ragdoll.active_set` over `current` (module docs).
fn read_recovery(value: &Value, current: RagdollRecovery) -> Result<RagdollRecovery, RpcError> {
    let bad = |message: String| invalid("ragdoll_bad_recovery", message);
    let mut r = match value {
        Value::String(s) if s == "full" => return Ok(RagdollRecovery::full()),
        Value::String(s) if s == "off" => return Ok(RagdollRecovery::default()),
        Value::Object(_) => current,
        _ => {
            return Err(bad(
                "`recovery` is \"full\", \"off\" or an object of recovery fields".into(),
            ));
        }
    };
    let Value::Object(fields) = value else {
        return Ok(r);
    };
    for (key, v) in fields {
        let num = || {
            v.as_f64()
                .map(|n| n as f32)
                .filter(|n| n.is_finite() && *n >= 0.0)
                .ok_or_else(|| bad(format!("`recovery.{key}` must be a finite number >= 0")))
        };
        let flag = || {
            v.as_bool()
                .ok_or_else(|| bad(format!("`recovery.{key}` must be true or false")))
        };
        match key.as_str() {
            "max_steps" => {
                r.max_steps = v
                    .as_u64()
                    .filter(|n| *n <= 16)
                    .ok_or_else(|| bad("`recovery.max_steps` must be an integer in 0..=16".into()))?
                    as u32;
            }
            "trigger_margin" => r.trigger_margin = num()?,
            "overshoot" => r.overshoot = num()?,
            "max_step" => r.max_step = num()?,
            "min_width" => r.min_width = num()?,
            "swing_time" => r.swing_time = num()?,
            "lift" => r.lift = num()?,
            "pre_bend" => r.pre_bend = num()?,
            "crouch" => r.crouch = num()?,
            "absorb_time" => r.absorb_time = num()?,
            "max_air_time" => r.max_air_time = num()?,
            "landing" => r.landing = flag()?,
            "moving_support" => r.moving_support = flag()?,
            other => return Err(bad(format!("unknown recovery field {other:?}"))),
        }
    }
    if !r.valid() {
        return Err(bad("`recovery.pre_bend` must be within 0..=0.5".into()));
    }
    Ok(r)
}

/// Largest impulse `ragdoll.push` takes, kg·m/s.
const MAX_PUSH: f32 = 10_000.0;

/// `ragdoll.push`: a one-step impulse on one body of a built ragdoll (module docs).
pub fn push(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = ragdoll_entity(world, params)?;
    let impulse = params
        .get("impulse")
        .and_then(Value::as_array)
        .filter(|a| a.len() == 3)
        .and_then(|a| {
            let v: Vec<f32> = a.iter().filter_map(|x| x.as_f64().map(|n| n as f32)).collect();
            (v.len() == 3).then(|| regia_core::math::Vec3::new(v[0], v[1], v[2]))
        })
        .filter(|v| v.is_finite() && v.length() <= MAX_PUSH)
        .ok_or_else(|| {
            invalid(
                "ragdoll_bad_impulse",
                format!("`impulse` must be [x, y, z] in kg·m/s, finite, at most {MAX_PUSH} long"),
            )
        })?;
    let Some(bodies) = world.get::<RagdollBodies>(entity) else {
        return Err(invalid(
            "ragdoll_not_built",
            "the ragdoll has no bodies yet: they are built on the first fixed step after the Ragdoll is added",
        ));
    };
    let skeleton = world
        .get::<regia_animation::SkinnedMesh>(entity)
        .map(|s| s.skeleton.clone());
    let name_of = |joint: u32| {
        skeleton
            .as_ref()
            .and_then(|s| s.joint_names.get(joint as usize).cloned())
    };
    let chosen = match params.get("joint") {
        None | Some(Value::Null) => {
            // The heaviest body: on a humanoid the chest.
            let ragdoll = world.get::<Ragdoll>(entity);
            bodies
                .bodies
                .iter()
                .max_by(|a, b| {
                    let mass = |j: u32| {
                        let name = name_of(j);
                        ragdoll
                            .and_then(|r| {
                                r.profile
                                    .bones
                                    .iter()
                                    .find(|bone| Some(&bone.joint) == name.as_ref())
                            })
                            .map_or(0.0, |bone| bone.mass)
                    };
                    mass(a.joint).total_cmp(&mass(b.joint))
                })
                .map(|b| (b.entity, b.joint))
        }
        Some(v) => {
            let joint = v
                .as_str()
                .ok_or_else(|| invalid("ragdoll_unknown_joint", "`joint` must be a bone name"))?;
            bodies
                .bodies
                .iter()
                .find(|b| name_of(b.joint).as_deref() == Some(joint))
                .map(|b| (b.entity, b.joint))
                .map(Some)
                .ok_or_else(|| {
                    invalid(
                        "ragdoll_unknown_joint",
                        format!("joint {joint:?} has no ragdoll bone"),
                    )
                })?
        }
    };
    let Some((body, joint)) = chosen else {
        return Err(invalid("ragdoll_not_built", "the ragdoll has no bodies"));
    };
    let joint = name_of(joint);
    world.entity_mut(body).insert(regia_physics::ExternalImpulse {
        impulse,
        torque_impulse: regia_core::math::Vec3::ZERO,
    });
    Ok(json!({
        "entity": entity.to_bits(),
        "body": body.to_bits(),
        "joint": joint,
        "impulse": vec3(impulse),
    }))
}

/// `ragdoll.state`: the active controller's state of one rig (module docs).
pub fn state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = ragdoll_entity(world, params)?;
    Ok(state_json(world, entity))
}

fn number(v: &Value, what: &str, code: &'static str) -> Result<f32, RpcError> {
    v.as_f64()
        .map(|n| n as f32)
        .filter(|n| n.is_finite())
        .ok_or_else(|| invalid(code, format!("`{what}` must be a finite number")))
}

fn unit(v: &Value, what: &str) -> Result<f32, RpcError> {
    let n = number(v, what, "ragdoll_bad_strength")?;
    if (0.0..=1.0).contains(&n) {
        Ok(n)
    } else {
        Err(invalid(
            "ragdoll_bad_strength",
            format!("`{what}` must be within 0..=1"),
        ))
    }
}

/// `ragdoll.active_set`: set group strength and motor gains (module docs). A rejected call changes nothing.
pub fn active_set(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = ragdoll_entity(world, params)?;
    let Some(ragdoll) = world.get::<Ragdoll>(entity) else {
        return Err(RpcError::invalid_params("the entity lost its Ragdoll"));
    };
    let mut profile = ragdoll.profile.active.clone();
    let bones: Vec<String> = ragdoll.profile.bones.iter().map(|b| b.joint.clone()).collect();
    let mut changed = false;
    match params.get("strength") {
        None | Some(Value::Null) => {}
        Some(Value::Object(groups)) => {
            for (key, value) in groups {
                let group = RagdollGroup::from_name(key).ok_or_else(|| {
                    invalid(
                        "ragdoll_bad_strength",
                        format!("unknown group {key:?} (spine|arms|legs)"),
                    )
                })?;
                profile.strength.set(group, unit(value, key)?);
            }
            changed = true;
        }
        Some(v) => {
            let s = unit(v, "strength")?;
            for g in RagdollGroup::ALL {
                profile.strength.set(g, s);
            }
            changed = true;
        }
    }
    if let Some(value) = params.get("recovery").filter(|v| !v.is_null()) {
        profile.recovery = read_recovery(value, profile.recovery)?;
        changed = true;
    }
    match params.get("gains") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            for item in list {
                let bad_joint = || invalid("ragdoll_bad_gain", "each gain needs `joint` (a ragdoll bone)");
                let joint = item.get("joint").and_then(Value::as_str).ok_or_else(bad_joint)?;
                if joint.is_empty() {
                    return Err(bad_joint());
                }
                if !bones.iter().any(|b| b == joint) {
                    return Err(invalid(
                        "ragdoll_unknown_joint",
                        format!("joint {joint:?} has no ragdoll bone"),
                    ));
                }
                let read = |key: &str| -> Result<Option<f32>, RpcError> {
                    match item.get(key) {
                        None | Some(Value::Null) => Ok(None),
                        Some(v) => number(v, key, "ragdoll_bad_gain").map(Some),
                    }
                };
                let (kp, kd, max_torque) = (read("kp")?, read("kd")?, read("max_torque")?);
                if kp.is_some_and(|v| v < 0.0) || kd.is_some_and(|v| v < 0.0) {
                    return Err(invalid("ragdoll_bad_gain", "`kp` and `kd` must be >= 0"));
                }
                if max_torque.is_some_and(|v| v <= 0.0) {
                    return Err(invalid("ragdoll_bad_gain", "`max_torque` must be > 0"));
                }
                let group = match item.get("group") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(v.as_str().and_then(RagdollGroup::from_name).ok_or_else(|| {
                        invalid("ragdoll_bad_gain", "`group` is \"spine\", \"arms\" or \"legs\"")
                    })?),
                };
                let balance = match item.get("balance") {
                    None | Some(Value::Null) => None,
                    Some(v) => Some(match v.as_str() {
                        Some("none") => RagdollBalanceRole::None,
                        Some("hip") => RagdollBalanceRole::Hip,
                        Some("ankle") => RagdollBalanceRole::Ankle,
                        _ => {
                            return Err(invalid(
                                "ragdoll_bad_gain",
                                "`balance` is \"none\", \"hip\" or \"ankle\"",
                            ));
                        }
                    }),
                };
                let index = match profile.motors.iter().position(|m| m.joint == joint) {
                    Some(i) => i,
                    None => {
                        profile.motors.push(RagdollMotor {
                            joint: joint.to_string(),
                            group: RagdollGroup::Spine,
                            kp: 0.0,
                            kd: 0.0,
                            max_torque: 100.0,
                            balance: RagdollBalanceRole::None,
                        });
                        profile.motors.len() - 1
                    }
                };
                let motor = &mut profile.motors[index];
                if let Some(v) = kp {
                    motor.kp = v;
                }
                if let Some(v) = kd {
                    motor.kd = v;
                }
                if let Some(v) = max_torque {
                    motor.max_torque = v;
                }
                if let Some(g) = group {
                    motor.group = g;
                }
                if let Some(b) = balance {
                    motor.balance = b;
                }
            }
            changed = true;
        }
        Some(_) => {
            return Err(invalid(
                "ragdoll_bad_gain",
                "`gains` is a list of {joint, kp?, kd?, max_torque?, group?, balance?}",
            ));
        }
    }
    if !changed {
        return Err(RpcError::invalid_params(
            "nothing to set: pass `strength`, `gains` or `recovery`",
        ));
    }
    if let Some(mut ragdoll) = world.get_mut::<Ragdoll>(entity) {
        ragdoll.profile.active = profile;
    }
    Ok(state_json(world, entity))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_framework::ragdoll::{RagdollBone, RagdollProfile, RagdollShape};

    fn rig(world: &mut World) -> Entity {
        let bone = |joint: &str| RagdollBone {
            joint: joint.into(),
            radius: 0.1,
            mass: 5.0,
            length: None,
            limits: None,
            shape: RagdollShape::Capsule,
        };
        let profile = RagdollProfile {
            bones: vec![bone("hips"), bone("spine")],
            ..RagdollProfile::default()
        };
        world.spawn(Ragdoll::new(profile)).id()
    }

    fn code(err: &RpcError) -> Option<&Value> {
        err.data.as_ref().and_then(|d| d.get("error_code"))
    }

    #[test]
    fn state_reports_an_unbuilt_ragdoll_and_active_set_sets_gains_and_strength() {
        let mut world = World::new();
        let e = rig(&mut world);
        let out = state(&mut world, &json!({ "entity": e.to_bits() })).expect("state");
        assert_eq!(out["mode"], "kinematic");
        assert_eq!(out["built"], false);
        assert_eq!(out["drives"], false, "a profile with no motors is passive");
        let out = active_set(
            &mut world,
            &json!({
                "entity": e.to_bits(),
                "strength": { "arms": 0.5 },
                "gains": [{ "joint": "spine", "kp": 300.0, "kd": 30.0, "max_torque": 400.0, "group": "spine" }],
            }),
        )
        .expect("set");
        assert_eq!(out["drives"], true);
        assert_eq!(out["strength"]["arms"], 0.5);
        assert_eq!(out["motors"][0]["joint"], "spine");
        let out =
            active_set(&mut world, &json!({ "entity": e.to_bits(), "strength": 0.25 })).expect("all groups");
        assert_eq!(out["strength"]["legs"], 0.25);
        let motor = &world.get::<Ragdoll>(e).expect("ragdoll").profile.active.motors[0];
        assert_eq!((motor.kp, motor.kd, motor.max_torque), (300.0, 30.0, 400.0));
    }

    #[test]
    fn active_set_rejects_bad_fields_and_changes_nothing() {
        let mut world = World::new();
        let e = rig(&mut world);
        let id = e.to_bits();
        for (params, want) in [
            (json!({ "entity": id, "strength": 1.5 }), "ragdoll_bad_strength"),
            (
                json!({ "entity": id, "strength": { "tail": 0.5 } }),
                "ragdoll_bad_strength",
            ),
            (
                json!({ "entity": id, "gains": [{ "joint": "knee", "kp": 1.0 }] }),
                "ragdoll_unknown_joint",
            ),
            (
                json!({ "entity": id, "gains": [{ "joint": "spine", "kp": -1.0 }] }),
                "ragdoll_bad_gain",
            ),
            (
                json!({ "entity": id, "gains": [{ "joint": "spine", "max_torque": 0.0 }] }),
                "ragdoll_bad_gain",
            ),
            (
                json!({ "entity": id, "gains": [{ "joint": "spine", "balance": "knee" }] }),
                "ragdoll_bad_gain",
            ),
            (
                json!({ "entity": id, "gains": { "joint": "spine" } }),
                "ragdoll_bad_gain",
            ),
            (
                json!({ "entity": id, "strength": 0.5, "gains": [{ "joint": "spine", "kd": "x" }] }),
                "ragdoll_bad_gain",
            ),
        ] {
            let err = active_set(&mut world, &params).expect_err("rejected");
            assert_eq!(code(&err), Some(&json!(want)), "{params}");
        }
        let ragdoll = world.get::<Ragdoll>(e).expect("ragdoll");
        assert!(
            ragdoll.profile.active.motors.is_empty(),
            "a rejected call changes nothing"
        );
        assert_eq!(ragdoll.profile.active.strength, RagdollGroupStrength::default());
        assert!(
            active_set(&mut world, &json!({ "entity": id })).is_err(),
            "nothing to set"
        );
        let plain = world.spawn_empty().id();
        let err = state(&mut world, &json!({ "entity": plain.to_bits() })).expect_err("no ragdoll");
        assert_eq!(code(&err), Some(&json!("ragdoll_not_found")));
    }

    /// Spec 93 § 10 (ADR-2963): `ragdoll.active_set {recovery}` takes `"full"`, `"off"` or an
    /// object of fields over the current values; bad values are `ragdoll_bad_recovery` and
    /// change nothing.
    #[test]
    fn active_set_recovery_presets_fields_and_errors() {
        let mut world = World::new();
        let e = rig(&mut world);
        let id = e.to_bits();
        let out = active_set(&mut world, &json!({ "entity": id, "recovery": "full" })).expect("full");
        assert_eq!(out["recovery"], recovery_json(&RagdollRecovery::full()));
        let out = active_set(
            &mut world,
            &json!({ "entity": id, "recovery": { "max_steps": 2, "landing": false } }),
        )
        .expect("fields");
        let mut want = RagdollRecovery::full();
        want.max_steps = 2;
        want.landing = false;
        assert_eq!(
            out["recovery"],
            recovery_json(&want),
            "fields over the current values"
        );
        for bad in [
            json!("maybe"),
            json!(3),
            json!({ "max_steps": 99 }),
            json!({ "lift": -1.0 }),
            json!({ "landing": 1 }),
            json!({ "wings": 1.0 }),
            json!({ "pre_bend": 0.9 }),
        ] {
            let err =
                active_set(&mut world, &json!({ "entity": id, "recovery": bad })).expect_err("rejected");
            assert_eq!(code(&err), Some(&json!("ragdoll_bad_recovery")), "{bad}");
        }
        let now = world.get::<Ragdoll>(e).expect("ragdoll").profile.active.recovery;
        assert_eq!(
            recovery_json(&now),
            recovery_json(&want),
            "a rejected call changes nothing"
        );
        let out = active_set(&mut world, &json!({ "entity": id, "recovery": "off" })).expect("off");
        assert_eq!(out["recovery"], recovery_json(&RagdollRecovery::default()));
        let plain = world.spawn_empty().id();
        let err = active_set(&mut world, &json!({ "entity": plain.to_bits(), "strength": 0.5 }))
            .expect_err("no ragdoll");
        assert_eq!(code(&err), Some(&json!("ragdoll_not_found")));
    }

    /// Spec 93 § 10 (ADR-2963): `ragdoll.push` puts a one-step impulse on a built body; it names
    /// `ragdoll_not_found`, `ragdoll_bad_impulse`, `ragdoll_not_built` and
    /// `ragdoll_unknown_joint`.
    #[test]
    fn push_impulses_a_built_body_and_names_its_errors() {
        let mut world = World::new();
        let e = rig(&mut world);
        let id = e.to_bits();
        let plain = world.spawn_empty().id();
        for (params, want) in [
            (
                json!({ "entity": plain.to_bits(), "impulse": [1, 0, 0] }),
                "ragdoll_not_found",
            ),
            (json!({ "entity": id }), "ragdoll_bad_impulse"),
            (json!({ "entity": id, "impulse": [1, 0] }), "ragdoll_bad_impulse"),
            (
                json!({ "entity": id, "impulse": [1e9, 0, 0] }),
                "ragdoll_bad_impulse",
            ),
            (json!({ "entity": id, "impulse": [1, 0, 0] }), "ragdoll_not_built"),
        ] {
            let err = push(&mut world, &params).expect_err("rejected");
            assert_eq!(code(&err), Some(&json!(want)), "{params}");
        }
        // Built by hand: two bodies, no skeleton names.
        let (a, b) = (world.spawn_empty().id(), world.spawn_empty().id());
        let body =
            |joint: u32, entity: Entity, parent: Option<usize>| regia_framework::ragdoll::RagdollBody {
                joint,
                entity,
                parent,
                hit: 0.0,
                hit_decay: 0.0,
            };
        let mut built = RagdollBodies::default();
        built.bodies = vec![body(0, a, None), body(1, b, Some(0))];
        world.entity_mut(e).insert(built);
        let err = push(
            &mut world,
            &json!({ "entity": id, "joint": "knee", "impulse": [1, 0, 0] }),
        )
        .expect_err("unknown joint");
        assert_eq!(code(&err), Some(&json!("ragdoll_unknown_joint")));
        let out = push(&mut world, &json!({ "entity": id, "impulse": [0, 0, 30] })).expect("pushes");
        let hit = out["body"].as_u64().expect("body");
        assert!(hit == a.to_bits() || hit == b.to_bits(), "{out}");
        assert_eq!(out["impulse"], json!([0.0, 0.0, 30.0]));
        let pushed = if hit == a.to_bits() { a } else { b };
        let impulse = world
            .get::<regia_physics::ExternalImpulse>(pushed)
            .expect("an impulse on the body");
        assert_eq!(impulse.impulse, regia_core::math::Vec3::new(0.0, 0.0, 30.0));
    }

    /// Spec 93 § 10 State: a built ragdoll reports every state field, with the names the book
    /// lists (`tilt`, `driven_motors`, the recovery fields), at their values before the first step.
    #[test]
    fn state_reports_every_built_field() {
        let mut world = World::new();
        let e = rig(&mut world);
        let (a, b) = (world.spawn_empty().id(), world.spawn_empty().id());
        let body =
            |joint: u32, entity: Entity, parent: Option<usize>| regia_framework::ragdoll::RagdollBody {
                joint,
                entity,
                parent,
                hit: 0.0,
                hit_decay: 0.0,
            };
        let mut built = RagdollBodies::default();
        built.bodies = vec![body(0, a, None), body(1, b, Some(0))];
        world.entity_mut(e).insert(built);
        let out = state(&mut world, &json!({ "entity": e.to_bits() })).expect("state");
        assert_eq!(out["built"], true);
        assert_eq!(out["phase"], "animated");
        for group in ["spine", "arms", "legs"] {
            assert_eq!(out["hit_strength"][group], 1.0, "{group}: {out}");
        }
        assert_eq!(out["hit_strength"].as_object().map(|m| m.len()), Some(3));
        for key in ["center_of_mass", "com_offset", "tilt"] {
            let v = out[key]
                .as_array()
                .unwrap_or_else(|| panic!("{key} is an array: {out}"));
            assert_eq!(v.len(), 3, "{key}");
            assert!(v.iter().all(Value::is_number), "{key}: {out}");
        }
        for key in [
            "support_points",
            "planted_feet",
            "driven_motors",
            "steps",
            "total_steps",
        ] {
            assert!(out[key].is_u64(), "{key} is a count: {out}");
        }
        assert_eq!(out["steps"], 0);
        assert_eq!(out["total_steps"], 0);
        assert_eq!(out["driven_motors"], 0);
        assert!(out["peak_torque"].is_number(), "{out}");
        assert_eq!(out["settled"], false);
        assert_eq!(out["step_phase"], "stand");
        for key in [
            "support_distance",
            "capture_distance",
            "facing",
            "get_up_clip",
            "swing_target",
            "support_body",
            "fall_cause",
        ] {
            assert!(out.get(key).is_some(), "{key} is present: {out}");
        }
        assert_eq!(out["facing"], Value::Null, "no facing before it settles");
        assert_eq!(out["fall_cause"], Value::Null, "it has not fallen");
        assert_eq!(out["support_body"], Value::Null, "no foot on anything");
    }
}
