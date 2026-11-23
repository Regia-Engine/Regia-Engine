// SPDX-License-Identifier: MPL-2.0
//! Display-locked frame pacing (N12, ADR-1942, `docs/specs/11-platform-and-input.md` § 3a).
//!
//! A phone that renders as fast as it can stuffs the swapchain queue: frames reach the screen in
//! bursts and every frame carries a buffer of extra latency. The cure, from the Android frame
//! pacing documentation, is to start each frame on a display vsync and to hold every frame on
//! screen for the same whole number of vsyncs (the swap interval), picked so the slowest common
//! frame still fits.
//!
//! - [`PacingModel`] is that logic as a pure state machine over nanosecond timestamps: it estimates
//!   the refresh period from vsync ticks, picks the swap interval, and says which tick releases
//!   the next frame. [`simulate`] drives it with virtual time, on every platform, for the tests and
//!   for `pacing.simulate`.
//! - [`VsyncTicks`] carries ticks from the platform's vsync thread to the frame loop.
//! - [`DisplayPacer`] is the model plus the ticks, called once per frame by the runner. Without
//!   ticks (no platform source, the display off, a stall) it hands the frame back to the software
//!   cap, [`regia_app::FramePacing`], so a missing tick never blocks a frame for long.
//!
//! Android is the only tick source so far (`platform/android/vsync.rs`, `AChoreographer`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use regia_app::prelude::*;

/// Vsync intervals the period estimate looks back over.
const PERIOD_WINDOW: usize = 16;
/// Intervals needed before the estimate is trusted.
const MIN_PERIOD_SAMPLES: usize = 4;
/// A tick interval below this is not a display (500 Hz).
const MIN_TICK_NS: u64 = 2_000_000;
/// A tick interval above this is a gap in the ticks, not a refresh rate (5 Hz).
const MAX_TICK_NS: u64 = 200_000_000;
/// A period that moves more than 1 / this is a refresh-rate change.
const PERIOD_CHANGE_DIVISOR: u64 = 20;
/// A frame held on screen for longer than this looks worse than a missed slot now and then, so
/// the interval stops rising here (20 fps).
pub const MAX_SLOT_NS: u64 = 50_000_000;
/// The largest swap interval `pacing.set` accepts (a forced interval). The automatic ceiling is
/// the slot limit [`MAX_SLOT_NS`] instead.
pub const MAX_SWAP_INTERVAL: u32 = 8;
/// An over-budget frame adds 2 to the raise counter, a frame on budget takes 1 away; at this
/// value the interval rises. Four frames in a row, or about a third of the frames over time.
const RAISE_AT: u32 = 8;
/// Frames a lower interval must fit before the interval drops (doubles on a bounce).
const BASE_HOLD_FRAMES: u32 = 60;
/// The longest such wait.
const MAX_HOLD_FRAMES: u32 = 960;
/// A lower interval must leave 1 / this of the frame cost spare.
const HEADROOM_DIVISOR: u64 = 8;
/// A frame costing at most `1 - 1 / this` of the frame that last raised the slot counts towards a
/// probe of the shorter slot (ADR-2339).
const PROBE_DIVISOR: u64 = 4;
/// What a frame that does not count towards a probe takes off the count: a probe needs about two
/// thirds of the frames to count, so the spikes of a real phone do not hold it off forever.
const STUCK_LEAK: u32 = 2;
/// The longest a probe of a shorter slot runs before the frames are judged against it.
const PROBE_FRAMES: u32 = 120;
/// Frames in a row that fit the probed slot to end the probe early.
const PROBE_SETTLE: u32 = 8;
/// The largest [`PacingSim::stretch`].
pub const MAX_SIM_STRETCH: f32 = 0.95;
/// Ticks this close before the slot still start it (half a vsync hides the tick jitter).
const SLOT_TOLERANCE_DIVISOR: u64 = 2;
/// A frame that ends within period / this after a tick still starts on that tick.
const LATE_START_DIVISOR: u64 = 8;
/// Frames in a row the swapchain held back that also started at once on a missed slot's tick
/// before one frame waits a vsync to drain the display queue ([`PacingModel::set_queue_drain`],
/// ADR-5564).
const DRAIN_AFTER_HELD: u32 = 8;
/// The most such frames between two drains: a drain that does not stick doubles the run the
/// next one needs, up to this (one vsync in about 17 s at 60 Hz).
const MAX_DRAIN_AFTER: u32 = 1024;
/// A queue that fills again within this many frames of the run after a drain did not stay
/// drained: the frames are limited by the display queue itself, not stuck one image ahead.
const DRAIN_SETTLE: u32 = 8;
/// A tick further than period / this from a whole number of periods after the last one is on a
/// grid the compositor moved ([`PacingModel::set_grid_follow`], ADR-5946). Tick times jitter by
/// tens of microseconds; a 120 Hz panel that gives the app 60 Hz ticks moves them by half a period.
const GRID_MOVE_DIVISOR: u64 = 8;
/// Timed-out waits in a row before the frame loop stops waiting for ticks.
const STALL_LIMIT: u32 = 3;
/// The longest a frame waits for a tick: one slot of the lowest software cap (`MIN_PACING_FPS`).
const MAX_WAIT: Duration = Duration::from_secs(1);
/// Ticks kept for the frame loop between two frames.
const PENDING_TICKS: usize = 32;

/// How frames are paced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PacingMode {
    /// On the display's vsync where the platform has a tick source, with an automatic swap interval.
    #[default]
    Auto,
    /// Only the software cap ([`regia_app::FramePacing`]).
    Off,
    /// On the display's vsync, this many vsyncs per frame.
    Interval(u32),
}

/// What [`PacingMode::Auto`] counts as a frame's cost when it picks the swap interval (ADR-2320).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PacingCost {
    /// The frame's work: its time from start to end less the time it sat blocked in the
    /// swapchain (acquire and present, reported by the renderer with [`note_swapchain_wait`]).
    /// That blocking is the display queue draining at the rate pacing itself chose, so counting
    /// it makes every slow slot look full and the interval only ever rises. A frame the
    /// swapchain held back also starts at once when it has missed its tick
    /// ([`PacingModel::swapchain_held`]).
    #[default]
    Work,
    /// The whole time from start to end, swapchain blocking included, and no notice of the
    /// swapchain otherwise: the v0.2.10 model.
    Frame,
}

impl PacingCost {
    /// The name `pacing.get` reports and `pacing.set` takes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Frame => "frame",
        }
    }
}

/// Which rate the pacer suggests to the compositor while the slot is longer than one vsync
/// (`ANativeWindow_setFrameRate`, ADR-2339).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PacingHint {
    /// The slowest rate on the panel's grid that still gives every frame two vsyncs or more. A
    /// phone's display queue holds two of the app's buffers, so a frame needs about two latches:
    /// a panel slowed to the slot's own rate ran the Fairphone 5 at half the slot's rate, whatever
    /// the pacing (a 33 ms slot at 15 fps).
    #[default]
    Double,
    /// The slot's own rate, one vsync per frame: the v0.2.10 hint.
    Slot,
    /// No hint: the panel keeps its rate and the swap interval does the pacing.
    Off,
}

impl PacingHint {
    /// The name `pacing.get` reports and `pacing.set` takes.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Double => "double",
            Self::Slot => "slot",
            Self::Off => "off",
        }
    }
}

/// The requested [`PacingMode`] (`pacing.set`).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FramePacingSettings {
    /// The mode the next frame uses.
    pub mode: PacingMode,
    /// What `Auto` counts as a frame's cost.
    pub cost: PacingCost,
    /// Which rate goes to the compositor.
    pub hint: PacingHint,
}

/// The frame loop's performance hint session (ADR-5461; Android's `APerformanceHint`, API 33):
/// the slot as the target and each paced frame's work as the actual time, so the scheduler keeps
/// the frame loop and the vsync thread on cores and clocks that meet the slot. `enabled` is the
/// request (`pacing.set` `perf_hint`); the runner writes the rest. Platforms without the API,
/// and older phones, keep `active` false. Off by default: on the TEST Pixel 9 Pro it did not shorten
/// the late frame starts the wake-ahead removes ([`FrameWakeAhead`]), and the frames there are
/// not short of CPU (ADR-5461).
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FramePerfHint {
    /// Keep a session open while frames are paced on the display (default `false`).
    pub enabled: bool,
    /// A session is open.
    pub active: bool,
    /// The platform gave no session (no API, or it refused); not asked again until `enabled`
    /// is switched off and on.
    pub unavailable: bool,
    /// The session's target, nanoseconds (the slot).
    pub target_ns: Option<u64>,
    /// Frames reported to the session.
    pub reports: u64,
    /// Reports and target updates the platform refused.
    pub failures: u64,
}

/// Wake-ahead for frames paced on the display (ADR-5461). A frame loop that sleeps until its release
/// tick arrives can take a millisecond or more to run again after the tick wakes it (on the TEST Pixel
/// 9 Pro, about one frame in ten), and that late start is a long frame interval. With a wake-ahead the
/// loop sleeps only until `lead_ns` before the tick is due to reach it (the predicted tick plus the
/// measured delivery delay), then polls for the tick, so it is already running when the tick comes.
/// A callback more than [`PREDICTED_RELEASE_MARGIN_NS`] late releases the frame on the predicted tick.
/// The poll stops `lead_ns` after the due time and the loop sleeps for the tick as before. `None`
/// sleeps until the tick. `pacing.set` `wake_ahead_ms`.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameWakeAhead {
    /// How long before the tick is due the frame loop stops sleeping, nanoseconds; at most
    /// [`MAX_WAKE_AHEAD_NS`]. Default [`DEFAULT_WAKE_AHEAD_NS`].
    pub lead_ns: Option<u64>,
    /// Frames so far whose tick the poll caught (written by the runner).
    pub polled_releases: u64,
    /// Frames so far released on a predicted tick because its callback ran late (written by the
    /// runner; [`PREDICTED_RELEASE_MARGIN_NS`]).
    pub predicted_releases: u64,
}

impl Default for FrameWakeAhead {
    fn default() -> Self {
        Self {
            lead_ns: Some(DEFAULT_WAKE_AHEAD_NS),
            polled_releases: 0,
            predicted_releases: 0,
        }
    }
}

/// The default [`FrameWakeAhead`]: 2.5 ms, longer than the frame loop's measured wake-up delay after
/// a timed sleep on the TEST Pixel 9 Pro (p50 1.7 ms, p99 about 2.1 ms; ADR-5461).
pub const DEFAULT_WAKE_AHEAD_NS: u64 = 2_500_000;

/// The largest [`FrameWakeAhead`]: 4 ms.
pub const MAX_WAKE_AHEAD_NS: u64 = 4_000_000;

/// How far past its usual delivery (the predicted tick plus the running mean delay) a tick's callback
/// may run before the wake-ahead poll releases the frame on the predicted tick: 0.6 ms. On the TEST
/// Pixel 9 Pro delivery is 0.5 ms at the median and under 1 ms at p99, with rare callbacks 1.3-2.4 ms
/// late (the vsync thread woken late); a frame started that late is a long interval (ADR-5461).
/// Only a tick one slot past the newest real tick is predicted.
pub const PREDICTED_RELEASE_MARGIN_NS: u64 = 600_000;

/// The delivery delay assumed before a tick source has stamped any tick: 0.5 ms, about what
/// Choreographer showed on the TEST Pixel 9 Pro (ADR-5461).
pub const DEFAULT_DELIVERY_NS: u64 = 500_000;

/// Steady frame starts for frames paced on the display (ADR-5946).
///
/// A frame that starts when its tick's callback is seen inherits the callback's delivery jitter: on
/// the TEST Pixel 9 Pro the callback reaches the vsync thread 0.3 to 1.0 ms after the tick's own time,
/// so two frame starts in a row are up to 0.7 ms further apart than the ticks they started on, and the
/// p99 of the frame interval sat at 1.03x the refresh with nothing wrong (the limit is 1.05x). With
/// `grid` the frame is released when its tick is *due* to reach the frame loop: the tick's time on
/// the grid of the real ticks plus the usual delivery delay ([`next_grid_phase`]), whether the
/// callback has run yet or not. The callbacks still set the grid, so a display that stops ticking
/// stops the frames, and a tick that arrives early waits for its due time. Needs the wake-ahead
/// ([`FrameWakeAhead`]): the loop has to be running at the due time.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSteadyStart {
    /// Release each paced frame when its tick is due on the grid of the vsync ticks (default
    /// `true`); `false` releases it when the tick's callback is seen (ADR-5461).
    pub grid: bool,
    /// Follow a tick grid the compositor moved (default `true`; [`PacingModel::set_grid_follow`]).
    pub grid_follow: bool,
    /// Where a frame released on the grid starts against its tick's due time, nanoseconds:
    /// negative starts it that long before the tick is due, positive after (default
    /// [`DEFAULT_START_OFFSET_NS`], at most [`MAX_START_OFFSET_NS`] either way). The compositor
    /// wakes for its own frame at the app's tick; a frame that starts right then asks for its
    /// swapchain image while the compositor is still handing the last one back, and waits for it.
    pub start_offset_ns: i64,
    /// Held late starts in a row before a frame waits a vsync to drain the display queue (default
    /// [`STEADY_DRAIN_RUN`]; [`PacingModel::set_drain_run`], ADR-5564).
    pub drain_run: u32,
    /// Frames so far released on the grid (written by the runner).
    pub grid_releases: u64,
}

