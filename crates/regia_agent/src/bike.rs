// SPDX-License-Identifier: MPL-2.0
//! Agent-protocol surface for the two-wheel balance model (ADR-4017, `book/agents/bike.md`).
//!
//! - `physics.bike_step` advances the one bike held on the world by one fixed step. The caller
//!   passes the steer, speed, drive and the two wheel-ray lengths. It does not touch the car solver.
//! - `physics.bike_state` reads that bike. Before the first successful step the read reports the
//!   upright default and `present: false`, and writes nothing.
//!
//! A non-finite steer (or any other field [`regia_physics::vehicle_bike::bike_step`] refuses) answers
//! `bike_bad_input` and leaves the stored state as it was.

use regia_app::prelude::*;
use regia_physics::vehicle_bike::{
    BikeConfig, BikeError, BikeInput, BikeState, FIXED_STEP_S, bike_step as step_bike,
};
use serde_json::{Value, json};

use crate::params::{opt_bool, opt_f64};
use crate::protocol::{INVALID_PARAMS, RpcError};

/// The one bike the agent methods step. Absent until the first accepted `physics.bike_step`.
#[derive(Resource, Clone, Copy, Debug, Default)]
struct BikeSession {
    config: BikeConfig,
    state: BikeState,
}

fn bike_error(error: BikeError) -> RpcError {
    RpcError::new(
        INVALID_PARAMS,
        error.code(),
        format!("[{}] {error}", error.code()),
        "pass finite steer, speed, drive and dt (dt in (0, 0.25]), and ray lengths of zero or more; \
         a ray longer than 0.7 m is not in contact; see book/agents/bike.md",
    )
}

fn finite_f32(name: &str, value: f64) -> Result<f32, RpcError> {
    let number = value as f32;
    if number.is_finite() {
        Ok(number)
    } else {
        Err(bike_error(BikeError::BadInput).with_field(name))
    }
}

trait NameTheField {
    fn with_field(self, name: &str) -> Self;
}

impl NameTheField for RpcError {
    fn with_field(mut self, name: &str) -> Self {
        if let Some(Value::Object(data)) = self.data.as_mut() {
            data.insert("param".into(), Value::String(name.into()));
        }
        self
    }
}

fn number(params: &Value, key: &str, default: f32) -> Result<f32, RpcError> {
    match opt_f64(params, key)? {
        None => Ok(default),
        Some(value) => finite_f32(key, value),
    }
}

/// A wheel-ray length. Any positive length past f32 range is a ray that missed: the physics
/// reads `+∞` as "not in contact" (book/agents/bike.md: a longer length is not in contact), and
/// JSON has no infinity — refusing an out-of-range finite value made a miss inexpressible. A
/// negative length is still `bike_bad_input`, like any other field.
fn ray_length(params: &Value, key: &str, default: f32) -> Result<f32, RpcError> {
    match opt_f64(params, key)? {
        None => Ok(default),
        Some(value) if value < 0.0 => Err(bike_error(BikeError::BadInput).with_field(key)),
        Some(value) => {
            let number = value as f32;
            if number.is_finite() {
                Ok(number)
            } else {
                Ok(f32::INFINITY)
            }
        }
    }
}

fn state_json(state: BikeState, present: bool) -> Value {
    json!({
        "present": present,
        "lean": state.lean,
        "steer": state.steer,
        "speed": state.speed,
        "drive": state.drive,
        "front_contact": state.front_contact,
        "rear_contact": state.rear_contact,
        "fallen": state.fallen,
        "digest": format!("{:016x}", state.digest()),
    })
}

/// `physics.bike_state`: the stored bike, or the upright default when none has been stepped.
pub fn bike_state(world: &World, _params: &Value) -> Result<Value, RpcError> {
    let Some(session) = world.get_resource::<BikeSession>().copied() else {
        return Ok(state_json(BikeState::default(), false));
    };
    Ok(state_json(session.state, true))
}

