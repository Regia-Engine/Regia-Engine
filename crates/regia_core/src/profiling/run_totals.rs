// SPDX-License-Identifier: MPL-2.0
//! Whole-run span and ECS-system totals for headless benches (`REGIA_SPAN_REPORT`).
//!
//! [`FrameProfiler`] keeps the last [`super::HISTORY_FRAMES`] frames for overlays. A bench that
//! wants to know where a whole run's CPU time went (the hotspot list in `docs/perf-log.md`) wraps
//! the process profiler in a [`RunTotals`], calls [`RunTotals::end_frame`] once per measured
//! frame (and [`RunTotals::skip_frame`] for warm-up frames), and prints [`RunTotals::report`].
//!
//! Rows are keyed by span name (ECS systems by system name) and summed over every thread. Each
//! has an inclusive time (a parent contains its children) and a self time (minus the child spans
//! entered inside it on the same thread); the table is sorted by self time, so the top rows are
//! where the time is actually spent. A system's self time is its inclusive time. Systems are read with
//! `take_system_times`, which drains a process-wide counter, so a process where something else
//! already calls it (the telemetry plugin) uses [`RunTotals::spans_only`].

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::Duration;

use super::{FrameProfiler, system_name, take_system_times};

/// Environment variable read by the engine's headless benches: when set, they print a
/// [`RunTotals::report`] with that many rows at exit (`REGIA_SPAN_REPORT=30`). A value that is
/// not a positive number prints [`DEFAULT_REPORT_ROWS`] rows.
pub const SPAN_REPORT_ENV: &str = "REGIA_SPAN_REPORT";

/// Rows a report prints when [`SPAN_REPORT_ENV`] is set without a usable number.
pub const DEFAULT_REPORT_ROWS: usize = 30;

/// Rows requested by a [`SPAN_REPORT_ENV`] value: `None` when unset, else the number, or
/// [`DEFAULT_REPORT_ROWS`] when the value is empty, zero or not a number.
#[must_use]
pub fn parse_report_rows(value: Option<&str>) -> Option<usize> {
    let value = value?;
    Some(
        value
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|&n| n > 0)
            .unwrap_or(DEFAULT_REPORT_ROWS),
    )
}

/// Rows requested through the [`SPAN_REPORT_ENV`] environment variable ([`parse_report_rows`]).
#[must_use]
pub fn report_rows_from_env() -> Option<usize> {
    let value = crate::brand::env_var(SPAN_REPORT_ENV).ok();
    parse_report_rows(value.as_deref())
}

/// What a [`SpanRow`] measures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpanKind {
    /// A `tracing` span, by span name (inclusive of its child spans).
    Span,
    /// A `bevy_ecs` system, by system name.
    System,
}

/// One line of a [`RunTotals`] table.
#[derive(Debug, Clone, PartialEq)]
pub struct SpanRow {
    /// Span or system name.
    pub name: String,
    /// Span or system.
    pub kind: SpanKind,
    /// Time over every counted frame (all threads), inclusive of child spans.
    pub total: Duration,
    /// `total` minus the child spans entered inside it on the same thread (a system: `total`).
    pub self_time: Duration,
    /// Enter/exit pairs (span) or runs (system) over every counted frame.
    pub calls: u64,
}

/// Sums a [`FrameProfiler`]'s frames over a whole run (see the module docs).
#[derive(Debug)]
pub struct RunTotals {
    profiler: FrameProfiler,
    spans: HashMap<&'static str, (Duration, Duration, u64)>,
    systems: HashMap<u32, (Duration, u64)>,
    frames: u64,
    scratch: Vec<(u32, Duration, u32)>,
    take_systems: bool,
}

impl RunTotals {
    /// Totals fed by `profiler`, which must be installed in the process subscriber (for example
    /// through [`crate::log::LogConfig::profiler`]).
    #[must_use]
    pub fn new(profiler: FrameProfiler) -> Self {
        Self {
            profiler,
            spans: HashMap::new(),
            systems: HashMap::new(),
            frames: 0,
            scratch: Vec::new(),
            take_systems: true,
        }
    }

