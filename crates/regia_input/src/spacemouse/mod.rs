// SPDX-License-Identifier: MPL-2.0
//! SpaceMouse (6-DoF navigation device) support, `docs/specs/29-spacemouse.md`.
//!
//! One set of defaults for every Regia Engine UI app (the editor, Tincal, Red-Rouge): the merged
//! device state ([`SpaceMouseState`]) goes through the shared pure function [`nav::step`], and
//! each app applies the resulting [`NavDelta`] to its own camera. The constants live in
//! [`NavigationDefaults`]; the user may change only [`SpaceMouseConfig`] (enabled, sensitivity,
//! invert).
//!
//! Desktop only (Linux, Windows): on Android the module does not exist.
//!
//! ```
//! use regia_input::spacemouse::{NavContext, NavigationDefaults, SpaceMouseState, nav};
//!
//! // Twist the cap fully counter-clockwise for a tenth of a second in an orbit viewport.
//! let state = SpaceMouseState { rotation: [0.0, 1.0, 0.0], ..Default::default() };
//! let delta = nav::step(&state, 0.1, NavContext::orbit(5.0));
//! assert!((delta.orbit.x - NavigationDefaults::ROTATION_RATE * 0.1).abs() < 1e-6);
//! ```

mod backend;
pub mod nav;
mod plugin;
pub mod report;

use regia_app::prelude::*;

pub use crate::config::SpaceMouseConfig;
pub use backend::{BackendEvent, FakeSpaceMouse, SpaceMouseBackend, SpaceMouseDeviceInfo, SpaceMouseWaker};
pub use nav::{NavContext, NavDelta, NavMode};

pub use plugin::{
    LINUX_UDEV_RULE, SpaceMouseAction, SpaceMouseBackendSlot, SpaceMouseButton, SpaceMouseConnected,
    SpaceMouseDevice, SpaceMouseDeviceStatus, SpaceMouseDevices, SpaceMouseDisconnected, SpaceMouseInjection,
    SpaceMousePlugin, update_spacemouse,
};

/// The fixed navigation constants of `docs/specs/29-spacemouse.md` §§ 2.1–2.3. Apps do not
/// change these; they are `pub const` so tests, docs and the agent surface quote one source.
#[derive(Debug, Clone, Copy)]
pub struct NavigationDefaults;

impl NavigationDefaults {
    /// Raw axis value at full deflection of a wired SpaceMouse: `n = clamp(raw / 350, -1, 1)`.
    pub const RAW_FULL_SCALE: f32 = 350.0;
    /// Per-axis dead zone on the normalised value.
    pub const DEAD_ZONE: f32 = 0.04;
    /// Exponent of the response curve applied after the dead zone (quadratic).
    pub const CURVE_EXPONENT: f32 = 2.0;
    /// Orbit pan speed at full deflection, in orbit distances per second.
    pub const PAN_RATE: f32 = 1.0;
    /// Orbit zoom rate: `distance ← distance · exp(v.z · 1.5 · dt)`.
    pub const ZOOM_RATE: f32 = 1.5;
    /// Orbit, fly-turn and roll rate at full deflection, radians per second.
    pub const ROTATION_RATE: f32 = 1.5;
    /// Fly speed at full deflection, in `scene_scale` units per second.
    pub const FLY_SPEED: f32 = 1.0;
    /// Pitch limit while horizon lock is on, degrees either side of the horizon.
    pub const PITCH_LIMIT_DEG: f32 = 89.0;
    /// Horizon lock at startup (on).
    pub const HORIZON_LOCK: bool = true;
    /// Dominant-axis mode at startup (off).
    pub const DOMINANT_AXIS: bool = false;
    /// The longest frame [`nav::step`] integrates, seconds: a hitch never throws the camera.
    pub const MAX_DT: f32 = 0.1;
    /// The default navigation mode of object and asset viewports.
    pub const MODE: NavMode = NavMode::Orbit;
    /// View2D pan speed at full deflection, in visible extents per second.
    pub const VIEW2D_PAN_RATE: f32 = 1.0;
    /// Timeline scrub speed at full twist, in visible spans per second.
    pub const TIMELINE_SCRUB_RATE: f32 = 1.0;
    /// Step: the shaped `|rz|` past which one step fires.
    pub const STEP_THRESHOLD: f32 = 0.5;
    /// Step: the auto-repeat interval while the tilt is held past the threshold, seconds.
    pub const STEP_REPEAT_S: f32 = 0.35;
    /// Object tool: rest time that ends a gesture (one undo entry per gesture), seconds.
    pub const GESTURE_REST_S: f32 = 0.3;
}

