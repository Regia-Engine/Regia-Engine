// SPDX-License-Identifier: MPL-2.0
//! `physics.track_step` and `physics.track_state`: a planar skid-steer body
//! (`book/agents/vehicle-track.md`, ADR-4629).
//!
//! The call does not touch the wheeled vehicle and does not store a body. Each call carries its
//! own pose. `physics.track_step` applies `steps` integrator steps. `physics.track_state` reports
//! the pose it was given.

use regia_ecs::prelude::World;
use regia_physics::vehicle_track::{self, TrackCommand, TrackError, TrackState};
use serde_json::{Value, json};

use crate::protocol::RpcError;

/// `physics.track_step`: advance a skid-steer pose by `steps` steps of `dt` seconds.
pub(crate) fn track_step(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let dt = required_dt(params)?;
    let steps = step_count(params)?;
    let state = pose_from(params)?;
    let command = command_from(params)?;
    let stepped = vehicle_track::step_for(&state, command, dt, steps).map_err(refused)?;
    Ok(report(stepped))
}

/// `physics.track_state`: center speed, yaw rate and digest of a pose. The pose is not moved.
pub(crate) fn track_state(_world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let state = pose_from(params)?;
    // Zero steps still refuses a non-finite pose, and leaves a finite one unchanged.
    let checked = vehicle_track::step_for(&state, TrackCommand::default(), 1.0, 0).map_err(refused)?;
    Ok(report(checked))
}

fn required_dt(params: &Value) -> Result<f32, RpcError> {
    match crate::params::opt_f64(params, "dt")? {
        Some(dt) => Ok(dt as f32),
        None => Err(RpcError::invalid_params("`dt` is required")),
    }
}

fn step_count(params: &Value) -> Result<u32, RpcError> {
    match crate::params::opt_u64(params, "steps")? {
        None => Ok(1),
        Some(n) => match u32::try_from(n) {
            Ok(n) if (1..=vehicle_track::MAX_TRACK_STEPS).contains(&n) => Ok(n),
            _ => Err(RpcError::invalid_params(
                "`steps` must be an integer in 1..=10000",
            )),
        },
    }
}

fn command_from(params: &Value) -> Result<TrackCommand, RpcError> {
    Ok(TrackCommand {
        left_throttle: opt_f32(params, "left_throttle")?,
        right_throttle: opt_f32(params, "right_throttle")?,
    })
}

fn pose_from(params: &Value) -> Result<TrackState, RpcError> {
    Ok(TrackState {
        left_speed: opt_f32(params, "left_speed")?,
        right_speed: opt_f32(params, "right_speed")?,
        yaw: opt_f32(params, "yaw")?,
        x: opt_f32(params, "x")?,
        z: opt_f32(params, "z")?,
        // The counter counts the steps of this call. A previous result's `steps` is the next
        // call's step count when the caller sends the result back, which `step_count` reads.
        steps: 0,
    })
}

fn opt_f32(params: &Value, key: &str) -> Result<f32, RpcError> {
    match crate::params::opt_f64(params, key)? {
        Some(value) => Ok(value as f32),
        None => Ok(0.0),
    }
}

fn refused(error: TrackError) -> RpcError {
    let code = error.code();
    RpcError::new(
        crate::protocol::INVALID_PARAMS,
        code,
        format!("[{code}] {error}"),
        "send finite throttles, speeds and a pose, and a positive finite dt; see book/agents/vehicle-track.md",
    )
}

