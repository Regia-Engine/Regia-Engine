// SPDX-License-Identifier: MIT-0
//! {{name}} gameplay: a third-person starter (Third Person template).
//!
//! Everything the engine's third-person sample shows, rewritten for a fresh project:
//!
//! - `character`: a rapier-driven capsule character with a procedural walk cycle and
//!   ray-cast foot IK, and the boom camera that shortens on collision.
//! - `level`: ground, blocks and a flight of stairs.
//! - `collectible`: pickups with a saved progress resource.
//! - `hud`: the coin counter, health and enemy counts, a save/load toast and the Esc pause menu.
//! - `combat`: health, damage events and the three-hit sword combo.
//! - `enemy`: three grunts (idle, chase, wind-up, strike, flinch, topple) and the saved list of
//!   the defeated.
//! - `respawn`: death, the "You died" countdown and the respawn.
//! - `menu`: the title screen and the settings panel (volumes, sensitivity, graphics preset).
//! - `sound`: synthesized sound effects behind `SoundCue` messages, and an ambient bed.
//! - This file: input wiring and the save (spec 91): F5 quicksaves, F9 quickloads, and the
//!   player's position rides in `PlayerCheckpoint`.
//!
//! `README.md` walks through the parts and what to replace first.

use regia::physics::PhysicsPlugin;
use regia::prelude::*;
use regia_core::math::Vec3;
use regia_framework::camera::{CameraPlugin, CameraRig};
use regia_framework::character::{
    Character, CharacterInput, CharacterPlugin, CharacterState, use_rapier_character_controller,
};
use regia_framework::flow::{GameFlow, GameFlowPlugin, GameFlowSettings};
use regia_framework::save_flow::{SaveFlowPlugin, SaveInfo, SaveOp, SaveOutcome};
use regia_framework::save_game::{RegisterSaveableExt, SavePlugin, SaveableResource};
use regia_framework::script::ScriptPlugin;
use regia_framework::settings::SettingsPlugin;
use regia_input::prelude::*;
use regia_render::Environment;
use regia_scene::Transform;

/// Mouse look sensitivity (radians per pixel).
const MOUSE_SENSITIVITY: f32 = 0.003;
/// Right-stick look rate at full deflection (radians per second).
const STICK_TURN_RATE: f32 = 3.0;

pub mod character;
pub mod collectible;
pub mod combat;
pub mod enemy;
pub mod hud;
pub mod level;
pub mod menu;
pub mod respawn;
pub mod sound;

/// The template's game plugin.
pub struct Game;

/// Where the player stands, kept in every save. Copied from the character each frame; after a
/// load the character is moved back to it.
///
/// A load respawns `Saveable` entities from their reflected components only, and meshes and
/// physics bodies are not reflected, so the player entity itself is not marked `Saveable`.
/// Add the rest of your player state here (health, inventory, ...); it is saved with the game.
#[derive(Resource, Reflect, Default, Clone, Debug, PartialEq)]
#[reflect(Resource, Default)]
pub struct PlayerCheckpoint {
    /// The character's position.
    pub position: Vec3,
    /// The boom's yaw (radians).
    pub view_yaw: f32,
}

impl SaveableResource for PlayerCheckpoint {}

