// SPDX-License-Identifier: MPL-2.0
//! Touch state (`docs/specs/11-platform-and-input.md` § 6): the [`Touches`] resource filled from
//! the platform's `TouchInput` messages, and the gesture recogniser that emits [`GestureEvent`]s
//! (tap, double tap, long press, swipe, pinch, two-finger pan).
//!
//! Positions are physical pixels from the window's top-left (y down), as reported by the
//! platform. Mouse emulation from touch is **off** by default (specs 11 § 6); games opt in with
//! [`TouchMouseEmulation`].
//!
//! Gesture thresholds are public constants adapted from Android's `ViewConfiguration` defaults
//! (see `docs/provenance.md`); they are in physical pixels, so they feel slightly smaller on
//! high-DPI screens — the UI lane's virtual controls can layer their own, logical-px thresholds.

use std::collections::BTreeMap;

use regia_app::prelude::*;
use regia_core::math::Vec2;
use regia_platform::{TouchInput, TouchPhase};

use crate::device::LastUsedDevice;

/// Maximum movement (px) between down and up for a touch to count as a tap.
pub const TAP_SLOP_PX: f32 = 16.0;
/// Maximum duration (s) of a tap.
pub const TAP_MAX_SECS: f32 = 0.3;
/// Maximum gap (s) between two taps of a double tap.
pub const DOUBLE_TAP_GAP_SECS: f32 = 0.35;
/// Maximum distance (px) between two taps of a double tap.
pub const DOUBLE_TAP_SLOP_PX: f32 = 64.0;
/// Hold duration (s) before a long press fires.
pub const LONG_PRESS_SECS: f32 = 0.5;
/// Maximum movement (px) during a hold for a long press to stay alive.
pub const LONG_PRESS_SLOP_PX: f32 = 20.0;
/// Minimum total distance (px) for a swipe on release.
pub const SWIPE_MIN_PX: f32 = 48.0;
/// Maximum duration (s) of a swipe. A drag of at least [`SWIPE_MIN_PX`] that lasts longer is
/// neither a tap nor a swipe. This is the same cap as `TAP_MAX_SECS.max(1.0)` while
/// [`TAP_MAX_SECS`] is 0.3; a later tap cap above 1 second would not raise this constant.
pub const SWIPE_MAX_SECS: f32 = 1.0;
/// Relative distance change between emitted pinch events.
pub const PINCH_MIN_CHANGE: f32 = 0.05;
/// Smallest finger separation (pixels) that can anchor a pinch baseline.
pub const PINCH_MIN_BASE_PX: f32 = 8.0;
/// Minimum centroid movement (px) between emitted two-finger pan events.
pub const PAN_MIN_DELTA_PX: f32 = 4.0;

/// One tracked finger.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchPoint {
    /// Stable finger id for the duration of the touch.
    pub id: u64,
    /// Where the finger went down.
    pub start: Vec2,
    /// Position at the previous update.
    pub previous: Vec2,
    /// Current position.
    pub current: Vec2,
    /// Current phase.
    pub phase: TouchPhase,
    /// Always `1.0` while down: the platform touch event carries no pressure yet.
    pub pressure: f32,
    /// Seconds the touch has been down: frame deltas accumulated from the frame after the
    /// start (the start frame is time 0). Never a subtraction of absolute gameplay time, so a
    /// tap classifies the same in a fresh session and after hours of play (audit finding 3,
    /// ADR-4768).
    pub age: f32,
    /// Total distance (px) travelled since `start` (not the displacement).
    pub travelled: f32,
}

impl TouchPoint {
    /// Displacement from the start of the touch.
    pub fn displacement(&self) -> Vec2 {
        self.current - self.start
    }
}

