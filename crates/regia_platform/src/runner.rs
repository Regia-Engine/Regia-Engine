// SPDX-License-Identifier: MPL-2.0
//! The winit runner (`docs/specs/11-platform-and-input.md` § 1–2): owns the OS event loop and drives
//! [`App::update`] once per redraw.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use regia_app::prelude::*;
use regia_app::{LoopControl, LoopMode};
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, Ime, MouseScrollDelta, StartCause, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow};
use winit::window::WindowId;

use crate::events::*;
use crate::lifecycle::{AppLifecycle, LifecycleTracker, apply_transition};
use crate::window::{DisplayHandle, PrimaryWindow, Window, WindowHandles, WindowSettings, window_attributes};

/// Runner options, mainly for automated tests.
#[derive(Resource, Debug, Clone, Default)]
pub struct RunnerSettings {
    /// Desktop only: every N frames, run a full suspend → resume cycle through the same code path the
    /// OS uses, to exercise surface teardown and recreation without a device.
    pub simulate_suspend_every: Option<u64>,
}

/// A requested redraw that does not arrive within this time means the compositor stopped sending them
/// (Wayland sends no frame callbacks to windows on hidden workspaces, and often no occlusion event).
const STALL_AFTER: Duration = Duration::from_millis(250);

/// How long a requested redraw may stay undelivered before a FOCUSED window counts as stalled. A focused window
/// is almost always visible, so a missing callback for a quarter second is far more likely a slow frame or a
/// busy compositor than a hidden window (DO27 10-16: a normal focused Hyprland window flapped into the hidden
/// throttle, 54 -> 25 -> 10 fps). Unfocused windows keep [`STALL_AFTER`] (the hidden-workspace case).
const FOCUSED_STALL_AFTER: Duration = Duration::from_secs(2);

/// Whether redraws have stalled: one was requested at `wanted_since` and has not arrived. Time spent inside our own
/// frames is not a stall: the clock starts when the redraw is requested, not when the last frame began.
/// `disabled` (`REGIA_NO_HIDDEN_THROTTLE`) turns the heuristic off; only explicit occlusion hides then.
fn redraw_stalled(wanted_since: Option<Instant>, now: Instant, focused: bool, disabled: bool) -> bool {
    let after = if focused { FOCUSED_STALL_AFTER } else { STALL_AFTER };
    !disabled && wanted_since.is_some_and(|since| now.saturating_duration_since(since) > after)
}

/// Whether a hidden (throttled) window is due a simulation step from `about_to_wait`: a forced frame, or the
/// throttle period has passed since the last step. Other wake-ups (a redraw that already stepped, input events)
/// must not add steps, or the step count depends on how many events the compositor sends.
fn hidden_frame_due(force: bool, since_last: Duration, throttle: Duration) -> bool {
    force || since_last >= throttle
}

/// Winit's wait start excludes our previous callbacks and frame work.
fn event_wait_duration(cause: StartCause, now: Instant) -> Option<Duration> {
    match cause {
        StartCause::WaitCancelled { start, .. } | StartCause::ResumeTimeReached { start, .. } => {
            Some(now.saturating_duration_since(start))
        }
        StartCause::Init | StartCause::Poll => None,
    }
}

fn hidden_deadline(frame_start: Instant, now: Instant, throttle: Duration) -> Instant {
    (frame_start + throttle).max(now)
}

fn hidden_poll_deadline(frame_start: Instant, now: Instant, throttle: Duration) -> Instant {
    hidden_deadline(frame_start, now, throttle).min(now + STALL_AFTER)
}

/// A focused or fullscreen window that has been "hidden" (occluded, or stalled redraws) for this long is
/// presented anyway. The hidden heuristics can be wrong (missing frame callbacks on a fullscreen Xwayland or HDR
/// surface; a stale occlusion event) and a wrong "hidden" on the focused, topmost window freezes the
/// screen while the game keeps running (DO27 10-15: an unfocused fullscreen Wayland window, hidden for 10
/// minutes). A fullscreen window on a workspace that is really not shown may block in present for a while;
/// that is the price, and `Surface` acquire timeouts are already handled by the renderer.
const FOCUSED_HIDDEN_GRACE: Duration = Duration::from_secs(2);

/// Stall / visibility log lines are rate-limited to one per this interval, with a count of the rest.
const VISIBILITY_LOG_EVERY: Duration = Duration::from_secs(5);

/// Whether a window that has been hidden since `hidden_since` must be presented regardless: it is
/// focused or fullscreen (a fullscreen window stays on top of its workspace even without focus, so a
/// wrong "hidden" freezes the screen either way) and the hidden state has outlasted
/// [`FOCUSED_HIDDEN_GRACE`].
fn must_present_anyway(focused: bool, fullscreen: bool, hidden_since: Option<Instant>, now: Instant) -> bool {
    (focused || fullscreen)
        && hidden_since.is_some_and(|since| now.saturating_duration_since(since) >= FOCUSED_HIDDEN_GRACE)
}

/// Whether a loop started in `mode` wants the vsync thread. A vsync callback wakes the process on every
/// display refresh, which is what a [`LoopMode::Lazy`] app exists to avoid: it presents rarely, and its
/// swapchain paces the frames it does present (ADR-2533).
fn wants_display_clock(mode: LoopMode) -> bool {
    mode != LoopMode::Lazy
}

/// True while every window is hidden or redraws have stalled. Frames keep running at a throttled rate
/// (specs/11 § 2: minimise ≈ throttled frames) and the renderer skips presenting (presenting to a hidden
/// Wayland surface can block on vsync).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SurfacesHidden(pub bool);

/// Render ticks in a row from which [`OccludedAcquires::hides`] answers true.
pub const OCCLUDED_ACQUIRES_HIDE: u32 = 3;

/// Render ticks in a row on which the OS refused every window surface's texture because the window is
/// occluded (wgpu `CurrentSurfaceTexture::Occluded`; on macOS the window's occlusion state says "not
/// visible": a locked console, a sleeping display) (ADR-7335).
///
/// The renderer counts them; from [`OCCLUDED_ACQUIRES_HIDE`] on, the runner steps frames at the hidden
/// rate instead of on every redraw. winit reports occlusion only when it changes, so a window created
/// while already occluded never gets a `WindowEvent::Occluded`, and macOS keeps delivering redraws to it
/// at once: before this, such a window ran about 1,500 empty frames a second. [`SurfacesHidden`] stays
/// false meanwhile, so the renderer keeps asking for a texture on each step, and the first one it gets
/// resets the count and ends the throttle.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OccludedAcquires(pub u32);

impl OccludedAcquires {
    /// Notes one render tick: `attempted` window surfaces asked for a texture, `occluded` of them were
    /// refused as occluded, and `acquired` says whether any surface got one. A tick that tried nothing
    /// (no window, a zero-sized one) leaves the count alone.
    pub fn note_tick(&mut self, attempted: u32, occluded: u32, acquired: bool) {
        if acquired {
            self.0 = 0;
        } else if attempted > 0 && occluded >= attempted {
            self.0 = self.0.saturating_add(1);
        } else if attempted > 0 {
            self.0 = 0;
        }
    }

    /// The OS has refused the surfaces as occluded for [`OCCLUDED_ACQUIRES_HIDE`] ticks in a row.
    pub fn hides(self) -> bool {
        self.0 >= OCCLUDED_ACQUIRES_HIDE
    }
}

/// Whether a `RedrawRequested` runs a frame. While the OS refuses the surfaces as occluded
/// ([`OccludedAcquires::hides`]) only one frame per `throttle` runs, the hidden rate; otherwise every
/// redraw does.
fn redraw_runs_frame(refused: bool, since_last: Duration, throttle: Duration) -> bool {
    !refused || hidden_frame_due(false, since_last, throttle)
}

/// The window that has keyboard focus, as the windowing system last reported it (`None` while no window of
/// this app has it). The renderer reads it to decide which windows wait for the compositor (ADR-1750).
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FocusedWindow(pub Option<Entity>);

impl FocusedWindow {
    /// Applies one focus event: a gain names the window, a loss clears it only if that window held the
    /// focus (some compositors send the new window's gain before the old window's loss).
    pub fn update(&mut self, window: Entity, focused: bool) {
        if focused {
            self.0 = Some(window);
        } else if self.0 == Some(window) {
            self.0 = None;
        }
    }

