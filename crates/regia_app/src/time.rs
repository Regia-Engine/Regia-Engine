// SPDX-License-Identifier: MPL-2.0
//! Time resources (`docs/specs/10-core.md` § 3.2).

use std::time::{Duration, Instant};

use regia_ecs::prelude::*;

/// Longest wall-clock frame delta fed to gameplay (`RealTime::tick`). Longer gaps (debugger pauses,
/// OS stalls) are clamped. A `TimeUpdateStrategy::ManualDuration` is not clamped.
pub const MAX_DELTA: Duration = Duration::from_millis(250);

/// A gap between two frames at least this long is a discontinuity (a laptop lid closed, a
/// hibernate, a long suspend: Windows' `Instant` counts standby), not a frame: it is treated like
/// [`RealTime::reset_clock`]. Shorter stalls (a 4.7 s TDR) stay frames, clamped for gameplay.
pub const DISCONTINUITY_GAP: Duration = Duration::from_secs(30);

/// Largest gameplay [`Time::time_scale`] honored; larger (or infinite) values saturate here.
pub const MAX_TIME_SCALE: f32 = 1000.0;
/// Highest fixed-step rate honored by [`FixedTime::step`] (one step is then 1 ns, never zero).
pub const MAX_FIXED_HZ: f64 = 1.0e9;
/// Largest [`FixedTime::max_substeps`] honored per frame.
pub const MAX_SUBSTEPS_LIMIT: u32 = 64;
/// Lowest frame-rate cap honored by [`FramePacing`] (one frame per second).
pub const MIN_PACING_FPS: f32 = 1.0;
/// Highest frame-rate cap honored by [`FramePacing`].
pub const MAX_PACING_FPS: f32 = 10_000.0;

/// Fixed-step rate made safe for `Duration` math: non-finite becomes 60, the rest is clamped to
/// `1..=`[`MAX_FIXED_HZ`] (`f64::clamp` passes NaN through, so it is handled first).
pub(crate) fn sanitize_hz(hz: f64) -> f64 {
    if hz.is_finite() {
        hz.clamp(1.0, MAX_FIXED_HZ)
    } else {
        60.0
    }
}

/// Unscaled wall-clock time, for UI and the editor.
#[derive(Resource, Debug, Clone, Default)]
pub struct RealTime {
    delta: Duration,
    raw_delta: Duration,
    elapsed: Duration,
    last: Option<Instant>,
}

impl RealTime {
    /// Wall-clock duration of the last frame (clamped to [`MAX_DELTA`] when read from the wall
    /// clock; a manual duration is stored whole).
    pub fn delta(&self) -> Duration {
        self.delta
    }
    /// Wall-clock duration of the last frame, not clamped: what a frame-time report needs (a
    /// 4 s main-loop stall is 4 s here and [`MAX_DELTA`] in [`Self::delta`]). Zero after
    /// [`Self::reset_clock`], like [`Self::delta`].
    pub fn raw_delta(&self) -> Duration {
        self.raw_delta
    }
    /// Sum of all deltas.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
    /// Forgets the last frame instant, so the next frame has a zero delta. Called after a suspend.
    pub fn reset_clock(&mut self) {
        self.last = None;
    }
    /// Advances by exactly `delta` (manual time, [`TimeUpdateStrategy::ManualDuration`]). Not
    /// clamped; the next automatic [`RealTime::tick`] starts from a zero delta.
    pub fn advance_by(&mut self, delta: Duration) {
        self.delta = delta;
        self.raw_delta = delta;
        self.elapsed = self.elapsed.saturating_add(delta);
        self.last = None;
    }
    /// Advances to `now`.
    pub fn tick(&mut self, now: Instant) {
        self.raw_delta = self
            .last
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        if self.raw_delta >= DISCONTINUITY_GAP {
            self.raw_delta = Duration::ZERO;
        }
        self.delta = self.raw_delta.min(MAX_DELTA);
        self.elapsed = self.elapsed.saturating_add(self.delta);
        self.last = Some(now);
    }
}

/// Gameplay time: scaled and pausable. It does not advance while `paused` or `suspended`
/// is set. `paused` changes only when the game sets it. Suspend ([`Time::set_suspended`])
/// does not write `paused`.
#[derive(Resource, Debug, Clone)]
pub struct Time {
    delta: Duration,
    elapsed: Duration,
    frame_count: u64,
    /// Gameplay speed multiplier (slow motion). UI uses [`RealTime`].
    pub time_scale: f32,
    /// Gameplay pause set by the game.
    pub paused: bool,
    suspended: bool,
    /// The simulation clock's delta: `delta`, unless a [`SimStep`] sets it apart (ADR-1524).
    sim_delta: Duration,
    /// Sum of the simulation deltas.
    sim_elapsed: Duration,
}

impl Default for Time {
    fn default() -> Self {
        Self {
            delta: Duration::ZERO,
            elapsed: Duration::ZERO,
            frame_count: 0,
            time_scale: 1.0,
            paused: false,
            suspended: false,
            sim_delta: Duration::ZERO,
            sim_elapsed: Duration::ZERO,
        }
    }
}

/// What `FixedUpdate` replaces in [`Time`] while it runs, so the frame's view can be put back.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameView {
    delta: Duration,
    elapsed: Duration,
    sim_delta: Duration,
    sim_elapsed: Duration,
}

