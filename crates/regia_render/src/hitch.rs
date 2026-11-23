// SPDX-License-Identifier: MPL-2.0
//! Hitch metrics (`specs/27` § 2.4): frame-time percentiles against the median, the frames
//! over 2 × p50, the longest frame and the frame-path device calls after warm-up.
//!
//! One implementation serves the DO27 soak (`lookdev::soak`), its soak test and the agent
//! method `play.hitch_report`, so the gate a merge is held to is the number an agent reads.

use crate::FramePathCounters;

/// One measured frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HitchFrame {
    /// Frame interval as presented (the bench's wall time per frame, the app's real delta), ms.
    pub frame_ms: f32,
    /// CPU time inside the renderer, ms (0 when unknown).
    pub cpu_ms: f32,
    /// GPU frame time, ms (0 when timing is off).
    pub gpu_ms: f32,
    /// Frame-path device calls during this frame.
    pub counters: FramePathCounters,
}

/// The § 2.4 targets. [`HitchTargets::SPEC`] is the gate; a ★ value moves when the spec does.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitchTargets {
    /// Largest share of frames over 2 × p50 (0.001 = 0.1 %).
    pub over_2x_fraction: f32,
    /// Largest p99 / p50.
    pub p99_ratio: f32,
    /// Largest p99.9 / p50.
    pub p999_ratio: f32,
    /// Largest longest-frame / p50.
    pub longest_ratio: f32,
    /// No frame may take this long, ms.
    pub max_frame_ms: f32,
}

impl HitchTargets {
    /// `specs/27` § 2.4 as of 2026-10-07.
    pub const SPEC: Self = Self {
        over_2x_fraction: 0.001,
        p99_ratio: 1.3,
        p999_ratio: 2.0,
        longest_ratio: 2.5,
        max_frame_ms: 50.0,
    };
}

impl Default for HitchTargets {
    fn default() -> Self {
        Self::SPEC
    }
}

/// One frame over 2 × p50, with what the frame path did in it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct HitchSpike {
    /// Index into the measured frames.
    pub frame: usize,
    /// The frame's time, ms.
    pub frame_ms: f32,
    /// Its CPU time, ms.
    pub cpu_ms: f32,
    /// Its GPU time, ms.
    pub gpu_ms: f32,
    /// Its device calls.
    pub counters: FramePathCounters,
}

/// The § 2.4 report over a run of measured frames (warm-up already excluded).
#[derive(Clone, Debug, PartialEq)]
pub struct HitchReport {
    /// Measured frames.
    pub frames: usize,
    /// Median frame time, ms.
    pub p50_ms: f32,
    /// 99th percentile, ms.
    pub p99_ms: f32,
    /// 99.9th percentile, ms.
    pub p999_ms: f32,
    /// Longest frame, ms.
    pub longest_ms: f32,
    /// Index of the longest frame.
    pub longest_frame: usize,
    /// Frames over 2 × p50.
    pub over_2x: usize,
    /// Frames over [`HitchTargets::max_frame_ms`].
    pub over_max: usize,
    /// CPU p50 / p99, ms.
    pub cpu_p50_ms: f32,
    /// CPU p99, ms.
    pub cpu_p99_ms: f32,
    /// GPU p50, ms.
    pub gpu_p50_ms: f32,
    /// GPU p99, ms.
    pub gpu_p99_ms: f32,
    /// Device calls summed over the measured frames.
    pub counters: FramePathCounters,
    /// Frames that created a pipeline, bind group, buffer or texture.
    pub frames_with_creates: usize,
    /// The worst spikes (over 2 × p50), longest first, at most 32.
    pub spikes: Vec<HitchSpike>,
    /// The targets the verdict used.
    pub targets: HitchTargets,
}

/// `p`-quantile of sorted `values` (nearest rank; 0 for none).
#[must_use]
pub fn percentile_sorted(values: &[f32], p: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let rank = ((values.len() - 1) as f32 * p.clamp(0.0, 1.0)).round() as usize;
    values[rank.min(values.len() - 1)]
}

fn sorted(frames: &[HitchFrame], f: impl Fn(&HitchFrame) -> f32) -> Vec<f32> {
    let mut v: Vec<f32> = frames.iter().map(f).collect();
    v.sort_by(f32::total_cmp);
    v
}