    /// Forgets `window` when it is destroyed while holding the focus.
    pub fn window_destroyed(&mut self, window: Entity) {
        self.update(window, false);
    }
}

/// True when frames are rendered by a software (CPU) adapter: lavapipe, llvmpipe, WARP.
///
/// A plain fact for crates that must not depend on the renderer: `regia_render` publishes it
/// whenever its `RenderDevice` is created or replaced, and the audio output gate's `Auto` throttle
/// reads it (a CPU-bound software renderer starves the realtime audio callback). Absent or `false`
/// means a hardware adapter, or no device yet.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SoftwareRenderer(pub bool);

/// How often the machine slept while the app ran (Steam Deck: the Deck's power button or lid). The
/// runner updates it when [`crate::sleep::SleepDetector`] sees a gap and answers with a synthetic
/// suspend → resume cycle, because winit reports `suspended` only on mobile.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SystemSleepStats {
    /// Sleeps detected.
    pub sleeps: u32,
    /// Total time slept.
    pub total_slept: Duration,
}

/// Frame pacing on the display's vsync, where the platform has a tick source (ADR-1942).
struct DisplayClock {
    pacer: crate::frame_pacing::DisplayPacer,
    thread: crate::platform::VsyncThread,
    /// The rate last suggested to the compositor.
    hint: Option<f32>,
    /// The frame loop's performance hint session (ADR-5461).
    perf: Option<crate::platform::PerfHintSession>,
    /// The kernel id of the frame loop thread (the one that started the clock).
    loop_thread: i32,
}

impl DisplayClock {
    /// Keeps the performance hint session in step with `hint.enabled` and reports a paced frame's
    /// `work` against the slot of `row` (ADR-5461).
    fn drive_perf_hint(
        &mut self,
        hint: &mut crate::frame_pacing::FramePerfHint,
        row: &crate::frame_pacing::PacingTraceRow,
        work: Duration,
    ) {
        if !hint.enabled {
            if self.perf.take().is_some() {
                tracing::info!(target: "regia_platform", "frame pacing: performance hint session closed");
            }
            *hint = crate::frame_pacing::FramePerfHint {
                enabled: false,
                ..Default::default()
            };
            return;
        }
        let Some(target) = crate::frame_pacing::perf_hint_target_ns(row.period_ns, row.interval) else {
            return;
        };
        if !row.paced {
            return;
        }
        let as_i64 = |n: u64| i64::try_from(n).unwrap_or(i64::MAX);
        if self.perf.is_none() && !hint.unavailable {
            let threads: Vec<i32> = std::iter::once(self.loop_thread)
                .chain(self.thread.thread_id())
                .filter(|t| *t > 0)
                .collect();
            self.perf = crate::platform::PerfHintSession::open(&threads, as_i64(target));
            hint.active = self.perf.is_some();
            hint.unavailable = self.perf.is_none();
            hint.target_ns = self.perf.as_ref().map(|_| target);
            if hint.active {
                tracing::info!(
                    target: "regia_platform",
                    "frame pacing: performance hint session for {} threads, target {:.2} ms",
                    threads.len(),
                    target as f64 / 1e6
                );
            } else {
                tracing::info!(target: "regia_platform", "frame pacing: no performance hint session");
            }
        }
        let Some(session) = self.perf.as_mut() else {
            return;
        };
        if crate::frame_pacing::perf_hint_retarget(hint.target_ns, target) {
            if session.set_target(as_i64(target)) {
                hint.target_ns = Some(target);
            } else {
                hint.failures += 1;
            }
        }
        let work = as_i64(u64::try_from(work.as_nanos()).unwrap_or(u64::MAX));
        if session.report(work) {
            hint.reports += 1;
        } else {
            hint.failures += 1;
        }
    }
}

struct WinitRunner {
    app: App,
    sleep: crate::sleep::SleepDetector,
    display_clock: Option<DisplayClock>,
    tracker: LifecycleTracker,
    windows: HashMap<WindowId, Entity>,
    primary: Option<Entity>,
    /// Per window: occluded (winit `Occluded`).
    hidden: HashMap<WindowId, bool>,
    /// Per window: zero-sized (minimised). Kept apart from `hidden` so a resize of a still
    /// occluded window does not clear the occlusion.
    zero_sized: HashMap<WindowId, bool>,
    exit: Option<AppExit>,
    /// Run the next frame directly from `about_to_wait` instead of waiting for `RedrawRequested`.
    /// Set after every resume: recreated surfaces have presented nothing yet, and on Wayland winit only
    /// delivers redraws after a frame callback, which an unmapped surface never gets.
    force_frame: bool,
    /// Since when a redraw has been requested and not delivered (`None`: none outstanding).
    redraw_wanted_since: Option<Instant>,
    /// `REGIA_NO_HIDDEN_THROTTLE=1`: the redraw-stall heuristic is off.
    no_hidden_throttle: bool,
    /// Redraws stopped arriving; running throttled frames until one does.
    stalled: bool,
    /// When the last frame started.
    last_frame: Instant,
    /// An input or window event arrived since the last frame ([`LoopMode::Lazy`] runs a frame).
    dirty: bool,
    /// The primary window has input focus (from `WindowEvent::Focused`).
    focused: bool,
    /// Since when the surfaces have counted as hidden (occluded or stalled); `None` while visible.
    hidden_since: Option<Instant>,
    /// The forced-present override has been logged for this hidden episode.
    forced_logged: bool,
    /// Last time a stall / visibility line was logged, and how many were suppressed since.
    last_visibility_log: Option<Instant>,
    suppressed_visibility_logs: u32,
    /// The OS appearance last reported as `SystemThemeChanged` (true = dark).
    last_dark: Option<bool>,
    /// Reads the OS display density as a scale factor, live (ADR-5379): Android's
    /// `Configuration.densityDpi / 160` through JNI, `None` elsewhere (winit's value stands).
    display_scale: fn() -> Option<f64>,
}

/// The scale factor of an Android density in dpi (`densityDpi / 160`, winit's formula); `None`
/// for a density that is not positive.
#[cfg(any(target_os = "android", test))]
pub(crate) fn scale_from_density_dpi(dpi: i32) -> Option<f64> {
    (dpi > 0).then(|| f64::from(dpi) / 160.0)
}

/// The live OS display scale ([`WinitRunner::display_scale`] outside tests).
fn os_display_scale() -> Option<f64> {
    #[cfg(target_os = "android")]
    {
        crate::platform::system_display_scale()
    }
    #[cfg(not(target_os = "android"))]
    {
        None
    }
}

impl WinitRunner {
    fn transition(&mut self, events: Vec<crate::LifecycleEvent>) {
        for event in events {
            if event == crate::LifecycleEvent::Resumed {
                self.force_frame = true;
                self.redraw_wanted_since = None;
            }
            if let Some(clock) = &mut self.display_clock {
                match event {
                    crate::LifecycleEvent::Resumed => {
                        clock.pacer.resume_ticks();
                        clock.thread.set_active(true);
                        // The new surface has no rate preference yet.
                        clock.hint = None;
                    }
                    crate::LifecycleEvent::Suspended => {
                        clock.thread.set_active(false);
                        clock.pacer.suspend_ticks();
                    }
                    _ => {}
                }
            }
            apply_transition(&mut self.app, event);
        }
    }

    /// Starts the platform's vsync ticks; without a tick source frames stay on the software cap.
    fn start_display_clock(&mut self) {
        if self.display_clock.is_some() || !wants_display_clock(self.loop_control().mode()) {
            return;
        }
        let ticks = crate::frame_pacing::VsyncTicks::new();
        let Some(thread) = crate::platform::start_vsync_thread(ticks.clone()) else {
            return;
        };
        let source = crate::frame_pacing::PacingSource::Choreographer;
        let mut pacer = crate::frame_pacing::DisplayPacer::new(ticks, source, crate::platform::monotonic_ns);
        pacer.set_sched_probe(Some(crate::platform::thread_sched_sample));
        self.display_clock = Some(DisplayClock {
            pacer,
            thread,
            hint: None,
            perf: None,
            loop_thread: crate::platform::current_thread_id(),
        });
    }

