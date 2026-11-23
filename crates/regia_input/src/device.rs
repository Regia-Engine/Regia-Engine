// SPDX-License-Identifier: MPL-2.0
//! Raw device state (`docs/specs/11-platform-and-input.md` § 6): the [`Keyboard`] and [`Mouse`]
//! resources, the `PreUpdate` systems that fill them from the platform's input messages, the
//! [`Devices`] read-only view the action layer evaluates bindings against, and the
//! [`LastUsedDevice`] tracker for button-prompt UIs.
//!
//! Touch ([`crate::Touches`]) and gamepads ([`crate::Gamepads`]) live in their own modules.
//!
//! Per-frame data — `just_pressed`, `just_released`, [`Mouse::delta`] and [`Mouse::wheel`] — is
//! cleared at the start of each device update, so right after [`update_keyboard`] /
//! [`update_mouse`] it describes exactly the messages that arrived since the last frame. The
//! mouse delta is additionally cleared once the actions have been evaluated (see
//! [`crate::clear_mouse_delta`]).

use std::borrow::Borrow;
use std::collections::HashSet;
use std::fmt;

use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_platform::{
    ButtonState, CursorMoved, KeyboardInput, MouseButton, MouseButtonInput, MouseMotion, MouseWheel,
};

/// A physical key name as reported by the platform layer, e.g. `"KeyW"`: the position of the key,
/// independent of the keyboard layout.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct KeyCode(String);

impl KeyCode {
    /// Creates a key code from its platform name, e.g. `"KeyW"`.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The platform name of this key.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for KeyCode {
    fn from(name: &str) -> Self {
        Self::new(name)
    }
}

impl From<String> for KeyCode {
    fn from(name: String) -> Self {
        Self(name)
    }
}

impl fmt::Display for KeyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Borrow<str> for KeyCode {
    fn borrow(&self) -> &str {
        &self.0
    }
}

/// Keyboard state: which physical keys are held, and which went down or up this frame.
///
/// Updated every frame in `PreUpdate` from `regia_platform::KeyboardInput` messages
/// (see [`update_keyboard`]), using `physical_key` only. `logical_key` stays on the message
/// and is not stored here. Auto-repeat events never re-trigger [`Keyboard::just_pressed`].
#[derive(Resource, Debug, Clone, Default)]
pub struct Keyboard {
    pub(crate) held: HashSet<KeyCode>,
    pub(crate) just_pressed: HashSet<KeyCode>,
    pub(crate) just_released: HashSet<KeyCode>,
}

impl Keyboard {
    /// True while `key` (a physical name such as `"KeyW"`) is held down.
    pub fn pressed(&self, key: &str) -> bool {
        self.held.contains(key)
    }

    /// True on the frame `key` went down (auto-repeat excluded).
    pub fn just_pressed(&self, key: &str) -> bool {
        self.just_pressed.contains(key)
    }

    /// True on the frame `key` came back up.
    pub fn just_released(&self, key: &str) -> bool {
        self.just_released.contains(key)
    }
}

/// What this frame's `WindowFocused` messages say. Reads every message (no short circuit), so
/// none is left unread for the next frame.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct FocusChange {
    /// Some message reported a focus loss: inputs held from earlier frames lost their up events.
    pub(crate) lost: bool,
    /// The app ends the batch with no focused window (no window's last message says focused), so
    /// inputs pressed in this batch (before the loss; an unfocused app gets no input) lose their
    /// up events too. Focus moving between two of the app's windows is not a loss, in either
    /// message order.
    pub(crate) ends_unfocused: bool,
}

/// Reads this frame's focus messages (see [`FocusChange`]).
pub(crate) fn focus_change(focus: &mut MessageReader<regia_platform::WindowFocused>) -> FocusChange {
    let mut change = FocusChange::default();
    // The last state per window this batch (a handful of windows at most).
    let mut last: Vec<(Entity, bool)> = Vec::new();
    for message in focus.read() {
        change.lost |= !message.focused;
        match last.iter_mut().find(|(window, _)| *window == message.window) {
            Some(entry) => entry.1 = message.focused,
            None => last.push((message.window, message.focused)),
        }
    }
    change.ends_unfocused = change.lost && !last.iter().any(|(_, focused)| *focused);
    change
}

