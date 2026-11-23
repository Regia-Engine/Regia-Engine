// SPDX-License-Identifier: MPL-2.0
//! Raw pointers to view gestures: the same recognizer serves browser pointer events and Android
//! motion events, so both players feel the same.
//!
//! - one touch or the primary mouse button: orbit; lifting after a quick move flings;
//! - two touches: pan by the centroid, pinch by the distance, twist by the angle;
//! - the secondary or middle mouse button: pan;
//! - two taps within `DOUBLE_TAP_MS` and `DOUBLE_TAP_PX`: reset;
//! - the wheel: zoom (`ViewInput::Scroll`).
//!
//! Times are milliseconds from any clock the caller likes; the recognizer reads no clock itself.

use glam::Vec2;

use crate::controller::ViewInput;

/// A press shorter than this, that moved less than `TAP_SLOP_PX`, is a tap.
pub const TAP_MS: f64 = 250.0;
/// How far a tap may move, in pixels.
pub const TAP_SLOP_PX: f32 = 10.0;
/// Two taps closer than this in time are a double tap.
pub const DOUBLE_TAP_MS: f64 = 300.0;
/// Two taps closer than this in space are a double tap.
pub const DOUBLE_TAP_PX: f32 = 30.0;
/// A release more than this long after the last move does not fling.
pub const FLING_WINDOW_MS: f64 = 60.0;
/// The most pointers tracked at once; further presses are ignored until one lifts.
pub const MAX_POINTERS: usize = 10;

/// Which device a pointer is, and for a mouse which button.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerKind {
    /// A finger or a pen.
    Touch,
    /// The primary mouse button (orbit).
    MousePrimary,
    /// The secondary or middle mouse button (pan).
    MouseSecondary,
}

#[derive(Clone, Copy, Debug)]
struct Pointer {
    id: u64,
    kind: PointerKind,
    pos: Vec2,
    down_pos: Vec2,
    down_ms: f64,
}

/// The pointer recognizer. Feed it events; it returns the view inputs they make.
#[derive(Clone, Debug, Default)]
pub struct GestureRecognizer {
    pointers: Vec<Pointer>,
    last_move_ms: f64,
    velocity: Vec2,
    last_tap: Option<(f64, Vec2)>,
    /// Whether the current press sequence moved enough to stop counting as a tap.
    moved: bool,
    /// Two fingers were down during this press sequence (lifting them never flings or taps).
    multi: bool,
    /// Device pixels per CSS pixel or dp, when the host says; the tap distances are meant for a finger.
    pixel_ratio: Option<f32>,
}

fn finite2(x: f32, y: f32) -> Option<Vec2> {
    (x.is_finite() && y.is_finite()).then_some(Vec2::new(x, y))
}

impl GestureRecognizer {
    /// An idle recognizer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Pointers come in device pixels at `ratio` per CSS pixel or dp: the tap distances grow with it, so a
    /// finger on a dense screen can still tap. Without a call they are plain pixels.
    pub fn set_pixel_ratio(&mut self, ratio: f32) {
        self.pixel_ratio = (ratio.is_finite() && ratio > 0.0).then_some(ratio);
    }

    fn scaled(&self, pixels: f32) -> f32 {
        pixels * self.pixel_ratio.unwrap_or(1.0)
    }

    /// How many pointers are down.
    pub fn active(&self) -> usize {
        self.pointers.len()
    }

    /// A pointer went down at `(x, y)` pixels.
    pub fn pointer_down(&mut self, id: u64, kind: PointerKind, x: f32, y: f32, t_ms: f64) -> Vec<ViewInput> {
        let Some(pos) = finite2(x, y) else {
            return Vec::new();
        };
        if self.pointers.iter().any(|p| p.id == id) || self.pointers.len() >= MAX_POINTERS {
            return Vec::new();
        }
        if self.pointers.is_empty() {
            self.moved = false;
            self.multi = false;
            self.velocity = Vec2::ZERO;
        }
        self.pointers.push(Pointer {
            id,
            kind,
            pos,
            down_pos: pos,
            down_ms: t_ms,
        });
        if self.pointers.len() >= 2 {
            self.multi = true;
        }
        self.last_move_ms = t_ms;
        vec![ViewInput::Grab]
    }

