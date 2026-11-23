// SPDX-License-Identifier: MIT-0
//! {{name}} gameplay: the First Person template.
//!
//! A first-person camera (`regia_framework`'s [`CameraRig`] in `CameraMode::FirstPerson`) welded
//! to a player pawn, moved with the `Move`/`Look` actions, in a small level you can interact
//! with. It is the stripped-down starting point of the engine's arena sample; add weapons, AI and your own
//! world from here.
//!
//! - `level`: floor, a wall with a gate, a button pillar and a lamp (procedural boxes).
//! - `interact`: look at the gate or the button, read the prompt, press E. The state of every
//!   door and button is a saved resource, so a load restores it.
//! - `hud`: crosshair, interaction prompt, save/load toast and the Esc pause menu.
//! - `menu`: the settings panel (volumes, mouse sensitivity, invert Y, graphics preset).
//! - `controls`: the F10 rebinding screen.
//! - This file: input wiring, collision with the walls, and the save (F5 quicksave, F9 quickload;
//!   the player's position rides in `PlayerCheckpoint`).
//!
//! `README.md` lists where to extend each part.

mod controls;
pub mod hud;
pub mod interact;
pub mod level;
pub mod menu;

use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::camera::{CameraMode, CameraPlugin, CameraRig, ChaseCamera, FirstPerson};
use regia_framework::flow::{GameFlow, GameFlowPlugin, GameFlowSettings};
use regia_framework::save_flow::{SaveFlowPlugin, SaveInfo, SaveOp, SaveOutcome};
use regia_framework::save_game::{RegisterSaveableExt, SavePlugin, SaveableResource};
use regia_framework::settings::{GameSettings, SettingsPlugin};
use regia_input::prelude::*;
use regia_render::{Camera, Environment};
use regia_scene::{GlobalTransform, Transform};

/// The player walk speed (m/s).
const WALK_SPEED: f32 = 6.0;
/// Mouse look sensitivity (radians per pixel) at a settings sensitivity of 1.
const MOUSE_SENSITIVITY: f32 = 0.0025;
/// Right-stick look rate at full deflection (radians per second).
const STICK_TURN_RATE: f32 = 3.0;
/// Where the player starts (eye height).
pub const SPAWN: Vec3 = Vec3::new(0.0, 1.6, 4.0);

/// Marks the player pawn.
#[derive(Component, Debug, Clone, Copy, Default)]
pub struct Player;

/// Where the player stands, kept in every save. Copied from the pawn each frame; after a load the
/// pawn is moved back to it. Add the rest of your player state here (health, inventory, ...).
#[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Resource, Default)]
pub struct PlayerCheckpoint {
    /// The pawn's position.
    pub position: Vec3,
    /// The view's yaw (radians).
    pub view_yaw: f32,
    /// The view's pitch (radians).
    pub view_pitch: f32,
}

impl SaveableResource for PlayerCheckpoint {}

