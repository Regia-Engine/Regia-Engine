// SPDX-License-Identifier: MPL-2.0
//! The lazy loop: a browser frame is requested only while something changed.
//!
//! No clock and no browser here. The host calls [`LazyLoop::invalidate`] on any change and requests an
//! animation frame when told to; while nothing is dirty nothing is pending, so an idle page holds no
//! callback and no timer.

/// The longest step one frame reports, in seconds: a stalled tab must not jump.
pub const MAX_STEP_S: f32 = 0.1;
/// The step of the first frame after an idle stretch, in seconds.
pub const FIRST_STEP_S: f32 = 1.0 / 60.0;

/// Whether a browser frame must be requested now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Schedule {
    /// Request one animation frame.
    Request,
    /// Nothing to do.
    Nothing,
}

/// What the loop has done, as the page and the tests see it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LoopCounters {
    /// Animation frames requested.
    pub requests: u64,
    /// Animation frames that arrived and were worked on.
    pub frames: u64,
}

/// The loop's state.
#[derive(Clone, Debug)]
pub struct LazyLoop {
    dirty: bool,
    pending: bool,
    visible: bool,
    stopped: bool,
    manual: bool,
    last_ms: Option<f64>,
    counters: LoopCounters,
}

impl Default for LazyLoop {
    fn default() -> Self {
        Self::new()
    }
}

impl LazyLoop {
    /// A visible loop with nothing to draw.
    pub fn new() -> Self {
        LazyLoop {
            dirty: false,
            pending: false,
            visible: true,
            stopped: false,
            manual: false,
            last_ms: None,
            counters: LoopCounters::default(),
        }
    }

    /// The counters so far.
    pub fn counters(&self) -> LoopCounters {
        self.counters
    }

    /// Whether a browser frame is pending.
    pub fn pending(&self) -> bool {
        self.pending
    }

    /// Whether something waits to be drawn.
    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Whether the page is visible.
    pub fn visible(&self) -> bool {
        self.visible
    }

    /// Whether the host steps frames itself ([`LazyLoop::set_manual`]).
    pub fn manual(&self) -> bool {
        self.manual
    }

    fn schedule(&mut self) -> Schedule {
        if self.dirty && self.visible && !self.pending && !self.stopped && !self.manual {
            self.pending = true;
            self.counters.requests += 1;
            Schedule::Request
        } else {
            Schedule::Nothing
        }
    }

    /// Something changed and needs a frame.
    pub fn invalidate(&mut self) -> Schedule {
        self.dirty = true;
        self.schedule()
    }

    /// The page became visible or hidden. Hidden: no frames until visible again, and the host cancels
    /// the pending one when this leaves [`LazyLoop::pending`] false. Visible: one frame is drawn, since a
    /// browser may have dropped the canvas content.
    pub fn set_visible(&mut self, visible: bool) -> Schedule {
        if visible == self.visible {
            return Schedule::Nothing;
        }
        self.visible = visible;
        self.last_ms = None;
        if visible {
            self.invalidate()
        } else {
            self.pending = false;
            Schedule::Nothing
        }
    }

    /// Frames are stepped by the host, for scripted capture: nothing is requested from the browser while
    /// on. Turning it off requests a frame if something is dirty.
    pub fn set_manual(&mut self, manual: bool) -> Schedule {
        self.manual = manual;
        self.last_ms = None;
        if manual {
            self.pending = false;
            Schedule::Nothing
        } else {
            self.schedule()
        }
    }

    /// The host cancelled the browser frame it had requested (a lost device): nothing is pending now, so the
    /// next change requests a frame again instead of waiting for one that never comes. What is dirty stays
    /// dirty.
    pub fn forget_pending(&mut self) {
        self.pending = false;
        self.last_ms = None;
    }

    /// The loop ends for good; nothing is requested again.
    pub fn stop(&mut self) {
        self.stopped = true;
        self.pending = false;
        self.dirty = false;
    }

    /// A browser frame arrived at `now_ms`. The step to integrate in seconds, or `None` when the frame
    /// is stale (cancelled by hiding or stopping while on its way).
    pub fn frame_begin(&mut self, now_ms: f64) -> Option<f32> {
        if !self.pending || self.stopped || !self.visible {
            return None;
        }
        self.pending = false;
        self.counters.frames += 1;
        self.dirty = false;
        let step = match self.last_ms {
            Some(last) if now_ms >= last => (((now_ms - last) / 1000.0) as f32).min(MAX_STEP_S),
            _ => FIRST_STEP_S,
        };
        self.last_ms = Some(now_ms);
        Some(step)
    }

