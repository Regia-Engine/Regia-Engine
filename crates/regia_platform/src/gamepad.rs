// SPDX-License-Identifier: MPL-2.0
//! Raw gamepad plumbing (`docs/specs/11-platform-and-input.md` § 6): the standard-layout button and
//! axis enums and the messages backends translate their native events into.
//!
//! Backends (gilrs on desktop, `android-activity` events on Android) live in `regia_input`; the
//! platform layer only owns these neutral types and the Android JNI queries the backends need
//! (device enumeration and rumble, [`crate::enumerate_gamepads`] / [`crate::gamepad_rumble`]).
//!
//! Conventions (matching the action layer, `docs/02-architecture.md` § 3): stick X is right,
//! stick Y is **up** (native backends flip where the OS reports down-positive), triggers are
//! `0..1`.

use regia_app::prelude::*;

use crate::events::ButtonState;

/// A button of the standard gamepad layout. Every backend maps its native buttons onto this set;
/// buttons a pad does not have simply never fire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum GamepadButton {
    /// Face button at the bottom (A / Cross).
    South,
    /// Face button at the right (B / Circle).
    East,
    /// Face button at the left (X / Square).
    West,
    /// Face button at the top (Y / Triangle).
    North,
    /// Left digital shoulder (L1 / LB).
    LeftShoulder,
    /// Right digital shoulder (R1 / RB).
    RightShoulder,
    /// Left analog trigger (L2 / LT); also reported as [`GamepadAxis::LeftTrigger`].
    LeftTrigger,
    /// Right analog trigger (R2 / RT); also reported as [`GamepadAxis::RightTrigger`].
    RightTrigger,
    /// Left stick pressed (L3).
    LeftStick,
    /// Right stick pressed (R3).
    RightStick,
    /// D-pad up.
    DPadUp,
    /// D-pad down.
    DPadDown,
    /// D-pad left.
    DPadLeft,
    /// D-pad right.
    DPadRight,
    /// Start / Options / +.
    Start,
    /// Select / Share / − / Back.
    Select,
    /// Mode / Home / PS button (often reserved by the OS).
    Mode,
}

/// An axis of the standard gamepad layout. Sticks are −1..=1 (x right, y up), triggers 0..1, the
/// d-pad axes −1..=1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub enum GamepadAxis {
    /// Left stick, left/right.
    LeftStickX,
    /// Left stick, down/up.
    LeftStickY,
    /// Right stick, left/right.
    RightStickX,
    /// Right stick, down/up.
    RightStickY,
    /// Left analog trigger, 0..1.
    LeftTrigger,
    /// Right analog trigger, 0..1.
    RightTrigger,
    /// D-pad left/right (hat X).
    DPadX,
    /// D-pad down/up (hat Y).
    DPadY,
}

/// A gamepad was connected. `device` is a backend-defined stable id for this session.
#[derive(Message, Debug, Clone, PartialEq, Eq)]
pub struct GamepadConnected {
    /// Backend device id.
    pub device: u64,
    /// Human-readable pad name as reported by the OS.
    pub name: String,
}

/// A gamepad was disconnected.
#[derive(Message, Debug, Clone, Copy, PartialEq, Eq)]
pub struct GamepadDisconnected {
    /// Backend device id.
    pub device: u64,
}

/// A gamepad button changed. `value` is 0..1 (analog value for triggers, 0 or 1 for digital
/// buttons); `state` is the pressed/released edge.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct GamepadButtonChanged {
    /// Backend device id.
    pub device: u64,
    /// Which button.
    pub button: GamepadButton,
    /// Down or up.
    pub state: ButtonState,
    /// Analog magnitude, 0..1.
    pub value: f32,
}

/// A gamepad axis changed.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct GamepadAxisChanged {
    /// Backend device id.
    pub device: u64,
    /// Which axis.
    pub axis: GamepadAxis,
    /// New value (see [`GamepadAxis`] for ranges).
    pub value: f32,
}

/// A request to rumble a pad, queued on the `Gamepads` resource in `regia_input` and
/// applied by the active backend (fire and forget).
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct RumbleRequest {
    /// Backend device id.
    pub device: u64,
    /// Motor strength, 0..1.
    pub strength: f32,
    /// How long to rumble.
    pub duration: std::time::Duration,
}

pub(crate) fn register(app: &mut App) {
    app.add_message::<GamepadConnected>()
        .add_message::<GamepadDisconnected>()
        .add_message::<GamepadButtonChanged>()
        .add_message::<GamepadAxisChanged>()
        .add_message::<RumbleRequest>();
}
