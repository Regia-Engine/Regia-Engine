// SPDX-License-Identifier: MIT-0
//! Input: keyboard + mouse for player 0, gamepads for joining players. Writes each
//! character's `CharacterInput` and its camera rig's yaw/pitch.

use regia::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_framework::camera::CameraRig;
use regia_framework::character::CharacterInput;
use regia_framework::player::{Player, PlayerController, PlayerManager};
use regia_platform::{ButtonState, KeyboardInput, MouseMotion};

/// Look deltas this frame, per player index (mouse for player 0, right stick for pads).
#[derive(Resource, Debug, Default, Clone)]
pub struct LookInput {
    /// Yaw/pitch delta in radians per player.
    pub deltas: std::collections::BTreeMap<u8, Vec2>,
}

/// Set by `--split`: join a second local player once player 0 exists.
#[derive(Resource, Debug, Default, Clone, Copy)]
pub struct SplitOnStart(pub bool);

/// Movement keys currently held (player 0).
#[derive(Resource, Debug, Default, Clone)]
pub struct ActionKeys {
    /// Physical key names currently held.
    pub held: std::collections::HashSet<String>,
}

/// Persistent read cursors for the platform message streams (cursor reads see both
/// double-buffer halves, so events written any time since the last read are delivered —
/// exactly once).
#[derive(Resource, Debug, Default)]
pub struct InputCursors {
    /// Keyboard events.
    pub(crate) keyboard: regia_ecs::ecs::message::MessageCursor<regia_platform::KeyboardInput>,
    /// Mouse motion.
    pub(crate) mouse: regia_ecs::ecs::message::MessageCursor<regia_platform::MouseMotion>,
    /// UI interaction events.
    pub(crate) ui: regia_ecs::ecs::message::MessageCursor<regia_ui::interaction::UiEvent>,
    /// Mouse buttons (the left button attacks, recorded as `MouseLeft` in [`ActionKeys`]).
    pub(crate) mouse_buttons: regia_ecs::ecs::message::MessageCursor<regia_platform::MouseButtonInput>,
}

/// The character `player` possesses, if any.
pub fn possessed(world: &World, player: u8) -> Option<Entity> {
    let player_entity = world.get_resource::<PlayerManager>()?.player_entity(player)?;
    let controller = world.get::<Player>(player_entity)?.controller;
    let character = world.get::<PlayerController>(controller)?.possessed;
    (character != Entity::PLACEHOLDER).then_some(character)
}

/// The player possessing `character`, if any.
pub fn player_of(world: &World, character: Entity) -> Option<u8> {
    let manager = world.get_resource::<PlayerManager>()?;
    manager
        .iter()
        .map(|(index, _)| index)
        .find(|index| possessed(world, *index) == Some(character))
}

/// Mouse look sensitivity (radians per pixel).
const SENSITIVITY: f32 = 0.0035;
/// Right-stick look sensitivity (radians per unit of axis).
const STICK_SENSITIVITY: f32 = 0.05;

/// Key edges: pause, save/load, join, and held-key bookkeeping.
#[tracing::instrument(skip_all, target = "third_person", name = "sample_keys")]
pub fn handle_global_keys(world: &mut World) {
    if !world.contains_resource::<Messages<KeyboardInput>>() {
        return; // no platform plugin (headless tests)
    }
    let mut cursors = world.remove_resource::<InputCursors>().unwrap_or_default();
    let events: Vec<KeyboardInput> = world
        .get_resource::<Messages<KeyboardInput>>()
        .map(|messages| cursors.keyboard.read(messages).cloned().collect())
        .unwrap_or_default();
    let buttons: Vec<regia_platform::MouseButtonInput> = world
        .get_resource::<Messages<regia_platform::MouseButtonInput>>()
        .map(|messages| cursors.mouse_buttons.read(messages).cloned().collect())
        .unwrap_or_default();
    world.insert_resource(cursors);
    for button in buttons {
        if button.button == regia_platform::MouseButton::Left {
            let mut keys = world.resource_mut::<ActionKeys>();
            match button.state {
                ButtonState::Pressed => keys.held.insert("MouseLeft".to_owned()),
                ButtonState::Released => keys.held.remove("MouseLeft"),
            };
        }
    }
    for key in events {
        if key.repeat {
            continue;
        }
        {
            let mut keys = world.resource_mut::<ActionKeys>();
            match key.state {
                ButtonState::Pressed => {
                    keys.held.insert(key.physical_key.clone());
                }
                ButtonState::Released => {
                    keys.held.remove(&key.physical_key);
                }
            }
        }
        // The engine pause (Pause/Break, `[game] pause` in Regia.toml, ADR-10104) freezes the game:
        // the sample's own keys wait for the resume. Held keys above are still tracked.
        if key.state == ButtonState::Pressed && !engine_paused(world) {
            match key.physical_key.as_str() {
                "Escape" => crate::hud::toggle_pause(world),
                "F5" => crate::save::save_now(world),
                "F9" => crate::save::load_now(world),
                "F2" => join_second_player(world),
                "KeyE" => {
                    crate::car::toggle(world, 0);
                }
                "KeyT" => {
                    crate::npc::interact(world);
                }
                "Digit1" => {
                    crate::npc::answer(world, 0);
                }
                "Digit2" => {
                    crate::npc::answer(world, 1);
                }
                "KeyL" => {
                    crate::npc::toggle_locale(world);
                }
                "F6" => crate::demo_fx::toggle_record(world),
                "F7" => crate::demo_fx::toggle_play(world),
                "F8" => crate::demo_fx::toggle_scrub_bar(world),
                "BracketLeft" => crate::demo_fx::scrub(world, -1),
                "BracketRight" => crate::demo_fx::scrub(world, 1),
                _ => {}
            }
        }
    }
}

