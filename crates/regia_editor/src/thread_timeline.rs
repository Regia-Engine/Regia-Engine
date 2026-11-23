// SPDX-License-Identifier: MPL-2.0
//! The Profiler panel's Timeline tab (`docs/specs/83-prefab-profiler-volumes.md` § 9, ADR-7325):
//! one frame of the frame profiler's per-thread timeline, one block of rows per thread, span bars
//! nested by depth, zoom about the pointer, pan, hover with name and duration, click to hold the
//! frame.
//!
//! The geometry is [`thread_timeline_layout`], a pure function of the frame and the view (the
//! pattern of [`crate::trace_timeline::trace_layout`]), so it is tested without a GPU. Sub-pixel
//! bars merge ([`crate::timeline_geometry::merge_subpixel_bars`]): a row holds at most one bar per
//! pixel column, and a merged bar carries its count. The panel keeps the layout in a
//! [`LayoutCache`] keyed by the frame and the view, so a steady repaint does not rebuild it.

use regia::telemetry::profile_capture::{CaptureSpan, CaptureThread, CaptureTimelineFrame};
use serde_json::{Value, json};

use crate::timeline_geometry::{LayoutCache, PixelBar, merge_subpixel_bars, view_key};
use crate::trace_timeline::TimelineView;

/// Narrowest bar drawn, points (a merged run is never narrower than one pixel column).
pub const MIN_BAR_WIDTH: f32 = 1.0;

/// Height of one timeline row, points.
pub const ROW_HEIGHT: f32 = 16.0;

/// One thread's rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadRows {
    /// The thread's name.
    pub name: String,
    /// First row of the thread.
    pub first_row: usize,
    /// Rows the thread takes (its deepest span plus one).
    pub rows: usize,
}

/// One drawn bar.
#[derive(Clone, Debug, PartialEq)]
pub struct ThreadRect {
    /// The span name (the first span's, for a merged bar).
    pub name: String,
    /// Index into [`ThreadLayout::threads`].
    pub thread: usize,
    /// Row from the top.
    pub row: usize,
    /// Nesting depth on its thread.
    pub depth: u16,
    /// Left edge, points from the view's left (clipped to the view).
    pub x: f32,
    /// Width, points.
    pub width: f32,
    /// Start, ns from the frame's start (the first span's, for a merged bar).
    pub start_ns: u64,
    /// End, ns (the last end, for a merged bar).
    pub end_ns: u64,
    /// Spans the bar stands for (1 = one span; more = sub-pixel spans merged).
    pub count: u32,
}

impl ThreadRect {
    /// The bar's label: the span name, or `<count> spans` for a merged bar.
    #[must_use]
    pub fn label(&self) -> String {
        if self.count > 1 {
            format!("{} spans", self.count)
        } else {
            self.name.clone()
        }
    }

    /// Duration, ms.
    #[must_use]
    pub fn duration_ms(&self) -> f64 {
        self.end_ns.saturating_sub(self.start_ns) as f64 / 1.0e6
    }
}

/// A laid-out frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ThreadLayout {
    /// The threads, in the frame's order.
    pub threads: Vec<ThreadRows>,
    /// The bars, by row, then left edge.
    pub rects: Vec<ThreadRect>,
    /// Spans in the visible window.
    pub visible_spans: usize,
    /// Rows in total.
    pub rows: usize,
    /// The time shown at the left and right edges, ns from the frame's start.
    pub window_ns: (u64, u64),
}

/// The time span the frame covers: its duration, or its last span end when that is later.
fn frame_span_ns(frame: &CaptureTimelineFrame) -> u64 {
    frame
        .threads
        .iter()
        .flat_map(|t| t.spans.iter().map(|s| s.end_ns))
        .fold(frame.frame_ns, u64::max)
        .max(1)
}

