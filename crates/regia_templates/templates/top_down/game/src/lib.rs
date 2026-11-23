// SPDX-License-Identifier: MIT-0
//! {{name}} gameplay: a top-down starter (Top Down template, spec 91).
//!
//! A capsule character with rapier-driven movement, an overhead camera tilted toward the
//! horizon, screen-relative movement (`camera_relative_move`), and quicksave / quickload on
//! F5 / F9 through the framework's save flow.
//!
//! On top of that:
//!
//! - `level`: the ground and four pillars (static colliders).
//! - `enemy`: one enemy that walks a navmesh (`regia_nav`), thinks with a behavior tree
//!   (`regia_ai`), and wears a health bar. Whether it is defeated is saved.
//! - `combat`: health, damage, the player's attack and the player's death and respawn.
//! - `hud`: health and enemy status, a controls hint and a save/load toast.
//! - This file: input (WASD / left stick, **click to walk**, Q / E turn the view, F / right
//!   click attack), the fixed-angle camera and the save checkpoint.
//!
//! `README.md` lists where to extend each part.

use regia::physics::{CharacterBody, PhysicsPlugin};
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::camera::{CameraMode, CameraPlugin, CameraRig, ChaseCamera, Overhead};
use regia_framework::character::{
    Character, CharacterInput, CharacterMovement, CharacterPlugin, CharacterState,
    use_rapier_character_controller,
};
use regia_framework::character_control::{CharacterControlPlugin, camera_relative_move};
use regia_framework::save_flow::{SaveFlowPlugin, SaveInfo, SaveOp, SaveOutcome};
use regia_framework::save_game::{RegisterSaveableExt, SavePlugin, SaveableResource};
use regia_framework::script::ScriptPlugin;
use regia_input::prelude::*;
use regia_render::{Camera, ClearMode, Environment, MeshRenderer};
use regia_scene::{GlobalTransform, Transform, components::Name};

pub mod combat;
pub mod enemy;
pub mod hud;
pub mod level;

/// The template's game plugin.
pub struct Game;

/// Where the player stands, kept in every save. Copied from the character each frame; after a
/// load the character is moved back to it.
///
/// A load respawns `Saveable` entities from their reflected components only, and meshes and
/// physics bodies are not reflected, so the player entity itself is not marked `Saveable`.
/// Add the rest of your player state here (inventory, ...); it is saved with the game. Health
/// and the enemy's fate ride in `enemy::CombatProgress`, also saved.
#[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Resource, Default)]
pub struct PlayerCheckpoint {
    /// The character's position.
    pub position: Vec3,
    /// The camera's yaw (radians).
    pub view_yaw: f32,
}

impl SaveableResource for PlayerCheckpoint {}