    /// Totals of span times only: ECS system times are left for whoever else drains them
    /// (telemetry's per-system table).
    #[must_use]
    pub fn spans_only(profiler: FrameProfiler) -> Self {
        Self {
            take_systems: false,
            ..Self::new(profiler)
        }
    }

    /// For a headless bench: when [`SPAN_REPORT_ENV`] is set, starts logging ([`crate::log::init`],
    /// default filter, no panic hook) with a new profiler installed, and returns totals fed by it
    /// and the number of rows to print. `Ok(None)` when the variable is unset.
    ///
    /// Call it first thing in `main`: logging is installed once per process, and spans are only
    /// seen when this profiler is part of that installation.
    pub fn install_from_env() -> Result<Option<(Self, usize)>, crate::log::LogError> {
        let Some(rows) = report_rows_from_env() else {
            return Ok(None);
        };
        let profiler = FrameProfiler::new();
        crate::log::init(crate::log::LogConfig {
            panic_hook: false,
            profiler: Some(profiler.clone()),
            ..crate::log::LogConfig::default()
        })?;
        Ok(Some((Self::new(profiler), rows)))
    }

    /// The profiler these totals read.
    #[must_use]
    pub fn profiler(&self) -> &FrameProfiler {
        &self.profiler
    }

    /// Closes the profiler's frame without counting it (a warm-up frame).
    pub fn skip_frame(&mut self) {
        self.profiler.end_frame();
        if self.take_systems {
            take_system_times(&mut self.scratch);
        }
    }

    /// Closes the profiler's frame and adds its span and system times to the totals.
    pub fn end_frame(&mut self) {
        self.profiler.end_frame();
        {
            let state = self.profiler.shared.state.lock();
            if let Some(frame) = state.frames.back() {
                for t in &frame.spans {
                    let entry = self.spans.entry(t.name).or_default();
                    entry.0 += t.total;
                    entry.1 += t.self_time;
                    entry.2 += u64::from(t.calls);
                }
            }
        }
        if self.take_systems {
            let mut systems = std::mem::take(&mut self.scratch);
            take_system_times(&mut systems);
            self.add_systems(&systems);
            self.scratch = systems;
        }
        self.frames += 1;
    }

    /// Adds system times as [`take_system_times`] reports them: `(slot, time, runs)`.
    fn add_systems(&mut self, systems: &[(u32, Duration, u32)]) {
        for &(slot, time, runs) in systems {
            let entry = self.systems.entry(slot).or_default();
            entry.0 += time;
            entry.1 += u64::from(runs);
        }
    }

    /// Frames counted so far.
    #[must_use]
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Every span and system, most self time first (ties by name).
    #[must_use]
    pub fn rows(&self) -> Vec<SpanRow> {
        let mut rows: Vec<SpanRow> = self
            .spans
            .iter()
            .map(|(&name, &(total, self_time, calls))| SpanRow {
                name: name.to_owned(),
                kind: SpanKind::Span,
                total,
                self_time,
                calls,
            })
            .chain(self.systems.iter().map(|(&slot, &(total, calls))| SpanRow {
                name: system_name(slot).map_or_else(|| format!("system #{slot}"), |n| n.to_string()),
                kind: SpanKind::System,
                total,
                self_time: total,
                calls,
            }))
            .collect();
        rows.sort_by(|a, b| {
            b.self_time
                .cmp(&a.self_time)
                .then_with(|| b.total.cmp(&a.total))
                .then_with(|| a.name.cmp(&b.name))
        });
        rows
    }