/// A recognised gesture (specs 11 § 6). Positions are physical pixels, y down.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub enum GestureEvent {
    /// A quick tap: down and up within [`TAP_MAX_SECS`], moving less than [`TAP_SLOP_PX`].
    Tap {
        /// Where the tap ended.
        position: Vec2,
    },
    /// Two taps within [`DOUBLE_TAP_GAP_SECS`] and [`DOUBLE_TAP_SLOP_PX`] of each other.
    DoubleTap {
        /// Where the second tap ended.
        position: Vec2,
    },
    /// A finger held for [`LONG_PRESS_SECS`] without moving more than [`LONG_PRESS_SLOP_PX`].
    LongPress {
        /// Where the finger is held.
        position: Vec2,
    },
    /// A fast drag: released within [`SWIPE_MAX_SECS`] with at least [`SWIPE_MIN_PX`] of displacement.
    Swipe {
        /// Unit direction of the dominant axis (y up: `(0, 1)` is a swipe up).
        direction: Vec2,
        /// Where the swipe started.
        start: Vec2,
        /// Where it ended.
        end: Vec2,
    },
    /// Two fingers moving apart / together; `scale` is the distance change since the pinch
    /// started (1.0-based, e.g. 1.2 = 20% further apart).
    Pinch {
        /// Current distance relative to the distance when the second finger landed.
        scale: f32,
        /// Midpoint of the two fingers.
        center: Vec2,
    },
    /// Two fingers moving together.
    TwoFingerPan {
        /// Centroid movement since the previous pan event.
        delta: Vec2,
        /// Current centroid.
        center: Vec2,
    },
}

/// Recogniser state carried between frames (part of the [`Touches`] resource).
#[derive(Debug, Default)]
struct Gestures {
    /// The last tap (position, age in seconds since it ended, accumulated frame deltas): aged
    /// every frame so the double-tap gap compares small accumulated numbers, not absolute
    /// gameplay time (audit finding 3, ADR-4768).
    last_tap: Option<(Vec2, f32)>,
    /// Touches whose long press has already fired.
    long_pressed: Vec<u64>,
    /// Distance between the two fingers when the pinch started, and at the last Pinch event.
    pinch: Option<(f32, f32)>,
    /// Centroid at the last TwoFingerPan event.
    pan_center: Option<Vec2>,
    /// Whether the left mouse button is currently held by touch emulation.
    left_synthesized: bool,
}

/// Active touches (`Touches` in specs 11 § 6), one entry per finger currently down.
#[derive(Resource, Debug, Default)]
pub struct Touches {
    points: BTreeMap<u64, TouchPoint>,
    gestures: Gestures,
}

impl Touches {
    /// The touch with the given finger id, if tracked.
    pub fn get(&self, id: u64) -> Option<&TouchPoint> {
        self.points.get(&id)
    }

    /// All tracked touches.
    pub fn iter(&self) -> impl Iterator<Item = &TouchPoint> {
        self.points.values()
    }

    /// How many fingers are down.
    pub fn count(&self) -> usize {
        self.points
            .values()
            .filter(|p| p.phase != TouchPhase::Ended && p.phase != TouchPhase::Cancelled)
            .count()
    }

    /// The centroid (mean position) of all tracked touches, if any.
    pub fn centroid(&self) -> Option<Vec2> {
        let mut iter = self.iter();
        let first = iter.next()?.current;
        let mut sum = first;
        let mut n = 1;
        for point in iter {
            sum += point.current;
            n += 1;
        }
        Some(sum / n as f32)
    }
}

/// Whether touches synthesize mouse button/position input (off by default, specs 11 § 6: games
/// opt in). When enabled, the first touch moves the virtual cursor and presses the left button.
#[derive(Resource, Debug, Clone, Copy, Default)]
pub struct TouchMouseEmulation(pub bool);