    /// A pointer moved to `(x, y)` pixels.
    pub fn pointer_move(&mut self, id: u64, x: f32, y: f32, t_ms: f64) -> Vec<ViewInput> {
        let Some(pos) = finite2(x, y) else {
            return Vec::new();
        };
        let Some(idx) = self.pointers.iter().position(|p| p.id == id) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if self.pointers.len() >= 2 {
            // The first two pointers make the two-finger gesture; others are ignored.
            if idx < 2 {
                let other = self.pointers[1 - idx].pos;
                let old = self.pointers[idx].pos;
                let (c0, c1) = ((old + other) * 0.5, (pos + other) * 0.5);
                let (d0, d1) = (old - other, pos - other);
                let pan = c1 - c0;
                if pan != Vec2::ZERO {
                    out.push(ViewInput::Pan {
                        dx_px: pan.x,
                        dy_px: pan.y,
                    });
                }
                if d0.length() > 1.0 && d1.length() > 1.0 {
                    let scale = d1.length() / d0.length();
                    if (scale - 1.0).abs() > 1e-6 {
                        out.push(ViewInput::Pinch { scale });
                    }
                    // Screen y points down, so the counter-clockwise angle is the negated one.
                    let twist = -d0.angle_to(d1).to_degrees();
                    if twist.abs() > 1e-4 {
                        out.push(ViewInput::Twist { deg: twist });
                    }
                }
                self.moved = true;
            }
        } else {
            let p = self.pointers[idx];
            let delta = pos - p.pos;
            if (pos - p.down_pos).length() > self.scaled(TAP_SLOP_PX) {
                self.moved = true;
            }
            if delta != Vec2::ZERO {
                match p.kind {
                    PointerKind::MouseSecondary => out.push(ViewInput::Pan {
                        dx_px: delta.x,
                        dy_px: delta.y,
                    }),
                    _ => out.push(ViewInput::Orbit {
                        dx_px: delta.x,
                        dy_px: delta.y,
                    }),
                }
                let dt = (t_ms - self.last_move_ms).max(1.0) as f32 / 1000.0;
                let v = delta / dt;
                // Exponential average of the last few moves, so one jittery sample does not fling.
                self.velocity = if v.is_finite() {
                    self.velocity * 0.4 + v * 0.6
                } else {
                    Vec2::ZERO
                };
            }
            self.last_move_ms = t_ms;
        }
        self.pointers[idx].pos = pos;
        out
    }

    /// A pointer lifted.
    pub fn pointer_up(&mut self, id: u64, t_ms: f64) -> Vec<ViewInput> {
        let Some(idx) = self.pointers.iter().position(|p| p.id == id) else {
            return Vec::new();
        };
        let p = self.pointers.remove(idx);
        let mut out = Vec::new();
        if !self.pointers.is_empty() {
            return out;
        }
        if self.multi {
            self.last_tap = None;
            return out;
        }
        let quick = t_ms - p.down_ms <= TAP_MS;
        if !self.moved && quick {
            match self.last_tap {
                Some((t0, pos0))
                    if t_ms - t0 <= DOUBLE_TAP_MS
                        && (p.pos - pos0).length() <= self.scaled(DOUBLE_TAP_PX) =>
                {
                    out.push(ViewInput::DoubleTap);
                    self.last_tap = None;
                }
                _ => self.last_tap = Some((t_ms, p.pos)),
            }
        } else {
            self.last_tap = None;
            if p.kind != PointerKind::MouseSecondary && t_ms - self.last_move_ms <= FLING_WINDOW_MS {
                out.push(ViewInput::Release {
                    vx_px_s: self.velocity.x,
                    vy_px_s: self.velocity.y,
                });
            }
        }
        self.velocity = Vec2::ZERO;
        out
    }

    /// A pointer was cancelled by the system (a palm, a gesture taken by the OS): forget it with
    /// no tap and no fling.
    pub fn pointer_cancel(&mut self, id: u64) -> Vec<ViewInput> {
        self.pointers.retain(|p| p.id != id);
        if self.pointers.is_empty() {
            self.velocity = Vec2::ZERO;
            self.last_tap = None;
        }
        Vec::new()
    }