impl Default for FrameSteadyStart {
    fn default() -> Self {
        Self {
            grid: true,
            grid_follow: true,
            start_offset_ns: DEFAULT_START_OFFSET_NS,
            drain_run: STEADY_DRAIN_RUN,
            grid_releases: 0,
        }
    }
}

/// The default [`FrameSteadyStart::start_offset_ns`].
pub const DEFAULT_START_OFFSET_NS: i64 = 0;

/// The largest [`FrameSteadyStart::start_offset_ns`] either way: 8 ms.
pub const MAX_START_OFFSET_NS: i64 = 8_000_000;

/// The default [`FrameSteadyStart::drain_run`]: 2. One held late start can be a single slow
/// compositor frame; two in a row is a display queue that stays full. ADR-5564 waited for 8, and
/// each of them is a frame start that follows the compositor instead of the vsync.
pub const STEADY_DRAIN_RUN: u32 = 2;

/// How the display pacer released a frame ([`PacingTraceRow::release`], ADR-5946).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingRelease {
    /// Woken by the tick from a sleep, or the tick was already there.
    Slept,
    /// The wake-ahead poll saw the tick ([`FrameWakeAhead`]).
    Polled,
    /// The tick's callback was late; released on the predicted tick (ADR-5461).
    Predicted,
    /// Released when its tick was due on the tick grid ([`FrameSteadyStart`]).
    Grid,
}

impl PacingRelease {
    /// The name `pacing.trace` reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Slept => "slept",
            Self::Polled => "polled",
            Self::Predicted => "predicted",
            Self::Grid => "grid",
        }
    }
}

/// What the scheduler has done with the calling thread so far, for the pacing trace
/// ([`DisplayPacer::set_sched_probe`], ADR-5946). Linux and Android: `/proc/thread-self/schedstat`,
/// `getrusage(RUSAGE_THREAD)` and `getcpu`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SchedSample {
    /// Total time the thread was ready to run but waited for a CPU, nanoseconds.
    pub run_delay_ns: u64,
    /// Times the scheduler took the CPU away from the thread (involuntary context switches).
    pub preemptions: u64,
    /// The CPU the thread is on now.
    pub cpu: u32,
}

/// The run delay of a thread from its `/proc/<pid>/task/<tid>/schedstat` text: the second of the
/// three numbers (time on a CPU, time waiting for one, time slices; `proc(5)`), nanoseconds. `None`
/// for anything else.
#[must_use]
pub fn parse_schedstat_run_delay(text: &str) -> Option<u64> {
    let mut fields = text.split_ascii_whitespace();
    let _on_cpu: u64 = fields.next()?.parse().ok()?;
    let run_delay = fields.next()?.parse().ok()?;
    let _slices: u64 = fields.next()?.parse().ok()?;
    Some(run_delay)
}

/// The first tick at or past `bound` on the grid of `latest_tick + k * period_ns`: the tick the pacer
/// waits for. `None` without a period.
#[must_use]
pub fn expected_release_tick(latest_tick: u64, period_ns: u64, bound: u64) -> Option<u64> {
    if period_ns == 0 {
        return None;
    }
    let ahead = bound.saturating_sub(latest_tick);
    latest_tick.checked_add(ahead.div_ceil(period_ns).checked_mul(period_ns)?)
}

/// The next delivery-delay estimate after a tick that reached the vsync thread `delivery_ns` after
/// its own time: a running mean over about 8 ticks, at most `period_ns / 4` (a stray late callback
/// does not move the poll window far).
#[must_use]
pub fn next_delivery_estimate(estimate_ns: u64, delivery_ns: u64, period_ns: u64) -> u64 {
    let sample = delivery_ns.min(period_ns / 4);
    (estimate_ns * 7 + sample) / 8
}

/// The grid phase after a tick that reached the vsync thread `delivery_ns` after its own time: how
/// long after a tick's time a frame released on the grid starts ([`FrameSteadyStart`], ADR-5946). A
/// mean over about 32 ticks of samples held within `period_ns / 64` of the phase, at most
/// `period_ns / 4`: it follows the phone's usual delivery delay within a few seconds, and one late
/// callback moves it by microseconds, so the frame starts stay one period apart.
#[must_use]
pub fn next_grid_phase(phase_ns: u64, delivery_ns: u64, period_ns: u64) -> u64 {
    let reach = period_ns / 64;
    let sample = delivery_ns.clamp(phase_ns.saturating_sub(reach), phase_ns.saturating_add(reach));
    ((phase_ns * 31 + sample + 16) / 32).min(period_ns / 4)
}

/// The performance hint target for a slot of `interval` vsyncs of `period_ns`: the slot itself
/// (ADR-5461). `None` without a period estimate or interval.
#[must_use]
pub fn perf_hint_target_ns(period_ns: Option<u64>, interval: u32) -> Option<u64> {
    let period = period_ns.filter(|p| *p > 0)?;
    (interval > 0).then(|| period.saturating_mul(u64::from(interval)))
}

/// A target this close to the session's (1 / this of it) is not sent again: the period estimate
/// wobbles by a few microseconds (ADR-5461).
pub const PERF_HINT_RETARGET_DIVISOR: u64 = 50;

/// Whether `new` differs enough from the session's `current` target to update it.
#[must_use]
pub fn perf_hint_retarget(current: Option<u64>, new: u64) -> bool {
    current.is_none_or(|c| c.abs_diff(new) > c / PERF_HINT_RETARGET_DIVISOR)
}

/// Swapchain blocking of the current frame, nanoseconds (see [`note_swapchain_wait`]).
static SWAPCHAIN_WAIT_NS: AtomicU64 = AtomicU64::new(0);

/// Adds time the current frame spent blocked on the swapchain (`get_current_texture` and
/// `present`). The renderer calls it once per window frame; the runner takes the sum when the
/// frame ends ([`take_swapchain_wait`]).
pub fn note_swapchain_wait(wait: Duration) {
    let nanos = u64::try_from(wait.as_nanos()).unwrap_or(u64::MAX);
    SWAPCHAIN_WAIT_NS.fetch_add(nanos, Ordering::Relaxed);
}

/// The swapchain blocking noted since the last call, and resets it.
#[must_use]
pub fn take_swapchain_wait() -> Duration {
    Duration::from_nanos(SWAPCHAIN_WAIT_NS.swap(0, Ordering::Relaxed))
}

/// Frames a [`FramePacingTrace`] keeps.
pub const TRACE_CAPACITY: usize = 1024;

/// One frame as the display pacer saw it (`pacing.trace`). Times are nanoseconds on the clock
/// of the vsync ticks (`CLOCK_MONOTONIC` on Android).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct PacingTraceRow {
    /// When the frame started.
    pub start_ns: u64,
    /// From the start to the end of the frame's work, swapchain blocking included.
    pub cost_ns: u64,
    /// Of that, time blocked in the swapchain (acquire and present).
    pub swapchain_wait_ns: u64,
    /// Time the pacer then waited for a tick (0: the frame was not paced on the display).
    pub waited_ns: u64,
    /// The tick that released the next frame (`None`: not paced, or the wait timed out).
    pub release_tick_ns: Option<u64>,
    /// Vsync ticks that arrived during this frame and its wait.
    pub ticks: u64,
    /// The refresh period estimate after this frame.
    pub period_ns: Option<u64>,
    /// The swap interval after this frame.
    pub interval: u32,
    /// The compositor rate hint in effect after this frame.
    pub hint_hz: Option<f32>,
    /// The frame was paced on the display's ticks.
    pub paced: bool,
    /// The longest interval between two vsync ticks the pacer received during this frame and its
    /// wait (0: fewer than two ticks). A tick source that keeps time shows one period here
    /// (ADR-4913).
    pub tick_gap_max_ns: u64,
    /// Vsyncs the tick source skipped during this frame: whole refresh periods missing between
    /// two received ticks. 0 while the ticks keep up, whatever the frames do (ADR-4913).
    pub missed_ticks: u64,
    /// Time the software cap ([`regia_app::FramePacing`]) then slept, for a frame the display did
    /// not pace (ADR-4913).
    pub cap_waited_ns: u64,
    /// When the release tick's callback ran on the platform's vsync thread (`None`: no release
    /// tick, or a source that does not stamp delivery). Less `release_tick_ns`, the
    /// compositor's delivery delay (ADR-5461).
    pub tick_delivered_ns: Option<u64>,
    /// When the frame loop came back from waiting for the release tick (`None`: not paced).
    /// Less `tick_delivered_ns`, the hand-off from the vsync thread; the next row's `start_ns`
    /// less this, the event loop's own time (ADR-5461).
    pub woke_ns: Option<u64>,
    /// How the frame was released (`None`: not paced, or the wait timed out; ADR-5946).
    pub release: Option<PacingRelease>,
    /// How long after its target the frame loop came back from the timed sleep that precedes the
    /// wake-ahead poll (`None`: no such sleep). More than the wake-ahead is a late start (ADR-5946).
    pub sleep_late_ns: Option<u64>,
    /// Time the frame loop thread was ready to run but had no CPU during the frame's work
    /// (`None`: no scheduler probe; ADR-5946).
    pub work_run_delay_ns: Option<u64>,
    /// The same during the wait for the release tick.
    pub wait_run_delay_ns: Option<u64>,
    /// Times the scheduler took the CPU from the frame loop thread during the frame and its wait.
    pub preemptions: Option<u64>,
    /// The CPU the frame loop was on when the wait returned.
    pub wake_cpu: Option<u32>,
}

/// What held the frames of a trace back the most ([`PacingBreakdown::limit`], ADR-4913).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingLimit {
    /// The frame's own work (CPU, and a GPU the CPU waits for).
    Work,
    /// Blocking in the swapchain (acquire and present): back-pressure from the display queue.
    Swapchain,
    /// The display pacer waiting for the slot's tick.
    Wait,
    /// The software cap's sleep.
    Cap,
    /// Time outside the frame: the event loop between frames.
    Loop,
    /// The tick source itself: vsync ticks went missing, so slots started late.
    Ticks,
}

impl PacingLimit {
    /// The name `pacing.trace` reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Work => "work",
            Self::Swapchain => "swapchain",
            Self::Wait => "wait",
            Self::Cap => "cap",
            Self::Loop => "loop",
            Self::Ticks => "ticks",
        }
    }
}

/// A trace split into where the time between frame starts went ([`pacing_breakdown`], ADR-4913).
/// Each interval from one frame's start to the next is that frame's work, its swapchain blocking,
/// the pacer's wait, the software cap's sleep and the rest (the event loop), so the shares add up
/// to 1. Tick cadence is counted apart: the ticks run on their own thread, whatever the frames do.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PacingBreakdown {
    /// Frame-to-frame intervals measured (rows - 1).
    pub intervals: u32,
    /// Median interval between frame starts, ns.
    pub interval_p50_ns: u64,
    /// Median work (cost less swapchain blocking), ns.
    pub work_p50_ns: u64,
    /// Median swapchain blocking, ns.
    pub swapchain_p50_ns: u64,
    /// Median pacer wait, ns.
    pub wait_p50_ns: u64,
    /// Median software-cap sleep, ns.
    pub cap_p50_ns: u64,
    /// Median time outside the frame, ns.
    pub loop_p50_ns: u64,
    /// Shares of the total interval time: work, swapchain, wait, cap, loop (sum 1).
    pub shares: [f64; 5],
    /// The newest refresh period estimate in the trace, ns.
    pub period_ns: Option<u64>,
    /// Median interval in refresh periods.
    pub vsyncs_per_frame: Option<f64>,
    /// Ticks received over the measured intervals.
    pub ticks: u64,
    /// Ticks the source skipped over the measured intervals.
    pub missed_ticks: u64,
    /// The longest tick-to-tick gap seen, ns.
    pub tick_gap_max_ns: u64,
    /// What held the frames back the most.
    pub limit: PacingLimit,
}

/// Missed ticks above 1 / this of the expected ticks make the tick source the limit.
const MISSED_TICK_DIVISOR: u64 = 8;

