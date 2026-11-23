// SPDX-License-Identifier: MPL-2.0
//! The Profiler panel's data, as pure functions (`docs/specs/83-prefab-profiler-volumes.md` § 4.3,
//! ADR-9740): which spans belong to the edit world and which to play, the frame-time history and its
//! hitches, ECS entity counts, and the flame view's colors when two captures are compared. The
//! window in [`crate::profiler_panel`] only paints what these return, so they are tested without a
//! GPU or a window.

use std::collections::BTreeMap;

use regia::telemetry::profile_capture::{CaptureNode, ProfileCapture};
use regia_ecs::prelude::*;

/// Span names that wrap a play frame: the in-process PIE play world (`pie_frame`, `pie_step`,
/// `crate::pie::PieSession`), its client worlds (`pie_client_frame`, `crate::pie_worlds`) and the
/// pictures the editor draws of a play world (`pie_render`: a viewport pane or the play window,
/// `crate::render`, ADR-8594). Everything under one of them is play; everything else is the editor.
pub const PIE_SPANS: &[&str] = &["pie_frame", "pie_step", "pie_client_frame", "pie_render"];

/// Frames the history graph shows (the frame profiler's history).
pub const HISTORY_FRAMES: usize = regia_core::profiling::HISTORY_FRAMES;

/// Components listed by entity count.
pub const TOP_COMPONENTS: usize = 20;

/// A frame is a hitch when it takes this many times the history's median (and at least
/// [`HITCH_MIN_MS`]).
pub const HITCH_FACTOR: f64 = 2.0;

/// The shortest frame that can be a hitch, ms (a 2× spike of a 0.1 ms idle frame is not one).
pub const HITCH_MIN_MS: f64 = 4.0;

/// Spans listed per hitch.
pub const HITCH_TOP_SPANS: usize = 5;

fn is_pie(name: &str) -> bool {
    PIE_SPANS.contains(&name)
}

/// Total inclusive time of the play subtrees under `nodes`, ms.
fn pie_time(nodes: &[CaptureNode]) -> f64 {
    nodes
        .iter()
        .map(|n| {
            if is_pie(&n.name) {
                n.inclusive_ms
            } else {
                pie_time(&n.children)
            }
        })
        .sum()
}

/// Every play subtree under `nodes`, outermost first in tree order.
fn pie_roots(nodes: &[CaptureNode], out: &mut Vec<CaptureNode>) {
    for node in nodes {
        if is_pie(&node.name) {
            out.push(node.clone());
        } else {
            pie_roots(&node.children, out);
        }
    }
}

/// `nodes` without their play subtrees; each ancestor's inclusive time loses what it held of them.
fn without_pie(nodes: &[CaptureNode]) -> Vec<CaptureNode> {
    nodes
        .iter()
        .filter(|n| !is_pie(&n.name))
        .map(|n| {
            let removed = pie_time(&n.children);
            CaptureNode {
                name: n.name.clone(),
                inclusive_ms: round_us(n.inclusive_ms - removed),
                self_ms: n.self_ms,
                calls: n.calls,
                children: without_pie(&n.children),
            }
        })
        .collect()
}

fn round_us(ms: f64) -> f64 {
    let r = (ms * 1000.0).round() / 1000.0;
    if r == 0.0 { 0.0 } else { r }
}

