// SPDX-License-Identifier: MPL-2.0
//! Touch → action mapping without UI widgets (ADR-527): [`TouchControls`] maps screen zones
//! and recognised gestures to actions, through the same [`InputInjection`] path the UI lane's
//! virtual controls use (specs 11 § 7), so trigger and phase evaluation is the same as for a key.
//!
//! - A [`TouchZone`] is a rectangle in normalized window coordinates (0..1, origin top-left,
//!   y down). The first finger that lands in it claims it until the finger lifts; the zone holds
//!   its action at `1.0` ([`TouchControlKind::Button`]) or at the finger's displacement from where
//!   it landed ([`TouchControlKind::Stick`], a floating stick: y up, unit length at `radius_px`).
//! - A [`GestureBinding`] pulses an action for a few frames when a [`GestureEvent`] of its kind is
//!   recognised (optionally only inside a rectangle). A zone claims the finger that starts inside
//!   it. Gesture suppression uses the gesture's own point: a tap, double tap or long press uses
//!   its `position` (the lift or the hold), a swipe uses its `start`, and a pinch or two-finger
//!   pan uses its center.
//!
//! The mapping is plain data set from code (`app.insert_resource(TouchControls { .. })`); the
//! input config's RON shape is unchanged. Agents read the live state with `input.touch_controls`.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::{Vec2, Vec3};
use regia_platform::{PrimaryWindow, TouchPhase, Window};

use crate::InputInjection;
use crate::touch::{GestureEvent, TouchPoint, Touches};

/// A rectangle in normalized window coordinates (0..1, origin top-left, y down).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchRect {
    /// Top-left corner.
    pub min: Vec2,
    /// Bottom-right corner.
    pub max: Vec2,
}

impl TouchRect {
    /// A rectangle from its corners.
    #[must_use]
    pub fn new(min: Vec2, max: Vec2) -> Self {
        Self { min, max }
    }

    /// The left half of the screen (a common move-stick zone).
    #[must_use]
    pub fn left_half() -> Self {
        Self::new(Vec2::ZERO, Vec2::new(0.5, 1.0))
    }

    /// The right half of the screen.
    #[must_use]
    pub fn right_half() -> Self {
        Self::new(Vec2::new(0.5, 0.0), Vec2::ONE)
    }

    /// Whether the normalized point lies inside (edges included).
    #[must_use]
    pub fn contains(&self, p: Vec2) -> bool {
        p.x >= self.min.x && p.x <= self.max.x && p.y >= self.min.y && p.y <= self.max.y
    }
}

/// What a claimed zone writes to its action.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TouchControlKind {
    /// `1.0` on x while a finger holds the zone.
    Button,
    /// A floating stick centred where the finger landed: displacement / `radius_px`, y up,
    /// clamped to unit length; at or below `dead_zone`, after that fraction is clamped to
    /// `0.0..=0.99`, it reads zero.
    Stick {
        /// Physical pixels of travel for a full deflection.
        radius_px: f32,
        /// Fraction of the radius that reads as zero, compared with `<=` after clamping to
        /// `0.0..=0.99`. A value of `1.0` therefore clamps to `0.99` and a full deflection is not zero.
        dead_zone: f32,
    },
}

/// A screen zone bound to an action.
#[derive(Debug, Clone, PartialEq)]
pub struct TouchZone {
    /// Name shown by `input.touch_controls`.
    pub name: String,
    /// Where a finger must land to claim it.
    pub rect: TouchRect,
    /// The action it drives (declared in the `InputConfig`).
    pub action: String,
    /// Button or stick.
    pub kind: TouchControlKind,
}

/// Which recognised gesture a [`GestureBinding`] reacts to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GestureKind {
    /// [`GestureEvent::Tap`].
    Tap,
    /// [`GestureEvent::DoubleTap`].
    DoubleTap,
    /// [`GestureEvent::LongPress`].
    LongPress,
    /// A [`GestureEvent::Swipe`] in any direction (the value is the direction, y up).
    Swipe,
    /// A swipe up.
    SwipeUp,
    /// A swipe down.
    SwipeDown,
    /// A swipe left.
    SwipeLeft,
    /// A swipe right.
    SwipeRight,
    /// [`GestureEvent::Pinch`]: the value's x is `scale - 1` (positive = apart).
    Pinch,
    /// [`GestureEvent::TwoFingerPan`]: the value is the centroid movement in screen heights, y up.
    TwoFingerPan,
}