/// Splits `rows` (oldest first, as [`FramePacingTrace::rows`] gives them) into where the time
/// between frame starts went. `None` with fewer than two rows. The limit is the tick source when
/// more than an eighth of the expected vsyncs never arrived; otherwise the largest share, ties to
/// the first of work, swapchain, wait, cap, loop.
#[must_use]
pub fn pacing_breakdown(rows: &[PacingTraceRow]) -> Option<PacingBreakdown> {
    if rows.len() < 2 {
        return None;
    }
    let mut intervals = Vec::with_capacity(rows.len() - 1);
    let mut parts: [Vec<u64>; 5] = Default::default();
    let mut totals = [0u64; 5];
    let mut ticks = 0u64;
    let mut missed = 0u64;
    let mut gap_max = 0u64;
    for pair in rows.windows(2) {
        let (row, next) = (pair[0], pair[1]);
        let interval = next.start_ns.saturating_sub(row.start_ns);
        let swap = row.swapchain_wait_ns.min(row.cost_ns);
        let work = row.cost_ns - swap;
        let inside = row
            .cost_ns
            .saturating_add(row.waited_ns)
            .saturating_add(row.cap_waited_ns);
        let outside = interval.saturating_sub(inside);
        for (i, value) in [work, swap, row.waited_ns, row.cap_waited_ns, outside]
            .into_iter()
            .enumerate()
        {
            parts[i].push(value);
            totals[i] = totals[i].saturating_add(value);
        }
        intervals.push(interval);
        // A row's ticks arrived during that frame; the next row's ticks during the interval.
        ticks = ticks.saturating_add(next.ticks);
        missed = missed.saturating_add(next.missed_ticks);
        gap_max = gap_max.max(next.tick_gap_max_ns);
    }
    let p50 = |values: &mut Vec<u64>| {
        values.sort_unstable();
        values[values.len() / 2]
    };
    let sum: u64 = totals.iter().fold(0u64, |a, b| a.saturating_add(*b));
    let shares = totals.map(|t| if sum == 0 { 0.0 } else { t as f64 / sum as f64 });
    let interval_p50 = p50(&mut intervals);
    let period = rows.iter().rev().find_map(|r| r.period_ns);
    let ticks_short = missed.saturating_mul(MISSED_TICK_DIVISOR) > ticks.saturating_add(missed);
    let limit = if ticks_short && missed > 0 {
        PacingLimit::Ticks
    } else {
        let order = [
            PacingLimit::Work,
            PacingLimit::Swapchain,
            PacingLimit::Wait,
            PacingLimit::Cap,
            PacingLimit::Loop,
        ];
        let mut best = 0;
        for i in 1..order.len() {
            if totals[i] > totals[best] {
                best = i;
            }
        }
        order[best]
    };
    let [work, swap, wait, cap, outside] = &mut parts;
    Some(PacingBreakdown {
        intervals: u32::try_from(rows.len() - 1).unwrap_or(u32::MAX),
        interval_p50_ns: interval_p50,
        work_p50_ns: p50(work),
        swapchain_p50_ns: p50(swap),
        wait_p50_ns: p50(wait),
        cap_p50_ns: p50(cap),
        loop_p50_ns: p50(outside),
        shares,
        period_ns: period,
        vsyncs_per_frame: period.filter(|p| *p > 0).map(|p| interval_p50 as f64 / p as f64),
        ticks,
        missed_ticks: missed,
        tick_gap_max_ns: gap_max,
        limit,
    })
}

/// The tick cadence the pacer saw since the last trace row (ADR-4913).
#[derive(Debug, Clone, Copy, Default)]
struct TickCadence {
    last: Option<u64>,
    gap_max: u64,
    missed: u64,
}

impl TickCadence {
    /// One received tick; `period` is the refresh estimate before it.
    fn note(&mut self, tick_ns: u64, period: Option<u64>) {
        match self.last {
            Some(last) if tick_ns <= last => return,
            Some(last) => {
                let gap = tick_ns - last;
                // A gap past `MAX_TICK_NS` is the ticks stopping (background), not a skipped vsync.
                if gap <= MAX_TICK_NS {
                    self.gap_max = self.gap_max.max(gap);
                    if let Some(period) = period.filter(|p| *p > 0) {
                        let vsyncs = (gap + period / 2) / period;
                        self.missed += vsyncs.saturating_sub(1);
                    }
                }
            }
            None => {}
        }
        self.last = Some(tick_ns);
    }

    /// The gap and missed count since the last call; the timeline is kept.
    fn take(&mut self) -> (u64, u64) {
        let out = (self.gap_max, self.missed);
        self.gap_max = 0;
        self.missed = 0;
        out
    }
}

/// The last [`TRACE_CAPACITY`] frames of the display pacer, newest last (`pacing.trace`). Only a
/// platform with a vsync tick source (Android) fills it.
#[derive(Resource, Debug, Clone, Default)]
pub struct FramePacingTrace {
    rows: std::collections::VecDeque<PacingTraceRow>,
    /// Also log every frame's row (target `regia_pacing`, level info: logcat on Android).
    pub log: bool,
}

impl FramePacingTrace {
    /// Appends a row, dropping the oldest past [`TRACE_CAPACITY`].
    pub fn push(&mut self, row: PacingTraceRow) {
        if self.rows.len() == TRACE_CAPACITY {
            self.rows.pop_front();
        }
        self.rows.push_back(row);
        if self.log {
            let ms = |ns: u64| ns as f64 / 1e6;
            tracing::info!(
                target: "regia_pacing",
                "frame start={} cost={:.2} swap_wait={:.2} waited={:.2} tick={} ticks={} period={:.2} interval={} hint={:?} paced={} tick_gap_max={:.2} missed_ticks={}",
                row.start_ns,
                ms(row.cost_ns),
                ms(row.swapchain_wait_ns),
                ms(row.waited_ns),
                row.release_tick_ns.map_or(-1, |t| i64::try_from(t).unwrap_or(-1)),
                row.ticks,
                row.period_ns.map_or(0.0, ms),
                row.interval,
                row.hint_hz,
                row.paced,
                ms(row.tick_gap_max_ns),
                row.missed_ticks
            );
        }
    }

    /// Records the software cap's sleep on the newest row: the runner calls the cap after the
    /// display pacer has pushed the row of a frame it did not pace (ADR-4913).
    pub fn note_cap_wait(&mut self, waited: Duration) {
        if let Some(row) = self.rows.back_mut() {
            row.cap_waited_ns = u64::try_from(waited.as_nanos()).unwrap_or(u64::MAX);
        }
    }

    /// The rows, oldest first.
    pub fn rows(&self) -> impl ExactSizeIterator<Item = &PacingTraceRow> + DoubleEndedIterator {
        self.rows.iter()
    }

    /// Forgets every row.
    pub fn clear(&mut self) {
        self.rows.clear();
    }
}

/// Where a frame's start time comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PacingSource {
    /// The software cap: `FramePacing`, then whatever the swapchain blocks on.
    #[default]
    Software,
    /// Android `AChoreographer` vsync callbacks.
    Choreographer,
}

impl PacingSource {
    /// The name `pacing.get` reports.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Software => "software",
            Self::Choreographer => "choreographer",
        }
    }
}

/// Counters of a [`PacingModel`], since it started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PacingStats {
    /// Frames released on a tick.
    pub frames: u64,
    /// Of those, frames that started a vsync or more after their slot, and frames that started at
    /// once on their slot's tick after the previous frame overran it ([`PacingModel::set_late_start`],
    /// ADR-4913): the same overrun counts the same with and without the late start.
    pub late: u64,
    /// Frames the swapchain held back ([`PacingModel::swapchain_held`]): the display queue set
    /// their start, so they are counted here and not as late.
    pub held: u64,
    /// Times the swap interval changed.
    pub interval_changes: u64,
    /// Times the refresh period changed.
    pub refresh_changes: u64,
    /// Times the model tried a shorter slot its frames did not fit on paper
    /// ([`PacingModel::set_probe`], ADR-2339).
    pub probes: u64,
    /// Frames held a vsync to drain a display queue that stayed one image full
    /// ([`PacingModel::set_queue_drain`], ADR-5564).
    pub drains: u64,
    /// Times the compositor moved the tick grid and the model followed
    /// ([`PacingModel::set_grid_follow`], ADR-5946).
    pub grid_moves: u64,
}

/// What pacing did so far; the runner updates it every frame and `pacing.get` reads it.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct FramePacingReport {
    /// Where frame starts come from right now.
    pub source: PacingSource,
    /// Vsyncs seen from the platform.
    pub ticks: u64,
    /// The estimated refresh rate.
    pub refresh_hz: Option<f32>,
    /// Vsyncs per frame.
    pub swap_interval: u32,
    /// The model's counters.
    pub stats: PacingStats,
    /// Waits that ended without a tick (the display off, a stalled tick thread).
    pub timeouts: u64,
    /// The rate last suggested to the compositor, frames per second (`None`: none).
    pub frame_rate_hint: Option<f32>,
}

/// The pure pacing state machine. Times are nanoseconds on the clock of the vsync ticks.
#[derive(Debug, Clone)]
pub struct PacingModel {
    last_tick: Option<u64>,
    deltas: [u64; PERIOD_WINDOW],
    delta_count: usize,
    period: Option<u64>,
    /// The panel's own period: what the ticks run at with no hint of ours in effect.
    panel: Option<u64>,
    /// A shorter slot asked of the compositor, kept until the panel moves.
    goal: Option<u64>,
    interval: u32,
    over: u32,
    fit: u32,
    /// The costliest frame of the current run of fitting frames.
    fit_peak: u64,
    hold: u32,
    frames_seen: u64,
    lowered_at: Option<u64>,
    last_release: Option<u64>,
    /// The swapchain held the last frame back for a noticeable time ([`Self::swapchain_held`]).
    held: bool,
    /// Probe shorter slots ([`Self::set_probe`]).
    probe: bool,
    /// The costliest frame that raised the slot, or the one that ended the last bounced probe
    /// (0: none did).
    raised_by: u64,
    /// Frames well below `raised_by` that still did not fit the shorter slot, less
    /// `STUCK_LEAK` for every other frame.
    stuck: u32,
    /// Frames left in the current probe (0: none running).
    probe_left: u32,
    /// Frames in a row that fit the probed slot.
    probe_fit: u32,
    /// The slot the last probe left, to go back to if it bounces.
    probe_from: Option<u64>,
    /// Which rate goes to the compositor ([`Self::set_hint`]).
    hint: PacingHint,
    /// A frame that missed its slot's tick starts at once ([`Self::set_late_start`], ADR-4913).
    late_start: bool,
    /// The frame [`Self::plan`] last planned ended past its slot, so [`Self::released`] counts it
    /// as late even when its release tick is the slot's own (ADR-4913).
    ended_late: std::cell::Cell<bool>,
    /// Drain a display queue that stays full ([`Self::set_queue_drain`], ADR-5564).
    queue_drain: bool,
    /// Frames in a row [`Self::plan`] started at once because the swapchain held them back.
    held_starts: std::cell::Cell<u32>,
    /// The run of such frames the next drain needs (`DRAIN_AFTER_HELD`, doubled when a drain
    /// does not stick).
    drain_after: std::cell::Cell<u32>,
    /// Frames planned since the last drain (`u32::MAX`: none yet).
    since_drain: std::cell::Cell<u32>,
    /// The frame [`Self::plan`] last planned waits a vsync to drain the queue.
    draining: std::cell::Cell<bool>,
    /// The run of held late starts the first drain needs ([`Self::set_drain_run`]).
    drain_run: u32,
    /// Follow a tick grid the compositor moved ([`Self::set_grid_follow`]).
    grid_follow: bool,
    stats: PacingStats,
}

impl Default for PacingModel {
    fn default() -> Self {
        Self::new()
    }
}