impl Time {
    /// Gameplay delta of the current frame (zero while paused).
    pub fn delta(&self) -> Duration {
        self.delta
    }
    /// Gameplay delta in seconds.
    pub fn delta_secs(&self) -> f32 {
        self.delta.as_secs_f32()
    }
    /// Total gameplay time.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }
    /// Total gameplay time in seconds.
    pub fn elapsed_secs(&self) -> f32 {
        self.elapsed.as_secs_f32()
    }
    /// Frames run since startup (counts paused frames too).
    pub fn frame_count(&self) -> u64 {
        self.frame_count
    }
    /// True while the app is suspended by the OS (set by the platform layer).
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }
    /// Marks the app suspended or resumed. Gameplay time does not advance while suspended.
    pub fn set_suspended(&mut self, suspended: bool) {
        self.suspended = suspended;
    }
    /// Advances by one frame whose real duration was `real_delta`.
    pub fn advance(&mut self, real_delta: Duration) {
        self.frame_count += 1;
        self.delta = if self.paused || self.suspended {
            Duration::ZERO
        } else {
            // NaN and negative scales freeze time; +inf and huge scales saturate. `0 * inf` and
            // overflowing products would otherwise panic inside `Duration`.
            let scale = if self.time_scale.is_nan() {
                0.0
            } else {
                self.time_scale.clamp(0.0, MAX_TIME_SCALE)
            };
            Duration::try_from_secs_f64(real_delta.as_secs_f64() * f64::from(scale)).unwrap_or(Duration::MAX)
        };
        self.elapsed = self.elapsed.saturating_add(self.delta);
        self.sim_delta = self.delta;
        self.sim_elapsed = self.sim_elapsed.saturating_add(self.delta);
    }

    /// Moves both clocks to `elapsed` and `sim_elapsed`; the deltas and the frame count stay. For a
    /// caller that runs a stretch of time from its own zero and hands the clocks back after it (a
    /// render-out job, ADR-1881). Call it between frames' fixed loops, not from `FixedUpdate`.
    pub fn set_clocks(&mut self, elapsed: Duration, sim_elapsed: Duration) {
        self.elapsed = elapsed;
        self.sim_elapsed = sim_elapsed;
    }

    /// Simulation delta of the current frame. It is [`Self::delta`] unless a [`SimStep`] sets the
    /// simulation clock apart (a render-out `time_map`, ADR-1524): then it is that step (zero while
    /// paused or suspended), while [`Self::delta`] keeps the presentation step. The fixed loop
    /// (`FixedUpdate`: physics, gameplay) accumulates it, and a sim bridge passes it to
    /// `read_snapshot`.
    pub fn sim_delta(&self) -> Duration {
        self.sim_delta
    }

    /// Total simulation time (the sum of [`Self::sim_delta`]). It equals [`Self::elapsed`] until a
    /// [`SimStep`] first differs from the presentation step.
    pub fn sim_elapsed(&self) -> Duration {
        self.sim_elapsed
    }

    /// Replaces this frame's simulation delta by `step` (zero while paused or suspended), after
    /// [`Self::advance`].
    pub(crate) fn apply_sim_step(&mut self, step: Duration) {
        let step = if self.paused || self.suspended {
            Duration::ZERO
        } else {
            step
        };
        self.sim_elapsed = self
            .sim_elapsed
            .saturating_sub(self.sim_delta)
            .saturating_add(step);
        self.sim_delta = step;
    }

    /// The frame's deltas and clocks, for [`Self::restore_frame_view`].
    pub(crate) fn frame_view(&self) -> FrameView {
        FrameView {
            delta: self.delta,
            elapsed: self.elapsed,
            sim_delta: self.sim_delta,
            sim_elapsed: self.sim_elapsed,
        }
    }

    /// Shows `FixedUpdate` systems one fixed step as the gameplay (and simulation) delta, with
    /// `elapsed` as the simulation clock at the end of that step. Pair with
    /// [`Time::restore_frame_view`].
    pub(crate) fn set_fixed_view(&mut self, step: Duration, elapsed: Duration) {
        self.delta = step;
        self.elapsed = elapsed;
        self.sim_delta = step;
        self.sim_elapsed = elapsed;
    }

    /// Puts back the frame view saved before the fixed steps.
    pub(crate) fn restore_frame_view(&mut self, view: FrameView) {
        self.delta = view.delta;
        self.elapsed = view.elapsed;
        self.sim_delta = view.sim_delta;
        self.sim_elapsed = view.sim_elapsed;
    }
}

/// The simulation clock's step, set apart from the presentation clock (two clocks, ADR-1524).
///
/// Without this resource the simulation follows the presentation: [`Time::sim_delta`] is
/// [`Time::delta`]. With it, every frame's [`Time::sim_delta`] is exactly this duration (zero while
/// paused or suspended; [`Time::time_scale`] does not apply to it) and [`Time::delta`] keeps the
/// presentation step. The fixed loop accumulates the simulation step, so `FixedUpdate` (physics,
/// gameplay) and sim bridges (`read_snapshot`, a replay player) run on it; `Update` systems that read
/// [`Time::delta`] (VFX, shot players, UI tweens, the material clock) and the render-out shutter
/// stay on the presentation step. A render-out with a `time_map` sets it per frame (slow motion)
/// and removes it when the job ends.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq)]
pub struct SimStep(pub Duration);

/// Fixed-step accumulator (`FixedUpdate`).
#[derive(Resource, Debug, Clone)]
pub struct FixedTime {
    /// Steps per second (default 60).
    pub hz: f64,
    /// Most steps run in one frame (spiral-of-death guard).
    pub max_substeps: u32,
    accumulator: Duration,
    /// The (sanitized) rate the bank was accumulated at; a different rate drops the bank.
    banked_hz: f64,
    /// The simulation clock at the end of the last step actually run: the next frame's steps
    /// start here. Time the substep cap discarded is never added back.
    last_step_end: Duration,
    /// The frame's simulation clock (`frame_elapsed`) the last time the fixed loop ran. The next
    /// frame's simulation start (`frame_elapsed - delta`) equals it unless `Time::set_clocks`
    /// moved the clock between frames (a render-out job restarting or handing the clocks back,
    /// ADR-1881); a mismatch re-anchors the step clock instead of continuing from the stale
    /// [`FixedTime::last_step_end`], which after a forward jump would lag the frame clock by the
    /// jump for the rest of the session.
    last_frame_clock: Duration,
}

