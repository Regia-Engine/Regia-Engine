// SPDX-License-Identifier: MPL-2.0
//! The v0.1.14 live signals (DO27 asks 2 and 1, step S0): the newest frame's render scale, GPU
//! time, CPU-versus-GPU bound class and the main thread's waits on the GPU ([`LiveFrame`]), and
//! their summary-window aggregates (`render_scale_permille`, `frame_bound`, `wait_*_ms`).
//!
//! Measurement only. Nothing is persisted: the render scale here is what the renderer drew, not a
//! setting.

use regia_render::{FrameExtent, FrameWaits};
use serde::Serialize;
use serde_json::{Value, json};

/// A frame whose GPU time is at least this share of its interval is [`FrameBound::GpuBound`].
pub const GPU_BOUND_SHARE: f32 = 0.90;
/// A frame whose GPU time is at most this share of its interval is [`FrameBound::CpuBound`]: the
/// GPU idles a quarter of the frame or more, so something else sets the pace.
pub const CPU_BOUND_SHARE: f32 = 0.75;
/// An interval with at least this share in measured event sleep is wait-dominated, not evidence
/// of CPU saturation. Small normal redraw waits do not suppress timing-share classification.
pub const EVENT_WAIT_SHARE: f32 = 0.50;

/// What limits the frame rate, from the GPU frame time against the frame interval (wall).
///
/// `gpu_ms / frame_ms` at or over [`GPU_BOUND_SHARE`] is GPU-bound (a lower render scale helps);
/// at or under [`CPU_BOUND_SHARE`] it is CPU-bound (the GPU waits: main-thread work, a
/// compositor or vsync gate, or a frame cap; a lower render scale does not help); in between is
/// balanced. Without GPU timing it is unknown.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameBound {
    /// No GPU timing (timestamp queries off or unsupported) or no frame yet.
    #[default]
    Unknown,
    /// The GPU idles over a quarter of the frame.
    CpuBound,
    /// The GPU is busy for most of the frame.
    GpuBound,
    /// Neither share threshold is crossed.
    Balanced,
}

impl FrameBound {
    /// Classifies a frame of interval `frame_ms` with GPU time `gpu_ms` (both ms; `gpu_ms` 0 when
    /// the GPU is not timed).
    #[must_use]
    pub fn classify(frame_ms: f32, gpu_ms: f32) -> Self {
        if !(frame_ms > 0.0 && gpu_ms > 0.0) {
            return Self::Unknown;
        }
        let share = gpu_ms / frame_ms;
        if share >= GPU_BOUND_SHARE {
            Self::GpuBound
        } else if share <= CPU_BOUND_SHARE {
            Self::CpuBound
        } else {
            Self::Balanced
        }
    }

    /// Stable name: `unknown`, `cpu_bound`, `gpu_bound`, `balanced`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::CpuBound => "cpu_bound",
            Self::GpuBound => "gpu_bound",
            Self::Balanced => "balanced",
        }
    }
}

/// Why a frame's bound class is [`FrameBound::Unknown`] (v0.5.1, DO27 telemetry-hitch-floor):
/// load frames said `unknown` with no reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownBound {
    /// GPU timing is off: no renderer, timestamp queries unsupported, or timing not turned on.
    NoGpuTiming,
    /// A scene is loading: the newest phase marker is a load start (`scene_load_start`, a host
    /// marker named `loading` or ending in `_load_start`).
    Loading,
    /// The window drew no scene: UI or a splash only, or a game-frame warm has the renderer.
    NoScene,
    /// Timing is on and a scene drew, but its GPU sample has not been read back yet (two frames
    /// late), or the readback missed.
    NoGpuSample,
    /// The runner reports a hidden, stalled or surface-refused window.
    Hidden,
    /// No new renderer CPU sample exists for this update tick.
    RendererIdle,
    /// Measured event sleep accounts for at least [`EVENT_WAIT_SHARE`] of the interval.
    EventWait,
}

impl UnknownBound {
    /// Every reason, in index order.
    pub const ALL: [Self; 7] = [
        Self::NoGpuTiming,
        Self::Loading,
        Self::NoScene,
        Self::NoGpuSample,
        Self::Hidden,
        Self::RendererIdle,
        Self::EventWait,
    ];