/// Merges sibling nodes of the same name (two `pie_frame` roots from two call sites become one).
fn merge_siblings(nodes: Vec<CaptureNode>) -> Vec<CaptureNode> {
    let mut by_name: BTreeMap<String, CaptureNode> = BTreeMap::new();
    for node in nodes {
        match by_name.get_mut(&node.name) {
            Some(existing) => {
                existing.inclusive_ms += node.inclusive_ms;
                existing.self_ms += node.self_ms;
                existing.calls += node.calls;
                existing.children.extend(node.children);
                let children = std::mem::take(&mut existing.children);
                existing.children = merge_siblings(children);
            }
            None => {
                by_name.insert(node.name.clone(), node);
            }
        }
    }
    let mut out: Vec<CaptureNode> = by_name.into_values().collect();
    out.sort_by(|a, b| {
        b.inclusive_ms
            .total_cmp(&a.inclusive_ms)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// The part of `capture` one in-process source owns: with `play` the play subtrees (each
/// [`PIE_SPANS`] node and everything under it, as roots), otherwise the tree without them. The
/// frame time is the play time, or the frame time minus it. GPU passes stay with the editor (the
/// play world draws in the editor's viewport).
#[must_use]
pub fn split_source(capture: &ProfileCapture, play: bool) -> ProfileCapture {
    let play_ms = pie_time(&capture.cpu);
    let (cpu, frame_ms, gpu) = if play {
        let mut roots = Vec::new();
        pie_roots(&capture.cpu, &mut roots);
        (merge_siblings(roots), round_us(play_ms), Vec::new())
    } else {
        (
            without_pie(&capture.cpu),
            round_us(capture.frame_ms - play_ms),
            capture.gpu.clone(),
        )
    };
    let mut out = ProfileCapture {
        format_version: capture.format_version,
        frames: capture.frames,
        frame_ms,
        cpu,
        truncated: capture.truncated,
        gpu,
        folded: String::new(),
        timeline: Vec::new(),
        timeline_truncated: 0,
    };
    out.folded = folded(&out.cpu);
    out
}

fn folded(nodes: &[CaptureNode]) -> String {
    fn walk(nodes: &[CaptureNode], prefix: &str, out: &mut String) {
        for node in nodes {
            let path = if prefix.is_empty() {
                node.name.clone()
            } else {
                format!("{prefix};{}", node.name)
            };
            out.push_str(&format!("{path} {}\n", node.self_ms));
            walk(&node.children, &path, out);
        }
    }
    let mut out = String::new();
    walk(nodes, "", &mut out);
    out
}

/// The span names of a capture by inclusive time, most first (each name once, its largest
/// inclusive time anywhere in the tree), at most `n`.
#[must_use]
pub fn top_span_names(capture: &ProfileCapture, n: usize) -> Vec<String> {
    fn walk(nodes: &[CaptureNode], out: &mut BTreeMap<String, f64>) {
        for node in nodes {
            let e = out.entry(node.name.clone()).or_insert(0.0);
            *e = e.max(node.inclusive_ms);
            walk(&node.children, out);
        }
    }
    let mut by = BTreeMap::new();
    walk(&capture.cpu, &mut by);
    let mut rows: Vec<(String, f64)> = by.into_iter().collect();
    rows.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    rows.into_iter().take(n).map(|(name, _)| name).collect()
}

/// ECS counts of one world: what the Profiler's Entities view shows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityCounts {
    /// Spawned entities (resources are entities in this ECS; the same number as
    /// `memory.report`'s `ecs.entities`).
    pub entities: u32,
    /// Archetypes.
    pub archetypes: usize,
    /// The components held by the most entities, most first (ties by name): `(name, entities)`.
    pub components: Vec<(String, u64)>,
}

/// The counts of `world`: entities, archetypes and the `top` components by entity count. Walks the
/// archetype table (not the entities), so the cost is the number of archetypes times their
/// components. The same count `profiler.frame { entities }` answers for a process source
/// (`regia_agent::profiler_entity_counts`), so both sources show the same rows.
#[must_use]
pub fn entity_counts(world: &World, top: usize) -> EntityCounts {
    let (entities, archetypes, components) = regia_agent::profiler_entity_counts(world, top);
    EntityCounts {
        entities,
        archetypes,
        components,
    }
}

/// The counts of a `profiler.frame { entities }` answer's `entities` object, or `None` when the
/// answer has none (a build older than the field).
#[must_use]
pub fn entity_counts_from_answer(entities: &serde_json::Value) -> Option<EntityCounts> {
    let components = entities["components"]
        .as_array()?
        .iter()
        .filter_map(|row| Some((row["name"].as_str()?.to_owned(), row["entities"].as_u64()?)))
        .collect();
    Some(EntityCounts {
        entities: u32::try_from(entities["entities"].as_u64()?).unwrap_or(u32::MAX),
        archetypes: usize::try_from(entities["archetypes"].as_u64()?).unwrap_or(usize::MAX),
        components,
    })
}

/// One frame of the history graph.
#[derive(Clone, Debug, PartialEq)]
pub struct FrameSample {
    /// The frame's time, ms (for the play source, the play spans' time in it).
    pub ms: f64,
    /// Its costliest spans: `(name, inclusive ms)`, most first.
    pub spans: Vec<(String, f64)>,
}

/// The median frame time of `samples`, ms (0 when empty).
#[must_use]
pub fn median_ms(samples: &[FrameSample]) -> f64 {
    let mut ms: Vec<f64> = samples.iter().map(|s| s.ms).collect();
    if ms.is_empty() {
        return 0.0;
    }
    ms.sort_by(f64::total_cmp);
    let mid = ms.len() / 2;
    if ms.len().is_multiple_of(2) {
        (ms[mid - 1] + ms[mid]) / 2.0
    } else {
        ms[mid]
    }
}

/// The hitch threshold of `samples`, ms: [`HITCH_FACTOR`] × the median, at least [`HITCH_MIN_MS`].
#[must_use]
pub fn hitch_threshold_ms(samples: &[FrameSample]) -> f64 {
    (median_ms(samples) * HITCH_FACTOR).max(HITCH_MIN_MS)
}

/// The indices of the hitch frames of `samples` (slower than `threshold_ms`), oldest first.
#[must_use]
pub fn hitches(samples: &[FrameSample], threshold_ms: f64) -> Vec<usize> {
    samples
        .iter()
        .enumerate()
        .filter(|(_, s)| s.ms > threshold_ms)
        .map(|(i, _)| i)
        .collect()
}

/// Every span name in a capture's tree.
#[must_use]
pub fn capture_names(capture: &ProfileCapture) -> std::collections::BTreeSet<String> {
    fn walk(nodes: &[CaptureNode], out: &mut std::collections::BTreeSet<String>) {
        for node in nodes {
            out.insert(node.name.clone());
            walk(&node.children, out);
        }
    }
    let mut out = std::collections::BTreeSet::new();
    walk(&capture.cpu, &mut out);
    out
}

/// The history of a local source from the frame profiler: one sample per retained frame, oldest
/// first. `play_names` are the span names of the play source's tree ([`split_source`] with
/// `play`; per-frame totals carry names, not paths). With `play` a frame's time is its
/// [`PIE_SPANS`] time (0 for a frame without play) and its spans are those of `play_names`;
/// otherwise its wall time minus the play time, and the spans not in `play_names`. A name used by
/// both sides counts as play.
#[must_use]
pub fn local_history(
    profiler: &regia_core::profiling::FrameProfiler,
    play: bool,
    play_names: &std::collections::BTreeSet<String>,
) -> Vec<FrameSample> {
    let _span = tracing::trace_span!("profiler_history").entered();
    let times = profiler.frame_times();
    let ms_of = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    times
        .iter()
        .enumerate()
        .map(|(index, duration)| {
            let spans = profiler.frame_spans(index);
            let play_ms: f64 = spans
                .iter()
                .filter(|(name, _, _)| is_pie(name))
                .map(|(_, d, _)| ms_of(*d))
                .sum();
            let ms = if play {
                play_ms
            } else {
                (ms_of(*duration) - play_ms).max(0.0)
            };
            let spans = spans
                .iter()
                .filter(|(name, _, _)| !is_pie(name) && play_names.contains(*name) == play)
                .take(HITCH_TOP_SPANS)
                .map(|(name, d, _)| ((*name).to_owned(), round_us(ms_of(*d))))
                .collect();
            FrameSample {
                ms: round_us(ms),
                spans,
            }
        })
        .collect()
}

/// The flame bar color of a span that changed by `delta_ms` when the largest change is
/// `max_abs_ms`: grey for no change, toward red for slower (positive), toward green for faster.
#[must_use]
pub fn delta_color(delta_ms: f64, max_abs_ms: f64) -> [u8; 3] {
    let t = if max_abs_ms > 0.0 {
        (delta_ms / max_abs_ms).clamp(-1.0, 1.0)
    } else {
        0.0
    };
    let grey = 150.0;
    let mix = |to: f64, k: f64| (grey + (to - grey) * k).round() as u8;
    if t >= 0.0 {
        [mix(230.0, t), mix(60.0, t), mix(50.0, t)]
    } else {
        [mix(60.0, -t), mix(200.0, -t), mix(80.0, -t)]
    }
}

/// One row of the Profiler's Lockstep view: a system (or the whole tick) of the deterministic
/// tick profile (ADR-8498, ADR-8562). Milliseconds, from the report's nanoseconds.
#[derive(Clone, Debug, PartialEq)]
pub struct LockstepRow {
    /// System name (`"tick"` for the whole-tick row).
    pub system: String,
    /// The row's budget, ms.
    pub budget_ms: f64,
    /// Nearest-rank median, ms.
    pub p50_ms: f64,
    /// Nearest-rank 90th percentile, ms.
    pub p90_ms: f64,
    /// Nearest-rank 99th percentile, ms.
    pub p99_ms: f64,
    /// Slowest retained sample, ms.
    pub max_ms: f64,
    /// The p99 is over the row's budget.
    pub over_budget: bool,
    /// p99 as a fraction of the whole-tick budget (the bar length; 1.0 is the budget line).
    pub bar: f32,
}

/// One over-budget tick as the Lockstep view lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct LockstepTick {
    /// Tick number.
    pub tick: u64,
    /// Whole-tick time, ms.
    pub total_ms: f64,
    /// The slowest system on that tick and its time, ms.
    pub worst: (String, f64),
}

/// What the Lockstep view shows: one row per system in list order, the whole-tick row, the budget
/// line and the over-budget ticks, newest first.
#[derive(Clone, Debug, PartialEq)]
pub struct LockstepView {
    /// `"thread_cpu"` or `"wall"`.
    pub clock: &'static str,
    /// Ticks the ring holds.
    pub samples: u32,
    /// The whole-tick budget (the budget line), ms.
    pub tick_budget_ms: f64,
    /// One row per system, in the profiler's list order.
    pub rows: Vec<LockstepRow>,
    /// The whole tick.
    pub tick: LockstepRow,
    /// Over-budget ticks, newest first.
    pub over_budget: Vec<LockstepTick>,
}

fn ns_ms(ns: u64) -> f64 {
    ns as f64 / 1.0e6
}

/// The Lockstep view of a tick profile report. Pure: tested headless.
#[must_use]
pub fn lockstep_view(report: &regia_framework::sim_host::ProfileReport) -> LockstepView {
    let tick_budget = report.tick_budget_ns;
    let bar = |p99: u64| {
        if tick_budget == 0 {
            0.0
        } else {
            (p99 as f64 / tick_budget as f64) as f32
        }
    };
    let rows = report
        .systems
        .iter()
        .map(|s| LockstepRow {
            system: s.system.clone(),
            budget_ms: ns_ms(s.budget_ns),
            p50_ms: ns_ms(s.p50_ns),
            p90_ms: ns_ms(s.p90_ns),
            p99_ms: ns_ms(s.p99_ns),
            max_ms: ns_ms(s.max_ns),
            over_budget: s.budget_ns > 0 && s.p99_ns > s.budget_ns,
            bar: bar(s.p99_ns),
        })
        .collect();
    let t = report.tick;
    let tick = LockstepRow {
        system: "tick".to_owned(),
        budget_ms: ns_ms(tick_budget),
        p50_ms: ns_ms(t.p50_ns),
        p90_ms: ns_ms(t.p90_ns),
        p99_ms: ns_ms(t.p99_ns),
        max_ms: ns_ms(t.max_ns),
        over_budget: tick_budget > 0 && t.p99_ns > tick_budget,
        bar: bar(t.p99_ns),
    };
    let over_budget = report
        .over_budget
        .iter()
        .rev()
        .map(|o| {
            let worst = o
                .systems
                .iter()
                .max_by_key(|s| s.ns)
                .map_or_else(|| (String::new(), 0.0), |s| (s.system.clone(), ns_ms(s.ns)));
            LockstepTick {
                tick: o.tick,
                total_ms: ns_ms(o.total_ns),
                worst,
            }
        })
        .collect();
    LockstepView {
        clock: report.clock.as_str(),
        samples: report.samples,
        tick_budget_ms: ns_ms(tick_budget),
        rows,
        tick,
        over_budget,
    }
}

/// One line per row, as the view's accessible names read (`"ai: p50 5.000 ms, p99 9.000 ms, max
/// 9.000 ms, budget 4.000 ms, over budget"`).
#[must_use]
pub fn lockstep_row_label(row: &LockstepRow) -> String {
    format!(
        "{}: p50 {:.3} ms, p99 {:.3} ms, max {:.3} ms, budget {:.3} ms{}",
        row.system,
        row.p50_ms,
        row.p99_ms,
        row.max_ms,
        row.budget_ms,
        if row.over_budget { ", over budget" } else { "" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(name: &str, inclusive: f64, children: Vec<CaptureNode>) -> CaptureNode {
        let child: f64 = children.iter().map(|c| c.inclusive_ms).sum();
        CaptureNode {
            name: name.into(),
            inclusive_ms: inclusive,
            self_ms: round_us(inclusive - child),
            calls: 1.0,
            children,
        }
    }

    /// The editor frame: `editor_frame` 9 ms holding the UI (3) and `pie_frame` (5), which holds
    /// a play-only system span (4); a second root `pie_client_frame` (1).
    fn capture() -> ProfileCapture {
        ProfileCapture {
            format_version: 1,
            frames: 1,
            frame_ms: 10.0,
            cpu: vec![
                n(
                    "editor_frame",
                    9.0,
                    vec![
                        n("pie_frame", 5.0, vec![n("play_only_system", 4.0, vec![])]),
                        n("editor_ui", 3.0, vec![]),
                    ],
                ),
                n("pie_client_frame", 1.0, vec![n("client_tick", 1.0, vec![])]),
            ],
            truncated: 0,
            gpu: Vec::new(),
            folded: String::new(),
            timeline: Vec::new(),
            timeline_truncated: 0,
        }
    }

    fn names(c: &ProfileCapture) -> Vec<String> {
        fn walk(nodes: &[CaptureNode], out: &mut Vec<String>) {
            for node in nodes {
                out.push(node.name.clone());
                walk(&node.children, out);
            }
        }
        let mut out = Vec::new();
        walk(&c.cpu, &mut out);
        out
    }

    #[test]
    fn a_play_span_shows_under_the_play_source_and_not_under_the_edit_source() {
        let play = split_source(&capture(), true);
        let edit = split_source(&capture(), false);
        assert!(names(&play).contains(&"play_only_system".to_owned()));
        assert!(!names(&edit).contains(&"play_only_system".to_owned()));
        assert_eq!(names(&edit), ["editor_frame", "editor_ui"]);
        assert_eq!(play.frame_ms, 6.0, "pie_frame 5 + pie_client_frame 1");
        assert_eq!(edit.frame_ms, 4.0);
        assert_eq!(
            edit.cpu[0].inclusive_ms, 4.0,
            "the editor frame loses its play time"
        );
        assert_eq!(
            play.cpu.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["pie_frame", "pie_client_frame"]
        );
        assert!(
            play.folded.contains("pie_frame;play_only_system 4\n"),
            "{}",
            play.folded
        );
    }

    #[test]
    fn top_span_names_rank_by_inclusive_time() {
        assert_eq!(
            top_span_names(&capture(), 3),
            ["editor_frame", "pie_frame", "play_only_system"]
        );
    }

    #[test]
    fn hitches_are_frames_over_twice_the_median() {
        let sample = |ms: f64| FrameSample {
            ms,
            spans: Vec::new(),
        };
        let samples: Vec<FrameSample> = [8.0, 8.0, 9.0, 30.0, 8.0, 17.0, 8.0].map(sample).to_vec();
        assert_eq!(median_ms(&samples), 8.0);
        let threshold = hitch_threshold_ms(&samples);
        assert_eq!(threshold, 16.0);
        assert_eq!(hitches(&samples, threshold), [3, 5]);
        let idle: Vec<FrameSample> = [0.1, 0.1, 0.3].map(sample).to_vec();
        assert_eq!(
            hitch_threshold_ms(&idle),
            HITCH_MIN_MS,
            "an idle editor has no hitches"
        );
        assert!(hitches(&idle, hitch_threshold_ms(&idle)).is_empty());
        assert_eq!(median_ms(&[]), 0.0);
    }

    #[test]
    fn entity_counts_rank_components_by_entities() {
        #[derive(Component)]
        struct Wide;
        #[derive(Component)]
        struct Narrow;
        let mut world = World::new();
        for k in 0..5 {
            if k < 2 {
                world.spawn((Wide, Narrow));
            } else {
                world.spawn(Wide);
            }
        }
        let counts = entity_counts(&world, TOP_COMPONENTS);
        assert_eq!(counts.entities, world.entities().count_spawned());
        assert!(counts.archetypes >= 3);
        let rank: Vec<(&str, u64)> = counts
            .components
            .iter()
            .filter(|(name, _)| name.ends_with("Wide") || name.ends_with("Narrow"))
            .map(|(name, k)| (name.rsplit("::").next().unwrap_or(name), *k))
            .collect();
        assert_eq!(rank, [("Wide", 5), ("Narrow", 2)]);
        assert!(entity_counts(&world, 1).components.len() <= 1);
    }

    #[test]
    fn local_history_reads_play_time_per_frame() {
        use tracing_subscriber::layer::SubscriberExt;
        let profiler = regia_core::profiling::FrameProfiler::new();
        let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
        tracing::dispatcher::with_default(&dispatch, || {
            for play in [false, true] {
                let _ui = tracing::info_span!("editor_ui").entered();
                if play {
                    let _pie = tracing::info_span!("pie_frame").entered();
                    let _sys = tracing::info_span!("play_only_system").entered();
                }
                drop(_ui);
                profiler.end_frame();
            }
        });
        let play_names: std::collections::BTreeSet<String> = ["play_only_system".to_owned()].into();
        let edit = local_history(&profiler, false, &play_names);
        let play = local_history(&profiler, true, &play_names);
        assert_eq!((edit.len(), play.len()), (2, 2));
        assert_eq!(play[0].ms, 0.0, "no play in the first frame");
        assert!(play[1].spans.iter().any(|(n, _)| n == "play_only_system"));
        assert!(play[1].spans.iter().all(|(n, _)| n != "pie_frame"));
        assert!(edit[0].spans.iter().any(|(n, _)| n == "editor_ui"));
        assert!(
            play[1].spans.iter().all(|(n, _)| n != "editor_ui"),
            "edit spans stay out of play"
        );
        assert!(
            edit[1].spans.iter().all(|(n, _)| n != "play_only_system"),
            "play spans stay out of edit"
        );
    }

    #[test]
    fn delta_colors_run_from_green_through_grey_to_red() {
        assert_eq!(delta_color(0.0, 2.0), [150, 150, 150]);
        assert_eq!(delta_color(2.0, 2.0), [230, 60, 50]);
        assert_eq!(delta_color(-2.0, 2.0), [60, 200, 80]);
        assert_eq!(delta_color(5.0, 0.0), [150, 150, 150], "no change anywhere");
        let half = delta_color(1.0, 2.0);
        assert!(half[0] > 150 && half[1] < 150);
    }
}

#[cfg(test)]
mod lockstep_tests {
    use regia_framework::sim_host::ProfileReport;

    use super::*;

    fn report() -> ProfileReport {
        serde_json::from_value(serde_json::json!({
            "clock": "wall",
            "samples": 3,
            "tick_budget_ns": 4_000_000,
            "system_budget_ns": 2_000_000,
            "systems": [
                { "system": "move", "budget_ns": 2_000_000, "p50_ns": 1_000_000, "p90_ns": 1_000_000, "p99_ns": 1_000_000, "max_ns": 1_000_000 },
                { "system": "ai", "budget_ns": 2_000_000, "p50_ns": 5_000_000, "p90_ns": 9_000_000, "p99_ns": 9_000_000, "max_ns": 9_000_000 },
                { "system": "hash", "budget_ns": 2_000_000, "p50_ns": 0, "p90_ns": 0, "p99_ns": 0, "max_ns": 0 }
            ],
            "tick": { "p50_ns": 6_000_000, "p90_ns": 10_000_000, "p99_ns": 10_000_000, "max_ns": 10_000_000 },
            "over_budget": [
                { "tick": 2, "total_ns": 6_000_000, "systems": [ { "system": "move", "ns": 1_000_000 }, { "system": "ai", "ns": 5_000_000 }, { "system": "hash", "ns": 0 } ] },
                { "tick": 3, "total_ns": 10_000_000, "systems": [ { "system": "move", "ns": 1_000_000 }, { "system": "ai", "ns": 9_000_000 }, { "system": "hash", "ns": 0 } ] }
            ]
        }))
        .expect("a report")
    }

    #[test]
    fn lockstep_profile_view_shows_every_system_row() {
        let report = report();
        let view = lockstep_view(&report);
        let names: Vec<&str> = view.rows.iter().map(|r| r.system.as_str()).collect();
        assert_eq!(
            names,
            ["move", "ai", "hash"],
            "every system, in list order, idle ones too"
        );
        assert_eq!(view.clock, "wall");
        assert_eq!(view.samples, 3);
        assert_eq!(view.tick_budget_ms, 4.0, "the budget line");
        let ai = &view.rows[1];
        assert_eq!((ai.p50_ms, ai.p99_ms, ai.max_ms), (5.0, 9.0, 9.0));
        assert!(ai.over_budget && !view.rows[0].over_budget && !view.rows[2].over_budget);
        assert_eq!(view.rows[0].bar, 0.25);
        assert!(view.tick.over_budget);
        assert_eq!(view.tick.system, "tick");
        let ticks: Vec<u64> = view.over_budget.iter().map(|t| t.tick).collect();
        assert_eq!(ticks, [3, 2], "over-budget ticks, newest first");
        assert_eq!(view.over_budget[0].worst, ("ai".to_owned(), 9.0));
        assert_eq!(
            lockstep_row_label(ai),
            "ai: p50 5.000 ms, p99 9.000 ms, max 9.000 ms, budget 2.000 ms, over budget"
        );
    }
}