    /// A wheel or trackpad scroll of `lines` (positive = away from the user = zoom in).
    pub fn wheel(&mut self, lines: f32) -> Vec<ViewInput> {
        if lines.is_finite() && lines != 0.0 {
            vec![ViewInput::Scroll { lines }]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tap_distances_grow_with_the_pixel_ratio() {
        let double_tap = |g: &mut GestureRecognizer, gap: f32| {
            g.pointer_down(1, PointerKind::Touch, 100.0, 100.0, 0.0);
            g.pointer_up(1, 20.0);
            g.pointer_down(2, PointerKind::Touch, 100.0 + gap, 100.0, 100.0);
            g.pointer_up(2, 120.0)
                .iter()
                .any(|i| matches!(i, ViewInput::DoubleTap))
        };
        assert!(!double_tap(&mut GestureRecognizer::new(), DOUBLE_TAP_PX * 2.0));
        let mut dense = GestureRecognizer::new();
        dense.set_pixel_ratio(3.0);
        assert!(double_tap(&mut dense, DOUBLE_TAP_PX * 2.0));
        // A press that moves inside the wider slop is still a tap, not a drag.
        let mut dense = GestureRecognizer::new();
        dense.set_pixel_ratio(3.0);
        dense.pointer_down(1, PointerKind::Touch, 0.0, 0.0, 0.0);
        dense.pointer_move(1, TAP_SLOP_PX * 2.0, 0.0, 10.0);
        dense.pointer_up(1, 20.0);
        dense.pointer_down(2, PointerKind::Touch, 0.0, 0.0, 100.0);
        assert!(
            dense
                .pointer_up(2, 120.0)
                .iter()
                .any(|i| matches!(i, ViewInput::DoubleTap))
        );
        // A bad ratio changes nothing.
        let mut g = GestureRecognizer::new();
        g.set_pixel_ratio(f32::NAN);
        assert!(!double_tap(&mut g, DOUBLE_TAP_PX * 2.0));
    }

    #[test]
    fn one_finger_orbits_and_flings() {
        let mut g = GestureRecognizer::new();
        g.pointer_down(1, PointerKind::Touch, 100.0, 100.0, 0.0);
        let mut last = Vec::new();
        for i in 1..=5 {
            last = g.pointer_move(1, 100.0 + 20.0 * i as f32, 100.0, 16.0 * i as f64);
        }
        assert!(matches!(last[0], ViewInput::Orbit { dx_px, .. } if dx_px == 20.0));
        let up = g.pointer_up(1, 90.0);
        assert!(
            matches!(up[0], ViewInput::Release { vx_px_s, .. } if vx_px_s > 1000.0),
            "{up:?}"
        );
    }

    #[test]
    fn a_slow_release_does_not_fling() {
        let mut g = GestureRecognizer::new();
        g.pointer_down(1, PointerKind::Touch, 0.0, 0.0, 0.0);
        g.pointer_move(1, 50.0, 0.0, 16.0);
        assert!(g.pointer_up(1, 500.0).is_empty());
    }

    /// Spec 42 § 3.4: the double-tap window is `DOUBLE_TAP_MS` between the two releases, inclusive.
    #[test]
    fn the_double_tap_window_ends_at_double_tap_ms() {
        let second_release = |gap_ms: f64| {
            let mut g = GestureRecognizer::new();
            g.pointer_down(1, PointerKind::Touch, 50.0, 50.0, 0.0);
            g.pointer_up(1, 10.0);
            g.pointer_down(2, PointerKind::Touch, 50.0, 50.0, gap_ms);
            g.pointer_up(2, 10.0 + gap_ms)
        };
        assert_eq!(second_release(DOUBLE_TAP_MS), vec![ViewInput::DoubleTap]);
        assert_eq!(second_release(DOUBLE_TAP_MS - 1.0), vec![ViewInput::DoubleTap]);
        assert!(second_release(DOUBLE_TAP_MS + 1.0).is_empty());
    }

    #[test]
    fn two_taps_reset() {
        let mut g = GestureRecognizer::new();
        g.pointer_down(1, PointerKind::Touch, 10.0, 10.0, 0.0);
        assert!(g.pointer_up(1, 80.0).is_empty());
        g.pointer_down(2, PointerKind::Touch, 14.0, 12.0, 200.0);
        assert_eq!(g.pointer_up(2, 260.0), vec![ViewInput::DoubleTap]);
        // Too far apart in time: no reset.
        g.pointer_down(3, PointerKind::Touch, 10.0, 10.0, 1000.0);
        g.pointer_up(3, 1050.0);
        g.pointer_down(4, PointerKind::Touch, 10.0, 10.0, 1500.0);
        assert!(g.pointer_up(4, 1550.0).is_empty());
    }

    #[test]
    fn two_fingers_pinch_pan_and_twist() {
        let mut g = GestureRecognizer::new();
        g.pointer_down(1, PointerKind::Touch, 100.0, 100.0, 0.0);
        g.pointer_down(2, PointerKind::Touch, 200.0, 100.0, 0.0);
        let out = g.pointer_move(2, 300.0, 100.0, 16.0);
        assert!(
            out.iter()
                .any(|i| matches!(i, ViewInput::Pinch { scale } if (*scale - 2.0).abs() < 1e-5))
        );
        assert!(
            out.iter()
                .any(|i| matches!(i, ViewInput::Pan { dx_px, .. } if (*dx_px - 50.0).abs() < 1e-5))
        );
        let out = g.pointer_move(2, 100.0, 0.0, 32.0);
        assert!(
            out.iter()
                .any(|i| matches!(i, ViewInput::Twist { deg } if (*deg - 90.0).abs() < 1e-3)),
            "{out:?}"
        );
        g.pointer_up(2, 40.0);
        assert!(
            g.pointer_up(1, 41.0).is_empty(),
            "lifting a pinch never flings or taps"
        );
    }

    #[test]
    fn right_mouse_pans() {
        let mut g = GestureRecognizer::new();
        g.pointer_down(9, PointerKind::MouseSecondary, 0.0, 0.0, 0.0);
        let out = g.pointer_move(9, 5.0, 6.0, 10.0);
        assert_eq!(
            out,
            vec![ViewInput::Pan {
                dx_px: 5.0,
                dy_px: 6.0
            }]
        );
        assert!(g.pointer_up(9, 15.0).is_empty());
    }
}
