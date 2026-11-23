// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for the character rig systems (Tincal C4/C5/C7):
//! `anim.rig_attach` compiles a `.drivers.ron` document (twist chains, helper joints, drivers,
//! pose-space correctives) onto a rig, `anim.rig_state` reports what it computed last frame,
//! and `anim.profile_check` checks a rig's skeleton against a skeleton profile.
//!
//! Handlers are plain functions over `&mut World`; tests drive them without a server.

use std::sync::Arc;

use regia_animation::{DriverSet, NamedMorphs, RigDrivers, SkeletonProfile, SkinnedMesh};
use regia_app::prelude::*;
use serde_json::{Value, json};

use crate::protocol::RpcError;

fn rig_entity(world: &World, params: &Value) -> Result<Entity, RpcError> {
    let entity = crate::scene::entity_from(params)?;
    if world.get_entity(entity).is_err() || world.get::<SkinnedMesh>(entity).is_none() {
        return Err(RpcError::not_found(
            format!(
                "entity {} is not a skinned rig (no SkinnedMesh)",
                entity.to_bits()
            ),
            "list animated rigs with anim.list",
        ));
    }
    Ok(entity)
}

fn text_param<'a>(params: &'a Value, name: &str) -> Result<Option<&'a str>, RpcError> {
    crate::params::opt_str(params, name)
}

/// `anim.rig_attach`: compile `ron` (a `.drivers.ron` document) against the rig's skeleton and
/// attach it. `morphs` lists the morph target names in bank order (default: the names the
/// document uses, in first-appearance order). With `dry_run: true` the document is only
/// checked.
pub fn rig_attach(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_entity(world, params)?;
    let ron = text_param(params, "ron")?
        .ok_or_else(|| RpcError::invalid_params("`ron` (the .drivers.ron text) is required"))?;
    let set = DriverSet::from_ron_str(ron).map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let morphs: Vec<String> = match params.get("morphs") {
        None | Some(Value::Null) => NamedMorphs::from_set(&set).names,
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| v.as_str().map(str::to_string))
            .collect::<Option<_>>()
            .ok_or_else(|| RpcError::invalid_params("`morphs` must be an array of names"))?,
        Some(_) => return Err(RpcError::invalid_params("`morphs` must be an array of names")),
    };
    let Some(binding) = world.get::<SkinnedMesh>(entity) else {
        return Err(RpcError::internal("the rig lost its skeleton"));
    };
    let rig = set
        .compile(&binding.skeleton, &morphs)
        .map_err(|e| RpcError::invalid_params(e.to_string()))?;
    let summary = json!({
        "entity": entity.to_bits(),
        "twists": rig.twist_names().collect::<Vec<_>>(),
        "helpers": rig.helper_names().collect::<Vec<_>>(),
        "drivers": rig.driver_names().collect::<Vec<_>>(),
        "correctives": rig.corrective_names().collect::<Vec<_>>(),
        "materials": rig.material_names(),
        "morphs": morphs,
    });
    let dry_run = crate::scene::dry_run_flag(params)?;
    if !dry_run {
        world.entity_mut(entity).insert(RigDrivers::new(Arc::new(rig)));
    }
    let mut out = summary;
    out["attached"] = json!(!dry_run);
    Ok(out)
}

/// `anim.rig_state`: what the attached rig computed in the last frame: the input read and the
/// value written by each driver, the roll measured by each twist chain, helper angles,
/// corrective kernel activations, and material parameter outputs.
pub fn rig_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_entity(world, params)?;
    let Some(drivers) = world.get::<RigDrivers>(entity) else {
        return Err(RpcError::not_found(
            format!("entity {} has no attached rig drivers", entity.to_bits()),
            "attach a .drivers.ron with anim.rig_attach",
        ));
    };
    let (rig, state) = (&drivers.rig, &drivers.state);
    let pair = |names: Vec<&str>, values: &[f32]| -> Vec<Value> {
        names
            .into_iter()
            .zip(values)
            .map(|(n, v)| json!({ "name": n, "value": v }))
            .collect()
    };
    let drivers_json: Vec<Value> = rig
        .driver_names()
        .zip(state.driver_inputs.iter().zip(&state.driver_outputs))
        .map(|(n, (i, o))| json!({ "name": n, "input": i, "output": o }))
        .collect();
    let materials: serde_json::Map<String, Value> = rig
        .material_names()
        .iter()
        .zip(&state.materials)
        .map(|(n, v)| (n.clone(), json!(v)))
        .collect();
    let correctives: Vec<Value> = rig
        .corrective_names()
        .zip(&state.corrective_activations)
        .map(|(n, a)| json!({ "name": n, "activations": a }))
        .collect();
    Ok(json!({
        "entity": entity.to_bits(),
        "evaluated": !state.driver_inputs.is_empty() || !state.twist_degrees.is_empty()
            || !state.corrective_activations.is_empty(),
        "twists": pair(rig.twist_names().collect(), &state.twist_degrees),
        "helpers": pair(rig.helper_names().collect(), &state.helper_degrees),
        "drivers": drivers_json,
        "correctives": correctives,
        "materials": materials,
    }))
}

