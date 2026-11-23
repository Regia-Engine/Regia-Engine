// SPDX-License-Identifier: MPL-2.0
//! How the frame loop runs (`docs/specs/10-core.md` § 3.2): one [`LoopSettings`] view over
//! [`FramePacing`], [`FixedTime`] and the runner's throttling, plus the [`LoopMode`] the runners
//! follow (designed after Piston's `EventSettings`, see `docs/provenance.md`).
//!
//! | Mode | Frames | Input | Pacing |
//! |---|---|---|---|
//! | [`LoopMode::Continuous`] | every vsync / redraw (default) | delivered | [`FramePacing`] cap |
//! | [`LoopMode::Bench`] | flat out, back to back | **ignored** by the window runner | none |
//! | [`LoopMode::Lazy`] | on input, window events and [`LoopControl::request_redraw`], else at the idle rate | delivered | [`FramePacing`] cap |
//! | [`LoopMode::RunFrames`] | `n` frames, then frozen | delivered | cap, then the idle rate |
//!
//! *Frozen* (after [`LoopMode::RunFrames`]): gameplay [`Time`] is paused and frames run at
//! [`LoopControl::throttled_fps`] so the window and the agent protocol stay responsive; switching
//! to another mode thaws it. The headless runner has no OS events, so it treats `Lazy` like
//! `Continuous`.

use std::time::Duration;

use regia_ecs::prelude::*;

use crate::time::{
    FixedTime, FramePacing, MAX_PACING_FPS, MAX_SUBSTEPS_LIMIT, MIN_PACING_FPS, Time, sanitize_hz,
};

/// The runner mode.
///
/// | Mode | Frames | Input | Pacing |
/// |---|---|---|---|
/// | [`Self::Continuous`] | every vsync / redraw (default) | delivered | [`FramePacing`] cap |
/// | [`Self::Bench`] | flat out, back to back | **ignored** by the window runner | none |
/// | [`Self::Lazy`] | on input, window events and [`LoopControl::request_redraw`], else at the idle rate | delivered | [`FramePacing`] cap |
/// | [`Self::RunFrames`] | `n` frames, then frozen | delivered | cap, then the idle rate |
///
/// After [`LoopMode::RunFrames`] the loop is *frozen*: gameplay [`Time`] is paused and frames run
/// at [`LoopControl::throttled_fps`] so the window and the agent protocol stay responsive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LoopMode {
    /// A frame per redraw, capped by [`FramePacing`].
    #[default]
    Continuous,
    /// No sleeps, no frame cap, input ignored: frames back to back for benchmarks.
    Bench,
    /// Frames only on input, window events or a redraw request, plus an idle tick.
    Lazy,
    /// Runs this many frames (from when the mode was set), then freezes.
    RunFrames(u64),
}

impl LoopMode {
    /// `RunFrames(0)` becomes `RunFrames(1)` (a zero budget would freeze after running a frame
    /// anyway); other modes are unchanged.
    pub fn normalized(self) -> Self {
        match self {
            Self::RunFrames(0) => Self::RunFrames(1),
            other => other,
        }
    }

    /// The protocol name: `continuous`, `bench`, `lazy` or `run_frames`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Continuous => "continuous",
            Self::Bench => "bench",
            Self::Lazy => "lazy",
            Self::RunFrames(_) => "run_frames",
        }
    }

    /// Parses a protocol name; `run_frames` takes `frames` (at least 1).
    pub fn from_name(name: &str, frames: Option<u64>) -> Option<Self> {
        match name {
            "continuous" => Some(Self::Continuous),
            "bench" => Some(Self::Bench),
            "lazy" => Some(Self::Lazy),
            "run_frames" | "run_n_then_freeze" => frames.filter(|n| *n > 0).map(Self::RunFrames),
            _ => None,
        }
    }
}

/// Lowest throttled frame rate [`LoopSettings::apply`] stores (one frame per second).
pub const MIN_THROTTLED_FPS: f32 = 1.0;
/// Highest throttled frame rate [`LoopSettings::apply`] stores.
pub const MAX_THROTTLED_FPS: f32 = 1_000.0;

/// The throttled rate the runners honor: invalid becomes the default, the rest is clamped to
/// [`MIN_THROTTLED_FPS`]`..=`[`MAX_THROTTLED_FPS`].
fn sanitize_throttled_fps(fps: f32) -> f32 {
    if fps.is_finite() && fps > 0.0 {
        fps.clamp(MIN_THROTTLED_FPS, MAX_THROTTLED_FPS)
    } else {
        DEFAULT_THROTTLED_FPS
    }
}

