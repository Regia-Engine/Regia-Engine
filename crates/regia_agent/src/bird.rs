// SPDX-License-Identifier: MPL-2.0
//! `bird.*`: the startled bird (ADR-1502, `book/agents/camera-body.md`).
//!
//! - `bird.spawn` queues a bird from a spec.
//! - `bird.list` reports every bird (phase, position, trigger).
//! - `bird.remove` removes one.
//! - `bird.trigger` derives a spec's startle time from the loaded camera path without
//!   spawning it.
//! - `bird.place_on_path` gives the feet position for a bird the camera passes at a time.

use regia_app::prelude::*;
use regia_core::math::wake::WakeShape;
use regia_framework::bird::{BirdFlock, BirdSim, BirdSpec, find_trigger, place_on_path};
use regia_framework::camera_body::CameraBody;
use regia_framework::shots::{ShotPlayer, ShotSequence};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// Most birds in one world.
const MAX_BIRDS: usize = 256;

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

fn spec_param(params: &Value) -> Result<BirdSpec, RpcError> {
    let spec = params.get("spec").filter(|v| v.is_object()).ok_or_else(|| {
        RpcError::invalid_params("`spec` is required: a bird spec object (agents/camera-body)")
    })?;
    BirdSpec::from_json(&spec.to_string()).map_err(|e| RpcError::invalid_params(e.to_string()))
}

fn name_param(params: &Value) -> Result<String, RpcError> {
    crate::params::opt_str(params, "name")?
        .filter(|n| !n.trim().is_empty())
        .map(str::to_string)
        .ok_or_else(|| RpcError::invalid_params("`name` must be a bird's name"))
}

fn wake_shape(world: &World) -> WakeShape {
    world
        .get_resource::<CameraBody>()
        .map_or_else(WakeShape::default, |b| b.spec().wake)
}

fn sequence(world: &World) -> Option<ShotSequence> {
    world.get_resource::<ShotPlayer>()?.sequence().cloned()
}

fn no_flock() -> RpcError {
    RpcError::not_found(
        "no bird flock",
        "the host adds regia_framework::bird::BirdPlugin (the showcase does)",
    )
}

fn timing(spec: &BirdSpec, seq: Option<&ShotSequence>, wake: &WakeShape) -> Result<Value, RpcError> {
    let trigger = seq.and_then(|s| find_trigger(spec, s, wake));
    let sim = BirdSim::new(spec.clone(), trigger);
    Ok(json!({
        "trigger": serde_json::to_value(trigger).map_err(|e| RpcError::internal(e.to_string()))?,
        "flinch_s": sim.flinch_time(),
        "takeoff_s": sim.takeoff_time(),
        "path_loaded": seq.is_some(),
    }))
}

/// `bird.spawn`.
pub(crate) fn spawn(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let spec = spec_param(params)?;
    let wake = wake_shape(world);
    let seq = sequence(world);
    let timing = timing(&spec, seq.as_ref(), &wake)?;
    let mut flock = world.get_resource_mut::<BirdFlock>().ok_or_else(no_flock)?;
    let replacing = flock.reports().iter().any(|r| r.name == spec.name);
    if !replacing && flock.reports().len() + flock.pending.len() >= MAX_BIRDS {
        return Err(RpcError::invalid_params(format!("at most {MAX_BIRDS} birds")));
    }
    let name = spec.name.clone();
    flock.spawn(spec);
    Ok(json!({ "name": name, "replaced": replacing, "timing": timing }))
}

/// `bird.list`.
pub(crate) fn list(world: &mut World, _params: &Value) -> Result<Value, RpcError> {
    let flock = world.get_resource::<BirdFlock>().ok_or_else(no_flock)?;
    Ok(json!({
        "birds": serde_json::to_value(flock.reports()).map_err(|e| RpcError::internal(e.to_string()))?,
        "pending": flock.pending.iter().map(|s| s.name.clone()).collect::<Vec<_>>(),
    }))
}

