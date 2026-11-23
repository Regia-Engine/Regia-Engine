// SPDX-License-Identifier: MIT-0
//! Touch controls for the hover slice on touch screens (M3 Android lane).
//!
//! The screen is split in half: a drag that starts on the **left half** drives the sample's
//! `"Move"` action as a virtual stick (offset from the touch's start point, clamped to the unit
//! circle, like `keys2d` normalises diagonals); a drag that starts on the **right half** drives
//! `"Look"` exactly like a mouse delta, at the same sensitivity. Keyboard and mouse keep working
//! unchanged — the touch values are added on top in `crate::write_move_input` and
//! `crate::write_look_input`.
//!
//! This is deliberately sample-side: the general touch → action mapping arrives with the M7
//! input system (`docs/specs/11-platform-and-input.md` § 6).

use std::collections::HashMap;

use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_platform::{PrimaryWindow, TouchInput, TouchPhase, Window};

/// Look sensitivity of the touch drag, radians per physical pixel. Identical to the mouse path
/// in `crate::write_look_input` so both inputs feel the same.
pub const LOOK_RADIANS_PER_PIXEL: f32 = 0.003;

/// Radius of the virtual move stick as a fraction of the window's shorter edge.
const MOVE_STICK_FRACTION: f32 = 0.16;

/// Lower bound of the stick radius in physical pixels, so narrow windows stay usable.
const MOVE_STICK_MIN_PIXELS: f32 = 96.0;

/// Radius of the virtual move stick in physical pixels for a window of this physical size.
pub fn move_stick_radius(window: (u32, u32)) -> f32 {
    let shorter_edge = window.0.min(window.1) as f32;
    (shorter_edge * MOVE_STICK_FRACTION).max(MOVE_STICK_MIN_PIXELS)
}

/// What one finger drives, decided by the half of the screen the touch started in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Role {
    /// Virtual move stick (left half).
    Move,
    /// Look drag (right half).
    Look,
}

/// Per-finger tracking state.
#[derive(Debug, Clone, Copy)]
struct Finger {
    role: Role,
    /// Where the touch started; the centre of the virtual stick for [`Role::Move`].
    origin: Vec2,
    /// Latest position; the reference point of the look delta for [`Role::Look`].
    last: Vec2,
}

/// Touch state of the sample: per-finger tracking plus the per-frame values gameplay reads.
///
/// Filled by [`update_touch_controls`] from `regia_platform::TouchInput` messages. Pure logic
/// lives in [`TouchControls::apply`] so it is unit-testable without a window.
#[derive(Resource, Debug, Default)]
pub struct TouchControls {
    fingers: HashMap<u64, Finger>,
    /// Virtual stick value of the move finger: unit circle, x = right, y = forward.
    move_axes: Vec2,
    /// Look drag accumulated this frame in physical pixels, y down, like `Mouse::delta`.
    look_delta: Vec2,
}

impl TouchControls {
    /// The move value: x = right, y = forward, clamped to the unit circle. Zero while no move
    /// finger is down.
    pub fn move_axes(&self) -> Vec2 {
        self.move_axes
    }

    /// The look drag accumulated this frame, in physical pixels. Cleared at the start of every
    /// [`update_touch_controls`] run, like `Mouse::delta`.
    pub fn look_delta(&self) -> Vec2 {
        self.look_delta
    }

    /// Whether a finger currently drives the given role.
    pub fn finger_count(&self, role_is_move: bool) -> usize {
        self.fingers
            .values()
            .filter(|f| (f.role == Role::Move) == role_is_move)
            .count()
    }

    /// Starts a new frame: per-frame accumulators reset. The stick value persists while the
    /// finger is held.
    pub fn begin_frame(&mut self) {
        self.look_delta = Vec2::ZERO;
    }

    /// Folds one touch event in. `window` is the window's physical size, used to split the
    /// screen into halves and to scale the virtual stick.
    pub fn apply(&mut self, id: u64, phase: TouchPhase, position: Vec2, window: (u32, u32)) {
        match phase {
            TouchPhase::Started => {
                let role = if position.x < window.0 as f32 * 0.5 {
                    Role::Move
                } else {
                    Role::Look
                };
                if role == Role::Move {
                    self.move_axes = Vec2::ZERO;
                }
                self.fingers.insert(
                    id,
                    Finger {
                        role,
                        origin: position,
                        last: position,
                    },
                );
            }
            TouchPhase::Moved => {
                let Some(finger) = self.fingers.get_mut(&id) else {
                    return;
                };
                match finger.role {
                    Role::Move => {
                        let offset = position - finger.origin;
                        // Screen y points down; the stick's forward is up.
                        let value = Vec2::new(offset.x, -offset.y) / move_stick_radius(window);
                        self.move_axes = clamp_to_unit_circle(value);
                    }
                    Role::Look => {
                        self.look_delta += position - finger.last;
                    }
                }
                finger.last = position;
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                if let Some(finger) = self.fingers.remove(&id)
                    && finger.role == Role::Move
                {
                    self.move_axes = Vec2::ZERO;
                }
            }
        }
    }
}

/// Scales `value` down to the unit circle; diagonals end at length 1, like `keys2d`.
pub fn clamp_to_unit_circle(value: Vec2) -> Vec2 {
    let length = value.length();
    if length > 1.0 { value / length } else { value }
}

