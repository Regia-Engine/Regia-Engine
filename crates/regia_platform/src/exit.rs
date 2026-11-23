// SPDX-License-Identifier: MPL-2.0
//! Clean exit (ADR-188): one ordered, time-boxed GPU shutdown for every way an app quits.
//!
//! A quit used to drop the window surfaces while the app was still fullscreen and the GPU still
//! busy, and left the device to the world's drop, or to the process's exit handlers. On Windows
//! with NVIDIA (fullscreen, Immediate present) that hard-locked the PC; on Linux the desktop froze
//! for about a minute. The runner now runs [`run_clean_exit`] from winit's `exiting`, so a window
//! close, [`AppExit`], [`crate::request_quit`] and [`request_clean_exit`] all take the same path:
//!
//! 1. [`ExitStep::StopBackgroundWork`]: background GPU work stops (the pipeline warm queue).
//! 2. [`ExitStep::LeaveFullscreen`]: every fullscreen window goes back to windowed.
//! 3. [`ExitStep::WaitGpuIdle`]: the GPU drains, bounded ([`CleanExitSettings::gpu_idle_timeout`]).
//! 4. [`ExitStep::DropSurfaces`]: the `Suspended` and `WillTerminate` lifecycle transitions; the
//!    renderer drops every window surface (swapchain).
//! 5. [`ExitStep::DropGpuResources`]: the renderers, then the whole world (every entity,
//!    resource and system state), so nothing that holds a GPU object is left. The OS windows
//!    are kept until the end of the sequence.
//! 6. [`ExitStep::DropDevice`]: the GPU device, bounded ([`CleanExitSettings::device_drop_timeout`]).
//! 7. [`ExitStep::SaveCaches`]: the pipeline cache is written to disk.
//!
//! Engine crates add their part with [`ExitHooks`]. Every step logs its duration, and a step
//! over its budget logs a warning and the sequence goes on: the sequence never waits without a
//! bound of its own. A synchronous `Drop` that blocks forever cannot be interrupted, though, and
//! neither can the driver's own exit handlers after `main` returns: a host keeps a last-resort
//! watchdog for that ([`CleanExitSettings::watchdog`] or [`arm_exit_watchdog`]), which ends the
//! process without running exit handlers (`_exit` / `TerminateProcess`).
//!
//! Suspending (Android's home button, [`crate::RunnerSettings::simulate_suspend_every`]) is not
//! an exit: it still only drops the surfaces and keeps the device.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use regia_app::prelude::*;

use crate::CleanExitInProgress;
use crate::lifecycle::{LifecycleEvent, apply_transition};
use crate::window::{DisplayHandle, WindowHandles};

/// One step of the clean exit, in the order [`run_clean_exit`] runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExitStep {
    /// Background GPU work stops (the pipeline warm queue's workers).
    StopBackgroundWork,
    /// Every fullscreen window goes back to windowed, before its swapchain goes.
    LeaveFullscreen,
    /// Wait, bounded, for the GPU to finish the submitted work.
    WaitGpuIdle,
    /// The `Suspended` (if running) and `WillTerminate` lifecycle transitions: surfaces drop.
    DropSurfaces,
    /// The renderers and every other GPU-holding object: the world is cleared.
    DropGpuResources,
    /// The GPU device itself.
    DropDevice,
    /// Caches written to disk (the pipeline cache).
    SaveCaches,
}

impl ExitStep {
    /// Every step, in order.
    pub const ALL: [ExitStep; 7] = [
        ExitStep::StopBackgroundWork,
        ExitStep::LeaveFullscreen,
        ExitStep::WaitGpuIdle,
        ExitStep::DropSurfaces,
        ExitStep::DropGpuResources,
        ExitStep::DropDevice,
        ExitStep::SaveCaches,
    ];

