// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for scripted warps and the root-motion route (spec 34 M3 and M6,
//! ADR-1644, `book/agents/montages.md`): `anim.warp_state` reads a rig's warp targets and
//! scripted warps, `anim.scripted_warp` adds, removes, pauses and resumes scripted warps and
//! sets their targets, `anim.root_motion_route` reads or sets where root motion goes.
//!
//! Errors carry a stable lowercase `error_code` in `data` (listed in the book page).

use regia_animation::warp::{RotationKind, WarpAnchor, WarpMode, yaw_of};
use regia_animation::{
    Animator, MontagePlayer, RootMotionRoute, ScriptedWarp, ScriptedWarps, WarpError, WarpTarget,
};
use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use serde_json::{Value, json};

use crate::anim_montage::{finite_param, invalid, missing, rig_of, string_param, vec3_json};
use crate::protocol::RpcError;

fn target_json(name: &str, target: &WarpTarget) -> Value {
    json!({
        "name": name,
        "position": vec3_json(target.position),
        "facing_yaw": target.facing.map(yaw_of),
    })
}

fn mode_str(mode: WarpMode) -> &'static str {
    match mode {
        WarpMode::Similarity => "similarity",
        WarpMode::PathScale => "path_scale",
    }
}

fn kind_str(kind: RotationKind) -> &'static str {
    match kind {
        RotationKind::Auto => "auto",
        RotationKind::MatchTarget => "match_target",
        RotationKind::FaceTarget => "face_target",
    }
}

fn anchor_json(anchor: &WarpAnchor) -> Value {
    match anchor {
        WarpAnchor::Root => json!("root"),
        WarpAnchor::Bone(name) => json!({ "bone": name }),
        WarpAnchor::Offset(v) => json!({ "offset": v }),
    }
}

/// `anim.warp_state { entity }`: the rig's warp targets (montage and scripted), its scripted
/// warps (progress, enabled) and its root-motion route.
pub fn warp_state(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    let route = world
        .get::<Animator>(entity)
        .map_or(RootMotionRoute::default(), |a| a.root_motion_route);
    let montage_targets: Vec<Value> = world.get::<MontagePlayer>(entity).map_or(Vec::new(), |p| {
        p.warp_targets().iter().map(|(n, t)| target_json(n, t)).collect()
    });
    let (scripted, scripted_targets) = match world.get::<ScriptedWarps>(entity) {
        Some(warps) => {
            let list: Vec<Value> = warps
                .list()
                .iter()
                .map(|s| {
                    json!({
                        "id": s.warp.id,
                        "target": s.warp.target,
                        "duration": s.warp.duration,
                        "elapsed": s.elapsed,
                        "enabled": s.enabled,
                        "mode": mode_str(s.warp.mode),
                        "translation": s.warp.translation,
                        "vertical": s.warp.vertical,
                        "rotation": s.warp.rotation,
                        "rotation_kind": kind_str(s.warp.rotation_kind),
                        "rotation_fraction": s.warp.rotation_fraction,
                        "anchor": anchor_json(&s.warp.anchor),
                    })
                })
                .collect();
            let targets: Vec<Value> = warps.targets().iter().map(|(n, t)| target_json(n, t)).collect();
            (list, targets)
        }
        None => (Vec::new(), Vec::new()),
    };
    Ok(json!({
        "entity": entity.to_bits(),
        "root_motion_route": route.as_str(),
        "montage_targets": montage_targets,
        "scripted": scripted,
        "scripted_targets": scripted_targets,
    }))
}

fn bool_param(params: &Value, name: &str) -> Result<Option<bool>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(b)) => Ok(Some(*b)),
        Some(_) => Err(invalid("bad_bool", format!("`{name}` must be a boolean"))),
    }
}