/// The First Person template's game plugin.
pub struct Game;

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        let config = InputConfig::new()
            .action("Move", ActionKind::Axis2D)
            .action("Look", ActionKind::Axis2D)
            .action("Use", ActionKind::Button)
            .action("Pause", ActionKind::Button)
            .action("QuickSave", ActionKind::Button)
            .action("QuickLoad", ActionKind::Button)
            .binding(Binding::new(
                "Move",
                InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
            ))
            .binding(Binding::new("Move", InputSource::gamepad_stick(Stick::Left)))
            // Mouse: radians per pixel; screen Y grows downward, pitch grows upward, so Y is negated.
            .binding(
                Binding::new("Look", InputSource::MouseDelta).with_modifier(Vec3::new(
                    MOUSE_SENSITIVITY,
                    -MOUSE_SENSITIVITY,
                    1.0,
                )),
            )
            // Stick: radians per second (already up-positive), scaled by the frame time.
            .binding(
                Binding::new("Look", InputSource::gamepad_stick(Stick::Right))
                    .with_modifier(Vec3::new(STICK_TURN_RATE, STICK_TURN_RATE, 1.0))
                    .with_modifier(Modifier::ScaleByDelta),
            )
            .binding(Binding::new("Use", InputSource::key("KeyE")))
            .binding(Binding::new(
                "Use",
                InputSource::gamepad_button(GamepadButton::West),
            ))
            .binding(Binding::new("Pause", InputSource::key("Escape")))
            .binding(Binding::new("QuickSave", InputSource::key("F5")))
            .binding(Binding::new("QuickLoad", InputSource::key("F9")));

        // No title screen: the game starts in `Playing` (pause is `GameFlow::Paused`).
        if !app.world().contains_resource::<GameFlowSettings>() {
            app.insert_resource(GameFlowSettings {
                skip_main_menu: true,
                ..Default::default()
            });
        }
        // A headless app (tests) has no audio plugin: add one without a device, so the settings
        // menu's volumes have buses to apply to.
        if !app.has_plugin(std::any::type_name::<regia::audio::AudioPlugin>()) {
            if !app.world().contains_resource::<regia::audio::AudioSettings>() {
                app.insert_resource(regia::audio::AudioSettings {
                    headless: true,
                    ..Default::default()
                });
            }
            app.add_plugins(regia::audio::AudioPlugin);
        }

        app.add_plugins((
            InputPlugin::new(config),
            regia_scene::ScenePlugin,
            CameraPlugin,
            controls::ControlsPlugin,
            SavePlugin::default(),
            SaveFlowPlugin,
        ))
        // A plugin tuple holds at most eight plugins; the rest go in their own call.
        .add_plugins((GameFlowPlugin, SettingsPlugin))
        .register_saveable_resource::<PlayerCheckpoint>()
        .register_saveable_resource::<interact::WorldState>()
        .register_type::<interact::Interactable>()
        .register_type::<interact::Door>()
        .register_type::<interact::Button>()
        .register_type::<hud::PromptLabel>()
        .register_type::<hud::ToastLabel>()
        .register_type::<menu::MenuState>()
        .init_resource::<PlayerCheckpoint>()
        .init_resource::<interact::WorldState>()
        .init_resource::<interact::Focus>()
        .init_resource::<hud::MenuCursor>()
        .init_resource::<Environment>()
        .add_systems(
            Startup,
            (set_save_level, level::setup_level, spawn_player, hud::setup_hud).chain(),
        )
        .add_systems(
            Update,
            (
                move_player.run_if(playing),
                // Restore first: a load applied in `Last` is seen here a frame later, and
                // tracking before restoring would copy the live pose over the loaded one.
                restore_checkpoint,
                track_checkpoint,
                interact::find_focus,
                interact::use_focus.run_if(playing),
                interact::sync_from_state,
                interact::animate,
                hud::toggle_pause,
                hud::menu_clicks,
                menu::menu_clicks,
                menu::sync_menus,
                hud::refresh_prompt,
                hud::toast,
            )
                .chain(),
        )
        .add_systems(OnEnter(GameFlow::Paused), |world: &mut World| {
            hud::show_pause(world, true)
        })
        .add_systems(OnExit(GameFlow::Paused), |world: &mut World| {
            hud::show_pause(world, false)
        });
    }
}

/// Run condition: the game is in `GameFlow::Playing` (not paused).
fn playing(flow: Option<Res<State<GameFlow>>>) -> bool {
    flow.is_none_or(|flow| *flow.get() == GameFlow::Playing)
}

/// `Startup`: names the level in every save's slot info.
fn set_save_level(mut info: ResMut<SaveInfo>) {
    info.level = "main".into();
}

/// `Startup`: a player pawn at eye height and the first-person camera welded to it.
fn spawn_player(mut commands: Commands) {
    let player = commands
        .spawn((
            regia_scene::components::Name::new("player"),
            Player,
            Transform::from_translation(SPAWN),
            GlobalTransform::default(),
        ))
        .id();
    let rig = CameraRig {
        mode: CameraMode::FirstPerson(FirstPerson {
            eye_offset: Vec3::ZERO, // the pawn already sits at eye height
            bob: 0.0,
        }),
        pitch: 0.0,
        yaw: 0.0,
        ..CameraRig::default()
    };
    commands.spawn((
        regia_scene::components::Name::new("main_camera"),
        Transform::default(),
        GlobalTransform::default(),
        rig,
        ChaseCamera { target: player },
        Camera::default(),
    ));
}