/// Lays `frame` out in `view`: threads one under another in the frame's order, each taking one
/// row per depth; x and width from the visible time window; spans wholly outside it are left out,
/// the rest clipped to it; sub-pixel spans merged so a row holds at most one bar per pixel column.
/// Deterministic for a given frame and view.
#[must_use]
pub fn thread_timeline_layout(frame: &CaptureTimelineFrame, view: &TimelineView) -> ThreadLayout {
    let _span = tracing::info_span!("thread_timeline_layout").entered();
    let view = view.clamped();
    let mut threads = Vec::with_capacity(frame.threads.len());
    let mut row = 0;
    for thread in &frame.threads {
        let rows = thread
            .spans
            .iter()
            .map(|s| usize::from(s.depth))
            .max()
            .unwrap_or(0)
            + 1;
        threads.push(ThreadRows {
            name: thread.name.clone(),
            first_row: row,
            rows,
        });
        row += rows;
    }
    let span = frame_span_ns(frame) as f64;
    let visible = span / f64::from(view.zoom);
    let start = f64::from(view.pan) * (span - visible);
    let width = f64::from(view.width);
    let mut layout = ThreadLayout {
        threads,
        rects: Vec::new(),
        visible_spans: 0,
        rows: row,
        window_ns: (start as u64, (start + visible) as u64),
    };
    if width <= 0.0 {
        return layout;
    }
    let scale = width / visible;
    let at = |t: u64| (t as f64 - start) * scale;
    // Every span in the window, with its thread and the span it is.
    let mut spans: Vec<(usize, &CaptureSpan)> = Vec::new();
    let mut bars: Vec<PixelBar> = Vec::new();
    for (t, thread) in frame.threads.iter().enumerate() {
        let first_row = layout.threads[t].first_row;
        for s in &thread.spans {
            let (x0, x1) = (at(s.start_ns), at(s.end_ns.max(s.start_ns)));
            if x1 < 0.0 || x0 > width {
                continue;
            }
            bars.push(PixelBar {
                row: first_row + usize::from(s.depth),
                x0,
                x1,
                t0: s.start_ns,
                t1: s.end_ns,
                item: spans.len(),
                count: 1,
            });
            spans.push((t, s));
        }
    }
    layout.visible_spans = spans.len();
    bars.sort_by(|a, b| {
        a.row
            .cmp(&b.row)
            .then(a.x0.total_cmp(&b.x0))
            .then(a.item.cmp(&b.item))
    });
    let limit = view.width;
    for bar in merge_subpixel_bars(&bars) {
        let (thread, first) = spans[bar.item];
        let left = bar.x0.max(0.0) as f32;
        let right = bar.x1.min(width) as f32;
        let mut w = (right - left).max(MIN_BAR_WIDTH);
        if w > limit {
            w = limit;
        }
        let x = if left + w > limit {
            (limit - w).max(0.0)
        } else {
            left
        };
        layout.rects.push(ThreadRect {
            name: first.name.clone(),
            thread,
            row: bar.row,
            depth: first.depth,
            x,
            width: w,
            start_ns: bar.t0,
            end_ns: bar.t1,
            count: bar.count,
        });
    }
    layout
}

/// The bar under `(x, row)`, if any.
#[must_use]
pub fn rect_at(layout: &ThreadLayout, x: f32, row: usize) -> Option<&ThreadRect> {
    layout
        .rects
        .iter()
        .find(|r| r.row == row && x >= r.x && x < r.x + r.width.max(1.0))
}

/// The frame a point on a strip's time axis lies in: the frame whose `CaptureStripFrame::start_ns`
/// is the latest at or before `t_ns` (hover names the frame a bar belongs to; ADR-7409).
#[must_use]
pub fn frame_at_offset(
    frames: &[regia::telemetry::profile_capture::CaptureStripFrame],
    t_ns: u64,
) -> Option<u64> {
    frames.iter().rev().find(|f| f.start_ns <= t_ns).map(|f| f.number)
}

/// The strip frame under view x `x` (points from the timeline's left edge) of `layout`, drawn
/// `width` points wide: the layout's time-to-x mapping inverted, then [`frame_at_offset`]. A
/// click on a strip holds the frame it lands in, not the strip's newest (w11-profiler-finish).
#[must_use]
pub fn frame_at_x(
    layout: &ThreadLayout,
    frames: &[regia::telemetry::profile_capture::CaptureStripFrame],
    x: f32,
    width: f32,
) -> Option<u64> {
    let (start, end) = layout.window_ns;
    let width = f64::from(width.max(1.0));
    let along = f64::from(x).clamp(0.0, width) / width;
    let t = start as f64 + along * end.saturating_sub(start) as f64;
    frame_at_offset(frames, t as u64)
}