/// Whether the engine's game pause holds the game (the Pause/Break key; ADR-10104). This sample
/// reads raw keys and mouse motion, which the pause's input contexts do not cover, so it checks.
pub fn engine_paused(world: &World) -> bool {
    world
        .get_resource::<regia_framework::game_pause::GamePause>()
        .is_some_and(regia_framework::game_pause::GamePause::is_paused)
}

/// `Update`: the engine pause screen's Quit to menu goes back to the title screen (ADR-10104).
pub fn quit_to_menu(
    world: &mut World,
    mut cursor: Local<regia_ecs::ecs::message::MessageCursor<regia_framework::game_pause::PauseScreenEvent>>,
) {
    let quit = world
        .get_resource::<Messages<regia_framework::game_pause::PauseScreenEvent>>()
        .is_some_and(|messages| {
            cursor
                .read(messages)
                .any(|e| *e == regia_framework::game_pause::PauseScreenEvent::QuitToMenu)
        });
    if quit && let Some(mut next) = world.get_resource_mut::<NextState<regia_framework::flow::GameFlow>>() {
        next.set(regia_framework::flow::GameFlow::MainMenu);
    }
}

/// Joins player 2 when [`SplitOnStart`] is set (the windowed `--split` flag).
#[tracing::instrument(skip_all, target = "third_person", name = "split_on_start")]
pub fn join_if_requested(world: &mut World) {
    let requested = world.get_resource::<SplitOnStart>().is_some_and(|flag| flag.0);
    if !requested {
        return;
    }
    let count = world
        .get_resource::<PlayerManager>()
        .map(|m| m.count())
        .unwrap_or(0);
    if count == 0 {
        return; // player 0 is spawned by `spawn_for_players` first
    }
    if count >= 2 {
        if let Some(mut flag) = world.get_resource_mut::<SplitOnStart>() {
            flag.0 = false;
        }
        return;
    }
    join_second_player(world);
}

/// Spawns a second local player (keyboard-split) so split-screen is testable without a
/// gamepad; gamepad joins go through the framework's `detect_local_join`.
fn join_second_player(world: &mut World) {
    let can_join = world
        .get_resource::<PlayerManager>()
        .is_some_and(|m| m.can_join());
    if !can_join {
        return;
    }
    let spawned = PlayerManager::spawn_player_static(world, regia_framework::player::PlayerKind::Local);
    if let Some((player_entity, _controller)) = spawned {
        let index = world.get::<Player>(player_entity).map(|p| p.index).unwrap_or(1);
        let _ = crate::character::spawn_player_character(world, index);
        tracing::info!(target: "third_person", "player {index} joined on keyboard");
    }
}

