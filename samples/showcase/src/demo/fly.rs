// SPDX-License-Identifier: MIT-0
//! The free-fly camera: the engine's spectator rig (`regia_framework::camera`, [`CameraMode::Free`]) on the
//! showcase camera, flown with the keyboard and mouse or a gamepad, and the demo's hotkeys.
//!
//! | Input | Keyboard and mouse | Gamepad |
//! |---|---|---|
//! | Move | W A S D | left stick |
//! | Look | hold the right mouse button and move | right stick |
//! | Up / down | E or Space / Q or Ctrl | right / left shoulder or trigger |
//! | Faster | Shift | left stick click |
//! | Panel, credits | F1, F2 | Select (View) toggles the panel |
//! | Screenshot | F12 | North (Y / Triangle) |
//! | Benchmark, stop | F9, Esc | Start |
//!
//! Every frame the device input becomes one [`FreeLookInput`] ([`free_look`], a pure function); an agent's
//! `demo.fly` replaces it for a number of frames ([`FlyOverride`]). During a benchmark the camera follows the
//! views and the input is ignored.

use regia::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_framework::camera::{CameraMode, CameraPlugin, CameraRig, Free, FreeLookInput};
use regia_input::prelude::*;
use regia_platform::{ButtonState, KeyboardInput};
use regia_render::Camera;
use regia_scene::Transform;

use super::{DemoCommand, DemoPage, DemoQueue, DemoState};

/// Look speed of the mouse, radians per pixel of motion.
pub const MOUSE_RADIANS_PER_PIXEL: f32 = 0.0025;
/// Look speed of a fully pushed right stick, radians per second.
pub const PAD_RADIANS_PER_SECOND: f32 = 2.2;
/// The camera's cruising speed, meters per second (Shift or the stick click multiplies it by
/// [`SPRINT_MULTIPLIER`]).
pub const FLY_SPEED_M_PER_S: f32 = 20.0;
/// The sprint multiplier.
pub const SPRINT_MULTIPLIER: f32 = 4.0;
/// Most frames one `demo.fly` call may hold its input for.
pub const MAX_FLY_FRAMES: u32 = 600;

/// Action names the demo binds (prefixed so a host's own config does not collide).
pub mod actions {
    /// Move: x strafe right, y forward.
    pub const MOVE: &str = "demo_move";
    /// Mouse look (pixels this frame).
    pub const LOOK_MOUSE: &str = "demo_look_mouse";
    /// Stick look (`-1..=1`).
    pub const LOOK_PAD: &str = "demo_look_pad";
    /// Hold to look with the mouse.
    pub const MOUSE_LOOK: &str = "demo_mouse_look";
    /// Fly up.
    pub const UP: &str = "demo_up";
    /// Fly down.
    pub const DOWN: &str = "demo_down";
    /// Fly faster.
    pub const SPRINT: &str = "demo_sprint";
    /// Gamepad: toggle the panel.
    pub const PANEL: &str = "demo_panel";
    /// Gamepad: screenshot.
    pub const SCREENSHOT: &str = "demo_screenshot";
    /// Gamepad: benchmark.
    pub const BENCHMARK: &str = "demo_benchmark";
}