impl PacingModel {
    /// A model that has seen no tick yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            last_tick: None,
            deltas: [0; PERIOD_WINDOW],
            delta_count: 0,
            period: None,
            panel: None,
            goal: None,
            interval: 1,
            over: 0,
            fit: 0,
            fit_peak: 0,
            hold: BASE_HOLD_FRAMES,
            frames_seen: 0,
            lowered_at: None,
            last_release: None,
            held: false,
            probe: true,
            raised_by: 0,
            stuck: 0,
            probe_left: 0,
            probe_fit: 0,
            probe_from: None,
            hint: PacingHint::Slot,
            late_start: false,
            ended_late: std::cell::Cell::new(false),
            queue_drain: false,
            held_starts: std::cell::Cell::new(0),
            drain_after: std::cell::Cell::new(DRAIN_AFTER_HELD),
            since_drain: std::cell::Cell::new(u32::MAX),
            draining: std::cell::Cell::new(false),
            drain_run: DRAIN_AFTER_HELD,
            grid_follow: false,
            stats: PacingStats::default(),
        }
    }

    /// Feeds a vsync timestamp. A repeated or older tick and a gap in the ticks teach nothing.
    pub fn on_tick(&mut self, tick_ns: u64) {
        let Some(last) = self.last_tick else {
            self.last_tick = Some(tick_ns);
            return;
        };
        if tick_ns <= last {
            return;
        }
        self.last_tick = Some(tick_ns);
        let delta = tick_ns - last;
        if !(MIN_TICK_NS..=MAX_TICK_NS).contains(&delta) {
            return;
        }
        if self.grid_follow
            && let Some(period) = self.period
        {
            self.follow_grid(tick_ns, delta, period);
        }
        self.deltas[self.delta_count % PERIOD_WINDOW] = delta;
        self.delta_count += 1;
        self.estimate_period();
    }

    /// A tick `delta` after the last one: when that is not a whole number of periods, the
    /// compositor moved the grid ([`Self::set_grid_follow`]). A frame released on a tick the old
    /// grid promised (a predicted tick, past the last real one) has taken this tick's slot, so the
    /// next slot is a whole one after this tick. A frame released on a real tick has this tick as
    /// its next slot's tick, however early it comes.
    fn follow_grid(&mut self, tick_ns: u64, delta: u64, period: u64) {
        let whole = (delta + period / 2) / period * period;
        if delta.abs_diff(whole) <= period / GRID_MOVE_DIVISOR {
            return;
        }
        self.stats.grid_moves += 1;
        if let Some(release) = self.last_release {
            let last_real = tick_ns - delta;
            let predicted = release > last_real + period / GRID_MOVE_DIVISOR;
            self.last_release = Some(if predicted {
                tick_ns
            } else {
                tick_ns.saturating_sub(period)
            });
        }
    }

    /// The lower quartile of the recent tick intervals: a skipped tick only ever makes an
    /// interval longer, so this holds even when most ticks are skipped.
    fn estimate_period(&mut self) {
        let count = self.delta_count.min(PERIOD_WINDOW);
        if count < MIN_PERIOD_SAMPLES {
            return;
        }
        let mut sorted = self.deltas;
        sorted[..count].sort_unstable();
        let estimate = sorted[count / 4];
        if let Some(old) = self.period
            && old.abs_diff(estimate) > old / PERIOD_CHANGE_DIVISOR
        {
            // A panel moving while a hint is out is taken to be following it. Otherwise this is
            // the panel's rate now, whoever changed it.
            self.panel = match self.panel {
                Some(panel) if self.frame_rate_hint().is_some() => Some(panel.min(estimate)),
                _ => Some(estimate),
            };
            // Keep the slot, not the count of vsyncs: the frames still cost what they did.
            // The 20 fps limit is that duration (`MAX_SLOT_NS`), so the new interval is however
            // many vsyncs hold it. `MAX_SWAP_INTERVAL` is only the cap on a forced `Interval`.
            // A slot already slower than 20 fps (a software cap) keeps its own duration.
            let slot = self.goal.take().unwrap_or(u64::from(self.interval) * old);
            let pace_ceiling = Self::intervals_for(MAX_SLOT_NS, estimate).max(1);
            let preserve = u32::try_from((slot + estimate / 2) / estimate).unwrap_or(u32::MAX);
            self.interval = if slot > MAX_SLOT_NS {
                preserve.max(1)
            } else {
                preserve.clamp(1, pace_ceiling)
            };
            self.over = 0;
            self.fit = 0;
            self.stuck = 0;
            self.stats.refresh_changes += 1;
            // The old slot lies between two of the new grid's and the nearer one is the longer
            // (25 ms of a 120 Hz grid is 33 ms at 60 Hz). Nothing the frames did says they need
            // that, and a longer slot is hard to leave: coming down takes a whole hold of frames
            // that fit, and a phone's frames cost more the longer they are left idle (ADR-2339).
            // So take the shorter one as a probe. Frames that do not fit it go back to the longer
            // (ADR-5946).
            let shorter = u32::try_from(slot / estimate).unwrap_or(u32::MAX).max(1);
            let kept = u64::from(self.interval) * estimate;
            if self.probe
                && slot <= MAX_SLOT_NS
                && shorter < self.interval
                && kept > slot + estimate / PERIOD_CHANGE_DIVISOR
            {
                self.probe_from = Some(kept);
                self.interval = shorter;
                self.stats.probes += 1;
                self.probe_left = PROBE_FRAMES;
                self.probe_fit = 0;
                self.lowered_at = Some(self.frames_seen);
            }
        }
        self.period = Some(estimate);
        self.panel.get_or_insert(estimate);
    }

    /// The newest tick the model has seen (`None` before the first, or after a restart).
    #[must_use]
    pub fn last_tick_ns(&self) -> Option<u64> {
        self.last_tick
    }

    /// The estimated refresh period.
    #[must_use]
    pub fn period_ns(&self) -> Option<u64> {
        self.period
    }

    /// Vsyncs per frame.
    #[must_use]
    pub fn swap_interval(&self) -> u32 {
        self.interval
    }

    /// Whether the model probes shorter slots (on by default; [`PacingCost::Work`] runs with it,
    /// [`PacingCost::Frame`] without, as v0.2.10 did). A frame's cost can grow with the slot
    /// pacing gives it: a phone's clocks fall when the frames leave it idle, and a GPU that waits
    /// for the display's buffers waits longer at a slower rate. Such a cost never fits a shorter
    /// slot on paper. So when the slot was raised by a frame much costlier than the current ones
    /// (they cost at most three quarters of it) and the current ones still do not fit the next shorter slot, the
    /// model tries that slot after a hold anyway. For up to `PROBE_FRAMES` frames, or until
    /// `PROBE_SETTLE` in a row fit, a frame that misses its tick starts at once (an idle wait
    /// would keep the clocks down) and an over-budget frame does not count towards a raise. If the
    /// frames then fit, the model stays; if not, the raise that follows goes back to the slot the
    /// probe left and doubles the next wait (ADR-2339).
    pub fn set_probe(&mut self, probe: bool) {
        self.probe = probe;
    }

    /// Which rate [`Self::frame_rate_hint`] suggests ([`PacingHint`]). A new model hints the
    /// slot's own rate ([`PacingHint::Slot`], the v0.2.10 model); the runner sets
    /// [`FramePacingSettings::hint`], [`PacingHint::Double`] by default.
    pub fn set_hint(&mut self, hint: PacingHint) {
        self.hint = hint;
    }

    /// Whether a frame that ended after its slot's tick starts at once on that tick instead of
    /// waiting for the next one (off for a new model; the runner turns it on with
    /// [`PacingCost::Work`]). Waiting costs a whole idle vsync for an overrun of a fraction of one:
    /// on a 60 Hz phone with a three-vsync slot a frame 3 ms over its 50 ms slot was shown after
    /// 66.7 ms, and its idle wait moved every later slot by a vsync (ADR-4913). The frame is still
    /// over budget: [`Self::end_frame`] counts it towards a raise as before.
    pub fn set_late_start(&mut self, on: bool) {
        self.late_start = on;
    }

    /// Whether a display queue that stays one image full is drained (off for a new model; the
    /// runner turns it on with [`PacingCost::Work`]). A held frame that missed its slot's tick
    /// starts at once ([`Self::plan`], ADR-2320), and so does any late frame with
    /// [`Self::set_late_start`]. After a burst (pacing off, a hitch, a preset change) the queue
    /// can hold one image more than the slot needs; every frame then blocks in the swapchain,
    /// starts at once, and refills it, so frame starts follow the compositor's buffer releases
    /// instead of the vsync (a 60 Hz phone showed a presented p99 of 1.25-1.29x the refresh with
    /// work of 9 ms, against 1.03-1.05x when not stuck). With this on, after `DRAIN_AFTER_HELD`
    /// such frames in a row the next one waits for the following tick, which shows the last
    /// image a vsync longer and leaves the queue one image emptier. A queue that fills again
    /// right away (frames really limited by the display queue) doubles the run the next drain
    /// needs, up to `MAX_DRAIN_AFTER` (ADR-5564).
    pub fn set_queue_drain(&mut self, on: bool) {
        self.queue_drain = on;
        if !on {
            self.held_starts.set(0);
        }
    }

    /// Whether a tick grid the compositor moved is followed (off for a new model; the runner turns
    /// it on with [`PacingCost::Work`]). A phone whose panel runs faster than the rate it gives the
    /// app (the TEST Pixel 9 Pro: a 120 Hz panel, 60 Hz ticks) moves the app's ticks by one panel
    /// vsync now and then: the next tick comes half a period off the old grid. Half a period is
    /// exactly the tolerance [`Self::plan`] gives a slot's tick, so by a few microseconds either way
    /// the model took the moved tick for the slot's own or not. A frame released on a predicted
    /// tick ([`FrameSteadyStart`], ADR-5461) that the compositor then issued half a period later
    /// was followed, half the time, by a frame started at once on that late tick: one frame more
    /// than the compositor had ticks, a display queue one image fuller from then on, and every
    /// frame held in the swapchain until a drain ([`Self::set_queue_drain`]). With this on, a tick
    /// that is not a whole number of periods after the last one (off by more than an eighth) moves
    /// the last release onto the new grid. After a release on a predicted tick it is this tick (the
    /// frame in flight took its slot, the next frame has a whole slot from here). After a release
    /// on a real tick it is one period before this tick, so this tick is the next slot's and a
    /// frame that is done starts on it at once, as it would on any tick up to half a period early
    /// (ADR-5946).
    pub fn set_grid_follow(&mut self, on: bool) {
        self.grid_follow = on;
    }

    /// How many held late starts in a row the first drain waits for ([`Self::set_queue_drain`];
    /// 8 for a new model, at least 1). A drain that does not stick still doubles the run.
    pub fn set_drain_run(&mut self, run: u32) {
        let run = run.clamp(1, MAX_DRAIN_AFTER);
        if run != self.drain_run {
            self.drain_run = run;
            self.drain_after.set(run);
            self.held_starts.set(0);
        }
    }

    /// The counters so far.
    #[must_use]
    pub fn stats(&self) -> PacingStats {
        self.stats
    }

    /// The fewest vsyncs that hold `cap_ns`: the cap is a limit, so a 45 fps cap on a 60 Hz panel
    /// runs at 30. The 2 % tolerance keeps a 60 fps cap on a 60.2 Hz panel at one vsync.
    #[must_use]
    pub fn intervals_for(cap_ns: u64, period_ns: u64) -> u32 {
        let exact = (cap_ns as f64 / period_ns.max(1) as f64 - 0.02).ceil();
        (exact as u32).max(1)
    }

    /// The automatic ceiling: the fewest vsyncs whose slot holds [`MAX_SLOT_NS`] (20 fps), so a
    /// 50 ms frame always fits (on a 90 Hz panel that is 5 vsyncs, 55.6 ms). Unlike a forced
    /// interval it is not limited to [`MAX_SWAP_INTERVAL`]: on a 240 Hz panel it is 12.
    #[must_use]
    pub fn slot_ceiling(period_ns: u64) -> u32 {
        Self::intervals_for(MAX_SLOT_NS, period_ns)
    }

    /// Accounts for a finished frame: `cost_ns` of work from its start to now, the software cap's
    /// period, and an interval the host forces.
    pub fn end_frame(&mut self, cost_ns: u64, cap_ns: Option<u64>, forced: Option<u32>) {
        let Some(period) = self.period else {
            return;
        };
        self.frames_seen += 1;
        if let Some(interval) = forced {
            // Vsyncs of the panel: the interval over ticks that followed the hint would double
            // the slot again, and that hint would halve the ticks again.
            let panel = self.panel.unwrap_or(period);
            let slot = u64::from(interval.clamp(1, MAX_SWAP_INTERVAL)) * panel;
            let ticks = (slot + period / 2) / period;
            self.set_interval(u32::try_from(ticks).unwrap_or(MAX_SWAP_INTERVAL).max(1));
            self.goal = None;
            self.over = 0;
            self.fit = 0;
            self.stuck = 0;
            self.probe_left = 0;
            self.probe_from = None;
            return;
        }
        let floor = cap_ns.map_or(1, |cap| Self::intervals_for(cap, period));
        let ceiling = Self::slot_ceiling(period).max(floor);
        self.set_interval(self.interval.clamp(floor, ceiling));
        if self.hold > BASE_HOLD_FRAMES
            && self
                .lowered_at
                .is_some_and(|at| self.frames_seen - at > u64::from(self.hold) * 4)
        {
            self.hold = BASE_HOLD_FRAMES;
            self.lowered_at = None;
        }
        let probing = self.probe_left > 0;
        if probing {
            self.probe_left -= 1;
            // The probed slot: the goal while the compositor has not followed it yet.
            let probed = self.goal.unwrap_or(u64::from(self.interval) * period);
            self.probe_fit = if cost_ns <= probed { self.probe_fit + 1 } else { 0 };
            if self.probe_fit >= PROBE_SETTLE {
                self.probe_left = 0;
            }
            if self.probe_left == 0 {
                // A bounce is counted from the end of the probe.
                self.lowered_at = Some(self.frames_seen);
            }
        }
        // A longer slot asked of the compositor (a probe going back) is the slot already.
        let slot = (u64::from(self.interval) * period).max(self.goal.unwrap_or(0));
        if cost_ns > slot {
            if !probing {
                self.over = (self.over + 2).min(RAISE_AT);
            }
            self.fit = 0;
        } else {
            self.over = self.over.saturating_sub(1);
        }
        if self.over >= RAISE_AT && self.interval < ceiling {
            self.raise(cost_ns, period, ceiling);
            return;
        }
        let drop_interval = self.interval > floor;
        let lower = if drop_interval {
            Some(u64::from(self.interval - 1) * period)
        } else {
            self.shorter_panel_slot(period, cap_ns)
        };
        let Some(lower) = lower else {
            self.stuck = 0;
            return;
        };
        let fits = cost_ns + cost_ns / HEADROOM_DIVISOR <= lower;
        if self.probe
            && self.probe_left == 0
            && !fits
            && self.raised_by > 0
            && cost_ns <= self.raised_by - self.raised_by / PROBE_DIVISOR
        {
            self.stuck += 1;
        } else {
            // Leaky, not reset: a phone's frames spike now and then (ADR-2339).
            self.stuck = self.stuck.saturating_sub(STUCK_LEAK);
        }
        if fits {
            if self.fit == 0 {
                self.fit_peak = 0;
            }
            self.fit += 1;
            self.fit_peak = self.fit_peak.max(cost_ns);
        } else {
            self.fit = 0;
        }
        if self.fit >= self.hold {
            // Straight down to the shortest slot the costliest frame of the hold fits with the
            // same headroom (ADR-2320): one step per hold took 10-20 s after a boot stall.
            let need = self.fit_peak + self.fit_peak / HEADROOM_DIVISOR;
            if drop_interval {
                let fits = u32::try_from(need.div_ceil(period)).unwrap_or(u32::MAX);
                self.set_interval(fits.clamp(floor, self.interval - 1));
            } else {
                self.goal = Some(self.shortest_panel_slot(need, period, cap_ns).unwrap_or(lower));
            }
            self.fit = 0;
            self.over = 0;
            self.stuck = 0;
            self.probe_from = None;
            self.lowered_at = Some(self.frames_seen);
        } else if self.stuck >= self.hold {
            // A probe (ADR-2339): one step down, whatever the cost says. Frames that do not fit
            // there raise the slot again soon, and that bounce doubles the hold.
            self.probe_from = Some(self.goal.unwrap_or(u64::from(self.interval) * period));
            if drop_interval {
                self.set_interval(self.interval - 1);
            } else {
                self.goal = Some(lower);
            }
            self.stats.probes += 1;
            self.probe_left = PROBE_FRAMES;
            self.probe_fit = 0;
            self.fit = 0;
            self.over = 0;
            self.stuck = 0;
            self.lowered_at = Some(self.frames_seen);
        }
    }

    /// The panel followed a hint, so one vsync is already the whole slot and the interval has
    /// nowhere lower to go. A shorter slot then means asking the compositor: the next one on the
    /// panel's own grid, if the cap allows it.
    fn shorter_panel_slot(&self, period: u64, cap_ns: Option<u64>) -> Option<u64> {
        let panel = self.panel?;
        if self.interval != 1 || self.goal.is_some() || period <= panel + panel / PERIOD_CHANGE_DIVISOR {
            return None;
        }
        let steps = (period + panel / 2) / panel;
        let lower = panel * steps.saturating_sub(1).max(1);
        let allowed = cap_ns.is_none_or(|cap| Self::intervals_for(cap, lower) == 1);
        (lower < period && allowed).then_some(lower)
    }

    /// The shortest slot on the panel's grid, below the current `period`, that holds `need_ns`
    /// and that the cap allows.
    fn shortest_panel_slot(&self, need_ns: u64, period: u64, cap_ns: Option<u64>) -> Option<u64> {
        let panel = self.panel?.max(1);
        let steps = (period + panel / 2) / panel;
        (1..steps)
            .map(|k| panel * k)
            .find(|slot| *slot >= need_ns && cap_ns.is_none_or(|cap| Self::intervals_for(cap, *slot) == 1))
    }

    /// The slot a hint of ours asks for, while that is longer than the panel's own period.
    fn hinted_slot(&self) -> Option<u64> {
        let panel = self.panel?;
        let slot = self
            .goal
            .or_else(|| Some(u64::from(self.interval) * self.period?))?;
        (slot > panel + panel / PERIOD_CHANGE_DIVISOR).then_some(slot)
    }

    /// The rate to suggest to the compositor, frames per second; `None` at the panel's own rate.
    /// It describes the slot, so it does not clear when the panel follows it: that would send the
    /// panel straight back. [`PacingHint::Slot`] asks for the slot's rate, so one vsync becomes the
    /// whole slot. [`PacingHint::Double`] asks for the slowest whole divisor of the panel's rate
    /// that still divides the slot into two vsyncs or more (a slot of `n` panel vsyncs: `n / d`
    /// vsyncs of `d` panel vsyncs each, `d` the largest divisor of `n` up to `n / 2`), and nothing
    /// when that is the panel's own rate. [`PacingHint::Off`] never asks.
    #[must_use]
    pub fn frame_rate_hint(&self) -> Option<f32> {
        let slot = self.hinted_slot()?;
        match self.hint {
            PacingHint::Slot => Some(hz_from_ns(slot)),
            PacingHint::Off => None,
            PacingHint::Double => {
                let panel = self.panel?.max(1);
                let vsyncs = ((slot + panel / 2) / panel).max(1);
                let each = (1..=vsyncs / 2).rev().find(|d| vsyncs.is_multiple_of(*d))?;
                (each > 1).then(|| hz_from_ns(panel * each))
            }
        }
    }

    /// Up to where the last frame would have fit; a raise soon after a drop was a bounce, so the
    /// next drop waits longer. A bounce after a probe goes back to the slot the probe left.
    fn raise(&mut self, cost_ns: u64, period: u64, ceiling: u32) {
        let mut back = None;
        if let Some(at) = self.lowered_at.take()
            && self.frames_seen - at <= u64::from(self.hold) * 2
        {
            self.hold = (self.hold * 2).min(MAX_HOLD_FRAMES);
            back = self.probe_from;
        }
        self.probe_from = None;
        // The reference for a probe: the costliest raise so far (a start-up stall), until a probe
        // bounces; then the frame that ended it, so a cost that needs the slot is probed once.
        self.raised_by = if back.is_some() {
            cost_ns
        } else {
            self.raised_by.max(cost_ns)
        };
        match back.filter(|slot| *slot > u64::from(self.interval) * period) {
            Some(slot) => {
                let ticks = (slot + period / 2) / period;
                if (ticks * period).abs_diff(slot) <= period / PERIOD_CHANGE_DIVISOR {
                    self.set_interval(u32::try_from(ticks).unwrap_or(ceiling).clamp(1, ceiling));
                } else {
                    // Off the grid of the current ticks: ask the compositor for it again.
                    self.goal = Some(slot);
                }
            }
            None => {
                let need = u32::try_from(cost_ns.div_ceil(period)).unwrap_or(ceiling);
                self.set_interval(need.clamp(self.interval + 1, ceiling));
            }
        }
        self.over = 0;
        self.fit = 0;
        self.stuck = 0;
        self.probe_left = 0;
    }

    fn set_interval(&mut self, interval: u32) {
        if interval != self.interval {
            self.interval = interval;
            self.goal = None;
            self.stats.interval_changes += 1;
        }
    }

    /// Tells the model how long the frame that just ended sat blocked in the swapchain (acquire
    /// and present), before [`Self::plan`]. More than a fraction of a vsync means the display
    /// queue was full and the compositor's latch released the frame: it has been paced by the
    /// display already (ADR-2320).
    pub fn swapchain_held(&mut self, wait_ns: u64) {
        self.held = self
            .period
            .is_some_and(|period| wait_ns > period / LATE_START_DIVISOR);
    }

    /// The earliest tick that may release the next frame, or `None` while there is no period
    /// yet. The next frame starts on a tick after the last one (a frame that ended just after a
    /// tick still takes that tick) and no sooner than its slot. A frame the swapchain held back
    /// ([`Self::swapchain_held`]) that has missed its slot's tick starts at once on that tick:
    /// the latch that freed its buffer comes just after a tick, so waiting for the next tick
    /// would cost a whole slot on every frame (ADR-2320). So does a late frame while a probe runs
    /// ([`Self::set_probe`], ADR-2339), and any late frame with [`Self::set_late_start`] (ADR-4913).
    #[must_use]
    pub fn plan(&self, now_ns: u64) -> Option<u64> {
        self.ended_late.set(false);
        let period = self.period?;
        let latest = self.last_tick?;
        let tolerance = period / SLOT_TOLERANCE_DIVISOR;
        let slot = self
            .last_release
            .map_or(0, |last| last + u64::from(self.interval) * period);
        self.draining.set(false);
        self.since_drain.set(self.since_drain.get().saturating_add(1));
        if (self.held || self.probe_left > 0 || self.late_start) && slot <= latest + tolerance {
            if self.queue_drain && self.held && self.last_release.is_some() {
                let run = self.held_starts.get() + 1;
                if run >= self.drain_after.get() {
                    // The queue stayed full for the whole run: skip a vsync to drain it.
                    let after =
                        if self.since_drain.get() < self.drain_after.get().saturating_add(DRAIN_SETTLE) {
                            (self.drain_after.get().saturating_mul(2)).min(MAX_DRAIN_AFTER)
                        } else {
                            self.drain_run
                        };
                    self.drain_after.set(after);
                    self.held_starts.set(0);
                    self.since_drain.set(0);
                    self.draining.set(true);
                    return Some(latest + period / 2);
                }
                self.held_starts.set(run);
            } else {
                self.held_starts.set(0);
            }
            // Started on its own slot's tick, but after it: still a late frame.
            self.ended_late.set(
                self.late_start && self.last_release.is_some() && now_ns > slot + period / LATE_START_DIVISOR,
            );
            return Some(latest);
        }
        self.held_starts.set(0);
        let next = if now_ns.saturating_sub(latest) <= period / LATE_START_DIVISOR {
            latest
        } else {
            latest + period / 2
        };
        Some(next.max(slot.saturating_sub(tolerance)))
    }

    /// How long the next frame may wait for its tick: its whole slot plus a vsync, so a slow
    /// slot is not mistaken for a stall. `None` while there is no period yet.
    #[must_use]
    pub fn wait_limit(&self) -> Option<Duration> {
        let slot = self.period?.saturating_mul(u64::from(self.interval) + 1);
        Some(Duration::from_nanos(slot).min(MAX_WAIT))
    }

    /// Records the tick that released a frame.
    pub fn released(&mut self, tick_ns: u64) {
        self.stats.frames += 1;
        let ended_late = self.ended_late.take();
        if self.draining.take() {
            self.stats.drains += 1;
        }
        if self.held {
            self.stats.held += 1;
        } else if let (Some(last), Some(period)) = (self.last_release, self.period) {
            let slot = last + u64::from(self.interval) * period;
            if ended_late || tick_ns > slot + period / SLOT_TOLERANCE_DIVISOR {
                self.stats.late += 1;
            }
        }
        self.last_release = Some(tick_ns);
    }

    /// The next frame has no slot to be late for: this one was not released on a tick.
    pub fn forget_slot(&mut self) {
        self.last_release = None;
    }

    /// Forgets the tick timeline, keeping the period: the ticks stopped (the app went to the
    /// background) and the first tick after that is no slot to compare with.
    pub fn restart(&mut self) {
        self.last_tick = None;
        self.last_release = None;
        self.held = false;
        self.goal = None;
        self.over = 0;
        self.fit = 0;
        self.stuck = 0;
        self.probe_left = 0;
        self.probe_from = None;
        self.held_starts.set(0);
        self.since_drain.set(u32::MAX);
        self.drain_after.set(self.drain_run);
    }
}