    /// Stable snake-case name, including `hidden`, `renderer_idle` and `event_wait`.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::NoGpuTiming => "no_gpu_timing",
            Self::Loading => "loading",
            Self::NoScene => "no_scene",
            Self::NoGpuSample => "no_gpu_sample",
            Self::Hidden => "hidden",
            Self::RendererIdle => "renderer_idle",
            Self::EventWait => "event_wait",
        }
    }

    /// The reason for a frame without a GPU sample, first match wins: GPU timing off
    /// (`timing_on` false), a load in progress (`loading`), no scene drawn (`warming`: a warm
    /// owns the renderer, or `scene_drawn` false), else the sample is still pending.
    #[must_use]
    pub fn classify(timing_on: bool, loading: bool, warming: bool, scene_drawn: bool) -> Self {
        if !timing_on {
            Self::NoGpuTiming
        } else if loading {
            Self::Loading
        } else if warming || !scene_drawn {
            Self::NoScene
        } else {
            Self::NoGpuSample
        }
    }
}

/// Whether a phase marker name means a scene is loading ([`UnknownBound::Loading`]).
#[must_use]
pub fn is_loading_phase(name: &str) -> bool {
    name == "loading" || name.starts_with("loading_") || name.ends_with("_load_start")
}

/// The render scale the renderer drew at, in permille of the output width (750 = 0.75).
#[must_use]
pub fn render_scale_permille(extent: &FrameExtent) -> u32 {
    let output = u64::from(extent.output.0.max(1));
    let permille = (u64::from(extent.internal.0) * 1000 + output / 2) / output;
    u32::try_from(permille).unwrap_or(u32::MAX)
}

/// The newest frame as telemetry saw it ([`crate::Telemetry::live`], `telemetry.live`). Updated
/// every frame without allocating.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[non_exhaustive]
pub struct LiveFrame {
    /// Optional source render frames, freshness and absence reasons. Absent in older producers.
    pub timing: Option<regia_framework::feedback::RenderTimingSample>,
    /// CPU milliseconds only when this tick consumed a fresh publication; otherwise null.
    pub cpu_sample_ms: Option<f32>,
    /// GPU milliseconds only when this tick consumed a fresh publication; otherwise null.
    pub gpu_sample_ms: Option<f32>,
    /// Completed app frame described by `event_wait_ms`; phases lag the live telemetry tick.
    pub phases_frame: Option<u64>,
    /// Event-loop sleep in the last completed app frame, separate from active CPU work.
    pub event_wait_ms: Option<f32>,
    /// Current runner visibility and configured throttle period; absent for custom loops.
    pub runner: Option<LiveRunnerState>,
    /// Telemetry frame number (0 before the first).
    pub frame: u64,
    /// Frame interval (wall), ms.
    pub frame_ms: f32,
    /// The renderer's CPU time: `FrameRenderer::render_into` only (not the sim, extract,
    /// acquire or present), ms; 0 when no fresh CPU publication exists.
    pub cpu_ms: f32,
    /// Fresh GPU frame time, ms (0 when absent; read back two frames late).
    pub gpu_ms: f32,
    /// Timing-share class when publications are fresh, the runner is visible and event sleep
    /// is less than [`EVENT_WAIT_SHARE`] of the interval; otherwise unknown. Not CPU utilization.
    pub bound: FrameBound,
    /// v0.5.1: why `bound` is [`FrameBound::Unknown`]; `None` when it is known.
    pub bound_reason: Option<UnknownBound>,
    /// Render scale of the renderer's last frame, permille ([`render_scale_permille`]); `None`
    /// before it drew.
    pub render_scale_permille: Option<u32>,
    /// The last window frame's surface acquire, ms ([`FrameWaits::acquire_ms`]).
    pub wait_acquire_ms: f32,
    /// Its present, ms.
    pub wait_present_ms: f32,
    /// Its blocking screenshot readbacks, ms.
    pub wait_readback_ms: f32,
    /// The frame showed the paused-scene hold's kept image (ADR-7890): no scene pass ran, so a
    /// frame-time governor must not read it as a cheap scene frame.
    pub held: bool,
}