fn vec3_param(params: &Value, name: &str) -> Result<Option<[f32; 3]>, RpcError> {
    match params.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.len() == 3 => {
            let c: Vec<f32> = items
                .iter()
                .filter_map(Value::as_f64)
                .map(|v| v as f32)
                .filter(|v| v.is_finite())
                .collect();
            match c.as_slice() {
                [x, y, z] => Ok(Some([*x, *y, *z])),
                _ => Err(invalid(
                    "warp_target_invalid",
                    format!("`{name}` needs three finite numbers"),
                )),
            }
        }
        Some(_) => Err(invalid("warp_target_invalid", format!("`{name}` is [x, y, z]"))),
    }
}

fn warp_error_code(error: &WarpError) -> &'static str {
    match error {
        WarpError::BadDuration => "warp_bad_duration",
        WarpError::BadName => "warp_bad_name",
        WarpError::BadFraction => "warp_bad_fraction",
    }
}

fn required<'v>(params: &'v Value, name: &str, code: &str) -> Result<&'v str, RpcError> {
    string_param(params, name)?
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid(code, format!("`{name}` is required")))
}

/// `anim.scripted_warp { entity, op, ... }`: `add` (id, target, duration, mode?, translation?,
/// vertical?, rotation?, rotation_kind?, rotation_fraction?, offset?), `remove` (id),
/// `pause_all`, `resume_all`, `set_target` (name, position, facing_yaw?) and `clear_target`
/// (name). Creating operations add a `ScriptedWarps` component when the rig has none;
/// rejected removals leave an absent component absent.
pub fn scripted_warp(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    let op = required(params, "op", "warp_unknown_op")?.to_string();
    // Parse everything before touching the world, so a rejected call leaves no trace.
    enum Op {
        Add(Box<ScriptedWarp>),
        Remove(String),
        PauseAll,
        ResumeAll,
        SetTarget(String, WarpTarget),
        ClearTarget(String),
    }
    let parsed = match op.as_str() {
        "add" => {
            let mut warp = ScriptedWarp::new(
                required(params, "id", "warp_bad_name")?,
                required(params, "target", "warp_bad_name")?,
                finite_param(params, "duration")?.unwrap_or(f32::NAN),
            );
            warp.mode = match string_param(params, "mode")? {
                None | Some("similarity") => WarpMode::Similarity,
                Some("path_scale") => WarpMode::PathScale,
                Some(other) => {
                    return Err(invalid(
                        "warp_bad_mode",
                        format!("`mode` is similarity or path_scale, got {other:?}"),
                    ));
                }
            };
            warp.rotation_kind = match string_param(params, "rotation_kind")? {
                None | Some("auto") => RotationKind::Auto,
                Some("match_target") => RotationKind::MatchTarget,
                Some("face_target") => RotationKind::FaceTarget,
                Some(other) => {
                    return Err(invalid(
                        "warp_bad_mode",
                        format!("`rotation_kind` is auto, match_target or face_target, got {other:?}"),
                    ));
                }
            };
            warp.translation = bool_param(params, "translation")?.unwrap_or(true);
            warp.vertical = bool_param(params, "vertical")?.unwrap_or(false);
            warp.rotation = bool_param(params, "rotation")?.unwrap_or(false);
            warp.rotation_fraction = finite_param(params, "rotation_fraction")?.unwrap_or(1.0);
            if let Some(offset) = vec3_param(params, "offset")? {
                warp.anchor = WarpAnchor::Offset(offset);
            }
            Op::Add(Box::new(warp))
        }
        "remove" => Op::Remove(required(params, "id", "warp_bad_name")?.to_string()),
        "pause_all" => Op::PauseAll,
        "resume_all" => Op::ResumeAll,
        "set_target" => {
            let name = required(params, "name", "warp_bad_name")?.to_string();
            let position = vec3_param(params, "position")?
                .ok_or_else(|| invalid("warp_target_invalid", "`position` is required: [x, y, z]"))?;
            let facing = finite_param(params, "facing_yaw")?.map(Quat::from_rotation_y);
            Op::SetTarget(
                name,
                WarpTarget {
                    position: Vec3::from_array(position),
                    facing,
                },
            )
        }
        "clear_target" => Op::ClearTarget(required(params, "name", "warp_bad_name")?.to_string()),
        other => {
            return Err(invalid(
                "warp_unknown_op",
                format!(
                    "`op` is add, remove, pause_all, resume_all, set_target or clear_target, got {other:?}"
                ),
            ));
        }
    };
    if let Op::Add(warp) = &parsed {
        // Validate on a scratch set so a bad request creates no component either.
        let mut scratch = ScriptedWarps::new();
        if let Err(error) = scratch.add((**warp).clone()) {
            return Err(invalid(warp_error_code(&error), error.to_string()));
        }
    }
    let unknown_id = |id: &str| {
        missing(
            "warp_unknown_id",
            format!("the rig has no scripted warp {id:?}"),
            "list them with anim.warp_state",
        )
    };
    let unknown_target = |name: &str| {
        missing(
            "warp_unknown_target",
            format!("no scripted-warp target named {name:?}"),
            "list them with anim.warp_state",
        )
    };
    if world.get::<ScriptedWarps>(entity).is_none() {
        match &parsed {
            Op::Remove(id) => return Err(unknown_id(id)),
            Op::ClearTarget(name) => return Err(unknown_target(name)),
            _ => {}
        }
        world.entity_mut(entity).insert(ScriptedWarps::new());
    }
    let Some(mut warps) = world.get_mut::<ScriptedWarps>(entity) else {
        return Err(RpcError::internal("the scripted warp set could not be added"));
    };
    let result = match parsed {
        Op::Add(warp) => {
            let id = warp.id.clone();
            warps
                .add(*warp)
                .map_err(|e| invalid(warp_error_code(&e), e.to_string()))?;
            json!({ "id": id })
        }
        Op::Remove(id) => {
            if !warps.remove(&id) {
                return Err(unknown_id(&id));
            }
            json!({ "id": id })
        }
        Op::PauseAll => {
            warps.pause_all();
            json!({})
        }
        Op::ResumeAll => {
            warps.resume_all();
            json!({})
        }
        Op::SetTarget(name, target) => {
            warps.set_target(name.clone(), target);
            json!({ "name": name })
        }
        Op::ClearTarget(name) => {
            if !warps.clear_target(&name) {
                return Err(unknown_target(&name));
            }
            json!({ "name": name })
        }
    };
    Ok(json!({ "entity": entity.to_bits(), "op": op, "result": result, "warps": warps.list().len() }))
}

