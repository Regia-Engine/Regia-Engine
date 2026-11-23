// SPDX-License-Identifier: MPL-2.0
//! Multi-touch gestures (ADR-273 I08, ADR-1842): pinch, two-finger scroll and swipe from a stream
//! of touch samples.
//!
//! [`GestureRecognizer`] is a pure state machine. The host feeds it [`TouchSample`]s (finger id,
//! phase, position in UI pixels, time in seconds) and it returns [`Gesture`]s. It never reads a
//! clock, so a recorded sequence always produces the same gestures.
//!
//! - **Pinch**: the distance between two fingers has changed by at least
//!   [`GestureConfig::pinch_threshold`] and by at least as much as their midpoint has moved.
//!   A tie is a pinch. Reports the cumulative scale since the pinch began and the centre.
//! - **Two-finger scroll**: the midpoint has moved at least [`GestureConfig::scroll_threshold`]
//!   and strictly more than the distance has changed. Reports the per-update delta.
//! - **Swipe**: one finger that travels at least [`GestureConfig::swipe_distance`] pixels in at most
//!   [`GestureConfig::swipe_time`] seconds before it lifts, along the dominant axis.
//!
//! A third finger cancels the two-finger gestures until only two remain. More than one active touch
//! is tracked (the single-touch path in `UiPointer` still drives the first finger).
//!
//! The agent surface is `ui.touch` (`book/agents/ui-touch.md`).

use std::collections::BTreeMap;

use regia_app::prelude::*;

/// Phase of one touch sample.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchStage {
    /// Finger down.
    Down,
    /// Finger moved.
    Move,
    /// Finger up.
    Up,
    /// Touch cancelled.
    Cancel,
}

/// One touch sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TouchSample {
    /// Finger id, stable for the touch.
    pub id: u64,
    /// Phase.
    pub stage: TouchStage,
    /// Position in UI pixels.
    pub pos: [f32; 2],
    /// Time in seconds (any origin, monotonic).
    pub time: f32,
}

/// Direction of a swipe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SwipeDir {
    /// Towards smaller x.
    Left,
    /// Towards larger x.
    Right,
    /// Towards smaller y (up on screen).
    Up,
    /// Towards larger y.
    Down,
}

impl SwipeDir {
    /// Lowercase name for the agent protocol.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

/// A recognized gesture.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Gesture {
    /// Two fingers moved apart or together. `scale` is relative to the start of the pinch.
    Pinch {
        /// Cumulative scale (`1` = unchanged, above 1 = fingers apart).
        scale: f32,
        /// Midpoint of the fingers.
        center: [f32; 2],
    },
    /// Two fingers moved together in one direction.
    TwoFingerScroll {
        /// Movement of the midpoint since the previous update.
        delta: [f32; 2],
    },
    /// A quick one-finger stroke.
    Swipe {
        /// Dominant direction.
        dir: SwipeDir,
        /// Straight-line distance in pixels.
        distance: f32,
        /// Duration in seconds.
        duration: f32,
    },
}

/// Thresholds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GestureConfig {
    /// Distance change in pixels before a pinch starts.
    pub pinch_threshold: f32,
    /// Midpoint travel in pixels before a two-finger scroll starts.
    pub scroll_threshold: f32,
    /// Least swipe distance in pixels.
    pub swipe_distance: f32,
    /// Longest swipe in seconds.
    pub swipe_time: f32,
}

impl Default for GestureConfig {
    fn default() -> Self {
        Self {
            pinch_threshold: 12.0,
            scroll_threshold: 10.0,
            swipe_distance: 80.0,
            swipe_time: 0.4,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Two {
    /// Fingers down, not yet classified.
    Pending,
    Pinch,
    Scroll,
}

#[derive(Debug, Clone, Copy)]
struct Finger {
    start: [f32; 2],
    start_time: f32,
    pos: [f32; 2],
}

/// The recognizer. See the module docs.
#[derive(Debug, Clone, Default)]
pub struct GestureRecognizer {
    /// Thresholds.
    pub config: GestureConfig,
    fingers: BTreeMap<u64, Finger>,
    two: Option<Two>,
    /// Distance and midpoint when the two-finger gesture began or last updated.
    base_dist: f32,
    base_mid: [f32; 2],
    last_mid: [f32; 2],
    /// A swipe is only possible if this touch never joined a second finger.
    solo: bool,
}

fn dist(a: [f32; 2], b: [f32; 2]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2)).sqrt()
}

fn mid(a: [f32; 2], b: [f32; 2]) -> [f32; 2] {
    [(a[0] + b[0]) * 0.5, (a[1] + b[1]) * 0.5]
}

impl GestureRecognizer {
    /// A recognizer with the given thresholds.
    pub fn new(config: GestureConfig) -> Self {
        Self {
            config,
            ..Self::default()
        }
    }