/// Serializable view of the runner's current state, not a host visibility heuristic.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct LiveRunnerState {
    /// Primary window focus, false until an OS focus event.
    pub focused: bool,
    /// All windows occluded or zero-sized.
    pub occluded: bool,
    /// Redraw delivery stalled.
    pub stalled: bool,
    /// The OS repeatedly refused every surface texture.
    pub surface_refused: bool,
    /// Configured hidden/frozen/idle period in milliseconds, including update work.
    pub throttle_ms: f32,
}

impl From<regia_app::RunnerState> for LiveRunnerState {
    fn from(s: regia_app::RunnerState) -> Self {
        Self {
            focused: s.focused,
            occluded: s.occluded,
            stalled: s.stalled,
            surface_refused: s.surface_refused,
            throttle_ms: s.throttle_ms,
        }
    }
}

/// Mean and max of one value over a summary window.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct MeanMax {
    sum: f64,
    max: f32,
    n: u64,
}

impl MeanMax {
    pub(crate) fn add(&mut self, v: f32) {
        self.sum += f64::from(v);
        self.max = self.max.max(v);
        self.n += 1;
    }

    pub(crate) fn mean(&self) -> f64 {
        if self.n == 0 {
            0.0
        } else {
            self.sum / self.n as f64
        }
    }

    pub(crate) fn json(&self) -> Value {
        if self.n == 0 {
            return Value::Null;
        }
        json!({ "mean": r2(self.mean()), "max": r2(f64::from(self.max)) })
    }
}

/// The window aggregates of the live signals (preallocated; a frame only adds numbers).
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct LiveWindow {
    acquire: MeanMax,
    present: MeanMax,
    readback: MeanMax,
    waits: MeanMax,
    last_wait_seq: u64,
    scale_sum: u64,
    scale_min: u32,
    scale_max: u32,
    scale_frames: u64,
    bound_frames: [u64; 4],
    unknown_reasons: [u64; 7],
    paired_wall_ms: f64,
    paired_gpu_ms: f64,
}

impl LiveWindow {
    /// Adds a window frame's waits once per publication (`waits.seq` changes); a gap frame's are
    /// skipped.
    pub(crate) fn waits(&mut self, waits: &FrameWaits, gap: bool) {
        if waits.seq == 0 || waits.seq == self.last_wait_seq {
            return;
        }
        self.last_wait_seq = waits.seq;
        if gap {
            return;
        }
        self.acquire.add(waits.acquire_ms);
        self.present.add(waits.present_ms);
        self.readback.add(waits.readback_ms);
        self.waits.add(waits.total_ms());
    }

    /// Adds a (non-gap) frame's render scale and bound class.
    pub(crate) fn frame(&mut self, scale: Option<u32>, bound: FrameBound, reason: Option<UnknownBound>) {
        if let Some(r) = reason {
            self.unknown_reasons[r as usize] += 1;
        }
        if let Some(s) = scale {
            self.scale_min = if self.scale_frames == 0 {
                s
            } else {
                self.scale_min.min(s)
            };
            self.scale_max = self.scale_max.max(s);
            self.scale_sum += u64::from(s);
            self.scale_frames += 1;
        }
        self.bound_frames[bound as usize] += 1;
    }

    /// Only fresh CPU/GPU pairs from visible ticks not dominated by event sleep classify.
    pub(crate) fn pair(&mut self, frame_ms: f32, gpu_ms: f32) {
        self.paired_wall_ms += f64::from(frame_ms);
        self.paired_gpu_ms += f64::from(gpu_ms);
    }

    /// The most frequent reason of the window's unknown-bound frames (the first in
    /// [`UnknownBound::ALL`] order on a tie), `None` without such a frame.
    pub(crate) fn unknown_reason(&self) -> Option<UnknownBound> {
        let (i, n) = self
            .unknown_reasons
            .iter()
            .copied()
            .enumerate()
            .fold((0, 0), |best, (i, n)| if n > best.1 { (i, n) } else { best });
        (n > 0).then_some(UnknownBound::ALL[i])
    }