impl Default for FixedTime {
    fn default() -> Self {
        Self {
            hz: 60.0,
            max_substeps: 5,
            accumulator: Duration::ZERO,
            banked_hz: 60.0,
            last_step_end: Duration::ZERO,
            last_frame_clock: Duration::ZERO,
        }
    }
}

impl FixedTime {
    /// Length of one step (`hz` clamped to 1..=1e9 so the step is at least 1 ns; a non-finite
    /// `hz` counts as 60).
    pub fn step(&self) -> Duration {
        Duration::from_secs_f64(1.0 / sanitize_hz(self.hz))
    }
    /// Adds `delta` and returns how many steps to run now. Excess time beyond `max_substeps` is dropped.
    pub fn accumulate(&mut self, delta: Duration) -> u32 {
        // Store the rate that is honored, and drop time banked at another step length.
        let hz = sanitize_hz(self.hz);
        self.hz = hz;
        if hz != self.banked_hz {
            self.accumulator = Duration::ZERO;
            self.banked_hz = hz;
        }
        let step = self.step();
        // `delta` saturates instead of overflowing; the excess is dropped below anyway.
        self.accumulator = self.accumulator.saturating_add(delta);
        let max_substeps = self.max_substeps.min(MAX_SUBSTEPS_LIMIT);
        let mut steps = 0;
        while self.accumulator >= step && steps < max_substeps {
            self.accumulator -= step;
            steps += 1;
        }
        if steps == max_substeps && self.accumulator >= step {
            self.accumulator = Duration::ZERO;
        }
        steps
    }
    /// Drops any banked time (the step length changed, or the clock was reset).
    pub fn clear_accumulator(&mut self) {
        self.accumulator = Duration::ZERO;
    }
    /// The simulation clock at the end of the last step actually run. The frame loop starts the
    /// next frame's steps here, so time the substep cap discarded is not added back later.
    pub(crate) fn last_step_end(&self) -> Duration {
        self.last_step_end
    }
    /// Records where the last run step ended (the frame loop calls this after its steps).
    pub(crate) fn set_last_step_end(&mut self, end: Duration) {
        self.last_step_end = end;
    }
    /// The frame's simulation clock the fixed loop last ran against; see the field.
    pub(crate) fn last_frame_clock(&self) -> Duration {
        self.last_frame_clock
    }
    /// Records the frame's simulation clock the fixed loop ran against (the frame loop calls
    /// this every frame, also when no step ran, so a `set_clocks` on a zero-step frame is still
    /// detected by the next one).
    pub(crate) fn set_last_frame_clock(&mut self, clock: Duration) {
        self.last_frame_clock = clock;
    }
    /// Time left in the accumulator: the sub-step phase a re-anchored clock keeps. A bank made
    /// at another rate is dropped by [`FixedTime::accumulate`] the frame it is read, so it does
    /// not count here either (the [`FixedTime::would_run`] / [`FixedTime::overstep_fraction`]
    /// rule).
    pub(crate) fn banked(&self) -> Duration {
        if sanitize_hz(self.hz) != self.banked_hz {
            Duration::ZERO
        } else {
            self.accumulator
        }
    }
    /// Whether [`FixedTime::accumulate`] with `delta` would run at least one fixed step this
    /// frame (without consuming anything). Lets a `PreUpdate` system predict a zero-step frame,
    /// whose action evaluation must then run in `PreUpdate` instead of `FixedUpdate`.
    pub fn would_run(&self, delta: Duration) -> bool {
        let max_substeps = self.max_substeps.min(MAX_SUBSTEPS_LIMIT);
        if max_substeps == 0 {
            return false;
        }
        // `accumulate` drops a bank accumulated at another rate (the `hz` changed), so the same
        // bank must not count towards the prediction either: an `hz` edit between frames is
        // honoured the frame it is read, not one frame late (fixrev review, finding 6 — the old
        // prediction kept the stale bank, skipped the zero-step-frame fallback for a frame that
        // then ran no step, and that frame evaluated its actions nowhere).
        let banked = if sanitize_hz(self.hz) != self.banked_hz {
            Duration::ZERO
        } else {
            self.accumulator
        };
        banked.saturating_add(delta) >= self.step()
    }
    /// Fraction of a step left in the accumulator, for interpolating rendered transforms.
    pub fn overstep_fraction(&self) -> f32 {
        // A bank accumulated at another rate (a direct `hz` write is honored by the next
        // `accumulate`) is not a fraction of the current step.
        if sanitize_hz(self.hz) != self.banked_hz {
            return 0.0;
        }
        let step = self.step().as_secs_f64();
        if step > 0.0 {
            (self.accumulator.as_secs_f64() / step).min(1.0) as f32
        } else {
            0.0
        }
    }
}

/// Configures deterministic fixed-timestep stepping for headless / agent runs.
///
/// Sets [`TimeUpdateStrategy::ManualDuration`] to one step at `hz` (`hz` sanitized to
/// `1..=`[`MAX_FIXED_HZ`]; `0` becomes 1 Hz and a non-finite value becomes 60) and pins
/// [`FixedTime::max_substeps`] to 1. One [`crate::App::update`] runs one `FixedUpdate` when
/// the simulation delta is at least that step: `time_scale` is 1, no shorter [`SimStep`] is
/// installed, and the app is not paused or suspended. [`Time::advance`] multiplies the manual
/// delta by `time_scale`, so a scale below 1 banks time and a step runs only once the bank
/// reaches `1/hz`. Scale 0.5 at 1 Hz runs a step on every second update (a half step is an exact
/// number of nanoseconds). Scale 0 runs none. At 60 Hz the stored step is 16_666_667 ns and half
/// of that frame is 8_333_333 ns, so the first step lands on the third update. A scale above 1
/// still runs at most one step and drops the rest. Pause and suspend are a
/// zero gameplay delta, so no `FixedUpdate` runs. Used by offscreen captures so two lavapipe
/// runs produce byte-identical screenshots.
pub fn enable_fixed_step(app: &mut crate::App, hz: f64) {
    let hz = sanitize_hz(hz);
    let step = Duration::from_secs_f64(1.0 / hz);
    app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
    app.insert_resource(FixedTime {
        hz,
        max_substeps: 1,
        accumulator: Duration::ZERO,
        banked_hz: hz,
        last_step_end: Duration::ZERO,
        last_frame_clock: Duration::ZERO,
    });
}