/// The ticks between the platform's vsync thread and the frame loop.
#[derive(Debug, Default)]
pub struct VsyncTicks {
    state: Mutex<TickState>,
    arrived: Condvar,
}

#[derive(Debug, Default)]
struct TickState {
    latest: Option<u64>,
    /// When `latest` reached the vsync thread, for a source that stamps it.
    latest_delivered: Option<u64>,
    total: u64,
    pending: Vec<(u64, Option<u64>)>,
    /// Older pending ticks were dropped (more than `PENDING_TICKS` arrived between two drains).
    dropped: bool,
}

impl VsyncTicks {
    /// A new channel, shared between the two threads.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// One vsync, called on the vsync thread. Takes no time worth measuring and cannot panic.
    pub fn tick(&self, tick_ns: u64) {
        self.record(tick_ns, None);
    }

    /// [`Self::tick`] from a source that also says when the tick reached it: `delivered_ns` on
    /// the ticks' clock, read in the callback. The pacer puts it in the trace (ADR-5461).
    pub fn tick_delivered(&self, tick_ns: u64, delivered_ns: u64) {
        self.record(tick_ns, Some(delivered_ns));
    }

    fn record(&self, tick_ns: u64, delivered_ns: Option<u64>) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.latest.is_none_or(|l| tick_ns > l) {
            state.latest_delivered = delivered_ns;
        }
        state.latest = Some(state.latest.map_or(tick_ns, |l| l.max(tick_ns)));
        state.total += 1;
        if state.pending.len() == PENDING_TICKS {
            state.pending.remove(0);
            state.dropped = true;
        }
        state.pending.push((tick_ns, delivered_ns));
        drop(state);
        self.arrived.notify_all();
    }

    /// Hands the ticks since the last call to `feed`.
    pub fn drain(&self, mut feed: impl FnMut(u64)) {
        self.drain_full(|tick, _, _| feed(tick));
    }

    /// Like [`Self::drain`], and also says, for the first tick handed over, whether older ticks
    /// were dropped just before it (more ticks arrived between two drains than the channel
    /// keeps). The gap before that tick then holds vsyncs that did happen; a caller measuring
    /// tick gaps must not count them as skipped.
    pub fn drain_marked(&self, mut feed: impl FnMut(u64, bool)) {
        self.drain_full(|tick, _, after_drop| feed(tick, after_drop));
    }

    /// [`Self::drain`], each tick with the time it reached the vsync thread when its source stamped
    /// it ([`Self::tick_delivered`], ADR-5946).
    pub fn drain_stamped(&self, mut feed: impl FnMut(u64, Option<u64>)) {
        self.drain_full(|tick, delivered, _| feed(tick, delivered));
    }

    /// [`Self::drain_stamped`] and [`Self::drain_marked`] in one: each tick with its delivery stamp
    /// and whether older ticks were dropped just before it.
    pub fn drain_full(&self, mut feed: impl FnMut(u64, Option<u64>, bool)) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut after_drop = std::mem::take(&mut state.dropped);
        for (tick, delivered) in state.pending.drain(..) {
            feed(tick, delivered, after_drop);
            after_drop = false;
        }
    }

    /// Ticks so far.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).total
    }

    /// The newest tick that has arrived (`None`: none yet).
    #[must_use]
    pub fn latest(&self) -> Option<u64> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner).latest
    }

    /// Blocks until a tick at or past `min_tick_ns` has arrived, for at most `timeout`. Returns
    /// that tick, or `None` when the time ran out.
    pub fn wait_for(&self, min_tick_ns: u64, timeout: Duration) -> Option<u64> {
        self.wait_for_delivered(min_tick_ns, timeout)
            .map(|(tick, _)| tick)
    }

    /// [`Self::wait_for`], with the time that tick reached the vsync thread when its source
    /// stamped it ([`Self::tick_delivered`]), read together under one lock (ADR-5461).
    pub fn wait_for_delivered(&self, min_tick_ns: u64, timeout: Duration) -> Option<(u64, Option<u64>)> {
        // A timeout too large for `Instant` (`Duration::MAX`) waits without a deadline; it used
        // to return `None` at once (audit v033b).
        let deadline = Instant::now().checked_add(timeout);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(latest) = state.latest.filter(|l| *l >= min_tick_ns) {
                return Some((latest, state.latest_delivered));
            }
            let left = deadline.map_or(Duration::MAX, |d| d.saturating_duration_since(Instant::now()));
            if left.is_zero() {
                return None;
            }
            state = self
                .arrived
                .wait_timeout(state, left.min(Duration::from_secs(3600)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }
}

/// [`PacingModel`] on live ticks: what the runner calls once per frame.
pub struct DisplayPacer {
    ticks: Arc<VsyncTicks>,
    model: PacingModel,
    source: PacingSource,
    clock: fn() -> u64,
    stalled: u32,
    stalled_at: u64,
    timeouts: u64,
    /// The tick that released the last frame (for the trace).
    last_release: Option<u64>,
    /// Ticks counted into the trace so far.
    traced_ticks: u64,
    /// The tick cadence since the last trace row (ADR-4913).
    cadence: TickCadence,
    /// When the last release tick reached the vsync thread (ADR-5461).
    last_delivered: Option<u64>,
    /// When the last frame came back from its tick wait (ADR-5461).
    last_woke: Option<u64>,
    /// The wake-ahead in effect ([`FrameWakeAhead`]).
    wake_ahead_ns: Option<u64>,
    /// The tick delivery delay, a running mean of the stamped ticks.
    delivery_ns: u64,
    /// Frames released from the wake-ahead poll (`pacing.get`).
    polled: u64,
    /// Frames released on a predicted tick whose callback ran late (`pacing.get`).
    predicted: u64,
    /// Release frames when their tick is due on the grid ([`FrameSteadyStart::grid`]).
    grid: bool,
    /// How long after a tick's own time a frame released on the grid starts ([`next_grid_phase`]).
    grid_phase_ns: u64,
    /// Follow a tick grid the compositor moved ([`PacingModel::set_grid_follow`]).
    grid_follow: bool,
    /// Where a frame released on the grid starts against its tick's due time
    /// ([`FrameSteadyStart::start_offset_ns`]).
    start_offset_ns: i64,
    /// Frames released on the grid (`pacing.get`).
    grid_released: u64,
    /// Reads the calling thread's scheduler counters ([`Self::set_sched_probe`]).
    sched_probe: Option<fn() -> Option<SchedSample>>,
    /// The counters when the last wait returned: the start of the current frame.
    sched_at_start: Option<SchedSample>,
    /// What the last paced frame's row says about its release and the scheduler.
    last_wait: WaitStamps,
}

/// What [`DisplayPacer::pace_frame`] keeps of a wait for [`DisplayPacer::trace_row`].
#[derive(Debug, Clone, Copy, Default)]
struct WaitStamps {
    release: Option<PacingRelease>,
    sleep_late_ns: Option<u64>,
    work_run_delay_ns: Option<u64>,
    wait_run_delay_ns: Option<u64>,
    preemptions: Option<u64>,
    wake_cpu: Option<u32>,
}

/// How a wait for the release tick ended.
struct Waited {
    /// The release tick and its delivery stamp (`None`: the wait timed out).
    released: Option<(u64, Option<u64>)>,
    how: PacingRelease,
    /// How late the timed sleep before the poll ended.
    sleep_late_ns: Option<u64>,
}

impl DisplayPacer {
    /// A pacer on `ticks`; `clock` reads the clock the ticks are stamped with.
    #[must_use]
    pub fn new(ticks: Arc<VsyncTicks>, source: PacingSource, clock: fn() -> u64) -> Self {
        Self {
            ticks,
            model: PacingModel::new(),
            source,
            clock,
            stalled: 0,
            stalled_at: 0,
            timeouts: 0,
            last_release: None,
            traced_ticks: 0,
            cadence: TickCadence::default(),
            last_delivered: None,
            last_woke: None,
            wake_ahead_ns: None,
            delivery_ns: DEFAULT_DELIVERY_NS,
            polled: 0,
            predicted: 0,
            grid: true,
            grid_phase_ns: DEFAULT_DELIVERY_NS,
            grid_follow: true,
            start_offset_ns: 0,
            grid_released: 0,
            sched_probe: None,
            sched_at_start: None,
            last_wait: WaitStamps::default(),
        }
    }

    /// Whether frames are released when their tick is due on the tick grid instead of when its
    /// callback is seen ([`FrameSteadyStart::grid`]; on for a new pacer). Only frames that wait
    /// with a wake-ahead ([`Self::set_wake_ahead`]) are released that way.
    pub fn set_grid_release(&mut self, on: bool) {
        self.grid = on;
    }

    /// Whether the model follows a tick grid the compositor moved while frames are paced with
    /// [`PacingCost::Work`] ([`PacingModel::set_grid_follow`]; on for a new pacer).
    pub fn set_grid_follow(&mut self, on: bool) {
        self.grid_follow = on;
    }

    /// Moves the start of every frame released on the grid by `offset_ns` against its tick's due
    /// time ([`FrameSteadyStart::start_offset_ns`]; 0 for a new pacer), at most
    /// [`MAX_START_OFFSET_NS`] either way.
    pub fn set_start_offset(&mut self, offset_ns: i64) {
        self.start_offset_ns = offset_ns.clamp(-MAX_START_OFFSET_NS, MAX_START_OFFSET_NS);
    }

    /// A function that reads the calling thread's scheduler counters ([`SchedSample`]); the trace
    /// rows then say how long the frame loop waited for a CPU. `None` (a new pacer) reads nothing.
    pub fn set_sched_probe(&mut self, probe: Option<fn() -> Option<SchedSample>>) {
        self.sched_probe = probe;
        self.sched_at_start = None;
    }

    /// Frames so far released on the tick grid ([`FrameSteadyStart`]).
    #[must_use]
    pub fn grid_releases(&self) -> u64 {
        self.grid_released
    }

    /// The first held late start run that drains the display queue ([`PacingModel::set_drain_run`]).
    pub fn set_drain_run(&mut self, run: u32) {
        self.model.set_drain_run(run);
    }

    /// Sets the wake-ahead ([`FrameWakeAhead`]) for the frames paced from now on, capped at
    /// [`MAX_WAKE_AHEAD_NS`].
    pub fn set_wake_ahead(&mut self, lead_ns: Option<u64>) {
        self.wake_ahead_ns = lead_ns.map(|p| p.min(MAX_WAKE_AHEAD_NS)).filter(|p| *p > 0);
    }

    /// Frames so far whose tick the wake-ahead poll caught ([`FrameWakeAhead`]).
    #[must_use]
    pub fn polled_releases(&self) -> u64 {
        self.polled
    }

    /// Frames so far released on a predicted tick because the tick's callback ran late
    /// ([`PREDICTED_RELEASE_MARGIN_NS`]).
    #[must_use]
    pub fn predicted_releases(&self) -> u64 {
        self.predicted
    }

    /// Waits for a tick at or past `bound`, for at most `timeout`: with a wake-ahead, sleeps until
    /// shortly before the tick is due, then polls for it ([`FrameWakeAhead`]). With the grid release
    /// ([`FrameSteadyStart`]) the poll releases the frame at the tick's due time: the tick's time on
    /// the grid plus the mean delivery delay, whether its callback has run or not, and never before
    /// it. Without, a tick whose callback runs more than [`PREDICTED_RELEASE_MARGIN_NS`] past its
    /// usual delivery releases the frame on its predicted time, once per real tick. Returns the
    /// tick and its delivery stamp, and how the frame was released.
    fn wait_for_release(&self, bound: u64, timeout: Duration) -> Waited {
        let slept = |released| Waited {
            released,
            how: PacingRelease::Slept,
            sleep_late_ns: None,
        };
        // How long after its own time the release tick is due at the frame loop.
        let phase = if self.grid {
            self.grid_phase_ns
        } else {
            self.delivery_ns
        };
        let plan = self.wake_ahead_ns.and_then(|lead| {
            let period = self.model.period_ns()?;
            let latest = self.model.last_tick_ns()?;
            let tick = expected_release_tick(latest, period, bound)?;
            // Predict a tick only one slot past the newest real one: when the display stops ticking,
            // the frames stop as they did before.
            let slot = period.saturating_mul(u64::from(self.model.swap_interval().max(1)));
            let may_predict = tick.saturating_sub(latest) <= slot;
            Some((lead, tick, tick.saturating_add(phase), may_predict, slot))
        });
        let Some((lead, tick_due, due, may_predict, slot)) = plan else {
            return slept(self.ticks.wait_for_delivered(bound, timeout));
        };
        // A frame released on the grid starts `start_offset_ns` off its tick's due time.
        let offset = if self.grid { self.start_offset_ns } else { 0 };
        let shifted = |at: u64| at.saturating_add_signed(offset);
        let due = shifted(due);
        let started = Instant::now();
        let now = (self.clock)();
        let sleep_until = due.saturating_sub(lead);
        let sleep = sleep_until.saturating_sub(now);
        // A clock that does not match the ticks (a stand-in in a test) asks for a sleep past the
        // wait limit: wait as without a wake-ahead.
        if Duration::from_nanos(sleep) >= timeout {
            return slept(self.ticks.wait_for_delivered(bound, timeout));
        }
        let mut sleep_late_ns = None;
        if sleep > 0 {
            if let Some(early) = self.ticks.wait_for_delivered(bound, Duration::from_nanos(sleep)) {
                // The tick came before the poll. Without the grid the frame starts on it. On the grid
                // it starts when that tick is due, which a start offset can put well after the tick
                // itself: sleep on to the wake-ahead before that, and let the poll hold the frame.
                let tick_due_at = shifted(early.0.saturating_add(phase));
                let now = (self.clock)();
                if !self.grid || now >= tick_due_at {
                    return slept(Some(early));
                }
                let rest = tick_due_at.saturating_sub(lead).saturating_sub(now);
                if rest > 0 {
                    std::thread::sleep(Duration::from_nanos(rest).min(timeout));
                }
            } else {
                sleep_late_ns = Some((self.clock)().saturating_sub(sleep_until));
            }
        }
        {
            let _span = tracing::trace_span!("pace_wake_ahead_poll").entered();
            // Poll until `lead` past the due time on the ticks' clock. A clock that stands still (a
            // stand-in in a test) never gets there: twice the lead on the wall clock ends that poll.
            // A clock that moves needs no second limit, and a wall-clock one would cut a poll short
            // whenever the thread lost its CPU for that long.
            let poll_until = due.saturating_add(lead);
            let predict_at = due.saturating_add(PREDICTED_RELEASE_MARGIN_NS);
            let wall_limit = Duration::from_nanos(lead.saturating_mul(2));
            let polled = Instant::now();
            let polled_from = now;
            let out = |released, how| Waited {
                released: Some(released),
                how,
                sleep_late_ns,
            };
            loop {
                let seen = self.ticks.wait_for_delivered(bound, Duration::ZERO);
                let now = (self.clock)();
                if self.grid {
                    match seen {
                        // The tick is here: the frame starts when that tick was due, not before.
                        Some((tick, delivered)) if now >= shifted(tick.saturating_add(phase)) => {
                            return out((tick, delivered), PacingRelease::Grid);
                        }
                        // Due, and its callback has not run yet: start on the grid. The grid is
                        // only trusted one slot past the newest real tick (read now: a frame that
                        // starts ahead of its tick ended before the tick of its own slot arrived).
                        None if now >= due
                            && (may_predict
                                || self
                                    .ticks
                                    .latest()
                                    .is_some_and(|latest| tick_due.saturating_sub(latest) <= slot)) =>
                        {
                            return out((tick_due, None), PacingRelease::Grid);
                        }
                        _ => {}
                    }
                } else {
                    if let Some(tick) = seen {
                        return out(tick, PacingRelease::Polled);
                    }
                    if may_predict && now >= predict_at && now < poll_until {
                        return out((tick_due, None), PacingRelease::Predicted);
                    }
                }
                if now >= poll_until || (now == polled_from && polled.elapsed() >= wall_limit) {
                    break;
                }
                std::hint::spin_loop();
                std::thread::yield_now();
            }
        }
        let left = timeout.saturating_sub(started.elapsed());
        Waited {
            released: self.ticks.wait_for_delivered(bound, left),
            how: PacingRelease::Slept,
            sleep_late_ns,
        }
    }

    fn feed(&mut self) {
        let model = &mut self.model;
        let cadence = &mut self.cadence;
        let phase = &mut self.grid_phase_ns;
        self.ticks.drain_full(|tick, delivered, after_drop| {
            if after_drop {
                // The ticks between the last one seen and this one were dropped by the channel,
                // not skipped by the display: do not measure that gap.
                cadence.last = None;
            }
            cadence.note(tick, model.period_ns());
            model.on_tick(tick);
            // A frame released on the grid starts before its tick's callback about half the time,
            // so the release often has no stamp to learn from: every stamped tick counts instead.
            if let (Some(at), Some(period)) = (delivered, model.period_ns()) {
                *phase = next_grid_phase(*phase, at.saturating_sub(tick), period);
            }
        });
    }

    /// Paces the frame that just took `cost`. `cap` is the software cap's period. Returns the time
    /// waited, or `None` when this frame is not paced here and the software cap applies:
    /// [`PacingMode::Off`], no period yet, or a stall. It hints the slot's own rate
    /// ([`PacingHint::Slot`], as v0.2.10 did); [`Self::pace_frame`] takes the settings.
    pub fn pace(&mut self, cost: Duration, cap: Option<Duration>, mode: PacingMode) -> Option<Duration> {
        self.pace_frame(
            cost,
            Duration::ZERO,
            cap,
            FramePacingSettings {
                mode,
                hint: PacingHint::Slot,
                ..Default::default()
            },
        )
    }

    /// [`Self::pace`] for a frame that spent `swapchain_wait` of its `cost` blocked in the
    /// swapchain; `settings.cost` says whether `Auto` counts that blocking ([`PacingCost`]).
    pub fn pace_frame(
        &mut self,
        cost: Duration,
        swapchain_wait: Duration,
        cap: Option<Duration>,
        settings: FramePacingSettings,
    ) -> Option<Duration> {
        let cost = match settings.cost {
            PacingCost::Work => cost.saturating_sub(swapchain_wait),
            PacingCost::Frame => cost,
        };
        self.last_release = None;
        self.last_delivered = None;
        self.last_woke = None;
        self.last_wait = WaitStamps::default();
        let sched_at_end = self.sched_probe.and_then(|probe| probe());
        // A frame that does not wait here starts about now.
        let sched_at_start = std::mem::replace(&mut self.sched_at_start, sched_at_end);
        let mode = settings.mode;
        let _span = tracing::trace_span!("pace_on_display").entered();
        if self.stalled >= STALL_LIMIT && self.ticks.total() != self.stalled_at {
            // Ticks are back. The gap is no slot to compare with.
            self.stalled = 0;
            self.model.restart();
        }
        self.feed();
        let forced = match mode {
            PacingMode::Off => {
                self.skip_frame();
                return None;
            }
            PacingMode::Auto => None,
            PacingMode::Interval(interval) => Some(interval),
        };
        let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        self.model.set_probe(settings.cost == PacingCost::Work);
        self.model.set_late_start(settings.cost == PacingCost::Work);
        self.model.set_queue_drain(settings.cost == PacingCost::Work);
        self.model
            .set_grid_follow(self.grid_follow && settings.cost == PacingCost::Work);
        self.model.set_hint(settings.hint);
        self.model.swapchain_held(match settings.cost {
            PacingCost::Work => nanos(swapchain_wait),
            PacingCost::Frame => 0,
        });
        self.model.end_frame(nanos(cost), cap.map(nanos), forced);
        if self.stalled >= STALL_LIMIT {
            return None;
        }
        let bound = self.model.plan((self.clock)())?;
        let timeout = self.model.wait_limit()?;
        let started = Instant::now();
        let Waited {
            released,
            how,
            sleep_late_ns,
        } = self.wait_for_release(bound, timeout);
        let waited = started.elapsed();
        let woke = (self.clock)();
        let sched_at_wake = self.sched_probe.and_then(|probe| probe());
        self.last_wait = WaitStamps {
            release: released.map(|_| how),
            sleep_late_ns,
            work_run_delay_ns: sched_at_start
                .zip(sched_at_end)
                .map(|(a, b)| b.run_delay_ns.saturating_sub(a.run_delay_ns)),
            wait_run_delay_ns: sched_at_end
                .zip(sched_at_wake)
                .map(|(a, b)| b.run_delay_ns.saturating_sub(a.run_delay_ns)),
            preemptions: sched_at_start
                .or(sched_at_end)
                .zip(sched_at_wake)
                .map(|(a, b)| b.preemptions.saturating_sub(a.preemptions)),
            wake_cpu: sched_at_wake.map(|s| s.cpu),
        };
        self.sched_at_start = sched_at_wake;
        self.feed();
        match released {
            Some((tick, delivered)) => {
                self.stalled = 0;
                self.model.released(tick);
                self.last_release = Some(tick);
                self.last_delivered = delivered;
                self.last_woke = Some(woke);
                self.polled += u64::from(how == PacingRelease::Polled);
                self.predicted += u64::from(how == PacingRelease::Predicted);
                self.grid_released += u64::from(how == PacingRelease::Grid);
                if let (Some(at), Some(period)) = (delivered, self.model.period_ns()) {
                    self.delivery_ns =
                        next_delivery_estimate(self.delivery_ns, at.saturating_sub(tick), period);
                }
            }
            None => {
                self.stalled += 1;
                self.stalled_at = self.ticks.total();
                self.timeouts += 1;
            }
        }
        Some(waited)
    }

    /// A frame this pacer did not release (a hidden window, a bench): the next one has no slot.
    pub fn skip_frame(&mut self) {
        self.last_release = None;
        self.last_delivered = None;
        self.last_woke = None;
        self.last_wait = WaitStamps::default();
        self.sched_at_start = None;
        self.feed();
        self.model.forget_slot();
    }

    /// The [`PacingTraceRow`] of the frame just paced (or skipped): it took `cost`, of which
    /// `swapchain_wait` blocked in the swapchain, and then waited `waited` for a tick. Call once
    /// per frame, after [`Self::pace_frame`] or [`Self::skip_frame`].
    pub fn trace_row(
        &mut self,
        cost: Duration,
        swapchain_wait: Duration,
        waited: Option<Duration>,
    ) -> PacingTraceRow {
        let nanos = |d: Duration| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
        let waited_ns = waited.map_or(0, nanos);
        let now = (self.clock)();
        let total = self.ticks.total();
        let ticks = total.saturating_sub(self.traced_ticks);
        self.traced_ticks = total;
        let (tick_gap_max_ns, missed_ticks) = self.cadence.take();
        PacingTraceRow {
            start_ns: now.saturating_sub(waited_ns).saturating_sub(nanos(cost)),
            cost_ns: nanos(cost),
            swapchain_wait_ns: nanos(swapchain_wait),
            waited_ns,
            release_tick_ns: self.last_release,
            ticks,
            period_ns: self.model.period_ns(),
            interval: self.model.swap_interval(),
            hint_hz: self.model.frame_rate_hint(),
            paced: waited.is_some(),
            tick_gap_max_ns,
            missed_ticks,
            cap_waited_ns: 0,
            tick_delivered_ns: self.last_delivered,
            woke_ns: self.last_woke,
            release: self.last_wait.release,
            sleep_late_ns: self.last_wait.sleep_late_ns,
            work_run_delay_ns: self.last_wait.work_run_delay_ns,
            wait_run_delay_ns: self.last_wait.wait_run_delay_ns,
            preemptions: self.last_wait.preemptions,
            wake_cpu: self.last_wait.wake_cpu,
        }
    }

    /// The app went to the background: the ticks stop until [`Self::resume_ticks`].
    pub fn suspend_ticks(&mut self) {
        self.feed();
        self.stalled = 0;
        self.model.restart();
        self.cadence = TickCadence::default();
    }

    /// The app is back; the first ticks after a gap start a new timeline.
    pub fn resume_ticks(&mut self) {
        self.model.restart();
        self.cadence = TickCadence::default();
    }

    /// The numbers for [`FramePacingReport`]; `mode` says whether frames are paced here.
    #[must_use]
    pub fn report(&self, mode: PacingMode) -> FramePacingReport {
        let paced = mode != PacingMode::Off && self.model.period_ns().is_some() && self.stalled < STALL_LIMIT;
        FramePacingReport {
            source: if paced {
                self.source
            } else {
                PacingSource::Software
            },
            ticks: self.ticks.total(),
            refresh_hz: self.model.period_ns().map(hz_from_ns),
            swap_interval: self.model.swap_interval(),
            stats: self.model.stats(),
            timeouts: self.timeouts,
            frame_rate_hint: None,
        }
    }

    /// The rate to suggest to the compositor ([`PacingModel::frame_rate_hint`]).
    #[must_use]
    pub fn frame_rate_hint(&self) -> Option<f32> {
        self.model.frame_rate_hint()
    }
}

fn hz_from_ns(period_ns: u64) -> f32 {
    (1e9 / period_ns.max(1) as f64) as f32
}

#[cfg(test)]
mod simulate_matches_runner {
    use super::*;

    /// `pace_frame` turns late start on for [`PacingCost::Work`]. [`simulate`] is that runner on
    /// virtual time, so a 53 ms frame forced onto a 50 ms slot (3 vsyncs at 60 Hz) must start on
    /// the slot's tick. The old wait idles until the next vsync and the gap is 66.7 ms.
    #[test]
    fn work_cost_simulation_does_not_idle_a_vsync_after_an_overrun() {
        let report = simulate(&PacingSim {
            refresh_hz: 60.0,
            frame_ms: vec![53.0],
            frames: 40,
            cap_fps: None,
            interval: Some(3),
            refresh_switch: None,
            follow_hint: false,
            swapchain: None,
            cost: PacingCost::Work,
            stretch: 0.0,
            hint: PacingHint::Off,
        })
        .expect("run");
        assert!(
            report.tail_mean_gap_ms < 60.0,
            "work-cost overrun must not slip to the next vsync: {report:?}"
        );
    }
}

/// A run for [`simulate`].
#[derive(Debug, Clone, PartialEq)]
pub struct PacingSim {
    /// The panel's refresh rate.
    pub refresh_hz: f32,
    /// Frame costs, ms, repeated for as long as the run lasts.
    pub frame_ms: Vec<f32>,
    /// Frames to run.
    pub frames: u32,
    /// The software cap, frames per second.
    pub cap_fps: Option<f32>,
    /// A forced swap interval.
    pub interval: Option<u32>,
    /// `(frame, hz)`: the panel changes rate at that frame.
    pub refresh_switch: Option<(u32, f32)>,
    /// The compositor honors the rate hint: the ticks slow to the hinted rate, a whole divisor
    /// of the panel's, and return when the hint clears. Off, it ignores the hint, as older
    /// phones do.
    pub follow_hint: bool,
    /// A stand-in swapchain and compositor (ADR-2320); `None`: acquire never blocks.
    pub swapchain: Option<SwapchainSim>,
    /// What `Auto` counts as a frame's cost.
    pub cost: PacingCost,
    /// How much of the idle time recent frames had a frame's work grows into (0..=0.95; 0: none):
    /// a frame of `w` ms when frames recently started `g` ms apart (a running mean over about 8
    /// frames) costs `w + stretch * (g - w)`. It stands in for a phone whose clocks fall when
    /// paced frames leave it idle and rise when they leave it none (ADR-2339).
    pub stretch: f32,
    /// Which rate the model suggests to the compositor.
    pub hint: PacingHint,
}

/// The swapchain of a [`PacingSim`] run: `images` buffers, a GPU that runs the presented frames
/// one after another, `gpu_ms` each, and a compositor that latches the oldest finished frame
/// `latch_ms` after each tick. The buffer a latched frame replaces stays with the display until
/// the next latch, while it is still scanned out. A frame takes a buffer when it starts and blocks
/// while all of them are queued or held by the display, as a phone's acquire does.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SwapchainSim {
    /// Buffers the app can hold, queued and on screen together (2..=8).
    pub images: u32,
    /// GPU time after the present, ms (0..=10000).
    pub gpu_ms: f32,
    /// The compositor's latch after each tick, ms (0..=1000).
    pub latch_ms: f32,
}