    /// Paces the frame that just ran on the display's vsync. `false` leaves it to the software cap.
    fn pace_on_display(&mut self, cost: Duration) -> bool {
        if self.display_clock.is_none() {
            return false;
        }
        let hidden = self.all_hidden() || self.stalled;
        let bench = self.loop_control().mode() == LoopMode::Bench;
        let Some(clock) = &mut self.display_clock else {
            return false;
        };
        let world = self.app.world();
        let settings = world
            .get_resource::<crate::frame_pacing::FramePacingSettings>()
            .copied()
            .unwrap_or_default();
        let mode = settings.mode;
        let cap = world
            .get_resource::<regia_app::FramePacing>()
            .and_then(regia_app::FramePacing::period);
        let swapchain_wait = crate::frame_pacing::take_swapchain_wait().min(cost);
        let wake_ahead = world
            .get_resource::<crate::frame_pacing::FrameWakeAhead>()
            .copied()
            .unwrap_or_default();
        clock.pacer.set_wake_ahead(wake_ahead.lead_ns);
        let steady = world
            .get_resource::<crate::frame_pacing::FrameSteadyStart>()
            .copied()
            .unwrap_or_default();
        clock.pacer.set_grid_release(steady.grid);
        clock.pacer.set_grid_follow(steady.grid_follow);
        clock.pacer.set_start_offset(steady.start_offset_ns);
        clock.pacer.set_drain_run(steady.drain_run);
        let waited = if hidden || bench {
            clock.pacer.skip_frame();
            None
        } else {
            clock.pacer.pace_frame(cost, swapchain_wait, cap, settings)
        };
        let row = clock.pacer.trace_row(cost, swapchain_wait, waited);
        let mut perf_hint = world
            .get_resource::<crate::frame_pacing::FramePerfHint>()
            .copied()
            .unwrap_or_default();
        let perf_before = perf_hint;
        clock.drive_perf_hint(&mut perf_hint, &row, cost.saturating_sub(swapchain_wait));
        let mut report = clock.pacer.report(mode);
        // A rate only means something while frames follow it.
        let hint = clock
            .pacer
            .frame_rate_hint()
            .filter(|_| report.source != crate::frame_pacing::PacingSource::Software);
        // The estimate wobbles by a fraction of a hertz: only a real change reaches the compositor.
        let moved = match (hint, clock.hint) {
            (Some(new), Some(old)) => (new - old).abs() > 1.0,
            (None, None) => false,
            _ => true,
        };
        if moved {
            clock.hint = hint;
            for (_, os) in world.resource::<WindowHandles>().iter() {
                crate::platform::set_frame_rate_hint(os, hint.unwrap_or(0.0));
            }
        }
        report.frame_rate_hint = hint;
        if let Some(mut trace) = self
            .app
            .world_mut()
            .get_resource_mut::<crate::frame_pacing::FramePacingTrace>()
        {
            trace.push(crate::frame_pacing::PacingTraceRow { hint_hz: hint, ..row });
        }
        if let Some(mut slot) = self
            .app
            .world_mut()
            .get_resource_mut::<crate::frame_pacing::FramePacingReport>()
        {
            *slot = report;
        }
        let (polled, predicted) = (clock.pacer.polled_releases(), clock.pacer.predicted_releases());
        if (polled, predicted) != (wake_ahead.polled_releases, wake_ahead.predicted_releases)
            && let Some(mut slot) = self
                .app
                .world_mut()
                .get_resource_mut::<crate::frame_pacing::FrameWakeAhead>()
        {
            slot.polled_releases = polled;
            slot.predicted_releases = predicted;
        }
        if perf_hint != perf_before
            && let Some(mut slot) = self
                .app
                .world_mut()
                .get_resource_mut::<crate::frame_pacing::FramePerfHint>()
        {
            *slot = perf_hint;
        }
        let grid_releases = clock.pacer.grid_releases();
        if grid_releases != steady.grid_releases
            && let Some(mut slot) = self
                .app
                .world_mut()
                .get_resource_mut::<crate::frame_pacing::FrameSteadyStart>()
        {
            slot.grid_releases = grid_releases;
        }
        if let Some(waited) = waited {
            self.app.note_pacing_wait(waited);
        }
        waited.is_some()
    }

    fn create_primary_window(&mut self, event_loop: &ActiveEventLoop) -> bool {
        let settings = self
            .app
            .world()
            .get_resource::<WindowSettings>()
            .cloned()
            .unwrap_or_default();
        let attributes = window_attributes(&settings.window);
        // The screen-reader adapter subclasses the window before it is first shown (ADR-1998).
        #[cfg(feature = "accesskit")]
        let hidden = crate::accessibility::wants_hidden_window(self.app.world());
        #[cfg(feature = "accesskit")]
        let attributes = {
            let visible = attributes.visible && !hidden;
            attributes.with_visible(visible)
        };
        let os_window = match event_loop.create_window(attributes) {
            Ok(w) => Arc::new(w),
            Err(err) => {
                tracing::error!(target: "regia_platform", "cannot create window: {err}");
                return false;
            }
        };
        let size = os_window.inner_size();
        let mut window = settings.window;
        window.physical_size = (size.width, size.height);
        window.scale_factor = os_window.scale_factor();
        let world = self.app.world_mut();
        world.insert_resource(DisplayHandle(event_loop.owned_display_handle()));
        let entity = world
            .spawn((
                window,
                PrimaryWindow,
                Name::new("Primary Window"),
                crate::window::WindowStatus {
                    monitor: os_window.current_monitor().map(|m| crate::window::MonitorExtent {
                        physical_size: (m.size().width, m.size().height),
                        scale_factor: m.scale_factor(),
                    }),
                    ..Default::default()
                },
            ))
            .id();
        world
            .resource_mut::<WindowHandles>()
            .insert(entity, os_window.clone());
        if let Err(error) = crate::alt_menu::install(&os_window) {
            tracing::warn!(target: "regia_platform", %error, "bare-Alt menu suppression unavailable");
        }
        self.windows.insert(os_window.id(), entity);
        self.primary = Some(entity);
        #[cfg(feature = "accesskit")]
        {
            crate::accessibility::attach_to_window(self.app.world_mut(), entity, &os_window);
            if hidden {
                os_window.set_visible(true);
            }
        }
        let dark = os_window
            .theme()
            .map(|theme| theme == winit::window::Theme::Dark)
            .or_else(crate::platform::system_dark_appearance);
        self.report_appearance(entity, dark);
        self.report_text_scale();
        tracing::info!(
            target: "regia_platform",
            "window created: {}x{} physical, scale {:.2}, backend {}, fullscreen {}",
            size.width,
            size.height,
            os_window.scale_factor(),
            display_backend(&os_window),
            os_window.fullscreen().is_some()
        );
        true
    }

