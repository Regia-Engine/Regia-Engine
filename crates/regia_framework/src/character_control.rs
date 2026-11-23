// SPDX-License-Identifier: MPL-2.0
//! Character control helpers (`docs/specs/91-character-save-templates.md` § 2.3): the stick-to-world
//! mapping both character templates use, and [`MoveTo`], which walks a [`Character`] to a point.
//!
//! [`MoveTo`] is the driver for point-and-click movement, simple AI errands and agent tests: each
//! frame it writes [`CharacterInput::move_dir`] toward the target (horizontal only, slowing inside
//! `slow_radius`), and on arrival it zeroes the input, removes itself and sends one
//! [`MoveToArrived`]. It walks a straight line; paths around obstacles come from `regia_nav`.

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_scene::Transform;

use crate::detmath;
use regia_scene::layers::LayerParked;

use crate::character::{Character, CharacterInput};

/// Smallest speed fraction inside the slow radius, so a character never stalls short of the
/// accept radius.
const MIN_APPROACH_SPEED: f32 = 0.15;

/// Maps a stick or WASD vector (`x` right, `y` forward, as `Actions::axis2d` reports it) to a
/// world-space horizontal direction for a camera looking along `yaw` (radians about +Y, 0 = looking
/// down -Z). The result has no Y and a length of at most 1. The basis comes from the deterministic
/// kernel (spec 15 § 5), not the platform libm: a predicted character's move vector must be the
/// same bits on both lockstep peers.
pub fn camera_relative_move(yaw: f32, axes: Vec2) -> Vec3 {
    let (sin, cos) = detmath::sin_cos(yaw);
    let forward = Vec3::new(-sin, 0.0, -cos).normalize_or_zero();
    let right = Vec3::new(cos, 0.0, -sin).normalize_or_zero();
    (forward * axes.y + right * axes.x).clamp_length_max(1.0)
}

/// Walk this character to `target` (horizontal distance; height is ignored).
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component)]
pub struct MoveTo {
    /// The world-space point to reach.
    pub target: Vec3,
    /// Arrived when the horizontal distance is at most this (metres).
    pub accept_radius: f32,
    /// Inside this horizontal distance the input scales down with the distance (metres; 0 = full
    /// speed until arrival).
    pub slow_radius: f32,
    /// Hold sprint while walking there.
    pub sprint: bool,
}

impl MoveTo {
    /// A move to `target` with a 0.25 m accept radius and a 1.5 m slow radius, walking.
    pub fn new(target: Vec3) -> Self {
        Self {
            target,
            accept_radius: 0.25,
            slow_radius: 1.5,
            sprint: false,
        }
    }

    /// The input a character at `position` should get: the direction to the target scaled by the
    /// approach speed, or `None` once it has arrived.
    pub fn steer(&self, position: Vec3) -> Option<Vec3> {
        let offset = (self.target - position).with_y(0.0);
        let distance = offset.length();
        if distance <= self.accept_radius.max(0.0) {
            return None;
        }
        let speed = if self.slow_radius > 0.0 && distance < self.slow_radius {
            (distance / self.slow_radius).clamp(MIN_APPROACH_SPEED, 1.0)
        } else {
            1.0
        };
        Some(offset / distance * speed)
    }
}

/// A [`MoveTo`] finished: the character came within its accept radius.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct MoveToArrived {
    /// The character.
    pub entity: Entity,
    /// The target it reached.
    pub target: Vec3,
}

/// Live characters with a destination.
type Movers<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static MoveTo,
        &'static Transform,
        &'static mut CharacterInput,
    ),
    (With<Character>, Without<LayerParked>),
>;

/// `Update`: steers every character with a [`MoveTo`].
#[tracing::instrument(skip_all, target = "regia_framework", name = "drive_move_to")]
fn drive_move_to(mut commands: Commands, mut movers: Movers, mut arrived: MessageWriter<MoveToArrived>) {
    for (entity, move_to, transform, mut input) in &mut movers {
        match move_to.steer(transform.translation) {
            Some(direction) => {
                input.move_dir = direction;
                input.sprint = move_to.sprint;
            }
            None => {
                input.move_dir = Vec3::ZERO;
                input.sprint = false;
                commands.entity(entity).remove::<MoveTo>();
                arrived.write(MoveToArrived {
                    entity,
                    target: move_to.target,
                });
            }
        }
    }
}