/// The stand-in compositor of [`simulate`].
struct Compositor {
    images: usize,
    gpu: u64,
    /// Time from a tick to its latch.
    latch_after: u64,
    /// Ticks whose latch has not run yet.
    pending: std::collections::VecDeque<u64>,
    /// When each presented frame's GPU work ends, oldest first.
    queue: std::collections::VecDeque<u64>,
    /// When the GPU finishes the last presented frame.
    gpu_free: u64,
    /// Buffers the display holds: the one on screen, and the one it replaced until the next latch.
    on_screen: usize,
    /// A replaced buffer goes back at the next latch.
    retiring: bool,
}

impl Compositor {
    /// Runs every latch up to `now`.
    fn run_until(&mut self, now: u64) {
        while let Some(&tick) = self.pending.front() {
            if tick + self.latch_after > now {
                break;
            }
            self.pending.pop_front();
            self.latch(tick + self.latch_after);
        }
    }

    fn latch(&mut self, at: u64) {
        if std::mem::take(&mut self.retiring) {
            self.on_screen -= 1;
        }
        if self.queue.front().is_some_and(|ready| *ready <= at) {
            self.queue.pop_front();
            self.on_screen += 1;
            self.retiring = self.on_screen > 1;
        }
    }

    /// A frame presented at `at`: the GPU takes it after the frames before it.
    fn present(&mut self, at: u64) {
        self.gpu_free = self.gpu_free.max(at) + self.gpu;
        self.queue.push_back(self.gpu_free);
    }