    /// The summary fields, then a reset (the wait publication counter is kept).
    /// The bound ratio includes only the eligible pairs accumulated by `pair`.
    pub(crate) fn take_json(&mut self, live: &LiveFrame) -> [(&'static str, Value); 7] {
        let scale = if self.scale_frames == 0 {
            Value::Null
        } else {
            json!({
                "last": live.render_scale_permille,
                "mean": r2(self.scale_sum as f64 / self.scale_frames as f64),
                "min": self.scale_min,
                "max": self.scale_max,
            })
        };
        let counted: u64 = self.bound_frames.iter().sum();
        let bound = json!({
            "class": FrameBound::classify(self.paired_wall_ms as f32, self.paired_gpu_ms as f32).name(),
            "gpu_share": if self.paired_wall_ms > 0.0 && self.paired_gpu_ms > 0.0 {
                json!(r2(self.paired_gpu_ms / self.paired_wall_ms))
            } else {
                Value::Null
            },
            "frames": {
                "cpu_bound": self.bound_frames[FrameBound::CpuBound as usize],
                "gpu_bound": self.bound_frames[FrameBound::GpuBound as usize],
                "balanced": self.bound_frames[FrameBound::Balanced as usize],
                "unknown": self.bound_frames[FrameBound::Unknown as usize],
            },
            "counted": counted,
            "unknown_reasons": {
                "no_gpu_timing": self.unknown_reasons[UnknownBound::NoGpuTiming as usize],
                "loading": self.unknown_reasons[UnknownBound::Loading as usize],
                "no_scene": self.unknown_reasons[UnknownBound::NoScene as usize],
                "no_gpu_sample": self.unknown_reasons[UnknownBound::NoGpuSample as usize],
                "hidden": self.unknown_reasons[UnknownBound::Hidden as usize],
                "renderer_idle": self.unknown_reasons[UnknownBound::RendererIdle as usize],
                "event_wait": self.unknown_reasons[UnknownBound::EventWait as usize],
            },
        });
        let out = [
            ("render_scale_permille", scale),
            ("frame_bound", bound),
            ("wait_acquire_ms", self.acquire.json()),
            ("wait_present_ms", self.present.json()),
            ("wait_readback_ms", self.readback.json()),
            ("cpu_wait_on_gpu_ms", self.waits.json()),
            ("live", serde_json::to_value(live).unwrap_or(Value::Null)),
        ];
        *self = Self {
            last_wait_seq: self.last_wait_seq,
            ..Self::default()
        };
        out
    }
}

fn r2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bound_thresholds() {
        assert_eq!(FrameBound::classify(16.0, 15.0), FrameBound::GpuBound);
        assert_eq!(
            FrameBound::classify(16.0, 14.4),
            FrameBound::GpuBound,
            "exactly 90 %"
        );
        assert_eq!(FrameBound::classify(16.0, 13.0), FrameBound::Balanced);
        assert_eq!(
            FrameBound::classify(16.0, 12.0),
            FrameBound::CpuBound,
            "exactly 75 %"
        );
        // DO27 10-16: a 59 ms frame with a 30 ms GPU is not GPU-bound.
        assert_eq!(FrameBound::classify(59.0, 30.0), FrameBound::CpuBound);
        assert_eq!(FrameBound::classify(16.0, 0.0), FrameBound::Unknown);
        assert_eq!(FrameBound::classify(0.0, 5.0), FrameBound::Unknown);
        assert_eq!(FrameBound::GpuBound.name(), "gpu_bound");
        assert_eq!(
            serde_json::to_value(FrameBound::CpuBound).ok(),
            Some(json!("cpu_bound"))
        );
    }