/// Action names of the device buttons (§ 2.5). Stable: apps and agents match on these strings.
pub mod actions {
    /// Button 1 (left): frame the selection, or everything when nothing is selected.
    pub const FIT_SELECTION: &str = "view.fit_selection";
    /// Button 2 (right): reset to the app's home view.
    pub const HOME: &str = "view.home";
    /// Button 3: toggle dominant-axis mode.
    pub const DOMINANT_TOGGLE: &str = "spacemouse.dominant_toggle";
    /// Button 4 (and the UI action of the same name): toggle horizon lock.
    pub const HORIZON_LOCK_TOGGLE: &str = "spacemouse.horizon_lock_toggle";
    /// The opt-in Object tool (§ 2.3b): no device button by default; apps bind it in their UI.
    pub const OBJECT_TOOL_TOGGLE: &str = "spacemouse.object_tool_toggle";
}

/// The default action of device button `button` (1-based, § 2.5): buttons 5 and up are
/// `spacemouse.button.N`, unassigned.
pub fn button_action(button: u8) -> String {
    match button {
        1 => actions::FIT_SELECTION.to_string(),
        2 => actions::HOME.to_string(),
        3 => actions::DOMINANT_TOGGLE.to_string(),
        4 => actions::HORIZON_LOCK_TOGGLE.to_string(),
        n => format!("spacemouse.button.{n}"),
    }
}

/// The latest merged state of every connected SpaceMouse (§ 3): normalised per § 2.1
/// (`clamp(raw / 350, -1, 1)`, spec frame +X right, +Y up, +Z toward the viewer), **before**
/// the dead zone. Updated once per frame in `PreUpdate`; read it and call [`nav::step`].
///
/// Rotation axes: `rx` pitch (tilt the cap away from you = positive), `ry` yaw (twist
/// counter-clockwise seen from above = positive), `rz` roll (tilt the cap left = positive).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct SpaceMouseState {
    /// `x, y, z` in -1..=1.
    pub translation: [f32; 3],
    /// `rx, ry, rz` in -1..=1.
    pub rotation: [f32; 3],
    /// Pressed buttons, bit `n - 1` for button `n`.
    pub buttons: u32,
}

impl SpaceMouseState {
    /// A state from raw device values already in the spec frame (§ 2.1 normalisation applied).
    pub fn from_raw(translation: [i32; 3], rotation: [i32; 3], buttons: u32) -> Self {
        let n = |raw: i32| (raw as f32 / NavigationDefaults::RAW_FULL_SCALE).clamp(-1.0, 1.0);
        Self {
            translation: translation.map(n),
            rotation: rotation.map(n),
            buttons,
        }
    }

    /// The six axes `x, y, z, rx, ry, rz`.
    pub fn axes(&self) -> [f32; 6] {
        let [x, y, z] = self.translation;
        let [rx, ry, rz] = self.rotation;
        [x, y, z, rx, ry, rz]
    }

    /// Whether button `button` (1-based) is held.
    pub fn pressed(&self, button: u8) -> bool {
        (1..=32).contains(&button) && self.buttons & (1 << (button - 1)) != 0
    }

    /// True when no axis is deflected and no button is held.
    pub fn is_idle(&self) -> bool {
        self.buttons == 0 && self.axes().iter().all(|v| *v == 0.0)
    }

    /// The sum of several states, each axis clamped to -1..=1 and the buttons OR-ed (§ 1: all
    /// devices feed one merged state).
    pub fn merged<'a>(states: impl IntoIterator<Item = &'a SpaceMouseState>) -> Self {
        let mut out = Self::default();
        for s in states {
            for i in 0..3 {
                out.translation[i] += s.translation[i];
                out.rotation[i] += s.rotation[i];
            }
            out.buttons |= s.buttons;
        }
        out.translation = out.translation.map(|v| v.clamp(-1.0, 1.0));
        out.rotation = out.rotation.map(|v| v.clamp(-1.0, 1.0));
        out
    }
}

