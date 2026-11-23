// SPDX-License-Identifier: MPL-2.0
//! The counter and mark listing (P-312, P-318; `book/agents/profiler.md` "Monitors"): every named
//! counter a host or the engine reports through [`counter`], with its newest samples, and the newest
//! trace marks, so the editor Profiler can graph any counter (its Monitors tab) and draw marks on its
//! History graph and thread timeline.
//!
//! [`counter`] is the telemetry counter API: it keeps the sample here and forwards it to the process
//! trace recorder ([`crate::trace::Recorder::counter`], the `counters` stream of a recording). The
//! listing is bounded: [`MAX_COUNTERS`] names (a new name past it is dropped and counted in
//! [`CounterListing::dropped_names`]) and [`SAMPLES_PER_COUNTER`] samples per name (oldest dropped).
//! Marks keep the newest [`MAX_MARKS`]. Each sample and mark carries the frame it was taken in: the
//! frame in progress of the frame profiler attached with [`attach_frame_profiler`] (frames ended
//! before it, the number the profiler's history and timeline use), or 0 when none is attached.
//!
//! Cost: one short lock and, for a new name only, one allocation per sample; no IO.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Most counter names the listing keeps; samples of further names are dropped (and counted).
pub const MAX_COUNTERS: usize = 512;

/// Samples kept per counter (the frame profiler's history length).
pub const SAMPLES_PER_COUNTER: usize = regia_core::profiling::HISTORY_FRAMES;

/// Most marks kept (the newest).
pub const MAX_MARKS: usize = 512;

/// Longest counter or mark name kept, bytes; longer names are cut at a character boundary.
pub const MAX_NAME_BYTES: usize = 120;

/// One counter sample: the frame it was taken in and its value.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct CounterSample {
    /// The frame profiler's frame in progress when sampled (0 with no profiler attached).
    pub frame: u64,
    /// The value.
    pub value: f64,
}

/// A counter's samples, oldest first, and their summary.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CounterSeries {
    /// The counter's name.
    pub name: String,
    /// The kept samples, oldest first.
    pub samples: Vec<CounterSample>,
    /// Samples ever taken (kept or dropped as old).
    pub total: u64,
}

/// Minimum, maximum and mean of a set of values.
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize)]
pub struct Summary {
    /// The newest value.
    pub last: f64,
    /// The smallest value.
    pub min: f64,
    /// The largest value.
    pub max: f64,
    /// The arithmetic mean.
    pub avg: f64,
    /// How many values.
    pub count: usize,
}

/// The summary of `values` in order (`None` when empty or when no value is finite). Non-finite
/// values are skipped.
#[must_use]
pub fn summarize(values: impl IntoIterator<Item = f64>) -> Option<Summary> {
    let mut out: Option<Summary> = None;
    let mut sum = 0.0;
    for v in values.into_iter().filter(|v| v.is_finite()) {
        sum += v;
        out = Some(match out {
            None => Summary {
                last: v,
                min: v,
                max: v,
                avg: v,
                count: 1,
            },
            Some(s) => Summary {
                last: v,
                min: s.min.min(v),
                max: s.max.max(v),
                avg: 0.0,
                count: s.count + 1,
            },
        });
    }
    out.map(|mut s| {
        s.avg = sum / s.count as f64;
        s
    })
}

impl CounterSeries {
    /// Min, max, mean and newest of the kept samples.
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        summarize(self.samples.iter().map(|s| s.value))
    }
}

/// A recorded mark: a named instant.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct MarkEntry {
    /// Marks recorded before this one since the process started.
    pub seq: u64,
    /// The mark's name.
    pub name: String,
    /// What made it: `trace` (a trace mark) or the kind a host noted ([`note_mark`]).
    pub kind: String,
    /// The frame profiler's frame in progress (0 with no profiler attached).
    pub frame: u64,
}