/// Registers [`MoveTo`] and [`MoveToArrived`] and runs the steering in `Update`. Use with
/// [`CharacterPlugin`](crate::character::CharacterPlugin).
pub struct CharacterControlPlugin;

impl Plugin for CharacterControlPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<MoveTo>()
            .add_message::<MoveToArrived>()
            .add_systems(Update, drive_move_to);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::character::{CharacterMovement, CharacterPlugin, CharacterState};
    use crate::save_game::{SaveGame, SavePlugin, SaveSpec, Saveable};

    #[test]
    fn camera_relative_move_follows_the_camera_yaw() {
        let close = |a: Vec3, b: Vec3| (a - b).length() < 1e-5;
        // Yaw 0 looks down -Z: forward is -Z, right is +X.
        assert!(close(camera_relative_move(0.0, Vec2::new(0.0, 1.0)), -Vec3::Z));
        assert!(close(camera_relative_move(0.0, Vec2::new(1.0, 0.0)), Vec3::X));
        // A quarter turn left (+90 degrees about +Y) looks down -X.
        let quarter = std::f32::consts::FRAC_PI_2;
        assert!(close(
            camera_relative_move(quarter, Vec2::new(0.0, 1.0)),
            -Vec3::X
        ));
        assert!(close(
            camera_relative_move(quarter, Vec2::new(1.0, 0.0)),
            -Vec3::Z
        ));
        // Diagonals are clamped to length 1, and nothing points up.
        let diagonal = camera_relative_move(0.3, Vec2::new(1.0, 1.0));
        assert!((diagonal.length() - 1.0).abs() < 1e-5 && diagonal.y == 0.0);
    }

    fn reference_sin_cos(radians: f32) -> (f32, f32) {
        let q = f64::from(radians) / std::f64::consts::FRAC_PI_2;
        let k = (q + 0.5).floor();
        let x = (q - k) * std::f64::consts::FRAC_PI_2;
        let x2 = x * x;
        let (mut sin, mut cos) = (0.0, 0.0);
        for n in (0..8).rev() {
            let m = f64::from(2 * n);
            cos = 1.0 - cos * x2 / ((m + 1.0) * (m + 2.0));
            sin = 1.0 - sin * x2 / ((m + 2.0) * (m + 3.0));
        }
        let sin = x * sin;
        let (sin, cos) = match (k as i64).rem_euclid(4) {
            0 => (sin, cos),
            1 => (cos, -sin),
            2 => (-sin, -cos),
            _ => (-cos, sin),
        };
        (sin as f32, cos as f32)
    }

    #[test]
    fn camera_relative_move_uses_deterministic_yaw_bits() {
        for i in -1_000..1_000 {
            let yaw = i as f32 * 0.013;
            let (sin, cos) = reference_sin_cos(yaw);
            let actual_sin_cos = detmath::sin_cos(yaw);
            assert_eq!(
                (actual_sin_cos.0.to_bits(), actual_sin_cos.1.to_bits()),
                (sin.to_bits(), cos.to_bits()),
                "kernel mismatch at yaw = {yaw}"
            );
            let forward = Vec3::new(-sin, 0.0, -cos).normalize_or_zero();
            let right = Vec3::new(cos, 0.0, -sin).normalize_or_zero();
            let axes = Vec2::new(0.0, 1.0);
            let expected = (forward * axes.y + right * axes.x).clamp_length_max(1.0);
            let actual = camera_relative_move(yaw, axes);
            assert_eq!(
                actual.to_array().map(f32::to_bits),
                expected.to_array().map(f32::to_bits),
                "yaw = {yaw}"
            );
        }
    }

    #[test]
    fn steer_slows_inside_the_slow_radius_and_stops_inside_the_accept_radius() {
        let move_to = MoveTo::new(Vec3::new(10.0, 5.0, 0.0));
        let far = move_to.steer(Vec3::ZERO).expect("far away");
        assert!(
            (far - Vec3::X).length() < 1e-5,
            "full speed, height ignored: {far:?}"
        );
        let near = move_to.steer(Vec3::new(9.25, 0.0, 0.0)).expect("near");
        assert!(
            (near.length() - 0.5).abs() < 1e-5,
            "0.75 m of 1.5 m: half speed, {near:?}"
        );
        let slower = move_to
            .steer(Vec3::new(9.7, 0.0, 0.0))
            .expect("still outside 0.25 m");
        assert!((slower.length() - 0.2).abs() < 1e-5, "0.3 m of 1.5 m: {slower:?}");
        assert_eq!(move_to.steer(Vec3::new(9.9, -3.0, 0.1)), None, "arrived");
        // The floor: with a small accept radius, 0.2 m out would be 0.13 of full speed.
        let tight = MoveTo {
            accept_radius: 0.1,
            ..MoveTo::new(Vec3::new(10.0, 0.0, 0.0))
        };
        let creeping = tight.steer(Vec3::new(9.8, 0.0, 0.0)).expect("outside 0.1 m");
        assert!(
            (creeping.length() - MIN_APPROACH_SPEED).abs() < 1e-5,
            "{creeping:?}"
        );
    }

    /// One fixed step per update at 62.5 Hz, flat ground, one character with a `MoveTo`.
    fn app() -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CharacterPlugin, CharacterControlPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        let mut fixed = FixedTime::default();
        fixed.hz = 62.5;
        app.insert_resource(fixed);
        let entity = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 1.2, 0.0)),
                Character,
                CharacterMovement::default(),
                CharacterInput::default(),
                CharacterState::default(),
                MoveTo::new(Vec3::new(4.0, 0.0, -3.0)),
            ))
            .id();
        (app, entity)
    }

    #[test]
    fn move_to_walks_a_character_to_the_target_and_reports_once() {
        let (mut app, entity) = app();
        let mut arrivals = Vec::new();
        let mut cursor = app.world().resource::<Messages<MoveToArrived>>().get_cursor();
        for _ in 0..300 {
            app.update();
            let messages = app.world().resource::<Messages<MoveToArrived>>();
            arrivals.extend(cursor.read(messages).cloned());
        }
        assert_eq!(arrivals.len(), 1, "one arrival: {arrivals:?}");
        assert_eq!(arrivals[0].entity, entity);
        assert!(
            app.world().get::<MoveTo>(entity).is_none(),
            "the component is removed"
        );
        let position = app.world().get::<Transform>(entity).map(|t| t.translation);
        let position = position.unwrap_or(Vec3::splat(f32::MAX));
        let miss = (position - Vec3::new(4.0, 0.0, -3.0)).with_y(0.0).length();
        // Braking after the input stops carries the character a little past the accept radius.
        assert!(miss < 1.0, "ended {miss} m from the target at {position:?}");
        let input = app
            .world()
            .get::<CharacterInput>(entity)
            .cloned()
            .unwrap_or_default();
        assert_eq!(input.move_dir, Vec3::ZERO, "the input is released");
    }

    #[test]
    fn a_saved_character_restores_its_transform_and_movement_settings() {
        let (mut app, entity) = app();
        app.add_plugins(SavePlugin::default())
            .register_type::<Transform>();
        for _ in 0..20 {
            app.update();
        }
        let walked = app.world().get::<Transform>(entity).map(|t| t.translation);
        app.world_mut().entity_mut(entity).insert(Saveable);
        if let Some(mut movement) = app.world_mut().get_mut::<CharacterMovement>(entity) {
            movement.max_walk_speed = 3.25;
        }
        let bytes = SaveGame::encode(app.world_mut(), &SaveSpec::default()).expect("encode");
        app.world_mut().despawn(entity);
        SaveGame::decode(app.world_mut(), &bytes, &SaveSpec::default()).expect("decode");
        let world = app.world_mut();
        let restored: Vec<(Vec3, f32, bool)> = world
            .query_filtered::<(&Transform, &CharacterMovement, Option<&MoveTo>), With<Character>>()
            .iter(world)
            .map(|(t, m, move_to)| (t.translation, m.max_walk_speed, move_to.is_some()))
            .collect();
        assert_eq!(restored.len(), 1, "one character back");
        assert_eq!(Some(restored[0].0), walked, "position");
        assert_eq!(restored[0].1, 3.25, "movement settings");
        assert!(restored[0].2, "an unfinished MoveTo is saved too");
    }
}
