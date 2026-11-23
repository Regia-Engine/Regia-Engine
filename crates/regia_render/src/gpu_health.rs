// SPDX-License-Identifier: MPL-2.0
//! GPU health, process-wide: a lost device, uncaptured wgpu errors and VRAM pressure (ADR-171).
//!
//! - **Device lost.** wgpu's device-lost callback (or a window surface that stays lost) sets one
//!   flag, [`device_lost`]. The render plugin then stops drawing, runs the
//!   [`crate::DeviceLostHooks`] (the feedback plugin writes a crash bundle with reason
//!   `device_lost`), logs one error and asks the app to exit with [`DEVICE_LOST_EXIT_CODE`]: a
//!   driver reset ends the game with a report instead of a frozen black window. A device
//!   destroyed while the app shuts down ([`note_exiting`]) is not a loss.
//! - **No surface.** A window surface that cannot be created (after one retry) or stays lost
//!   takes the same path with reason [`SURFACE_UNAVAILABLE`] and exit code
//!   [`SURFACE_UNAVAILABLE_EXIT_CODE`]: a game with nothing to present to never keeps running
//!   behind a blank window.
//! - **Uncaptured errors** are counted ([`gpu_error_count`]); `render.problems` carries
//!   [`PROBLEM_GPU_ERRORS`] once there is one, so a release build that logs and continues
//!   (ADR-159) still tells the tester.
//! - **VRAM pressure.** [`signal_pressure`] makes every renderer free its idle GPU memory at its
//!   next frame ([`crate::signal_memory_pressure`]), with a per-source minimum interval. The
//!   first accepted out-of-memory signal steps the texture streaming budget down one notch
//!   (halved, at most [`MAX_BUDGET_STEPS`] times, never below 256 MiB), and so does each later
//!   one within [`BUDGET_STEP_WINDOW`] of the one before (ADR-223: on a small GPU the first
//!   out-of-memory error may be the last one the device survives); the VRAM probe and the OS
//!   low-memory event only trim.
//! - **Pressure that persists.** An out-of-memory signal that arrives within
//!   [`BUDGET_STEP_WINDOW`] of an earlier accepted signal (a trim already ran and memory still
//!   ran out) holds the streamed textures one mip lower for [`MIP_HOLD`]
//!   ([`texture_mips_held`]): every streamed set gives up its top level once, the ones in view
//!   included, never below its floor ([`crate::TextureStreaming::initial_max_px`]), and need-driven
//!   growth stops one level short. When the hold ends, the sets grow back by need.
//! - **Caught out-of-memory errors.** Streamed texture creation runs inside a wgpu
//!   `OutOfMemory` error scope; a failure is counted ([`captured_oom_count`]), signals pressure
//!   like an uncaptured one and never reaches the uncaptured-error handler.
//!
//! The engine installs [`install_device_hooks`] on the device it creates for a window; the
//! telemetry plugin replaces them with its own, which call [`note_uncaptured_error`] and
//! [`note_device_lost`] too.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use regia_core::telemetry::{self as taps, Counter};

use crate::host_checklist::FrameProblem;

/// Process exit code of an app that ended because its GPU device was lost.
pub const DEVICE_LOST_EXIT_CODE: u8 = 3;

/// Process exit code of an app that ended because no window surface could be created or
/// recovered (the adapter cannot present to the window, or a surface stayed lost).
pub const SURFACE_UNAVAILABLE_EXIT_CODE: u8 = 4;

/// [`DeviceLostInfo::reason`] of a window surface that cannot be created or recovered.
pub const SURFACE_UNAVAILABLE: &str = "surface_unavailable";

/// [`FrameProblem::code`]: wgpu reported uncaptured errors since start (validation, internal or
/// out-of-memory). Release builds log them and keep drawing; frames may be wrong.
pub const PROBLEM_GPU_ERRORS: &str = "gpu_errors";

/// [`FrameProblem::code`]: GPU memory ran short; idle GPU memory was freed and the texture
/// streaming budget may have been lowered.
pub const PROBLEM_VRAM_PRESSURE: &str = "vram_pressure";

