// SPDX-License-Identifier: MPL-2.0
//! v0.5.1 hitch truth (DO27 tester items telemetry-hitch-floor and focus-guard-fullscreen,
//! ADR-7898): the pacing floor, swapchain-wait frames (`present_wait`), the frame interval's
//! measured and unaccounted time, and a hitch cause taken from frame-thread work only.
//!
//! The frame interval telemetry times (`RealTime::raw_delta`, read in `Last`) runs from `First`
//! of the previous update to `First` of this one. [`regia_app::FramePhases`] times each update
//! from the end of the one before. So the interval is the previous update's schedules after
//! `First` plus this update's time between updates and its `First`: [`Interval`] adds exactly
//! those, and whatever is left (`unaccounted_ms`) ran inside no phase the engine measures.

use regia_app::{FramePhases, PHASE_COUNT, PHASE_NAMES, Phase};
use regia_framework::feedback::FrameSample;
use serde_json::{Value, json};

/// A hitch whose unaccounted time is at least this share of the frame is caused by
/// `outside_frame`: the process was not running engine code (the OS, a window-mode change, a
/// driver call outside every phase, the thread not scheduled).
pub const OUTSIDE_FRAME_SHARE: f32 = 0.5;

/// A frame-thread span names the hitch only when it took at least this share of the frame's
/// excess over the median (a 0.2 ms span does not explain a 700 ms frame).
pub const SPAN_CAUSE_SHARE: f32 = 0.5;

/// Spans that wrap a whole update on the frame thread (`App::update`'s `frame`): never a cause.
const UMBRELLA_SPANS: &[&str] = &["frame"];

/// The pacing period of the active frame cap or vsync refresh, ms; 0 when neither paces the
/// loop. `fps_cap` is `FramePacing::target_fps`; `refresh_hz` counts only with `vsync` on. The
/// slower of the two sets the pace.
#[must_use]
pub fn pacing_period_ms(fps_cap: Option<f32>, vsync: bool, refresh_hz: Option<f32>) -> f32 {
    let period = |hz: f32| {
        if hz.is_finite() && hz > 0.0 {
            1000.0 / hz
        } else {
            0.0
        }
    };
    let cap = fps_cap.map_or(0.0, period);
    let display = if vsync {
        refresh_hz.map_or(0.0, period)
    } else {
        0.0
    };
    cap.max(display)
}

/// The shortest frame that can be a hitch, ms: [`crate::TelemetryOptions::hitch_min_ms`], raised
/// to [`crate::TelemetryOptions::hitch_floor_periods`] × the pacing period when the loop is
/// paced (ADR-7898).
#[must_use]
pub fn hitch_floor_ms(opts: &crate::TelemetryOptions, pacing_period_ms: f32) -> f32 {
    let paced = if pacing_period_ms.is_finite() && pacing_period_ms > 0.0 {
        opts.hitch_floor_periods.max(0.0) * pacing_period_ms
    } else {
        0.0
    };
    opts.hitch_min_ms.max(paced)
}

/// Whether a hitch frame (already over the floor and the median tests) is a swapchain or
/// display wait rather than work: its acquire + present wait (`wait_ms`, this frame's) is at
/// least [`crate::TelemetryOptions::present_wait_share`] of its excess over the median, and the
/// excess stays within [`crate::TelemetryOptions::hitch_abs_ms`] (a longer wait is a stall
/// whatever it waits on, and stays a hitch).
#[must_use]
pub fn is_present_wait(frame_ms: f32, median_ms: f32, wait_ms: f32, opts: &crate::TelemetryOptions) -> bool {
    let excess = frame_ms - median_ms;
    excess > 0.0 && excess <= opts.hitch_abs_ms && wait_ms >= opts.present_wait_share * excess
}

/// The phases before `First`'s end: [`Phase::OutsideUpdate`], [`Phase::HostSchedules`],
/// [`Phase::Pacing`], [`Phase::First`]. The appended [`Phase::EventWait`] also belongs to this
/// interval's head and is handled separately to preserve earlier phase indices.
const HEAD: usize = Phase::First as usize + 1;

/// One hitch frame's interval as the engine measured it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Interval {
    /// The frame interval, ms.
    pub frame_ms: f32,
    /// Phases inside the interval, ms: schedules after `First` from the previous update;
    /// indices `..HEAD` and `EventWait` from this one.
    pub phases: [f32; PHASE_COUNT],
    /// Their sum, ms.
    pub measured_ms: f32,
    /// `frame_ms - measured_ms`, never negative, ms.
    pub unaccounted_ms: f32,
}

impl Interval {
    /// The interval of a frame from the previous update's phases and this update's.
    pub(crate) fn of(frame_ms: f32, previous: &FramePhases, this: &FramePhases) -> Self {
        let mut phases = [0.0; PHASE_COUNT];
        for (i, v) in phases.iter_mut().enumerate() {
            *v = if i < HEAD || i == Phase::EventWait as usize {
                this.ms[i]
            } else {
                previous.ms[i]
            };
        }
        let measured_ms: f32 = phases.iter().sum();
        Self {
            frame_ms,
            phases,
            measured_ms,
            unaccounted_ms: (frame_ms - measured_ms).max(0.0),
        }
    }

    /// The largest measured phase: `(name, "previous" | "this", ms)`.
    fn largest(&self) -> (&'static str, &'static str, f32) {
        let (i, ms) = self
            .phases
            .iter()
            .copied()
            .enumerate()
            .fold(
                (0, f32::MIN),
                |best, (i, v)| if v > best.1 { (i, v) } else { best },
            );
        (
            PHASE_NAMES[i],
            if i < HEAD || i == Phase::EventWait as usize {
                "this"
            } else {
                "previous"
            },
            ms,
        )
    }

