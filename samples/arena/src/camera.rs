// SPDX-License-Identifier: MIT-0
//! First-person camera and player input (`docs/specs/14` M20 `samples/arena`).
//!
//! The camera is a [`CameraRig`] in [`CameraMode::FirstPerson`], welded to the local player's
//! pawn; `player_input` reads the `Move`/`Look`/`Fire` actions, steers the pawn in the camera's
//! yaw frame, drives the rig's yaw/pitch, and fires the rifle.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_framework::camera::{CameraMode, CameraRig, ChaseCamera, FirstPerson};
use regia_input::Actions;
use regia_render::{Camera, ClearMode};
use regia_scene::{GlobalTransform, Transform};

use crate::bots::clamp_to_arena;
use crate::match_flow::Phase;
use crate::pawn::Pawn;
use crate::weapons::{FireRequest, Weapon};

/// Player walk speed (m/s).
pub const PLAYER_SPEED: f32 = 6.0;
/// Look sensitivity (rad per unit of the `Look` axis, which is already pixel-scaled).
pub const LOOK_SENSITIVITY: f32 = 0.0025;
/// Fixed EV100 for the sunlit arena: 14.5, a sunny-day exposure under the 90 klx sun.
/// (It was 13.0, 1.5 EV brighter, to compensate the old linear-input AgX fit that
/// printed middle grey at 0.17; under the corrected transform EV 13 overexposes the
/// bots to pastel.)
pub const EXPOSURE_EV: f32 = 14.5;
/// Render layer of the local player's body. The first-person camera excludes it so the
/// view is from the pawn's eyes and not from inside its own mesh.
pub const PLAYER_MESH_LAYER: u32 = 1;

/// Installs the player input system.
pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Update, player_input);
    }
}

/// `Startup` (called after the match spawns pawns): spawns the first-person camera chasing the
/// local player pawn (or any pawn in a pure bot match).
pub fn spawn_camera(world: &mut World) {
    let target = {
        let mut pawns = world.query_filtered::<(Entity, &Pawn), ()>();
        pawns
            .iter(world)
            .min_by_key(|(_, pawn)| pawn.slot)
            .map(|(entity, _)| entity)
    };
    let Some(target) = target else {
        return; // no pawns yet; a later run would spawn the camera
    };
    let local_player = world
        .get_resource::<crate::config::ArenaConfig>()
        .is_none_or(|c| c.local_player);

    let eye = world
        .get::<Transform>(target)
        .map(|t| t.translation)
        .unwrap_or(Vec3::new(0.0, crate::pawn::PAWN_EYE_HEIGHT, 0.0));
    // Look toward the arena centre so a fresh match frames the floor and the other pawns.
    let yaw = yaw_toward(eye, Vec3::ZERO);
    let look = Quat::from_rotation_y(yaw);
    let transform = Transform {
        translation: eye,
        rotation: look,
        scale: Vec3::ONE,
    };
    world.spawn((
        regia_scene::components::Name::new("main_camera"),
        transform,
        GlobalTransform(transform.compute_affine()),
        CameraRig {
            mode: CameraMode::FirstPerson(FirstPerson {
                eye_offset: Vec3::ZERO, // the pawn already sits at eye height
                bob: 0.0,
            }),
            pitch: 0.0,
            yaw,
            ..CameraRig::default()
        },
        ChaseCamera { target },
        Camera {
            fov_y: 60.0_f32.to_radians(),
            near: 0.05,
            far: 200.0,
            exposure_ev: Some(EXPOSURE_EV),
            clear: ClearMode::Color([0.45, 0.62, 0.85, 1.0]),
            // Hide the possessed pawn's body (layer 1) so the shot is from its eyes.
            layer_mask: !(1 << PLAYER_MESH_LAYER),
            ..Camera::default()
        },
        crate::spectate::SpectatorFocus {
            enabled: !local_player,
            slot: 1,
        },
    ));
    // A bot-only match is watched through the spectator camera, following bot 1 (ADR-2134).
    if !local_player && !world.contains_resource::<regia_framework::spectator::SpectatorCamera>() {
        let watched = {
            let mut pawns = world.query::<(Entity, &Pawn, &Transform)>();
            pawns
                .iter(world)
                .filter(|(_, pawn, _)| pawn.slot == 1)
                .min_by_key(|(entity, _, _)| entity.index())
                .map(|(_, _, t)| t.translation)
        };
        world.insert_resource(crate::spectate::following_spectator(watched.unwrap_or(eye)));
    }
}