/// The demo's input bindings.
#[must_use]
pub fn input_config() -> InputConfig {
    use actions as a;
    InputConfig::new()
        .action(a::MOVE, ActionKind::Axis2D)
        .action(a::LOOK_MOUSE, ActionKind::Axis2D)
        .action(a::LOOK_PAD, ActionKind::Axis2D)
        .action(a::MOUSE_LOOK, ActionKind::Button)
        .action(a::UP, ActionKind::Button)
        .action(a::DOWN, ActionKind::Button)
        .action(a::SPRINT, ActionKind::Button)
        .action(a::PANEL, ActionKind::Button)
        .action(a::SCREENSHOT, ActionKind::Button)
        .action(a::BENCHMARK, ActionKind::Button)
        .binding(Binding::new(
            a::MOVE,
            InputSource::keys2d("KeyW", "KeyS", "KeyA", "KeyD"),
        ))
        .binding(Binding::new(a::MOVE, InputSource::gamepad_stick(Stick::Left)))
        .binding(Binding::new(a::LOOK_MOUSE, InputSource::MouseDelta))
        .binding(Binding::new(
            a::LOOK_PAD,
            InputSource::gamepad_stick(Stick::Right),
        ))
        .binding(Binding::new(
            a::MOUSE_LOOK,
            InputSource::mouse_button(MouseButton::Right),
        ))
        .binding(Binding::new(a::UP, InputSource::key("KeyE")))
        .binding(Binding::new(a::UP, InputSource::key("Space")))
        .binding(Binding::new(
            a::UP,
            InputSource::gamepad_button(GamepadButton::RightShoulder),
        ))
        .binding(Binding::new(
            a::UP,
            InputSource::gamepad_button(GamepadButton::RightTrigger),
        ))
        .binding(Binding::new(a::DOWN, InputSource::key("KeyQ")))
        .binding(Binding::new(a::DOWN, InputSource::key("ControlLeft")))
        .binding(Binding::new(
            a::DOWN,
            InputSource::gamepad_button(GamepadButton::LeftShoulder),
        ))
        .binding(Binding::new(
            a::DOWN,
            InputSource::gamepad_button(GamepadButton::LeftTrigger),
        ))
        .binding(Binding::new(a::SPRINT, InputSource::key("ShiftLeft")))
        .binding(Binding::new(a::SPRINT, InputSource::key("ShiftRight")))
        .binding(Binding::new(
            a::SPRINT,
            InputSource::gamepad_button(GamepadButton::LeftStick),
        ))
        .binding(Binding::new(
            a::PANEL,
            InputSource::gamepad_button(GamepadButton::Select),
        ))
        .binding(Binding::new(
            a::SCREENSHOT,
            InputSource::gamepad_button(GamepadButton::North),
        ))
        .binding(Binding::new(
            a::BENCHMARK,
            InputSource::gamepad_button(GamepadButton::Start),
        ))
}

/// One frame of device input, read from the demo's actions.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FlySticks {
    /// Move axes (keys and left stick, summed): x strafe right, y forward.
    pub move_axes: Vec2,
    /// Mouse motion this frame, pixels (x right, y down).
    pub mouse: Vec2,
    /// Right stick, `-1..=1` (x right, y up).
    pub look_pad: Vec2,
    /// The right mouse button is held (mouse look).
    pub mouse_look: bool,
    /// Fly up held.
    pub up: bool,
    /// Fly down held.
    pub down: bool,
    /// Faster held.
    pub sprint: bool,
}

/// The free camera's input for one frame of `dt` seconds: mouse look only while the right button is held, the
/// stick's look scaled by time, the move axes clamped to the unit circle.
#[must_use]
pub fn free_look(sticks: &FlySticks, dt: f32) -> FreeLookInput {
    let dt = if dt.is_finite() { dt.clamp(0.0, 0.25) } else { 0.0 };
    let mut look = sticks.look_pad * (PAD_RADIANS_PER_SECOND * dt);
    if sticks.mouse_look {
        // Mouse right turns right (the rig's yaw decreases), mouse down looks down.
        look += Vec2::new(sticks.mouse.x, -sticks.mouse.y) * MOUSE_RADIANS_PER_PIXEL;
    }
    let mut move_axes = sticks.move_axes;
    if move_axes.length_squared() > 1.0 {
        move_axes = move_axes.normalize_or_zero();
    }
    let rise = f32::from(u8::from(sticks.up)) - f32::from(u8::from(sticks.down));
    FreeLookInput {
        look,
        move_axes,
        rise,
        sprint: sticks.sprint,
    }
}

/// An agent's flight input (`demo.fly`): used instead of the devices for `frames` more frames.
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct FlyOverride {
    /// The input each frame.
    pub input: FreeLookInput,
    /// Frames left.
    pub frames: u32,
}

/// Marks the camera the demo flies.
#[derive(Component, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DemoCamera;

/// The rig's yaw and pitch for a camera rotation (the rig's convention: yaw 0 faces −Z, positive towards −X;
/// pitch positive looks up).
#[must_use]
pub fn yaw_pitch(rotation: Quat) -> (f32, f32) {
    let forward = rotation * Vec3::NEG_Z;
    let yaw = (-forward.x).atan2(-forward.z);
    let pitch = forward.y.clamp(-1.0, 1.0).asin();
    (yaw, pitch)
}