/// `bird.remove`.
pub(crate) fn remove(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let name = name_param(params)?;
    let mut flock = world.get_resource_mut::<BirdFlock>().ok_or_else(no_flock)?;
    let known =
        flock.reports().iter().any(|r| r.name == name) || flock.pending.iter().any(|s| s.name == name);
    if !known {
        return Err(RpcError::not_found(
            format!("no bird named `{name}`"),
            "bird.list names them",
        ));
    }
    flock.pending.retain(|s| s.name != name);
    flock.remove.push(name.clone());
    Ok(json!({ "removed": name }))
}

/// `bird.trigger`.
pub(crate) fn trigger(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let wake = wake_shape(world);
    let seq = sequence(world);
    let spec = if params.get("spec").is_some_and(Value::is_object) {
        spec_param(params)?
    } else {
        let name = name_param(params)?;
        let flock = world.get_resource::<BirdFlock>().ok_or_else(no_flock)?;
        let report =
            flock.reports().iter().find(|r| r.name == name).ok_or_else(|| {
                RpcError::not_found(format!("no bird named `{name}`"), "bird.list names them")
            })?;
        // The live bird already holds its trigger on the loaded path.
        return Ok(json!({
            "trigger": serde_json::to_value(report.trigger).map_err(|e| RpcError::internal(e.to_string()))?,
            "flinch_s": report.flinch_s,
            "takeoff_s": report.takeoff_s,
            "path_loaded": seq.is_some(),
        }));
    };
    timing(&spec, seq.as_ref(), &wake)
}

/// `bird.place_on_path`.
pub(crate) fn place(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let time = number(params, "time_s")?.ok_or_else(|| RpcError::invalid_params("`time_s` is required"))?;
    let right = number(params, "right_m")?.unwrap_or(0.0) as f32;
    let up = number(params, "up_m")?.unwrap_or(0.0) as f32;
    let ahead = number(params, "ahead_m")?.unwrap_or(0.0) as f32;
    let seq = sequence(world).ok_or_else(|| {
        RpcError::not_found(
            "no camera path loaded",
            "load one with camera.set_path or render.sequence {\"action\": \"load\"} first",
        )
    })?;
    let position = place_on_path(&seq, time, right, up, ahead).ok_or_else(|| {
        RpcError::not_found(
            "the path has no pose at that time",
            "pick a time inside the sequence",
        )
    })?;
    Ok(json!({ "position": position, "time_s": time }))
}

#[cfg(test)]
mod tests {
    use regia_framework::camera_body::CameraBody;
    use regia_framework::shots::{CameraPath, LookAt, PathInterp, PathKey, Shot, ShotCamera, Track};
    use serde_json::json;

    use super::*;

    /// A world with a 40 m/s pass along +X, 50 m up, passing x = 0 at t = 5 s, 5 m to +Z.
    fn world() -> World {
        let mut world = World::new();
        let mut player = ShotPlayer::default();
        player.load(ShotSequence {
            shots: vec![Shot {
                name: "pass".into(),
                duration: 10.0,
                camera: ShotCamera {
                    path: CameraPath {
                        interp: PathInterp::Linear,
                        keys: vec![
                            PathKey {
                                time: 0.0,
                                position: [-200.0, 50.0, 5.0],
                            },
                            PathKey {
                                time: 10.0,
                                position: [200.0, 50.0, 5.0],
                            },
                        ],
                        look_at: LookAt::Heading,
                    },
                    fov: Track::default(),
                    roll: Track::default(),
                    dof: None,
                    rotation: Vec::new(),
                },
                fade_in: 0.0,
                fade_out: 0.0,
                fade_color: [0.0; 3],
            }],
            ..ShotSequence::default()
        });
        world.insert_resource(player);
        world.insert_resource(BirdFlock::default());
        world.insert_resource(CameraBody::default());
        world
    }

    fn code(err: &RpcError) -> Option<&str> {
        err.data.as_ref()?.get("code")?.as_str()
    }