/// Turn speed of the view in radians per second.
const TURN_RATE: f32 = 1.8;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        let config = InputConfig::new()
            .action("Move", ActionKind::Axis2D)
            .action("Look", ActionKind::Axis2D)
            .action("TurnLeft", ActionKind::Button)
            .action("TurnRight", ActionKind::Button)
            .action("Jump", ActionKind::Button)
            .action("Click", ActionKind::Button)
            .action("Attack", ActionKind::Button)
            .action("QuickSave", ActionKind::Button)
            .action("QuickLoad", ActionKind::Button)
            .binding(Binding::new(
                "Move",
                InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
            ))
            .binding(Binding::new("Move", InputSource::gamepad_stick(Stick::Left)))
            .binding(Binding::new("Look", InputSource::gamepad_stick(Stick::Right)))
            .binding(Binding::new("TurnLeft", InputSource::key("KeyQ")))
            .binding(Binding::new("TurnRight", InputSource::key("KeyE")))
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .binding(Binding::new(
                "Jump",
                InputSource::gamepad_button(GamepadButton::South),
            ))
            .binding(Binding::new(
                "Click",
                InputSource::mouse_button(MouseButton::Left),
            ))
            .binding(Binding::new("Attack", InputSource::key("KeyF")))
            .binding(Binding::new(
                "Attack",
                InputSource::mouse_button(MouseButton::Right),
            ))
            .binding(Binding::new(
                "Attack",
                InputSource::gamepad_button(GamepadButton::West),
            ))
            .binding(Binding::new("QuickSave", InputSource::key("F5")))
            .binding(Binding::new("QuickLoad", InputSource::key("F9")));

        // A plugin tuple holds at most eight plugins; the save plugins go in their own call.
        app.add_plugins((
            regia_scene::ScenePlugin,
            PhysicsPlugin,
            CharacterPlugin,
            CharacterControlPlugin,
            CameraPlugin,
            ScriptPlugin,
            InputPlugin::new(config),
        ))
        .add_plugins((SavePlugin::default(), SaveFlowPlugin, enemy::EnemyPlugin));
        use_rapier_character_controller(app);
        app.register_saveable_resource::<PlayerCheckpoint>()
            .register_saveable_resource::<enemy::CombatProgress>()
            .register_type::<combat::Health>()
            .register_type::<combat::Fighter>()
            .register_type::<hud::StatusLabel>()
            .register_type::<hud::EnemyLabel>()
            .register_type::<hud::ToastLabel>()
            .add_message::<combat::DamageEvent>()
            .add_message::<combat::Died>()
            .init_resource::<PlayerCheckpoint>()
            .init_resource::<enemy::CombatProgress>()
            .init_resource::<MoveTarget>()
            .init_resource::<Environment>()
            .add_systems(
                Startup,
                (
                    set_save_level,
                    level::setup_level,
                    setup_character,
                    setup_camera,
                    hud::setup_hud,
                    enemy::setup,
                )
                    .chain(),
            )
            .add_systems(
                Update,
                (
                    pick_move_target,
                    write_character_input,
                    turn_view,
                    // Restore first: a load applied in `Last` is seen here a frame later, and
                    // tracking before restoring would copy the live pose over the loaded one.
                    restore_checkpoint,
                    track_checkpoint,
                    combat::player_attack,
                    combat::apply_damage,
                    enemy::handle_deaths,
                    combat::respawn_player,
                    enemy::reconcile_on_change,
                    enemy::update_bars,
                    track_health,
                    hud::refresh_status,
                    hud::toast,
                )
                    .chain(),
            );
    }
}

/// `Startup`: names the level in every save's slot info.
fn set_save_level(mut info: ResMut<SaveInfo>) {
    info.level = "main".into();
}

/// The player: a character capsule with a box mesh standing in for the body. The capsule's
/// centre rests at 1.2 m (half height 0.9 + radius 0.3), so the box is 1.2 m half-height to
/// reach the floor. Replace the box with your model (the `third_person` template shows a
/// procedural walking rig).
fn setup_character(mut commands: Commands) {
    let start = Transform::from_translation(combat::SPAWN_POINT);
    commands.spawn((
        Name::new("Player1"),
        start,
        GlobalTransform(start.compute_affine()),
        Character,
        CharacterMovement::default(),
        CharacterInput::default(),
        CharacterState::default(),
        CharacterBody::default(),
        combat::Health::new(combat::PLAYER_HEALTH),
        combat::Fighter::default(),
        MeshRenderer::new(
            std::sync::Arc::new(level::cube_mesh(Vec3::new(0.3, 1.2, 0.3))),
            level::solid([0.2, 0.55, 0.9, 1.0]),
        ),
    ));
}

/// The overhead camera: tilted 55 degrees down, following the character.
fn setup_camera(mut commands: Commands, characters: Query<Entity, With<Character>>) {
    let Some(target) = characters.iter().next() else {
        warn!("top_down template: no character to follow");
        return;
    };
    let mode = Overhead {
        height: 16.0,
        pitch: -55.0_f32.to_radians(),
        ..Overhead::default()
    };
    commands.spawn((
        Name::new("Player1Camera"),
        Transform::default(),
        CameraRig {
            mode: CameraMode::Overhead(mode),
            ..CameraRig::default()
        },
        ChaseCamera { target },
        Camera {
            clear: ClearMode::Color([0.06, 0.07, 0.09, 1.0]),
            ..Camera::default()
        },
    ));
}