impl GestureKind {
    /// Stable lowercase name (agent protocol).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Tap => "tap",
            Self::DoubleTap => "double_tap",
            Self::LongPress => "long_press",
            Self::Swipe => "swipe",
            Self::SwipeUp => "swipe_up",
            Self::SwipeDown => "swipe_down",
            Self::SwipeLeft => "swipe_left",
            Self::SwipeRight => "swipe_right",
            Self::Pinch => "pinch",
            Self::TwoFingerPan => "two_finger_pan",
        }
    }
}

/// A gesture bound to an action.
#[derive(Debug, Clone, PartialEq)]
pub struct GestureBinding {
    /// The gesture.
    pub gesture: GestureKind,
    /// The action it pulses.
    pub action: String,
    /// Only gestures inside this rectangle (by their start / centre); `None` = anywhere.
    pub rect: Option<TouchRect>,
    /// Frames the action is held (at least 1).
    pub frames: u64,
}

/// What one evaluation asks of [`InputInjection`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TouchControlOutput {
    /// Actions to hold at a value this frame.
    pub holds: Vec<(String, Vec3)>,
    /// Actions no zone holds any more.
    pub releases: Vec<String>,
    /// Gesture pulses: action, value, frames.
    pub pulses: Vec<(String, Vec3, u64)>,
}

/// Touch zones and gesture bindings (see the module docs). Absent = no mapping.
#[derive(Resource, Debug, Clone, Default, PartialEq)]
pub struct TouchControls {
    /// Zones, in priority order (a finger claims the first zone it lands in).
    pub zones: Vec<TouchZone>,
    /// Gesture bindings.
    pub gestures: Vec<GestureBinding>,
    /// Finger id → zone index.
    claims: BTreeMap<u64, usize>,
    /// Actions the zones held at the last evaluation.
    held: Vec<String>,
}

impl TouchControls {
    /// Which zone (index) each finger holds.
    pub fn claims(&self) -> impl Iterator<Item = (u64, usize)> + '_ {
        self.claims.iter().map(|(f, z)| (*f, *z))
    }

    /// Actions the zones hold now.
    #[must_use]
    pub fn held_actions(&self) -> &[String] {
        &self.held
    }

    /// Maps this frame's touches and gestures; `screen` is the window's physical size.
    pub fn evaluate<'a>(
        &mut self,
        touches: impl IntoIterator<Item = &'a TouchPoint>,
        gestures: &[GestureEvent],
        screen: Vec2,
    ) -> TouchControlOutput {
        let mut out = TouchControlOutput::default();
        if screen.x <= 0.0 || screen.y <= 0.0 {
            // No usable window (minimized or destroyed): nothing can be touched, so release
            // whatever was held rather than leave the last stick vector injected.
            self.claims.clear();
            out.releases = std::mem::take(&mut self.held);
            return out;
        }
        let norm = |p: Vec2| p / screen;
        let touches: Vec<&TouchPoint> = touches.into_iter().collect();
        let down = |p: &TouchPoint| matches!(p.phase, TouchPhase::Started | TouchPhase::Moved);

        // Claims end when the finger lifts or is no longer tracked.
        self.claims
            .retain(|id, _| touches.iter().any(|p| p.id == *id && down(p)));
        for point in touches.iter().filter(|p| down(p)) {
            if self.claims.contains_key(&point.id) {
                continue;
            }
            let start = norm(point.start);
            let free = (0..self.zones.len())
                .find(|i| self.zones[*i].rect.contains(start) && !self.claims.values().any(|c| c == i));
            if let Some(zone) = free {
                self.claims.insert(point.id, zone);
            }
        }

        let mut held = Vec::new();
        for (id, zone) in &self.claims {
            let (Some(point), Some(zone)) = (touches.iter().find(|p| p.id == *id), self.zones.get(*zone))
            else {
                continue;
            };
            let value = match zone.kind {
                TouchControlKind::Button => Vec3::X,
                TouchControlKind::Stick { radius_px, dead_zone } => stick_value(point, radius_px, dead_zone),
            };
            out.holds.push((zone.action.clone(), value));
            held.push(zone.action.clone());
        }
        for action in &self.held {
            if !held.contains(action) && !out.releases.contains(action) {
                out.releases.push(action.clone());
            }
        }
        self.held = held;

        let in_zone = |p: Vec2| self.zones.iter().any(|z| z.rect.contains(norm(p)));
        for gesture in gestures {
            let (at, value) = gesture_value(gesture, screen);
            if in_zone(at) {
                continue;
            }
            for binding in &self.gestures {
                if matches_kind(binding.gesture, gesture) && binding.rect.is_none_or(|r| r.contains(norm(at)))
                {
                    out.pulses
                        .push((binding.action.clone(), value, binding.frames.max(1)));
                }
            }
        }
        out
    }
}