    fn frame(&mut self, event_loop: &ActiveEventLoop) {
        if self.tracker.state() != crate::LifecycleState::Running {
            return;
        }
        if let Some(slept) = self.sleep.poll() {
            tracing::info!(
                target: "regia_platform",
                "system woke after {:.1} s asleep: recreating surfaces and audio",
                slept.as_secs_f32()
            );
            self.app.world_mut().insert_resource(SystemSleepStats {
                sleeps: self.sleep.sleeps,
                total_slept: self.sleep.total_slept,
            });
            let events = crate::sleep::wake_events(&mut self.tracker, Some(slept));
            self.transition(events);
            if let Some(mut pacing) = self.app.world_mut().get_resource_mut::<regia_app::FramePacing>() {
                // The deadline chain is stale after a sleep; restart it.
                pacing.wait_for(None);
            }
        }
        // One queued lifecycle drill cycle per frame (`lifecycle.drill`): the surfaces drop and
        // come back through the same transitions an Android home press takes.
        let drill_events = match self.app.world_mut().get_resource_mut::<crate::LifecycleDrill>() {
            Some(mut drill) => crate::lifecycle_drill::next_drill_events(&mut self.tracker, &mut drill),
            None => Vec::new(),
        };
        self.transition(drill_events);
        self.last_frame = Instant::now();
        self.dirty = false;
        let runner = regia_app::RunnerState {
            focused: self.focused,
            occluded: self.all_hidden(),
            stalled: self.stalled,
            surface_refused: self.surfaces_refused(),
            throttle_ms: self.loop_control().throttle_period().as_secs_f32() * 1000.0,
        };
        self.app.world_mut().insert_resource(runner);
        self.app.update();
        // A frozen loop is paced by the event loop's wake-up time (`about_to_wait`), not by sleeping.
        if !self.loop_control().is_frozen() {
            let cost = self.last_frame.elapsed();
            if !self.pace_on_display(cost) {
                let slept = self.app.pace_frame();
                // The display pacer pushed this frame's row already; the cap's sleep is the rest
                // of its time (ADR-4913).
                if self.display_clock.is_some()
                    && let Some(mut trace) = self
                        .app
                        .world_mut()
                        .get_resource_mut::<crate::frame_pacing::FramePacingTrace>()
                {
                    trace.note_cap_wait(slept);
                }
            }
        }
        // A fast exit (ADR-8833) flushes and ends the process here once its frame has come; an
        // `AppExit` in `ExitMode::Fast` becomes one. While one waits, no clean exit starts.
        crate::fast_exit::poll_fast_exit(&mut self.app);
        if crate::fast_exit::fast_exit_requested(self.app.world()) {
            self.set_hidden(true);
            return;
        }
        if let Some(exit) = self.app.should_exit() {
            self.exit = Some(exit);
            event_loop.exit();
            return;
        }
        let every = self
            .app
            .world()
            .get_resource::<RunnerSettings>()
            .and_then(|s| s.simulate_suspend_every);
        let frame = self.app.world().resource::<Time>().frame_count();
        if let Some(n) = every.filter(|n| *n > 0 && frame.is_multiple_of(*n)) {
            tracing::info!(target: "regia_platform", "simulated suspend/resume at frame {frame} (every {n})");
            let suspended = self.tracker.suspended();
            self.transition(suspended);
            let resumed = self.tracker.resumed();
            self.transition(resumed);
        }
    }

    /// The renderer reports that the OS refuses every surface as occluded ([`OccludedAcquires`]).
    fn surfaces_refused(&self) -> bool {
        self.app
            .world()
            .get_resource::<OccludedAcquires>()
            .is_some_and(|o| o.hides())
    }

    fn all_hidden(&self) -> bool {
        !self.windows.is_empty()
            && self.windows.keys().all(|id| {
                self.hidden.get(id).copied().unwrap_or(false)
                    || self.zero_sized.get(id).copied().unwrap_or(false)
            })
    }

    /// Any OS window is fullscreen.
    fn any_fullscreen(&self) -> bool {
        self.app
            .world()
            .resource::<WindowHandles>()
            .iter()
            .any(|(_, w)| w.fullscreen().is_some())
    }

    fn set_hidden(&mut self, hidden: bool) {
        // The fast exit hid the windows: nothing is presented again (a Wayland surface that
        // got a null buffer must not get a new one).
        let hidden = hidden || crate::fast_exit::fast_exit_requested(self.app.world());
        let mut flag = self.app.world_mut().resource_mut::<SurfacesHidden>();
        if flag.0 != hidden {
            flag.0 = hidden;
            self.log_visibility(format_args!("surfaces hidden: {hidden}"));
        }
    }

    /// Log a visibility transition at most once per [`VISIBILITY_LOG_EVERY`]; a compositor that flaps
    /// the state must not turn into thousands of log events a minute.
    fn log_visibility(&mut self, line: std::fmt::Arguments<'_>) {
        let now = Instant::now();
        if self
            .last_visibility_log
            .is_some_and(|last| now.duration_since(last) < VISIBILITY_LOG_EVERY)
        {
            self.suppressed_visibility_logs += 1;
            return;
        }
        let suppressed = std::mem::take(&mut self.suppressed_visibility_logs);
        self.last_visibility_log = Some(now);
        if suppressed > 0 {
            tracing::debug!(target: "regia_platform", "{line} ({suppressed} similar lines suppressed)");
        } else {
            tracing::debug!(target: "regia_platform", "{line}");
        }
    }

    fn request_redraws(&self) {
        for os in self
            .app
            .world()
            .resource::<WindowHandles>()
            .iter()
            .map(|(_, w)| w)
        {
            os.request_redraw();
        }
    }

    /// `about_to_wait` for the frozen, bench and lazy loops ([`LoopControl`]).
    fn run_non_continuous(
        &mut self,
        event_loop: &ActiveEventLoop,
        control: &LoopControl,
        throttle: Duration,
    ) {
        if control.is_frozen() {
            // Frozen: game time is paused; keep the window and the agent protocol alive at the
            // throttled rate.
            if std::mem::take(&mut self.force_frame) || self.last_frame.elapsed() >= throttle {
                self.frame(event_loop);
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.last_frame + throttle));
            return;
        }
        match control.mode() {
            LoopMode::Bench => {
                // Flat out: no redraw round trip, no sleeping.
                self.force_frame = false;
                self.frame(event_loop);
                event_loop.set_control_flow(ControlFlow::Poll);
            }
            _ => {
                // Lazy: a frame on input / window events or a request, else the idle tick.
                let requested = self
                    .app
                    .world_mut()
                    .get_resource_mut::<LoopControl>()
                    .is_some_and(|mut c| c.take_redraw_request());
                if self.dirty
                    || requested
                    || std::mem::take(&mut self.force_frame)
                    || self.last_frame.elapsed() >= throttle
                {
                    self.frame(event_loop);
                }
                event_loop.set_control_flow(ControlFlow::WaitUntil(self.last_frame + throttle));
            }
        }
    }

    fn write<M: Message>(&mut self, message: M) {
        self.app.world_mut().write_message(message);
    }

    /// Android: copies the Settings font size into [`crate::SystemTextScale`] when it changed
    /// (window creation, every resume, every configuration change; ADR-4932). Other platforms keep
    /// the default 1.
    fn report_text_scale(&mut self) {
        #[cfg(target_os = "android")]
        if let Some(scale) = crate::platform::system_font_scale() {
            let world = self.app.world_mut();
            if world.get_resource::<crate::SystemTextScale>().map(|s| s.0) != Some(scale) {
                tracing::info!(target: "regia_platform", "OS text size {scale:.2}");
                world.insert_resource(crate::SystemTextScale(scale));
            }
        }
    }

    /// Re-reads what Android changes without an event of its own (night mode, font size, display
    /// size): after every resume, and when the primary window gains focus (leaving Settings through
    /// the recent-apps switcher or a split screen does not always pause the app).
    fn reread_os_settings(&mut self) {
        if let Some(window) = self.primary {
            self.report_appearance(window, crate::platform::system_dark_appearance());
        }
        self.report_text_scale();
        self.report_display_scale();
    }

    /// Re-reads the OS display density ([`Self::display_scale`]) and applies it to the primary
    /// window when it moved (ADR-5379). winit 0.30.13 on Android takes the scale from the native
    /// configuration it copied at the last `ConfigChanged`, which Settings > Display size and
    /// `wm density` leave at the old density, so its `ScaleFactorChanged` never carries the change.
    fn report_display_scale(&mut self) {
        let Some(window) = self.primary else {
            return;
        };
        let Some(scale) = (self.display_scale)() else {
            return;
        };
        if let Some(current) = self.app.world().get::<Window>(window).map(|w| w.scale_factor)
            && (current - scale).abs() > 1e-6
        {
            tracing::info!(target: "regia_platform", "OS display scale {scale:.3} (was {current:.3})");
            self.set_scale_factor(window, scale);
        }
    }

    /// Stores `scale_factor` on `window` and writes [`WindowScaleFactorChanged`].
    fn set_scale_factor(&mut self, window: Entity, scale_factor: f64) {
        if let Some(mut w) = self.app.world_mut().get_mut::<Window>(window) {
            w.scale_factor = scale_factor;
        }
        self.write(WindowScaleFactorChanged { window, scale_factor });
    }

    /// winit's `ScaleFactorChanged`: on Android every configuration change, carrying a scale that
    /// may be stale, so the live OS density wins when there is one (ADR-5379).
    fn scale_factor_changed(&mut self, window: Entity, winit_scale: f64) {
        let scale = (self.display_scale)()
            .filter(|_| self.primary == Some(window))
            .unwrap_or(winit_scale);
        self.set_scale_factor(window, scale);
        // winit's Android backend reports every configuration change (the manifest keeps
        // `uiMode` changes in-process) as a scale-factor change: re-read the night mode.
        self.report_appearance(window, crate::platform::system_dark_appearance());
        self.report_text_scale();
    }

    /// `WindowEvent::Focused`.
    fn focus_changed(&mut self, window: Entity, focused: bool) {
        if self.primary == Some(window) {
            self.focused = focused;
        }
        self.app
            .world_mut()
            .resource_mut::<FocusedWindow>()
            .update(window, focused);
        self.write(WindowFocused { window, focused });
        if focused && self.primary == Some(window) {
            self.reread_os_settings();
        }
    }

    /// Writes [`SystemThemeChanged`] when the OS appearance is known and differs from the last
    /// report (window creation, every resume, winit's `ThemeChanged`).
    fn report_appearance(&mut self, window: Entity, dark: Option<bool>) {
        if let Some(dark) = dark
            && self.last_dark != Some(dark)
        {
            self.last_dark = Some(dark);
            self.write(SystemThemeChanged { window, dark });
        }
    }

    /// Writes a raw input message unless the loop ignores input ([`LoopMode::Bench`]).
    fn write_input<M: Message>(&mut self, message: M) {
        if self.loop_control().accepts_input() {
            self.write(message);
        }
    }

    fn loop_control(&self) -> LoopControl {
        self.app
            .world()
            .get_resource::<LoopControl>()
            .cloned()
            .unwrap_or_default()
    }
}