/// Where a click told the player to walk (ground position), until it arrives or a key overrides it.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct MoveTarget(pub Option<Vec3>);

/// Distance at which a click target counts as reached (meters).
const ARRIVE: f32 = 0.35;

/// The point where the camera ray through pixel `(px, py)` of a `width` x `height` image meets
/// the ground plane `y = 0`; `None` when the ray points up or at the horizon.
///
/// `eye` and `rotation` are the camera's world pose (it looks down -Z) and `fov_y` its vertical
/// field of view in radians.
#[must_use]
pub fn ground_point(
    eye: Vec3,
    rotation: regia_core::math::Quat,
    fov_y: f32,
    size: (f32, f32),
    pixel: (f32, f32),
) -> Option<Vec3> {
    if size.0 <= 0.0 || size.1 <= 0.0 {
        return None;
    }
    let ndc_x = pixel.0 / size.0 * 2.0 - 1.0;
    let ndc_y = 1.0 - pixel.1 / size.1 * 2.0;
    let half = (fov_y * 0.5).tan();
    let direction = rotation * Vec3::NEG_Z
        + rotation * Vec3::X * (ndc_x * half * size.0 / size.1)
        + rotation * Vec3::Y * (ndc_y * half);
    if direction.y >= -1e-4 {
        return None;
    }
    let t = -eye.y / direction.y;
    (t > 0.0).then(|| eye + direction * t)
}

/// `Update`: a left click sets [`MoveTarget`] to the ground point under the cursor.
fn pick_move_target(
    actions: Res<Actions>,
    mouse: Option<Res<Mouse>>,
    windows: Query<&regia_platform::Window, With<regia_platform::PrimaryWindow>>,
    cameras: Query<(&GlobalTransform, &Camera)>,
    mut target: ResMut<MoveTarget>,
) {
    if !actions.just_pressed_this_frame("Click") {
        return;
    }
    let (Some(mouse), Some(window), Some((camera_at, camera))) =
        (mouse, windows.iter().next(), cameras.iter().next())
    else {
        return;
    };
    let (_, rotation, eye) = camera_at.0.to_scale_rotation_translation();
    let size = (window.physical_size.0 as f32, window.physical_size.1 as f32);
    let cursor = mouse.position();
    if let Some(point) = ground_point(eye, rotation, camera.fov_y, size, (cursor.x, cursor.y)) {
        target.0 = Some(point.with_y(0.0));
    }
}

/// WASD / left stick → `CharacterInput`, relative to the screen (the camera's yaw). With no key
/// held, the player walks toward the clicked [`MoveTarget`]. A dead player does not move.
fn write_character_input(
    actions: Res<Actions>,
    rigs: Query<&CameraRig>,
    mut target: ResMut<MoveTarget>,
    mut inputs: Query<(&mut CharacterInput, &Transform, &combat::Fighter), With<Character>>,
) {
    let yaw = rigs.iter().next().map_or(0.0, |rig| rig.yaw);
    let keys = camera_relative_move(yaw, actions.axis2d("Move"));
    for (mut input, transform, fighter) in &mut inputs {
        let mut move_dir = keys;
        if keys.length_squared() > 1e-4 {
            target.0 = None;
        } else if let Some(goal) = target.0 {
            let to = (goal - transform.translation).with_y(0.0);
            if to.length() < ARRIVE {
                target.0 = None;
            } else {
                move_dir = to.normalize_or_zero();
            }
        }
        let down = fighter.respawn_in.is_some();
        input.move_dir = if down { Vec3::ZERO } else { move_dir };
        input.jump = !down && actions.pressed("Jump");
    }
}