    /// v0.5.1: each reason for an unknown bound, first match wins.
    #[test]
    fn unknown_bound_names_its_reason() {
        use UnknownBound as U;
        assert_eq!(
            U::classify(false, true, true, false),
            U::NoGpuTiming,
            "timing off wins"
        );
        assert_eq!(U::classify(true, true, true, false), U::Loading);
        assert_eq!(
            U::classify(true, false, true, true),
            U::NoScene,
            "a warm owns the renderer"
        );
        assert_eq!(U::classify(true, false, false, false), U::NoScene, "UI only");
        assert_eq!(U::classify(true, false, false, true), U::NoGpuSample);
        assert_eq!(
            U::ALL.map(U::name),
            [
                "no_gpu_timing",
                "loading",
                "no_scene",
                "no_gpu_sample",
                "hidden",
                "renderer_idle",
                "event_wait"
            ]
        );
        assert_eq!(serde_json::to_value(U::NoScene).ok(), Some(json!("no_scene")));
        assert!(is_loading_phase("scene_load_start") && is_loading_phase("map_load_start"));
        assert!(is_loading_phase("loading") && is_loading_phase("loading_map"));
        assert!(!is_loading_phase("scene_load_end") && !is_loading_phase("match_start"));
        let mut w = LiveWindow::default();
        assert_eq!(w.unknown_reason(), None);
        w.frame(None, FrameBound::Unknown, Some(U::Loading));
        w.frame(None, FrameBound::Unknown, Some(U::NoScene));
        w.frame(None, FrameBound::Unknown, Some(U::NoScene));
        w.frame(None, FrameBound::CpuBound, None);
        assert_eq!(w.unknown_reason(), Some(U::NoScene));
    }

    #[test]
    fn render_scale_is_internal_over_output_width() {
        let extent = |output: (u32, u32), internal: (u32, u32)| FrameExtent {
            output,
            internal,
            upscaler: "TAAU",
        };
        assert_eq!(render_scale_permille(&extent((2560, 1440), (1920, 1080))), 750);
        assert_eq!(render_scale_permille(&extent((3840, 1600), (3840, 1600))), 1000);
        assert_eq!(render_scale_permille(&extent((1920, 1080), (1286, 724))), 670);
        assert_eq!(render_scale_permille(&extent((0, 0), (0, 0))), 0);
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)] // `FrameWaits` is `#[non_exhaustive]`
    fn window_aggregates_and_resets() {
        let mut w = LiveWindow::default();
        let mut waits = FrameWaits::default();
        waits.acquire_ms = 2.0;
        waits.present_ms = 1.0;
        waits.readback_ms = 0.0;
        waits.seq = 1;
        w.waits(&waits, false);
        w.waits(&waits, false); // same publication: not counted twice
        waits.acquire_ms = 4.0;
        waits.readback_ms = 6.0;
        waits.seq = 2;
        w.waits(&waits, false);
        waits.acquire_ms = 100.0;
        waits.seq = 3;
        w.waits(&waits, true); // a gap
        w.frame(Some(700), FrameBound::CpuBound, None);
        w.frame(Some(800), FrameBound::GpuBound, None);
        w.frame(None, FrameBound::Unknown, Some(UnknownBound::NoScene));
        let live = LiveFrame {
            render_scale_permille: Some(800),
            ..LiveFrame::default()
        };
        w.pair(20.0, 19.0);
        let fields: serde_json::Map<String, Value> = w
            .take_json(&live)
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        assert_eq!(fields["wait_acquire_ms"], json!({ "mean": 3.0, "max": 4.0 }));
        assert_eq!(fields["wait_present_ms"], json!({ "mean": 1.0, "max": 1.0 }));
        assert_eq!(fields["wait_readback_ms"], json!({ "mean": 3.0, "max": 6.0 }));
        assert_eq!(fields["cpu_wait_on_gpu_ms"], json!({ "mean": 7.0, "max": 11.0 }));
        assert_eq!(
            fields["render_scale_permille"],
            json!({ "last": 800, "mean": 750.0, "min": 700, "max": 800 })
        );
        assert_eq!(fields["frame_bound"]["class"], "gpu_bound");
        assert_eq!(fields["frame_bound"]["gpu_share"], 0.95);
        assert_eq!(fields["frame_bound"]["frames"]["cpu_bound"], 1);
        assert_eq!(fields["frame_bound"]["frames"]["unknown"], 1);
        assert_eq!(fields["live"]["render_scale_permille"], 800);
        // The next window starts empty, and an old publication is still not re-counted.
        w.waits(&waits, false);
        let again: serde_json::Map<String, Value> = w
            .take_json(&live)
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v))
            .collect();
        assert_eq!(again["wait_acquire_ms"], Value::Null);
        assert_eq!(again["render_scale_permille"], Value::Null);
        assert_eq!(again["frame_bound"]["class"], "unknown");
    }
}