    #[test]
    fn trigger_and_placement_follow_the_loaded_path() {
        let mut w = world();
        let spot = place(&mut w, &json!({ "time_s": 5.0, "right_m": -5.0 })).expect("placed");
        // Travelling +X the right is +Z: 5 m to the left of the path at z = 5 is z = 0.
        let p: Vec<f64> = serde_json::from_value(spot["position"].clone()).expect("position");
        assert!((p[0]).abs() < 1.0e-3 && (p[2]).abs() < 1.0e-3, "{p:?}");
        let r = trigger(
            &mut w,
            &json!({ "spec": { "name": "crow", "position": [0.0, 49.86, 0.0] } }),
        )
        .expect("timing");
        let t = r["trigger"]["time_s"].as_f64().expect("a trigger");
        assert!(t < 5.0 && t > 1.0, "{t}");
        assert_eq!(r["trigger"]["cause"], "closing");
        assert!(r["takeoff_s"].as_f64().expect("takeoff") > t);
    }

    #[test]
    fn spawn_list_remove_and_errors() {
        let mut w = world();
        let r = spawn(
            &mut w,
            &json!({ "spec": { "name": "crow", "position": [0.0, 49.86, 0.0] } }),
        )
        .expect("spawned");
        assert_eq!(r["name"], "crow");
        let listed = list(&mut w, &json!({})).expect("list");
        assert_eq!(listed["pending"], json!(["crow"]));
        remove(&mut w, &json!({ "name": "crow" })).expect("removed");
        let missing = remove(&mut w, &json!({ "name": "nope" })).expect_err("unknown bird");
        assert_eq!(code(&missing), Some("F0003"));
        let bad = spawn(&mut w, &json!({ "spec": { "name": "x", "wings": 3 } })).expect_err("bad spec");
        assert_eq!(code(&bad), Some("F0002"));
        let mut empty = World::new();
        assert_eq!(
            code(&list(&mut empty, &json!({})).expect_err("no flock")),
            Some("F0003")
        );
        assert_eq!(
            code(&place(&mut empty, &json!({ "time_s": 1.0 })).expect_err("no path")),
            Some("F0003")
        );
    }

    #[test]
    fn camera_body_calls_answer_on_the_loaded_path() {
        let mut w = world();
        let set =
            crate::camera_body::set(&mut w, &json!({ "spec": { "wake": { "gain": 0.2 } } })).expect("set");
        assert_eq!(set["spec"]["wake"]["gain"], json!(0.2f32));
        let wake = crate::camera_body::wake(&mut w, &json!({ "points": [[-2.0, 50.0, 7.0]], "time": 5.0 }))
            .expect("wake");
        assert!(
            wake["samples"][0]["gust_m_s"].as_f64().expect("gust") > 0.0,
            "{wake}"
        );
        let near = crate::camera_body::proximity_call(
            &mut w,
            &json!({ "radius_m": 10.0, "targets": [{ "id": "post", "position": [0.0, 50.0, 0.0] }] }),
        )
        .expect("proximity");
        assert_eq!(near["hits"][0]["id"], "post");
        assert!((near["hits"][0]["closest_s"].as_f64().expect("time") - 5.0).abs() < 1.0e-3);
        // No terrain or scatter in this world: nothing to hit.
        let clear = crate::camera_body::clearance(&mut w, &json!({})).expect("clearance");
        assert_eq!(clear["clear"], json!(true));
        let lift = crate::camera_body::lift_path(&mut w, &json!({ "dry_run": true })).expect("lift");
        assert_eq!(lift["applied"], json!(false));
        let mut empty = World::new();
        assert_eq!(
            code(&crate::camera_body::proximity_call(&mut empty, &json!({})).expect_err("no path")),
            Some("F0003")
        );
        assert_eq!(
            code(&crate::camera_body::get(&mut empty, &json!({})).expect_err("no body")),
            Some("F0003")
        );
    }
}