impl Plugin for Game {
    fn build(&self, app: &mut App) {
        let config = InputConfig::new()
            .action("Move", ActionKind::Axis2D)
            .action("Look", ActionKind::Axis2D)
            .action("Jump", ActionKind::Button)
            .action("QuickSave", ActionKind::Button)
            .action("QuickLoad", ActionKind::Button)
            .action("Pause", ActionKind::Button)
            .action("Attack", ActionKind::Button)
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
            .binding(Binding::new("Jump", InputSource::key("Space")))
            .binding(Binding::new(
                "Jump",
                InputSource::gamepad_button(GamepadButton::South),
            ))
            .binding(Binding::new("QuickSave", InputSource::key("F5")))
            .binding(Binding::new("QuickLoad", InputSource::key("F9")))
            .binding(Binding::new("Pause", InputSource::key("Escape")))
            .binding(Binding::new("Attack", InputSource::key("KeyJ")))
            .binding(Binding::new(
                "Attack",
                InputSource::mouse_button(MouseButton::Left),
            ))
            .binding(Binding::new(
                "Attack",
                InputSource::gamepad_button(GamepadButton::West),
            ));

        // The title screen shows unless the host skipped it (tests and the agent smoke do).
        if !app.world().contains_resource::<GameFlowSettings>() {
            app.insert_resource(GameFlowSettings {
                skip_main_menu: false,
                ..Default::default()
            });
        }
        app.add_plugins((
            regia_scene::ScenePlugin,
            PhysicsPlugin,
            CharacterPlugin,
            CameraPlugin,
            ScriptPlugin,
            SavePlugin::default(),
            SaveFlowPlugin,
            InputPlugin::new(config),
        ))
        // A plugin tuple holds at most eight plugins; the rest go in their own call.
        .add_plugins((
            GameFlowPlugin,
            SettingsPlugin,
            sound::SoundPlugin,
            // Startup playback of a `.frec` recording for packaged smoke runs (ADR-3968).
            regia_framework::startup_playback::StartupPlaybackPlugin,
        ));
        use_rapier_character_controller(app);
        app.register_saveable_resource::<PlayerCheckpoint>()
            .register_saveable_resource::<collectible::CollectProgress>()
            .register_type::<collectible::CollectibleId>()
            .register_type::<hud::HudLabel>()
            .register_type::<hud::ToastLabel>()
            .register_type::<hud::CombatLabel>()
            .register_type::<hud::EnemiesLabel>()
            .register_type::<hud::BannerLabel>()
            .register_type::<combat::Health>()
            .register_type::<combat::Fighter>()
            .register_type::<enemy::EnemyId>()
            .register_type::<respawn::Respawn>()
            .register_type::<menu::MenuState>()
            .register_saveable_resource::<enemy::CombatProgress>()
            .add_message::<combat::DamageEvent>()
            .add_message::<combat::Died>()
            .init_resource::<respawn::Respawn>()
            .init_resource::<enemy::CombatProgress>()
            .init_resource::<PlayerCheckpoint>()
            .init_resource::<collectible::CollectProgress>()
            .init_resource::<hud::MenuCursor>()
            .init_resource::<Environment>()
            .add_systems(
                Startup,
                (
                    level::setup_level,
                    character::setup_character,
                    character::setup_camera,
                    enemy::spawn_all,
                    hud::setup_hud,
                    set_save_level,
                )
                    .chain(),
            )
            .add_systems(
                Update,
                (
                    write_character_input.run_if(playing),
                    write_camera_look.run_if(playing),
                    // Restore first: a load applied in `Last` is seen here a frame later, and
                    // tracking before restoring would copy the live pose over the loaded one.
                    restore_checkpoint,
                    track_checkpoint,
                    character::advance_phase,
                    character::animate_rig,
                    collectible::collect_nearby,
                    collectible::reconcile,
                    collectible::spin,
                    hud::toggle_pause,
                    hud::menu_clicks,
                    menu::menu_clicks,
                    menu::sync_menus,
                    hud::refresh_counter,
                    hud::refresh_combat,
                    hud::refresh_banner,
                    hud::toast,
                )
                    .chain(),
            )
            // The fight. Gameplay waits for `GameFlow::Playing` (not the title screen, not pause).
            .add_systems(
                Update,
                (
                    combat::tick_fighter,
                    combat::player_attack.run_if(playing),
                    enemy::think.run_if(playing),
                    enemy::react_to_hits,
                    combat::apply_damage,
                    enemy::handle_deaths,
                    respawn::on_player_death,
                    respawn::run_respawn,
                    enemy::topple_corpses,
                    enemy::reconcile,
                    combat::animate_sword,
                    track_health,
                )
                    .chain()
                    .after(restore_checkpoint),
            )
            .add_systems(OnEnter(GameFlow::Paused), |world: &mut World| {
                hud::show_pause(world, true)
            })
            .add_systems(OnExit(GameFlow::Paused), |world: &mut World| {
                hud::show_pause(world, false)
            });
    }
}

/// Run condition: the game is in `GameFlow::Playing` (not on the title screen, not paused).
fn playing(flow: Option<Res<State<GameFlow>>>) -> bool {
    flow.is_none_or(|flow| *flow.get() == GameFlow::Playing)
}

/// `Startup`: names the level in every save's slot info.
fn set_save_level(mut info: ResMut<SaveInfo>) {
    info.level = "main".into();
}

/// WASD / left stick → `CharacterInput`, relative to the boom camera's yaw (the stick
/// points where the character runs on screen; the character then turns to face it).
fn write_character_input(
    actions: Res<Actions>,
    rigs: Query<&CameraRig>,
    characters: Query<Entity, With<Character>>,
    mut inputs: Query<&mut CharacterInput>,
    respawn: Res<respawn::Respawn>,
) {
    let down = respawn::is_down(&respawn);
    let axes = actions.axis2d("Move");
    let yaw = rigs.iter().next().map_or(0.0, |rig| rig.yaw);
    let forward = (regia_core::math::Quat::from_rotation_y(yaw) * -Vec3::Z)
        .with_y(0.0)
        .normalize_or_zero();
    let right = (regia_core::math::Quat::from_rotation_y(yaw) * Vec3::X)
        .with_y(0.0)
        .normalize_or_zero();
    for entity in &characters {
        let Ok(mut input) = inputs.get_mut(entity) else {
            continue;
        };
        input.move_dir = if down {
            Vec3::ZERO
        } else {
            (forward * axes.y + right * axes.x).clamp_length_max(1.0)
        };
        input.jump = !down && actions.pressed("Jump");
    }
}

/// Mouse / right stick → boom yaw and pitch (the bindings carry rad per pixel / rad per second).
fn write_camera_look(actions: Res<Actions>, mut rigs: Query<&mut CameraRig>) {
    let look = actions.axis2d("Look");
    for mut rig in &mut rigs {
        rig.yaw -= look.x;
        rig.pitch = (rig.pitch + look.y).clamp(-1.2, 1.2);
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

/// Copies the player's health into the saved [`enemy::CombatProgress`] (only when it changed, so
/// the progress is not marked changed every frame).
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

/// After a successful load, moves the player back to the restored checkpoint.
/// (`hud::toast` shows the "Saved" / "Loaded" toast from the same outcomes.)
fn restore_checkpoint(
    mut outcomes: MessageReader<SaveOutcome>,
    checkpoint: Res<PlayerCheckpoint>,
    progress: Res<enemy::CombatProgress>,
    mut respawn: ResMut<respawn::Respawn>,
    mut characters: Query<(&mut Transform, &mut CharacterState, &mut combat::Health), With<Character>>,
    mut rigs: Query<&mut CameraRig>,
) {
    let loaded = outcomes
        .read()
        .any(|outcome| outcome.op == SaveOp::Load && outcome.result.is_ok());
    if !loaded {
        return;
    }
    respawn.left = None;
    for (mut transform, mut state, mut health) in &mut characters {
        transform.translation = checkpoint.position;
        state.velocity = Vec3::ZERO;
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