/// The counter names and the listing's bounds, as [`listing`] reports them.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct CounterListing {
    /// Every counter name, sorted.
    pub names: Vec<String>,
    /// Samples dropped because the listing was full of other names.
    pub dropped_names: u64,
}

#[derive(Default)]
struct Board {
    counters: HashMap<String, (VecDeque<CounterSample>, u64)>,
    dropped_names: u64,
    marks: VecDeque<MarkEntry>,
    marks_total: u64,
    profiler: Option<regia_core::profiling::FrameProfiler>,
}

fn board() -> MutexGuard<'static, Board> {
    static BOARD: OnceLock<Mutex<Board>> = OnceLock::new();
    BOARD
        .get_or_init(|| Mutex::new(Board::default()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn cut(name: &str) -> &str {
    if name.len() <= MAX_NAME_BYTES {
        return name;
    }
    let mut end = MAX_NAME_BYTES;
    while !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

impl Board {
    fn frame(&self) -> u64 {
        self.profiler
            .as_ref()
            .and_then(regia_core::profiling::FrameProfiler::newest_frame_number)
            .map_or(0, |n| n + 1)
    }

    fn note(&mut self, name: &str, value: f64) {
        let frame = self.frame();
        let name = cut(name);
        if !self.counters.contains_key(name) {
            if self.counters.len() >= MAX_COUNTERS {
                self.dropped_names += 1;
                return;
            }
            self.counters
                .insert(name.to_owned(), (VecDeque::with_capacity(8), 0));
        }
        if let Some((samples, total)) = self.counters.get_mut(name) {
            if samples.len() >= SAMPLES_PER_COUNTER {
                samples.pop_front();
            }
            samples.push_back(CounterSample { frame, value });
            *total += 1;
        }
    }
}

/// Reports `value` for the counter `name` (the telemetry counter API): kept in the listing for the
/// Profiler's Monitors tab and sent to the process trace recorder's `counters` stream. Games call
/// this for their own counters (`enemies_alive`, `nav_queries`); a name shows in the listing from
/// its first sample on.
pub fn counter(name: &str, value: f64) {
    board().note(name, value);
    crate::trace::recorder().counter(name, value);
}

/// [`counter`] without the trace recorder: keeps the sample in the listing only (the editor
/// Profiler's own samplers use it, so a recording is not filled with editor-side values).
pub fn note_counter(name: &str, value: f64) {
    board().note(name, value);
}

/// Every counter name and the listing's dropped count.
#[must_use]
pub fn listing() -> CounterListing {
    let b = board();
    let mut names: Vec<String> = b.counters.keys().cloned().collect();
    names.sort();
    CounterListing {
        names,
        dropped_names: b.dropped_names,
    }
}

/// The samples of counter `name` (`None` when it never reported).
#[must_use]
pub fn series(name: &str) -> Option<CounterSeries> {
    let b = board();
    let (samples, total) = b.counters.get(name)?;
    Some(CounterSeries {
        name: name.to_owned(),
        samples: samples.iter().copied().collect(),
        total: *total,
    })
}

/// Records a mark of `kind` (`trace` for a trace mark, `level_load` for a level load, or a host's
/// own kind) in the listing, in the frame in progress.
pub fn note_mark(name: &str, kind: &str) {
    let mut b = board();
    let frame = b.frame();
    let seq = b.marks_total;
    b.marks_total += 1;
    if b.marks.len() >= MAX_MARKS {
        b.marks.pop_front();
    }
    b.marks.push_back(MarkEntry {
        seq,
        name: cut(name).to_owned(),
        kind: cut(kind).to_owned(),
        frame,
    });
}

/// The kept marks recorded at or after `since_seq`, oldest first.
#[must_use]
pub fn marks_since(since_seq: u64) -> Vec<MarkEntry> {
    board()
        .marks
        .iter()
        .filter(|m| m.seq >= since_seq)
        .cloned()
        .collect()
}

/// Tags counter samples and marks from now on with `profiler`'s frame in progress (replaces an
/// earlier one). The editor attaches its process profiler when the Profiler panel reads it.
pub fn attach_frame_profiler(profiler: &regia_core::profiling::FrameProfiler) {
    board().profiler = Some(profiler.clone());
}

/// Whether a frame profiler is attached ([`attach_frame_profiler`]).
#[must_use]
pub fn frame_profiler_attached() -> bool {
    board().profiler.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_counter_lists_its_name_and_summarizes_its_samples() {
        let name = "counters_test.enemies_alive";
        for v in [3.0, 7.0, 5.0] {
            counter(name, v);
        }
        assert!(listing().names.iter().any(|n| n == name));
        let series = series(name).expect("the counter reported");
        assert_eq!(series.total, 3);
        let values: Vec<f64> = series.samples.iter().map(|s| s.value).collect();
        assert_eq!(values, [3.0, 7.0, 5.0]);
        let s = series.summary().expect("three samples");
        assert_eq!((s.last, s.min, s.max, s.avg, s.count), (5.0, 3.0, 7.0, 5.0, 3));
        assert_eq!(super::series("counters_test.never"), None);
    }

    #[test]
    fn a_counter_keeps_its_newest_samples_only() {
        let name = "counters_test.ring";
        let extra = 25;
        for i in 0..SAMPLES_PER_COUNTER + extra {
            note_counter(name, i as f64);
        }
        let series = series(name).expect("reported");
        assert_eq!(series.samples.len(), SAMPLES_PER_COUNTER);
        assert_eq!(series.total, (SAMPLES_PER_COUNTER + extra) as u64);
        assert_eq!(series.samples[0].value, extra as f64, "the oldest were dropped");
    }

    #[test]
    fn summarize_skips_non_finite_values_and_empty_is_none() {
        assert_eq!(summarize([]), None);
        assert_eq!(summarize([f64::NAN]), None);
        let s = summarize([1.0, f64::INFINITY, 3.0]).expect("two finite");
        assert_eq!((s.min, s.max, s.avg, s.count, s.last), (1.0, 3.0, 2.0, 2, 3.0));
    }

    #[test]
    fn long_names_are_cut_at_a_character_boundary() {
        let long = "é".repeat(MAX_NAME_BYTES);
        note_counter(&long, 1.0);
        let kept = listing()
            .names
            .into_iter()
            .find(|n| n.starts_with('é'))
            .expect("kept");
        assert!(kept.len() <= MAX_NAME_BYTES && long.starts_with(&kept));
    }

    #[test]
    fn marks_are_kept_in_order_with_their_kind() {
        let before = marks_since(0).last().map_or(0, |m| m.seq + 1);
        note_mark("counters_test.level", "level_load");
        crate::trace::recorder().mark("counters_test.trace_mark", None);
        let marks: Vec<MarkEntry> = marks_since(before)
            .into_iter()
            .filter(|m| m.name.starts_with("counters_test."))
            .collect();
        let got: Vec<(&str, &str)> = marks.iter().map(|m| (m.name.as_str(), m.kind.as_str())).collect();
        assert_eq!(
            got,
            [
                ("counters_test.level", "level_load"),
                ("counters_test.trace_mark", "trace")
            ]
        );
        assert!(marks[0].seq < marks[1].seq);
    }

    #[test]
    fn samples_carry_the_attached_profilers_frame_in_progress() {
        // A board of its own: the process listing is shared with the other tests of this binary.
        let mut board = Board::default();
        board.note("framed", 1.0);
        let profiler = regia_core::profiling::FrameProfiler::new();
        profiler.end_frame();
        profiler.end_frame();
        board.profiler = Some(profiler.clone());
        board.note("framed", 2.0);
        profiler.end_frame();
        board.note("framed", 3.0);
        let frames: Vec<u64> = board.counters["framed"].0.iter().map(|s| s.frame).collect();
        let newest = profiler.newest_frame_number().expect("three frames ended");
        assert_eq!(
            frames,
            [0, newest, newest + 1],
            "no profiler reads 0, then the frame in progress"
        );
    }
}