/// An accepted out-of-memory signal this close after the previous one steps the texture budget
/// down again (the first one always steps); one this close after any accepted signal starts a
/// mip hold ([`texture_mips_held`]).
pub const BUDGET_STEP_WINDOW: Duration = Duration::from_secs(60);

/// How long streamed textures stay held one mip lower after the last out-of-memory signal that
/// found pressure persisting ([`texture_mips_held`]).
pub const MIP_HOLD: Duration = Duration::from_secs(60);

/// The most times the texture streaming budget is halved.
pub const MAX_BUDGET_STEPS: u32 = 3;

/// The texture streaming budget is never stepped below this, MiB.
pub const MIN_STEPPED_BUDGET_MIB: u32 = 256;

/// The share of the adapter's VRAM (percent) above which the telemetry probe signals pressure.
pub const VRAM_PRESSURE_PERCENT: u64 = 85;

/// Device-local VRAM below which the probe never signals, MiB: an APU carve-out or a tiny heap
/// says nothing about what the allocator (which also counts host-visible memory) holds.
pub const VRAM_PRESSURE_MIN_MIB: u64 = 2048;

/// Why a device counts as lost (or the window has nothing to present to).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceLostInfo {
    /// `unknown` (the driver lost it), `destroyed` (destroyed while running) or
    /// [`SURFACE_UNAVAILABLE`] (no window surface could be created, or one stayed lost).
    pub reason: &'static str,
    /// wgpu's or the engine's message.
    pub message: String,
}

impl DeviceLostInfo {
    /// The crash-bundle reason: `surface_unavailable` for [`SURFACE_UNAVAILABLE`], else
    /// `device_lost`.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        if self.reason == SURFACE_UNAVAILABLE {
            SURFACE_UNAVAILABLE
        } else {
            "device_lost"
        }
    }

    /// The process exit code: [`SURFACE_UNAVAILABLE_EXIT_CODE`] or [`DEVICE_LOST_EXIT_CODE`].
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        if self.reason == SURFACE_UNAVAILABLE {
            SURFACE_UNAVAILABLE_EXIT_CODE
        } else {
            DEVICE_LOST_EXIT_CODE
        }
    }
}

/// What asked for memory to be freed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PressureSource {
    /// The OS low-memory event (mobile). No minimum interval; trims only.
    OsLowMemory,
    /// wgpu `Error::OutOfMemory`, uncaptured or caught by the engine's error scopes. At most once
    /// per 5 s; the first, and each within 60 s of the one before, lower the texture budget.
    OutOfMemory,
    /// GPU memory in use above [`VRAM_PRESSURE_PERCENT`] of the adapter's (telemetry probe). At
    /// most once per 30 s; trims only, never lowers the texture budget.
    VramHigh,
}

impl PressureSource {
    /// Stable name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::OsLowMemory => "os_low_memory",
            Self::OutOfMemory => "out_of_memory",
            Self::VramHigh => "vram_high",
        }
    }

    /// The shortest time between two accepted signals from this source.
    #[must_use]
    pub const fn min_interval(self) -> Duration {
        match self {
            Self::OsLowMemory => Duration::ZERO,
            Self::OutOfMemory => Duration::from_secs(5),
            Self::VramHigh => Duration::from_secs(30),
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::OsLowMemory => 0,
            Self::OutOfMemory => 1,
            Self::VramHigh => 2,
        }
    }
}

/// The pressure hysteresis: per-source minimum intervals and the budget step rule.
#[derive(Clone, Debug, Default)]
pub struct PressureGate {
    last_by_source: [Option<Instant>; 3],
    last_oom: Option<Instant>,
    /// The last accepted signal of any source.
    last_accepted: Option<Instant>,
    accepted: u64,
    steps: u32,
    last_source: Option<PressureSource>,
    /// Out-of-memory signals that found pressure persisting (each starts or extends a mip hold).
    persistent: u64,
    /// The mip hold ends here.
    held_until: Option<Instant>,
}