    /// Stable snake-case name (`regia::exit` events, `summary.json`).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            ExitStep::StopBackgroundWork => "stop_background_work",
            ExitStep::LeaveFullscreen => "leave_fullscreen",
            ExitStep::WaitGpuIdle => "wait_gpu_idle",
            ExitStep::DropSurfaces => "drop_surfaces",
            ExitStep::DropGpuResources => "drop_gpu_resources",
            ExitStep::DropDevice => "drop_device",
            ExitStep::SaveCaches => "save_caches",
        }
    }

    /// Short name for logs.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            ExitStep::StopBackgroundWork => "stop background work",
            ExitStep::LeaveFullscreen => "leave fullscreen",
            ExitStep::WaitGpuIdle => "wait for GPU idle",
            ExitStep::DropSurfaces => "drop surfaces",
            ExitStep::DropGpuResources => "drop renderers and GPU resources",
            ExitStep::DropDevice => "drop device",
            ExitStep::SaveCaches => "save caches",
        }
    }
}

/// Budgets and the last-resort watchdog of the clean exit. Optional resource; the defaults
/// apply without it.
#[derive(Resource, Debug, Clone, PartialEq, Eq)]
pub struct CleanExitSettings {
    /// Longest wait for the GPU to go idle ([`ExitStep::WaitGpuIdle`]). Default 1 s.
    pub gpu_idle_timeout: Duration,
    /// Longest wait for the device to be destroyed ([`ExitStep::DropDevice`]); the drop runs on
    /// a helper thread, and after this the sequence goes on without it. Default 1 s.
    pub device_drop_timeout: Duration,
    /// Ends the process this long after the clean exit starts if it is still running then
    /// (including the exit handlers after `main` returns), without running exit handlers.
    /// `None` (default): no watchdog.
    pub watchdog: Option<Duration>,
    /// Exit code the watchdog ends the process with. Default 0.
    pub watchdog_exit_code: i32,
}

impl Default for CleanExitSettings {
    fn default() -> Self {
        Self {
            gpu_idle_timeout: Duration::from_secs(1),
            device_drop_timeout: Duration::from_secs(1),
            watchdog: None,
            watchdog_exit_code: 0,
        }
    }
}

impl CleanExitSettings {
    /// The time `step` may take before the sequence logs it as over budget.
    #[must_use]
    pub fn budget(&self, step: ExitStep) -> Duration {
        const SLACK: Duration = Duration::from_millis(250);
        match step {
            // The warm queue waits up to 2 s for a job in a compile.
            ExitStep::StopBackgroundWork => Duration::from_millis(2500),
            ExitStep::LeaveFullscreen => Duration::from_millis(250),
            ExitStep::WaitGpuIdle => self.gpu_idle_timeout + SLACK,
            ExitStep::DropSurfaces => Duration::from_millis(500),
            ExitStep::DropGpuResources => Duration::from_secs(1),
            ExitStep::DropDevice => self.device_drop_timeout + SLACK,
            ExitStep::SaveCaches => Duration::from_millis(500),
        }
    }
}

/// One exit hook: runs during its [`ExitStep`] with the app's world.
pub type ExitHook = Box<dyn FnMut(&mut World) + Send + Sync>;

