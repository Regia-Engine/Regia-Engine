// SPDX-License-Identifier: MPL-2.0
//! Character bridge for motion-matching trajectory prediction (ADR-2858): turns a framework
//! character's [`CharacterInput`] into the desired velocity (and, for strafing, the aim yaw) of
//! its [`TrajectoryPredictor`], so a game drives motion matching with the same input it gives
//! the character controller instead of writing a trajectory by hand.
//!
//! Add [`MotionMatchCharacter`] next to `Character` + `CharacterInput` + `CharacterMovement` +
//! `TrajectoryPredictor` + `MotionMatching`, and install [`MotionMatchCharacterPlugin`] (after the
//! motion-matching plugin). Every frame the desired velocity is `move_dir · max_walk_speed`
//! (times the sprint multiplier while sprinting), on the ground plane; with
//! [`MotionMatchCharacter::aim_yaw`] set the predictor strafes (faces the aim, moves with the
//! input), otherwise it faces where it moves. With [`MotionMatchCharacter::follow_transform`]
//! the predictor's simulated ground position is reset to the character's transform each frame,
//! so the predicted path starts where the character really is.

// Source: original design (glue between the character controller's input and the spring
// predictor of `regia_animation::motion_matching::predict`). Original code; nothing ported.

use regia_animation::motion_matching::{FacingMode, TrajectoryPredictor, run_trajectory_predictors};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;

use crate::character::{CharacterInput, CharacterMovement};

/// Drives an entity's [`TrajectoryPredictor`] from its character input.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct MotionMatchCharacter {
    /// Strafe: face this yaw (radians, 0 = +Z, positive toward +X) whatever the movement
    /// direction. `None` faces the movement direction.
    pub aim_yaw: Option<f32>,
    /// Reset the predictor's ground position to the entity's `Transform` every frame (default
    /// true).
    pub follow_transform: bool,
}

impl Default for MotionMatchCharacter {
    fn default() -> Self {
        Self {
            aim_yaw: None,
            follow_transform: true,
        }
    }
}

/// The desired ground velocity of a character input: `move_dir` (clamped to length 1, Y
/// dropped) times the walk speed, times the sprint multiplier while sprinting. Non-finite input
/// is no movement.
pub fn desired_velocity(input: &CharacterInput, movement: &CharacterMovement) -> Vec3 {
    let flat = Vec3::new(input.move_dir.x, 0.0, input.move_dir.z);
    if !flat.is_finite() {
        return Vec3::ZERO;
    }
    let dir = if flat.length_squared() > 1.0 {
        flat.normalize()
    } else {
        flat
    };
    let movement = movement.sanitized();
    let speed = movement.max_walk_speed
        * if input.sprint {
            movement.sprint_speed_multiplier
        } else {
            1.0
        };
    dir * speed
}

/// Write each bridged character's input into its predictor (before the predictors advance).
#[tracing::instrument(target = "regia_framework", skip_all)]
pub fn drive_predictors_from_characters(
    mut characters: Query<(
        &MotionMatchCharacter,
        &CharacterInput,
        &CharacterMovement,
        Option<&Transform>,
        &mut TrajectoryPredictor,
    )>,
) {
    for (bridge, input, movement, transform, mut predictor) in &mut characters {
        predictor.desired_velocity = desired_velocity(input, movement);
        match bridge.aim_yaw.filter(|a| a.is_finite()) {
            Some(aim) => {
                predictor.facing = FacingMode::Aim;
                predictor.aim_yaw = aim;
            }
            None => predictor.facing = FacingMode::Velocity,
        }
        if bridge.follow_transform
            && let Some(transform) = transform
            && transform.translation.is_finite()
        {
            predictor.state.position.x = transform.translation.x;
            predictor.state.position.z = transform.translation.z;
        }
    }
}

/// Installs [`drive_predictors_from_characters`] before the motion-matching predictors run.
#[derive(Clone, Copy, Debug, Default)]
pub struct MotionMatchCharacterPlugin;

impl Plugin for MotionMatchCharacterPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(
            FixedUpdate,
            drive_predictors_from_characters.before(run_trajectory_predictors),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_becomes_the_desired_velocity_and_aim_strafes() {
        let movement = CharacterMovement::default();
        let walk = movement.sanitized().max_walk_speed;
        let mut input = CharacterInput {
            move_dir: Vec3::new(2.0, 5.0, 0.0),
            ..CharacterInput::default()
        };
        // Clamped to length 1, Y dropped.
        let v = desired_velocity(&input, &movement);
        assert!((v - Vec3::new(walk, 0.0, 0.0)).length() < 1e-5, "{v}");
        input.sprint = true;
        let sprint = desired_velocity(&input, &movement);
        assert!(sprint.length() >= v.length());
        input.move_dir = Vec3::NAN;
        assert_eq!(desired_velocity(&input, &movement), Vec3::ZERO);

        let mut world = World::new();
        let entity = world
            .spawn((
                MotionMatchCharacter {
                    aim_yaw: Some(0.5),
                    follow_transform: true,
                },
                CharacterInput {
                    move_dir: Vec3::Z,
                    ..CharacterInput::default()
                },
                movement.clone(),
                Transform::from_translation(Vec3::new(3.0, 1.0, -2.0)),
                TrajectoryPredictor::new(Vec3::ZERO, 0.0),
            ))
            .id();
        let mut schedule = Schedule::default();
        schedule.add_systems(drive_predictors_from_characters);
        schedule.run(&mut world);
        let predictor = world.get::<TrajectoryPredictor>(entity).expect("predictor");
        assert_eq!(predictor.facing, FacingMode::Aim);
        assert_eq!(predictor.aim_yaw, 0.5);
        assert!((predictor.desired_velocity - Vec3::new(0.0, 0.0, walk)).length() < 1e-5);
        assert_eq!(
            (predictor.state.position.x, predictor.state.position.z),
            (3.0, -2.0)
        );
    }
}