    /// Fingers currently down.
    pub fn active_touches(&self) -> usize {
        self.fingers.len()
    }

    fn pair(&self) -> Option<([f32; 2], [f32; 2])> {
        if self.fingers.len() != 2 {
            return None;
        }
        let mut it = self.fingers.values();
        Some((it.next()?.pos, it.next()?.pos))
    }

    fn begin_two(&mut self) {
        if let Some((a, b)) = self.pair() {
            self.two = Some(Two::Pending);
            self.base_dist = dist(a, b);
            self.base_mid = mid(a, b);
            self.last_mid = self.base_mid;
        }
    }

    /// Feeds one sample; returns the gestures it completes or advances (usually none or one).
    pub fn feed(&mut self, s: TouchSample) -> Vec<Gesture> {
        let mut out = Vec::new();
        match s.stage {
            TouchStage::Down => {
                if !s.pos.iter().all(|v| v.is_finite()) || !s.time.is_finite() {
                    return out;
                }
                self.solo = self.fingers.is_empty();
                self.fingers.insert(
                    s.id,
                    Finger {
                        start: s.pos,
                        start_time: s.time,
                        pos: s.pos,
                    },
                );
                if self.fingers.len() == 2 {
                    self.begin_two();
                } else {
                    self.two = None;
                }
            }
            TouchStage::Move => {
                if !s.pos.iter().all(|v| v.is_finite()) {
                    return out;
                }
                if let Some(f) = self.fingers.get_mut(&s.id) {
                    f.pos = s.pos;
                }
                if let (Some(state), Some((a, b))) = (self.two, self.pair()) {
                    out.extend(self.update_two(state, a, b));
                }
            }
            TouchStage::Up => {
                let finger = self.fingers.remove(&s.id);
                if let Some(f) = finger
                    && self.solo
                    && self.fingers.is_empty()
                    && s.time.is_finite()
                    && s.pos.iter().all(|v| v.is_finite())
                {
                    out.extend(self.swipe(f, s.pos, s.time));
                }
                self.after_lift();
            }
            TouchStage::Cancel => {
                self.fingers.remove(&s.id);
                self.after_lift();
            }
        }
        out
    }

    fn after_lift(&mut self) {
        self.two = None;
        if self.fingers.len() == 2 {
            self.begin_two();
        }
    }

    fn swipe(&self, f: Finger, end: [f32; 2], time: f32) -> Option<Gesture> {
        let dx = end[0] - f.start[0];
        let dy = end[1] - f.start[1];
        let distance = (dx * dx + dy * dy).sqrt();
        let duration = time - f.start_time;
        if distance < self.config.swipe_distance || duration > self.config.swipe_time || duration < 0.0 {
            return None;
        }
        let dir = if dx.abs() >= dy.abs() {
            if dx < 0.0 { SwipeDir::Left } else { SwipeDir::Right }
        } else if dy < 0.0 {
            SwipeDir::Up
        } else {
            SwipeDir::Down
        };
        Some(Gesture::Swipe {
            dir,
            distance,
            duration,
        })
    }