/// What [`PressureGate::offer`] decided.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PressureDecision {
    /// Free idle GPU memory now.
    pub trim: bool,
    /// The texture budget went down a notch.
    pub budget_stepped: bool,
}

impl PressureGate {
    /// Offers a signal from `source` at `now`.
    pub fn offer(&mut self, source: PressureSource, now: Instant) -> PressureDecision {
        let slot = &mut self.last_by_source[source.index()];
        if let Some(last) = *slot
            && now.saturating_duration_since(last) < source.min_interval()
        {
            return PressureDecision {
                trim: false,
                budget_stepped: false,
            };
        }
        *slot = Some(now);
        // Only real allocation failures lower the budget: the VRAM probe and the OS low-memory
        // event trim only (a healthy card keeps its allocator blocks reserved near the top of
        // VRAM; Android sends low-memory events in bursts). The first out-of-memory error steps
        // at once (ADR-223: on a small GPU it may be the last one the device survives); later
        // ones step when they come within the window of the one before.
        let mut budget_stepped = false;
        if source == PressureSource::OutOfMemory {
            let near = |last: Option<Instant>| {
                last.is_some_and(|last| now.saturating_duration_since(last) < BUDGET_STEP_WINDOW)
            };
            budget_stepped =
                (self.last_oom.is_none() || near(self.last_oom)) && self.steps < MAX_BUDGET_STEPS;
            // A trim (or another out-of-memory error) ran shortly before and memory still ran
            // out: idle memory is not enough, so the streamed textures in use give a level too.
            if near(self.last_accepted) {
                self.persistent += 1;
                self.held_until = Some(now + MIP_HOLD);
            }
            self.last_oom = Some(now);
        }
        if budget_stepped {
            self.steps += 1;
        }
        self.last_accepted = Some(now);
        self.accepted += 1;
        self.last_source = Some(source);
        PressureDecision {
            trim: true,
            budget_stepped,
        }
    }

    /// Accepted signals so far.
    #[must_use]
    pub fn accepted(&self) -> u64 {
        self.accepted
    }

    /// Texture budget steps so far.
    #[must_use]
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// Out-of-memory signals that found pressure persisting so far (each one drops a mip of
    /// every streamed set once and starts or extends the mip hold).
    #[must_use]
    pub fn persistent(&self) -> u64 {
        self.persistent
    }

    /// Whether streamed textures are held one mip lower at `now` (within [`MIP_HOLD`] of the
    /// last persistent out-of-memory signal).
    #[must_use]
    pub fn mips_held(&self, now: Instant) -> bool {
        self.held_until.is_some_and(|until| now < until)
    }
}

/// `budget_mib` stepped down `steps` notches (halved each, not below
/// [`MIN_STEPPED_BUDGET_MIB`]); an unlimited budget (`0`) stays unlimited.
#[must_use]
pub fn stepped_budget_mib(budget_mib: u32, steps: u32) -> u32 {
    if budget_mib == 0 || steps == 0 {
        return budget_mib;
    }
    let floor = MIN_STEPPED_BUDGET_MIB.min(budget_mib);
    (budget_mib >> steps.min(31)).max(floor)
}

/// Whether `used` bytes of GPU memory are above [`VRAM_PRESSURE_PERCENT`] of `vram_mib`
/// (never with less than [`VRAM_PRESSURE_MIN_MIB`] of device-local VRAM).
#[must_use]
pub fn vram_over_threshold(used: u64, vram_mib: u64) -> bool {
    vram_mib >= VRAM_PRESSURE_MIN_MIB
        && u128::from(used) * 100 > u128::from(vram_mib) * 1024 * 1024 * u128::from(VRAM_PRESSURE_PERCENT)
}

/// Whether a device-lost callback with `reason` means the game lost its device: always for
/// `Unknown` (a driver reset, a GPU hang) and for `Destroyed` while the app runs; never once the
/// app is shutting down.
#[must_use]
pub fn counts_as_loss(reason: wgpu::DeviceLostReason, exiting: bool) -> bool {
    !exiting
        && matches!(
            reason,
            wgpu::DeviceLostReason::Unknown | wgpu::DeviceLostReason::Destroyed
        )
}

