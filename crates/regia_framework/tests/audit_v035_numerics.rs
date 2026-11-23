// SPDX-License-Identifier: MPL-2.0
//! audit-v035-numerics (fix-audit-wave12, ADR-2395): character movement and camera driver fed
//! non-finite or out-of-range numbers. Each test failed on the v0.2.14 candidate (34e3b4e5f).

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3};
use regia_core::math::{det_atan2, det_sin_cos, det_yaw_of};
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterState, FlatPlaneQuery, RotationMode,
    step_character,
};
use regia_scene::Transform;

const DT: f32 = 1.0 / 62.5;
/// Resting capsule-centre height of the default capsule on the flat plane.
const REST_Y: f32 = 1.2;

fn spawn_character(app: &mut App, movement: CharacterMovement) -> regia_ecs::prelude::Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, REST_Y, 0.0)),
            Character,
            movement,
            CharacterInput::default(),
            CharacterState::default(),
        ))
        .id()
}

fn walk() -> CharacterInput {
    CharacterInput {
        move_dir: Vec3::new(1.0, 0.0, 0.0),
        ..CharacterInput::default()
    }
}

/// H4: a negative or NaN `turn_rate` crossed (or poisoned) the bounds of `f32::clamp` in the
/// orient-to-movement turn and panicked inside `FixedUpdate`.
#[test]
fn a_negative_or_nan_turn_rate_does_not_panic() {
    for turn_rate in [-1.0, f32::NAN, f32::NEG_INFINITY] {
        let mut app = App::new();
        let e = spawn_character(
            &mut app,
            CharacterMovement {
                turn_rate,
                rotation_mode: RotationMode::OrientToMovement,
                ..CharacterMovement::default()
            },
        );
        for _ in 0..5 {
            step_character(app.world_mut(), &FlatPlaneQuery, e, &walk(), DT);
        }
        let t = *app.world().get::<Transform>(e).expect("transform");
        assert!(
            t.translation.is_finite() && t.rotation.is_finite(),
            "turn_rate {turn_rate}: {t:?}"
        );
    }
}

/// H4 (second half): a negative `dt` from a direct caller made `max_turn` negative as well.
#[test]
fn a_negative_dt_does_not_panic_and_moves_nothing() {
    let mut app = App::new();
    let e = spawn_character(&mut app, CharacterMovement::default());
    let before = *app.world().get::<Transform>(e).expect("transform");
    step_character(app.world_mut(), &FlatPlaneQuery, e, &walk(), -DT);
    step_character(app.world_mut(), &FlatPlaneQuery, e, &walk(), 0.0);
    step_character(app.world_mut(), &FlatPlaneQuery, e, &walk(), f32::NAN);
    let after = *app.world().get::<Transform>(e).expect("transform");
    assert_eq!(
        before, after,
        "a step that is not finite and positive moves nothing"
    );
    let v = app.world().get::<CharacterState>(e).expect("state").velocity;
    assert!(v.is_finite(), "{v:?}");
}

/// H5: one step with a NaN acceleration latched a NaN velocity in `CharacterState` and a NaN
/// translation in the Transform; restoring the parameter never recovered them.
#[test]
fn one_nan_parameter_does_not_latch_a_nan_velocity_or_transform() {
    for field in [
        "acceleration",
        "air_control",
        "gravity_scale",
        "jump_velocity",
        "max_walk_speed",
    ] {
        let mut app = App::new();
        let e = spawn_character(&mut app, CharacterMovement::default());
        let set = |app: &mut App, value: f32| {
            let mut m = app.world_mut().get_mut::<CharacterMovement>(e).expect("movement");
            match field {
                "acceleration" => m.acceleration = value,
                "air_control" => m.air_control = value,
                "gravity_scale" => m.gravity_scale = value,
                "jump_velocity" => m.jump_velocity = value,
                _ => m.max_walk_speed = value,
            }
        };
        let defaults = CharacterMovement::default();
        let good = match field {
            "acceleration" => defaults.acceleration,
            "air_control" => defaults.air_control,
            "gravity_scale" => defaults.gravity_scale,
            "jump_velocity" => defaults.jump_velocity,
            _ => defaults.max_walk_speed,
        };
        let jump = CharacterInput { jump: true, ..walk() };
        set(&mut app, f32::NAN);
        step_character(app.world_mut(), &FlatPlaneQuery, e, &jump, DT);
        step_character(app.world_mut(), &FlatPlaneQuery, e, &walk(), DT);
        set(&mut app, good);
        for _ in 0..30 {
            step_character(app.world_mut(), &FlatPlaneQuery, e, &walk(), DT);
        }
        let v = app.world().get::<CharacterState>(e).expect("state").velocity;
        let t = app.world().get::<Transform>(e).expect("transform").translation;
        assert!(
            v.is_finite() && t.is_finite(),
            "{field}: velocity {v:?}, translation {t:?}"
        );
        assert!(
            t.x > 0.1,
            "{field}: walks again once the parameter is fixed: {t:?}"
        );
    }
}

