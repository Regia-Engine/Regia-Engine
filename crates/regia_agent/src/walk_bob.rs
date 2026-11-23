// SPDX-License-Identifier: MPL-2.0
//! Agent methods for the first-person walk bob (ADR-2486, book/agents/walk-bob.md):
//! `camera.walk_bob` reads the bob settings and gait state of first-person cameras,
//! `camera.set_walk_bob` turns a camera's bob on, changes it, or removes it.
//!
//! Handlers are plain functions over `&mut World`. A world without `CameraPlugin` answers
//! `UNSUPPORTED`. Categorical mistakes carry `data.reason` (lowercase codes listed on the book page).

use regia_app::prelude::*;
use regia_framework::camera::walk_bob::{WalkBob, WalkBobState, cadence_steps_per_minute};
use regia_framework::camera::{CameraDirector, CameraMode, CameraRig};
use serde_json::{Value, json};

use crate::protocol::{self, RpcError};

/// Most cameras `camera.walk_bob` lists.
const MAX_LISTED: usize = 256;

fn unsupported() -> RpcError {
    RpcError::new(
        protocol::UNSUPPORTED,
        "F0016",
        "no camera rigs in this app",
        "add regia_framework::camera::CameraPlugin",
    )
}

/// `F0002` with `data.reason`.
fn refused(reason: &str, message: impl Into<String>, hint: &str) -> RpcError {
    let mut error = RpcError::new(protocol::INVALID_PARAMS, "F0002", message, hint);
    if let Some(Value::Object(data)) = error.data.as_mut() {
        data.insert("reason".into(), json!(reason));
    }
    error
}

fn entity_param(params: &Value, required: bool) -> Result<Option<Entity>, RpcError> {
    match params.get("entity") {
        None | Some(Value::Null) if required => Err(RpcError::invalid_params(
            "`entity` (the camera's id from camera.walk_bob) is required",
        )),
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let bits = value
                .as_u64()
                .ok_or_else(|| RpcError::invalid_params("`entity` must be an entity id"))?;
            Entity::try_from_bits(bits)
                .map(Some)
                .ok_or_else(|| RpcError::invalid_params(format!("{bits} is not an entity id")))
        }
    }
}

/// An optional finite number in `lo..=hi`.
fn ranged(params: &Value, field: &str, lo: f32, hi: f32) -> Result<Option<f32>, RpcError> {
    match params.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let n = value
                .as_f64()
                .ok_or_else(|| RpcError::invalid_params(format!("`{field}` must be a number")))?
                as f32;
            if !n.is_finite() || n < lo || n > hi {
                return Err(refused(
                    "walk_bob_out_of_range",
                    format!("`{field}` must be in {lo}..={hi}, got {n}"),
                    "see book/agents/walk-bob.md for the ranges",
                ));
            }
            Ok(Some(n))
        }
    }
}

/// The amplitude of a first-person rig (`None` when the rig is not first person).
fn first_person_amplitude(world: &World, entity: Entity) -> Option<f32> {
    match world.get::<CameraRig>(entity)?.mode {
        CameraMode::FirstPerson(mode) => Some(mode.bob),
        _ => None,
    }
}

/// The settings a camera bobs with and where they come from.
fn effective(world: &World, entity: Entity) -> (Option<WalkBob>, &'static str) {
    if let Some(bob) = world.get::<WalkBob>(entity) {
        return (Some(*bob), "component");
    }
    match first_person_amplitude(world, entity) {
        Some(a) if a.is_finite() && a > 0.0 => (Some(WalkBob::from_amplitude(a)), "mode_amplitude"),
        _ => (None, "off"),
    }
}

fn settings_json(bob: &WalkBob) -> Value {
    json!({
        "vertical_m": bob.vertical_m,
        "lateral_m": bob.lateral_m,
        "roll_deg": bob.roll_rad.to_degrees(),
        "reference_speed": bob.reference_speed,
        "max_gain": bob.max_gain,
        "leg_length_m": bob.leg_length_m,
        "fixation_m": bob.fixation_m,
        "smoothing": bob.smoothing,
        "max_slope_deg": bob.max_slope_deg,
        "teleport_m": bob.teleport_m,
    })
}

fn camera_json(world: &World, entity: Entity) -> Value {
    let (bob, source) = effective(world, entity);
    let state = world.get::<WalkBobState>(entity).map(|s| {
        let leg = bob.map_or(WalkBob::default().leg_length_m, |b| b.sanitized().leg_length_m);
        json!({
            "phase": s.phase,
            "speed": s.speed,
            "stride_m": s.stride_m,
            "cadence_spm": cadence_steps_per_minute(s.speed, leg),
            "steps": s.steps,
            "stance_foot": s.stance_foot().as_str(),
            "running": s.running,
            "offset": [s.offset.x, s.offset.y, s.offset.z],
            "pitch_deg": s.pitch.to_degrees(),
            "roll_deg": s.roll.to_degrees(),
        })
    });
    json!({
        "entity": entity.to_bits(),
        "first_person": first_person_amplitude(world, entity).is_some(),
        "source": source,
        "settings": bob.as_ref().map(settings_json),
        "state": state,
    })
}