fn add(a: FramePathCounters, b: FramePathCounters) -> FramePathCounters {
    FramePathCounters {
        pipelines: a.pipelines + b.pipelines,
        bind_groups: a.bind_groups + b.bind_groups,
        blocking_polls: a.blocking_polls + b.blocking_polls,
        submits: a.submits + b.submits,
        map_asyncs: a.map_asyncs + b.map_asyncs,
        buffers: a.buffers + b.buffers,
        buffer_bytes: a.buffer_bytes + b.buffer_bytes,
        textures: a.textures + b.textures,
        texture_bytes: a.texture_bytes + b.texture_bytes,
        upload_bytes: a.upload_bytes + b.upload_bytes,
        inline_bakes: a.inline_bakes + b.inline_bakes,
    }
}

impl HitchReport {
    /// The report over `frames` against `targets`.
    #[must_use]
    pub fn new(frames: &[HitchFrame], targets: HitchTargets) -> Self {
        let wall = sorted(frames, |f| f.frame_ms);
        let cpu = sorted(frames, |f| f.cpu_ms);
        let gpu = sorted(frames, |f| f.gpu_ms);
        let p50 = percentile_sorted(&wall, 0.5);
        let (longest_frame, longest_ms) = frames
            .iter()
            .enumerate()
            .map(|(i, f)| (i, f.frame_ms))
            .max_by(|a, b| a.1.total_cmp(&b.1))
            .unwrap_or((0, 0.0));
        let mut spikes: Vec<HitchSpike> = frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.frame_ms > 2.0 * p50)
            .map(|(frame, f)| HitchSpike {
                frame,
                frame_ms: f.frame_ms,
                cpu_ms: f.cpu_ms,
                gpu_ms: f.gpu_ms,
                counters: f.counters,
            })
            .collect();
        let over_2x = spikes.len();
        spikes.sort_by(|a, b| b.frame_ms.total_cmp(&a.frame_ms));
        spikes.truncate(32);
        let creates = |c: &FramePathCounters| c.pipelines + c.bind_groups + c.buffers + c.textures;
        Self {
            frames: frames.len(),
            p50_ms: p50,
            p99_ms: percentile_sorted(&wall, 0.99),
            p999_ms: percentile_sorted(&wall, 0.999),
            longest_ms,
            longest_frame,
            over_2x,
            over_max: frames
                .iter()
                .filter(|f| f.frame_ms > targets.max_frame_ms)
                .count(),
            cpu_p50_ms: percentile_sorted(&cpu, 0.5),
            cpu_p99_ms: percentile_sorted(&cpu, 0.99),
            gpu_p50_ms: percentile_sorted(&gpu, 0.5),
            gpu_p99_ms: percentile_sorted(&gpu, 0.99),
            counters: frames
                .iter()
                .fold(FramePathCounters::default(), |a, f| add(a, f.counters)),
            frames_with_creates: frames.iter().filter(|f| creates(&f.counters) > 0).count(),
            spikes,
            targets,
        }
    }

    /// The § 2.4 rows this run misses (empty = pass). Pipelines, bind groups and blocking
    /// polls after warm-up must be zero.
    #[must_use]
    pub fn failures(&self) -> Vec<String> {
        let t = self.targets;
        let mut out = Vec::new();
        if self.frames == 0 {
            out.push("no measured frames".to_owned());
            return out;
        }
        let p50 = self.p50_ms.max(1e-6);
        let c = self.counters;
        if c.pipelines > 0 {
            out.push(format!(
                "{} pipeline creations after warm-up (target 0)",
                c.pipelines
            ));
        }
        if c.bind_groups > 0 {
            out.push(format!(
                "{} bind-group creations after warm-up (target 0)",
                c.bind_groups
            ));
        }
        if c.blocking_polls > 0 {
            out.push(format!(
                "{} blocking polls after warm-up (target 0)",
                c.blocking_polls
            ));
        }
        if c.inline_bakes > 0 {
            out.push(format!(
                "{} CPU sky bakes inside frames after warm-up (target 0: FrameRenderer::set_async_sky)",
                c.inline_bakes
            ));
        }
        if self.over_max > 0 {
            out.push(format!(
                "{} frames over {} ms (target 0)",
                self.over_max, t.max_frame_ms
            ));
        }
        let share = self.over_2x as f32 / self.frames as f32;
        if share > t.over_2x_fraction {
            out.push(format!(
                "{} frames over 2 x p50 = {:.3} % (target <= {:.3} %)",
                self.over_2x,
                share * 100.0,
                t.over_2x_fraction * 100.0
            ));
        }
        if self.p99_ms / p50 > t.p99_ratio {
            out.push(format!(
                "p99/p50 = {:.3} (target <= {})",
                self.p99_ms / p50,
                t.p99_ratio
            ));
        }
        if self.p999_ms / p50 > t.p999_ratio {
            out.push(format!(
                "p99.9/p50 = {:.3} (target <= {})",
                self.p999_ms / p50,
                t.p999_ratio
            ));
        }
        if self.longest_ms / p50 > t.longest_ratio {
            out.push(format!(
                "longest frame {:.2} ms = {:.2} x p50 (target <= {})",
                self.longest_ms,
                self.longest_ms / p50,
                t.longest_ratio
            ));
        }
        out
    }

    /// No § 2.4 row missed.
    #[must_use]
    pub fn passes(&self) -> bool {
        self.failures().is_empty()
    }

    /// A few lines for a terminal or a log.
    #[must_use]
    pub fn summary(&self) -> String {
        let p50 = self.p50_ms.max(1e-6);
        let c = self.counters;
        let mut s = format!(
            "{} frames: p50 {:.3} ms, p99 {:.3} ({:.3}x), p99.9 {:.3} ({:.3}x), longest {:.2} ms ({:.2}x, frame {}), \
             > 2 x p50: {} ({:.3} %), > {} ms: {}\n  cpu p50/p99 {:.3}/{:.3} ms, gpu p50/p99 {:.3}/{:.3} ms\n  \
             after warm-up: {} pipelines, {} bind groups, {} blocking polls, {} buffers ({} KiB), {} textures ({} KiB), \
             {} map_async, {} KiB uploaded, {} inline sky bakes; {} frames created something\n",
            self.frames,
            self.p50_ms,
            self.p99_ms,
            self.p99_ms / p50,
            self.p999_ms,
            self.p999_ms / p50,
            self.longest_ms,
            self.longest_ms / p50,
            self.longest_frame,
            self.over_2x,
            self.over_2x as f32 * 100.0 / self.frames.max(1) as f32,
            self.targets.max_frame_ms,
            self.over_max,
            self.cpu_p50_ms,
            self.cpu_p99_ms,
            self.gpu_p50_ms,
            self.gpu_p99_ms,
            c.pipelines,
            c.bind_groups,
            c.blocking_polls,
            c.buffers,
            c.buffer_bytes / 1024,
            c.textures,
            c.texture_bytes / 1024,
            c.map_asyncs,
            c.upload_bytes / 1024,
            c.inline_bakes,
            self.frames_with_creates,
        );
        let failures = self.failures();
        if failures.is_empty() {
            s.push_str("  verdict: PASS (specs/27 § 2.4)\n");
        } else {
            s.push_str(&format!(
                "  verdict: FAIL (specs/27 § 2.4): {}\n",
                failures.join("; ")
            ));
        }
        s
    }
}