/// Default [`LoopControl::throttled_fps`].
pub const DEFAULT_THROTTLED_FPS: f32 = 10.0;

/// The live loop mode the runners read. Change it through [`LoopControl::set_mode`] or
/// [`LoopSettings::apply`] (which inserts it). Absent means the default: [`LoopMode::Continuous`].
#[derive(Resource, Debug, Clone)]
pub struct LoopControl {
    mode: LoopMode,
    /// Frame rate while every window is hidden, while frozen, and the idle tick of
    /// [`LoopMode::Lazy`].
    pub throttled_fps: f32,
    frames_in_mode: u64,
    frozen: bool,
    paused_by_freeze: bool,
    /// `set_mode` ran while frozen: the next `Last` thaws before it counts a frame.
    thaw_pending: bool,
    redraw_requested: bool,
}

impl Default for LoopControl {
    fn default() -> Self {
        Self {
            mode: LoopMode::Continuous,
            throttled_fps: DEFAULT_THROTTLED_FPS,
            frames_in_mode: 0,
            frozen: false,
            paused_by_freeze: false,
            thaw_pending: false,
            redraw_requested: false,
        }
    }
}

impl LoopControl {
    /// The current mode.
    pub fn mode(&self) -> LoopMode {
        self.mode
    }

    /// Switches mode and restarts the frame count. A frozen loop thaws at the end of the current
    /// frame (that frame is not counted), whatever the new mode. `RunFrames(0)` runs one frame
    /// (the same as `RunFrames(1)`), like the protocol's minimum.
    pub fn set_mode(&mut self, mode: LoopMode) {
        self.mode = mode.normalized();
        self.frames_in_mode = 0;
        if self.frozen {
            self.thaw_pending = true;
        }
    }

    /// Frames run since the mode was set (stops counting once frozen).
    pub fn frames_in_mode(&self) -> u64 {
        self.frames_in_mode
    }

    /// True once a [`LoopMode::RunFrames`] budget is spent.
    pub fn is_frozen(&self) -> bool {
        self.frozen
    }

    /// Whether the window runner forwards input events ([`LoopMode::Bench`] ignores them).
    pub fn accepts_input(&self) -> bool {
        self.mode != LoopMode::Bench
    }

    /// Whether [`FramePacing`]'s cap applies ([`LoopMode::Bench`] runs uncapped; a frozen loop
    /// runs at the throttled rate instead).
    pub fn paces(&self) -> bool {
        self.mode != LoopMode::Bench && !self.frozen
    }

    /// The frame period of [`LoopControl::throttled_fps`] (at least 1 ms, at most 1 s).
    pub fn throttle_period(&self) -> Duration {
        Duration::from_secs_f64(1.0 / f64::from(sanitize_throttled_fps(self.throttled_fps)))
    }

    /// Asks a [`LoopMode::Lazy`] runner for a frame soon (other modes run frames anyway).
    pub fn request_redraw(&mut self) {
        self.redraw_requested = true;
    }

    /// Takes a pending [`LoopControl::request_redraw`] (runners call this).
    pub fn take_redraw_request(&mut self) -> bool {
        std::mem::take(&mut self.redraw_requested)
    }
}

/// `First` system, ordered ahead of `update_time`: while a frozen [`LoopMode::RunFrames`] loop
/// owns the gameplay pause, the flag is restored before the clocks advance, so an unpause
/// written any time after the previous frame's `Last` cannot leak a frame of gameplay time (or a
/// `FixedUpdate`) into the frozen loop. `update_loop_control` still re-forces the pause in
/// `Last`, so a toggle later in the same frame cannot stick either.
pub(crate) fn enforce_freeze_pause(control: Option<Res<LoopControl>>, time: Option<ResMut<Time>>) {
    let (Some(control), Some(mut time)) = (control, time) else {
        return;
    };
    if control.frozen && control.paused_by_freeze {
        time.paused = true;
    }
}