/// Adds the camera plugin and the demo's input bindings (unless the app has an input plugin already, whose
/// config then lacks the demo's actions: the keys and the agent still work) and the flight systems.
pub fn install(app: &mut App) {
    if !app.has_plugin(std::any::type_name::<CameraPlugin>()) {
        app.add_plugins(CameraPlugin);
    }
    if !app.has_plugin(std::any::type_name::<InputPlugin>()) {
        app.add_plugins(InputPlugin::new(input_config()));
    }
    app.add_systems(Update, (attach_rig, drive).chain());
}

/// Cameras that have no rig yet.
type Unrigged<'w, 's> =
    Query<'w, 's, (Entity, &'static Transform), (With<Camera>, Without<CameraRig>, Without<DemoCamera>)>;

/// `Update`: puts the free rig on the showcase camera once it exists, facing where the scene's camera faces.
fn attach_rig(mut commands: Commands, cameras: Unrigged) {
    for (entity, transform) in &cameras {
        let (yaw, pitch) = yaw_pitch(transform.rotation);
        commands.entity(entity).insert((
            DemoCamera,
            CameraRig {
                mode: CameraMode::Free(Free {
                    speed: FLY_SPEED_M_PER_S,
                    sprint_multiplier: SPRINT_MULTIPLIER,
                    accel: 8.0,
                }),
                pitch,
                yaw,
                ..CameraRig::default()
            },
        ));
    }
}

/// `Update`: this frame's [`FreeLookInput`] from the agent's override, else the devices; zero while a
/// benchmark runs.
fn drive(
    time: Res<Time>,
    input: Option<Res<Actions>>,
    state: Res<DemoState>,
    mut fly: Option<ResMut<FlyOverride>>,
    mut free: ResMut<FreeLookInput>,
    mut commands: Commands,
) {
    let _span = tracing::info_span!("demo_fly").entered();
    if state.bench.running() {
        *free = FreeLookInput::default();
        return;
    }
    if let Some(over) = fly.as_mut() {
        *free = over.input;
        over.frames = over.frames.saturating_sub(1);
        if over.frames == 0 {
            commands.remove_resource::<FlyOverride>();
        }
        return;
    }
    let Some(input) = input else {
        *free = FreeLookInput::default();
        return;
    };
    use actions as a;
    let sticks = FlySticks {
        move_axes: input.axis2d(a::MOVE),
        mouse: input.axis2d(a::LOOK_MOUSE),
        look_pad: input.axis2d(a::LOOK_PAD),
        mouse_look: input.pressed(a::MOUSE_LOOK),
        up: input.pressed(a::UP),
        down: input.pressed(a::DOWN),
        sprint: input.pressed(a::SPRINT),
    };
    *free = free_look(&sticks, time.delta_secs());
}

/// What a hotkey does.
#[must_use]
pub fn hotkey(key: &str, state: &DemoState) -> Option<DemoCommand> {
    match key {
        "F1" => Some(DemoCommand::ShowPage(if state.page == DemoPage::Hidden {
            DemoPage::Controls
        } else {
            DemoPage::Hidden
        })),
        "F2" => Some(DemoCommand::ShowPage(if state.page == DemoPage::Credits {
            DemoPage::Controls
        } else {
            DemoPage::Credits
        })),
        "F12" => Some(DemoCommand::Screenshot(None)),
        "F9" if !state.bench.running() => Some(DemoCommand::StartBenchmark {
            warmup: None,
            frames: None,
            out: None,
        }),
        "Escape" if state.bench.running() => Some(DemoCommand::CancelBenchmark),
        _ => None,
    }
}

/// `Update`: the keyboard and gamepad hotkeys queue their commands.
pub fn hotkeys(
    mut keys: MessageReader<KeyboardInput>,
    input: Option<Res<Actions>>,
    state: Res<DemoState>,
    mut queue: ResMut<DemoQueue>,
) {
    for key in keys.read() {
        if key.state != ButtonState::Pressed || key.repeat {
            continue;
        }
        if let Some(command) = hotkey(&key.physical_key, &state) {
            queue.0.push(command);
        }
    }
    let Some(input) = input else { return };
    if input.just_pressed(actions::PANEL)
        && let Some(command) = hotkey("F1", &state)
    {
        queue.0.push(command);
    }
    if input.just_pressed(actions::SCREENSHOT) {
        queue.0.push(DemoCommand::Screenshot(None));
    }
    if input.just_pressed(actions::BENCHMARK) {
        let key = if state.bench.running() { "Escape" } else { "F9" };
        if let Some(command) = hotkey(key, &state) {
            queue.0.push(command);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mouse_looks_only_while_the_button_is_held() {
        let mut sticks = FlySticks {
            mouse: Vec2::new(40.0, 10.0),
            ..FlySticks::default()
        };
        assert_eq!(free_look(&sticks, 1.0 / 60.0).look, Vec2::ZERO);
        sticks.mouse_look = true;
        let look = free_look(&sticks, 1.0 / 60.0).look;
        assert!(
            (look.x - 40.0 * MOUSE_RADIANS_PER_PIXEL).abs() < 1e-6,
            "mouse right turns right"
        );
        assert!(
            (look.y + 10.0 * MOUSE_RADIANS_PER_PIXEL).abs() < 1e-6,
            "mouse down looks down"
        );
    }

    #[test]
    fn the_stick_looks_by_time_and_moves_are_clamped() {
        let sticks = FlySticks {
            look_pad: Vec2::new(0.0, 1.0),
            move_axes: Vec2::new(1.0, 1.0) + Vec2::new(0.0, 1.0),
            up: true,
            sprint: true,
            ..FlySticks::default()
        };
        let input = free_look(&sticks, 0.2);
        assert!((input.look.y - PAD_RADIANS_PER_SECOND * 0.2).abs() < 1e-6);
        let stalled = free_look(&sticks, 2.0);
        assert!(
            (stalled.look.y - PAD_RADIANS_PER_SECOND * 0.25).abs() < 1e-6,
            "a stalled frame turns at most 0.25 s worth"
        );
        assert!((input.move_axes.length() - 1.0).abs() < 1e-5);
        assert_eq!(input.rise, 1.0);
        assert!(input.sprint);
        assert_eq!(free_look(&sticks, f32::NAN).look, Vec2::ZERO);
        let both = FlySticks {
            up: true,
            down: true,
            ..FlySticks::default()
        };
        assert_eq!(free_look(&both, 0.1).rise, 0.0);
    }

    #[test]
    fn yaw_and_pitch_match_the_rig_convention() {
        let (yaw, pitch) = yaw_pitch(Quat::IDENTITY);
        assert!(yaw.abs() < 1e-6 && pitch.abs() < 1e-6);
        // Facing −X is yaw +90°.
        let (yaw, _) = yaw_pitch(Quat::from_rotation_y(std::f32::consts::FRAC_PI_2));
        assert!((yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        let (_, pitch) = yaw_pitch(Quat::from_rotation_x(0.4));
        assert!((pitch - 0.4).abs() < 1e-5);
        // The rig's own rotation for a yaw and pitch round-trips.
        let rig = Quat::from_rotation_y(-1.1) * Quat::from_rotation_x(-0.3);
        let (yaw, pitch) = yaw_pitch(rig);
        assert!((yaw + 1.1).abs() < 1e-5 && (pitch + 0.3).abs() < 1e-5);
    }

    #[test]
    fn hotkeys_toggle_pages_and_start_or_stop_the_benchmark() {
        let mut world = World::new();
        world.insert_resource(crate::demo::tests_support::state());
        let mut state = world.resource::<DemoState>().clone();
        assert_eq!(
            hotkey("F1", &state),
            Some(DemoCommand::ShowPage(DemoPage::Hidden))
        );
        assert_eq!(
            hotkey("F2", &state),
            Some(DemoCommand::ShowPage(DemoPage::Credits))
        );
        state.page = DemoPage::Credits;
        assert_eq!(
            hotkey("F2", &state),
            Some(DemoCommand::ShowPage(DemoPage::Controls))
        );
        assert_eq!(hotkey("F12", &state), Some(DemoCommand::Screenshot(None)));
        assert!(matches!(
            hotkey("F9", &state),
            Some(DemoCommand::StartBenchmark { .. })
        ));
        assert_eq!(hotkey("Escape", &state), None, "nothing to stop");
        assert_eq!(hotkey("KeyW", &state), None);
    }
}