fn button(state: ElementState) -> ButtonState {
    match state {
        ElementState::Pressed => ButtonState::Pressed,
        ElementState::Released => ButtonState::Released,
    }
}

fn mouse_button(b: winit::event::MouseButton) -> MouseButton {
    match b {
        winit::event::MouseButton::Left => MouseButton::Left,
        winit::event::MouseButton::Right => MouseButton::Right,
        winit::event::MouseButton::Middle => MouseButton::Middle,
        winit::event::MouseButton::Back => MouseButton::Back,
        winit::event::MouseButton::Forward => MouseButton::Forward,
        winit::event::MouseButton::Other(n) => MouseButton::Other(n),
    }
}

fn physical_key_name(key: winit::keyboard::PhysicalKey) -> String {
    match key {
        winit::keyboard::PhysicalKey::Code(code) => format!("{code:?}"),
        winit::keyboard::PhysicalKey::Unidentified(native) => format!("Unidentified({native:?})"),
    }
}

/// The physical key's name, except that a key winit cannot identify whose logical key is
/// `BrowserBack` (Android's Back, `Unidentified(Android(0x0004))`) is named `BrowserBack`, so a
/// binding or a capture's cancel list can name it (ADR-4932).
fn physical_key_name_for(physical: winit::keyboard::PhysicalKey, logical: &winit::keyboard::Key) -> String {
    if matches!(physical, winit::keyboard::PhysicalKey::Unidentified(_))
        && *logical == winit::keyboard::Key::Named(winit::keyboard::NamedKey::BrowserBack)
    {
        return "BrowserBack".to_owned();
    }
    physical_key_name(physical)
}

fn logical_key_name(key: &winit::keyboard::Key) -> String {
    match key {
        winit::keyboard::Key::Character(text) => text.to_string(),
        winit::keyboard::Key::Named(named) => format!("{named:?}"),
        winit::keyboard::Key::Unidentified(native) => format!("Unidentified({native:?})"),
        winit::keyboard::Key::Dead(c) => format!("Dead({c:?})"),
    }
}

impl ApplicationHandler for WinitRunner {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if let Some(waited) = event_wait_duration(cause, Instant::now()) {
            self.app.record_event_wait(waited);
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.primary.is_none() && !self.create_primary_window(event_loop) {
            self.exit = Some(AppExit::Error(std::num::NonZeroU8::MIN));
            event_loop.exit();
            return;
        }
        self.start_display_clock();
        let events = self.tracker.resumed();
        self.transition(events);
        // Android has no appearance, font size or display size event that winit reports: any of
        // them may have changed while paused.
        self.reread_os_settings();
        event_loop.set_control_flow(ControlFlow::Poll);
    }

    fn suspended(&mut self, event_loop: &ActiveEventLoop) {
        let events = self.tracker.suspended();
        self.transition(events);
        event_loop.set_control_flow(ControlFlow::Wait);
    }

    fn memory_warning(&mut self, _event_loop: &ActiveEventLoop) {
        let events = self.tracker.memory_warning();
        self.transition(events);
    }