/// Where each mark of a strip is drawn (P-318): `(x, label)` for each `(frame number, label)` of
/// `marks` whose frame is in `frames` and whose frame start is inside the view, x points from the
/// timeline's left edge of a view `width` points wide (the inverse of [`frame_at_x`]). A mark sits
/// at the start of the frame it was recorded in.
#[must_use]
pub fn mark_xs(
    layout: &ThreadLayout,
    frames: &[regia::telemetry::profile_capture::CaptureStripFrame],
    marks: &[(u64, String)],
    width: f32,
) -> Vec<(f32, String)> {
    let (start, end) = layout.window_ns;
    let span = end.saturating_sub(start).max(1) as f64;
    let width = f64::from(width.max(1.0));
    marks
        .iter()
        .filter_map(|(number, label)| {
            let frame = frames.iter().find(|f| f.number == *number)?;
            let at = frame.start_ns;
            (start..=end).contains(&at).then(|| {
                (
                    (at.saturating_sub(start) as f64 / span * width) as f32,
                    label.clone(),
                )
            })
        })
        .collect()
}

/// The cache key of a layout: the source (`edit`, `pie`, a process), the frame number, its span
/// count, and the view.
pub type ThreadLayoutKey = (String, u64, usize, (u32, u32, u32));

/// The Timeline tab's kept layout.
pub type ThreadLayoutCache = LayoutCache<ThreadLayoutKey, ThreadLayout>;

/// The layout of `frame` in `view` for `source`, from `cache` while neither changed.
pub fn cached_layout(
    cache: &mut ThreadLayoutCache,
    source: &str,
    frame: &CaptureTimelineFrame,
    view: &TimelineView,
) -> std::sync::Arc<ThreadLayout> {
    let view = view.clamped();
    let key = (
        source.to_owned(),
        frame.number,
        frame.span_count(),
        view_key(&view),
    );
    cache.get_or_build(key, || thread_timeline_layout(frame, &view))
}

/// The play part (`play`) or the editor part of a frame: a span is play when it lies inside a
/// play span ([`crate::profiler_model::PIE_SPANS`]) of any thread, in time, or is one. Threads left
/// with no spans are dropped.
#[must_use]
pub fn split_timeline(frame: &CaptureTimelineFrame, play: bool) -> CaptureTimelineFrame {
    let intervals: Vec<(u64, u64)> = frame
        .threads
        .iter()
        .flat_map(|t| t.spans.iter())
        .filter(|s| crate::profiler_model::PIE_SPANS.contains(&s.name.as_str()))
        .map(|s| (s.start_ns, s.end_ns))
        .collect();
    let in_play = |s: &CaptureSpan| intervals.iter().any(|&(a, b)| s.start_ns >= a && s.end_ns <= b);
    let threads = frame
        .threads
        .iter()
        .filter_map(|t| {
            let spans: Vec<CaptureSpan> = t.spans.iter().filter(|s| in_play(s) == play).cloned().collect();
            (!spans.is_empty()).then(|| CaptureThread {
                index: t.index,
                name: t.name.clone(),
                spans,
            })
        })
        .collect();
    CaptureTimelineFrame {
        number: frame.number,
        frame_ns: frame.frame_ns,
        dropped: frame.dropped,
        threads,
    }
}

/// A `profiler.timeline` answer as a frame.
#[must_use]
pub fn frame_from_answer(answer: &Value) -> Option<CaptureTimelineFrame> {
    let value = json!({
        "number": answer.get("frame")?,
        "frame_ns": answer.get("frame_ns")?,
        "dropped": answer.get("dropped").cloned().unwrap_or(json!(0)),
        "threads": answer.get("threads")?,
    });
    serde_json::from_value(value).ok()
}