    /// The frame's work is done. `animating`: the view still moves, so it needs the next frame.
    pub fn frame_end(&mut self, animating: bool) -> Schedule {
        if animating {
            self.dirty = true;
        } else if !self.dirty {
            // The next frame starts a new stretch.
            self.last_ms = None;
        }
        self.schedule()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_holds_nothing_pending() {
        let l = LazyLoop::new();
        assert!(!l.pending());
        assert_eq!(l.counters().requests, 0);
    }

    #[test]
    fn many_changes_request_one_frame() {
        let mut l = LazyLoop::new();
        assert_eq!(l.invalidate(), Schedule::Request);
        assert_eq!(l.invalidate(), Schedule::Nothing);
        assert_eq!(l.invalidate(), Schedule::Nothing);
        assert_eq!(l.counters().requests, 1);
    }

    #[test]
    fn a_settled_frame_stops_the_loop() {
        let mut l = LazyLoop::new();
        l.invalidate();
        assert_eq!(l.frame_begin(0.0), Some(FIRST_STEP_S));
        assert_eq!(l.frame_end(false), Schedule::Nothing);
        assert!(!l.pending());
        assert!(!l.dirty());
    }

    #[test]
    fn animation_keeps_requesting_with_real_steps() {
        let mut l = LazyLoop::new();
        l.invalidate();
        l.frame_begin(1000.0);
        assert_eq!(l.frame_end(true), Schedule::Request);
        let dt = l.frame_begin(1016.0).expect("a frame");
        assert!((dt - 0.016).abs() < 1e-6, "{dt}");
        assert_eq!(l.frame_end(false), Schedule::Nothing);
    }

    #[test]
    fn a_stall_does_not_jump() {
        let mut l = LazyLoop::new();
        l.invalidate();
        l.frame_begin(0.0);
        l.frame_end(true);
        assert_eq!(l.frame_begin(60_000.0), Some(MAX_STEP_S));
    }

    #[test]
    fn the_first_frame_after_idle_is_one_short_step() {
        let mut l = LazyLoop::new();
        l.invalidate();
        l.frame_begin(0.0);
        l.frame_end(false);
        l.invalidate();
        assert_eq!(l.frame_begin(90_000.0), Some(FIRST_STEP_S));
    }

    #[test]
    fn hidden_requests_nothing_and_a_stale_frame_is_dropped() {
        let mut l = LazyLoop::new();
        l.invalidate();
        assert_eq!(l.set_visible(false), Schedule::Nothing);
        assert!(!l.pending());
        assert_eq!(l.frame_begin(5.0), None);
        assert_eq!(l.invalidate(), Schedule::Nothing);
        assert_eq!(l.counters().frames, 0);
    }

    #[test]
    fn showing_again_draws_one_frame_even_when_clean() {
        let mut l = LazyLoop::new();
        l.set_visible(false);
        assert_eq!(l.set_visible(true), Schedule::Request);
        assert!(l.frame_begin(1.0).is_some());
        assert_eq!(l.frame_end(false), Schedule::Nothing);
    }

    #[test]
    fn repeating_a_visibility_state_is_nothing() {
        let mut l = LazyLoop::new();
        assert_eq!(l.set_visible(true), Schedule::Nothing);
        assert!(!l.dirty());
    }

    #[test]
    fn a_change_during_a_frame_gets_the_next_frame() {
        let mut l = LazyLoop::new();
        l.invalidate();
        l.frame_begin(0.0);
        // A listener called from inside the frame changes something.
        assert_eq!(l.invalidate(), Schedule::Request);
        assert_eq!(l.frame_end(false), Schedule::Nothing, "already requested");
        assert!(l.pending());
    }

    #[test]
    fn manual_mode_never_asks_the_browser() {
        let mut l = LazyLoop::new();
        l.invalidate();
        assert_eq!(l.set_manual(true), Schedule::Nothing);
        assert!(!l.pending());
        assert_eq!(l.invalidate(), Schedule::Nothing);
        assert_eq!(l.set_manual(false), Schedule::Request, "the dirty frame goes out");
    }

    #[test]
    fn a_stopped_loop_stays_stopped() {
        let mut l = LazyLoop::new();
        l.invalidate();
        l.stop();
        assert_eq!(l.frame_begin(1.0), None);
        assert_eq!(l.invalidate(), Schedule::Nothing);
        assert_eq!(l.set_visible(false), Schedule::Nothing);
        assert_eq!(l.set_visible(true), Schedule::Nothing);
    }
}