/// `PreUpdate` (set [`crate::InputSet::Devices`]): drains `regia_platform::KeyboardInput`
/// messages into the [`Keyboard`] resource.
///
/// A press marks the key held and just-pressed; auto-repeat presses keep it held without
/// re-triggering just-pressed; a release unmarks it and marks it just-released. Per-frame sets are
/// cleared at the start of the system, so after it runs they hold exactly this frame's events.
#[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
pub fn update_keyboard(
    mut keyboard: ResMut<Keyboard>,
    mut inputs: MessageReader<KeyboardInput>,
    mut focus: MessageReader<regia_platform::WindowFocused>,
    mut last_used: ResMut<LastUsedDevice>,
) {
    keyboard.just_pressed.clear();
    keyboard.just_released.clear();
    // Focus loss swallows the key-up events: release everything so no key stays stuck.
    let focus = focus_change(&mut focus);
    if focus.lost {
        let held: Vec<KeyCode> = keyboard.held.drain().collect();
        keyboard.just_released.extend(held);
    }
    for input in inputs.read() {
        last_used.keyboard();
        let key = KeyCode::new(input.physical_key.as_str());
        match input.state {
            ButtonState::Pressed if input.repeat => {
                keyboard.held.insert(key);
            }
            ButtonState::Pressed => {
                if keyboard.held.insert(key.clone()) {
                    keyboard.just_pressed.insert(key);
                }
            }
            ButtonState::Released => {
                if keyboard.held.remove(&key) {
                    keyboard.just_released.insert(key);
                }
            }
        }
    }
    // The batch ends unfocused: a key pressed in it (before the loss) never gets its release,
    // which goes to the newly focused app. Release it now; its press edge stays (it happened).
    if focus.ends_unfocused {
        let held: Vec<KeyCode> = keyboard.held.drain().collect();
        keyboard.just_released.extend(held);
    }
}

/// Mouse state: buttons, cursor position, motion accumulated this frame and wheel motion.
///
/// Updated every frame in `PreUpdate` from the `regia_platform` mouse messages (see
/// [`update_mouse`]).
#[derive(Resource, Debug, Clone, Default)]
pub struct Mouse {
    pub(crate) held: HashSet<MouseButton>,
    pub(crate) just_pressed: HashSet<MouseButton>,
    pub(crate) just_released: HashSet<MouseButton>,
    pub(crate) position: Vec2,
    pub(crate) delta: Vec2,
    pub(crate) wheel: Vec2,
}

/// How many physical pixels one wheel "line" is when normalising pixel-unit (touchpad) wheel
/// events. A pragmatic default in the same ballpark as desktop browsers (Chromium uses 40 px per
/// line); project-level tuning can rescale the resulting action values.
pub const PIXELS_PER_WHEEL_LINE: f32 = 40.0;

impl Mouse {
    /// True while `button` is held down.
    pub fn pressed(&self, button: MouseButton) -> bool {
        self.held.contains(&button)
    }

    /// True on the frame `button` went down.
    pub fn just_pressed(&self, button: MouseButton) -> bool {
        self.just_pressed.contains(&button)
    }

    /// True on the frame `button` came back up.
    pub fn just_released(&self, button: MouseButton) -> bool {
        self.just_released.contains(&button)
    }

    /// The last known cursor position, in the physical pixels the platform reports (window
    /// top-left origin, y down). Converting to logical pixels needs the window scale factor and
    /// arrives with the full M7 system.
    pub fn position(&self) -> Vec2 {
        self.position
    }

    /// Movement accumulated from this frame's `MouseMotion` messages, in device units. Zeroed
    /// after the actions have been evaluated (see [`crate::clear_mouse_delta`]).
    pub fn delta(&self) -> Vec2 {
        self.delta
    }

    /// Wheel motion accumulated this frame, normalised to *lines* (specs 11 § 6): line/notch
    /// events pass through, pixel (touchpad) events are divided by [`PIXELS_PER_WHEEL_LINE`].
    /// The value is zeroed at the start of the next frame.
    pub fn wheel(&self) -> Vec2 {
        self.wheel
    }
}