/// `anim.root_motion_route { entity, route? }`: read the route, or set it to `ignore`,
/// `report`, `graph_and_montages` or `only_montages`.
pub fn root_motion_route(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let entity = rig_of(world, params)?;
    let wanted = match string_param(params, "route")? {
        None => None,
        Some("ignore") => Some(RootMotionRoute::Ignore),
        Some("report") => Some(RootMotionRoute::Report),
        Some("graph_and_montages") => Some(RootMotionRoute::GraphAndMontages),
        Some("only_montages") => Some(RootMotionRoute::OnlyMontages),
        Some(other) => {
            return Err(invalid(
                "root_route_unknown",
                format!("`route` is ignore, report, graph_and_montages or only_montages, got {other:?}"),
            ));
        }
    };
    let Some(mut animator) = world.get_mut::<Animator>(entity) else {
        return Err(RpcError::internal("animator vanished"));
    };
    let before = animator.root_motion_route;
    if let Some(route) = wanted {
        animator.root_motion_route = route;
    }
    Ok(json!({
        "entity": entity.to_bits(),
        "before": before.as_str(),
        "after": animator.root_motion_route.as_str(),
    }))
}

#[cfg(test)]
mod tests {
    use regia_animation::{AnimationClip, ClipLibrary};

    use super::*;