/// `camera.walk_bob { entity? }`: one camera's walk bob, or every rig's.
pub fn camera_walk_bob(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<CameraDirector>().is_none() {
        return Err(unsupported());
    }
    if let Some(entity) = entity_param(params, false)? {
        if world.get_entity(entity).is_err() {
            return Err(RpcError::not_found(
                format!("entity {} does not exist", entity.to_bits()),
                "call camera.walk_bob without `entity` to list the rigs",
            ));
        }
        return Ok(json!({ "camera": camera_json(world, entity) }));
    }
    let mut rigs: Vec<Entity> = world
        .query_filtered::<Entity, With<CameraRig>>()
        .iter(world)
        .collect();
    rigs.sort_by_key(|e| e.to_bits());
    let listed: Vec<Value> = rigs
        .iter()
        .take(MAX_LISTED)
        .map(|&entity| camera_json(world, entity))
        .collect();
    Ok(json!({
        "count": rigs.len(),
        "truncated": rigs.len() > listed.len(),
        "cameras": listed,
    }))
}

/// `camera.set_walk_bob { entity, remove?, vertical_m?, lateral_m?, roll_deg?, reference_speed?, max_gain?,
/// leg_length_m?, fixation_m?, smoothing?, max_slope_deg?, teleport_m? }`: gives a first-person camera a walk
/// bob (starting from the one it bobs with now, else the defaults) with the given fields changed, or removes
/// the component (the mode's `bob` amplitude, when above zero, still applies).
pub fn camera_set_walk_bob(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    if world.get_resource::<CameraDirector>().is_none() {
        return Err(unsupported());
    }
    let Some(entity) = entity_param(params, true)? else {
        return Err(RpcError::invalid_params("`entity` is required"));
    };
    if world.get_entity(entity).is_err() {
        return Err(RpcError::not_found(
            format!("entity {} does not exist", entity.to_bits()),
            "call camera.walk_bob to list the rigs",
        ));
    }
    if first_person_amplitude(world, entity).is_none() {
        return Err(refused(
            "walk_bob_not_first_person",
            format!("entity {} is not a first-person camera rig", entity.to_bits()),
            "camera.set_mode { mode: \"first_person\" } first, or pick another rig",
        ));
    }
    let remove = match params.get("remove") {
        None | Some(Value::Null) => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| RpcError::invalid_params("`remove` must be a boolean"))?,
    };
    // Validate every field before changing anything.
    let vertical = ranged(params, "vertical_m", 0.0, 0.2)?;
    let lateral = ranged(params, "lateral_m", 0.0, 0.2)?;
    let roll = ranged(params, "roll_deg", 0.0, 10.0)?;
    let reference = ranged(params, "reference_speed", 0.1, 20.0)?;
    let gain = ranged(params, "max_gain", 0.0, 10.0)?;
    let leg = ranged(params, "leg_length_m", 0.2, 3.0)?;
    let fixation = ranged(params, "fixation_m", 0.0, 1000.0)?;
    let smoothing = ranged(params, "smoothing", 0.0, 100.0)?;
    let slope = ranged(params, "max_slope_deg", 0.0, 90.0)?;
    let teleport = ranged(params, "teleport_m", 0.01, 1000.0)?;
    if remove {
        world.entity_mut(entity).remove::<WalkBob>();
        return Ok(json!({ "removed": true, "camera": camera_json(world, entity) }));
    }
    let mut bob = effective(world, entity).0.unwrap_or_default();
    let set = |slot: &mut f32, value: Option<f32>| {
        if let Some(v) = value {
            *slot = v;
        }
    };
    set(&mut bob.vertical_m, vertical);
    set(&mut bob.lateral_m, lateral);
    set(&mut bob.roll_rad, roll.map(f32::to_radians));
    set(&mut bob.reference_speed, reference);
    set(&mut bob.max_gain, gain);
    set(&mut bob.leg_length_m, leg);
    set(&mut bob.fixation_m, fixation);
    set(&mut bob.smoothing, smoothing);
    set(&mut bob.max_slope_deg, slope);
    set(&mut bob.teleport_m, teleport);
    world.entity_mut(entity).insert(bob);
    Ok(json!({ "removed": false, "camera": camera_json(world, entity) }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use regia_core::math::Vec3;
    use regia_framework::camera::{CameraPlugin, ChaseCamera, FirstPerson, ThirdPerson};
    use regia_scene::Transform;

    use super::*;

    fn app() -> (App, Entity, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        let pawn = app.world_mut().spawn(Transform::default()).id();
        let eye = app
            .world_mut()
            .spawn((
                Transform::default(),
                CameraRig {
                    mode: CameraMode::FirstPerson(FirstPerson::default()),
                    yaw: 0.0,
                    pitch: 0.0,
                    ..CameraRig::default()
                },
                ChaseCamera { target: pawn },
            ))
            .id();
        let chase = app
            .world_mut()
            .spawn((
                Transform::default(),
                CameraRig {
                    mode: CameraMode::ThirdPerson(ThirdPerson::default()),
                    yaw: 0.0,
                    pitch: 0.0,
                    ..CameraRig::default()
                },
                ChaseCamera { target: pawn },
            ))
            .id();
        (app, pawn, eye, chase)
    }

    fn reason(error: &RpcError) -> Option<String> {
        error
            .data
            .as_ref()
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    #[test]
    fn a_world_without_cameras_is_unsupported() {
        let mut world = World::new();
        let error = camera_walk_bob(&mut world, &json!({})).expect_err("no plugin");
        assert_eq!(error.code, protocol::UNSUPPORTED);
        let error = camera_set_walk_bob(&mut world, &json!({ "entity": 1 })).expect_err("no plugin");
        assert_eq!(error.code, protocol::UNSUPPORTED);
    }

    #[test]
    fn list_shows_rigs_and_the_bob_is_off_by_default() {
        let (mut app, _, eye, chase) = app();
        app.update();
        let list = camera_walk_bob(app.world_mut(), &json!({})).expect("list");
        assert_eq!(list["count"], json!(2));
        let eye_row = list["cameras"]
            .as_array()
            .expect("cameras")
            .iter()
            .find(|c| c["entity"] == json!(eye.to_bits()))
            .expect("eye listed");
        assert_eq!(eye_row["source"], json!("off"));
        assert_eq!(eye_row["first_person"], json!(true));
        assert!(eye_row["settings"].is_null());
        let one = camera_walk_bob(app.world_mut(), &json!({ "entity": chase.to_bits() })).expect("one");
        assert_eq!(one["camera"]["first_person"], json!(false));
    }

    #[test]
    fn set_turns_the_bob_on_and_the_state_reports_steps() {
        let (mut app, pawn, eye, _) = app();
        let set = camera_set_walk_bob(
            app.world_mut(),
            &json!({ "entity": eye.to_bits(), "vertical_m": 0.02, "leg_length_m": 0.95 }),
        )
        .expect("set");
        assert_eq!(set["camera"]["source"], json!("component"));
        assert!((set["camera"]["settings"]["vertical_m"].as_f64().unwrap() - 0.02).abs() < 1e-6);
        for _ in 0..240 {
            if let Some(mut t) = app.world_mut().get_mut::<Transform>(pawn) {
                t.translation += Vec3::new(0.0, 0.0, -1.4 * 0.016);
            }
            app.update();
        }
        let read = camera_walk_bob(app.world_mut(), &json!({ "entity": eye.to_bits() })).expect("read");
        let state = &read["camera"]["state"];
        assert!(state["steps"].as_u64().unwrap() >= 6, "{state}");
        assert!((state["speed"].as_f64().unwrap() - 1.4).abs() < 0.05, "{state}");
        let cadence = state["cadence_spm"].as_f64().unwrap();
        assert!((90.0..140.0).contains(&cadence), "{state}");
        let removed = camera_set_walk_bob(
            app.world_mut(),
            &json!({ "entity": eye.to_bits(), "remove": true }),
        )
        .expect("remove");
        assert_eq!(removed["camera"]["source"], json!("off"));
    }

    #[test]
    fn bad_requests_are_refused_with_reasons() {
        let (mut app, _, eye, chase) = app();
        let error = camera_set_walk_bob(app.world_mut(), &json!({ "entity": chase.to_bits() }))
            .expect_err("third person");
        assert_eq!(reason(&error).as_deref(), Some("walk_bob_not_first_person"));
        let error = camera_set_walk_bob(
            app.world_mut(),
            &json!({ "entity": eye.to_bits(), "vertical_m": 3.0 }),
        )
        .expect_err("range");
        assert_eq!(reason(&error).as_deref(), Some("walk_bob_out_of_range"));
        assert!(
            app.world().get::<WalkBob>(eye).is_none(),
            "a refused call changes nothing"
        );
        let error = camera_set_walk_bob(
            app.world_mut(),
            &json!({ "entity": eye.to_bits(), "lateral_m": "x" }),
        )
        .expect_err("type");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        let error = camera_set_walk_bob(app.world_mut(), &json!({})).expect_err("entity");
        assert_eq!(error.code, protocol::INVALID_PARAMS);
        let gone = app.world_mut().spawn_empty().id();
        app.world_mut().despawn(gone);
        let error =
            camera_set_walk_bob(app.world_mut(), &json!({ "entity": gone.to_bits() })).expect_err("gone");
        assert_ne!(error.code, protocol::INVALID_PARAMS);
    }
}