/// `PreUpdate` (set [`crate::InputSet::Devices`]): drains `regia_platform::MouseButtonInput`,
/// `MouseMotion`, `MouseWheel` and `CursorMoved` messages into the [`Mouse`] resource.
///
/// Delta and wheel accumulate every message of the frame (wheel events normalised to lines,
/// see [`Mouse::wheel`]); the cursor position is the last one reported. Per-frame data is cleared
/// at the start of the system, so after it runs it holds exactly this frame's messages.
#[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
pub fn update_mouse(
    mut mouse: ResMut<Mouse>,
    mut buttons: MessageReader<MouseButtonInput>,
    mut motions: MessageReader<MouseMotion>,
    mut wheels: MessageReader<MouseWheel>,
    mut cursors: MessageReader<CursorMoved>,
    mut focus: MessageReader<regia_platform::WindowFocused>,
    mut last_used: ResMut<LastUsedDevice>,
) {
    mouse.just_pressed.clear();
    mouse.just_released.clear();
    mouse.delta = Vec2::ZERO;
    mouse.wheel = Vec2::ZERO;
    // Focus loss swallows the button-up events: release every held button.
    let focus = focus_change(&mut focus);
    if focus.lost {
        let held: Vec<MouseButton> = mouse.held.drain().collect();
        mouse.just_released.extend(held);
    }
    for input in buttons.read() {
        last_used.mouse();
        match input.state {
            ButtonState::Pressed => {
                if mouse.held.insert(input.button) {
                    mouse.just_pressed.insert(input.button);
                }
            }
            ButtonState::Released => {
                if mouse.held.remove(&input.button) {
                    mouse.just_released.insert(input.button);
                }
            }
        }
    }
    // As for keys: a button pressed in a batch that ends unfocused never gets its release.
    if focus.ends_unfocused {
        let held: Vec<MouseButton> = mouse.held.drain().collect();
        mouse.just_released.extend(held);
    }
    for motion in motions.read() {
        if motion.delta != (0.0, 0.0) {
            last_used.mouse();
        }
        mouse.delta += finite_delta(motion.delta);
    }
    for wheel in wheels.read() {
        last_used.mouse();
        let scale = match wheel.unit {
            regia_platform::WheelUnit::Line => 1.0,
            regia_platform::WheelUnit::Pixel => 1.0 / PIXELS_PER_WHEEL_LINE,
        };
        mouse.wheel += Vec2::new(wheel.delta.0, wheel.delta.1) * scale;
    }
    for cursor in cursors.read() {
        mouse.position = Vec2::new(cursor.position.0 as f32, cursor.position.1 as f32);
    }
}

/// A read-only view over all raw devices, with the acting player's pad restriction applied.
/// [`crate::InputSource::raw`] evaluates against this; constructing one directly is only needed
/// for tests and custom sources.
#[derive(Debug, Clone, Copy)]
pub struct Devices<'a> {
    /// Keyboard state.
    pub keyboard: &'a Keyboard,
    /// Mouse state.
    pub mouse: &'a Mouse,
    /// Gamepad state (all connected pads).
    pub gamepads: &'a crate::gamepad::Gamepads,
    /// When set, gamepad sources only read this pad (per-player device assignment).
    pub pad: Option<u64>,
}

impl Devices<'_> {
    /// All devices, any pad.
    pub fn new<'a>(
        keyboard: &'a Keyboard,
        mouse: &'a Mouse,
        gamepads: &'a crate::gamepad::Gamepads,
    ) -> Devices<'a> {
        Devices {
            keyboard,
            mouse,
            gamepads,
            pad: None,
        }
    }

    /// The largest analog value of `button` across the allowed pads (0 if none pressed). Trigger
    /// buttons are analog, so a half-pull reports its magnitude. A press and release inside one
    /// batch stores 0 as the value but keeps the press edge for the rest of the frame, which
    /// counts as full actuation for every evaluation of the frame (see `InputSource::raw`).
    pub fn max_button_value(&self, button: regia_platform::GamepadButton) -> f32 {
        self.gamepads
            .pads()
            .filter(|(id, _)| self.pad.is_none_or(|pad| *id == pad))
            .map(|(_, pad)| {
                let value = pad.button_value(button);
                if value > 0.0 || !pad.just_pressed(button) {
                    value
                } else {
                    1.0
                }
            })
            .fold(0.0, f32::max)
    }

    /// The value of `axis` with the largest magnitude across the allowed pads that report it (0 if
    /// none does), so an idle or phantom pad never masks a live one.
    pub fn axis(&self, axis: regia_platform::GamepadAxis) -> f32 {
        self.gamepads
            .pads()
            .filter(|(id, _)| self.pad.is_none_or(|pad| *id == pad))
            .filter(|(_, pad)| pad.axes.contains_key(&axis))
            .map(|(_, pad)| pad.axis(axis))
            .fold(
                0.0,
                |best: f32, value| {
                    if value.abs() > best.abs() { value } else { best }
                },
            )
    }
}