/// Q / E or the right stick turn the view about the character.
fn turn_view(actions: Res<Actions>, time: Res<Time>, mut rigs: Query<&mut CameraRig>) {
    let mut turn = actions.axis2d("Look").x;
    if actions.pressed("TurnLeft") {
        turn -= 1.0;
    }
    if actions.pressed("TurnRight") {
        turn += 1.0;
    }
    let delta = -turn.clamp(-1.0, 1.0) * TURN_RATE * time.delta_secs();
    for mut rig in &mut rigs {
        rig.yaw += delta;
    }
}

/// Copies the player's position and view into the saved checkpoint.
fn track_checkpoint(
    characters: Query<&Transform, With<Character>>,
    rigs: Query<&CameraRig>,
    mut checkpoint: ResMut<PlayerCheckpoint>,
) {
    if let Some(transform) = characters.iter().next() {
        checkpoint.position = transform.translation;
    }
    if let Some(rig) = rigs.iter().next() {
        checkpoint.view_yaw = rig.yaw;
    }
}

/// Copies the player's health into the saved progress (only on a change).
fn track_health(
    players: Query<&combat::Health, With<Character>>,
    mut progress: ResMut<enemy::CombatProgress>,
) {
    if let Some(health) = players.iter().next()
        && (progress.health - health.current).abs() > f32::EPSILON
    {
        progress.health = health.current;
    }
}

/// After a successful load, moves the player back to the restored checkpoint and health.
/// (`hud::toast` shows the "Saved" / "Loaded" toast from the same outcomes.)
fn restore_checkpoint(
    mut outcomes: MessageReader<SaveOutcome>,
    checkpoint: Res<PlayerCheckpoint>,
    progress: Res<enemy::CombatProgress>,
    mut target: ResMut<MoveTarget>,
    mut characters: Query<
        (
            &mut Transform,
            &mut CharacterState,
            &mut combat::Health,
            &mut combat::Fighter,
        ),
        With<Character>,
    >,
    mut rigs: Query<&mut CameraRig>,
) {
    let loaded = outcomes
        .read()
        .any(|outcome| outcome.op == SaveOp::Load && outcome.result.is_ok());
    if !loaded {
        return;
    }
    target.0 = None;
    for (mut transform, mut state, mut health, mut fighter) in &mut characters {
        transform.translation = checkpoint.position;
        state.velocity = Vec3::ZERO;
        fighter.respawn_in = None;
        // A save made while dead restores a living player.
        health.current = if progress.health > 0.0 {
            progress.health
        } else {
            health.max
        };
    }
    for mut rig in &mut rigs {
        rig.yaw = checkpoint.view_yaw;
    }
}

// REGIA:SCRIPTS

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;

    #[test]
    fn a_ray_down_the_middle_of_a_tilted_camera_hits_the_ground_ahead() {
        // 10 m up, looking 45 degrees down toward -Z: the screen center meets the ground 10 m ahead.
        let rotation = Quat::from_rotation_x(-std::f32::consts::FRAC_PI_4);
        let hit = ground_point(
            Vec3::new(0.0, 10.0, 0.0),
            rotation,
            1.0,
            (800.0, 600.0),
            (400.0, 300.0),
        )
        .expect("the ray meets the ground");
        assert!(
            hit.x.abs() < 1e-3 && (hit.z + 10.0).abs() < 1e-2 && hit.y.abs() < 1e-3,
            "{hit}"
        );
    }

    #[test]
    fn a_ray_above_the_horizon_hits_nothing() {
        let hit = ground_point(
            Vec3::new(0.0, 2.0, 0.0),
            Quat::IDENTITY,
            1.0,
            (800.0, 600.0),
            (400.0, 0.0),
        );
        assert!(hit.is_none());
        assert!(ground_point(Vec3::ZERO, Quat::IDENTITY, 1.0, (0.0, 0.0), (0.0, 0.0)).is_none());
    }
}