    /// Every quit ends here (window close, `AppExit`, the OS ending the app): the clean exit
    /// runs the exit transitions between leaving fullscreen and dropping the device.
    fn exiting(&mut self, _event_loop: &ActiveEventLoop) {
        let events = self.tracker.exiting();
        if events.is_empty() {
            return;
        }
        crate::exit::run_clean_exit(&mut self.app, &events);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        let Some(&window) = self.windows.get(&id) else {
            return;
        };
        // Stamp input as the OS hands it over (winit exposes no OS event time).
        let timestamp = InputTimestamp::now();
        if !matches!(event, WindowEvent::RedrawRequested) {
            self.dirty = true;
        }
        match event {
            WindowEvent::RedrawRequested => {
                tracing::trace!(target: "regia_platform", "redraw requested");
                self.redraw_wanted_since = None;
                if self.stalled {
                    self.stalled = false;
                    self.set_hidden(false);
                }
                let throttle = self.loop_control().throttle_period();
                if !redraw_runs_frame(self.surfaces_refused(), self.last_frame.elapsed(), throttle) {
                    // The OS refuses the surfaces as occluded: `about_to_wait` steps at the hidden rate.
                    return;
                }
                self.frame(event_loop)
            }
            WindowEvent::CloseRequested => self.write(WindowCloseRequested { window }),
            WindowEvent::Resized(size) => {
                if let Some(mut w) = self.app.world_mut().get_mut::<Window>(window) {
                    w.physical_size = (size.width, size.height);
                }
                self.zero_sized.insert(id, size.width == 0 || size.height == 0);
                self.write(WindowResized {
                    window,
                    width: size.width,
                    height: size.height,
                });
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                self.scale_factor_changed(window, scale_factor);
            }
            WindowEvent::Focused(focused) => self.focus_changed(window, focused),
            WindowEvent::ThemeChanged(theme) => {
                self.report_appearance(window, Some(theme == winit::window::Theme::Dark));
            }
            WindowEvent::Occluded(occluded) => {
                self.hidden.insert(id, occluded);
            }
            WindowEvent::DroppedFile(path) => self.write(FileDropped { window, path }),
            WindowEvent::Ime(ime) => match ime {
                Ime::Enabled => self.write_input(ImeEvent::Enabled(window)),
                Ime::Preedit(text, cursor) => {
                    self.write_input(ImeEvent::Preedit(window, text));
                    self.write_input(ImeEvent::PreeditCursor(window, cursor));
                }
                Ime::Commit(text) => self.write_input(ImeEvent::Commit(window, text)),
                Ime::Disabled => self.write_input(ImeEvent::Disabled(window)),
            },
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state == ElementState::Pressed
                    && event.logical_key
                        == winit::keyboard::Key::Named(winit::keyboard::NamedKey::BrowserBack)
                {
                    self.write(BackPressed);
                }
                self.write_input(KeyboardInput {
                    window,
                    physical_key: physical_key_name_for(event.physical_key, &event.logical_key),
                    logical_key: logical_key_name(&event.logical_key),
                    state: button(event.state),
                    repeat: event.repeat,
                    timestamp,
                });
            }
            WindowEvent::MouseInput { state, button: b, .. } => self.write_input(MouseButtonInput {
                window,
                button: mouse_button(b),
                state: button(state),
                timestamp,
            }),
            WindowEvent::CursorMoved { position, .. } => self.write_input(CursorMoved {
                window,
                position: (position.x, position.y),
                timestamp,
            }),
            WindowEvent::MouseWheel { delta, .. } => self.write_input(match delta {
                MouseScrollDelta::LineDelta(x, y) => MouseWheel {
                    window,
                    delta: (x, y),
                    unit: WheelUnit::Line,
                    timestamp,
                },
                MouseScrollDelta::PixelDelta(p) => MouseWheel {
                    window,
                    delta: (p.x as f32, p.y as f32),
                    unit: WheelUnit::Pixel,
                    timestamp,
                },
            }),
            WindowEvent::Touch(touch) => self.write_input(TouchInput {
                window,
                id: touch.id,
                phase: match touch.phase {
                    winit::event::TouchPhase::Started => TouchPhase::Started,
                    winit::event::TouchPhase::Moved => TouchPhase::Moved,
                    winit::event::TouchPhase::Ended => TouchPhase::Ended,
                    winit::event::TouchPhase::Cancelled => TouchPhase::Cancelled,
                },
                position: (touch.location.x, touch.location.y),
                timestamp,
            }),
            WindowEvent::Destroyed => {
                self.windows.remove(&id);
                if self.primary == Some(window) {
                    self.focused = false;
                }
                self.app
                    .world_mut()
                    .resource_mut::<FocusedWindow>()
                    .window_destroyed(window);
                self.app
                    .world_mut()
                    .resource_mut::<WindowHandles>()
                    .remove(window);
            }
            _ => {}
        }
    }

    fn device_event(&mut self, _event_loop: &ActiveEventLoop, _id: DeviceId, event: DeviceEvent) {
        if let DeviceEvent::MouseMotion { delta } = event {
            self.dirty = true;
            self.write_input(MouseMotion {
                delta,
                timestamp: InputTimestamp::now(),
            });
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        if self.tracker.state() != crate::LifecycleState::Running {
            return;
        }
        let control = self.loop_control();
        let throttle = control.throttle_period();
        let continuous =
            !control.is_frozen() && matches!(control.mode(), LoopMode::Continuous | LoopMode::RunFrames(_));
        if !continuous {
            // These modes request no redraws; keep the stall detector from firing on that.
            self.stalled = false;
            self.redraw_wanted_since = None;
        }
        if continuous && self.surfaces_refused() {
            // The OS refuses every surface as occluded (ADR-7335): nothing can present, whatever the
            // window events say, so the focused-window override below does not apply. Step at the
            // hidden rate; `SurfacesHidden` stays false so each step asks for a texture again, and the
            // first one granted ends this. No redraw requests: macOS answers them at once, which spins.
            self.set_hidden(false);
            self.redraw_wanted_since = None;
            let force = std::mem::take(&mut self.force_frame);
            if hidden_frame_due(force, self.last_frame.elapsed(), throttle) {
                self.frame(event_loop);
            }
            event_loop.set_control_flow(ControlFlow::WaitUntil(self.last_frame + throttle));
            return;
        }
        let now = Instant::now();
        let hidden_now = self.all_hidden() || self.stalled;
        if hidden_now {
            self.hidden_since.get_or_insert(now);
        } else {
            self.hidden_since = None;
            self.forced_logged = false;
        }
        if hidden_now && must_present_anyway(self.focused, self.any_fullscreen(), self.hidden_since, now) {
            // The focused window has been "hidden" for seconds: the heuristic is wrong or stale. Present
            // (do not let the screen freeze under a running game), and keep asking for redraws so the
            // compositor's own callbacks take over again when they come back.
            if !self.forced_logged {
                self.forced_logged = true;
                tracing::warn!(
                    target: "regia_platform",
                    "focused/fullscreen window counted as hidden for {FOCUSED_HIDDEN_GRACE:?} (focused: {}, occluded: {}, stalled: {}); presenting anyway",
                    self.focused,
                    self.all_hidden(),
                    self.stalled
                );
            }
            self.set_hidden(false);
            let force = std::mem::take(&mut self.force_frame);
            if hidden_frame_due(force, self.last_frame.elapsed(), throttle) {
                self.frame(event_loop);
            }
            self.request_redraws();
            event_loop.set_control_flow(ControlFlow::WaitUntil(hidden_poll_deadline(
                self.last_frame,
                Instant::now(),
                throttle,
            )));
            return;
        }
        if hidden_now {
            // Hidden: keep simulating at a low rate without presenting; keep asking for redraws so that
            // the first one after the window becomes visible ends the throttled mode.
            self.set_hidden(true);
            let force = std::mem::take(&mut self.force_frame);
            if hidden_frame_due(force, self.last_frame.elapsed(), throttle) {
                self.frame(event_loop);
            }
            self.request_redraws();
            event_loop.set_control_flow(ControlFlow::WaitUntil(hidden_deadline(
                self.last_frame,
                Instant::now(),
                throttle,
            )));
            return;
        }
        self.set_hidden(false);
        if !continuous {
            self.run_non_continuous(event_loop, &control, throttle);
            return;
        }
        if std::mem::take(&mut self.force_frame) {
            self.frame(event_loop);
        }
        self.request_redraws();
        // Stamp after the forced frame: its own run time (a slow first frame after a resume) is
        // not a missing compositor callback.
        let now = Instant::now();
        let wanted_since = *self.redraw_wanted_since.get_or_insert(now);
        if redraw_stalled(Some(wanted_since), now, self.focused, self.no_hidden_throttle) {
            self.stalled = true;
            self.log_visibility(format_args!(
                "no redraw for {:?}; throttling until the window is visible",
                now.saturating_duration_since(wanted_since)
            ));
        }
        // Sleep until the redraw arrives (it wakes the loop) or the stall timeout passes; never spin.
        let after = if self.focused {
            FOCUSED_STALL_AFTER
        } else {
            STALL_AFTER
        };
        event_loop.set_control_flow(ControlFlow::WaitUntil(
            wanted_since + after + Duration::from_millis(1),
        ));
    }
}

/// Which windowing backend winit picked (wayland, xlib, xcb, win32, ...), for logs: on Linux the same
/// binary runs on Wayland natively or on Xwayland, and bug reports need to say which.
fn display_backend(window: &winit::window::Window) -> &'static str {
    use winit::raw_window_handle::{HasDisplayHandle, RawDisplayHandle};
    match window.display_handle().map(|h| h.as_raw()) {
        Ok(RawDisplayHandle::Wayland(_)) => "wayland",
        Ok(RawDisplayHandle::Xlib(_)) => "x11 (xlib)",
        Ok(RawDisplayHandle::Xcb(_)) => "x11 (xcb)",
        Ok(RawDisplayHandle::Windows(_)) => "win32",
        Ok(RawDisplayHandle::Android(_)) => "android",
        Ok(_) => "other",
        Err(_) => "unknown",
    }
}