/// `anim.profile_check`: check the rig's skeleton against a skeleton profile: `profile` names a
/// built-in (`generic-humanoid`, `vrm`), or `ron` is a `.skelprofile.ron` text. Reports the
/// match, missing and extra joints, hierarchy problems, and the measured arm elevation.
pub fn profile_check(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_entity(world, params)?;
    let profile = match (text_param(params, "ron")?, text_param(params, "profile")?) {
        (Some(ron), _) => {
            SkeletonProfile::from_ron_str(ron).map_err(|e| RpcError::invalid_params(e.to_string()))?
        }
        (None, Some(name)) => SkeletonProfile::builtin(name).ok_or_else(|| {
            RpcError::invalid_params(format!(
                "unknown built-in profile {name:?} (one of {})",
                SkeletonProfile::BUILTINS.join(", ")
            ))
        })?,
        (None, None) => {
            return Err(RpcError::invalid_params(format!(
                "pass `profile` ({}) or `ron` (a .skelprofile.ron text)",
                SkeletonProfile::BUILTINS.join(" | ")
            )));
        }
    };
    if let Err(problems) = profile.validate() {
        return Err(RpcError::invalid_params(format!(
            "the profile is invalid: {}",
            problems.join("; ")
        )));
    }
    let Some(binding) = world.get::<SkinnedMesh>(entity) else {
        return Err(RpcError::internal("the rig lost its skeleton"));
    };
    let report = profile.check_skeleton(&binding.skeleton);
    let rest_pose = report.rest_pose.as_ref().map(|r| {
        json!({
            "ok": r.ok,
            "expected_deg": r.expected.0,
            "tolerance_deg": r.expected.1,
            "arms": r.arms.iter().map(|a| json!({ "side": a.side, "elevation_deg": a.degrees })).collect::<Vec<_>>(),
        })
    });
    Ok(json!({
        "entity": entity.to_bits(),
        "profile": report.profile,
        "ok": report.ok(),
        "matched": report.matched,
        "missing_required": report.missing_required,
        "missing_optional": report.missing_optional,
        "missing_roles": report.missing_roles,
        "extra_joints": report.extra_joints,
        "forbidden_extra": report.forbidden_extra,
        "guessed": report.guessed,
        "hierarchy": report.hierarchy,
        "rest_pose": rest_pose,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_animation::components::spawn_skeleton;
    use regia_animation::{AnimationPlugin, Animator, Skeleton};
    use regia_core::math::{Quat, Vec3};
    use regia_scene::Transform;

    const RON: &str = r#"(
        format_version: 1,
        drivers: [(
            name: "wrinkle",
            input: JointAngle(joint: "fore", axis: (1.0, 0.0, 0.0)),
            curve: (keys: [(location: 0.0, value: 0.0, kind: Linear), (location: 90.0, value: 1.0, kind: Linear)]),
            output: Morph("bulge"),
        )],
    )"#;

    fn app_with_rig() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((regia_app::MinimalPlugins, AnimationPlugin));
        let names = ["root", "upper", "fore"].map(String::from).to_vec();
        let skeleton = Skeleton::from_local_bind(
            names,
            vec![None, Some(0), Some(1)],
            vec![regia_animation::skeleton::LocalBind::default(); 3],
        )
        .expect("valid skeleton");
        // A clip that holds the forearm bent 45 degrees about X.
        let mut clip = regia_animation::AnimationClip::empty("bent", 1.0);
        clip.tracks.push(regia_animation::clip::JointTrack {
            joint: 2,
            translations: regia_animation::clip::TrackCurve::Static(Vec3::ZERO),
            rotations: regia_animation::clip::TrackCurve::Static(Quat::from_rotation_x(45f32.to_radians())),
            scales: regia_animation::clip::TrackCurve::Static(Vec3::ONE),
        });
        let binding = spawn_skeleton(app.world_mut(), Arc::new(skeleton));
        let rig = app
            .world_mut()
            .spawn((Transform::default(), binding, Animator::single(clip, true)))
            .id();
        (app, rig)
    }

    #[test]
    fn attach_runs_the_drivers_and_state_reports_them() {
        let (mut app, rig) = app_with_rig();
        let attached =
            rig_attach(app.world_mut(), &json!({ "entity": rig.to_bits(), "ron": RON })).expect("attaches");
        assert_eq!(attached["attached"], json!(true));
        assert_eq!(attached["drivers"], json!(["wrinkle"]));
        assert_eq!(attached["morphs"], json!(["bulge"]));
        for _ in 0..3 {
            app.update();
        }
        let state = rig_state(app.world_mut(), &json!({ "entity": rig.to_bits() })).expect("state");
        assert_eq!(state["evaluated"], json!(true));
        assert_eq!(state["drivers"][0]["name"], "wrinkle");
        let input = state["drivers"][0]["input"].as_f64().expect("number");
        assert!((input - 45.0).abs() < 0.01, "{input}");
        let output = state["drivers"][0]["output"].as_f64().expect("number");
        assert!((output - 0.5).abs() < 1e-3, "{output}");
        // The morph weight reached the rig.
        let weights = app
            .world()
            .get::<regia_animation::MorphWeights>(rig)
            .expect("weights");
        assert!((weights.0[0] - 0.5).abs() < 1e-3, "{:?}", weights.0);
    }

    #[test]
    fn attach_rejects_non_boolean_dry_run_without_attaching() {
        let (mut app, rig) = app_with_rig();
        let err = rig_attach(
            app.world_mut(),
            &json!({ "entity": rig.to_bits(), "ron": RON, "dry_run": "true" }),
        )
        .expect_err("non-boolean dry_run");
        assert_eq!(err.code, RpcError::invalid_params("x").code);
        assert!(app.world().get::<RigDrivers>(rig).is_none());
    }

    #[test]
    fn attach_reports_unresolved_names_and_dry_run_attaches_nothing() {
        let (mut app, rig) = app_with_rig();
        let bad = RON.replace("\"fore\"", "\"forearm\"");
        let err = rig_attach(app.world_mut(), &json!({ "entity": rig.to_bits(), "ron": bad }))
            .expect_err("unknown joint");
        assert!(err.message.contains("forearm"), "{}", err.message);
        let dry = rig_attach(
            app.world_mut(),
            &json!({ "entity": rig.to_bits(), "ron": RON, "dry_run": true }),
        )
        .expect("dry run");
        assert_eq!(dry["attached"], json!(false));
        assert!(app.world().get::<RigDrivers>(rig).is_none());
        let err =
            rig_state(app.world_mut(), &json!({ "entity": rig.to_bits() })).expect_err("nothing attached");
        assert!(err.message.contains("no attached rig drivers"));
    }

    #[test]
    fn profile_check_reports_missing_joints_and_rejects_unknown_profiles() {
        let (mut app, rig) = app_with_rig();
        let report = profile_check(
            app.world_mut(),
            &json!({ "entity": rig.to_bits(), "profile": "generic-humanoid" }),
        )
        .expect("checks");
        assert_eq!(report["ok"], json!(false));
        assert!(report["missing_required"].as_array().is_some_and(|m| m.len() > 5));
        assert_eq!(report["extra_joints"], json!(["root", "upper", "fore"]));
        assert!(
            profile_check(
                app.world_mut(),
                &json!({ "entity": rig.to_bits(), "profile": "x" })
            )
            .is_err()
        );
        assert!(profile_check(app.world_mut(), &json!({ "entity": rig.to_bits() })).is_err());
        let plain = app.world_mut().spawn_empty().id();
        assert!(
            profile_check(
                app.world_mut(),
                &json!({ "entity": plain.to_bits(), "profile": "vrm" })
            )
            .is_err()
        );
    }
}