/// `Last`: counts [`LoopMode::RunFrames`] frames, freezes when the budget is spent (pausing
/// [`Time`]), and thaws when the mode changes.
pub(crate) fn update_loop_control(control: Option<ResMut<LoopControl>>, time: Option<ResMut<Time>>) {
    let (Some(mut control), Some(mut time)) = (control, time) else {
        return;
    };
    if control.frozen && std::mem::take(&mut control.thaw_pending) {
        // The mode was set while frozen: this frame ran paused, so it does not count. Thaw first.
        control.frozen = false;
        if std::mem::take(&mut control.paused_by_freeze) {
            time.paused = false;
        }
        tracing::info!(target: "regia_app", "loop thawed ({})", control.mode.name());
        return;
    }
    let budget = match control.mode {
        LoopMode::RunFrames(frames) => Some(frames),
        _ => None,
    };
    if let Some(frames) = budget
        && control.frames_in_mode < frames
    {
        control.frames_in_mode += 1;
    }
    let spent = budget.is_some_and(|frames| control.frames_in_mode >= frames);
    if spent && !control.frozen {
        control.frozen = true;
        if !time.paused {
            time.paused = true;
            control.paused_by_freeze = true;
        }
        tracing::info!(target: "regia_app", "loop frozen after {} frames", control.frames_in_mode);
    } else if spent && control.frozen && control.paused_by_freeze {
        // A pause toggle must not leak gameplay time into a frozen loop.
        time.paused = true;
    } else if !spent && control.frozen {
        control.frozen = false;
        if std::mem::take(&mut control.paused_by_freeze) {
            time.paused = false;
        }
        tracing::info!(target: "regia_app", "loop thawed ({})", control.mode.name());
    }
}

/// One view over every loop knob (after Piston's `EventSettings`; modes: [`LoopMode`], frozen and
/// throttling: [`LoopControl`]): read it with
/// [`LoopSettings::read`], change fields, write it back with [`LoopSettings::apply`].
///
/// ```
/// use regia_app::{App, LoopMode, LoopSettings};
///
/// let mut app = App::new();
/// LoopSettings::read(app.world())
///     .max_fps(Some(30.0))
///     .fixed_hz(120.0)
///     .mode(LoopMode::Bench)
///     .apply(app.world_mut());
/// let settings = LoopSettings::read(app.world());
/// assert_eq!(settings.mode, LoopMode::Bench);
/// assert_eq!(settings.max_fps, Some(30.0));
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct LoopSettings {
    /// The runner mode.
    pub mode: LoopMode,
    /// Frame cap ([`FramePacing::target_fps`]); `None` runs at vsync or uncapped.
    pub max_fps: Option<f32>,
    /// Fixed updates per second ([`FixedTime::hz`]).
    pub fixed_hz: f64,
    /// Catch-up limit: most fixed updates in one frame ([`FixedTime::max_substeps`]).
    pub max_substeps: u32,
    /// Hidden-window, frozen and lazy-idle frame rate ([`LoopControl::throttled_fps`]).
    pub throttled_fps: f32,
}

impl Default for LoopSettings {
    fn default() -> Self {
        let fixed = FixedTime::default();
        Self {
            mode: LoopMode::Continuous,
            max_fps: None,
            fixed_hz: fixed.hz,
            max_substeps: fixed.max_substeps,
            throttled_fps: DEFAULT_THROTTLED_FPS,
        }
    }
}

impl LoopSettings {
    /// The current values (defaults for resources that are missing).
    pub fn read(world: &World) -> Self {
        let defaults = Self::default();
        let fixed = world.get_resource::<FixedTime>();
        let control = world.get_resource::<LoopControl>();
        Self {
            mode: control.map_or(defaults.mode, LoopControl::mode),
            max_fps: world
                .get_resource::<FramePacing>()
                .and_then(|pacing| pacing.target_fps),
            fixed_hz: fixed.map_or(defaults.fixed_hz, |f| f.hz),
            max_substeps: fixed.map_or(defaults.max_substeps, |f| f.max_substeps),
            throttled_fps: control.map_or(defaults.throttled_fps, |c| c.throttled_fps),
        }
    }

    /// Writes every field to its resource (inserting missing ones). The mode is only re-set (and
    /// its frame count restarted) when it changed.
    pub fn apply(&self, world: &mut World) {
        // Store the rates the pacer honors, so `read` reports what actually runs.
        world.get_resource_or_insert_with(FramePacing::default).target_fps = self
            .max_fps
            .filter(|fps| fps.is_finite() && *fps > 0.0)
            .map(|fps| fps.clamp(MIN_PACING_FPS, MAX_PACING_FPS));
        {
            let mut fixed = world.get_resource_or_insert_with(FixedTime::default);
            let hz = sanitize_hz(self.fixed_hz);
            if fixed.hz != hz {
                // Time banked at the old step length would run as steps of the new one.
                fixed.clear_accumulator();
            }
            fixed.hz = hz;
            fixed.max_substeps = self.max_substeps.clamp(1, MAX_SUBSTEPS_LIMIT);
        }
        let mut control = world.get_resource_or_insert_with(LoopControl::default);
        control.throttled_fps = sanitize_throttled_fps(self.throttled_fps);
        let mode = self.mode.normalized();
        if control.mode() != mode {
            control.set_mode(mode);
        }
    }