/// The JSON `editor.profiler_timeline` returns for a laid-out frame.
#[must_use]
pub fn layout_json(frame: &CaptureTimelineFrame, layout: &ThreadLayout, max_rects: usize) -> Value {
    let merged = layout.rects.iter().filter(|r| r.count > 1).count();
    json!({
        "frame": frame.number,
        "frame_ms": (frame.frame_ns as f64 / 1e3).round() / 1e3,
        "dropped": frame.dropped,
        "spans": frame.span_count(),
        "visible_spans": layout.visible_spans,
        "window_ns": [layout.window_ns.0, layout.window_ns.1],
        "threads": layout.threads.iter().map(|t| json!({
            "name": t.name,
            "first_row": t.first_row,
            "rows": t.rows,
        })).collect::<Vec<_>>(),
        "rects": layout.rects.len(),
        "merged_rects": merged,
        "bars": layout.rects.iter().take(max_rects).map(|r| json!({
            "label": r.label(),
            "thread": layout.threads.get(r.thread).map(|t| t.name.clone()),
            "row": r.row,
            "depth": r.depth,
            "x": r.x,
            "width": r.width,
            "start_ns": r.start_ns,
            "end_ns": r.end_ns,
            "count": r.count,
        })).collect::<Vec<_>>(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(name: &str, start_ns: u64, end_ns: u64, depth: u16) -> CaptureSpan {
        CaptureSpan {
            name: name.into(),
            start_ns,
            end_ns,
            depth,
        }
    }

    fn frame() -> CaptureTimelineFrame {
        CaptureTimelineFrame {
            number: 12,
            frame_ns: 10_000_000,
            dropped: 0,
            threads: vec![
                CaptureThread {
                    index: 0,
                    name: "main".into(),
                    spans: vec![
                        span("editor_frame", 0, 10_000_000, 0),
                        span("pie_frame", 2_000_000, 7_000_000, 1),
                        span("play_update", 3_000_000, 5_000_000, 2),
                        span("ui", 8_000_000, 9_000_000, 1),
                    ],
                },
                CaptureThread {
                    index: 3,
                    name: "regia-compute (0)".into(),
                    spans: vec![span("play_system", 3_500_000, 4_500_000, 0)],
                },
            ],
        }
    }

    #[test]
    fn threads_get_rows_by_depth_and_bars_sit_at_their_times() {
        let view = TimelineView {
            zoom: 1.0,
            pan: 0.0,
            width: 1000.0,
        };
        let layout = thread_timeline_layout(&frame(), &view);
        assert_eq!(
            layout.threads,
            [
                ThreadRows {
                    name: "main".into(),
                    first_row: 0,
                    rows: 3
                },
                ThreadRows {
                    name: "regia-compute (0)".into(),
                    first_row: 3,
                    rows: 1
                },
            ]
        );
        let by = |name: &str| layout.rects.iter().find(|r| r.name == name).cloned().expect(name);
        let pie = by("pie_frame");
        assert_eq!((pie.row, pie.x, pie.width), (1, 200.0, 500.0));
        let worker = by("play_system");
        assert_eq!((worker.row, worker.thread, worker.x), (3, 1, 350.0));
        assert_eq!(by("play_update").depth, 2);
        assert!((pie.duration_ms() - 5.0).abs() < 1e-9);
        assert_eq!(
            rect_at(&layout, 250.0, 1).map(|r| r.name.as_str()),
            Some("pie_frame")
        );
        assert!(rect_at(&layout, 100.0, 1).is_none());
        assert_eq!(layout, thread_timeline_layout(&frame(), &view), "deterministic");
        // Zoomed into the second half: the first half's bars are culled or clipped.
        let zoomed = thread_timeline_layout(
            &frame(),
            &TimelineView {
                zoom: 2.0,
                pan: 1.0,
                width: 1000.0,
            },
        );
        assert_eq!(zoomed.window_ns, (5_000_000, 10_000_000));
        assert!(zoomed.rects.iter().all(|r| r.name != "play_system"));
        let pie = zoomed
            .rects
            .iter()
            .find(|r| r.name == "pie_frame")
            .expect("clipped");
        assert_eq!((pie.x, pie.width), (0.0, 400.0));
    }

    #[test]
    fn play_and_edit_split_by_the_play_spans() {
        let play = split_timeline(&frame(), true);
        let names: Vec<&str> = play
            .threads
            .iter()
            .flat_map(|t| t.spans.iter().map(|s| s.name.as_str()))
            .collect();
        assert_eq!(names, ["pie_frame", "play_update", "play_system"]);
        let edit = split_timeline(&frame(), false);
        assert_eq!(edit.threads.len(), 1, "the worker ran play only");
        assert_eq!(edit.span_count(), 2);
    }

    /// ADR-7325 acceptance: 100 000 spans on four threads lay out with at most one bar per pixel
    /// column per row, merged bars carry their counts (every span is accounted for), and a second
    /// call with the same frame and view is a cache hit.
    #[test]
    fn a_hundred_thousand_spans_lay_out_one_bar_per_pixel_column_and_cache() {
        let mut threads = Vec::new();
        for t in 0..4u32 {
            let mut spans = Vec::new();
            // 25 000 spans per thread: 5 000 roots of 2 µs, each with 4 nested children.
            for i in 0..5_000u64 {
                let start = i * 2_000;
                spans.push(span("root", start, start + 1_900, 0));
                for d in 1..5u16 {
                    let s = start + u64::from(d) * 100;
                    spans.push(span("child", s, s + 1_000 - u64::from(d) * 100, d));
                }
            }
            threads.push(CaptureThread {
                index: t,
                name: format!("worker {t}"),
                spans,
            });
        }
        let frame = CaptureTimelineFrame {
            number: 1,
            frame_ns: 10_000_000,
            dropped: 0,
            threads,
        };
        assert_eq!(frame.span_count(), 100_000);
        let view = TimelineView {
            zoom: 1.0,
            pan: 0.0,
            width: 800.0,
        };
        let mut cache = ThreadLayoutCache::default();
        let layout = cached_layout(&mut cache, "edit", &frame, &view);
        assert_eq!(layout.visible_spans, 100_000);
        let total: u64 = layout.rects.iter().map(|r| u64::from(r.count)).sum();
        assert_eq!(total, 100_000, "every span is in exactly one bar");
        let mut per_row: std::collections::BTreeMap<usize, Vec<i64>> = Default::default();
        for r in &layout.rects {
            per_row.entry(r.row).or_default().push(r.x.floor() as i64);
        }
        assert_eq!(per_row.len(), 20, "4 threads x 5 depths");
        for (row, mut columns) in per_row {
            let n = columns.len();
            assert!(n <= 800, "row {row}: {n} bars for 800 columns");
            columns.dedup();
            assert_eq!(columns.len(), n, "row {row}: two bars start in one pixel column");
        }
        assert!(
            layout
                .rects
                .iter()
                .any(|r| r.count > 1 && r.label().ends_with(" spans"))
        );
        let again = cached_layout(&mut cache, "edit", &frame, &view);
        assert!(std::sync::Arc::ptr_eq(&layout, &again));
        assert_eq!(
            (cache.builds(), cache.hits()),
            (1, 1),
            "the second call is a cache hit"
        );
        // Zoomed in far enough, bars are wider than a pixel and stand alone.
        let near = TimelineView {
            zoom: 10_000.0,
            pan: 0.0,
            width: 800.0,
        };
        let close = cached_layout(&mut cache, "edit", &frame, &near);
        assert_eq!(cache.builds(), 2);
        assert!(
            close.rects.iter().all(|r| r.count == 1),
            "{:?}",
            &close.rects[..4]
        );
    }

    #[test]
    fn answers_convert_and_the_json_names_the_rows() {
        let answer = json!({
            "frame": 12, "frame_ns": 10_000_000, "dropped": 0,
            "threads": [{ "index": 0, "name": "main", "spans": [
                { "name": "editor_frame", "start_ns": 0, "end_ns": 10_000_000, "depth": 0 }
            ] }],
        });
        let frame = frame_from_answer(&answer).expect("frame");
        assert_eq!(frame.span_count(), 1);
        assert!(frame_from_answer(&json!({})).is_none());
        let layout = thread_timeline_layout(&frame, &TimelineView::default());
        let out = layout_json(&frame, &layout, 10);
        assert_eq!(out["threads"][0]["name"], "main");
        assert_eq!(out["bars"][0]["label"], "editor_frame");
        assert_eq!(out["rects"], 1);
    }

    /// ADR-7325 (w6): three frames of a strip lay out on one time axis — frame k's first bar
    /// starts at or after frame k-1's last bar's end on the same row, and the strip's frame
    /// starts are exactly where each frame's bars begin.
    #[test]
    fn three_frames_of_a_strip_lay_out_in_order_with_boundaries_at_the_starts() {
        let make = |number: u64, tag: &'static str| {
            // The frame's root bar starts at the frame's start (the boundary), children inside.
            let mut spans = vec![span(tag, 0, 3_600_000, 0)];
            for i in 0..3u64 {
                spans.push(span("child", 400_000 + i * 900_000, 900_000 + i * 900_000, 1));
            }
            CaptureTimelineFrame {
                number,
                frame_ns: 4_000_000,
                dropped: 0,
                threads: vec![CaptureThread {
                    index: 0,
                    name: "main".into(),
                    spans,
                }],
            }
        };
        let strip =
            regia::telemetry::profile_capture::timeline_strip(&[make(4, "f4"), make(5, "f5"), make(6, "f6")]);
        assert_eq!(strip.frames.len(), 3);
        let view = TimelineView {
            zoom: 1.0,
            pan: 0.0,
            width: 1200.0,
        };
        let layout = thread_timeline_layout(&strip.combined, &view);
        let bar = |name: &str| layout.rects.iter().find(|r| r.name == name).cloned().expect(name);
        // Frame k's root bar starts at or after frame k-1's root bar ends (same row).
        let (f4, f5, f6) = (bar("f4"), bar("f5"), bar("f6"));
        assert_eq!((f4.row, f4.thread), (0, 0));
        assert!(f5.x >= f4.x + f4.width, "{f4:?} then {f5:?}");
        assert!(f6.x >= f5.x + f5.width, "{f5:?} then {f6:?}");
        // The boundary (the strip's frame start) is where that frame's bars begin.
        let total = strip.combined.frame_ns as f64;
        let at = |t: u64| (t as f64 / total * 1200.0) as f32;
        for (rect, frame) in [
            (&f4, &strip.frames[0]),
            (&f5, &strip.frames[1]),
            (&f6, &strip.frames[2]),
        ] {
            let boundary = at(frame.start_ns);
            assert!(
                (rect.x - boundary).abs() < 0.5,
                "frame {} starts at its boundary: {:?} vs {boundary}",
                frame.number,
                rect
            );
        }
        // The children of every frame nest on the row below, inside their frame's extent.
        let children: Vec<&crate::thread_timeline::ThreadRect> = layout
            .rects
            .iter()
            .filter(|r| r.name == "child" && r.count == 1)
            .collect();
        assert_eq!(children.len(), 9, "three per frame");
        for child in children {
            let inside = [f4.clone(), f5.clone(), f6.clone()]
                .iter()
                .any(|root| child.x >= root.x && child.x + child.width <= root.x + root.width);
            assert!(inside, "{child:?}");
        }
        // Hover names the frame a bar belongs to.
        for (rect, frame) in [
            (&f4, &strip.frames[0]),
            (&f5, &strip.frames[1]),
            (&f6, &strip.frames[2]),
        ] {
            assert_eq!(frame_at_offset(&strip.frames, rect.start_ns), Some(frame.number));
        }
        assert_eq!(frame_at_offset(&strip.frames, 0), Some(4));
        assert_eq!(frame_at_offset(&[], 100), None);
        // A click's x lands in the frame drawn there (w11-profiler-finish).
        for (rect, frame) in [
            (&f4, &strip.frames[0]),
            (&f5, &strip.frames[1]),
            (&f6, &strip.frames[2]),
        ] {
            let mid = rect.x + rect.width / 2.0;
            assert_eq!(
                frame_at_x(&layout, &strip.frames, mid, 1200.0),
                Some(frame.number)
            );
        }
        assert_eq!(
            frame_at_x(&layout, &strip.frames, -5.0, 1200.0),
            Some(4),
            "clamped left"
        );
        assert_eq!(
            frame_at_x(&layout, &strip.frames, 5000.0, 1200.0),
            Some(6),
            "clamped right"
        );
    }

    /// P-318: a mark of a strip frame is drawn at that frame's boundary; a mark of a frame outside
    /// the strip is not drawn.
    #[test]
    fn marks_sit_at_the_start_of_their_strip_frame() {
        let make = |number: u64| CaptureTimelineFrame {
            number,
            frame_ns: 4_000_000,
            dropped: 0,
            threads: vec![CaptureThread {
                index: 0,
                name: "main".into(),
                spans: vec![span("root", 0, 3_600_000, 0)],
            }],
        };
        let strip = regia::telemetry::profile_capture::timeline_strip(&[make(4), make(5), make(6)]);
        let view = TimelineView {
            zoom: 1.0,
            pan: 0.0,
            width: 1200.0,
        };
        let layout = thread_timeline_layout(&strip.combined, &view);
        let marks = vec![(5, "boss spawned".to_owned()), (9, "elsewhere".to_owned())];
        let xs = mark_xs(&layout, &strip.frames, &marks, 1200.0);
        assert_eq!(xs.len(), 1, "frame 9 is not on the strip: {xs:?}");
        let boundary = (strip.frames[1].start_ns as f64 / strip.combined.frame_ns as f64 * 1200.0) as f32;
        assert!((xs[0].0 - boundary).abs() < 0.5, "{xs:?} vs {boundary}");
        assert_eq!(xs[0].1, "boss spawned");
        assert_eq!(
            frame_at_x(&layout, &strip.frames, xs[0].0 + 1.0, 1200.0),
            Some(5),
            "the mark is drawn inside its own frame"
        );
    }

    /// ADR-7325 (w6): 100 000 spans across a four-frame strip still lay out with at most one bar
    /// per pixel column per row, every span in exactly one bar (the one-frame pattern).
    #[test]
    fn a_hundred_thousand_span_strip_merges_one_bar_per_pixel_column() {
        let mut frames = Vec::new();
        for f in 0..4u64 {
            let mut spans = Vec::new();
            for i in 0..5_000u64 {
                let start = i * 2_000;
                spans.push(span("root", start, start + 1_900, 0));
                for d in 1..5u16 {
                    let s = start + u64::from(d) * 100;
                    spans.push(span("child", s, s + 1_000 - u64::from(d) * 100, d));
                }
            }
            frames.push(CaptureTimelineFrame {
                number: f,
                frame_ns: 10_000_000,
                dropped: 0,
                threads: vec![CaptureThread {
                    index: 0,
                    name: "main".into(),
                    spans,
                }],
            });
        }
        let strip = regia::telemetry::profile_capture::timeline_strip(&frames);
        assert_eq!(strip.combined.span_count(), 100_000);
        let layout = thread_timeline_layout(
            &strip.combined,
            &TimelineView {
                zoom: 1.0,
                pan: 0.0,
                width: 800.0,
            },
        );
        assert_eq!(layout.visible_spans, 100_000);
        let total: u64 = layout.rects.iter().map(|r| u64::from(r.count)).sum();
        assert_eq!(total, 100_000, "every span is in exactly one bar");
        let mut per_row: std::collections::BTreeMap<usize, Vec<i64>> = Default::default();
        for r in &layout.rects {
            per_row.entry(r.row).or_default().push(r.x.floor() as i64);
        }
        assert_eq!(per_row.len(), 5, "5 depths");
        for (row, mut columns) in per_row {
            let n = columns.len();
            assert!(n <= 800, "row {row}: {n} bars for 800 columns");
            columns.dedup();
            assert_eq!(columns.len(), n, "row {row}: two bars start in one pixel column");
        }
    }
}
