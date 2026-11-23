// SPDX-License-Identifier: MPL-2.0
//! Pure Android input translation tables for the gamepad backend (`gamepad::android_backend`).
//!
//! Kept free of JNI and ECS types so the tables are unit-tested on every host; only the Android
//! build calls them outside tests.

use regia_platform::{GamepadButton, TouchPhase};

/// Android `AKEYCODE_*` values for gamepad keys (public SDK constants, see the KeyEvent docs).
///
/// Only the `BUTTON_*` family: the D-pad arrives as arrow keys, not pad buttons.
pub fn button_from_akeycode(code: u32) -> Option<GamepadButton> {
    Some(match code {
        96 => GamepadButton::South,          // AKEYCODE_BUTTON_A
        97 => GamepadButton::East,           // AKEYCODE_BUTTON_B
        98 => GamepadButton::Mode,           // AKEYCODE_BUTTON_C (rare; treat as Mode)
        99 => GamepadButton::West,           // AKEYCODE_BUTTON_X
        100 => GamepadButton::North,         // AKEYCODE_BUTTON_Y
        101 => GamepadButton::Mode,          // AKEYCODE_BUTTON_Z (rare; treat as Mode)
        102 => GamepadButton::LeftShoulder,  // AKEYCODE_BUTTON_L1
        103 => GamepadButton::RightShoulder, // AKEYCODE_BUTTON_R1
        104 => GamepadButton::LeftTrigger,   // AKEYCODE_BUTTON_L2
        105 => GamepadButton::RightTrigger,  // AKEYCODE_BUTTON_R2
        106 => GamepadButton::LeftStick,     // AKEYCODE_BUTTON_THUMBL
        107 => GamepadButton::RightStick,    // AKEYCODE_BUTTON_THUMBR
        108 => GamepadButton::Start,         // AKEYCODE_BUTTON_START
        109 => GamepadButton::Select,        // AKEYCODE_BUTTON_SELECT
        110 => GamepadButton::Mode,          // AKEYCODE_BUTTON_MODE
        _ => return None,
    })
}

/// Parses the runner's physical-key name for unidentified Android keys,
/// `"Unidentified(Android(<akeycode>))"`.
pub fn android_keycode(physical_key: &str) -> Option<u32> {
    let inner = physical_key
        .strip_prefix("Unidentified(Android(")?
        .strip_suffix("))")?;
    inner.parse().ok()
}