/// `PreUpdate`: folds this frame's [`TouchInput`] messages into [`TouchControls`].
///
/// Runs before `Update`, so `crate::write_move_input` and `crate::write_look_input` see the
/// same frame's touch values. Without a primary window (headless tests) it only resets the
/// per-frame accumulators.
pub fn update_touch_controls(
    mut controls: ResMut<TouchControls>,
    mut touches: MessageReader<TouchInput>,
    windows: Query<&Window, With<PrimaryWindow>>,
) {
    controls.begin_frame();
    let Some(window) = windows.iter().next() else {
        return;
    };
    let size = window.physical_size;
    for touch in touches.read() {
        controls.apply(
            touch.id,
            touch.phase,
            Vec2::new(touch.position.0 as f32, touch.position.1 as f32),
            size,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A phone-ish landscape window: 1920 × 1080 physical pixels.
    const WINDOW: (u32, u32) = (1920, 1080);

    #[test]
    fn stick_radius_is_a_fraction_of_the_shorter_edge_with_a_floor() {
        let radius = move_stick_radius(WINDOW);
        assert!((radius - 1080.0 * 0.16).abs() < 1e-3, "{radius}");
        // Narrow windows clamp to the minimum.
        assert_eq!(move_stick_radius((100, 100)), MOVE_STICK_MIN_PIXELS);
    }

    #[test]
    fn left_half_drag_moves_like_a_stick() {
        let mut controls = TouchControls::default();
        let radius = move_stick_radius(WINDOW);
        let origin = Vec2::new(200.0, 540.0); // left half
        controls.apply(1, TouchPhase::Started, origin, WINDOW);

        // Half the radius to the right: x = 0.5, y = 0.
        controls.apply(
            1,
            TouchPhase::Moved,
            origin + Vec2::new(radius * 0.5, 0.0),
            WINDOW,
        );
        assert!((controls.move_axes().x - 0.5).abs() < 1e-4);
        assert_eq!(controls.move_axes().y, 0.0);

        // Dragging up is forward (+y).
        controls.apply(1, TouchPhase::Moved, origin + Vec2::new(0.0, -radius), WINDOW);
        assert!((controls.move_axes().y - 1.0).abs() < 1e-4);
        assert_eq!(controls.move_axes().x, 0.0);

        // Far beyond the radius clamps to the unit circle.
        controls.apply(
            1,
            TouchPhase::Moved,
            origin + Vec2::new(radius * 5.0, -radius * 5.0),
            WINDOW,
        );
        let value = controls.move_axes();
        assert!(
            (value.length() - 1.0).abs() < 1e-4 && value.x > 0.7 && value.y > 0.7,
            "diagonal clamps to length 1: {value}"
        );

        // Releasing the finger stops the movement.
        controls.apply(1, TouchPhase::Ended, origin, WINDOW);
        assert_eq!(controls.move_axes(), Vec2::ZERO);
    }

    #[test]
    fn right_half_drag_looks_like_mouse_motion() {
        let mut controls = TouchControls::default();
        let start = Vec2::new(1500.0, 400.0); // right half
        controls.apply(2, TouchPhase::Started, start, WINDOW);
        assert_eq!(controls.look_delta(), Vec2::ZERO);

        controls.apply(2, TouchPhase::Moved, start + Vec2::new(60.0, 30.0), WINDOW);
        controls.apply(2, TouchPhase::Moved, start + Vec2::new(75.0, 45.0), WINDOW);
        assert_eq!(controls.look_delta(), Vec2::new(75.0, 45.0));

        // The delta is per frame; the stick value is untouched by look fingers.
        controls.begin_frame();
        assert_eq!(controls.look_delta(), Vec2::ZERO);

        // A cancelled look finger does not zero the stick of another, moving finger.
        controls.apply(1, TouchPhase::Started, Vec2::new(100.0, 500.0), WINDOW);
        controls.apply(1, TouchPhase::Moved, Vec2::new(100.0, 400.0), WINDOW);
        let stick = controls.move_axes();
        assert!(stick.y > 0.0);
        controls.apply(2, TouchPhase::Cancelled, start, WINDOW);
        assert_eq!(controls.move_axes(), stick);
        assert_eq!(controls.finger_count(true), 1);
        assert_eq!(controls.finger_count(false), 0);
    }

    #[test]
    fn stick_persists_across_frames_while_held() {
        let mut controls = TouchControls::default();
        let origin = Vec2::new(300.0, 900.0);
        controls.apply(1, TouchPhase::Started, origin, WINDOW);
        controls.apply(1, TouchPhase::Moved, origin + Vec2::new(0.0, -200.0), WINDOW);
        let held = controls.move_axes();
        assert!(held.y > 0.0);

        controls.begin_frame();
        assert_eq!(controls.move_axes(), held, "held stick keeps its value");
    }

    #[test]
    fn roles_are_decided_by_the_starting_half_not_the_current_one() {
        let mut controls = TouchControls::default();
        // Starts on the left, dragged across to the right: still a move finger.
        let origin = Vec2::new(100.0, 540.0);
        controls.apply(3, TouchPhase::Started, origin, WINDOW);
        controls.apply(3, TouchPhase::Moved, Vec2::new(1800.0, 540.0), WINDOW);
        assert_eq!(
            controls.look_delta(),
            Vec2::ZERO,
            "crossing the middle must not look"
        );
        assert!(controls.move_axes().x > 0.9);
    }
}