/// The runner installed by [`crate::PlatformPlugin`].
pub(crate) fn run(mut app: App) -> AppExit {
    app.init_resource::<AppLifecycle>()
        .init_resource::<WindowHandles>();
    let event_loop = match crate::platform::build_event_loop(app.world_mut()) {
        Ok(event_loop) => event_loop,
        Err(err) => {
            tracing::error!(target: "regia_platform", "cannot create the event loop: {err}");
            return AppExit::Error(std::num::NonZeroU8::MIN);
        }
    };
    let mut runner = WinitRunner {
        app,
        sleep: crate::sleep::SleepDetector::default(),
        display_clock: None,
        tracker: LifecycleTracker::default(),
        windows: HashMap::new(),
        primary: None,
        hidden: HashMap::new(),
        zero_sized: HashMap::new(),
        exit: None,
        force_frame: false,
        redraw_wanted_since: None,
        no_hidden_throttle: regia_core::brand::env_var_os("REGIA_NO_HIDDEN_THROTTLE")
            .is_some_and(|v| v != "0"),
        stalled: false,
        last_frame: Instant::now(),
        dirty: false,
        focused: false,
        hidden_since: None,
        forced_logged: false,
        last_visibility_log: None,
        suppressed_visibility_logs: 0,
        last_dark: None,
        display_scale: os_display_scale,
    };
    if let Err(err) = event_loop.run_app(&mut runner) {
        tracing::error!(target: "regia_platform", "event loop error: {err}");
        return AppExit::Error(std::num::NonZeroU8::MIN);
    }
    runner.exit.unwrap_or(AppExit::Success)
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_a_lazy_loop_goes_without_the_vsync_thread() {
        assert!(!wants_display_clock(LoopMode::Lazy));
        for mode in [LoopMode::Continuous, LoopMode::Bench, LoopMode::RunFrames(3)] {
            assert!(wants_display_clock(mode), "{mode:?}");
        }
    }

    use super::*;
    use regia_app::LoopSettings;

    /// From the TEST Pixel 9 Pro (2026-11-09, ADR-4932): Android's Back has no winit key code and
    /// reached the game as `Unidentified(Android(0x0004))`, so a rebind capture bound it and no
    /// cancel list could name it.
    #[test]
    fn the_android_back_key_has_a_name_a_binding_can_use() {
        use winit::keyboard::{Key, KeyCode, NamedKey, NativeKeyCode, PhysicalKey};
        let back = PhysicalKey::Unidentified(NativeKeyCode::Android(4));
        assert_eq!(
            physical_key_name_for(back, &Key::Named(NamedKey::BrowserBack)),
            "BrowserBack"
        );
        // Every other key keeps its name.
        let menu = PhysicalKey::Unidentified(NativeKeyCode::Android(82));
        assert_eq!(
            physical_key_name_for(menu, &Key::Named(NamedKey::ContextMenu)),
            physical_key_name(menu)
        );
        assert_eq!(
            physical_key_name_for(PhysicalKey::Code(KeyCode::KeyW), &Key::Character("w".into())),
            "KeyW"
        );
        assert_eq!(
            physical_key_name_for(
                PhysicalKey::Code(KeyCode::BrowserBack),
                &Key::Named(NamedKey::BrowserBack)
            ),
            "BrowserBack"
        );
    }

    fn runner() -> WinitRunner {
        let mut app = App::new();
        crate::events::register(&mut app);
        app.init_resource::<WindowHandles>()
            .init_resource::<SurfacesHidden>()
            .init_resource::<FocusedWindow>();
        WinitRunner {
            sleep: crate::sleep::SleepDetector::default(),
            display_clock: None,
            app,
            tracker: LifecycleTracker::default(),
            windows: HashMap::new(),
            primary: None,
            hidden: HashMap::new(),
            zero_sized: HashMap::new(),
            exit: None,
            force_frame: false,
            redraw_wanted_since: None,
            no_hidden_throttle: false,
            stalled: false,
            last_frame: Instant::now(),
            dirty: false,
            focused: false,
            hidden_since: None,
            forced_logged: false,
            last_visibility_log: None,
            suppressed_visibility_logs: 0,
            last_dark: None,
            display_scale: no_display_scale,
        }
    }

    fn no_display_scale() -> Option<f64> {
        None
    }

    thread_local! {
        /// The density the fake OS reports to [`fake_display_scale`] (each test runs on its own thread).
        static FAKE_DPI: std::cell::Cell<i32> = const { std::cell::Cell::new(360) };
    }

    fn fake_display_scale() -> Option<f64> {
        scale_from_density_dpi(FAKE_DPI.with(std::cell::Cell::get))
    }

    /// A runner whose OS density reads [`FAKE_DPI`], with a primary window at 360 dpi (2.25), like the
    /// TEST Pixel 9 Pro.
    fn runner_on_a_phone() -> (WinitRunner, Entity) {
        let mut r = runner();
        r.display_scale = fake_display_scale;
        FAKE_DPI.with(|d| d.set(360));
        let window = r
            .app
            .world_mut()
            .spawn((
                Window {
                    scale_factor: 2.25,
                    ..Default::default()
                },
                PrimaryWindow,
            ))
            .id();
        r.primary = Some(window);
        (r, window)
    }

    fn scale_messages(r: &WinitRunner) -> Vec<f64> {
        let messages = r.app.world().resource::<Messages<WindowScaleFactorChanged>>();
        messages
            .iter_current_update_messages()
            .map(|m| m.scale_factor)
            .collect()
    }

    fn window_scale(r: &WinitRunner, window: Entity) -> f64 {
        r.app
            .world()
            .get::<Window>(window)
            .map_or(0.0, |w| w.scale_factor)
    }

    /// From the TEST Pixel 9 Pro (2026-11-09, ADR-4932 row 12; ADR-5379): `wm density 480` left the
    /// window at scale 2.25, because winit 0.30.13 never reports a live density change on Android.
    /// The runner re-reads the density itself on resume and when the window gains focus.
    #[test]
    fn the_runner_rereads_the_display_density_on_resume_and_on_focus() {
        let (mut r, window) = runner_on_a_phone();
        // Nothing moved: no message, the scale stays.
        r.reread_os_settings();
        assert!(scale_messages(&r).is_empty());
        // Settings > Display size while the game was paused: the resume path picks it up.
        FAKE_DPI.with(|d| d.set(480));
        r.reread_os_settings();
        assert_eq!(window_scale(&r, window), 3.0);
        assert_eq!(scale_messages(&r), vec![3.0]);
        // Changed again while the game kept running behind a split screen: focus picks it up.
        FAKE_DPI.with(|d| d.set(360));
        r.focus_changed(window, false);
        assert_eq!(window_scale(&r, window), 3.0, "a focus loss reads nothing");
        r.focus_changed(window, true);
        assert_eq!(window_scale(&r, window), 2.25);
        assert_eq!(scale_messages(&r), vec![3.0, 2.25]);
        // A second focus gain with the same density writes nothing more.
        r.focus_changed(window, true);
        assert_eq!(scale_messages(&r), vec![3.0, 2.25]);
    }

    /// winit's `ScaleFactorChanged` on Android carries the stale density: the live OS reading wins.
    /// With no OS reading (desktop) winit's value stands, as before.
    #[test]
    fn a_stale_winit_scale_factor_loses_to_the_live_os_density() {
        let (mut r, window) = runner_on_a_phone();
        FAKE_DPI.with(|d| d.set(480));
        r.scale_factor_changed(window, 2.25);
        assert_eq!(window_scale(&r, window), 3.0);
        assert_eq!(scale_messages(&r), vec![3.0]);

        let (mut desktop, window) = runner_on_a_phone();
        desktop.display_scale = no_display_scale;
        desktop.scale_factor_changed(window, 1.5);
        assert_eq!(window_scale(&desktop, window), 1.5);
        assert_eq!(scale_messages(&desktop), vec![1.5]);
        desktop.reread_os_settings();
        assert_eq!(
            window_scale(&desktop, window),
            1.5,
            "no OS reading changes nothing"
        );
    }

    #[test]
    fn an_android_density_maps_to_winits_scale_factor() {
        assert_eq!(scale_from_density_dpi(160), Some(1.0));
        assert_eq!(scale_from_density_dpi(360), Some(2.25));
        assert_eq!(scale_from_density_dpi(480), Some(3.0));
        assert_eq!(scale_from_density_dpi(0), None);
        assert_eq!(scale_from_density_dpi(-1), None);
    }

    #[test]
    fn focused_window_follows_gains_and_ignores_a_late_loss() {
        let mut world = World::new();
        let (a, b) = (world.spawn_empty().id(), world.spawn_empty().id());
        let mut focus = FocusedWindow::default();
        focus.update(a, true);
        assert_eq!(focus.0, Some(a));
        // The new window's gain arrives before the old window's loss.
        focus.update(b, true);
        focus.update(a, false);
        assert_eq!(focus.0, Some(b));
        focus.update(b, false);
        assert_eq!(focus.0, None);
    }

    #[test]
    fn a_destroyed_window_gives_up_the_focus_only_if_it_held_it() {
        let mut world = World::new();
        let (a, b) = (world.spawn_empty().id(), world.spawn_empty().id());
        let mut focus = FocusedWindow::default();
        focus.update(a, true);
        focus.window_destroyed(b);
        assert_eq!(focus.0, Some(a));
        focus.window_destroyed(a);
        assert_eq!(focus.0, None);
    }

    fn key() -> KeyboardInput {
        KeyboardInput {
            window: Entity::PLACEHOLDER,
            physical_key: "KeyW".into(),
            logical_key: "w".into(),
            state: ButtonState::Pressed,
            repeat: false,
            timestamp: InputTimestamp::now(),
        }
    }

    #[test]
    fn hidden_wakeups_step_once_per_throttle_period() {
        let throttle = Duration::from_millis(100);
        assert!(!hidden_frame_due(false, Duration::ZERO, throttle));
        assert!(!hidden_frame_due(false, Duration::from_millis(99), throttle));
        assert!(hidden_frame_due(false, throttle, throttle));
        assert!(hidden_frame_due(true, Duration::ZERO, throttle));
    }

    #[test]
    fn focused_or_fullscreen_window_is_presented_after_the_hidden_grace() {
        let t0 = Instant::now();
        let later = t0 + FOCUSED_HIDDEN_GRACE;
        let half = t0 + FOCUSED_HIDDEN_GRACE / 2;
        assert!(
            must_present_anyway(true, false, Some(t0), later),
            "focused, hidden past the grace"
        );
        assert!(
            must_present_anyway(false, true, Some(t0), later),
            "unfocused fullscreen is on top too"
        );
        assert!(
            !must_present_anyway(true, true, Some(t0), half),
            "still inside the grace"
        );
        assert!(
            !must_present_anyway(false, false, Some(t0), later),
            "unfocused windowed stays throttled"
        );
        assert!(
            !must_present_anyway(true, true, None, later),
            "visible windows have nothing to override"
        );
    }

    #[test]
    fn a_slow_frame_is_not_a_stall() {
        let t0 = Instant::now();
        // Nothing requested yet (we were busy in a frame): never stalled, however long the frame took.
        assert!(!redraw_stalled(None, t0 + Duration::from_secs(5), false, false));
        assert!(!redraw_stalled(None, t0 + Duration::from_secs(5), true, false));
    }

    #[test]
    fn an_undelivered_redraw_stalls_an_unfocused_window_after_250_ms() {
        let t0 = Instant::now();
        assert!(!redraw_stalled(
            Some(t0),
            t0 + Duration::from_millis(200),
            false,
            false
        ));
        assert!(redraw_stalled(
            Some(t0),
            t0 + Duration::from_millis(300),
            false,
            false
        ));
    }

    #[test]
    fn a_focused_window_needs_two_seconds_and_the_switch_turns_it_off() {
        let t0 = Instant::now();
        assert!(
            !redraw_stalled(Some(t0), t0 + Duration::from_millis(1900), true, false),
            "a slow focused window is not stalled"
        );
        assert!(redraw_stalled(
            Some(t0),
            t0 + Duration::from_millis(2100),
            true,
            false
        ));
        assert!(
            !redraw_stalled(Some(t0), t0 + Duration::from_secs(60), false, true),
            "REGIA_NO_HIDDEN_THROTTLE disables it"
        );
    }

    #[test]
    fn a_resize_does_not_clear_the_occlusion_of_a_window() {
        let mut runner = runner();
        let id = WindowId::dummy();
        runner.windows.insert(id, Entity::PLACEHOLDER);
        runner.hidden.insert(id, true);
        runner.zero_sized.insert(id, false);
        assert!(runner.all_hidden(), "occluded");
        runner.zero_sized.insert(id, true);
        runner.zero_sized.insert(id, false);
        assert!(runner.all_hidden(), "a non-zero resize leaves the occlusion");
        runner.hidden.insert(id, false);
        assert!(!runner.all_hidden());
        runner.zero_sized.insert(id, true);
        assert!(runner.all_hidden(), "minimised");
    }

    #[test]
    fn visibility_log_is_rate_limited() {
        let mut runner = runner();
        for _ in 0..100 {
            runner.log_visibility(format_args!("surfaces hidden: true"));
        }
        assert_eq!(
            runner.suppressed_visibility_logs, 99,
            "one logged, the rest counted"
        );
    }

    #[test]
    fn bench_mode_ignores_input_but_not_window_events() {
        let mut runner = runner();
        runner.write_input(key());
        assert_eq!(runner.app.world().resource::<Messages<KeyboardInput>>().len(), 1);

        LoopSettings::read(runner.app.world())
            .bench()
            .apply(runner.app.world_mut());
        runner.write_input(key());
        runner.write_input(MouseMotion {
            delta: (1.0, 0.0),
            timestamp: InputTimestamp::now(),
        });
        runner.write(WindowCloseRequested {
            window: Entity::PLACEHOLDER,
        });
        let world = runner.app.world();
        assert_eq!(
            world.resource::<Messages<KeyboardInput>>().len(),
            1,
            "bench drops input"
        );
        assert_eq!(world.resource::<Messages<MouseMotion>>().len(), 0);
        assert_eq!(
            world.resource::<Messages<WindowCloseRequested>>().len(),
            1,
            "close requests still arrive"
        );
    }

    /// M5, 2026-11-12, console locked (ADR-7335): every acquire was refused as occluded and the loop ran
    /// ~1,500 empty frames a second. Three refused ticks in a row hide; one granted texture ends it.
    #[test]
    fn refused_acquires_hide_after_three_ticks_and_one_grant_ends_it() {
        let mut o = OccludedAcquires::default();
        for tick in 1..OCCLUDED_ACQUIRES_HIDE {
            o.note_tick(1, 1, false);
            assert!(!o.hides(), "tick {tick}");
        }
        o.note_tick(1, 1, false);
        assert!(o.hides());
        // A tick that tried no surface (zero-sized window) keeps the state.
        o.note_tick(0, 0, false);
        assert!(o.hides());
        // Two windows, one refused for another reason (outdated): not every surface is occluded.
        o.note_tick(2, 1, false);
        assert_eq!(o, OccludedAcquires(0));
        for _ in 0..OCCLUDED_ACQUIRES_HIDE {
            o.note_tick(2, 2, false);
        }
        assert!(o.hides());
        o.note_tick(2, 1, true);
        assert_eq!(o, OccludedAcquires(0), "a granted texture ends the throttle");
        let mut saturated = OccludedAcquires(u32::MAX);
        saturated.note_tick(1, 1, false);
        assert_eq!(saturated, OccludedAcquires(u32::MAX));
    }

    #[test]
    fn a_refused_window_steps_once_per_throttle_period_on_redraws() {
        let throttle = Duration::from_millis(100);
        assert!(
            redraw_runs_frame(false, Duration::ZERO, throttle),
            "visible: every redraw steps"
        );
        assert!(!redraw_runs_frame(true, Duration::from_millis(1), throttle));
        assert!(!redraw_runs_frame(true, Duration::from_millis(99), throttle));
        assert!(redraw_runs_frame(true, throttle, throttle));
    }

    #[test]
    fn the_runner_reads_refusals_from_the_renderer() {
        let mut r = runner();
        assert!(!r.surfaces_refused(), "no resource: not refused");
        r.app
            .insert_resource(OccludedAcquires(OCCLUDED_ACQUIRES_HIDE - 1));
        assert!(!r.surfaces_refused());
        r.app
            .world_mut()
            .resource_mut::<OccludedAcquires>()
            .note_tick(1, 1, false);
        assert!(r.surfaces_refused());
    }
}