    fn rig() -> (World, Entity) {
        let mut world = World::new();
        let mut clips = ClipLibrary::new();
        clips.insert(AnimationClip::empty("swing", 1.0));
        let graph = regia_animation::AnimGraph {
            parameters: Vec::new(),
            root: regia_animation::AnimNode::Clip(regia_animation::graph::ClipNode {
                clip: "swing".into(),
                looping: true,
                speed: 1.0,
                sync_group: None,
            }),
        };
        let rig = world.spawn(Animator::graph(graph, clips)).id();
        (world, rig)
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("error_code")?.as_str()
    }

    fn call(world: &mut World, rig: Entity, extra: Value) -> Result<Value, RpcError> {
        let mut params = json!({ "entity": rig.to_bits() });
        if let (Some(base), Some(more)) = (params.as_object_mut(), extra.as_object()) {
            base.extend(more.clone());
        }
        scripted_warp(world, &params)
    }

    #[test]
    fn a_rejected_add_leaves_no_component_behind() {
        let (mut world, rig) = rig();
        // Missing duration becomes NaN and the warp set refuses it.
        let err =
            call(&mut world, rig, json!({ "op": "add", "id": "w", "target": "t" })).expect_err("no duration");
        assert_eq!(code(&err), Some("warp_bad_duration"));
        for duration in [0.0, -1.0] {
            let err = call(
                &mut world,
                rig,
                json!({ "op": "add", "id": "w", "target": "t", "duration": duration }),
            )
            .expect_err("bad duration");
            assert_eq!(code(&err), Some("warp_bad_duration"), "{duration}");
        }
        let err = call(
            &mut world,
            rig,
            json!({ "op": "add", "id": "w", "target": "t", "duration": 1.0, "rotation_fraction": 2.0 }),
        )
        .expect_err("fraction");
        assert_eq!(code(&err), Some("warp_bad_fraction"));
        assert!(
            world.get::<ScriptedWarps>(rig).is_none(),
            "a rejected call must not add the component"
        );
    }

    #[test]
    fn rejected_removal_preserves_absent_warp_component() {
        for (params, expected) in [
            (json!({ "op": "remove", "id": "nope" }), "warp_unknown_id"),
            (
                json!({ "op": "clear_target", "name": "door" }),
                "warp_unknown_target",
            ),
        ] {
            let (mut world, rig) = rig();
            let err = call(&mut world, rig, params).expect_err("unknown warp or target");
            assert_eq!(code(&err), Some(expected));
            assert!(
                world.get::<ScriptedWarps>(rig).is_none(),
                "{expected} created a warp set"
            );
        }
    }

    #[test]
    fn add_parses_every_option_and_state_reads_it_back() {
        let (mut world, rig) = rig();
        let out = call(
            &mut world,
            rig,
            json!({
                "op": "add", "id": "lunge", "target": "enemy", "duration": 0.5, "mode": "path_scale",
                "rotation_kind": "face_target", "translation": false, "vertical": true, "rotation": true,
                "rotation_fraction": 0.5, "offset": [0.0, 1.0, 2.0]
            }),
        )
        .expect("add");
        assert_eq!(out["result"]["id"], json!("lunge"));
        assert_eq!(out["warps"], json!(1));
        let state = warp_state(&mut world, &json!({ "entity": rig.to_bits() })).expect("state");
        let w = &state["scripted"][0];
        assert_eq!(w["mode"], json!("path_scale"));
        assert_eq!(w["rotation_kind"], json!("face_target"));
        assert_eq!(w["translation"], json!(false));
        assert_eq!(w["vertical"], json!(true));
        assert_eq!(w["rotation"], json!(true));
        assert_eq!(w["rotation_fraction"], json!(0.5));
        assert_eq!(w["anchor"], json!({ "offset": [0.0, 1.0, 2.0] }));
        assert_eq!(w["elapsed"], json!(0.0));
        // Adding the same id again is a replace or a refusal, never a second entry.
        let _ = call(
            &mut world,
            rig,
            json!({ "op": "add", "id": "lunge", "target": "enemy", "duration": 0.5 }),
        );
        let state = warp_state(&mut world, &json!({ "entity": rig.to_bits() })).expect("state");
        assert_eq!(state["scripted"].as_array().map(Vec::len), Some(1), "{state}");
    }