/// Per-frame: player 0's held keys + mouse deltas, pad axes for other players → each
/// character's `CharacterInput` and camera rig angles. Movement is camera-relative.
#[tracing::instrument(skip_all, target = "third_person", name = "sample_input")]
pub fn drive_input(world: &mut World) {
    // --- gather raw input ----------------------------------------------------
    let mouse: Vec2 = {
        if !world.contains_resource::<Messages<MouseMotion>>() {
            Vec2::ZERO
        } else {
            let mut cursors = world.remove_resource::<InputCursors>().unwrap_or_default();
            let delta = world
                .get_resource::<Messages<MouseMotion>>()
                .map(|messages| {
                    cursors.mouse.read(messages).fold(Vec2::ZERO, |acc, e| {
                        Vec2::new(acc.x + e.delta.0 as f32, acc.y + e.delta.1 as f32)
                    })
                })
                .unwrap_or_default();
            world.insert_resource(cursors);
            delta
        }
    };
    let held: Vec<String> = world.resource::<ActionKeys>().held.iter().cloned().collect();
    let key_axes = {
        let x = (held.iter().any(|k| k == "KeyD") as i32 - held.iter().any(|k| k == "KeyA") as i32) as f32;
        let y = (held.iter().any(|k| k == "KeyW") as i32 - held.iter().any(|k| k == "KeyS") as i32) as f32;
        Vec2::new(x, y)
    };
    let key_jump = held.iter().any(|k| k == "Space");
    let key_sprint = held.iter().any(|k| k == "ShiftLeft" || k == "ShiftRight");
    // Look settings (the settings menu edits them live).
    let (sensitivity, invert) = world
        .get_resource::<regia_framework::settings::GameSettings>()
        .map_or((1.0, false), |s| (s.input.sensitivity, s.input.invert_y));
    let pitch_sign = if invert { -1.0 } else { 1.0 };
    // No control behind the title screen.
    let on_title = world
        .get_resource::<State<regia_framework::flow::GameFlow>>()
        .is_some_and(|s| *s.get() == regia_framework::flow::GameFlow::MainMenu)
        || engine_paused(world);

    // --- per player ----------------------------------------------------------
    let players: Vec<(u8, Entity)> = world
        .get_resource::<PlayerManager>()
        .map(|m| m.iter().collect())
        .unwrap_or_default();
    for (index, player_entity) in players {
        let Some(player_data) = world.get::<Player>(player_entity).cloned() else {
            continue;
        };
        let Some(controller) = world.get::<PlayerController>(player_data.controller).cloned() else {
            continue;
        };
        let character = controller.possessed;
        if character == Entity::PLACEHOLDER {
            continue;
        }

        // Input axes: keyboard for player 0, the player's pad for others.
        let (axes, jump, sprint, look) = if index == 0 {
            (
                key_axes,
                key_jump,
                key_sprint,
                Vec2::new(
                    mouse.x * SENSITIVITY * sensitivity,
                    -mouse.y * SENSITIVITY * sensitivity * pitch_sign,
                ),
            )
        } else if let Some(device) = player_data.device
            && let Some(pad) = world
                .get_resource::<regia_input::Gamepads>()
                .and_then(|g| g.pad(device))
        {
            let lx = pad.axis(regia_platform::GamepadAxis::LeftStickX);
            let ly = pad.axis(regia_platform::GamepadAxis::LeftStickY);
            let rx = pad.axis(regia_platform::GamepadAxis::RightStickX);
            let ry = pad.axis(regia_platform::GamepadAxis::RightStickY);
            (
                Vec2::new(lx, ly),
                pad.pressed(regia_platform::GamepadButton::South),
                pad.pressed(regia_platform::GamepadButton::RightTrigger),
                Vec2::new(
                    rx * STICK_SENSITIVITY * sensitivity,
                    -ry * STICK_SENSITIVITY * sensitivity * pitch_sign,
                ),
            )
        } else {
            continue;
        };

        // Down (dead, waiting to respawn), on the title screen or engine-paused: no movement.
        if on_title || crate::combat::is_down(world, character) {
            if let Some(mut input) = world.get_mut::<CharacterInput>(character) {
                *input = CharacterInput::default();
            }
            continue;
        }

        // In a car the move axes drive it (ADR-2134); the character stands still.
        if crate::car::drive(world, index, character, axes, jump, look) {
            continue;
        }

        // This player's camera rig (yaw defines the movement basis).
        let mut rig_yaw = 0.0f32;
        let mut rig_entity = None;
        {
            let mut query = world.query_filtered::<(Entity, &CameraRig, &crate::camera::PlayerCamera), ()>();
            for (entity, rig, cam) in query.iter(world) {
                if cam.player == index {
                    rig_yaw = rig.yaw;
                    rig_entity = Some(entity);
                    break;
                }
            }
        }

        // Camera-relative movement.
        let forward = Vec3::new(-rig_yaw.sin(), 0.0, -rig_yaw.cos());
        let right = Vec3::new(-forward.z, 0.0, forward.x);
        let move_dir = (forward * axes.y + right * axes.x).normalize_or_zero();

        if let Some(mut input) = world.get_mut::<CharacterInput>(character) {
            input.move_dir = move_dir;
            input.jump = jump;
            input.sprint = sprint;
        }
        if let Some(entity) = rig_entity
            && let Some(mut rig) = world.get_mut::<CameraRig>(entity)
        {
            rig.yaw -= look.x;
            rig.pitch = (rig.pitch + look.y).clamp(-1.2, 1.2);
        }
    }

    let _ = world.resource_mut::<LookInput>();
}