#[cfg(test)]
mod stall_deadline_tests {
    use super::*;

    #[test]
    fn a_long_hidden_period_has_future_poll_deadlines() {
        let start = Instant::now();
        let throttle = Duration::from_secs(1);
        for elapsed in [25, 275, 525, 775] {
            let now = start + Duration::from_millis(elapsed);
            let deadline = hidden_poll_deadline(start, now, throttle);
            assert!(deadline > now);
            assert!(deadline <= start + throttle);
        }
        assert_eq!(
            hidden_poll_deadline(
                start,
                start + Duration::from_millis(25),
                Duration::from_millis(100)
            ),
            start + Duration::from_millis(100)
        );
    }

    #[test]
    fn event_wakeups_count_only_the_wait() {
        let start = Instant::now();
        let deadline = start + Duration::from_millis(75);
        assert_eq!(
            event_wait_duration(
                StartCause::ResumeTimeReached {
                    start,
                    requested_resume: deadline
                },
                deadline
            ),
            Some(Duration::from_millis(75))
        );
        assert_eq!(
            event_wait_duration(
                StartCause::WaitCancelled {
                    start,
                    requested_resume: Some(deadline)
                },
                start + Duration::from_millis(10)
            ),
            Some(Duration::from_millis(10))
        );
        assert_eq!(event_wait_duration(StartCause::Poll, deadline), None);
        assert_eq!(event_wait_duration(StartCause::Init, deadline), None);
        assert_eq!(
            hidden_deadline(
                start,
                start + Duration::from_millis(125),
                Duration::from_millis(100)
            ),
            start + Duration::from_millis(125)
        );
    }

    #[test]
    fn hidden_period_includes_update_work() {
        let start = Instant::now();
        let after_update = start + Duration::from_millis(25);
        let deadline = hidden_deadline(start, after_update, Duration::from_millis(100));
        assert_eq!(deadline.duration_since(start), Duration::from_millis(100));
        assert_eq!(deadline.duration_since(after_update), Duration::from_millis(75));
    }
}