    /// Sets [`LoopSettings::mode`].
    pub fn mode(mut self, mode: LoopMode) -> Self {
        self.mode = mode;
        self
    }

    /// Sets [`LoopSettings::max_fps`].
    pub fn max_fps(mut self, max_fps: Option<f32>) -> Self {
        self.max_fps = max_fps;
        self
    }

    /// Sets [`LoopSettings::fixed_hz`] (updates per second).
    pub fn fixed_hz(mut self, hz: f64) -> Self {
        self.fixed_hz = hz;
        self
    }

    /// Sets [`LoopSettings::max_substeps`].
    pub fn max_substeps(mut self, steps: u32) -> Self {
        self.max_substeps = steps;
        self
    }

    /// Sets [`LoopSettings::throttled_fps`].
    pub fn throttled_fps(mut self, fps: f32) -> Self {
        self.throttled_fps = fps;
        self
    }

    /// Shorthand for [`LoopMode::Bench`].
    pub fn bench(self) -> Self {
        self.mode(LoopMode::Bench)
    }

    /// Shorthand for [`LoopMode::Lazy`].
    pub fn lazy(self) -> Self {
        self.mode(LoopMode::Lazy)
    }

    /// Shorthand for [`LoopMode::RunFrames`]: run `frames` frames, then freeze.
    pub fn run_n_then_freeze(self, frames: u64) -> Self {
        self.mode(LoopMode::RunFrames(frames))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::time::MAX_FIXED_HZ;
    use crate::{App, AppExit, Last, TimeUpdateStrategy, run_headless};

    #[test]
    fn settings_round_trip_through_the_resources() {
        let mut app = App::new();
        let settings = LoopSettings::read(app.world());
        assert_eq!(settings, LoopSettings::default());
        settings
            .clone()
            .max_fps(Some(48.0))
            .fixed_hz(30.0)
            .max_substeps(3)
            .throttled_fps(5.0)
            .lazy()
            .apply(app.world_mut());
        assert_eq!(app.world().resource::<FramePacing>().target_fps, Some(48.0));
        assert_eq!(app.world().resource::<FixedTime>().hz, 30.0);
        assert_eq!(app.world().resource::<FixedTime>().max_substeps, 3);
        let control = app.world().resource::<LoopControl>();
        assert_eq!(control.mode(), LoopMode::Lazy);
        assert_eq!(control.throttle_period(), Duration::from_millis(200));
        assert_eq!(LoopSettings::read(app.world()).mode, LoopMode::Lazy);
        // Invalid values are clamped, not stored.
        LoopSettings::read(app.world())
            .max_fps(Some(f32::NAN))
            .fixed_hz(0.0)
            .max_substeps(0)
            .apply(app.world_mut());
        let read = LoopSettings::read(app.world());
        assert_eq!((read.max_fps, read.fixed_hz, read.max_substeps), (None, 1.0, 1));
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in [
            LoopMode::Continuous,
            LoopMode::Bench,
            LoopMode::Lazy,
            LoopMode::RunFrames(4),
        ] {
            assert_eq!(LoopMode::from_name(mode.name(), Some(4)), Some(mode));
        }
        assert_eq!(LoopMode::from_name("run_frames", None), None);
        assert_eq!(LoopMode::from_name("warp", None), None);
    }

    #[test]
    fn run_n_then_freeze_pauses_time_and_thaws_on_mode_change() {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(10)));
        LoopSettings::default()
            .run_n_then_freeze(3)
            .apply(app.world_mut());
        for _ in 0..3 {
            app.update();
        }
        let control = app.world().resource::<LoopControl>();
        assert!(control.is_frozen());
        assert!(!control.paces(), "frozen frames run at the throttled rate");
        assert_eq!(control.frames_in_mode(), 3);
        let elapsed = app.world().resource::<Time>().elapsed();
        assert_eq!(elapsed, Duration::from_millis(30), "three frames of game time");
        for _ in 0..5 {
            app.update();
        }
        assert_eq!(
            app.world().resource::<Time>().elapsed(),
            elapsed,
            "frozen: time stands still"
        );
        assert_eq!(
            app.world().resource::<Time>().frame_count(),
            8,
            "frames still run"
        );