/// H5: a stored NaN velocity (written by a script or a bad snapshot) restarts from rest.
#[test]
fn a_nan_stored_velocity_restarts_from_rest() {
    let mut app = App::new();
    let e = spawn_character(&mut app, CharacterMovement::default());
    app.world_mut()
        .get_mut::<CharacterState>(e)
        .expect("state")
        .velocity = Vec3::NAN;
    for _ in 0..10 {
        step_character(app.world_mut(), &FlatPlaneQuery, e, &walk(), DT);
    }
    let v = app.world().get::<CharacterState>(e).expect("state").velocity;
    let t = app.world().get::<Transform>(e).expect("transform");
    assert!(
        v.is_finite() && t.translation.is_finite() && t.rotation.is_finite(),
        "{v:?} {t:?}"
    );
}

/// framework3-audit (Medium): the OrientToMovement facing must integrate on the deterministic
/// kernel (`det_atan2` / `det_sin_cos`, the same f64-polynomial style as `det_cos`), not on the
/// platform libm `atan2` / `sin` / `cos` and not through glam's libm `to_euler`: a lockstep peer
/// or a replay on another libm stores different `NetCharacterBase.stepped_yaw` bits. The step's
/// rotation must be bit-identical to a yaw traced with the kernel alone.
#[test]
fn orient_to_movement_yaw_matches_deterministic_kernel() {
    // Yaw-only quaternion from the kernel (mirrors the character step's construction).
    let yaw_rotation = |yaw: f32| {
        let (sin, cos) = det_sin_cos(yaw * 0.5);
        Quat::from_xyzw(0.0, sin, 0.0, cos)
    };
    // The yaw read back from such a rotation, kernel only (no `to_euler`): the same YXZ
    // extraction the step's `rotation_yaw` uses (fixrev-11-k restored the pre-fix YXZ
    // semantics; the twist mirror no longer matches the step bit-for-bit).
    let rotation_yaw = |q: Quat| det_yaw_of(q);
    let angle_diff = |to: f32, from: f32| {
        let mut d = to - from;
        while d > std::f32::consts::PI {
            d -= 2.0 * std::f32::consts::PI;
        }
        while d < -std::f32::consts::PI {
            d += 2.0 * std::f32::consts::PI;
        }
        d
    };

    let dt = 1.0f32 / 60.0;
    let max_turn = CharacterMovement::default().turn_rate * dt;
    let move_dir = Vec3::new(1.0, 0.0, 1.0).normalize();
    let target_yaw = det_atan2(-move_dir.x, -move_dir.z);

    let mut app = App::new();
    let e = spawn_character(
        &mut app,
        CharacterMovement {
            rotation_mode: RotationMode::OrientToMovement,
            ..CharacterMovement::default()
        },
    );
    let input = CharacterInput {
        move_dir: Vec3::new(1.0, 0.0, 1.0),
        ..CharacterInput::default()
    };
    let mut rotation = Quat::IDENTITY;
    for step in 0..120 {
        step_character(app.world_mut(), &FlatPlaneQuery, e, &input, dt);
        // The kernel-only trace of the same recurrence the step runs.
        let current_yaw = rotation_yaw(rotation);
        let diff = angle_diff(target_yaw, current_yaw);
        let turn = diff.clamp(-max_turn, max_turn);
        rotation = yaw_rotation(current_yaw + turn);
        let written = app.world().get::<Transform>(e).expect("transform").rotation;
        assert_eq!(
            written, rotation,
            "step {step}: the facing is the deterministic kernel's, not the platform libm's"
        );
    }
    assert_eq!(
        rotation_yaw(rotation),
        target_yaw,
        "one 720 deg/s step is 12 rad: 120 steps snap the facing onto the target"
    );
}