/// Yaw that aims `-Z`-forward `(−sin yaw, 0, −cos yaw)` from `from` toward `to`.
pub fn yaw_toward(from: Vec3, to: Vec3) -> f32 {
    let delta = to - from;
    (-delta.x).atan2(-delta.z)
}

/// `Update`: reads player actions and drives the pawn (movement) and camera (look, fire).
#[tracing::instrument(skip_all, target = "arena", name = "player_input")]
fn player_input(
    actions: Res<Actions>,
    time: Res<Time>,
    state: Option<Single<&crate::match_flow::ArenaState>>,
    mut pawns: Query<(Entity, &Pawn, &mut Transform)>,
    mut rigs: Query<&mut CameraRig>,
    chases: Query<&ChaseCamera>,
    mut fire: MessageWriter<FireRequest>,
) {
    // Player movement only during the warm-up and round.
    if state.is_some_and(|s| s.phase == Phase::MatchOver) {
        return;
    }
    let dt = time.delta_secs();
    let move_axes: Vec2 = actions.axis2d("Move");
    let look: Vec2 = actions.axis2d("Look") * LOOK_SENSITIVITY;

    // Snapshot the current yaw/pitch before mutating (movement uses the pre-look yaw).
    let (yaw, pitch) = rigs
        .iter()
        .next()
        .map(|rig| (rig.yaw, rig.pitch))
        .unwrap_or((0.0, 0.0));

    // Movement frame from the camera yaw.
    let forward = Vec3::new(-yaw.sin(), 0.0, -yaw.cos());
    let right = Vec3::new(yaw.cos(), 0.0, -yaw.sin());
    let dir = (forward * move_axes.y + right * move_axes.x).normalize_or_zero();

    // Move the local player's pawn and remember its eye position.
    let mut eye = Vec3::ZERO;
    let mut shooter = Entity::PLACEHOLDER;
    for (entity, pawn, mut transform) in &mut pawns {
        if pawn.is_bot {
            continue;
        }
        eye = transform.translation;
        shooter = entity;
        if dir.length_squared() > 1.0e-6 {
            transform.translation = clamp_to_arena(transform.translation + dir * PLAYER_SPEED * dt);
        }
    }

    // Update the camera rig's yaw/pitch from look input.
    for mut rig in &mut rigs {
        rig.yaw = wrap(rig.yaw - look.x);
        rig.pitch = (rig.pitch + look.y).clamp(-1.4, 1.4);
    }

    // Fire the rifle from the camera's eye.
    if actions.just_pressed_this_frame("Fire") {
        let shooter = chases.iter().next().map(|chase| chase.target).unwrap_or(shooter);
        let look_q = Quat::from_rotation_y(yaw) * Quat::from_rotation_x(pitch);
        let direction = look_q * Vec3::NEG_Z;
        fire.write(FireRequest {
            shooter,
            position: eye,
            direction,
            weapon: Weapon::Rifle,
        });
    }
}

/// Wraps an angle into `(-π, π]`.
fn wrap(angle: f32) -> f32 {
    let wrapped = angle.rem_euclid(std::f32::consts::TAU);
    if wrapped > std::f32::consts::PI {
        wrapped - std::f32::consts::TAU
    } else {
        wrapped
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yaw_toward_the_origin_from_the_south_west_corner_faces_the_centre() {
        let from = Vec3::new(-10.0, 1.6, -10.0);
        let yaw = yaw_toward(from, Vec3::ZERO);
        let forward = Vec3::new(-yaw.sin(), 0.0, -yaw.cos());
        let to_centre = Vec3::new(10.0, 0.0, 10.0).normalize();
        assert!(
            forward.dot(to_centre) > 0.99,
            "forward {forward:?} should aim at the center"
        );
    }

    #[test]
    fn forward_and_right_are_orthonormal_at_yaw_zero() {
        let yaw = 0.0f32;
        let forward = Vec3::new(-yaw.sin(), 0.0, -yaw.cos());
        let right = Vec3::new(yaw.cos(), 0.0, -yaw.sin());
        assert!((forward - Vec3::NEG_Z).length() < 1e-6);
        assert!((right - Vec3::X).length() < 1e-6);
        assert!(forward.dot(right).abs() < 1e-6);
    }
}