fn report(state: TrackState) -> Value {
    let forward = state.forward();
    json!({
        "left_speed": state.left_speed,
        "right_speed": state.right_speed,
        "longitudinal_speed": state.longitudinal_speed(),
        "yaw_rate": state.yaw_rate(),
        "yaw": state.yaw,
        "position": [state.x, 0.0_f32, state.z],
        "forward": [forward.x, forward.y, forward.z],
        "steps": state.steps,
        "digest": format!("{:016x}", state.digest()),
        "model": {
            "separation_m": vehicle_track::TRACK_SEPARATION_M,
            "max_speed_m_s": vehicle_track::MAX_TRACK_SPEED_M_S,
            "friction_accel_m_s2": vehicle_track::FRICTION_ACCEL_M_S2,
            "coast_drag_m_s2": vehicle_track::COAST_DRAG_M_S2,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1/64 s is exact in f32. 64 steps is one second and 128 steps is two.
    const DT: f64 = 1.0 / 64.0;
    const STEPS_1S: u64 = 64;
    const STEPS_2S: u64 = 128;

    fn num(value: &Value) -> f64 {
        value.as_f64().unwrap_or(f64::NAN)
    }

    fn step(params: Value) -> Value {
        let mut world = World::new();
        track_step(&mut world, &params).unwrap_or_else(|error| panic!("physics.track_step failed: {error:?}"))
    }

    #[test]
    fn equal_throttle_for_two_seconds_moves_forward_and_two_runs_match() {
        let params = json!({
            "dt": DT,
            "steps": STEPS_2S,
            "left_throttle": 1.0,
            "right_throttle": 1.0,
        });
        let once = step(params.clone());
        let twice = step(params);
        assert_eq!(
            once["digest"], twice["digest"],
            "physics.track_step digests match"
        );
        assert!(num(&once["yaw"]).abs() < 1e-4, "yaw {}", num(&once["yaw"]));
        let position = once["position"]
            .as_array()
            .unwrap_or_else(|| panic!("physics.track_step position"));
        let z = num(&position[2]);
        assert!(z < -1.0, "moved forward along -Z, z {z}");
        assert!(num(&position[0]).abs() < 1e-4, "no side slip");
        let time = DT * (STEPS_2S as f64);
        let expected = 0.5 * f64::from(vehicle_track::FRICTION_ACCEL_M_S2) * time * time;
        assert!(
            ((-z) - expected).abs() <= 0.01 * expected,
            "forward {} m, closed form {expected}",
            -z
        );
        let library = vehicle_track::step_for(
            &TrackState::default(),
            TrackCommand {
                left_throttle: 1.0,
                right_throttle: 1.0,
            },
            DT as f32,
            STEPS_2S as u32,
        )
        .unwrap_or_else(|error| panic!("physics.track_step library: {error}"));
        assert_eq!(once["digest"], format!("{:016x}", library.digest()));
    }

    #[test]
    fn left_track_faster_yaws_right_within_one_percent_of_the_closed_form() {
        let once = step(json!({
            "dt": DT,
            "steps": STEPS_1S,
            "left_throttle": 1.0,
            "right_throttle": 0.0,
        }));
        let yaw = num(&once["yaw"]);
        let time = DT * (STEPS_1S as f64);
        let expected = (0.0 - f64::from(vehicle_track::FRICTION_ACCEL_M_S2)) * time * time
            / (2.0 * f64::from(vehicle_track::TRACK_SEPARATION_M));
        assert!(
            yaw < 0.0,
            "physics.track_step left track faster yaws right, yaw {yaw}"
        );
        assert!(
            (yaw - expected).abs() <= 0.01 * expected.abs(),
            "yaw {yaw} is outside 1 percent of the closed form {expected}"
        );
        let forward = once["forward"]
            .as_array()
            .unwrap_or_else(|| panic!("physics.track_step forward"));
        assert!(num(&forward[0]) > 0.0, "the nose turned toward +X");
    }

    #[test]
    fn opposite_throttles_spin_in_place() {
        let once = step(json!({
            "dt": DT,
            "steps": STEPS_1S,
            "left_throttle": 1.0,
            "right_throttle": -1.0,
        }));
        assert!(
            num(&once["longitudinal_speed"]).abs() < 1e-3,
            "physics.track_step center speed {}",
            num(&once["longitudinal_speed"])
        );
        let position = once["position"]
            .as_array()
            .unwrap_or_else(|| panic!("physics.track_step position"));
        assert!(num(&position[0]).abs() < 1e-3 && num(&position[2]).abs() < 1e-3);
    }

    #[test]
    fn a_throttle_that_does_not_fit_in_f32_is_track_bad_input() {
        let mut world = World::new();
        let error = track_step(
            &mut world,
            &json!({ "dt": DT, "left_throttle": 1e308, "right_throttle": 0.0 }),
        )
        .expect_err("physics.track_step");
        let data = error
            .data
            .unwrap_or_else(|| panic!("physics.track_step missing data"));
        assert_eq!(data["code"], "track_bad_input");
        assert!(data["hint"].as_str().is_some());
    }

    #[test]
    fn track_state_reports_the_average_and_the_difference() {
        let mut world = World::new();
        let state = track_state(&mut world, &json!({ "left_speed": 4.0, "right_speed": 0.0 }))
            .unwrap_or_else(|error| panic!("physics.track_state failed: {error:?}"));
        assert!((num(&state["longitudinal_speed"]) - 2.0).abs() < 1e-6);
        assert!((num(&state["yaw_rate"]) - (-2.0)).abs() < 1e-6);
        assert_eq!(state["steps"], 0);
        assert_eq!(state["model"]["separation_m"], 2.0);
        let again = track_state(&mut world, &json!({ "left_speed": 4.0, "right_speed": 0.0 }))
            .unwrap_or_else(|error| panic!("physics.track_state failed: {error:?}"));
        assert_eq!(state["digest"], again["digest"]);
    }

    #[test]
    fn a_missing_dt_is_a_parameter_error() {
        let mut world = World::new();
        let error = track_step(&mut world, &json!({ "left_throttle": 1.0 })).expect_err("physics.track_step");
        let data = error
            .data
            .unwrap_or_else(|| panic!("physics.track_step missing data"));
        assert_eq!(data["code"], "F0002");
    }
}