/// The runtime navigation toggles (§§ 2.2–2.3): horizon lock (Button 4, default on) and
/// dominant-axis mode (Button 3, default off). The plugin flips them on the device buttons;
/// apps pass them to [`nav::step`] with [`NavContext::with_toggles`] and may flip them from a UI
/// action of the same name.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SpaceMouseNav {
    /// Horizon lock: no roll, pitch within ±89°.
    pub horizon_lock: bool,
    /// Dominant-axis mode: only the largest axis moves the camera.
    pub dominant: bool,
    /// The opt-in Object tool (§ 2.3b, default off): the cap moves the selection instead of the
    /// camera. Apps that offer it flip it on `actions::OBJECT_TOOL_TOGGLE`.
    pub object_tool: bool,
}

impl Default for SpaceMouseNav {
    fn default() -> Self {
        Self {
            horizon_lock: NavigationDefaults::HORIZON_LOCK,
            dominant: NavigationDefaults::DOMINANT_AXIS,
            object_tool: false,
        }
    }
}

/// Where an Object-tool gesture is (§ 2.3b), from [`ObjectGesture::update`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GesturePhase {
    /// No gesture.
    Idle,
    /// The cap left the rest position: open one undo entry (capture the start state).
    Began,
    /// The gesture continues (moving, or resting for less than the rest time).
    Moving,
    /// The cap rested for [`NavigationDefaults::GESTURE_REST_S`]: commit the one undo entry.
    Ended,
}

/// Groups Object-tool motion into gestures (§ 2.3b): a gesture runs from the first deflection
/// past the dead zone until the cap has rested for [`NavigationDefaults::GESTURE_REST_S`], and is
/// one undo entry in the app's command layer. Pure: feed it every frame.
///
/// ```
/// use regia_input::spacemouse::{GesturePhase, ObjectGesture, SpaceMouseState};
///
/// let mut gesture = ObjectGesture::default();
/// let pushed = SpaceMouseState { translation: [0.5, 0.0, 0.0], ..Default::default() };
/// assert_eq!(gesture.update(&pushed, 0.016), GesturePhase::Began);
/// assert_eq!(gesture.update(&SpaceMouseState::default(), 0.2), GesturePhase::Moving);
/// assert_eq!(gesture.update(&SpaceMouseState::default(), 0.2), GesturePhase::Ended);
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ObjectGesture {
    active: bool,
    rest: f32,
}

impl ObjectGesture {
    /// Advances by one frame of `dt` seconds with the latest merged state.
    pub fn update(&mut self, state: &SpaceMouseState, dt: f32) -> GesturePhase {
        let deflected = state.axes().iter().any(|n| nav::shape_axis(*n) != 0.0);
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        match (self.active, deflected) {
            (false, false) => GesturePhase::Idle,
            (false, true) => {
                self.active = true;
                self.rest = 0.0;
                GesturePhase::Began
            }
            (true, true) => {
                self.rest = 0.0;
                GesturePhase::Moving
            }
            (true, false) => {
                self.rest += dt;
                if self.rest >= NavigationDefaults::GESTURE_REST_S {
                    self.active = false;
                    self.rest = 0.0;
                    GesturePhase::Ended
                } else {
                    GesturePhase::Moving
                }
            }
        }
    }