/// Which physical device the player used last, for showing the right button prompts
/// (`docs/specs/11` § 7, `LastUsedDevice`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Resource, Default)]
pub struct LastUsedDevice(pub Option<InputDevice>);

/// A device class a player can be using.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputDevice {
    /// Physical keyboard.
    Keyboard,
    /// Mouse (any button/motion).
    Mouse,
    /// One gamepad.
    Gamepad(u64),
    /// Touchscreen.
    Touch,
}

impl LastUsedDevice {
    /// Records keyboard use if the resource still shows something else.
    pub(crate) fn keyboard(&mut self) {
        if self.0 != Some(InputDevice::Keyboard) {
            self.0 = Some(InputDevice::Keyboard);
        }
    }

    /// Records mouse use if the resource still shows something else.
    pub(crate) fn mouse(&mut self) {
        if self.0 != Some(InputDevice::Mouse) {
            self.0 = Some(InputDevice::Mouse);
        }
    }

    /// Records gamepad use if the resource still shows something else.
    pub(crate) fn gamepad(&mut self, id: u64) {
        if self.0 != Some(InputDevice::Gamepad(id)) {
            self.0 = Some(InputDevice::Gamepad(id));
        }
    }

    /// Records touch use if the resource still shows something else.
    pub(crate) fn touch(&mut self) {
        if self.0 != Some(InputDevice::Touch) {
            self.0 = Some(InputDevice::Touch);
        }
    }
}