    fn update_two(&mut self, state: Two, a: [f32; 2], b: [f32; 2]) -> Option<Gesture> {
        let d = dist(a, b);
        let m = mid(a, b);
        let ddist = d - self.base_dist;
        let dmid = dist(m, self.base_mid);
        match state {
            Two::Pending => {
                if ddist.abs() >= self.config.pinch_threshold && ddist.abs() >= dmid {
                    self.two = Some(Two::Pinch);
                } else if dmid >= self.config.scroll_threshold && dmid > ddist.abs() {
                    self.two = Some(Two::Scroll);
                    self.last_mid = self.base_mid;
                } else {
                    return None;
                }
                let next = self.two?;
                self.update_two(next, a, b)
            }
            Two::Pinch => {
                if self.base_dist <= f32::EPSILON {
                    // The fingers began at (nearly) the same point, so no meaningful start
                    // distance exists. Adopt the distance at the first separating move
                    // (scale 1 at that instant) instead of staying silent for the rest of
                    // the gesture.
                    if d <= f32::EPSILON {
                        return None;
                    }
                    self.base_dist = d;
                    return Some(Gesture::Pinch {
                        scale: 1.0,
                        center: m,
                    });
                }
                Some(Gesture::Pinch {
                    scale: d / self.base_dist,
                    center: m,
                })
            }
            Two::Scroll => {
                let delta = [m[0] - self.last_mid[0], m[1] - self.last_mid[1]];
                self.last_mid = m;
                if delta == [0.0, 0.0] {
                    return None;
                }
                Some(Gesture::TwoFingerScroll { delta })
            }
        }
    }
}

/// Gestures kept in [`UiGestureInput::recent`].
pub const RECENT_GESTURES: usize = 32;

/// Live gesture input (ADR-2045): when `enabled`, [`feed_live_gestures`] feeds every platform
/// [`regia_platform::TouchInput`] through [`Self::recognizer`] and sends a [`UiGestureEvent`] per
/// recognized gesture. Off by default: nothing reads touches for gestures until a game turns it
/// on (the single-touch pointer path in `UiPointer` is unaffected either way).
#[derive(Resource, Debug, Clone, Default)]
pub struct UiGestureInput {
    /// Master switch.
    pub enabled: bool,
    /// The recognizer the live touches go through.
    pub recognizer: GestureRecognizer,
    /// The last [`RECENT_GESTURES`] gestures, oldest first.
    pub recent: std::collections::VecDeque<Gesture>,
    /// Gestures recognized since start.
    pub count: u64,
}

/// A gesture recognized from live touches.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct UiGestureEvent {
    /// The gesture (positions in UI px: physical px over the OS scale factor).
    pub gesture: Gesture,
}

/// The [`TouchSample`] of a platform touch at `time` seconds, positions divided by `scale`
/// (physical px to UI px).
pub fn sample_of(t: &regia_platform::TouchInput, time: f32, scale: f32) -> TouchSample {
    let scale = if scale.is_finite() && scale > 0.0 {
        scale
    } else {
        1.0
    };
    TouchSample {
        id: t.id,
        stage: match t.phase {
            regia_platform::TouchPhase::Started => TouchStage::Down,
            regia_platform::TouchPhase::Moved => TouchStage::Move,
            regia_platform::TouchPhase::Ended => TouchStage::Up,
            regia_platform::TouchPhase::Cancelled => TouchStage::Cancel,
        },
        pos: [t.position.0 as f32 / scale, t.position.1 as f32 / scale],
        time,
    }
}