/// The stick value for a finger: displacement from where it landed, y up, unit length at
/// `radius_px`, zero inside the dead zone.
fn stick_value(point: &TouchPoint, radius_px: f32, dead_zone: f32) -> Vec3 {
    if radius_px <= 0.0 {
        return Vec3::ZERO;
    }
    let d = point.displacement() / radius_px;
    let v = Vec2::new(d.x, -d.y);
    let len = v.length();
    if len <= dead_zone.clamp(0.0, 0.99) {
        return Vec3::ZERO;
    }
    let v = if len > 1.0 { v / len } else { v };
    Vec3::new(v.x, v.y, 0.0)
}

/// Where a gesture happened (pixels) and the value its bindings write.
fn gesture_value(gesture: &GestureEvent, screen: Vec2) -> (Vec2, Vec3) {
    match *gesture {
        GestureEvent::Tap { position }
        | GestureEvent::DoubleTap { position }
        | GestureEvent::LongPress { position } => (position, Vec3::X),
        GestureEvent::Swipe { direction, start, .. } => (start, Vec3::new(direction.x, direction.y, 0.0)),
        GestureEvent::Pinch { scale, center } => (center, Vec3::new(scale - 1.0, 0.0, 0.0)),
        GestureEvent::TwoFingerPan { delta, center } => {
            (center, Vec3::new(delta.x / screen.y, -delta.y / screen.y, 0.0))
        }
    }
}

fn matches_kind(kind: GestureKind, gesture: &GestureEvent) -> bool {
    match (kind, gesture) {
        (GestureKind::Tap, GestureEvent::Tap { .. })
        | (GestureKind::DoubleTap, GestureEvent::DoubleTap { .. })
        | (GestureKind::LongPress, GestureEvent::LongPress { .. })
        | (GestureKind::Swipe, GestureEvent::Swipe { .. })
        | (GestureKind::Pinch, GestureEvent::Pinch { .. })
        | (GestureKind::TwoFingerPan, GestureEvent::TwoFingerPan { .. }) => true,
        (GestureKind::SwipeUp, GestureEvent::Swipe { direction, .. }) => direction.y > 0.5,
        (GestureKind::SwipeDown, GestureEvent::Swipe { direction, .. }) => direction.y < -0.5,
        (GestureKind::SwipeRight, GestureEvent::Swipe { direction, .. }) => direction.x > 0.5,
        (GestureKind::SwipeLeft, GestureEvent::Swipe { direction, .. }) => direction.x < -0.5,
        _ => false,
    }
}

/// `PreUpdate` (in `InputSet::Devices`, after the touch update): applies [`TouchControls`] to
/// [`InputInjection`]. Without a primary window (or a zero-size one) it releases whatever the zones held; does nothing without the resource.
pub(crate) fn apply_touch_controls(
    controls: Option<ResMut<TouchControls>>,
    touches: Res<Touches>,
    mut gestures: MessageReader<GestureEvent>,
    windows: Query<&Window, With<PrimaryWindow>>,
    injection: Option<ResMut<InputInjection>>,
    time: Res<Time>,
) {
    let gestures: Vec<GestureEvent> = gestures.read().copied().collect();
    let (Some(mut controls), Some(mut injection)) = (controls, injection) else {
        return;
    };
    let screen = windows.iter().next().map_or(Vec2::ZERO, |window| {
        Vec2::new(window.physical_size.0 as f32, window.physical_size.1 as f32)
    });
    let out = controls.evaluate(touches.iter(), &gestures, screen);
    apply_output(&mut injection, out, time.frame_count());
}