/// What engine crates run during the clean exit, by step, in the order added. The renderer
/// adds its warm-queue stop, GPU idle wait, device drop and pipeline-cache save here.
///
/// A hook that keeps something alive past [`ExitStep::DropGpuResources`] (the world is cleared
/// there) moves it into its own state first, as the renderer does with its device.
#[derive(Resource, Default)]
pub struct ExitHooks {
    hooks: Vec<(ExitStep, &'static str, ExitHook)>,
    done: Vec<DoneHook>,
    /// The fast exit's flush job builders (`ExitHooks::add_flush`, ADR-8833).
    pub(crate) flush: Vec<(&'static str, crate::fast_exit::FlushHook)>,
    /// The fast exit's hide hooks (`ExitHooks::on_fast_hide`, ADR-8833).
    pub(crate) fast_hide: Vec<(&'static str, crate::fast_exit::FastHideHook)>,
}

/// A callback run once the whole sequence has finished, with its report (no world: it is gone).
pub type DoneHook = Box<dyn FnOnce(&CleanExitReport) + Send + Sync>;

impl ExitHooks {
    /// Adds `hook` to `step`; `name` appears in the exit log.
    pub fn add(
        &mut self,
        step: ExitStep,
        name: &'static str,
        hook: impl FnMut(&mut World) + Send + Sync + 'static,
    ) -> &mut Self {
        self.hooks.push((step, name, Box::new(hook)));
        self
    }

    /// Adds `hook`, run once after the last step with the sequence's report. The world is
    /// already cleared then: the hook keeps what it needs in its own state. Telemetry uses it for
    /// `quit_done` and the final step timings in `summary.json`.
    pub fn on_done(&mut self, hook: impl FnOnce(&CleanExitReport) + Send + Sync + 'static) -> &mut Self {
        self.done.push(Box::new(hook));
        self
    }

    /// Number of hooks.
    #[must_use]
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// No hooks.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

/// How long one step took.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStepTiming {
    /// The step.
    pub step: ExitStep,
    /// How long it took.
    pub elapsed: Duration,
    /// Its budget ([`CleanExitSettings::budget`]).
    pub budget: Duration,
}

impl ExitStepTiming {
    /// The step took longer than its budget.
    #[must_use]
    pub fn over_budget(&self) -> bool {
        self.elapsed > self.budget
    }
}

/// What [`run_clean_exit`] did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanExitReport {
    /// Every step, in the order it ran.
    pub steps: Vec<ExitStepTiming>,
    /// Fullscreen windows sent back to windowed.
    pub left_fullscreen: usize,
    /// Entities despawned when the world was cleared (bevy stores resources as entities: they
    /// count too).
    pub entities_dropped: usize,
    /// The whole sequence.
    pub total: Duration,
}

/// A window that can leave fullscreen (the winit window; a mock in tests).
pub trait FullscreenWindow {
    /// The window is fullscreen (any kind).
    fn is_fullscreen(&self) -> bool;
    /// Back to windowed.
    fn leave_fullscreen(&self);
}

impl FullscreenWindow for winit::window::Window {
    fn is_fullscreen(&self) -> bool {
        self.fullscreen().is_some()
    }

    fn leave_fullscreen(&self) {
        self.set_fullscreen(None);
    }
}

/// Sends every fullscreen window in `windows` back to windowed; returns how many were.
pub fn leave_fullscreen<'a>(windows: impl IntoIterator<Item = &'a dyn FullscreenWindow>) -> usize {
    let mut n = 0;
    for window in windows {
        if window.is_fullscreen() {
            window.leave_fullscreen();
            n += 1;
        }
    }
    n
}

/// Asks the app to exit through the clean exit: writes [`AppExit::Success`], so the app exits at
/// the end of this frame and the runner runs [`run_clean_exit`]. Unlike [`crate::request_quit`]
/// it does not go through [`crate::ClosePolicy`]: the host has decided. Writing `AppExit` or
/// closing the primary window takes the same path; this call adds a log line and is the one
/// entry point to document for hosts.
pub fn request_clean_exit(world: &mut World) {
    tracing::info!(target: "regia_platform", "regia exit: clean exit requested");
    world.write_message(AppExit::Success);
}

/// The steps of the clean exit that have finished so far in this process, with their durations in
/// milliseconds (cleared when [`run_clean_exit`] starts). An exit hook reads it to report the
/// steps before its own (telemetry's `quit_done`).
#[must_use]
pub fn clean_exit_progress() -> Vec<(ExitStep, f64)> {
    PROGRESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

static PROGRESS: std::sync::Mutex<Vec<(ExitStep, f64)>> = std::sync::Mutex::new(Vec::new());

/// The `regia::exit` event's `data`: every step's time and budget, as JSON text (the platform
/// crate has no JSON dependency; the numbers and names need no escaping).
fn exit_data_json(report: &CleanExitReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(512);
    let _ = write!(
        out,
        "{{\"total_ms\":{:.1},\"left_fullscreen\":{},\"entities_dropped\":{},\"steps\":[",
        report.total.as_secs_f64() * 1e3,
        report.left_fullscreen,
        report.entities_dropped
    );
    for (i, t) in report.steps.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let _ = write!(
            out,
            "{{\"step\":\"{}\",\"ms\":{:.1},\"budget_ms\":{},\"over_budget\":{}}}",
            t.step.name(),
            t.elapsed.as_secs_f64() * 1e3,
            t.budget.as_millis(),
            t.over_budget()
        );
    }
    out.push_str("]}");
    out
}

/// Runs the clean exit on `app`: the [`ExitStep`]s in order, `transitions` (normally
/// [`crate::LifecycleTracker::exiting`]'s) during [`ExitStep::DropSurfaces`]. The platform runner
/// calls it from winit's `exiting`; a custom runner calls it once its loop has ended. The world is
/// empty afterwards; drop the app.
///
/// Logs `regia exit: <step> took <ms> ms` per step and `regia exit: done in <ms> ms`.
pub fn run_clean_exit(app: &mut App, transitions: &[LifecycleEvent]) -> CleanExitReport {
    let start = Instant::now();
    let settings = app
        .world()
        .get_resource::<CleanExitSettings>()
        .cloned()
        .unwrap_or_default();
    PROGRESS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    tracing::info!(target: "regia_platform", "regia exit: clean exit started");
    if let Some(deadline) = settings.watchdog {
        // The handle is dropped, never disarmed: the watchdog also covers the exit handlers.
        let _ = arm_exit_watchdog(deadline, settings.watchdog_exit_code);
    }
    let mut hooks = app.world_mut().remove_resource::<ExitHooks>().unwrap_or_default();
    app.world_mut().insert_resource(CleanExitInProgress);
    let mut report = CleanExitReport::default();
    // The OS windows outlive the device: dropped after the last step.
    let mut kept_windows: Option<(Option<WindowHandles>, Option<DisplayHandle>)> = None;
    for step in ExitStep::ALL {
        let t = Instant::now();
        match step {
            ExitStep::LeaveFullscreen => {
                if let Some(handles) = app.world().get_resource::<WindowHandles>() {
                    report.left_fullscreen +=
                        leave_fullscreen(handles.iter().map(|(_, w)| &**w as &dyn FullscreenWindow));
                }
            }
            ExitStep::DropSurfaces => {
                for &event in transitions {
                    apply_transition(app, event);
                }
            }
            _ => {}
        }
        for (_, name, hook) in hooks.hooks.iter_mut().filter(|(s, _, _)| *s == step) {
            let th = Instant::now();
            hook(app.world_mut());
            tracing::debug!(
                target: "regia_platform",
                "regia exit: {} / {name}: {:.1} ms",
                step.label(),
                th.elapsed().as_secs_f64() * 1e3
            );
        }
        if step == ExitStep::DropGpuResources {
            let world = app.world_mut();
            kept_windows = Some((
                world.remove_resource::<WindowHandles>(),
                world.remove_resource::<DisplayHandle>(),
            ));
            report.entities_dropped = world.query::<Entity>().iter(world).count();
            // The whole world goes, replaced by an empty one (`World::clear_all` would leave
            // bevy's resource cache pointing at despawned entities). Non-send data drops here,
            // on the event-loop thread.
            drop(std::mem::take(world));
            // The later steps' hooks still read their budgets.
            world.insert_resource(settings.clone());
        }
        let timing = ExitStepTiming {
            step,
            elapsed: t.elapsed(),
            budget: settings.budget(step),
        };
        let ms = timing.elapsed.as_secs_f64() * 1e3;
        if timing.over_budget() {
            tracing::warn!(
                target: "regia_platform",
                "regia exit: {} took {ms:.1} ms, over its {} ms budget; continuing",
                step.label(),
                timing.budget.as_millis()
            );
        } else {
            tracing::info!(target: "regia_platform", "regia exit: {} took {ms:.1} ms", step.label());
        }
        PROGRESS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((step, ms));
        report.steps.push(timing);
    }
    drop(kept_windows);
    report.total = start.elapsed();
    tracing::info!(
        target: "regia_platform",
        "regia exit: done in {:.1} ms ({} fullscreen window(s) restored, {} entities dropped)",
        report.total.as_secs_f64() * 1e3,
        report.left_fullscreen,
        report.entities_dropped
    );
    let data = exit_data_json(&report);
    tracing::info!(
        target: "regia::exit",
        total_ms = report.total.as_secs_f64() * 1e3,
        data = %data,
        "regia exit: {:.0} ms ({} steps)",
        report.total.as_secs_f64() * 1e3,
        report.steps.len()
    );
    for hook in hooks.done.drain(..) {
        hook(&report);
    }
    report
}

/// A last-resort exit timer ([`arm_exit_watchdog`]). Dropping the handle leaves it armed.
#[derive(Debug, Clone)]
pub struct ExitWatchdog {
    disarmed: Arc<AtomicBool>,
}

impl ExitWatchdog {
    /// The watchdog will not fire.
    pub fn disarm(&self) {
        self.disarmed.store(true, Ordering::Release);
    }
}

/// The watchdog thread's wait: `false` when `flag` (disarmed) is set before `deadline`, or during
/// `grace`, which runs once the deadline has passed (the message and a short pause before the
/// exit). A disarm in the grace used to be ignored (audit v033b).
fn watchdog_should_fire(flag: &AtomicBool, deadline: Duration, grace: impl FnOnce()) -> bool {
    // A deadline too large for `Instant` waits in slices until disarmed.
    let until = Instant::now().checked_add(deadline);
    loop {
        if flag.load(Ordering::Acquire) {
            return false;
        }
        let left = until.map_or(Duration::MAX, |u| u.saturating_duration_since(Instant::now()));
        if left.is_zero() {
            break;
        }
        std::thread::sleep(left.min(Duration::from_millis(50)));
    }
    grace();
    !flag.load(Ordering::Acquire)
}

/// Ends the process with `code` after `deadline` unless disarmed, without running exit handlers
/// (`_exit` on Linux and Android, `TerminateProcess` on Windows): the last resort when a driver
/// blocks in a destructor or an exit handler. Arm it when the quit starts; it keeps running
/// after `main` returns, so it covers the process's exit handlers too.
pub fn arm_exit_watchdog(deadline: Duration, code: i32) -> ExitWatchdog {
    let disarmed = Arc::new(AtomicBool::new(false));
    let flag = disarmed.clone();
    let spawned = std::thread::Builder::new()
        .name("regia-exit-wd".into())
        .spawn(move || {
            let fire = watchdog_should_fire(&flag, deadline, || {
                // The diagnostic goes out on its own thread: a blocked stderr must not stop the exit.
                let _ = std::thread::Builder::new()
                    .name("regia-exit-wd-msg".into())
                    .spawn(move || {
                        eprintln!(
                            "regia exit: watchdog fired after {deadline:?}; ending the process (code {code})"
                        );
                    });
                std::thread::sleep(Duration::from_millis(200));
            });
            if fire {
                crate::platform::exit_immediately(code)
            }
        });
    match spawned {
        Ok(_) => tracing::info!(target: "regia_platform", "regia exit: watchdog armed ({deadline:?})"),
        Err(err) => {
            tracing::warn!(target: "regia_platform", "regia exit: cannot start the watchdog: {err}")
        }
    }
    ExitWatchdog { disarmed }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::sync::Mutex;

    use super::*;
    use crate::lifecycle::AppLifecycle;

    #[test]
    fn an_unrepresentable_watchdog_deadline_does_not_overflow() {
        assert!(Instant::now().checked_add(Duration::MAX).is_none());
        // Arming with it must not panic the watchdog thread; disarming stops it.
        let watchdog = arm_exit_watchdog(Duration::MAX, 0);
        watchdog.disarm();
    }

    type Log = Arc<Mutex<Vec<String>>>;

    fn push(log: &Log, line: &str) {
        log.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(line.to_owned());
    }

    struct MockWindow {
        fullscreen: Cell<bool>,
        log: Log,
    }

    impl FullscreenWindow for MockWindow {
        fn is_fullscreen(&self) -> bool {
            self.fullscreen.get()
        }
        fn leave_fullscreen(&self) {
            self.fullscreen.set(false);
            push(&self.log, "window left fullscreen");
        }
    }

    #[derive(Resource)]
    struct SurfaceLog(Log);

    #[derive(Resource)]
    struct Gpu(#[allow(dead_code)] Arc<()>);

    fn app(log: &Log) -> App {
        let mut app = App::new();
        app.init_resource::<AppLifecycle>()
            .add_message::<LifecycleEvent>()
            .insert_resource(SurfaceLog(log.clone()));
        // A stand-in for the renderer's surface handling.
        app.add_systems(Lifecycle, |l: Res<AppLifecycle>, s: Res<SurfaceLog>| {
            match l.last_event {
                Some(LifecycleEvent::Suspended) => push(&s.0, "surfaces dropped"),
                Some(LifecycleEvent::WillTerminate) => push(&s.0, "will terminate"),
                _ => {}
            }
        });
        app
    }

    #[test]
    fn fullscreen_is_left_before_the_surfaces_drop_and_steps_run_in_order() {
        let log: Log = Arc::default();
        let mut app = app(&log);
        let gpu = Arc::new(());
        app.insert_resource(Gpu(gpu.clone()));
        let device = Arc::downgrade(&gpu);
        drop(gpu);
        let mut hooks = ExitHooks::default();
        for step in ExitStep::ALL {
            let log = log.clone();
            hooks.add(step, "probe", move |_| push(&log, step.label()));
        }
        let window_log = log.clone();
        hooks.add(ExitStep::LeaveFullscreen, "mock window", move |_| {
            let window = MockWindow {
                fullscreen: Cell::new(true),
                log: window_log.clone(),
            };
            assert_eq!(leave_fullscreen([&window as &dyn FullscreenWindow]), 1);
            assert!(!window.is_fullscreen());
        });
        let in_progress = log.clone();
        hooks.add(ExitStep::DropSurfaces, "in progress", move |world| {
            if world.contains_resource::<CleanExitInProgress>() {
                push(&in_progress, "in progress");
            }
        });
        app.insert_resource(hooks);
        app.world_mut().spawn_empty();

        let report = run_clean_exit(
            &mut app,
            &[LifecycleEvent::Suspended, LifecycleEvent::WillTerminate],
        );

        let steps: Vec<ExitStep> = report.steps.iter().map(|t| t.step).collect();
        assert_eq!(steps, ExitStep::ALL);
        let lines = log
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let at = |line: &str| {
            lines
                .iter()
                .position(|l| l == line)
                .unwrap_or_else(|| panic!("{line:?} missing from {lines:?}"))
        };
        assert!(at("window left fullscreen") < at("surfaces dropped"), "{lines:?}");
        assert!(at("stop background work") < at("window left fullscreen"));
        assert!(at("window left fullscreen") < at("wait for GPU idle"));
        assert!(at("wait for GPU idle") < at("surfaces dropped"));
        assert!(at("surfaces dropped") < at("will terminate"));
        assert!(at("will terminate") < at("in progress"));
        assert!(at("in progress") < at("drop renderers and GPU resources"));
        assert!(at("drop device") < at("save caches"));
        assert!(report.entities_dropped >= 1, "{report:?}");
        assert!(device.upgrade().is_none(), "the world's GPU stand-in is dropped");
        assert!(
            !app.world().contains_resource::<SurfaceLog>(),
            "the world is cleared"
        );
    }

    #[test]
    fn a_slow_step_is_logged_and_the_sequence_goes_on() {
        let log: Log = Arc::default();
        let mut app = app(&log);
        app.insert_resource(CleanExitSettings {
            gpu_idle_timeout: Duration::ZERO,
            ..CleanExitSettings::default()
        });
        let mut hooks = ExitHooks::default();
        hooks.add(ExitStep::WaitGpuIdle, "slow", |_| {
            std::thread::sleep(Duration::from_millis(300));
        });
        let after = log.clone();
        hooks.add(ExitStep::SaveCaches, "last", move |world| {
            push(&after, "last step ran");
            let kept = world.get_resource::<CleanExitSettings>();
            assert_eq!(
                kept.map(|s| s.gpu_idle_timeout),
                Some(Duration::ZERO),
                "the host's settings survive the clear"
            );
        });
        app.insert_resource(hooks);
        let report = run_clean_exit(&mut app, &[LifecycleEvent::WillTerminate]);
        let idle = report.steps[2];
        assert_eq!(idle.step, ExitStep::WaitGpuIdle);
        assert!(idle.over_budget(), "{idle:?}");
        assert_eq!(report.steps.len(), ExitStep::ALL.len());
        assert!(
            log.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .any(|l| l == "last step ran")
        );
    }

    #[test]
    fn leave_fullscreen_touches_only_fullscreen_windows() {
        let log: Log = Arc::default();
        let windowed = MockWindow {
            fullscreen: Cell::new(false),
            log: log.clone(),
        };
        let full = MockWindow {
            fullscreen: Cell::new(true),
            log: log.clone(),
        };
        assert_eq!(
            leave_fullscreen([&windowed as &dyn FullscreenWindow, &full as &dyn FullscreenWindow]),
            1
        );
        assert!(!full.is_fullscreen());
        assert_eq!(log.lock().map(|l| l.len()).unwrap_or(0), 1);
    }

    #[test]
    fn a_disarmed_watchdog_never_fires() {
        let wd = arm_exit_watchdog(Duration::from_millis(100), 7);
        wd.disarm();
        // Wait well past the deadline plus the 200 ms grace (audit-v035-flaky M5: 300 ms ended right
        // at the fire time, so a late-starting watchdog thread fired during a later test instead).
        // A broken disarm ends this test binary with code 7 here, inside this test.
        std::thread::sleep(Duration::from_millis(1500));
        // Still here: the process was not ended.
    }

    /// Audit v033b: a disarm after the deadline but inside the 200 ms grace before the exit was
    /// ignored. The grace here disarms; the watchdog must not fire.
    #[test]
    fn a_disarm_in_the_grace_still_wins() {
        let flag = AtomicBool::new(false);
        assert!(!watchdog_should_fire(&flag, Duration::ZERO, || flag.store(true, Ordering::Release)));
        let armed = AtomicBool::new(false);
        assert!(
            watchdog_should_fire(&armed, Duration::ZERO, || {}),
            "no disarm: it fires"
        );
        let disarmed = AtomicBool::new(true);
        assert!(
            !watchdog_should_fire(&disarmed, Duration::MAX, || {}),
            "disarmed before the deadline"
        );
    }

    #[test]
    fn the_exit_event_data_is_json_with_every_step() {
        let report = CleanExitReport {
            steps: ExitStep::ALL
                .iter()
                .map(|&step| ExitStepTiming {
                    step,
                    elapsed: Duration::from_millis(12),
                    budget: Duration::from_millis(10),
                })
                .collect(),
            left_fullscreen: 1,
            entities_dropped: 5,
            total: Duration::from_millis(90),
        };
        let text = exit_data_json(&report);
        assert!(text.starts_with("{\"total_ms\":90.0"), "{text}");
        assert_eq!(text.matches("\"over_budget\":true").count(), 7, "{text}");
        assert!(text.contains("\"step\":\"drop_device\""), "{text}");
    }

    #[test]
    fn request_clean_exit_writes_app_exit() {
        let mut app = App::new();
        request_clean_exit(app.world_mut());
        assert_eq!(app.should_exit(), Some(AppExit::Success));
    }
}