/// Collects [`HitchFrame`]s from a running app: call [`Self::frame`] once per presented
/// frame with its interval; device calls are the process-wide counters' delta.
#[derive(Clone, Debug, Default)]
pub struct HitchRecorder {
    last: Option<FramePathCounters>,
    frames: Vec<HitchFrame>,
}

impl HitchRecorder {
    /// An empty recorder (the first [`Self::frame`] call only sets the counter baseline).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records one frame of `frame_ms` (CPU and GPU times as the renderer measured them).
    pub fn frame(&mut self, frame_ms: f32, cpu_ms: f32, gpu_ms: f32) {
        let now = crate::frame_stats::process_counters();
        if let Some(last) = self.last.replace(now) {
            self.frames.push(HitchFrame {
                frame_ms,
                cpu_ms,
                gpu_ms,
                counters: now.since(last),
            });
        }
    }

    /// Frames recorded so far.
    #[must_use]
    pub fn frames(&self) -> &[HitchFrame] {
        &self.frames
    }

    /// The report over the frames after the first `warmup`.
    #[must_use]
    pub fn report(&self, warmup: usize, targets: HitchTargets) -> HitchReport {
        HitchReport::new(self.frames.get(warmup..).unwrap_or(&[]), targets)
    }
}

impl HitchFrame {
    /// Pipeline creates and bind-group creates on this frame, as signed ledger deltas.
    ///
    /// A count that does not fit in `i64` saturates at `i64::MAX`. The counter ledger is the
    /// reader; [`HitchReport`] is unchanged.
    #[must_use]
    pub(crate) fn create_deltas(&self) -> (i64, i64) {
        (
            signed_count(self.counters.pipelines),
            signed_count(self.counters.bind_groups),
        )
    }
}