/// `PreUpdate`: feeds live touches through the recognizer while [`UiGestureInput::enabled`].
/// Time is the real (unscaled) clock, so a paused game still recognizes gestures in menus.
pub fn feed_live_gestures(
    input: Option<ResMut<UiGestureInput>>,
    mut touches: MessageReader<regia_platform::TouchInput>,
    real: Option<Res<RealTime>>,
    scales: Option<Res<crate::scale::UiScales>>,
    mut out: MessageWriter<UiGestureEvent>,
) {
    let Some(mut input) = input else {
        touches.clear();
        return;
    };
    if !input.enabled {
        touches.clear();
        return;
    }
    let time = real.map_or(0.0, |r| r.elapsed().as_secs_f32());
    let scale = scales.map_or(1.0, |s| s.scale_factor as f32);
    for t in touches.read() {
        let sample = sample_of(t, time, scale);
        for g in input.recognizer.feed(sample) {
            input.count += 1;
            if input.recent.len() == RECENT_GESTURES {
                input.recent.pop_front();
            }
            input.recent.push_back(g);
            out.write(UiGestureEvent { gesture: g });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_non_finite_lift_position_emits_no_swipe() {
        let mut r = GestureRecognizer::default();
        let t = |stage, pos, time| TouchSample {
            id: 1,
            stage,
            pos,
            time,
        };
        r.feed(t(TouchStage::Down, [10.0, 10.0], 0.0));
        let out = r.feed(t(TouchStage::Up, [f32::NAN, f32::NAN], 0.1));
        assert!(out.is_empty(), "{out:?}");
    }

    fn t(id: u64, stage: TouchStage, x: f32, y: f32, time: f32) -> TouchSample {
        TouchSample {
            id,
            stage,
            pos: [x, y],
            time,
        }
    }

    fn run(r: &mut GestureRecognizer, seq: &[TouchSample]) -> Vec<Gesture> {
        seq.iter().flat_map(|s| r.feed(*s)).collect()
    }

    #[test]
    fn swipe_right_fast_and_far() {
        let mut r = GestureRecognizer::default();
        let g = run(
            &mut r,
            &[
                t(1, TouchStage::Down, 10.0, 100.0, 0.0),
                t(1, TouchStage::Move, 80.0, 105.0, 0.1),
                t(1, TouchStage::Up, 140.0, 110.0, 0.2),
            ],
        );
        assert_eq!(g.len(), 1);
        match g[0] {
            Gesture::Swipe {
                dir,
                distance,
                duration,
            } => {
                assert_eq!(dir, SwipeDir::Right);
                assert!(distance > 130.0);
                assert!((duration - 0.2).abs() < 1e-6);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn slow_or_short_strokes_are_not_swipes() {
        let mut r = GestureRecognizer::default();
        assert!(
            run(
                &mut r,
                &[
                    t(1, TouchStage::Down, 0.0, 0.0, 0.0),
                    t(1, TouchStage::Up, 200.0, 0.0, 1.0)
                ]
            )
            .is_empty()
        );
        assert!(
            run(
                &mut r,
                &[
                    t(2, TouchStage::Down, 0.0, 0.0, 0.0),
                    t(2, TouchStage::Up, 30.0, 0.0, 0.1)
                ]
            )
            .is_empty()
        );
    }

    #[test]
    fn swipe_directions() {
        for (end, want) in [
            ([0.0, -120.0], SwipeDir::Up),
            ([0.0, 120.0], SwipeDir::Down),
            ([-120.0, 10.0], SwipeDir::Left),
        ] {
            let mut r = GestureRecognizer::default();
            let g = run(
                &mut r,
                &[
                    t(1, TouchStage::Down, 0.0, 0.0, 0.0),
                    t(1, TouchStage::Up, end[0], end[1], 0.1),
                ],
            );
            assert!(matches!(g[0], Gesture::Swipe { dir, .. } if dir == want));
        }
    }

    #[test]
    fn pinch_out_reports_cumulative_scale() {
        let mut r = GestureRecognizer::default();
        let g = run(
            &mut r,
            &[
                t(1, TouchStage::Down, 100.0, 200.0, 0.0),
                t(2, TouchStage::Down, 200.0, 200.0, 0.0),
                t(1, TouchStage::Move, 80.0, 200.0, 0.1),
                t(2, TouchStage::Move, 220.0, 200.0, 0.1),
                t(1, TouchStage::Move, 50.0, 200.0, 0.2),
            ],
        );
        assert!(g.iter().all(|g| matches!(g, Gesture::Pinch { .. })));
        let Some(Gesture::Pinch { scale, center }) = g.last().copied() else {
            panic!("no pinch");
        };
        assert!((scale - 1.7).abs() < 1e-5, "{scale}");
        assert!((center[0] - 135.0).abs() < 1e-4);
    }

    #[test]
    fn pinch_starting_at_one_point_reports_scale_from_the_first_separation() {
        // Both fingers down at the same point leave base_dist at 0; the pinch must
        // adopt the distance at the first separating move (scale 1 there) instead of
        // staying silent for the rest of the gesture.
        let mut r = GestureRecognizer::default();
        let g = run(
            &mut r,
            &[
                t(1, TouchStage::Down, 100.0, 100.0, 0.0),
                t(2, TouchStage::Down, 100.0, 100.0, 0.0),
                t(1, TouchStage::Move, 80.0, 100.0, 0.1),
                t(2, TouchStage::Move, 120.0, 100.0, 0.1),
                t(1, TouchStage::Move, 60.0, 100.0, 0.2),
            ],
        );
        let scales: Vec<f32> = g
            .iter()
            .map(|g| match g {
                Gesture::Pinch { scale, .. } => *scale,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(scales.len(), 3, "{scales:?}");
        assert!(
            (scales[0] - 1.0).abs() < 1e-5,
            "scale 1 at the first separation: {scales:?}"
        );
        assert!((scales[1] - 2.0).abs() < 1e-5, "{scales:?}");
        assert!((scales[2] - 3.0).abs() < 1e-5, "{scales:?}");
    }

    #[test]
    fn two_finger_scroll_reports_deltas() {
        let mut r = GestureRecognizer::default();
        let g = run(
            &mut r,
            &[
                t(1, TouchStage::Down, 100.0, 100.0, 0.0),
                t(2, TouchStage::Down, 160.0, 100.0, 0.0),
                t(1, TouchStage::Move, 100.0, 130.0, 0.1),
                t(2, TouchStage::Move, 160.0, 130.0, 0.1),
                t(1, TouchStage::Move, 100.0, 150.0, 0.2),
                t(2, TouchStage::Move, 160.0, 150.0, 0.2),
            ],
        );
        let sum: f32 = g
            .iter()
            .map(|g| match g {
                Gesture::TwoFingerScroll { delta } => delta[1],
                other => panic!("{other:?}"),
            })
            .sum();
        assert!((sum - 50.0).abs() < 1e-4, "total travel {sum}");
    }

    #[test]
    fn second_finger_means_no_swipe_and_third_cancels() {
        let mut r = GestureRecognizer::default();
        let g = run(
            &mut r,
            &[
                t(1, TouchStage::Down, 0.0, 0.0, 0.0),
                t(2, TouchStage::Down, 10.0, 0.0, 0.0),
                t(2, TouchStage::Up, 10.0, 0.0, 0.05),
                t(1, TouchStage::Up, 300.0, 0.0, 0.1),
            ],
        );
        assert!(g.is_empty(), "{g:?}");
        let mut r = GestureRecognizer::default();
        for (i, x) in [0.0, 50.0, 100.0].into_iter().enumerate() {
            r.feed(t(i as u64, TouchStage::Down, x, 0.0, 0.0));
        }
        assert_eq!(r.active_touches(), 3);
        assert!(
            r.feed(t(0, TouchStage::Move, -80.0, 0.0, 0.1)).is_empty(),
            "three fingers: no gesture"
        );
    }

    #[test]
    fn cancel_and_nan_are_safe() {
        let mut r = GestureRecognizer::default();
        r.feed(t(1, TouchStage::Down, f32::NAN, 0.0, 0.0));
        assert_eq!(r.active_touches(), 0);
        r.feed(t(1, TouchStage::Down, 0.0, 0.0, 0.0));
        r.feed(t(1, TouchStage::Cancel, 0.0, 0.0, 0.1));
        assert_eq!(r.active_touches(), 0);
    }

    #[test]
    fn same_input_same_output() {
        let seq = [
            t(1, TouchStage::Down, 100.0, 200.0, 0.0),
            t(2, TouchStage::Down, 200.0, 200.0, 0.0),
            t(1, TouchStage::Move, 60.0, 200.0, 0.1),
        ];
        let a = run(&mut GestureRecognizer::default(), &seq);
        let b = run(&mut GestureRecognizer::default(), &seq);
        assert_eq!(a, b);
    }

    #[test]
    fn live_touches_feed_the_recognizer_only_when_enabled() {
        let mut world = World::new();
        world.init_resource::<Messages<regia_platform::TouchInput>>();
        world.init_resource::<Messages<UiGestureEvent>>();
        world.insert_resource(crate::scale::UiScales {
            window_size: [800.0, 600.0],
            scale_factor: 2.0,
        });
        world.insert_resource(UiGestureInput::default());
        let mut schedule = Schedule::default();
        schedule.add_systems(feed_live_gestures);
        let window = world.spawn_empty().id();
        let touch = |id: u64, phase: regia_platform::TouchPhase, x: f64| regia_platform::TouchInput {
            window,
            id,
            phase,
            position: (x, 100.0),
            timestamp: Default::default(),
        };
        let swipe = |world: &mut World| {
            world.write_message(touch(1, regia_platform::TouchPhase::Started, 0.0));
            world.write_message(touch(1, regia_platform::TouchPhase::Ended, 600.0));
        };
        swipe(&mut world);
        schedule.run(&mut world);
        assert_eq!(world.resource::<UiGestureInput>().count, 0, "off by default");
        world.resource_mut::<UiGestureInput>().enabled = true;
        swipe(&mut world);
        schedule.run(&mut world);
        let input = world.resource::<UiGestureInput>();
        assert_eq!(input.count, 1);
        // 600 physical px at scale 2 is 300 UI px, with no real clock (time 0): a swipe.
        assert!(matches!(
            input.recent[0],
            Gesture::Swipe { dir: SwipeDir::Right, distance, .. } if (distance - 300.0).abs() < 1e-3
        ));
        let sent: Vec<UiGestureEvent> = world.resource_mut::<Messages<UiGestureEvent>>().drain().collect();
        assert_eq!(sent.len(), 1);
    }
}