static LOST: AtomicBool = AtomicBool::new(false);
static LOST_INFO: Mutex<Option<DeviceLostInfo>> = Mutex::new(None);
static EXITING: AtomicBool = AtomicBool::new(false);
static OOM_ERRORS: AtomicU64 = AtomicU64::new(0);
static GATE: Mutex<Option<PressureGate>> = Mutex::new(None);
static PRESSURE_SIGNALS: AtomicU64 = AtomicU64::new(0);
static BUDGET_STEPS: AtomicU32 = AtomicU32::new(0);
static CAPTURED_OOMS: AtomicU64 = AtomicU64::new(0);
static MIP_DROPS: AtomicU64 = AtomicU64::new(0);

/// Whether the GPU device is lost: the render plugin stops drawing and the app exits.
#[must_use]
pub fn device_lost() -> bool {
    LOST.load(Ordering::Acquire)
}

/// Longest a worker shutdown (pipeline warm, material graph) waits for a compile in the driver
/// once the device is lost (ADR-276). The process then ends through the deliberate exit, which
/// skips the exit handlers, so a compile on the dead device needs no long wait.
pub(crate) const DEVICE_LOST_WORKER_WAIT: Duration = Duration::from_millis(250);

/// `normal`, or at most [`DEVICE_LOST_WORKER_WAIT`] once the device is lost. Touches no
/// thread-local (the warm queue's exit hook calls it).
pub(crate) fn worker_wait(normal: Duration) -> Duration {
    worker_wait_for(device_lost(), normal)
}

fn worker_wait_for(lost: bool, normal: Duration) -> Duration {
    if lost {
        normal.min(DEVICE_LOST_WORKER_WAIT)
    } else {
        normal
    }
}

/// The run condition form of `!device_lost()`.
#[must_use]
pub fn device_ok() -> bool {
    !device_lost()
}