        app.world_mut()
            .resource_mut::<LoopControl>()
            .set_mode(LoopMode::Continuous);
        app.update(); // thaws at the end of this frame
        app.update();
        assert!(!app.world().resource::<LoopControl>().is_frozen());
        assert!(!app.world().resource::<Time>().paused);
        assert_eq!(
            app.world().resource::<Time>().elapsed(),
            elapsed + Duration::from_millis(10)
        );
    }

    #[test]
    fn freezing_keeps_a_game_pause() {
        let mut app = App::new();
        app.world_mut().resource_mut::<Time>().paused = true;
        LoopSettings::default()
            .run_n_then_freeze(1)
            .apply(app.world_mut());
        app.update();
        assert!(app.world().resource::<LoopControl>().is_frozen());
        app.world_mut()
            .resource_mut::<LoopControl>()
            .set_mode(LoopMode::Continuous);
        app.update();
        assert!(
            app.world().resource::<Time>().paused,
            "the game's own pause survives the thaw"
        );
    }

    /// Exits after `frames` frames.
    fn exit_after(frames: u64) -> impl FnMut(Res<Time>, MessageWriter<AppExit>) {
        move |time: Res<Time>, mut exit: MessageWriter<AppExit>| {
            if time.frame_count() >= frames {
                exit.write(AppExit::Success);
            }
        }
    }

    #[test]
    fn bench_mode_runs_flat_out_ignoring_the_frame_cap() {
        const FRAMES: u64 = 40;
        // Capped at 200 fps, 40 frames take about 200 ms.
        let mut capped = App::new();
        capped.insert_resource(FramePacing::new(Some(200.0)));
        capped.add_systems(Last, exit_after(FRAMES));
        let start = Instant::now();
        assert_eq!(run_headless(capped), AppExit::Success);
        let capped_time = start.elapsed();
        assert!(capped_time >= Duration::from_millis(190), "{capped_time:?}");

        let mut bench = App::new();
        bench.insert_resource(FramePacing::new(Some(200.0)));
        LoopSettings::read(bench.world()).bench().apply(bench.world_mut());
        bench.add_systems(Last, exit_after(FRAMES));
        let start = Instant::now();
        assert_eq!(run_headless(bench), AppExit::Success);
        let bench_time = start.elapsed();
        assert!(
            bench_time < capped_time / 2,
            "bench {bench_time:?} must not sleep (capped {capped_time:?})"
        );
        let control = LoopControl {
            mode: LoopMode::Bench,
            ..LoopControl::default()
        };
        assert!(!control.accepts_input() && !control.paces());
    }

    #[test]
    fn lazy_redraw_requests_are_taken_once() {
        let mut control = LoopControl::default();
        assert!(control.accepts_input());
        assert!(!control.take_redraw_request());
        control.request_redraw();
        assert!(control.take_redraw_request());
        assert!(!control.take_redraw_request());
        control.throttled_fps = f32::NAN;
        assert_eq!(control.throttle_period(), Duration::from_millis(100));
    }

    #[test]
    fn extreme_knobs_are_clamped_not_fatal() {
        let mut control = LoopControl::default();
        for fps in [1e-20_f32, 1e-3, 1e20, f32::MAX, f32::MIN_POSITIVE] {
            control.throttled_fps = fps;
            let period = control.throttle_period();
            assert!(period >= Duration::from_millis(1) && period <= Duration::from_secs(1));
        }
        let mut app = crate::App::new();
        LoopSettings::read(app.world())
            .fixed_hz(1e20)
            .max_substeps(u32::MAX)
            .apply(app.world_mut());
        let fixed = app.world().resource::<FixedTime>();
        assert_eq!(fixed.hz, MAX_FIXED_HZ);
        assert_eq!(fixed.max_substeps, MAX_SUBSTEPS_LIMIT);
        LoopSettings::read(app.world())
            .fixed_hz(f64::NAN)
            .apply(app.world_mut());
        assert_eq!(app.world().resource::<FixedTime>().hz, FixedTime::default().hz);
    }

    #[test]
    fn run_frames_needs_a_positive_budget() {
        assert_eq!(LoopMode::from_name("run_frames", Some(0)), None);
        assert_eq!(
            LoopMode::from_name("run_frames", Some(3)),
            Some(LoopMode::RunFrames(3))
        );
    }

    #[test]
    fn changing_fixed_hz_drops_banked_time() {
        let mut app = App::new();
        LoopSettings::read(app.world())
            .fixed_hz(20.0)
            .apply(app.world_mut());
        app.world_mut()
            .resource_mut::<FixedTime>()
            .accumulate(Duration::from_millis(48));
        app.world_mut().resource_mut::<Time>().paused = true;
        LoopSettings::read(app.world())
            .fixed_hz(60.0)
            .apply(app.world_mut());
        let steps = app
            .world_mut()
            .resource_mut::<FixedTime>()
            .accumulate(Duration::ZERO);
        assert_eq!(steps, 0);
    }
}