/// How the clocks advance each frame.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TimeUpdateStrategy {
    /// From the wall clock (clamped to [`MAX_DELTA`]).
    #[default]
    Automatic,
    /// By exactly this duration per frame, regardless of the wall clock: headless tests, agent
    /// frame stepping and deterministic captures.
    ManualDuration(Duration),
}

/// `First` system: advances the clocks.
pub(crate) fn update_time(
    strategy: Option<Res<TimeUpdateStrategy>>,
    sim_step: Option<Res<SimStep>>,
    real: Option<ResMut<RealTime>>,
    time: Option<ResMut<Time>>,
    mut commands: Commands,
) {
    // A removed clock is re-created (as `update_loop_control` does) instead of aborting the frame.
    let (Some(mut real), Some(mut time)) = (real, time) else {
        commands.init_resource::<RealTime>();
        commands.init_resource::<Time>();
        return;
    };
    match strategy.map(|s| *s).unwrap_or_default() {
        TimeUpdateStrategy::Automatic => real.tick(Instant::now()),
        TimeUpdateStrategy::ManualDuration(delta) => real.advance_by(delta),
    }
    time.advance(real.delta());
    if let Some(step) = sim_step {
        time.apply_sim_step(step.0);
    }
}

/// The final stretch of a frame wait that is spun instead of slept (OS sleep overshoots by up to a
/// scheduler tick).
pub const SPIN_THRESHOLD: Duration = Duration::from_millis(1);

/// Optional frame-rate cap (`docs/specs/10-core.md` § 3.2). Runners call [`FramePacing::wait`]
/// (through [`crate::App::pace_frame`]) once per frame, after the frame.
///
/// Two caps compose here (ADR-7889): [`FramePacing::target_fps`], the player's cap (the settings
/// apply writes `display.frame_cap` into it), and the state cap, which the app copies each frame
/// (in `Last`) from [`crate::PacingCaps`] for the current [`crate::PacingState`]. The tighter one
/// paces ([`FramePacing::effective_fps`]), so a settings apply never removes a menu cap.
#[derive(Resource, Debug, Clone, Default)]
pub struct FramePacing {
    /// The player's cap in frames per second; `None` (default) runs uncapped (or at vsync).
    pub target_fps: Option<f32>,
    /// The current state's cap ([`crate::PacingCaps`]), refreshed by the app each frame.
    state_fps: Option<f32>,
    last: Option<Instant>,
}

/// A cap that can pace: finite and positive, clamped to [`MIN_PACING_FPS`]`..=`[`MAX_PACING_FPS`].
fn valid_cap(fps: Option<f32>) -> Option<f32> {
    fps.filter(|fps| fps.is_finite() && *fps > 0.0)
        .map(|fps| fps.clamp(MIN_PACING_FPS, MAX_PACING_FPS))
}