/// An untracked `Moved` report with coordinates in −1..=1 (plus a 1.1 stop margin, clamped) is a
/// joystick event: winit turns Android joystick MotionEvents into touch events with axis values as
/// position (joysticks never send a `Started` pointer-down). Untracked terminal events reset that stick to center;
/// tracked touches and `Started` events are refused.
pub fn joystick_from_touch(phase: TouchPhase, position: (f64, f64), tracked: bool) -> Option<(f32, f32)> {
    if tracked {
        return None;
    }
    if matches!(phase, TouchPhase::Ended | TouchPhase::Cancelled) {
        return Some((0.0, 0.0));
    }
    if phase != TouchPhase::Moved {
        return None;
    }
    let (x, y) = position;
    // A stick may report slightly past the unit stop; accept a margin and clamp into range.
    const MARGIN: f64 = 1.1;
    if !(-MARGIN..=MARGIN).contains(&x) || !(-MARGIN..=MARGIN).contains(&y) {
        return None;
    }
    let (x, y) = (x.clamp(-1.0, 1.0), y.clamp(-1.0, 1.0));
    // Android's AXIS_Y is down-positive (up = −1); the engine's stick Y is up-positive.
    Some((x as f32, -(y as f32)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stick_slightly_past_unit_is_clamped_not_dropped() {
        assert_eq!(
            joystick_from_touch(TouchPhase::Moved, (1.02, -1.05), false),
            Some((1.0, 1.0))
        );
        assert_eq!(joystick_from_touch(TouchPhase::Moved, (1.5, 0.0), false), None);
        assert_eq!(
            joystick_from_touch(TouchPhase::Moved, (f64::NAN, 0.0), false),
            None
        );
    }
    #[test]
    fn keycode_parses_only_the_android_unidentified_form() {
        assert_eq!(android_keycode("Unidentified(Android(96))"), Some(96));
        assert_eq!(android_keycode("Unidentified(Android(x))"), None);
        assert_eq!(android_keycode("Unidentified(Android(96)"), None);
        assert_eq!(android_keycode("KeyA"), None);
    }

    #[test]
    fn face_shoulder_codes_map() {
        assert_eq!(button_from_akeycode(96), Some(GamepadButton::South));
        assert_eq!(button_from_akeycode(100), Some(GamepadButton::North));
        assert_eq!(button_from_akeycode(103), Some(GamepadButton::RightShoulder));
        assert_eq!(
            button_from_akeycode(19),
            None,
            "dpad arrives as arrows, not pad buttons"
        );
        assert_eq!(button_from_akeycode(0), None);
    }

    #[test]
    fn stick_reports_flip_y_and_ignore_real_touches() {
        assert_eq!(
            joystick_from_touch(TouchPhase::Moved, (0.5, -1.0), false),
            Some((0.5, 1.0))
        );
        assert_eq!(joystick_from_touch(TouchPhase::Moved, (0.5, 0.5), true), None);
        assert_eq!(joystick_from_touch(TouchPhase::Started, (0.5, 0.5), false), None);
        assert_eq!(
            joystick_from_touch(TouchPhase::Moved, (300.0, 40.0), false),
            None,
            "pixel positions are touches, not axes"
        );
    }

    #[test]
    fn joystick_mapping_refuses_non_finite_and_non_moved_inputs() {
        for position in [
            (f64::NAN, 0.0),
            (0.0, f64::NAN),
            (f64::INFINITY, 0.0),
            (0.0, f64::NEG_INFINITY),
            // Past the 1.1 stop margin (v048 accepts and clamps up to it).
            (1.11, 0.0),
            (0.0, -1.11),
        ] {
            assert_eq!(joystick_from_touch(TouchPhase::Moved, position, false), None);
        }
        // Inside the margin the axis is clamped to the unit stop, never refused.
        assert_eq!(
            joystick_from_touch(TouchPhase::Moved, (1.05, -1.05), false),
            Some((1.0, 1.0))
        );
        assert_eq!(joystick_from_touch(TouchPhase::Ended, (0.5, 0.5), true), None);
        assert_eq!(joystick_from_touch(TouchPhase::Cancelled, (0.5, 0.5), true), None);
        assert_eq!(joystick_from_touch(TouchPhase::Started, (0.5, 0.5), false), None);
        assert_eq!(joystick_from_touch(TouchPhase::Moved, (0.5, 0.5), true), None);
    }

    #[test]
    fn untracked_terminal_touch_reports_release_the_joystick_to_center() {
        assert_eq!(
            joystick_from_touch(TouchPhase::Cancelled, (0.5, 0.5), false),
            Some((0.0, 0.0))
        );
        assert_eq!(
            joystick_from_touch(TouchPhase::Ended, (0.5, 0.5), false),
            Some((0.0, 0.0))
        );
    }

    #[test]
    fn android_keycode_parser_refuses_malformed_and_out_of_range_values() {
        for key in [
            "Unidentified(Android())",
            "Unidentified(Android(-1))",
            "Unidentified(Android(4294967296))",
            "Unidentified(Android(96)tail)",
            "Unidentified(Android( 96))",
            "KeyA",
        ] {
            assert_eq!(android_keycode(key), None, "accepted malformed key {key:?}");
        }
        assert_eq!(button_from_akeycode(u32::MAX), None);
        assert_eq!(button_from_akeycode(111), None);
    }
}