/// `count` as an `i64` delta, saturated at `i64::MAX`.
fn signed_count(count: u64) -> i64 {
    i64::try_from(count).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steady(n: usize, ms: f32) -> Vec<HitchFrame> {
        (0..n)
            .map(|_| HitchFrame {
                frame_ms: ms,
                cpu_ms: ms * 0.5,
                gpu_ms: ms * 0.8,
                ..HitchFrame::default()
            })
            .collect()
    }

    #[test]
    fn steady_frames_pass() {
        let report = HitchReport::new(&steady(1000, 4.0), HitchTargets::SPEC);
        assert_eq!(report.p50_ms, 4.0);
        assert_eq!(report.over_2x, 0);
        assert!(report.passes(), "{}", report.summary());
    }

    #[test]
    fn one_spike_in_a_thousand_trips_longest_but_not_share() {
        let mut frames = steady(2000, 4.0);
        frames[700].frame_ms = 30.0;
        let report = HitchReport::new(&frames, HitchTargets::SPEC);
        assert_eq!(report.over_2x, 1);
        assert_eq!(report.longest_frame, 700);
        let failures = report.failures();
        assert!(failures.iter().any(|f| f.contains("longest")), "{failures:?}");
        assert!(!failures.iter().any(|f| f.contains("2 x p50")), "{failures:?}");
    }

    #[test]
    fn creates_after_warmup_fail_the_gate() {
        let mut frames = steady(100, 4.0);
        frames[10].counters.bind_groups = 9;
        frames[11].counters.pipelines = 1;
        let report = HitchReport::new(&frames, HitchTargets::SPEC);
        assert_eq!(report.frames_with_creates, 2);
        let failures = report.failures();
        assert!(
            failures.iter().any(|f| f.starts_with("9 bind-group")),
            "{failures:?}"
        );
        assert!(
            failures.iter().any(|f| f.starts_with("1 pipeline")),
            "{failures:?}"
        );
    }

    #[test]
    fn sky_bakes_inside_frames_fail_the_gate() {
        let mut frames = steady(100, 4.0);
        frames[40].counters.inline_bakes = 1;
        let failures = HitchReport::new(&frames, HitchTargets::SPEC).failures();
        assert!(failures.iter().any(|f| f.contains("sky bakes")), "{failures:?}");
    }

    #[test]
    fn frames_over_50_ms_fail() {
        let mut frames = steady(100, 30.0);
        frames[5].frame_ms = 55.0;
        let report = HitchReport::new(&frames, HitchTargets::SPEC);
        assert_eq!(report.over_max, 1);
        assert!(!report.passes());
    }

    #[test]
    fn recorder_skips_the_baseline_frame_and_warmup() {
        let mut recorder = HitchRecorder::new();
        for _ in 0..5 {
            recorder.frame(4.0, 1.0, 3.0);
        }
        assert_eq!(recorder.frames().len(), 4);
        assert_eq!(recorder.report(2, HitchTargets::SPEC).frames, 2);
        assert_eq!(recorder.report(10, HitchTargets::SPEC).frames, 0);
    }

    #[test]
    fn percentile_is_nearest_rank() {
        let v: Vec<f32> = (1..=1000).map(|i| i as f32).collect();
        assert_eq!(percentile_sorted(&v, 0.5), 501.0);
        assert_eq!(percentile_sorted(&v, 0.999), 999.0);
        assert_eq!(percentile_sorted(&[], 0.5), 0.0);
    }
}
