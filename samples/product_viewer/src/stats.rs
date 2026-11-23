// SPDX-License-Identifier: MIT-0
//! What the app measures on itself: frame intervals while something moves, option switch times, draws and
//! wake-ups. Logged when the app comes to rest so a phone needs no debugger, and returned by `viewer.stats`.

use std::time::{Duration, Instant};

use serde::Serialize;

/// Wake-ups per second while nothing moves (the slowest the runner allows).
pub const IDLE_WAKES_PER_SECOND: f32 = 1.0;
/// Wake-up rate while something moves. Above every display's refresh: presenting paces the frames, this only
/// keeps the runner from sleeping between them.
pub const ACTIVE_WAKES_PER_SECOND: f32 = 240.0;
/// Wake-up rate while a package task runs (progress in the status line).
pub const WAITING_WAKES_PER_SECOND: f32 = 5.0;

/// The rate the runner's idle tick should have after a frame: `pending` means a frame is due now.
#[must_use]
pub fn wakes_per_second(pending: bool, waiting: bool) -> f32 {
    if pending {
        ACTIVE_WAKES_PER_SECOND
    } else if waiting {
        WAITING_WAKES_PER_SECOND
    } else {
        IDLE_WAKES_PER_SECOND
    }
}

/// How many frame intervals one report keeps.
const INTERVALS_KEPT: usize = 2048;
/// A gap this long between two presents ends a motion run (the finger rested, nothing moved).
const RUN_BREAK: Duration = Duration::from_millis(250);

/// Percentiles of a set of durations, milliseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize)]
pub struct Percentiles {
    /// Samples.
    pub count: usize,
    /// Median.
    pub p50_ms: f32,
    /// 95th percentile.
    pub p95_ms: f32,
    /// 99th percentile.
    pub p99_ms: f32,
    /// Largest.
    pub max_ms: f32,
}

/// Percentiles of `samples` (nearest rank).
#[must_use]
pub fn percentiles(samples: &[f32]) -> Percentiles {
    if samples.is_empty() {
        return Percentiles::default();
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f32::total_cmp);
    let rank = |p: f32| {
        let index = ((p * sorted.len() as f32).ceil() as usize).saturating_sub(1);
        sorted[index.min(sorted.len() - 1)]
    };
    Percentiles {
        count: sorted.len(),
        p50_ms: rank(0.50),
        p95_ms: rank(0.95),
        p99_ms: rank(0.99),
        max_ms: sorted[sorted.len() - 1],
    }
}

/// Counters and timings since the app started.
#[derive(Debug, Clone, Default)]
pub struct ViewerStats {
    /// Runs of the frame system (the idle tick included).
    pub wakes: u64,
    /// Suspends.
    pub suspends: u64,
    /// Resumes.
    pub resumes: u64,
    /// How long the last package took to read, verify and open, milliseconds.
    pub package_open_ms: Option<f32>,
    /// From process start to the first frame on screen, milliseconds.
    pub first_frame_ms: Option<f32>,
    /// Current wake-up rate.
    pub wakes_per_second: f32,
    intervals: Vec<f32>,
    switches: Vec<f32>,
    last_present: Option<Instant>,
    logged_at_presents: u64,
}

impl ViewerStats {
    /// A frame reached the screen; `moving` says the view was in motion.
    pub fn note_present(&mut self, now: Instant, moving: bool) {
        if moving && let Some(last) = self.last_present {
            let gap = now.saturating_duration_since(last);
            if gap < RUN_BREAK {
                if self.intervals.len() == INTERVALS_KEPT {
                    self.intervals.remove(0);
                }
                self.intervals.push(gap.as_secs_f32() * 1000.0);
            }
        }
        self.last_present = Some(now);
    }

    /// An option switch finished after `took`.
    pub fn note_switch(&mut self, took: Duration) {
        self.switches.push(took.as_secs_f32() * 1000.0);
        tracing::info!(target: "product_viewer", "viewer_switch_ms {:.2}", took.as_secs_f32() * 1000.0);
    }

    /// The app was suspended.
    pub fn note_suspend(&mut self) {
        self.suspends += 1;
        self.last_present = None;
    }

    /// The app came back.
    pub fn note_resume(&mut self) {
        self.resumes += 1;
        self.last_present = None;
    }

    /// Frame intervals while the view moved.
    #[must_use]
    pub fn frame_intervals(&self) -> Percentiles {
        percentiles(&self.intervals)
    }

    /// Option switch times.
    #[must_use]
    pub fn switch_times(&self) -> Percentiles {
        percentiles(&self.switches)
    }

    /// Forgets the timing samples (the counters stay), so a measurement starts clean.
    pub fn clear_timings(&mut self) {
        self.intervals.clear();
        self.switches.clear();
    }

    /// The host's half of `viewer.stats`.
    #[must_use]
    pub fn report(&self) -> serde_json::Value {
        serde_json::json!({
            "wakes": self.wakes,
            "wakes_per_second": self.wakes_per_second,
            "idle": self.wakes_per_second <= IDLE_WAKES_PER_SECOND,
            "suspends": self.suspends,
            "resumes": self.resumes,
            "package_open_ms": self.package_open_ms,
            "first_frame_ms": self.first_frame_ms,
            "frame_interval_ms": self.frame_intervals(),
            "switch_ms": self.switch_times(),
        })
    }

    /// One log line each time the app comes to rest after presenting.
    pub fn log_when_idle(&mut self, presents: u64, wakes: f32) {
        if wakes <= IDLE_WAKES_PER_SECOND && presents != self.logged_at_presents {
            self.logged_at_presents = presents;
            tracing::info!(target: "product_viewer", "viewer_stats presents={presents} {}", self.report());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_the_nearest_rank() {
        let samples: Vec<f32> = (1..=100).map(|n| n as f32).collect();
        let p = percentiles(&samples);
        assert_eq!(
            (p.count, p.p50_ms, p.p95_ms, p.p99_ms, p.max_ms),
            (100, 50.0, 95.0, 99.0, 100.0)
        );
        assert_eq!(percentiles(&[]), Percentiles::default());
        assert_eq!(percentiles(&[7.0]).p99_ms, 7.0);
    }

    #[test]
    fn only_moving_frames_inside_a_run_count() {
        let mut stats = ViewerStats::default();
        let start = Instant::now();
        stats.note_present(start, false);
        stats.note_present(start + Duration::from_millis(16), true);
        stats.note_present(start + Duration::from_millis(32), false);
        stats.note_present(start + Duration::from_secs(9), true);
        stats.note_present(start + Duration::from_secs(9) + Duration::from_millis(8), true);
        let p = stats.frame_intervals();
        assert_eq!(p.count, 2, "{p:?}");
    }

    #[test]
    fn a_suspend_breaks_the_run() {
        let mut stats = ViewerStats::default();
        let start = Instant::now();
        stats.note_present(start, true);
        stats.note_suspend();
        stats.note_present(start + Duration::from_millis(16), true);
        assert_eq!(stats.frame_intervals().count, 0);
    }

    #[test]
    fn the_wake_rate_follows_what_is_due() {
        assert_eq!(wakes_per_second(false, false), IDLE_WAKES_PER_SECOND);
        assert_eq!(wakes_per_second(false, true), WAITING_WAKES_PER_SECOND);
        assert_eq!(wakes_per_second(true, true), ACTIVE_WAKES_PER_SECOND);
        assert_eq!(wakes_per_second(true, false), ACTIVE_WAKES_PER_SECOND);
    }
}