/// `Update`: moves the pawn in the camera's yaw frame (sliding along walls and stopping at a
/// closed gate) and steers the rig from look input.
#[tracing::instrument(skip_all, name = "move_player")]
fn move_player(
    actions: Res<Actions>,
    time: Res<Time>,
    controls: Res<controls::ControlsOpen>,
    settings: Option<Res<GameSettings>>,
    solids: Query<(&Transform, &level::Solid), Without<Player>>,
    mut pawn: Query<&mut Transform, With<Player>>,
    mut rigs: Query<&mut CameraRig>,
) {
    if controls.0 {
        return;
    }
    let dt = time.delta_secs();
    let move_axes = actions.axis2d("Move");
    let (sensitivity, invert) = settings.map_or((1.0, false), |s| (s.input.sensitivity, s.input.invert_y));
    // The bindings carry the units (rad per pixel, rad per second); only the user setting scales here.
    let look = actions.axis2d("Look") * sensitivity;

    // Snapshot the current yaw before mutating.
    let yaw = rigs.iter().next().map_or(0.0, |rig| rig.yaw);
    let forward = Vec3::new(-yaw.sin(), 0.0, -yaw.cos());
    let right = Vec3::new(yaw.cos(), 0.0, -yaw.sin());
    let step = (forward * move_axes.y + right * move_axes.x).normalize_or_zero() * WALK_SPEED * dt;

    // Collision follows the current slab, including a gate that is still moving.
    let active: Vec<(&Transform, &level::Solid)> = solids.iter().collect();
    for mut transform in &mut pawn {
        // Each axis on its own, so the player slides along a wall instead of stopping dead.
        for axis in [Vec3::new(step.x, 0.0, 0.0), Vec3::new(0.0, 0.0, step.z)] {
            let wanted = transform.translation + axis;
            if !level::blocked(wanted, active.iter().copied()) {
                transform.translation = wanted;
            }
        }
    }
    for mut rig in &mut rigs {
        rig.yaw = wrap(rig.yaw - look.x);
        let pitch_delta = if invert { -look.y } else { look.y };
        rig.pitch = (rig.pitch + pitch_delta).clamp(-1.4, 1.4);
    }
}

/// Copies the pawn's position and view into the saved checkpoint.
fn track_checkpoint(
    pawns: Query<&Transform, With<Player>>,
    rigs: Query<&CameraRig>,
    mut checkpoint: ResMut<PlayerCheckpoint>,
) {
    if let Some(transform) = pawns.iter().next() {
        checkpoint.position = transform.translation;
    }
    if let Some(rig) = rigs.iter().next() {
        checkpoint.view_yaw = rig.yaw;
        checkpoint.view_pitch = rig.pitch;
    }
}

/// After a successful load, moves the pawn and the view back to the restored checkpoint.
/// (`hud::toast` shows the "Saved" / "Loaded" toast from the same outcomes.)
fn restore_checkpoint(
    mut outcomes: MessageReader<SaveOutcome>,
    checkpoint: Res<PlayerCheckpoint>,
    mut pawns: Query<&mut Transform, With<Player>>,
    mut rigs: Query<&mut CameraRig>,
) {
    let loaded = outcomes
        .read()
        .any(|outcome| outcome.op == SaveOp::Load && outcome.result.is_ok());
    if !loaded {
        return;
    }
    for mut transform in &mut pawns {
        transform.translation = checkpoint.position;
    }
    for mut rig in &mut rigs {
        rig.yaw = checkpoint.view_yaw;
        rig.pitch = checkpoint.view_pitch;
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
    fn angles_wrap_into_half_turns() {
        assert!((wrap(3.0 * std::f32::consts::PI) - std::f32::consts::PI).abs() < 1e-5);
        assert!((wrap(-0.5) + 0.5).abs() < 1e-6);
    }
}

// REGIA:SCRIPTS