    /// True between [`GesturePhase::Began`] and [`GesturePhase::Ended`].
    pub fn is_active(&self) -> bool {
        self.active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_values_normalise_and_merge() {
        let s = SpaceMouseState::from_raw([350, -175, 700], [0, -350, 35], 0b101);
        assert_eq!(s.translation, [1.0, -0.5, 1.0], "clamped at full scale");
        assert_eq!(s.rotation, [0.0, -1.0, 0.1]);
        assert!(s.pressed(1) && !s.pressed(2) && s.pressed(3));
        assert!(!s.pressed(0) && !s.pressed(33));
        let other = SpaceMouseState::from_raw([350, 0, 0], [0, 0, 0], 0b10);
        let merged = SpaceMouseState::merged([&s, &other]);
        assert_eq!(merged.translation[0], 1.0, "sum clamped");
        assert_eq!(merged.buttons, 0b111);
        assert!(SpaceMouseState::default().is_idle());
    }

    /// § 5: one Object-tool gesture is exactly one undo entry (fake command layer).
    #[test]
    fn one_gesture_is_one_undo_entry() {
        #[derive(Default)]
        struct FakeCommands {
            open: Option<f32>,
            entries: Vec<(f32, f32)>,
        }
        let mut commands = FakeCommands::default();
        let mut gesture = ObjectGesture::default();
        let mut position = 0.0_f32;
        let pushed = SpaceMouseState {
            translation: [1.0, 0.0, 0.0],
            ..Default::default()
        };
        let rest = SpaceMouseState::default();
        let dt = 1.0 / 60.0;
        // Push 0.5 s, rest 0.1 s (shorter than the rest time), push 0.2 s, rest 0.5 s.
        let frames = std::iter::repeat_n(pushed, 30)
            .chain(std::iter::repeat_n(rest, 6))
            .chain(std::iter::repeat_n(pushed, 12))
            .chain(std::iter::repeat_n(rest, 30));
        for state in frames {
            match gesture.update(&state, dt) {
                GesturePhase::Began => commands.open = Some(position),
                GesturePhase::Ended => {
                    if let Some(start) = commands.open.take() {
                        commands.entries.push((start, position));
                    }
                }
                GesturePhase::Moving | GesturePhase::Idle => {}
            }
            position += nav::step(&state, dt, NavContext::orbit(1.0)).pan.x;
        }
        assert_eq!(commands.entries.len(), 1, "one gesture, one undo entry");
        assert!(commands.entries[0].1 > 0.5, "the entry spans the whole gesture");
        assert!(!gesture.is_active());
        assert_eq!(
            gesture.update(&pushed, dt),
            GesturePhase::Began,
            "the next push starts a second gesture"
        );
    }

    #[test]
    fn button_actions_follow_the_table() {
        assert_eq!(button_action(1), "view.fit_selection");
        assert_eq!(button_action(2), "view.home");
        assert_eq!(button_action(3), "spacemouse.dominant_toggle");
        assert_eq!(button_action(4), "spacemouse.horizon_lock_toggle");
        assert_eq!(button_action(7), "spacemouse.button.7");
    }

    #[test]
    fn gesture_rest_boundary_ignores_the_dead_zone_and_a_non_finite_dt() {
        let mut gesture = ObjectGesture::default();
        let inside = SpaceMouseState {
            translation: [NavigationDefaults::DEAD_ZONE, 0.0, 0.0],
            ..SpaceMouseState::default()
        };
        assert_eq!(gesture.update(&inside, 1.0), GesturePhase::Idle);
        let buttons = SpaceMouseState {
            buttons: 0b1,
            ..SpaceMouseState::default()
        };
        assert_eq!(gesture.update(&buttons, 1.0), GesturePhase::Idle);
        assert!(!gesture.is_active());

        let nudge = SpaceMouseState {
            translation: [0.05, 0.0, 0.0],
            ..SpaceMouseState::default()
        };
        assert_eq!(gesture.update(&nudge, f32::NAN), GesturePhase::Began);
        assert!(gesture.is_active());
        assert_eq!(
            gesture.update(&SpaceMouseState::default(), f32::NEG_INFINITY),
            GesturePhase::Moving
        );
        assert!(gesture.is_active());
        assert_eq!(
            gesture.update(&SpaceMouseState::default(), 0.0),
            GesturePhase::Moving
        );
        assert_eq!(
            gesture.update(&SpaceMouseState::default(), NavigationDefaults::GESTURE_REST_S),
            GesturePhase::Ended
        );
        assert!(!gesture.is_active());

        assert_eq!(gesture.update(&nudge, -1.0), GesturePhase::Began);
        assert_eq!(
            gesture.update(&SpaceMouseState::default(), 0.299),
            GesturePhase::Moving
        );
        assert!(gesture.is_active());
        assert_eq!(
            gesture.update(&SpaceMouseState::default(), 0.0),
            GesturePhase::Moving
        );
    }
}
