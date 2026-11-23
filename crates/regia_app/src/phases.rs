// SPDX-License-Identifier: MPL-2.0
//! Main-thread frame breakdown: where the wall time between two [`crate::App::update`] calls went.
//!
//! [`crate::App::update`] times each schedule it runs with two `Instant::now()` reads per schedule
//! (no tracing, no allocation) and publishes the last completed frame as the [`FramePhases`]
//! resource. Telemetry aggregates it per summary window, so a player's log alone splits a slow
//! frame into ECS schedules, extract, render, frame pacing and the time spent outside the app
//! (the event loop, the OS).

use regia_ecs::prelude::*;

/// Number of entries in [`FramePhases::ms`].
pub const PHASE_COUNT: usize = 14;

/// Stable names of [`FramePhases::ms`], in index order (telemetry field names).
pub const PHASE_NAMES: [&str; PHASE_COUNT] = [
    "outside_update",
    "host_schedules",
    "pacing",
    "first",
    "pre_update",
    "state_transition",
    "fixed_update",
    "update",
    "post_update",
    "extract",
    "render",
    "last",
    "clear_trackers",
    "event_wait",
];

/// Index of each phase in [`FramePhases::ms`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum Phase {
    /// Wall time between the end of the previous `update` and the start of this one, minus
    /// [`Phase::HostSchedules`], [`Phase::Pacing`] and [`Phase::EventWait`]: event callbacks,
    /// other runner work and OS scheduling.
    OutsideUpdate = 0,
    /// Schedules the host ran through [`crate::App::run_schedule`] between two updates.
    HostSchedules = 1,
    /// [`crate::App::pace_frame`]'s wait for the frame cap (or a frozen loop's throttle).
    Pacing = 2,
    /// `Startup` (first frame only) and `First`.
    First = 3,
    /// `PreUpdate`.
    PreUpdate = 4,
    /// `StateTransition`.
    StateTransition = 5,
    /// Every `FixedUpdate` step of the frame.
    FixedUpdate = 6,
    /// `Update`.
    Update = 7,
    /// `PostUpdate`.
    PostUpdate = 8,
    /// `Extract`.
    Extract = 9,
    /// `Render` (surface acquire, `render_into`, present; `regia_render` splits it further).
    Render = 10,
    /// `Last` (telemetry reads the previous frame's value: it runs inside `Last`).
    Last = 11,
    /// `World::clear_trackers` at the end of the frame.
    ClearTrackers = 12,
    /// Time asleep in the window event loop between updates, excluding event callbacks.
    EventWait = 13,
}

/// The last completed frame's main-thread time by phase, published by [`crate::App::update`].
///
/// The entries sum to the wall time from the end of the update before it to the end of its own
/// update. Systems that read it inside a frame see the previous frame (the current one has not
/// finished).
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
pub struct FramePhases {
    /// Runner visibility and throttle configuration at this update's start; absent for custom loops.
    pub runner: Option<RunnerState>,
    /// Frames completed when this record was published (1 = the first frame).
    pub frame: u64,
    /// Milliseconds per phase, indexed by [`Phase`] (names in [`PHASE_NAMES`]).
    pub ms: [f32; PHASE_COUNT],
    /// `FixedUpdate` steps run in that frame.
    pub fixed_steps: u32,
}

impl FramePhases {
    /// Milliseconds of `phase`.
    #[must_use]
    pub fn get(&self, phase: Phase) -> f32 {
        self.ms[phase as usize]
    }

    /// Sum of every phase: the frame's wall time as the app saw it.
    #[must_use]
    pub fn total_ms(&self) -> f32 {
        self.ms.iter().sum()
    }
}

/// Timing state the app keeps between updates (not a resource: the app owns it).
#[derive(Debug, Default)]
pub(crate) struct PhaseClock {
    pub(crate) last_update_end: Option<std::time::Instant>,
    pub(crate) host_schedules: std::time::Duration,
    pub(crate) pacing: std::time::Duration,
    pub(crate) event_wait: std::time::Duration,
    pub(crate) frames: u64,
}

/// Milliseconds of a duration.
pub(crate) fn ms(d: std::time::Duration) -> f32 {
    d.as_secs_f32() * 1000.0
}

/// Window runner state sampled before an update. Focus starts false until an OS focus event.
/// Occlusion includes zero-sized windows; stalled and surface refusal are independent signals.
#[derive(Resource, Clone, Copy, Debug, Default, PartialEq)]
pub struct RunnerState {
    /// The primary window has received focus from the OS.
    pub focused: bool,
    /// Every window is occluded or zero-sized.
    pub occluded: bool,
    /// Requested redraws stopped arriving.
    pub stalled: bool,
    /// The OS repeatedly refused every attempted surface acquisition.
    pub surface_refused: bool,
    /// Configured hidden/frozen/idle period, milliseconds (not an additional post-update delay).
    pub throttle_ms: f32,
}

impl PhaseClock {
    /// Attribute only the measured wait within this between-update interval, exactly once.
    pub(crate) fn begin(&mut self, start: std::time::Instant, phases: &mut FramePhases) {
        if let Some(end) = self.last_update_end {
            let between = start.saturating_duration_since(end);
            let host = self.host_schedules.min(between);
            let pacing = self.pacing.min(between - host);
            let event_wait = self.event_wait.min(between - host - pacing);
            phases.ms[Phase::OutsideUpdate as usize] = ms(between - host - pacing - event_wait);
            phases.ms[Phase::HostSchedules as usize] = ms(host);
            phases.ms[Phase::Pacing as usize] = ms(pacing);
            phases.ms[Phase::EventWait as usize] = ms(event_wait);
        }
        self.host_schedules = std::time::Duration::ZERO;
        self.pacing = std::time::Duration::ZERO;
        self.event_wait = std::time::Duration::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn event_wait_is_separate_and_consumed_once() {
        let end = Instant::now();
        let mut clock = PhaseClock {
            last_update_end: Some(end),
            event_wait: Duration::from_millis(75),
            host_schedules: Duration::from_millis(4),
            pacing: Duration::from_millis(6),
            ..Default::default()
        };
        let mut phases = FramePhases::default();
        clock.begin(end + Duration::from_millis(90), &mut phases);
        assert_eq!(phases.get(Phase::EventWait), 75.0);
        assert_eq!(phases.get(Phase::OutsideUpdate), 5.0);
        assert_eq!(phases.total_ms(), 90.0);
        let mut next = FramePhases::default();
        clock.begin(end + Duration::from_millis(90), &mut next);
        assert_eq!(next.get(Phase::EventWait), 0.0);
        assert_eq!(next.total_ms(), 90.0);
    }

    #[test]
    fn wait_before_first_update_and_excess_wait_do_not_invent_time() {
        let now = Instant::now();
        let mut clock = PhaseClock {
            event_wait: Duration::from_secs(9),
            ..Default::default()
        };
        let mut phases = FramePhases::default();
        clock.begin(now, &mut phases);
        assert_eq!(phases.total_ms(), 0.0);
        clock.last_update_end = Some(now);
        clock.event_wait = Duration::from_secs(9);
        clock.begin(now + Duration::from_millis(10), &mut phases);
        assert_eq!(phases.get(Phase::EventWait), 10.0);
        assert_eq!(phases.get(Phase::OutsideUpdate), 0.0);
    }
}