/// The tighter (lower) of two caps; an unset or invalid cap does not limit.
pub fn tighter_cap(a: Option<f32>, b: Option<f32>) -> Option<f32> {
    match (valid_cap(a), valid_cap(b)) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// Where [`FramePacing`] reads the time and how it waits. [`SystemPacingClock`] is the real
/// clock; tests and simulations pass their own to [`FramePacing::wait_for_with`] and
/// [`crate::App::pace_frame_with`] so a wait is computed without sleeping.
pub trait PacingClock {
    /// The current instant.
    fn now(&mut self) -> Instant;
    /// Returns at (or after) `deadline`.
    fn sleep_until(&mut self, deadline: Instant);
}

/// The wall clock: [`Instant::now`] and [`sleep_until`].
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemPacingClock;

impl PacingClock for SystemPacingClock {
    fn now(&mut self) -> Instant {
        Instant::now()
    }

    fn sleep_until(&mut self, deadline: Instant) {
        sleep_until(deadline);
    }
}

impl FramePacing {
    /// Pacing with the given cap.
    pub fn new(target_fps: Option<f32>) -> Self {
        Self {
            target_fps,
            state_fps: None,
            last: None,
        }
    }

    /// The current state's cap as the app last copied it from [`crate::PacingCaps`] (`None`:
    /// the state has no cap).
    pub fn state_fps(&self) -> Option<f32> {
        self.state_fps
    }

    /// Sets the state cap. The app calls this each frame from [`crate::PacingCaps`] and
    /// [`crate::PacingState`]; a host that drives pacing without an [`crate::App`] may call it.
    pub fn set_state_fps(&mut self, fps: Option<f32>) {
        self.state_fps = fps;
    }

    /// The cap that paces: the tighter of [`FramePacing::target_fps`] and
    /// [`FramePacing::state_fps`], clamped to [`MIN_PACING_FPS`]`..=`[`MAX_PACING_FPS`].
    pub fn effective_fps(&self) -> Option<f32> {
        tighter_cap(self.target_fps, self.state_fps)
    }

    /// The frame period of [`FramePacing::effective_fps`], if a valid cap is set.
    pub fn period(&self) -> Option<Duration> {
        self.effective_fps()
            .map(|fps| Duration::from_secs_f64(1.0 / f64::from(fps)))
    }

    /// Blocks until the next frame is due (sleep, then spin for the last [`SPIN_THRESHOLD`]).
    /// Deadlines advance by whole periods so the average rate does not drift; a frame more than a
    /// period late resynchronises instead of rushing to catch up. Returns how long it waited.
    pub fn wait(&mut self) -> Duration {
        self.wait_for(self.period())
    }

    /// [`FramePacing::wait`] with an explicit period (`None` resets and returns at once); the loop
    /// modes use it to override the cap ([`crate::LoopControl`]).
    pub fn wait_for(&mut self, period: Option<Duration>) -> Duration {
        self.wait_for_with(period, &mut SystemPacingClock)
    }

    /// [`FramePacing::wait_for`] on `clock`: the same deadline chain, with the time read from and
    /// the wait spent on `clock` (a test clock makes the wait deterministic).
    pub fn wait_for_with(&mut self, period: Option<Duration>, clock: &mut impl PacingClock) -> Duration {
        let Some(period) = period else {
            self.last = None;
            return Duration::ZERO;
        };
        let now = clock.now();
        let Some(last) = self.last else {
            self.last = Some(now);
            return Duration::ZERO;
        };
        let Some(deadline) = last.checked_add(period) else {
            self.last = Some(now);
            return Duration::ZERO;
        };
        if deadline > now {
            clock.sleep_until(deadline);
            // The deadline stays the base (no drift), but the charge is the measured wait: an OS
            // wake that overshoots the cap shows up as pacing time, not as "outside update".
            let woke = clock.now();
            self.last = Some(next_base(deadline, woke, period));
            woke.saturating_duration_since(now)
        } else {
            self.last = Some(if now - deadline > period { now } else { deadline });
            Duration::ZERO
        }
    }
}

/// The base of the next deadline after a wait that ended at `woke`: the deadline itself, unless
/// the wake was more than a period late (then resynchronise to `woke`).
fn next_base(deadline: Instant, woke: Instant, period: Duration) -> Instant {
    if woke.saturating_duration_since(deadline) > period {
        woke
    } else {
        deadline
    }
}

/// Sleeps until `deadline`, spinning for the last [`SPIN_THRESHOLD`] for accuracy.
pub fn sleep_until(deadline: Instant) {
    loop {
        let now = Instant::now();
        if now >= deadline {
            return;
        }
        let remaining = deadline - now;
        if remaining > SPIN_THRESHOLD {
            std::thread::sleep(remaining - SPIN_THRESHOLD);
        } else {
            std::hint::spin_loop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS16: Duration = Duration::from_millis(16);

    #[test]
    fn set_clocks_moves_the_clocks_and_nothing_else() {
        let mut t = Time::default();
        t.advance(MS16);
        t.advance(MS16);
        t.set_clocks(Duration::from_secs(3), Duration::from_secs(1));
        assert_eq!(t.elapsed(), Duration::from_secs(3));
        assert_eq!(t.sim_elapsed(), Duration::from_secs(1));
        assert_eq!((t.delta(), t.frame_count()), (MS16, 2));
        t.advance(MS16);
        assert_eq!(t.elapsed(), Duration::from_secs(3) + MS16);
        assert_eq!(t.sim_elapsed(), Duration::from_secs(1) + MS16);
    }

    #[test]
    fn pause_and_suspend_freeze_gameplay_time() {
        let mut t = Time::default();
        t.advance(MS16);
        assert_eq!(t.elapsed(), MS16);
        t.paused = true;
        t.advance(MS16);
        assert_eq!(t.elapsed(), MS16);
        assert_eq!(t.delta(), Duration::ZERO);
        t.paused = false;
        t.set_suspended(true);
        t.advance(MS16);
        assert_eq!(t.elapsed(), MS16);
        assert_eq!(t.frame_count(), 3);
    }

    #[test]
    fn would_run_predicts_zero_step_frames() {
        let mut fixed = FixedTime::default(); // 60 Hz, max 5 substeps
        let step = fixed.step();
        assert!(!fixed.would_run(Duration::ZERO), "nothing banked, no delta");
        assert!(
            !fixed.would_run(step - Duration::from_nanos(1)),
            "just below one step"
        );
        assert!(fixed.would_run(step), "exactly one step crosses");
        // Half a step banks; the remainder of the step crosses the threshold.
        let short = step / 2;
        let rest = step - short;
        assert!(!fixed.would_run(short));
        fixed.accumulate(short);
        assert!(fixed.would_run(rest), "banked + delta crosses the step");
        // A zero-substeps configuration never runs.
        let none = FixedTime {
            max_substeps: 0,
            ..FixedTime::default()
        };
        assert!(!none.would_run(Duration::from_secs(1)));
    }

    /// `accumulate` drops time banked at another rate. On the frame the game changes `hz`,
    /// `would_run` must agree with it, or the input `PreUpdate` fallback skips a frame whose
    /// fixed loop then runs no step (that frame's actions are never evaluated).
    #[test]
    fn would_run_agrees_with_accumulate_when_the_rate_changes() {
        let mut fixed = FixedTime::default(); // 60 Hz
        // Bank 15 ms: below one 60 Hz step.
        assert_eq!(fixed.accumulate(Duration::from_millis(15)), 0);
        // The game halves the rate: a 30 Hz step is 33.3 ms and the 60 Hz bank is dropped.
        fixed.hz = 30.0;
        let delta = Duration::from_millis(20);
        let predicted = fixed.would_run(delta);
        let ran = fixed.accumulate(delta);
        assert_eq!(
            predicted,
            ran > 0,
            "would_run predicted {predicted}, accumulate ran {ran} steps"
        );
        // Across many rate changes and deltas the prediction always matches.
        let mut fixed = FixedTime::default();
        let rates = [60.0, 30.0, 120.0, 0.0, f64::NAN, 45.5, 60.0];
        for (i, hz) in rates.iter().copied().cycle().take(64).enumerate() {
            let delta = Duration::from_micros(3_000 + (i as u64 * 7_919) % 40_000);
            fixed.hz = hz;
            let predicted = fixed.would_run(delta);
            let ran = fixed.accumulate(delta);
            assert_eq!(predicted, ran > 0, "frame {i}: hz {hz}, delta {delta:?}");
        }
    }

    #[test]
    fn would_run_ignores_the_bank_after_an_hz_change() {
        // The review scenario: 15 ms banked at 60 Hz, then `hz = 50` (a 20 ms step). The stale
        // bank plus a 6 ms delta looks like a step to the raw accumulator, but `accumulate`
        // drops the bank the same frame, so `would_run` must predict no step — otherwise the
        // zero-step-frame fallback is skipped for a frame that then runs no step at all.
        let mut fixed = FixedTime::default(); // 60 Hz
        assert_eq!(fixed.accumulate(Duration::from_millis(15)), 0);
        fixed.hz = 50.0;
        assert!(
            !fixed.would_run(Duration::from_millis(6)),
            "the bank was made at 60 Hz: it does not count at 50 Hz"
        );
        assert_eq!(
            fixed.accumulate(Duration::from_millis(6)),
            0,
            "the bank is dropped, matching the prediction"
        );
        // At the new rate the bank refills and the prediction tracks it again.
        assert_eq!(fixed.accumulate(Duration::from_millis(13)), 0);
        assert!(fixed.would_run(Duration::from_millis(1)), "19 ms + 1 ms ≥ 20 ms");
        assert_eq!(fixed.accumulate(Duration::from_millis(1)), 1);
    }

    #[test]
    fn time_scale_scales_delta() {
        let mut t = Time {
            time_scale: 0.5,
            ..Time::default()
        };
        t.advance(Duration::from_millis(20));
        assert_eq!(t.delta(), Duration::from_millis(10));
    }

    #[test]
    fn a_huge_time_scale_saturates_instead_of_panicking() {
        for scale in [f32::INFINITY, f32::MAX, 1.0e30, f32::NAN] {
            let mut t = Time {
                time_scale: scale,
                ..Time::default()
            };
            t.advance(MS16);
            t.advance(MS16);
            assert_eq!(t.frame_count(), 2, "scale {scale}");
        }
        // ADR-1340 caps the scale at MAX_TIME_SCALE (this integration keeps that over ADR-538's
        // unbounded product, ADR-1363): an infinite scale is a 1000x step, not `Duration::MAX`.
        let mut t = Time {
            time_scale: f32::INFINITY,
            ..Time::default()
        };
        t.advance(MS16);
        assert_eq!(t.delta(), MS16 * MAX_TIME_SCALE as u32);
        assert_eq!(t.elapsed(), MS16 * MAX_TIME_SCALE as u32);
        // A step past `Duration`'s range still saturates instead of panicking.
        t.advance(Duration::MAX);
        assert_eq!(t.delta(), Duration::MAX);
        assert_eq!(t.elapsed(), Duration::MAX, "elapsed saturates too");
    }

    #[test]
    fn real_time_clamps_and_resets() {
        let start = Instant::now();
        let mut r = RealTime::default();
        r.tick(start);
        assert_eq!(r.delta(), Duration::ZERO);
        r.tick(start + Duration::from_secs(5));
        assert_eq!(r.delta(), MAX_DELTA);
        r.reset_clock();
        r.tick(start + Duration::from_secs(60));
        assert_eq!(
            r.delta(),
            Duration::ZERO,
            "first frame after a suspend has no delta"
        );
    }

    /// A main-loop stall keeps its real length for frame-time reports (lane v018-stalls:
    /// DO27 testers' 4.7 s first-scene frame was reported as 250 ms) while gameplay still gets
    /// the clamped delta.
    #[test]
    fn real_time_raw_delta_keeps_a_stall_unclamped() {
        let start = Instant::now();
        let mut r = RealTime::default();
        r.tick(start);
        assert_eq!(r.raw_delta(), Duration::ZERO);
        r.tick(start + Duration::from_millis(16));
        assert_eq!(r.raw_delta(), Duration::from_millis(16));
        assert_eq!(r.delta(), Duration::from_millis(16));
        r.tick(start + Duration::from_millis(16) + Duration::from_millis(4670));
        assert_eq!(r.raw_delta(), Duration::from_millis(4670));
        assert_eq!(r.delta(), MAX_DELTA, "gameplay still gets the clamp");
        r.reset_clock();
        r.tick(start + Duration::from_secs(60));
        assert_eq!(r.raw_delta(), Duration::ZERO, "a suspend is no frame");
        r.advance_by(Duration::from_secs(1));
        assert_eq!(r.raw_delta(), Duration::from_secs(1));
    }

    #[test]
    fn a_long_gap_is_a_discontinuity_not_a_frame() {
        let start = Instant::now();
        let mut r = RealTime::default();
        r.tick(start);
        r.tick(start + DISCONTINUITY_GAP - Duration::from_millis(1));
        assert_eq!(r.raw_delta(), DISCONTINUITY_GAP - Duration::from_millis(1));
        assert_eq!(r.delta(), MAX_DELTA);
        let before = r.elapsed();
        // A ten minute standby: no frame, no gameplay step, no elapsed time.
        r.tick(start + Duration::from_secs(600));
        assert_eq!((r.raw_delta(), r.delta()), (Duration::ZERO, Duration::ZERO));
        assert_eq!(r.elapsed(), before);
        r.tick(start + Duration::from_secs(600) + Duration::from_millis(16));
        assert_eq!(r.raw_delta(), Duration::from_millis(16));
    }

    #[test]
    fn a_late_wake_resynchronises_only_past_one_period() {
        let t0 = Instant::now();
        let p = Duration::from_millis(16);
        assert_eq!(next_base(t0, t0 + Duration::from_millis(10), p), t0);
        assert_eq!(next_base(t0, t0 + p, p), t0);
        let late = t0 + Duration::from_millis(17);
        assert_eq!(next_base(t0, late, p), late);
    }

    #[test]
    fn manual_time_advances_exactly() {
        let mut r = RealTime::default();
        r.advance_by(Duration::from_secs(1));
        r.advance_by(Duration::from_secs(1));
        assert_eq!(r.elapsed(), Duration::from_secs(2));
        assert_eq!(r.delta(), Duration::from_secs(1), "manual deltas are not clamped");
    }

    #[test]
    fn frame_pacing_caps_rate() {
        let mut pacing = FramePacing::new(Some(200.0));
        assert_eq!(pacing.period(), Some(Duration::from_millis(5)));
        let start = Instant::now();
        for _ in 0..11 {
            pacing.wait();
        }
        // 10 periods of 5 ms after the first call; generous upper bound for loaded CI machines.
        let elapsed = start.elapsed();
        assert!(elapsed >= Duration::from_millis(50), "{elapsed:?}");
        assert!(FramePacing::new(None).wait().is_zero());
        assert!(FramePacing::new(Some(f32::NAN)).period().is_none());
    }

    #[test]
    fn fixed_steps_accumulate_and_cap() {
        let mut f = FixedTime::default();
        assert_eq!(f.accumulate(Duration::from_millis(10)), 0);
        assert_eq!(f.accumulate(Duration::from_millis(10)), 1);
        assert!(f.overstep_fraction() > 0.0 && f.overstep_fraction() < 1.0);
        assert_eq!(f.accumulate(Duration::from_secs(1)), 5, "capped at max_substeps");
        assert_eq!(f.overstep_fraction(), 0.0, "excess dropped");
    }

    #[test]
    fn huge_or_non_finite_time_scale_saturates_instead_of_panicking() {
        for scale in [f32::INFINITY, 1e21, f32::MAX, f32::NAN, -1.0] {
            let mut t = Time {
                time_scale: scale,
                ..Time::default()
            };
            t.advance(MAX_DELTA);
            if scale > 0.0 {
                assert_eq!(
                    t.delta(),
                    Duration::from_secs(250),
                    "scale {scale} saturates at MAX_TIME_SCALE"
                );
            } else {
                assert_eq!(t.delta(), Duration::ZERO, "scale {scale}");
            }
        }
    }

    #[test]
    fn extreme_time_scale_saturates_instead_of_aborting() {
        for scale in [f32::INFINITY, f32::NAN, 1e22, f32::MAX, -5.0] {
            let mut t = Time {
                time_scale: scale,
                ..Time::default()
            };
            t.advance(Duration::ZERO);
            t.advance(MS16);
            t.advance(Duration::MAX);
            assert!(t.delta() <= Duration::MAX);
        }
        let mut t = Time {
            time_scale: f32::INFINITY,
            ..Time::default()
        };
        t.advance(Duration::from_secs(1));
        assert_eq!(t.delta(), Duration::from_secs(1000));
        let mut t = Time {
            time_scale: f32::NAN,
            ..Time::default()
        };
        t.advance(Duration::from_secs(1));
        assert_eq!(t.delta(), Duration::ZERO);
    }

    #[test]
    fn huge_fixed_hz_keeps_a_nonzero_step() {
        let mut f = FixedTime {
            hz: 3e9,
            ..FixedTime::default()
        };
        assert_eq!(f.step(), Duration::from_nanos(1));
        f.accumulate(Duration::from_nanos(2));
        assert!(f.overstep_fraction().is_finite());
        f.hz = f64::NAN;
        assert_eq!(f.step(), Duration::from_secs_f64(1.0 / 60.0));
    }

    #[test]
    fn tiny_frame_caps_saturate_instead_of_panicking() {
        for fps in [1e-20_f32, f32::MIN_POSITIVE] {
            assert_eq!(
                FramePacing::new(Some(fps)).period(),
                Some(Duration::from_secs_f64(1.0 / f64::from(MIN_PACING_FPS)))
            );
        }
    }

    #[test]
    fn manual_durations_saturate_the_clocks() {
        let mut r = RealTime::default();
        r.advance_by(Duration::MAX);
        r.advance_by(Duration::MAX);
        assert_eq!(r.elapsed(), Duration::MAX);
    }

    #[test]
    fn extreme_fixed_hz_and_substeps_stay_bounded() {
        for hz in [1e20, f64::INFINITY, f64::NAN, 1e9] {
            let mut f = FixedTime {
                hz,
                max_substeps: u32::MAX,
                ..FixedTime::default()
            };
            assert!(f.step() > Duration::ZERO);
            let steps = f.accumulate(Duration::from_millis(16));
            assert!(steps <= MAX_SUBSTEPS_LIMIT);
            assert!(f.overstep_fraction().is_finite());
        }
    }

    #[test]
    fn extreme_pacing_fps_clamps_the_period() {
        for fps in [1e-3_f32, 1e-20, 1e20, f32::MAX, f32::MIN_POSITIVE] {
            let period = FramePacing::new(Some(fps)).period().expect("positive fps");
            assert!(period >= Duration::from_secs_f64(1.0 / f64::from(MAX_PACING_FPS)));
            assert!(period <= Duration::from_secs_f64(1.0 / f64::from(MIN_PACING_FPS)));
        }
        let mut pacing = FramePacing::new(None);
        assert!(pacing.wait_for(Some(Duration::MAX)).is_zero());
        assert!(
            pacing.wait_for(Some(Duration::MAX)).is_zero(),
            "deadline overflow must not abort"
        );
    }

    #[test]
    fn enable_fixed_step_pins_manual_duration() {
        let mut app = crate::App::new();
        enable_fixed_step(&mut app, 60.0);
        let strategy = *app.world().resource::<TimeUpdateStrategy>();
        assert_eq!(
            strategy,
            TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(1.0 / 60.0))
        );
        let fixed = app.world().resource::<FixedTime>();
        assert!((fixed.hz - 60.0).abs() < 1e-9);
        assert_eq!(fixed.max_substeps, 1);
    }

    #[test]
    fn accumulate_honors_max_substeps_up_to_the_hard_cap() {
        // The documented default is 5 and the hard cap is 64 (spec 10 section 3.2).
        assert_eq!(FixedTime::default().max_substeps, 5);
        let mut fixed = FixedTime {
            max_substeps: 20,
            ..FixedTime::default()
        };
        assert_eq!(fixed.accumulate(Duration::from_millis(400)), 20);
        let mut fixed = FixedTime {
            max_substeps: 1000,
            ..FixedTime::default()
        };
        assert_eq!(fixed.accumulate(Duration::from_secs(10)), MAX_SUBSTEPS_LIMIT);
    }

    #[test]
    fn fixed_step_app_runs_one_fixed_update_per_frame_and_none_while_paused() {
        #[derive(Resource, Default)]
        struct Runs(u32);
        let mut app = crate::App::new();
        enable_fixed_step(&mut app, 60.0);
        app.init_resource::<Runs>();
        app.add_systems(crate::FixedUpdate, |mut r: ResMut<Runs>| r.0 += 1);
        app.update();
        app.update();
        assert_eq!(app.world().resource::<Runs>().0, 2);
        // Documented exception: pause is a zero gameplay delta, so no fixed step runs.
        app.world_mut().resource_mut::<Time>().paused = true;
        app.update();
        assert_eq!(app.world().resource::<Runs>().0, 2);
    }

    #[test]
    fn manual_duration_is_not_clamped_to_max_delta() {
        let mut app = crate::App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs(1)));
        app.update();
        assert_eq!(app.world().resource::<RealTime>().delta(), Duration::from_secs(1));
        assert_eq!(app.world().resource::<Time>().delta(), Duration::from_secs(1));
    }

    #[test]
    fn accumulate_saturates_instead_of_overflowing() {
        let mut fixed = FixedTime::default();
        fixed.accumulate(Duration::from_millis(1));
        assert_eq!(fixed.accumulate(Duration::MAX), fixed.max_substeps);
        assert!(fixed.overstep_fraction() < 1.0);
    }

    #[test]
    fn a_huge_pacing_period_does_not_overflow_the_deadline() {
        let mut pacing = FramePacing::new(Some(2f32.powi(-63)));
        assert_eq!(
            pacing.period(),
            Some(Duration::from_secs_f64(1.0 / f64::from(MIN_PACING_FPS)))
        );
        assert_eq!(pacing.wait(), Duration::ZERO);
        // The second call forms `last + period`; it must not panic.
        let _ = pacing.wait_for(Some(Duration::MAX));
    }

    #[test]
    fn removed_clocks_are_recreated() {
        let mut app = crate::App::new();
        app.update();
        app.world_mut().remove_resource::<Time>();
        app.world_mut().remove_resource::<RealTime>();
        app.world_mut().remove_resource::<FixedTime>();
        app.update();
        app.update();
        assert!(app.world().contains_resource::<Time>());
        assert!(app.world().contains_resource::<RealTime>());
        assert!(app.world().contains_resource::<FixedTime>());
    }

    #[test]
    fn infinite_time_scale_with_zero_delta_does_not_panic() {
        let mut t = Time {
            time_scale: f32::INFINITY,
            ..Time::default()
        };
        t.advance(Duration::ZERO);
        assert_eq!(t.delta(), Duration::ZERO);
        t.advance(Duration::from_millis(1));
        assert_eq!(t.delta(), Duration::from_secs(1), "saturates at MAX_TIME_SCALE");
        t.time_scale = f32::NAN;
        t.advance(MS16);
        assert_eq!(t.delta(), Duration::ZERO);
        t.time_scale = MAX_TIME_SCALE;
        t.advance(Duration::MAX);
        assert_eq!(t.delta(), Duration::MAX, "overflow saturates");
        t.advance(MS16);
        assert_eq!(t.elapsed(), Duration::MAX);
    }

    #[test]
    fn non_finite_and_extreme_fixed_hz_stay_bounded() {
        let sixty = Duration::from_secs_f64(1.0 / 60.0);
        for hz in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let f = FixedTime {
                hz,
                ..FixedTime::default()
            };
            assert_eq!(f.step(), sixty, "hz {hz}");
        }
        let f = FixedTime {
            hz: 3.0e9,
            ..FixedTime::default()
        };
        assert_eq!(f.step(), Duration::from_nanos(1));
        let f = FixedTime {
            hz: 1.0e-20,
            ..FixedTime::default()
        };
        assert_eq!(f.step(), Duration::from_secs(1));
    }

    #[test]
    fn enable_fixed_step_clamps_extreme_hz_and_matches_step() {
        for hz in [3.0e9, f64::NAN, f64::INFINITY, 0.0] {
            let mut app = crate::App::new();
            enable_fixed_step(&mut app, hz);
            let fixed = app.world().resource::<FixedTime>().clone();
            let strategy = *app.world().resource::<TimeUpdateStrategy>();
            assert_eq!(
                strategy,
                TimeUpdateStrategy::ManualDuration(fixed.step()),
                "hz {hz}"
            );
            assert!(!fixed.step().is_zero(), "hz {hz}");
        }
    }

    #[test]
    fn tiny_frame_caps_wait_at_most_one_second() {
        for fps in [1.0e-3, 1.0e-20, f32::MIN_POSITIVE, 0.5] {
            let p = FramePacing::new(Some(fps)).period();
            assert_eq!(p, Some(Duration::from_secs(1)), "fps {fps}");
        }
    }
}