/// `physics.bike_step`: one step of the balance model. `reset: true` restores the upright bike
/// first, then applies the step.
pub fn bike_step(world: &mut World, params: &Value) -> Result<Value, RpcError> {
    let reset = opt_bool(params, "reset")?.unwrap_or(false);
    let input = BikeInput {
        dt: number(params, "dt", FIXED_STEP_S)?,
        steer: number(params, "steer", 0.0)?,
        speed: number(params, "speed", 0.0)?,
        drive: number(params, "drive", 0.0)?,
        front_ray: ray_length(params, "front_ray", 0.3)?,
        rear_ray: ray_length(params, "rear_ray", 0.3)?,
    };
    let mut session = if reset {
        BikeSession::default()
    } else {
        world.get_resource::<BikeSession>().copied().unwrap_or_default()
    };
    step_bike(&mut session.state, &session.config, &input).map_err(bike_error)?;
    let body = state_json(session.state, true);
    world.insert_resource(session);
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins);
        app
    }

    #[test]
    fn physics_bike_step_refuses_a_non_finite_steer_and_keeps_state() {
        let mut app = app();
        let seeded = bike_step(
            app.world_mut(),
            &json!({ "steer": 0.0, "speed": 8.0, "drive": 0.4, "dt": 1.0 / 60.0 }),
        )
        .expect("physics.bike_step");
        assert_eq!(seeded["present"], true);
        assert!((seeded["drive"].as_f64().unwrap_or(0.0) - 0.4).abs() < 1.0e-6);

        let error = bike_step(app.world_mut(), &json!({ "steer": 1.0e30 })).expect_err("refused");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("bike_bad_input")
        );
        let after = bike_state(app.world_mut(), &json!({})).expect("physics.bike_state");
        assert_eq!(after, seeded);
    }

    /// corr-low: the book says a ray longer than the 0.7 m reach is "not in contact", but any
    /// length past f32 range (JSON has no infinity; 1e39 overflows) was refused as
    /// `bike_bad_input`, so a caller could not express a miss for a far floor.
    #[test]
    fn physics_bike_step_reads_a_ray_past_f32_range_as_not_in_contact() {
        let mut app = app();
        let stepped = bike_step(
            app.world_mut(),
            &json!({ "speed": 8.0, "front_ray": 1.0e39, "rear_ray": 0.3 }),
        )
        .expect("a ray past every reach misses");
        assert_eq!(stepped["front_contact"], false);
        assert_eq!(stepped["rear_contact"], true);

        let error = bike_step(app.world_mut(), &json!({ "front_ray": -1.0e39 }))
            .expect_err("a negative ray is still refused");
        assert_eq!(
            error.data.as_ref().and_then(|d| d["code"].as_str()),
            Some("bike_bad_input")
        );
    }

    #[test]
    fn physics_bike_state_is_absent_until_a_step() {
        let mut app = app();
        let idle = bike_state(app.world_mut(), &json!({})).expect("physics.bike_state");
        assert_eq!(idle["present"], false);
        assert_eq!(idle["lean"], 0.0);
        assert!(app.world().get_resource::<BikeSession>().is_none());
    }

    #[test]
    fn dispatched_fixed_steps_replay_and_invalid_reset_is_atomic() {
        fn call(app: &mut App, method: &str, params: Value) -> Result<Value, RpcError> {
            crate::dispatch::call_in_process(app.world_mut(), method, params)
                .expect("bike methods answer synchronously")
        }

        let mut app = app();
        let mut permissions = crate::safety::AgentPermissions::default();
        permissions.config.rate_limit_per_second = 10_000;
        app.insert_resource(permissions);
        let idle = call(&mut app, "physics.bike_state", json!({})).expect("read default");
        assert_eq!(idle["present"], false);

        let mut finals = Vec::new();
        for _ in 0..2 {
            for step in 0..300 {
                call(
                    &mut app,
                    "physics.bike_step",
                    json!({
                        "reset": step == 0,
                        "steer": 0.08,
                        "speed": 8.0,
                        "drive": 0.5,
                        "front_ray": 0.7,
                        "rear_ray": 0.71
                    }),
                )
                .expect("fixed step through dispatch");
            }
            finals.push(call(&mut app, "physics.bike_state", json!({})).expect("read run"));
        }
        assert_eq!(finals[0], finals[1]);
        assert_eq!(finals[0]["front_contact"], true);
        assert_eq!(finals[0]["rear_contact"], false);
        let expected = regia_physics::vehicle_bike::steady_lean(8.0, 0.08, &BikeConfig::default());
        let actual = finals[0]["lean"].as_f64().expect("lean number") as f32;
        assert!((actual - expected).abs() <= 0.1 * expected);

        for params in [
            json!({ "reset": true, "steer": 1.0e100 }),
            json!({ "reset": true, "rear_ray": -0.1 }),
            json!({ "reset": true, "dt": 0.0 }),
        ] {
            let error = call(&mut app, "physics.bike_step", params).expect_err("bad input");
            assert_eq!(
                error.data.as_ref().and_then(|d| d["code"].as_str()),
                Some("bike_bad_input")
            );
            assert_eq!(
                call(&mut app, "physics.bike_state", json!({})).expect("unchanged"),
                finals[0]
            );
        }
    }
}