    #[test]
    fn bad_enum_and_type_values_carry_stable_codes() {
        let (mut world, rig) = rig();
        let base = json!({ "op": "add", "id": "w", "target": "t", "duration": 1.0 });
        let with = |key: &str, value: Value| {
            let mut v = base.clone();
            v[key] = value;
            v
        };
        let cases = [
            ("mode", json!("wobble"), "warp_bad_mode"),
            ("mode", json!(3), "bad_string"),
            ("rotation_kind", json!("spin"), "warp_bad_mode"),
            ("translation", json!("yes"), "bad_bool"),
            ("vertical", json!(1), "bad_bool"),
            ("rotation", json!([]), "bad_bool"),
            ("duration", json!("fast"), "bad_number"),
            ("rotation_fraction", json!("x"), "bad_number"),
            ("offset", json!([1.0, 2.0]), "warp_target_invalid"),
            ("offset", json!([1.0, 2.0, "z"]), "warp_target_invalid"),
            ("offset", json!("here"), "warp_target_invalid"),
        ];
        for (key, value, want) in cases {
            let err = call(&mut world, rig, with(key, value.clone())).expect_err(key);
            assert_eq!(code(&err), Some(want), "{key}={value}");
        }
        assert!(world.get::<ScriptedWarps>(rig).is_none());
    }

    #[test]
    fn unknown_ops_ids_and_missing_names_are_refused() {
        let (mut world, rig) = rig();
        let err = call(&mut world, rig, json!({})).expect_err("no op");
        assert_eq!(code(&err), Some("warp_unknown_op"));
        let err = call(&mut world, rig, json!({ "op": "explode" })).expect_err("bad op");
        assert_eq!(code(&err), Some("warp_unknown_op"));
        let err = call(
            &mut world,
            rig,
            json!({ "op": "add", "target": "t", "duration": 1.0 }),
        )
        .expect_err("no id");
        assert_eq!(code(&err), Some("warp_bad_name"));
        let err = call(
            &mut world,
            rig,
            json!({ "op": "add", "id": "", "target": "t", "duration": 1.0 }),
        )
        .expect_err("empty id");
        assert_eq!(code(&err), Some("warp_bad_name"));
        let err = call(&mut world, rig, json!({ "op": "remove", "id": "nope" })).expect_err("unknown id");
        assert_eq!(code(&err), Some("warp_unknown_id"));
        let err = call(&mut world, rig, json!({ "op": "clear_target", "name": "nope" }))
            .expect_err("unknown target");
        assert_eq!(code(&err), Some("warp_unknown_target"));
        let err = call(&mut world, rig, json!({ "op": "set_target", "name": "t" })).expect_err("no position");
        assert_eq!(code(&err), Some("warp_target_invalid"));
        let err = call(
            &mut world,
            rig,
            json!({ "op": "set_target", "name": "t", "position": [0.0, 0.0, 0.0], "facing_yaw": "north" }),
        )
        .expect_err("bad yaw");
        assert_eq!(code(&err), Some("bad_number"));
    }