    /// The main thread's time in the window event loop before this update, ms.
    pub(crate) fn event_loop_ms(&self) -> f32 {
        self.phases[Phase::OutsideUpdate as usize] + self.phases[Phase::EventWait as usize]
    }

    /// `interval_ms` of the hitch record.
    pub(crate) fn json(&self) -> Value {
        let mut previous = serde_json::Map::new();
        for (i, name) in PHASE_NAMES.iter().enumerate().skip(HEAD) {
            if i == Phase::EventWait as usize {
                continue;
            }
            previous.insert((*name).to_owned(), json!(r2(self.phases[i])));
        }
        let (phase, update, ms) = self.largest();
        json!({
            "frame_ms": r2(self.frame_ms),
            "measured_ms": r2(self.measured_ms),
            "unaccounted_ms": r2(self.unaccounted_ms),
            "event_loop_ms": r2(self.event_loop_ms()),
            "event_wait_ms": r2(self.phases[Phase::EventWait as usize]),
            "host_schedules_ms": r2(self.phases[Phase::HostSchedules as usize]),
            "pacing_ms": r2(self.phases[Phase::Pacing as usize]),
            "first_ms": r2(self.phases[Phase::First as usize]),
            "previous_update": previous,
            "largest": { "phase": phase, "update": update, "ms": r2(ms) },
        })
    }
}

/// What a hitch frame was doing, for its record and `summary.json`'s `hitches.top_causes`:
/// `outside_frame` when unaccounted time dominates; else the creation counters (a pipeline
/// compiled on the frame thread, a blocking poll, a bake, a resource); else the frame thread's
/// largest span when it explains the excess ([`SPAN_CAUSE_SHARE`]); else the largest measured
/// phase (`event_loop`, `pacing`, `host_schedules` or `phase_<name>`); else `unknown`. A
/// worker's span is never the cause (it is listed as `background_spans`).
pub(crate) fn hitch_cause(
    s: &FrameSample,
    median_ms: f32,
    frame_top: Option<(&'static str, f32)>,
    interval: Option<&Interval>,
) -> String {
    if let Some(iv) = interval
        && iv.frame_ms > 0.0
        && iv.unaccounted_ms >= OUTSIDE_FRAME_SHARE * iv.frame_ms
    {
        return "outside_frame".into();
    }
    if s.pipelines > 0 {
        return "pipeline_create".into();
    }
    if s.blocking_polls > 0 {
        return "blocking_poll".into();
    }
    if s.inline_bakes > 0 {
        return "inline_bake".into();
    }
    if s.buffers + s.textures > 0 {
        return "gpu_resource_create".into();
    }
    let excess = (s.frame_ms - median_ms).max(0.0);
    if let Some((name, ms)) = frame_top
        && ms > 0.0
        && ms >= SPAN_CAUSE_SHARE * excess
    {
        return name.to_owned();
    }
    if let Some(iv) = interval {
        let (phase, _, ms) = iv.largest();
        if ms > 0.0 {
            return match phase {
                "outside_update" => "event_loop".into(),
                "pacing" => "pacing".into(),
                "event_wait" => "event_wait".into(),
                "host_schedules" => "host_schedules".into(),
                other => format!("phase_{other}"),
            };
        }
    }
    "unknown".into()
}

/// The frame thread's largest span that is not an umbrella span.
pub(crate) fn frame_top_span(frame: &[(&'static str, std::time::Duration)]) -> Option<(&'static str, f32)> {
    frame
        .iter()
        .find(|(name, _)| !UMBRELLA_SPANS.contains(name))
        .map(|&(name, d)| (name, d.as_secs_f32() * 1000.0))
}

/// A hitch whose cause waits one frame for its update's phases.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PendingCause {
    pub frame: u64,
    pub sample: FrameSample,
    pub median_ms: f32,
    pub frame_top: Option<(&'static str, f32)>,
}

/// Pending causes kept at once (a burst of consecutive hitches resolves one frame later each).
pub(crate) const PENDING_CAUSES: usize = 4;

/// Window events seen by telemetry in one frame (the platform's messages, and the primary
/// window's mode and size compared with the frame before).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowEvents {
    pub resized: u32,
    pub scale_changes: u32,
    pub focus_gained: u32,
    pub focus_lost: u32,
    pub mode_changes: u32,
}

impl WindowEvents {
    pub(crate) fn any(&self) -> bool {
        *self != Self::default()
    }

    pub(crate) fn merged(&self, other: &Self) -> Self {
        Self {
            resized: self.resized + other.resized,
            scale_changes: self.scale_changes + other.scale_changes,
            focus_gained: self.focus_gained + other.focus_gained,
            focus_lost: self.focus_lost + other.focus_lost,
            mode_changes: self.mode_changes + other.mode_changes,
        }
    }

    /// `window_events` of the hitch record: this frame's and the previous frame's events (the
    /// interval spans both), the window's mode and size now.
    pub(crate) fn json(&self, mode: Option<&str>, size: Option<(u32, u32)>, focused: Option<bool>) -> Value {
        json!({
            "resized": self.resized,
            "scale_changes": self.scale_changes,
            "focus_gained": self.focus_gained,
            "focus_lost": self.focus_lost,
            "mode_changes": self.mode_changes,
            "any": self.any(),
            "mode": mode,
            "size": size.map(|(w, h)| [w, h]),
            "focused": focused,
        })
    }
}

fn r2(v: f32) -> f64 {
    (f64::from(v) * 100.0).round() / 100.0
}