fn apply_output(injection: &mut InputInjection, out: TouchControlOutput, frame: u64) {
    for action in &out.releases {
        injection.release(action);
    }
    for (action, value) in out.holds {
        injection.hold(action, value);
    }
    for (action, value, frames) in out.pulses {
        injection.schedule(action, value, frame, frames);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: Vec2 = Vec2::new(1000.0, 500.0);

    fn finger(id: u64, start: (f32, f32), current: (f32, f32), phase: TouchPhase) -> TouchPoint {
        TouchPoint {
            id,
            start: Vec2::new(start.0, start.1),
            previous: Vec2::new(current.0, current.1),
            current: Vec2::new(current.0, current.1),
            phase,
            pressure: 1.0,
            age: 0.0,
            travelled: 0.0,
        }
    }

    fn controls() -> TouchControls {
        TouchControls {
            zones: vec![
                TouchZone {
                    name: "move".into(),
                    rect: TouchRect::left_half(),
                    action: "move".into(),
                    kind: TouchControlKind::Stick {
                        radius_px: 100.0,
                        dead_zone: 0.1,
                    },
                },
                TouchZone {
                    name: "fire".into(),
                    rect: TouchRect::new(Vec2::new(0.8, 0.6), Vec2::ONE),
                    action: "fire".into(),
                    kind: TouchControlKind::Button,
                },
            ],
            gestures: vec![
                GestureBinding {
                    gesture: GestureKind::SwipeUp,
                    action: "jump".into(),
                    rect: None,
                    frames: 2,
                },
                GestureBinding {
                    gesture: GestureKind::Tap,
                    action: "interact".into(),
                    rect: None,
                    frames: 1,
                },
            ],
            ..Default::default()
        }
    }

    #[test]
    fn stick_follows_the_finger_from_where_it_landed() {
        let mut c = controls();
        let out = c.evaluate(
            &[finger(1, (200.0, 250.0), (250.0, 150.0), TouchPhase::Moved)],
            &[],
            SCREEN,
        );
        assert_eq!(out.holds.len(), 1);
        let (action, v) = &out.holds[0];
        assert_eq!(action, "move");
        // 50 px right, 100 px up (y down on screen): clamped to unit length, y up.
        assert!((v.length() - 1.0).abs() < 1e-5 && v.y > v.x && v.x > 0.0, "{v:?}");
        let out = c.evaluate(
            &[finger(1, (200.0, 250.0), (205.0, 250.0), TouchPhase::Moved)],
            &[],
            SCREEN,
        );
        assert_eq!(out.holds[0].1, Vec3::ZERO, "inside the dead zone");
    }

    #[test]
    fn a_zone_is_held_until_its_finger_lifts_and_one_finger_per_zone() {
        let mut c = controls();
        let down = [
            finger(1, (900.0, 400.0), (900.0, 400.0), TouchPhase::Started),
            finger(2, (950.0, 450.0), (950.0, 450.0), TouchPhase::Started),
        ];
        let out = c.evaluate(&down, &[], SCREEN);
        assert_eq!(
            out.holds,
            [("fire".to_owned(), Vec3::X)],
            "the second finger finds the zone taken"
        );
        assert_eq!(c.claims().collect::<Vec<_>>(), [(1, 1)]);
        let up = [finger(1, (900.0, 400.0), (900.0, 400.0), TouchPhase::Ended)];
        let out = c.evaluate(&up, &[], SCREEN);
        assert!(out.holds.is_empty());
        assert_eq!(out.releases, ["fire"]);
        assert!(c.held_actions().is_empty());
        let out = c.evaluate(&[], &[], SCREEN);
        assert!(out.releases.is_empty(), "released once");
    }

    #[test]
    fn gesture_suppression_uses_the_gesture_point() {
        let mut controls = TouchControls {
            zones: vec![TouchZone {
                name: "move".into(),
                rect: TouchRect::left_half(),
                action: "move".into(),
                kind: TouchControlKind::Button,
            }],
            gestures: vec![
                GestureBinding {
                    gesture: GestureKind::Tap,
                    action: "tap".into(),
                    rect: None,
                    frames: 1,
                },
                GestureBinding {
                    gesture: GestureKind::Swipe,
                    action: "swipe".into(),
                    rect: None,
                    frames: 1,
                },
            ],
            ..TouchControls::default()
        };
        // SCREEN is 1000×500. The left half is normalized x ≤ 0.5, so pixel x = 495 is inside
        // and x = 510 is outside. A tap has no start: suppression reads `position`.
        let tap_lifted_outside = GestureEvent::Tap {
            position: Vec2::new(510.0, 500.0),
        };
        assert_eq!(
            controls.evaluate(&[], &[tap_lifted_outside], SCREEN).pulses,
            vec![("tap".to_string(), Vec3::X, 1)],
            "a tap pulses from its lift point"
        );
        let swipe_started_inside = GestureEvent::Swipe {
            direction: Vec2::X,
            start: Vec2::new(495.0, 500.0),
            end: Vec2::new(600.0, 500.0),
        };
        assert!(
            controls
                .evaluate(&[], &[swipe_started_inside], SCREEN)
                .pulses
                .is_empty(),
            "a swipe is suppressed from its start"
        );
        let tap_outside = GestureEvent::Tap {
            position: Vec2::new(510.0, 250.0),
        };
        assert_eq!(
            controls.evaluate(&[], &[tap_outside], SCREEN).pulses.len(),
            1,
            "a tap whose position is outside the zone still pulses"
        );
    }

    #[test]
    fn gestures_pulse_outside_zones_only() {
        let mut c = controls();
        let swipe_up = GestureEvent::Swipe {
            direction: Vec2::Y,
            start: Vec2::new(700.0, 200.0),
            end: Vec2::new(700.0, 50.0),
        };
        let tap_in_stick_zone = GestureEvent::Tap {
            position: Vec2::new(100.0, 100.0),
        };
        let out = c.evaluate(&[], &[swipe_up, tap_in_stick_zone], SCREEN);
        assert_eq!(out.pulses, [("jump".to_owned(), Vec3::Y, 2)]);
        let swipe_left = GestureEvent::Swipe {
            direction: Vec2::NEG_X,
            start: Vec2::new(700.0, 200.0),
            end: Vec2::new(600.0, 200.0),
        };
        assert!(
            c.evaluate(&[], &[swipe_left], SCREEN).pulses.is_empty(),
            "only up is bound"
        );
    }

    #[test]
    fn no_window_size_maps_nothing() {
        let mut c = controls();
        let out = c.evaluate(
            &[finger(1, (1.0, 1.0), (1.0, 1.0), TouchPhase::Started)],
            &[],
            Vec2::ZERO,
        );
        assert_eq!(out, TouchControlOutput::default());
    }

    #[test]
    fn losing_the_window_releases_held_zones() {
        let mut c = controls();
        let held = [finger(1, (200.0, 250.0), (250.0, 150.0), TouchPhase::Moved)];
        let out = c.evaluate(&held, &[], SCREEN);
        assert!(!out.holds.is_empty());
        let out = c.evaluate(&held, &[], Vec2::ZERO);
        assert!(out.holds.is_empty());
        assert_eq!(
            out.releases,
            vec!["move".to_string()],
            "stick released, not left injected"
        );
        assert!(c.held_actions().is_empty());
        // The window returns with the finger still down: the zone is claimed afresh.
        let out = c.evaluate(&held, &[], SCREEN);
        assert_eq!(out.holds.len(), 1);
    }

    #[test]
    fn output_drives_the_injection_like_a_virtual_control() {
        let mut injection = InputInjection::default();
        let mut c = controls();
        let out = c.evaluate(
            &[finger(1, (900.0, 400.0), (900.0, 400.0), TouchPhase::Started)],
            &[GestureEvent::Tap {
                position: Vec2::new(700.0, 100.0),
            }],
            SCREEN,
        );
        apply_output(&mut injection, out, 10);
        assert_eq!(injection.value_for("fire", 10), Some(Vec3::X));
        assert_eq!(injection.value_for("interact", 10), Some(Vec3::X));
        assert_eq!(injection.value_for("interact", 11), None, "a one-frame pulse");
        let out = c.evaluate(&[], &[], SCREEN);
        apply_output(&mut injection, out, 11);
        assert_eq!(injection.value_for("fire", 11), None, "released with the finger");
    }

    #[test]
    fn stick_dead_zone_gesture_values_and_zone_edges() {
        assert!(TouchRect::left_half().contains(Vec2::ZERO));
        assert!(TouchRect::left_half().contains(Vec2::new(0.5, 1.0)));
        assert!(!TouchRect::left_half().contains(Vec2::new(0.75, 0.5)));
        assert!(TouchRect::right_half().contains(Vec2::new(0.5, 0.0)));
        assert!(TouchRect::right_half().contains(Vec2::ONE));
        assert!(!TouchRect::right_half().contains(Vec2::new(0.25, 0.5)));
        assert_eq!(
            [
                GestureKind::Tap.name(),
                GestureKind::DoubleTap.name(),
                GestureKind::LongPress.name(),
                GestureKind::Swipe.name(),
                GestureKind::SwipeUp.name(),
                GestureKind::SwipeDown.name(),
                GestureKind::SwipeLeft.name(),
                GestureKind::SwipeRight.name(),
                GestureKind::Pinch.name(),
                GestureKind::TwoFingerPan.name(),
            ],
            [
                "tap",
                "double_tap",
                "long_press",
                "swipe",
                "swipe_up",
                "swipe_down",
                "swipe_left",
                "swipe_right",
                "pinch",
                "two_finger_pan",
            ]
        );

        let mut stick = TouchControls {
            zones: vec![TouchZone {
                name: "move".into(),
                rect: TouchRect::left_half(),
                action: "move".into(),
                kind: TouchControlKind::Stick {
                    radius_px: 100.0,
                    dead_zone: 0.2,
                },
            }],
            ..TouchControls::default()
        };
        let at = |controls: &mut TouchControls, current: (f32, f32)| {
            controls.evaluate(
                &[finger(1, (100.0, 100.0), current, TouchPhase::Moved)],
                &[],
                SCREEN,
            )
        };
        assert_eq!(
            at(&mut stick, (120.0, 100.0)).holds,
            vec![("move".to_string(), Vec3::ZERO)],
            "exactly the dead zone is zero"
        );
        assert_eq!(
            at(&mut stick, (140.0, 100.0)).holds,
            vec![("move".to_string(), Vec3::new(0.4, 0.0, 0.0))]
        );
        assert_eq!(
            at(&mut stick, (100.0, 60.0)).holds,
            vec![("move".to_string(), Vec3::new(0.0, 0.4, 0.0))],
            "screen-up is +y"
        );
        assert_eq!(
            at(&mut stick, (200.0, 100.0)).holds,
            vec![("move".to_string(), Vec3::X)],
            "full radius is exactly one, not renormalised"
        );
        assert_eq!(
            at(&mut stick, (250.0, 100.0)).holds,
            vec![("move".to_string(), Vec3::X)],
            "past the radius clamps to unit length"
        );
        stick.zones[0].kind = TouchControlKind::Stick {
            radius_px: 100.0,
            dead_zone: 1.0,
        };
        assert_eq!(
            at(&mut stick, (200.0, 100.0)).holds,
            vec![("move".to_string(), Vec3::X)],
            "dead_zone 1.0 clamps to 0.99, so a full deflection is not zero"
        );
        stick.zones[0].kind = TouchControlKind::Stick {
            radius_px: 0.0,
            dead_zone: 0.0,
        };
        assert_eq!(at(&mut stick, (250.0, 100.0)).holds[0].1, Vec3::ZERO);
        stick.zones[0].kind = TouchControlKind::Stick {
            radius_px: -5.0,
            dead_zone: 0.0,
        };
        assert_eq!(at(&mut stick, (250.0, 100.0)).holds[0].1, Vec3::ZERO);

        // A finger claims the first zone that contains where it landed. x = 0.5 is in both halves.
        let mut halves = TouchControls {
            zones: vec![
                TouchZone {
                    name: "left".into(),
                    rect: TouchRect::left_half(),
                    action: "left".into(),
                    kind: TouchControlKind::Button,
                },
                TouchZone {
                    name: "right".into(),
                    rect: TouchRect::right_half(),
                    action: "right".into(),
                    kind: TouchControlKind::Button,
                },
            ],
            ..TouchControls::default()
        };
        assert_eq!(
            halves
                .evaluate(
                    &[finger(1, (500.0, 250.0), (500.0, 250.0), TouchPhase::Started)],
                    &[],
                    SCREEN
                )
                .holds,
            vec![("left".to_string(), Vec3::X)]
        );
        assert_eq!(
            halves
                .evaluate(
                    &[
                        finger(1, (500.0, 250.0), (500.0, 250.0), TouchPhase::Moved),
                        finger(2, (750.0, 250.0), (750.0, 250.0), TouchPhase::Started),
                    ],
                    &[],
                    SCREEN,
                )
                .holds,
            vec![("left".to_string(), Vec3::X), ("right".to_string(), Vec3::X)]
        );
        let mut outside = TouchControls {
            zones: vec![TouchZone {
                name: "left".into(),
                rect: TouchRect::left_half(),
                action: "left".into(),
                kind: TouchControlKind::Button,
            }],
            ..TouchControls::default()
        };
        assert!(
            outside
                .evaluate(
                    &[finger(1, (750.0, 100.0), (100.0, 100.0), TouchPhase::Moved)],
                    &[],
                    SCREEN
                )
                .holds
                .is_empty(),
            "sliding into a zone does not claim it"
        );

        let mut gestures = TouchControls {
            gestures: vec![
                GestureBinding {
                    gesture: GestureKind::Swipe,
                    action: "any".into(),
                    rect: None,
                    frames: 0,
                },
                GestureBinding {
                    gesture: GestureKind::SwipeUp,
                    action: "up".into(),
                    rect: None,
                    frames: 2,
                },
                GestureBinding {
                    gesture: GestureKind::SwipeDown,
                    action: "down".into(),
                    rect: None,
                    frames: 1,
                },
                GestureBinding {
                    gesture: GestureKind::SwipeLeft,
                    action: "left".into(),
                    rect: None,
                    frames: 1,
                },
                GestureBinding {
                    gesture: GestureKind::SwipeRight,
                    action: "right".into(),
                    rect: None,
                    frames: 1,
                },
                GestureBinding {
                    gesture: GestureKind::Pinch,
                    action: "pinch".into(),
                    rect: None,
                    frames: 4,
                },
                GestureBinding {
                    gesture: GestureKind::TwoFingerPan,
                    action: "pan".into(),
                    rect: None,
                    frames: 1,
                },
                GestureBinding {
                    gesture: GestureKind::Tap,
                    action: "tap".into(),
                    rect: Some(TouchRect::right_half()),
                    frames: 1,
                },
            ],
            ..TouchControls::default()
        };
        let out = gestures.evaluate(
            &[],
            &[
                GestureEvent::Swipe {
                    direction: Vec2::new(0.0, 0.5),
                    start: Vec2::new(100.0, 100.0),
                    end: Vec2::new(100.0, 40.0),
                },
                GestureEvent::Swipe {
                    direction: Vec2::new(-0.5, 0.0),
                    start: Vec2::new(100.0, 100.0),
                    end: Vec2::new(40.0, 100.0),
                },
                GestureEvent::Swipe {
                    direction: Vec2::Y,
                    start: Vec2::new(100.0, 100.0),
                    end: Vec2::new(100.0, 20.0),
                },
                GestureEvent::Pinch {
                    scale: 1.25,
                    center: Vec2::new(80.0, 40.0),
                },
                GestureEvent::TwoFingerPan {
                    delta: Vec2::new(25.0, 50.0),
                    center: Vec2::new(10.0, 20.0),
                },
                GestureEvent::Tap {
                    position: Vec2::new(100.0, 100.0),
                },
                GestureEvent::Tap {
                    position: Vec2::new(800.0, 100.0),
                },
            ],
            SCREEN,
        );
        assert_eq!(
            out.pulses,
            vec![
                ("any".to_string(), Vec3::new(0.0, 0.5, 0.0), 1),
                ("any".to_string(), Vec3::new(-0.5, 0.0, 0.0), 1),
                ("any".to_string(), Vec3::Y, 1),
                ("up".to_string(), Vec3::Y, 2),
                ("pinch".to_string(), Vec3::new(0.25, 0.0, 0.0), 4),
                ("pan".to_string(), Vec3::new(0.05, -0.1, 0.0), 1),
                ("tap".to_string(), Vec3::X, 1),
            ]
        );
        assert_eq!(
            gestures.evaluate(&[], &[], Vec2::new(-10.0, 100.0)),
            TouchControlOutput::default()
        );
        assert_eq!(
            gestures.evaluate(&[], &[], Vec2::new(100.0, -1.0)),
            TouchControlOutput::default()
        );
    }
}