/// Why the device is lost (`None` while it is not).
#[must_use]
pub fn device_lost_info() -> Option<DeviceLostInfo> {
    LOST_INFO.lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// Marks the device lost (`reason` as in [`DeviceLostInfo::reason`]). The first call wins;
/// returns whether this one set it. Ignored once the app is shutting down.
pub fn mark_device_lost(reason: &'static str, message: &str) -> bool {
    if EXITING.load(Ordering::Acquire) {
        return false;
    }
    let mut info = LOST_INFO.lock().unwrap_or_else(PoisonError::into_inner);
    if info.is_some() {
        return false;
    }
    *info = Some(DeviceLostInfo {
        reason,
        message: message.to_owned(),
    });
    LOST.store(true, Ordering::Release);
    true
}

/// The device-lost callback's part: marks the device lost when [`counts_as_loss`]. Returns
/// whether it did.
pub fn note_device_lost(reason: wgpu::DeviceLostReason, message: &str) -> bool {
    if !counts_as_loss(reason, EXITING.load(Ordering::Acquire)) {
        return false;
    }
    let name = match reason {
        wgpu::DeviceLostReason::Destroyed => "destroyed",
        wgpu::DeviceLostReason::Unknown => "unknown",
    };
    mark_device_lost(name, message)
}

/// The app is shutting down (the render plugin calls this on `WillTerminate`): a device
/// destroyed or lost from here on is not reported as a loss.
pub fn note_exiting() {
    EXITING.store(true, Ordering::Release);
}

/// Writes a feedback bundle for an uncaptured validation or internal error. The framework
/// registers this; render does not depend on it. A later call replaces the hook.
static VALIDATION_BUNDLE_HOOK: Mutex<Option<fn(&'static str)>> = Mutex::new(None);

/// Registers `hook`, called from [`note_uncaptured_error`] for a validation error
/// (`"validation"`) or an internal error (`"internal"`). Out of memory does not call it.
/// A later call replaces the hook. Poisoned locks are recovered.
pub fn set_validation_bundle_hook(hook: fn(&'static str)) {
    *VALIDATION_BUNDLE_HOOK
        .lock()
        .unwrap_or_else(PoisonError::into_inner) = Some(hook);
}

fn validation_error_kind(error: &wgpu::Error) -> Option<&'static str> {
    match error {
        wgpu::Error::Validation { .. } => Some("validation"),
        wgpu::Error::Internal { .. } => Some("internal"),
        wgpu::Error::OutOfMemory { .. } => None,
    }
}

/// Counts one uncaptured wgpu error; an out-of-memory error also signals memory pressure.
/// A validation or internal error runs the hook from [`set_validation_bundle_hook`] when one
/// is registered (spec 54 § 4.1).
pub fn note_uncaptured_error(error: &wgpu::Error) {
    taps::add(Counter::GpuErrors, 1);
    if matches!(error, wgpu::Error::OutOfMemory { .. }) {
        OOM_ERRORS.fetch_add(1, Ordering::Relaxed);
        signal_pressure(PressureSource::OutOfMemory);
        return;
    }
    let Some(kind) = validation_error_kind(error) else {
        return;
    };
    if let Some(hook) = *VALIDATION_BUNDLE_HOOK
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
    {
        hook(kind);
    }
}

/// Uncaptured wgpu errors since the process started.
#[must_use]
pub fn gpu_error_count() -> u64 {
    taps::counter(Counter::GpuErrors)
}

/// Out-of-memory errors among them.
#[must_use]
pub fn gpu_oom_count() -> u64 {
    OOM_ERRORS.load(Ordering::Relaxed)
}

/// Counts one out-of-memory error the engine caught in its own error scope (`what` names the
/// allocation: `streamed texture`) and signals memory pressure, as the uncaptured handler does
/// for an uncaptured one. Not a [`gpu_error_count`] error: the allocation was handled.
pub fn note_captured_oom(what: &str, error: &wgpu::Error) {
    let n = CAPTURED_OOMS.fetch_add(1, Ordering::Relaxed) + 1;
    if signal_pressure(PressureSource::OutOfMemory) || n == 1 {
        tracing::warn!(
            target: "regia_render",
            what,
            caught = n,
            "GPU out of memory creating a {what} (caught; the engine retries smaller and backs off): {error}"
        );
    }
}

/// Out-of-memory errors the engine caught in its own error scopes ([`note_captured_oom`]).
#[must_use]
pub fn captured_oom_count() -> u64 {
    CAPTURED_OOMS.load(Ordering::Relaxed)
}

/// Asks every renderer to free idle GPU memory at its next frame, unless `source` signalled
/// within its [`PressureSource::min_interval`]. Returns whether the signal was accepted.
pub fn signal_pressure(source: PressureSource) -> bool {
    // `PressureDecision` keeps its two fields (hosts may build it); whether this signal found
    // pressure persisting is read off the gate's counter.
    let (decision, mips_dropped) = {
        let mut guard = GATE.lock().unwrap_or_else(PoisonError::into_inner);
        let gate = guard.get_or_insert_with(PressureGate::default);
        let before = gate.persistent();
        let decision = gate.offer(source, Instant::now());
        (decision, gate.persistent() > before)
    };
    if !decision.trim {
        return false;
    }
    PRESSURE_SIGNALS.fetch_add(1, Ordering::Relaxed);
    if mips_dropped {
        MIP_DROPS.fetch_add(1, Ordering::Relaxed);
    }
    crate::signal_memory_pressure();
    if decision.budget_stepped {
        let steps = BUDGET_STEPS.fetch_add(1, Ordering::Relaxed) + 1;
        tracing::warn!(
            target: "regia_render",
            source = source.name(),
            steps,
            mips_held = mips_dropped,
            "GPU out of memory: texture streaming budget halved ({steps} step(s))"
        );
    } else if mips_dropped {
        tracing::warn!(
            target: "regia_render",
            source = source.name(),
            "GPU out of memory again after a trim: streamed textures drop a mip and stay one lower for {} s",
            MIP_HOLD.as_secs()
        );
    } else {
        tracing::warn!(target: "regia_render", source = source.name(), "GPU memory pressure: freeing idle GPU memory");
    }
    true
}

/// Accepted memory-pressure signals since start.
#[must_use]
pub fn memory_pressure_count() -> u64 {
    PRESSURE_SIGNALS.load(Ordering::Relaxed)
}

/// How many notches the texture streaming budget is stepped down.
#[must_use]
pub fn texture_budget_steps() -> u32 {
    BUDGET_STEPS.load(Ordering::Relaxed)
}

/// Out-of-memory signals that found pressure persisting since start: each makes every renderer's
/// streamed sets give up one mip once (the renderers compare it with the value they last saw).
#[must_use]
pub fn texture_mip_drops() -> u64 {
    MIP_DROPS.load(Ordering::Relaxed)
}

/// Whether streamed textures are held one mip lower now: within [`MIP_HOLD`] of the last
/// out-of-memory signal that found pressure persisting. Read once a frame by each renderer.
#[must_use]
pub fn texture_mips_held() -> bool {
    GATE.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .is_some_and(|gate| gate.mips_held(Instant::now()))
}

/// Test hook (cargo feature `test-hooks`): ends the mip hold now instead of after [`MIP_HOLD`].
#[cfg(any(test, feature = "test-hooks"))]
pub fn release_texture_mip_hold() {
    if let Some(gate) = GATE.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
        gate.held_until = None;
    }
}

/// Appends the health problems (`render.problems`) that apply now.
pub(crate) fn push_problems(out: &mut Vec<FrameProblem>) {
    let errors = gpu_error_count();
    if errors > 0 {
        out.push(FrameProblem {
            code: PROBLEM_GPU_ERRORS,
            message: format!(
                "wgpu errors since start: {errors} (out of memory: {}); release builds log them \
                 (regia::gpu_error) and keep drawing, so frames may be wrong",
                gpu_oom_count()
            ),
        });
    }
    let pressure = memory_pressure_count();
    if pressure > 0 {
        out.push(FrameProblem {
            code: PROBLEM_VRAM_PRESSURE,
            message: format!(
                "GPU memory pressure: idle GPU memory freed {pressure} time(s); texture streaming \
                 budget stepped down {} notch(es); {} out-of-memory error(s) caught; streamed \
                 textures held a mip lower: {}",
                texture_budget_steps(),
                captured_oom_count(),
                if texture_mips_held() { "yes" } else { "no" },
            ),
        });
    }
}

/// Installs the engine's own uncaptured-error handler and device-lost callback on `device`
/// (the telemetry plugin replaces both with handlers that also report). Validation and internal
/// errors stay fatal, as wgpu's default handler makes them (not once the device is lost: calls on
/// a dead device fail until the app exits); an out-of-memory error is logged and signals memory
/// pressure instead; a lost device sets [`device_lost`].
pub fn install_device_hooks(device: &wgpu::Device) {
    device.on_uncaptured_error(Arc::new(|error: wgpu::Error| {
        note_uncaptured_error(&error);
        if device_lost() {
            // Calls on a dead device fail until the app has exited; that is expected.
            tracing::debug!(target: "regia_render", "wgpu error after the device was lost: {error}");
        } else if matches!(error, wgpu::Error::OutOfMemory { .. }) {
            tracing::error!(target: "regia_render", "wgpu out-of-memory error: {error}");
        } else {
            tracing::error!(target: "regia_render", "wgpu error: {error}");
            panic!("wgpu error: {error}\n");
        }
    }));
    device.set_device_lost_callback(|reason, message| {
        if note_device_lost(reason, &message) {
            tracing::error!(target: "regia_render", "GPU device lost ({reason:?}): {message}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_waits_are_short_once_the_device_is_lost() {
        let normal = Duration::from_secs(30);
        assert_eq!(worker_wait_for(false, normal), normal);
        assert_eq!(worker_wait_for(true, normal), DEVICE_LOST_WORKER_WAIT);
        let short = Duration::from_millis(20);
        assert_eq!(worker_wait_for(true, short), short, "never longer than asked");
        // The material graph shutdown's GPU idle poll (5 s) is capped the same way.
        let idle = Duration::from_secs(5);
        assert_eq!(worker_wait_for(true, idle), DEVICE_LOST_WORKER_WAIT);
        assert_eq!(worker_wait_for(false, idle), idle);
    }

    #[test]
    fn pressure_sources_keep_their_minimum_interval() {
        let mut gate = PressureGate::default();
        let t0 = Instant::now();
        assert!(gate.offer(PressureSource::VramHigh, t0).trim);
        assert!(
            !gate
                .offer(PressureSource::VramHigh, t0 + Duration::from_secs(29))
                .trim
        );
        // Another source is not held back by the probe's interval.
        assert!(
            gate.offer(PressureSource::OutOfMemory, t0 + Duration::from_secs(29))
                .trim
        );
        assert!(
            !gate
                .offer(PressureSource::OutOfMemory, t0 + Duration::from_secs(33))
                .trim
        );
        assert!(
            gate.offer(PressureSource::VramHigh, t0 + Duration::from_secs(31))
                .trim
        );
        assert_eq!(gate.accepted(), 3);
    }

    /// v0.1.16 (ADR-223) changed the first expectation: the first out-of-memory error now steps
    /// the budget at once. On the Radeon 610M crash (DO27 tester, 2026-10-17) the first one was
    /// also the fatal one, so the old "second within a minute" rule never stepped. The rest is
    /// unchanged: an isolated later error (two minutes after the last) does not step, one 30 s
    /// after the last does, and the steps stay capped.
    #[test]
    fn the_first_oom_steps_then_two_within_a_minute_and_steps_are_capped() {
        let mut gate = PressureGate::default();
        let t0 = Instant::now();
        let first = gate.offer(PressureSource::OutOfMemory, t0);
        assert!(first.trim && first.budget_stepped, "the first OOM steps");
        assert_eq!(gate.steps(), 1);
        let far = gate.offer(PressureSource::OutOfMemory, t0 + Duration::from_secs(120));
        assert!(far.trim && !far.budget_stepped, "two minutes apart does not step");
        let near = gate.offer(PressureSource::OutOfMemory, t0 + Duration::from_secs(150));
        assert!(near.budget_stepped, "30 s after the last OOM steps");
        assert_eq!(gate.steps(), 2);
        for i in 0..10 {
            gate.offer(PressureSource::OutOfMemory, t0 + Duration::from_secs(181 + 6 * i));
        }
        assert_eq!(gate.steps(), MAX_BUDGET_STEPS);
    }

    /// An out-of-memory error shortly after a trim (any source) means idle memory was not
    /// enough: the streamed textures in use are held a mip lower for `MIP_HOLD`, renewed by each
    /// such error. A lone OOM, or one long after the last signal, holds nothing.
    #[test]
    fn an_oom_soon_after_a_trim_holds_the_mips_and_the_hold_ends() {
        let mut gate = PressureGate::default();
        let t0 = Instant::now();
        gate.offer(PressureSource::OutOfMemory, t0);
        assert_eq!(
            gate.persistent(),
            0,
            "a first OOM with no trim before it holds nothing"
        );
        assert!(!gate.mips_held(t0));
        // The probe trims at 100 s; memory still runs out 20 s later.
        gate.offer(PressureSource::VramHigh, t0 + Duration::from_secs(100));
        let again = t0 + Duration::from_secs(120);
        gate.offer(PressureSource::OutOfMemory, again);
        assert_eq!(gate.persistent(), 1);
        assert!(gate.mips_held(again + MIP_HOLD - Duration::from_secs(1)));
        assert!(!gate.mips_held(again + MIP_HOLD), "the hold ends after MIP_HOLD");
        // Probe and OS signals alone never hold.
        let mut probe_only = PressureGate::default();
        for i in 0..10 {
            probe_only.offer(PressureSource::VramHigh, t0 + Duration::from_secs(31 * i));
            probe_only.offer(PressureSource::OsLowMemory, t0 + Duration::from_secs(31 * i));
        }
        assert_eq!(probe_only.persistent(), 0);
        assert!(!probe_only.mips_held(t0 + Duration::from_secs(300)));
    }

    /// Review fix: a healthy card whose allocator stays above 85 % for minutes, and Android's
    /// bursts of low-memory events, trim but never lower the texture budget.
    ///
    /// v0.1.16 (ADR-223): the run now starts with one out-of-memory error, so the "nor do they
    /// pair with a later OOM" check still tests pairing: since the first OOM steps on its own
    /// (see `the_first_oom_steps_then_two_within_a_minute_and_steps_are_capped`), a gate with
    /// no OOM before the probe signals would step on that later OOM whatever the probe did.
    #[test]
    fn probe_and_os_signals_never_step_the_budget() {
        let mut gate = PressureGate::default();
        let t0 = Instant::now();
        assert!(gate.offer(PressureSource::OutOfMemory, t0).budget_stepped);
        assert_eq!(gate.steps(), 1);
        for i in 0..20 {
            let at = t0 + Duration::from_secs(30 * i);
            assert!(gate.offer(PressureSource::VramHigh, at).trim);
            assert!(gate.offer(PressureSource::OsLowMemory, at).trim);
            assert!(
                gate.offer(PressureSource::OsLowMemory, at + Duration::from_millis(10))
                    .trim
            );
        }
        assert_eq!(gate.steps(), 1, "only the OOM stepped");
        // Nor do they pair with a later OOM (the last OOM was ten minutes before).
        let oom = gate.offer(PressureSource::OutOfMemory, t0 + Duration::from_secs(601));
        assert!(oom.trim && !oom.budget_stepped);
        assert_eq!(gate.steps(), 1);
    }

    #[test]
    fn budget_steps_halve_with_a_floor_and_keep_unlimited() {
        assert_eq!(stepped_budget_mib(4096, 0), 4096);
        assert_eq!(stepped_budget_mib(4096, 1), 2048);
        assert_eq!(stepped_budget_mib(4096, 3), 512);
        assert_eq!(stepped_budget_mib(1024, 3), 256);
        assert_eq!(stepped_budget_mib(256, 2), 256);
        assert_eq!(
            stepped_budget_mib(128, 1),
            128,
            "a budget under the floor is not raised"
        );
        assert_eq!(stepped_budget_mib(0, 3), 0, "unlimited stays unlimited");
    }

    #[test]
    fn vram_threshold_is_85_percent() {
        let mib = 1024 * 1024;
        assert!(!vram_over_threshold(8500 * mib, 10_000));
        assert!(vram_over_threshold(8600 * mib, 10_000));
        assert!(!vram_over_threshold(u64::MAX, 0), "unknown VRAM never signals");
        assert!(
            !vram_over_threshold(u64::MAX, 1024),
            "an APU carve-out or a tiny device-local heap never signals"
        );
        assert!(vram_over_threshold(1800 * mib, 2048));
    }

    #[test]
    fn surface_unavailable_has_its_own_kind_and_code() {
        let surface = DeviceLostInfo {
            reason: SURFACE_UNAVAILABLE,
            message: String::new(),
        };
        assert_eq!(surface.kind(), "surface_unavailable");
        assert_eq!(surface.exit_code(), SURFACE_UNAVAILABLE_EXIT_CODE);
        let lost = DeviceLostInfo {
            reason: "unknown",
            message: String::new(),
        };
        assert_eq!(lost.kind(), "device_lost");
        assert_eq!(lost.exit_code(), DEVICE_LOST_EXIT_CODE);
    }

    #[test]
    fn destroyed_at_shutdown_is_not_a_loss() {
        use wgpu::DeviceLostReason as R;
        assert!(counts_as_loss(R::Unknown, false));
        assert!(
            counts_as_loss(R::Destroyed, false),
            "destroyed while running is a loss"
        );
        assert!(!counts_as_loss(R::Destroyed, true), "a normal shutdown is not");
        assert!(!counts_as_loss(R::Unknown, true));
    }
}