    fn full(&self) -> bool {
        self.queue.len() + self.on_screen >= self.images
    }
}

/// One swap interval change of a [`PacingSim`] run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IntervalStep {
    /// The frame the interval changed on.
    pub frame: u32,
    /// The new interval.
    pub interval: u32,
}

/// What a [`PacingSim`] run did.
#[derive(Debug, Clone, PartialEq)]
pub struct PacingSimReport {
    /// The final period estimate, ms.
    pub period_ms: f32,
    /// The final swap interval.
    pub swap_interval: u32,
    /// Interval changes, in order.
    pub steps: Vec<IntervalStep>,
    /// The model's counters.
    pub stats: PacingStats,
    /// Over the last quarter of the run: the shortest gap between two frame starts, ms.
    pub tail_min_gap_ms: f32,
    /// Over the last quarter of the run: the longest gap between two frame starts, ms.
    pub tail_max_gap_ms: f32,
    /// Frames that were late over the last quarter of the run.
    pub tail_late: u64,
    /// The rate hint when the run ended.
    pub frame_rate_hint: Option<f32>,
    /// Over the last quarter of the run: the mean gap between two frame starts, ms.
    pub tail_mean_gap_ms: f32,
    /// Over the last quarter of the run: the mean time a frame blocked in the swapchain, ms.
    pub tail_swapchain_wait_ms: f32,
}