    /// A plain-text table of the `top` rows with the most self time: self and inclusive
    /// milliseconds and calls per counted frame, kind and name.
    #[must_use]
    pub fn report(&self, top: usize) -> String {
        let frames = self.frames.max(1) as f64;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0 / frames;
        let mut out = format!(
            "span report: {} frames; mean CPU ms per frame summed over all threads, by self time
  rank   self ms   incl ms  calls/frame  kind    name
",
            self.frames
        );
        for (rank, row) in self.rows().iter().take(top).enumerate() {
            let kind = match row.kind {
                SpanKind::Span => "span",
                SpanKind::System => "system",
            };
            let _ = writeln!(
                out,
                "  {:>4}  {:>8.3}  {:>8.3}  {:>11.1}  {kind:<6}  {}",
                rank + 1,
                ms(row.self_time),
                ms(row.total),
                row.calls as f64 / frames,
                row.name
            );
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profiling::scope;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn report_rows_parse_from_the_env_value() {
        assert_eq!(parse_report_rows(None), None);
        assert_eq!(parse_report_rows(Some("12")), Some(12));
        assert_eq!(parse_report_rows(Some(" 7 ")), Some(7));
        assert_eq!(parse_report_rows(Some("")), Some(DEFAULT_REPORT_ROWS));
        assert_eq!(parse_report_rows(Some("0")), Some(DEFAULT_REPORT_ROWS));
        assert_eq!(parse_report_rows(Some("all")), Some(DEFAULT_REPORT_ROWS));
    }

    #[test]
    fn counted_frames_sum_calls_and_skipped_frames_do_not() {
        // Spans only: draining the process-wide system counters would race the system test
        // in `profiling::tests`.
        let mut totals = RunTotals::spans_only(FrameProfiler::new());
        let subscriber = tracing_subscriber::registry().with(totals.profiler().clone());
        tracing::subscriber::with_default(subscriber, || {
            // Warm-up: never counted.
            for _ in 0..5 {
                scope!("run_totals_test_warm");
            }
            totals.skip_frame();
            for frame in 0..4u32 {
                {
                    scope!("run_totals_test_outer");
                    for _ in 0..=frame {
                        scope!("run_totals_test_inner");
                    }
                }
                totals.end_frame();
            }
        });
        assert_eq!(totals.frames(), 4);
        let rows = totals.rows();
        let row = |name: &str| rows.iter().find(|r| r.name == name && r.kind == SpanKind::Span);
        assert!(row("run_totals_test_warm").is_none(), "warm-up frame counted");
        assert_eq!(row("run_totals_test_outer").map(|r| r.calls), Some(4));
        // 1 + 2 + 3 + 4 inner scopes.
        assert_eq!(row("run_totals_test_inner").map(|r| r.calls), Some(10));
        // Inclusive: the outer span contains every inner one.
        let (outer, inner) = (
            row("run_totals_test_outer").map(|r| r.total),
            row("run_totals_test_inner").map(|r| r.total),
        );
        assert!(outer >= inner, "{outer:?} < {inner:?}");
        // Self time: the outer span's children are exactly the inner spans; a leaf's self time
        // is its inclusive time. Both sides add the same measured durations, so this is exact.
        let outer_row = row("run_totals_test_outer").expect("outer row");
        let inner_row = row("run_totals_test_inner").expect("inner row");
        assert_eq!(outer_row.total - outer_row.self_time, inner_row.total);
        assert_eq!(inner_row.self_time, inner_row.total);
        let report = totals.report(10);
        assert!(report.starts_with("span report: 4 frames"), "{report}");
        assert!(report.contains("run_totals_test_inner"), "{report}");
        assert!(report.contains("2.5"), "inner calls per frame: {report}");
    }

    #[test]
    fn systems_add_up_by_slot_and_sort_with_spans() {
        let mut totals = RunTotals::spans_only(FrameProfiler::new());
        totals.add_systems(&[(u32::MAX - 1, Duration::from_millis(5), 2)]);
        totals.add_systems(&[(u32::MAX - 1, Duration::from_millis(7), 3)]);
        totals.spans.insert(
            "small_span",
            (Duration::from_millis(3), Duration::from_millis(1), 9),
        );
        let rows = totals.rows();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].kind, SpanKind::System);
        assert_eq!(rows[0].name, format!("system #{}", u32::MAX - 1));
        assert_eq!(rows[0].total, Duration::from_millis(12));
        assert_eq!(rows[0].self_time, Duration::from_millis(12));
        assert_eq!(rows[0].calls, 5);
        assert_eq!(rows[1].self_time, Duration::from_millis(1));
        assert_eq!(rows[1].name, "small_span");
        // Zero counted frames: the report divides by one, never by zero.
        let report = totals.report(1);
        assert!(report.contains("12.000"), "{report}");
        assert!(!report.contains("small_span"), "top 1 only: {report}");
    }
}