    #[test]
    fn targets_and_pause_resume_round_trip() {
        let (mut world, rig) = rig();
        call(
            &mut world,
            rig,
            json!({ "op": "set_target", "name": "door", "position": [1.0, 2.0, 3.0], "facing_yaw": 0.5 }),
        )
        .expect("set");
        call(
            &mut world,
            rig,
            json!({ "op": "add", "id": "w", "target": "door", "duration": 1.0 }),
        )
        .expect("add");
        call(&mut world, rig, json!({ "op": "pause_all" })).expect("pause");
        let state = warp_state(&mut world, &json!({ "entity": rig.to_bits() })).expect("state");
        assert_eq!(state["scripted"][0]["enabled"], json!(false));
        assert_eq!(state["scripted_targets"][0]["name"], json!("door"));
        assert_eq!(state["scripted_targets"][0]["position"], json!([1.0, 2.0, 3.0]));
        let yaw = state["scripted_targets"][0]["facing_yaw"]
            .as_f64()
            .unwrap_or(f64::NAN);
        assert!((yaw - 0.5).abs() < 1.0e-5, "{yaw}");
        call(&mut world, rig, json!({ "op": "resume_all" })).expect("resume");
        let state = warp_state(&mut world, &json!({ "entity": rig.to_bits() })).expect("state");
        assert_eq!(state["scripted"][0]["enabled"], json!(true));
        call(&mut world, rig, json!({ "op": "clear_target", "name": "door" })).expect("clear");
        call(&mut world, rig, json!({ "op": "remove", "id": "w" })).expect("remove");
        let state = warp_state(&mut world, &json!({ "entity": rig.to_bits() })).expect("state");
        assert!(state["scripted"].as_array().is_some_and(Vec::is_empty));
        assert!(state["scripted_targets"].as_array().is_some_and(Vec::is_empty));
    }

    #[test]
    fn set_target_refuses_non_finite_positions() {
        let (mut world, rig) = rig();
        // JSON has no NaN literal; an out-of-f32-range number becomes infinite when narrowed.
        let err = call(
            &mut world,
            rig,
            json!({ "op": "set_target", "name": "t", "position": [1.0e300, 0.0, 0.0] }),
        )
        .expect_err("overflow");
        assert_eq!(code(&err), Some("warp_target_invalid"));
        let err = call(
            &mut world,
            rig,
            json!({ "op": "set_target", "name": "t", "position": [0.0, 0.0, 0.0], "facing_yaw": 1.0e300 }),
        )
        .expect_err("yaw overflow");
        assert_eq!(code(&err), Some("bad_number"));
    }

    #[test]
    fn root_motion_route_reads_sets_and_refuses() {
        let (mut world, rig) = rig();
        let params = |route: Option<&str>| match route {
            Some(r) => json!({ "entity": rig.to_bits(), "route": r }),
            None => json!({ "entity": rig.to_bits() }),
        };
        let read = root_motion_route(&mut world, &params(None)).expect("read");
        assert_eq!(read["before"], read["after"]);
        for (name, _) in [
            ("ignore", 0),
            ("report", 1),
            ("graph_and_montages", 2),
            ("only_montages", 3),
        ] {
            let out = root_motion_route(&mut world, &params(Some(name))).expect(name);
            assert_eq!(out["after"], json!(name));
            let back = root_motion_route(&mut world, &params(None)).expect("read back");
            assert_eq!(back["after"], json!(name));
        }
        let err = root_motion_route(&mut world, &params(Some("sideways"))).expect_err("bad route");
        assert_eq!(code(&err), Some("root_route_unknown"));
        let back = root_motion_route(&mut world, &params(None)).expect("unchanged");
        assert_eq!(
            back["after"],
            json!("only_montages"),
            "a refused set changes nothing"
        );
    }

    #[test]
    fn a_missing_or_unrigged_entity_is_refused_by_every_method() {
        let (mut world, _) = rig();
        let bare = world.spawn_empty().id();
        let gone = world.spawn_empty().id();
        world.despawn(gone);
        for (entity, want) in [(bare, "no_animator"), (gone, "entity_missing")] {
            let p = json!({ "entity": entity.to_bits(), "op": "pause_all" });
            assert_eq!(code(&scripted_warp(&mut world, &p).expect_err("rig")), Some(want));
            assert_eq!(code(&warp_state(&mut world, &p).expect_err("rig")), Some(want));
            assert_eq!(
                code(&root_motion_route(&mut world, &p).expect_err("rig")),
                Some(want)
            );
        }
    }
}