/// `PreUpdate` (set [`crate::InputSet::Devices`]): drains `regia_platform::TouchInput`
/// messages into the [`Touches`] resource, runs the gesture recogniser and writes
/// [`GestureEvent`]s. Touches removed at the end of the previous frame (finger already up) are
/// pruned here first, so a "Moved" for an unknown id is not a finger (see the Android joystick
/// detection in `crate::gamepad`).
#[tracing::instrument(target = "regia_input", level = "trace", skip_all)]
#[allow(clippy::too_many_arguments)]
pub fn update_touches(
    mut touches: ResMut<Touches>,
    mut inputs: MessageReader<TouchInput>,
    mut focus: MessageReader<regia_platform::WindowFocused>,
    time: Res<Time>,
    mut gestures: MessageWriter<GestureEvent>,
    mut last_used: ResMut<LastUsedDevice>,
    mut mouse: ResMut<crate::Mouse>,
    emulation: Option<Res<TouchMouseEmulation>>,
) {
    // Prune finished touches: their state was visible for exactly one frame.
    touches
        .points
        .retain(|_, p| p.phase != TouchPhase::Ended && p.phase != TouchPhase::Cancelled);
    let live: Vec<u64> = touches.points.keys().copied().collect();
    touches.gestures.long_pressed.retain(|id| live.contains(id));

    // Focus loss (alt-tab, activity pause) swallows the finger-up events: cancel live touches.
    let focus = crate::device::focus_change(&mut focus);
    if focus.lost {
        for point in touches.points.values_mut() {
            point.phase = TouchPhase::Cancelled;
        }
        touches.gestures.pinch = None;
        touches.gestures.pan_center = None;
    }

    let dt = time.delta_secs();
    // Age every live touch and the double-tap clock by this frame's delta, before the events
    // are folded in: a touch that starts this frame is time 0 for this frame's classification,
    // and every comparison below works on small accumulated ages (deterministic across session
    // age and replay boundaries; audit finding 3, ADR-4768).
    for point in touches.points.values_mut() {
        point.age += dt;
    }
    if let Some((_, ref mut age)) = touches.gestures.last_tap {
        *age += dt;
    }
    let mut emulate_cursor: Option<Vec2> = None;
    let mut emulate_button = false;

    for input in inputs.read() {
        let position = Vec2::new(input.position.0 as f32, input.position.1 as f32);
        last_used.touch();
        match input.phase {
            TouchPhase::Started => {
                touches.points.insert(
                    input.id,
                    TouchPoint {
                        id: input.id,
                        start: position,
                        previous: position,
                        current: position,
                        phase: TouchPhase::Started,
                        pressure: 1.0,
                        age: 0.0,
                        travelled: 0.0,
                    },
                );
                // A new finger resets multi-finger gesture baselines.
                touches.gestures.pinch = None;
                touches.gestures.pan_center = None;
                emulate_cursor = Some(position);
                emulate_button = true;
            }
            TouchPhase::Moved => {
                if let Some(point) = touches.points.get_mut(&input.id) {
                    let delta = position - point.current;
                    point.travelled += delta.length();
                    point.previous = point.current;
                    point.current = position;
                    point.phase = TouchPhase::Moved;
                    // Mouse emulation: the lowest active finger id drives the virtual cursor.
                    if emulate_cursor.is_none()
                        && touches
                            .points
                            .values()
                            .filter(|p| p.phase != TouchPhase::Ended && p.phase != TouchPhase::Cancelled)
                            .map(|p| p.id)
                            .min()
                            == Some(input.id)
                    {
                        emulate_cursor = Some(position);
                        emulate_button = true;
                    }
                }
            }
            TouchPhase::Ended | TouchPhase::Cancelled => {
                if let Some(point) = touches.points.get_mut(&input.id) {
                    // A cancel is terminal for this pointer sequence. Some platform batches can
                    // contain both the cancellation and a delayed end; keeping the cancel
                    // prevents the end from being interpreted as a tap.
                    if point.phase == TouchPhase::Cancelled && input.phase == TouchPhase::Ended {
                        continue;
                    }
                    point.previous = point.current;
                    point.current = position;
                    point.phase = input.phase;
                }
            }
        }
    }

    // The batch ends unfocused: a finger that went down in it (before the loss) never gets its
    // finger-up. Cancel it like the ones from earlier frames, and press no emulated button.
    if focus.ends_unfocused {
        for point in touches.points.values_mut() {
            if point.phase != TouchPhase::Ended {
                point.phase = TouchPhase::Cancelled;
            }
        }
        touches.gestures.pinch = None;
        touches.gestures.pan_center = None;
        emulate_button = false;
    }

    // Long presses: held past the timeout without wandering. A finger that never moves keeps
    // its `Started` phase (no Moved messages), so both phases count as held.
    let fired: Vec<u64> = touches
        .points
        .values()
        .filter(|point| {
            matches!(point.phase, TouchPhase::Started | TouchPhase::Moved)
                && !touches.gestures.long_pressed.contains(&point.id)
                && point.age >= LONG_PRESS_SECS
                && point.travelled <= LONG_PRESS_SLOP_PX
        })
        .map(|point| point.id)
        .collect();
    for id in &fired {
        touches.gestures.long_pressed.push(*id);
    }
    for id in fired {
        if let Some(point) = touches.points.get(&id) {
            gestures.write(GestureEvent::LongPress {
                position: point.current,
            });
        }
    }

    // Two-finger gestures: pinch and pan.
    let ids: Vec<u64> = touches.points.keys().copied().collect();
    if ids.len() == 2 {
        let a = touches.points[&ids[0]];
        let b = touches.points[&ids[1]];
        let center = (a.current + b.current) / 2.0;
        let distance = a.current.distance(b.current);
        // The baseline waits until the fingers are apart; a second contact reporting on top of
        // the first would otherwise lock a 1 px base and make the next sample a huge scale.
        if touches.gestures.pinch.is_some() || distance >= PINCH_MIN_BASE_PX {
            let (base, last) = touches.gestures.pinch.unwrap_or((distance, distance));
            touches.gestures.pinch = Some((base, last));
            let scale = distance / base;
            if (scale - last / base).abs() >= PINCH_MIN_CHANGE {
                touches.gestures.pinch = Some((base, distance));
                gestures.write(GestureEvent::Pinch { scale, center });
            }
        }
        match touches.gestures.pan_center {
            Some(previous) if center.distance(previous) >= PAN_MIN_DELTA_PX => {
                touches.gestures.pan_center = Some(center);
                gestures.write(GestureEvent::TwoFingerPan {
                    delta: center - previous,
                    center,
                });
            }
            None => touches.gestures.pan_center = Some(center),
            _ => {}
        }
    } else {
        touches.gestures.pinch = None;
        touches.gestures.pan_center = None;
    }

    // Taps, double taps and swipes fire on finger-up.
    let ended: Vec<TouchPoint> = touches
        .points
        .values()
        .filter(|point| point.phase == TouchPhase::Ended)
        .copied()
        .collect();
    let mut tap_events: Vec<GestureEvent> = Vec::new();
    for point in &ended {
        let duration = point.age;
        let displacement = point.displacement().length();
        if duration <= TAP_MAX_SECS && displacement <= TAP_SLOP_PX {
            tap_events.push(GestureEvent::Tap {
                position: point.current,
            });
            if let Some((last_position, last_age)) = touches.gestures.last_tap
                && last_age <= DOUBLE_TAP_GAP_SECS
                && last_position.distance(point.current) <= DOUBLE_TAP_SLOP_PX
            {
                tap_events.push(GestureEvent::DoubleTap {
                    position: point.current,
                });
                touches.gestures.last_tap = None;
            } else {
                touches.gestures.last_tap = Some((point.current, 0.0));
            }
        } else if duration <= SWIPE_MAX_SECS && displacement >= SWIPE_MIN_PX {
            let d = point.displacement();
            // Screen y is down; gestures report y up.
            let d = Vec2::new(d.x, -d.y);
            tap_events.push(GestureEvent::Swipe {
                direction: if d.x.abs() >= d.y.abs() {
                    Vec2::new(d.x.signum(), 0.0)
                } else {
                    Vec2::new(0.0, d.y.signum())
                },
                start: point.start,
                end: point.current,
            });
        }
    }
    for event in tap_events {
        gestures.write(event);
    }

    // Mouse emulation (opt-in): the first active touch drives cursor + left button. A button the
    // emulation pressed is released when the touch ends, even if emulation was switched off
    // meanwhile.
    let emulating = emulation.is_some_and(|e| e.0);
    if emulating {
        if let Some(position) = emulate_cursor {
            mouse.position = position;
        }
        if emulate_button {
            mouse.held.insert(regia_platform::MouseButton::Left);
            touches.gestures.left_synthesized = true;
        }
    }
    if touches.gestures.left_synthesized && !emulate_button && (!emulating || touches.count() == 0) {
        if mouse.held.remove(&regia_platform::MouseButton::Left) {
            mouse.just_released.insert(regia_platform::MouseButton::Left);
        }
        touches.gestures.left_synthesized = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn pinch_baseline_ignores_coincident_fingers() {
        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (100.0, 100.0));
        touch(&mut app, 2, TouchPhase::Started, (100.0, 100.0));
        app.update();
        let _ = gestures_of(&mut app);
        touch(&mut app, 2, TouchPhase::Moved, (220.0, 100.0));
        app.update();
        let first = gestures_of(&mut app);
        assert!(
            !first
                .iter()
                .any(|g| matches!(g, GestureEvent::Pinch { scale, .. } if *scale > 2.0)),
            "no huge pinch from a 0 px baseline: {first:?}"
        );
        touch(&mut app, 2, TouchPhase::Moved, (280.0, 100.0));
        app.update();
        for g in gestures_of(&mut app) {
            if let GestureEvent::Pinch { scale, .. } = g {
                assert!(
                    scale < 2.0,
                    "scale relative to the first real separation: {scale}"
                );
            }
        }
    }

    /// A headless app with the input plugin and manual 60 Hz time, for gesture timing.
    fn gesture_app() -> App {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f32(
            1.0 / 60.0,
        )));
        app.add_plugins(crate::InputPlugin::default());
        app
    }

    fn touch(app: &mut App, id: u64, phase: TouchPhase, position: (f64, f64)) {
        app.world_mut().write_message(TouchInput {
            window: Entity::PLACEHOLDER,
            id,
            phase,
            position,
            timestamp: regia_platform::InputTimestamp::ZERO,
        });
    }

    /// Collects the gesture events written since the last drain.
    fn gestures_of(app: &mut App) -> Vec<GestureEvent> {
        app.world_mut()
            .resource_mut::<Messages<GestureEvent>>()
            .drain()
            .collect::<Vec<_>>()
    }

    #[test]
    fn focus_loss_cancels_live_touches() {
        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (10.0, 10.0));
        app.update();
        assert_eq!(app.world().resource::<Touches>().count(), 1);
        app.world_mut().write_message(regia_platform::WindowFocused {
            window: Entity::PLACEHOLDER,
            focused: false,
        });
        app.update();
        let touches = app.world().resource::<Touches>();
        assert_eq!(touches.count(), 0, "cancelled touches do not count");
        assert_eq!(touches.get(1).map(|p| p.phase), Some(TouchPhase::Cancelled));
        app.update();
        assert!(app.world().resource::<Touches>().get(1).is_none());
    }

    #[test]
    fn touch_lifecycle_tracks_positions_and_travel() {
        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (100.0, 100.0));
        touch(&mut app, 1, TouchPhase::Moved, (140.0, 110.0));
        app.update();
        {
            let touches = app.world().resource::<Touches>();
            let point = touches.get(1).expect("tracked");
            assert_eq!(point.start, Vec2::new(100.0, 100.0));
            assert_eq!(point.current, Vec2::new(140.0, 110.0));
            assert!((point.travelled - 41.23).abs() < 0.1, "{}", point.travelled);
            assert_eq!(touches.count(), 1);
        }
        touch(&mut app, 1, TouchPhase::Ended, (145.0, 112.0));
        app.update();
        {
            let touches = app.world().resource::<Touches>();
            assert_eq!(touches.get(1).map(|p| p.phase), Some(TouchPhase::Ended));
            assert_eq!(touches.count(), 0, "ended touches do not count");
        }
        app.update(); // pruned after one frame of visibility
        assert!(app.world().resource::<Touches>().get(1).is_none());
    }

    #[test]
    fn quick_still_tap_then_double_tap() {
        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (50.0, 50.0));
        app.update();
        touch(&mut app, 1, TouchPhase::Ended, (55.0, 52.0));
        app.update();
        let events = gestures_of(&mut app);
        assert_eq!(
            events,
            vec![GestureEvent::Tap {
                position: Vec2::new(55.0, 52.0)
            }]
        );

        // Second tap within the gap and slop: Tap + DoubleTap.
        touch(&mut app, 2, TouchPhase::Started, (58.0, 54.0));
        app.update();
        touch(&mut app, 2, TouchPhase::Ended, (60.0, 55.0));
        app.update();
        let events = gestures_of(&mut app);
        assert_eq!(
            events,
            vec![
                GestureEvent::Tap {
                    position: Vec2::new(60.0, 55.0)
                },
                GestureEvent::DoubleTap {
                    position: Vec2::new(60.0, 55.0)
                },
            ]
        );
    }

    #[test]
    fn drag_release_is_a_swipe_not_a_tap() {
        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (100.0, 300.0));
        app.update();
        touch(&mut app, 1, TouchPhase::Moved, (100.0, 200.0));
        app.update();
        touch(&mut app, 1, TouchPhase::Ended, (102.0, 100.0));
        app.update();
        let events = gestures_of(&mut app);
        assert_eq!(
            events,
            vec![GestureEvent::Swipe {
                direction: Vec2::new(0.0, 1.0),
                start: Vec2::new(100.0, 300.0),
                end: Vec2::new(102.0, 100.0),
            }],
            "screen-up movement is a +y swipe"
        );
    }

    /// Age is frame deltas added before events. A touch that starts this frame is age 0, so the
    /// end frame's age is `(updates_after_start) / 60`.
    #[test]
    fn a_swipe_must_finish_within_one_second() {
        assert_eq!(SWIPE_MAX_SECS, 1.0);
        assert_eq!(
            SWIPE_MAX_SECS,
            TAP_MAX_SECS.max(1.0),
            "the named cap matches the old expression while the tap cap is 0.3"
        );

        let mut quick = gesture_app();
        touch(&mut quick, 1, TouchPhase::Started, (0.0, 0.0));
        quick.update();
        touch(&mut quick, 1, TouchPhase::Moved, (100.0, 0.0));
        quick.update();
        for _ in 0..28 {
            quick.update();
        }
        touch(&mut quick, 1, TouchPhase::Ended, (100.0, 0.0));
        quick.update();
        assert_eq!(
            gestures_of(&mut quick),
            vec![GestureEvent::Swipe {
                direction: Vec2::new(1.0, 0.0),
                start: Vec2::ZERO,
                end: Vec2::new(100.0, 0.0),
            }],
            "100 px in 0.5 s is a swipe"
        );

        let mut slow = gesture_app();
        touch(&mut slow, 1, TouchPhase::Started, (0.0, 0.0));
        slow.update();
        touch(&mut slow, 1, TouchPhase::Moved, (100.0, 0.0));
        slow.update();
        for _ in 0..64 {
            slow.update();
        }
        touch(&mut slow, 1, TouchPhase::Ended, (100.0, 0.0));
        slow.update();
        let events = gestures_of(&mut slow);
        assert!(
            events
                .iter()
                .all(|event| !matches!(event, GestureEvent::Tap { .. } | GestureEvent::Swipe { .. })),
            "100 px in 1.1 s is neither a tap nor a swipe: {events:?}"
        );
    }

    #[test]
    fn long_press_fires_once_while_held() {
        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (10.0, 10.0));
        app.update();
        app.update();
        // Frames are 1/60 s; LONG_PRESS_SECS needs ~30 more frames. Messages do not outlive
        // their frames unread, so drain as we go.
        let mut long_presses = 0;
        for _ in 0..35 {
            touch(&mut app, 1, TouchPhase::Moved, (11.0, 12.0));
            app.update();
            long_presses += gestures_of(&mut app)
                .iter()
                .filter(|g| matches!(g, GestureEvent::LongPress { .. }))
                .count();
        }
        assert_eq!(long_presses, 1, "fires exactly once");
    }

    #[test]
    fn pinch_and_two_finger_pan_emit() {
        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (100.0, 100.0));
        touch(&mut app, 2, TouchPhase::Started, (200.0, 100.0));
        app.update();
        assert!(
            gestures_of(&mut app).is_empty(),
            "no gesture from just putting fingers down"
        );
        // Spread from 100 px to 150 px apart: scale 1.5, one event (threshold 5%).
        touch(&mut app, 1, TouchPhase::Moved, (75.0, 100.0));
        touch(&mut app, 2, TouchPhase::Moved, (225.0, 100.0));
        app.update();
        let events = gestures_of(&mut app);
        assert!(
            events.iter().any(|g| matches!(
                g,
                GestureEvent::Pinch { scale, center }
                    if (*scale - 1.5).abs() < 0.01 && *center == Vec2::new(150.0, 100.0)
            )),
            "{events:?}"
        );
        // Pan both fingers right by 10 px: centroid moved, one pan event.
        touch(&mut app, 1, TouchPhase::Moved, (85.0, 100.0));
        touch(&mut app, 2, TouchPhase::Moved, (235.0, 100.0));
        app.update();
        let events = gestures_of(&mut app);
        assert!(
            events.iter().any(|g| matches!(
                g,
                GestureEvent::TwoFingerPan { delta, center }
                    if *delta == Vec2::new(10.0, 0.0) && *center == Vec2::new(160.0, 100.0)
            )),
            "{events:?}"
        );
    }

    #[test]
    fn mouse_emulation_is_off_by_default_and_opt_in_moves_the_cursor() {
        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (30.0, 40.0));
        app.update();
        assert_eq!(app.world().resource::<crate::Mouse>().position(), Vec2::ZERO);
        touch(&mut app, 1, TouchPhase::Ended, (30.0, 40.0));
        app.update();

        app.world_mut().insert_resource(TouchMouseEmulation(true));
        touch(&mut app, 2, TouchPhase::Started, (70.0, 80.0));
        app.update();
        let mouse = app.world().resource::<crate::Mouse>();
        assert_eq!(mouse.position(), Vec2::new(70.0, 80.0));
        assert!(mouse.pressed(regia_platform::MouseButton::Left));
        touch(&mut app, 2, TouchPhase::Ended, (70.0, 80.0));
        app.update();
        assert!(
            !app.world()
                .resource::<crate::Mouse>()
                .pressed(regia_platform::MouseButton::Left)
        );
    }

    #[test]
    fn synthesized_left_button_is_released_when_emulation_turns_off() {
        let mut app = gesture_app();
        app.world_mut().insert_resource(TouchMouseEmulation(true));
        touch(&mut app, 1, TouchPhase::Started, (70.0, 80.0));
        app.update();
        assert!(
            app.world()
                .resource::<crate::Mouse>()
                .pressed(regia_platform::MouseButton::Left)
        );
        app.world_mut().insert_resource(TouchMouseEmulation(false));
        touch(&mut app, 1, TouchPhase::Ended, (70.0, 80.0));
        app.update();
        assert!(
            !app.world()
                .resource::<crate::Mouse>()
                .pressed(regia_platform::MouseButton::Left)
        );
    }

    #[test]
    fn gesture_thresholds_and_swipe_axes() {
        // Exactly the tap slop is still a tap; just past it, and short of a swipe, is nothing.
        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (0.0, 0.0));
        app.update();
        touch(&mut app, 1, TouchPhase::Ended, (TAP_SLOP_PX as f64, 0.0));
        app.update();
        assert_eq!(
            gestures_of(&mut app),
            vec![GestureEvent::Tap {
                position: Vec2::new(TAP_SLOP_PX, 0.0)
            }]
        );
        assert_eq!(
            app.world()
                .resource::<Touches>()
                .get(1)
                .map(|point| point.displacement()),
            Some(Vec2::new(TAP_SLOP_PX, 0.0))
        );

        let mut app = gesture_app();
        touch(&mut app, 1, TouchPhase::Started, (0.0, 0.0));
        app.update();
        touch(&mut app, 1, TouchPhase::Ended, (20.0, 0.0));
        app.update();
        assert!(
            gestures_of(&mut app).is_empty(),
            "20 px is past a tap and short of a swipe"
        );

        let mut right = gesture_app();
        touch(&mut right, 1, TouchPhase::Started, (0.0, 0.0));
        right.update();
        touch(&mut right, 1, TouchPhase::Ended, (SWIPE_MIN_PX as f64, 0.0));
        right.update();
        assert_eq!(
            gestures_of(&mut right),
            vec![GestureEvent::Swipe {
                direction: Vec2::new(1.0, 0.0),
                start: Vec2::ZERO,
                end: Vec2::new(SWIPE_MIN_PX, 0.0),
            }]
        );

        let mut down = gesture_app();
        touch(&mut down, 1, TouchPhase::Started, (10.0, 10.0));
        down.update();
        touch(
            &mut down,
            1,
            TouchPhase::Ended,
            (10.0, 10.0 + SWIPE_MIN_PX as f64),
        );
        down.update();
        assert_eq!(
            gestures_of(&mut down),
            vec![GestureEvent::Swipe {
                direction: Vec2::new(0.0, -1.0),
                start: Vec2::new(10.0, 10.0),
                end: Vec2::new(10.0, 10.0 + SWIPE_MIN_PX),
            }],
            "screen-down is a -y swipe"
        );

        let mut left = gesture_app();
        touch(&mut left, 1, TouchPhase::Started, (100.0, 10.0));
        left.update();
        touch(
            &mut left,
            1,
            TouchPhase::Ended,
            (100.0 - SWIPE_MIN_PX as f64, 10.0),
        );
        left.update();
        assert_eq!(
            gestures_of(&mut left),
            vec![GestureEvent::Swipe {
                direction: Vec2::new(-1.0, 0.0),
                start: Vec2::new(100.0, 10.0),
                end: Vec2::new(100.0 - SWIPE_MIN_PX, 10.0),
            }]
        );

        // Equal |x| and |y| prefers the horizontal axis.
        let mut diagonal = gesture_app();
        touch(&mut diagonal, 1, TouchPhase::Started, (0.0, 0.0));
        diagonal.update();
        touch(&mut diagonal, 1, TouchPhase::Ended, (48.0, 48.0));
        diagonal.update();
        assert_eq!(
            gestures_of(&mut diagonal),
            vec![GestureEvent::Swipe {
                direction: Vec2::new(1.0, 0.0),
                start: Vec2::ZERO,
                end: Vec2::new(48.0, 48.0),
            }]
        );

        // A still finger held past the tap window, and released before a long press, is neither.
        let mut slow = gesture_app();
        touch(&mut slow, 1, TouchPhase::Started, (4.0, 4.0));
        slow.update();
        assert!(gestures_of(&mut slow).is_empty());
        for _ in 0..18 {
            slow.update();
            assert!(gestures_of(&mut slow).is_empty());
        }
        touch(&mut slow, 1, TouchPhase::Ended, (4.0, 4.0));
        slow.update();
        assert!(
            gestures_of(&mut slow).is_empty(),
            "too slow to tap, too still to swipe"
        );

        let mut cancelled = gesture_app();
        touch(&mut cancelled, 1, TouchPhase::Started, (5.0, 5.0));
        cancelled.update();
        touch(&mut cancelled, 1, TouchPhase::Cancelled, (5.0, 5.0));
        cancelled.update();
        assert!(gestures_of(&mut cancelled).is_empty(), "a cancel is not a tap");
        touch(&mut cancelled, 9, TouchPhase::Moved, (1.0, 1.0));
        cancelled.update();
        assert!(cancelled.world().resource::<Touches>().get(9).is_none());
        assert!(gestures_of(&mut cancelled).is_empty());

        // The second tap on the slop boundary doubles; one pixel past it does not.
        let mut taps = gesture_app();
        let tap_at = |app: &mut App, id: u64, position: (f64, f64)| {
            touch(app, id, TouchPhase::Started, position);
            app.update();
            let _ = gestures_of(app);
            touch(app, id, TouchPhase::Ended, position);
            app.update();
            gestures_of(app)
        };
        assert_eq!(
            tap_at(&mut taps, 1, (0.0, 0.0)),
            vec![GestureEvent::Tap { position: Vec2::ZERO }]
        );
        assert_eq!(
            tap_at(&mut taps, 2, (0.0, DOUBLE_TAP_SLOP_PX as f64)),
            vec![
                GestureEvent::Tap {
                    position: Vec2::new(0.0, DOUBLE_TAP_SLOP_PX)
                },
                GestureEvent::DoubleTap {
                    position: Vec2::new(0.0, DOUBLE_TAP_SLOP_PX)
                },
            ]
        );
        assert_eq!(
            tap_at(&mut taps, 3, (0.0, DOUBLE_TAP_SLOP_PX as f64)),
            vec![GestureEvent::Tap {
                position: Vec2::new(0.0, DOUBLE_TAP_SLOP_PX)
            }],
            "a completed double tap does not chain into a third"
        );
        assert_eq!(
            tap_at(&mut taps, 4, (0.0, (DOUBLE_TAP_SLOP_PX + 65.0) as f64)),
            vec![GestureEvent::Tap {
                position: Vec2::new(0.0, DOUBLE_TAP_SLOP_PX + 65.0)
            }],
            "65 px past the previous tap is outside the double-tap slop"
        );

        // An ended finger stays tracked for the frame, so it still pulls the centroid.
        let mut centroid = gesture_app();
        touch(&mut centroid, 1, TouchPhase::Started, (0.0, 0.0));
        touch(&mut centroid, 2, TouchPhase::Started, (10.0, 0.0));
        centroid.update();
        assert_eq!(
            centroid.world().resource::<Touches>().centroid(),
            Some(Vec2::new(5.0, 0.0))
        );
        touch(&mut centroid, 2, TouchPhase::Ended, (30.0, 0.0));
        centroid.update();
        {
            let touches = centroid.world().resource::<Touches>();
            assert_eq!(touches.count(), 1);
            assert_eq!(touches.centroid(), Some(Vec2::new(15.0, 0.0)));
        }
        centroid.update();
        assert_eq!(
            centroid.world().resource::<Touches>().centroid(),
            Some(Vec2::ZERO)
        );
    }
}