/// Frames the gap that [`PacingSim::stretch`] follows is averaged over.
const STRETCH_FRAMES: f64 = 8.0;
/// Ticks the model sees before a simulated run's first frame.
const SIM_WARMUP_TICKS: u32 = 8;
/// The largest frame cost a run accepts, ms.
pub const MAX_SIM_FRAME_MS: f32 = 10_000.0;
/// A run stops being worth its time past this many ticks: the bound on one call, whatever the
/// rates and costs multiply to.
const MAX_SIM_TICKS: u64 = 1_000_000;

/// Runs [`PacingModel`] on virtual time: an even tick train and frames that cost what `sim` says.
/// A cap runs as the runner runs it, limited to [`regia_app::MIN_PACING_FPS`] and
/// [`regia_app::MAX_PACING_FPS`]. `None` when a panel rate is outside what the model takes for a
/// display (5 to 500 Hz), the cap is not a positive number, a cost is non-finite, negative or above
/// [`MAX_SIM_FRAME_MS`], the stretch is outside 0 to [`MAX_SIM_STRETCH`], or the run would take
/// more than a million ticks.
#[must_use]
pub fn simulate(sim: &PacingSim) -> Option<PacingSimReport> {
    let period_of = |hz: f32| {
        let period = (hz.is_finite() && hz > 0.0).then(|| (1e9 / f64::from(hz)) as u64)?;
        (MIN_TICK_NS..=MAX_TICK_NS).contains(&period).then_some(period)
    };
    let mut panel = period_of(sim.refresh_hz)?;
    let mut period = panel;
    let cap = match sim.cap_fps {
        Some(fps) if fps.is_finite() && fps > 0.0 => {
            let fps = fps.clamp(regia_app::MIN_PACING_FPS, regia_app::MAX_PACING_FPS);
            Some((1e9 / f64::from(fps)) as u64)
        }
        Some(_) => return None,
        None => None,
    };
    let costs: Vec<u64> = sim
        .frame_ms
        .iter()
        .map(|ms| {
            (ms.is_finite() && (0.0..=MAX_SIM_FRAME_MS).contains(ms)).then(|| (f64::from(*ms) * 1e6) as u64)
        })
        .collect::<Option<_>>()?;
    if costs.is_empty() {
        return None;
    }
    let mut compositor = match sim.swapchain {
        None => None,
        Some(chain) => {
            let in_range = |v: f32, max: f32| v.is_finite() && (0.0..=max).contains(&v);
            if !(2..=MAX_SWAP_INTERVAL).contains(&chain.images)
                || !in_range(chain.gpu_ms, MAX_SIM_FRAME_MS)
                || !in_range(chain.latch_ms, 1000.0)
            {
                return None;
            }
            Some(Compositor {
                images: chain.images as usize,
                gpu: (f64::from(chain.gpu_ms) * 1e6) as u64,
                latch_after: (f64::from(chain.latch_ms) * 1e6) as u64,
                pending: std::collections::VecDeque::new(),
                queue: std::collections::VecDeque::new(),
                gpu_free: 0,
                on_screen: 0,
                retiring: false,
            })
        }
    };
    if !(sim.stretch.is_finite() && (0.0..=MAX_SIM_STRETCH).contains(&sim.stretch)) {
        return None;
    }
    let mut model = PacingModel::new();
    model.set_probe(sim.cost == PacingCost::Work);
    // The runner turns late start on with Work cost ([`FramePacer::pace_frame`], ADR-4913); the
    // simulation must match it or an over-budget frame idles to the next vsync on the sim only.
    model.set_late_start(sim.cost == PacingCost::Work);
    model.set_hint(sim.hint);
    let mut next_tick = period;
    for _ in 0..SIM_WARMUP_TICKS {
        model.on_tick(next_tick);
        next_tick += period;
    }
    // A tick reaches the model and, `latch` later, the compositor.
    fn tick_now(model: &mut PacingModel, compositor: &mut Option<Compositor>, tick: u64) {
        model.on_tick(tick);
        if let Some(c) = compositor {
            c.pending.push_back(tick);
        }
    }
    let mut tail_wait = 0_u64;
    let tail_from = sim.frames - sim.frames / 4;
    let mut clock = next_tick - period;
    let mut starts = Vec::new();
    let mut steps = Vec::new();
    let mut tail_late_start = 0;
    let mut tail_late_seen = false;
    let first_tick = next_tick;
    // The mean gap between frame starts over about the last `STRETCH_FRAMES` frames.
    let mut recent_gap = period as f64;
    for frame in 0..sim.frames {
        if (next_tick - first_tick) / panel.max(1) > MAX_SIM_TICKS {
            return None;
        }
        if let Some((at, hz)) = sim.refresh_switch
            && at == frame
        {
            panel = period_of(hz)?;
            period = panel;
        }
        if frame >= tail_from {
            if !tail_late_seen {
                tail_late_start = model.stats().late;
                tail_late_seen = true;
            }
            starts.push(clock);
        }
        let base = costs[frame as usize % costs.len()];
        let work = base + (f64::from(sim.stretch) * (recent_gap - base as f64).max(0.0)) as u64;
        // Acquire: wait for the latch that frees a buffer.
        let mut acquired = clock;
        if let Some(c) = &mut compositor {
            c.run_until(acquired);
            while c.full() {
                if c.pending.is_empty() {
                    // The vsync thread keeps ticking while the frame blocks.
                    model.on_tick(next_tick);
                    c.pending.push_back(next_tick);
                    next_tick += period;
                }
                let Some(tick) = c.pending.pop_front() else {
                    break;
                };
                acquired = acquired.max(tick + c.latch_after);
                c.latch(tick + c.latch_after);
                if (next_tick - first_tick) / panel.max(1) > MAX_SIM_TICKS {
                    return None;
                }
            }
        }
        let blocked = acquired - clock;
        let end = acquired + work;
        if let Some(c) = &mut compositor {
            c.present(end);
        }
        if frame >= tail_from {
            tail_wait += blocked;
        }
        while next_tick <= end {
            tick_now(&mut model, &mut compositor, next_tick);
            next_tick += period;
        }
        let (cost, held) = match sim.cost {
            PacingCost::Work => (work, blocked),
            PacingCost::Frame => (blocked + work, 0),
        };
        model.swapchain_held(held);
        let before = model.swap_interval();
        model.end_frame(cost, cap, sim.interval);
        if model.swap_interval() != before {
            steps.push(IntervalStep {
                frame,
                interval: model.swap_interval(),
            });
        }
        if sim.follow_hint {
            period = model
                .frame_rate_hint()
                .map_or(panel, |hz| ticks_following(panel, hz));
        }
        let bound = model.plan(end)?;
        let mut tick = next_tick.saturating_sub(period);
        while tick < bound {
            tick = next_tick;
            tick_now(&mut model, &mut compositor, tick);
            next_tick += period;
        }
        model.released(tick);
        let next = end.max(tick);
        recent_gap += ((next - clock) as f64 - recent_gap) / STRETCH_FRAMES;
        clock = next;
    }
    let ms = |ns: u64| (ns as f64 / 1e6) as f32;
    let gaps = starts.windows(2).map(|w| w[1] - w[0]);
    let tail_frames = starts.len().max(1) as u64;
    let tail_mean_gap = match (starts.first(), starts.last()) {
        (Some(first), Some(last)) if starts.len() > 1 => (last - first) / (starts.len() as u64 - 1),
        _ => 0,
    };
    Some(PacingSimReport {
        period_ms: ms(model.period_ns()?),
        swap_interval: model.swap_interval(),
        steps,
        stats: model.stats(),
        tail_min_gap_ms: ms(gaps.clone().min().unwrap_or(0)),
        tail_max_gap_ms: ms(gaps.max().unwrap_or(0)),
        tail_late: model.stats().late - tail_late_start,
        frame_rate_hint: model.frame_rate_hint(),
        tail_mean_gap_ms: ms(tail_mean_gap),
        tail_swapchain_wait_ms: ms(tail_wait / tail_frames),
    })
}

/// The tick period of a panel of `panel_ns` that honors a hint of `hz`: a whole divisor of its rate.
fn ticks_following(panel_ns: u64, hz: f32) -> u64 {
    let divisor = (1e9 / (panel_ns as f64 * f64::from(hz))).round().max(1.0) as u64;
    panel_ns.saturating_mul(divisor).min(MAX_TICK_NS)
}