/// Converts a raw `f64` pointer delta to `f32`, dropping it (zero) when it is not finite after
/// the cast (NaN, or an `f64` too large for `f32`).
fn finite_delta(delta: (f64, f64)) -> Vec2 {
    let v = Vec2::new(delta.0 as f32, delta.1 as f32);
    if v.is_finite() { v } else { Vec2::ZERO }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_pointer_deltas_are_dropped() {
        assert_eq!(finite_delta((1e300, 1.0)), Vec2::ZERO);
        assert_eq!(finite_delta((f64::NAN, 1.0)), Vec2::ZERO);
        assert_eq!(finite_delta((2.0, -3.0)), Vec2::new(2.0, -3.0));
    }

    #[test]
    fn key_code_set_lookups_work_by_name() {
        let mut held = HashSet::new();
        held.insert(KeyCode::new("KeyW"));
        assert!(held.contains("KeyW"), "Borrow<str> lookup without allocating");
        assert!(!held.contains("KeyA"));
    }

    #[test]
    fn key_code_conversions() {
        assert_eq!(KeyCode::new("KeyW").as_str(), "KeyW");
        assert_eq!(KeyCode::from(String::from("Space")), KeyCode::from("Space"));
        assert_eq!(KeyCode::from("Escape").to_string(), "Escape");
    }

    #[test]
    fn keyboard_defaults_are_empty() {
        let keyboard = Keyboard::default();
        assert!(!keyboard.pressed("KeyW"));
        assert!(!keyboard.just_pressed("KeyW"));
        assert!(!keyboard.just_released("KeyW"));
    }

    #[test]
    fn mouse_defaults_are_empty_and_zero() {
        let mouse = Mouse::default();
        assert!(!mouse.pressed(MouseButton::Left));
        assert!(!mouse.just_pressed(MouseButton::Left));
        assert!(!mouse.just_released(MouseButton::Left));
        assert_eq!(mouse.position(), Vec2::ZERO);
        assert_eq!(mouse.delta(), Vec2::ZERO);
        assert_eq!(mouse.wheel(), Vec2::ZERO);
    }

    #[test]
    fn last_used_device_tracks_the_latest_class() {
        let mut last = LastUsedDevice::default();
        assert_eq!(last.0, None);
        last.keyboard();
        assert_eq!(last.0, Some(InputDevice::Keyboard));
        last.keyboard(); // idempotent
        last.mouse();
        assert_eq!(last.0, Some(InputDevice::Mouse));
        last.gamepad(3);
        assert_eq!(last.0, Some(InputDevice::Gamepad(3)));
        last.touch();
        assert_eq!(last.0, Some(InputDevice::Touch));
    }

    #[test]
    fn gamepad_axis_readings_respect_the_pad_filter() {
        let mut gamepads = crate::gamepad::Gamepads::default();
        gamepads.set_axis_for_test(1, regia_platform::GamepadAxis::LeftStickX, 0.8);
        gamepads.set_axis_for_test(2, regia_platform::GamepadAxis::LeftStickX, -0.4);
        let (keyboard, mouse) = (Keyboard::default(), Mouse::default());
        let devices = Devices::new(&keyboard, &mouse, &gamepads);
        assert_eq!(devices.axis(regia_platform::GamepadAxis::LeftStickX), 0.8);
        let only_two = Devices {
            pad: Some(2),
            ..devices
        };
        assert_eq!(
            only_two.axis(regia_platform::GamepadAxis::LeftStickX),
            -0.4,
            "pad filter hides other pads"
        );
    }

    #[test]
    fn equal_axis_magnitudes_keep_the_lower_device_id() {
        let mut gamepads = crate::gamepad::Gamepads::default();
        gamepads.set_axis_for_test(1, regia_platform::GamepadAxis::LeftStickX, -0.5);
        gamepads.set_axis_for_test(2, regia_platform::GamepadAxis::LeftStickX, 0.5);
        let (keyboard, mouse) = (Keyboard::default(), Mouse::default());
        let devices = Devices::new(&keyboard, &mouse, &gamepads);
        assert_eq!(
            devices.axis(regia_platform::GamepadAxis::LeftStickX),
            -0.5,
            "a tie keeps the earlier pad, sign included"
        );
        gamepads.set_axis_for_test(1, regia_platform::GamepadAxis::LeftStickX, 0.5);
        gamepads.set_axis_for_test(2, regia_platform::GamepadAxis::LeftStickX, -0.75);
        let devices = Devices::new(&keyboard, &mouse, &gamepads);
        assert_eq!(
            devices.axis(regia_platform::GamepadAxis::LeftStickX),
            -0.75,
            "a strictly larger magnitude still wins"
        );
    }

    fn device_app() -> App {
        let mut app = App::new();
        app.add_message::<KeyboardInput>()
            .add_message::<MouseButtonInput>()
            .add_message::<MouseMotion>()
            .add_message::<MouseWheel>()
            .add_message::<CursorMoved>()
            .add_message::<regia_platform::WindowFocused>()
            .init_resource::<Keyboard>()
            .init_resource::<Mouse>()
            .init_resource::<LastUsedDevice>()
            .add_systems(PreUpdate, (update_keyboard, update_mouse));
        app
    }

    fn key(app: &mut App, name: &str, state: ButtonState, repeat: bool) {
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: name.into(),
            logical_key: name.into(),
            state,
            repeat,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
    }

    fn mouse_button(app: &mut App, button: MouseButton, state: ButtonState) {
        app.world_mut().write_message(MouseButtonInput {
            window: Entity::PLACEHOLDER,
            button,
            state,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
    }

    #[test]
    fn logical_key_is_not_stored_on_the_keyboard() {
        let mut app = device_app();
        app.world_mut().write_message(KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: "KeyA".into(),
            logical_key: "a".into(),
            state: ButtonState::Pressed,
            repeat: false,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();
        let keyboard = app.world().resource::<Keyboard>();
        assert!(keyboard.pressed("KeyA"));
        assert!(keyboard.just_pressed("KeyA"));
        assert!(!keyboard.pressed("a"), "the logical key is not a stored name");
        assert!(!keyboard.just_pressed("a"));
    }

    #[test]
    fn repeat_release_wheel_and_focus_edges() {
        let mut app = device_app();
        key(&mut app, "KeyW", ButtonState::Pressed, false);
        mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);
        app.world_mut().write_message(MouseMotion {
            delta: (1.5, -2.0),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.world_mut().write_message(MouseMotion {
            delta: (0.5, 0.25),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.world_mut().write_message(MouseWheel {
            window: Entity::PLACEHOLDER,
            delta: (80.0, -40.0),
            unit: regia_platform::WheelUnit::Pixel,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.world_mut().write_message(MouseWheel {
            window: Entity::PLACEHOLDER,
            delta: (0.5, 1.0),
            unit: regia_platform::WheelUnit::Line,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.world_mut().write_message(CursorMoved {
            window: Entity::PLACEHOLDER,
            position: (10.0, 20.0),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.world_mut().write_message(CursorMoved {
            window: Entity::PLACEHOLDER,
            position: (3.0, 4.0),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();
        {
            let keyboard = app.world().resource::<Keyboard>();
            assert!(keyboard.pressed("KeyW") && keyboard.just_pressed("KeyW"));
            let mouse = app.world().resource::<Mouse>();
            assert!(mouse.pressed(MouseButton::Left) && mouse.just_pressed(MouseButton::Left));
            assert_eq!(mouse.delta(), Vec2::new(2.0, -1.75), "motions in one frame add");
            assert_eq!(
                mouse.wheel(),
                Vec2::new(2.5, 0.0),
                "80 px is 2 lines, then the line event adds"
            );
            assert_eq!(mouse.position(), Vec2::new(3.0, 4.0), "the last cursor wins");
        }

        // Repeat of a key that was never pressed still holds it, without a just-pressed edge.
        // A second press of a held key, and a release of a key that is up, add no edges.
        key(&mut app, "KeyR", ButtonState::Pressed, true);
        key(&mut app, "KeyW", ButtonState::Pressed, false);
        key(&mut app, "KeyQ", ButtonState::Released, false);
        app.update();
        {
            let keyboard = app.world().resource::<Keyboard>();
            assert!(keyboard.pressed("KeyR"));
            assert!(!keyboard.just_pressed("KeyR"));
            assert!(keyboard.pressed("KeyW"));
            assert!(!keyboard.just_pressed("KeyW"));
            assert!(!keyboard.just_released("KeyW"));
            assert!(!keyboard.pressed("KeyQ"));
            assert!(!keyboard.just_released("KeyQ"));
        }

        mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);
        mouse_button(&mut app, MouseButton::Right, ButtonState::Released);
        app.world_mut().write_message(MouseMotion {
            delta: (1.0, 0.0),
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
        app.update();
        {
            let mouse = app.world().resource::<Mouse>();
            assert!(mouse.pressed(MouseButton::Left));
            assert!(!mouse.just_pressed(MouseButton::Left));
            assert!(!mouse.just_released(MouseButton::Right));
            assert_eq!(mouse.delta(), Vec2::new(1.0, 0.0));
            assert_eq!(mouse.wheel(), Vec2::ZERO, "wheel is per frame");
            assert_eq!(mouse.position(), Vec2::new(3.0, 4.0), "position sticks");
        }

        // A focus loss releases what was held before the batch. Focus that comes back in the
        // same batch lets a later press grab the key again: both edges, and it ends held. (A
        // batch that *ends* unfocused releases its own presses too: tests/device_batches.rs.)
        app.world_mut().write_message(regia_platform::WindowFocused {
            window: Entity::PLACEHOLDER,
            focused: false,
        });
        app.world_mut().write_message(regia_platform::WindowFocused {
            window: Entity::PLACEHOLDER,
            focused: true,
        });
        key(&mut app, "KeyW", ButtonState::Pressed, false);
        mouse_button(&mut app, MouseButton::Left, ButtonState::Pressed);
        app.update();
        {
            let keyboard = app.world().resource::<Keyboard>();
            assert!(keyboard.pressed("KeyW"));
            assert!(keyboard.just_pressed("KeyW"));
            assert!(keyboard.just_released("KeyW"));
            assert!(!keyboard.pressed("KeyR"), "focus released the repeat-held key");
            assert!(keyboard.just_released("KeyR"));
            let mouse = app.world().resource::<Mouse>();
            assert!(mouse.pressed(MouseButton::Left));
            assert!(mouse.just_pressed(MouseButton::Left));
            assert!(mouse.just_released(MouseButton::Left));
        }
    }
}
