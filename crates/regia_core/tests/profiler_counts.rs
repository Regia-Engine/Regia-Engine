// SPDX-License-Identifier: MPL-2.0
//! `FrameProfiler` bookkeeping checked by span *counts* and names only (no durations, so nothing here
//! depends on how fast the machine is): frames, history bound, `clear`, enable / disable around a span,
//! re-entered spans, and the `averages` / `top` argument edges.

use regia_core::profiling::{FrameProfiler, HISTORY_FRAMES, MAX_FLAME_EDGES_PER_FRAME, scope};
use tracing_subscriber::layer::SubscriberExt;

fn with_profiler(profiler: &FrameProfiler, f: impl FnOnce()) {
    let subscriber = tracing_subscriber::registry().with(profiler.clone());
    tracing::subscriber::with_default(subscriber, f);
}

fn names(list: &[(&'static str, std::time::Duration)]) -> Vec<&'static str> {
    let mut out: Vec<_> = list.iter().map(|(n, _)| *n).collect();
    out.sort_unstable();
    out
}

#[test]
fn a_frame_without_spans_is_still_a_frame() {
    let profiler = FrameProfiler::new();
    assert_eq!(profiler.frame_count(), 0);
    assert_eq!(profiler.last_frame_time(), None);
    assert_eq!(profiler.last_frame_span_count(), 0);
    assert_eq!(profiler.history_span_count(), 0);
    assert!(profiler.last_frame().is_empty() && profiler.top(5).is_empty());
    profiler.end_frame();
    assert_eq!(profiler.frame_count(), 1);
    assert!(profiler.last_frame_time().is_some());
    assert!(profiler.last_frame().is_empty());
    assert_eq!(profiler.averages(10), Vec::new(), "no span ever ran");
}

#[test]
fn span_counts_follow_the_frames() {
    let profiler = FrameProfiler::new();
    with_profiler(&profiler, || {
        for _ in 0..3 {
            scope!("a");
        }
        for _ in 0..2 {
            scope!("b");
        }
        profiler.end_frame();
        assert_eq!(profiler.last_frame_span_count(), 5);
        {
            scope!("a");
        }
        profiler.end_frame();
    });
    assert_eq!(profiler.last_frame_span_count(), 1);
    assert_eq!(profiler.history_span_count(), 6);
    assert_eq!(names(&profiler.last_frame()), ["a"]);
    assert_eq!(names(&profiler.averages(2)), ["a", "b"]);
    assert_eq!(names(&profiler.averages(1)), ["a"], "only the newest frame");
}

#[test]
fn history_drops_the_oldest_frame_and_its_spans() {
    let profiler = FrameProfiler::new();
    with_profiler(&profiler, || {
        {
            scope!("only_in_the_first_frame");
        }
        profiler.end_frame();
        for _ in 0..HISTORY_FRAMES {
            {
                scope!("steady");
            }
            profiler.end_frame();
        }
    });
    assert_eq!(profiler.frame_count(), HISTORY_FRAMES);
    assert_eq!(
        profiler.history_span_count(),
        HISTORY_FRAMES as u64,
        "the first frame left the history"
    );
    let all = profiler.averages(usize::MAX);
    assert_eq!(
        names(&all),
        ["steady"],
        "averages(usize::MAX) is the whole history, without a panic"
    );
}

#[test]
fn clear_drops_the_partial_frame_too() {
    let profiler = FrameProfiler::new();
    with_profiler(&profiler, || {
        scope!("before");
        profiler.end_frame();
        scope!("partial");
        profiler.clear();
        assert_eq!(profiler.frame_count(), 0);
        profiler.end_frame();
    });
    assert_eq!(profiler.frame_count(), 1);
    assert_eq!(
        profiler.last_frame_span_count(),
        0,
        "the span before clear() is gone"
    );
    assert_eq!(profiler.history_span_count(), 0);
}

#[test]
fn a_span_entered_while_disabled_is_never_counted() {
    let profiler = FrameProfiler::new();
    with_profiler(&profiler, || {
        profiler.set_enabled(false);
        assert!(!profiler.is_enabled());
        let span = tracing::info_span!("quiet").entered();
        profiler.set_enabled(true);
        drop(span);
        profiler.end_frame();
        assert_eq!(
            profiler.last_frame_span_count(),
            0,
            "entered while disabled: not measured"
        );
        {
            scope!("loud");
        }
        profiler.end_frame();
    });
    assert_eq!(names(&profiler.last_frame()), ["loud"]);
    assert_eq!(profiler.history_span_count(), 1);
}

#[test]
fn a_span_entered_enabled_still_closes_after_disabling() {
    let profiler = FrameProfiler::new();
    with_profiler(&profiler, || {
        let span = tracing::info_span!("straddle").entered();
        profiler.set_enabled(false);
        drop(span);
        profiler.end_frame();
    });
    // It was measured on entry, so it is counted on exit (and the per-thread stack does not leak it).
    assert_eq!(profiler.last_frame_span_count(), 1);
}

#[test]
fn a_span_open_across_end_frame_counts_in_the_frame_where_it_exits() {
    let profiler = FrameProfiler::new();
    with_profiler(&profiler, || {
        let span = tracing::info_span!("long").entered();
        profiler.end_frame();
        assert_eq!(profiler.last_frame_span_count(), 0);
        drop(span);
        profiler.end_frame();
    });
    assert_eq!(profiler.last_frame_span_count(), 1);
    assert_eq!(profiler.history_span_count(), 1);
}

#[test]
fn a_re_entered_span_counts_each_exit() {
    let profiler = FrameProfiler::new();
    with_profiler(&profiler, || {
        let span = tracing::info_span!("again");
        for _ in 0..4 {
            let _guard = span.enter();
        }
        profiler.end_frame();
    });
    assert_eq!(profiler.last_frame_span_count(), 4);
}

#[test]
fn averages_and_top_handle_their_argument_edges() {
    let profiler = FrameProfiler::new();
    with_profiler(&profiler, || {
        for name in ["x", "y", "z"] {
            let span = tracing::info_span!("t", which = name);
            let _g = span.enter();
        }
        profiler.end_frame();
    });
    assert!(profiler.averages(0).is_empty());
    assert!(profiler.top(0).is_empty());
    assert_eq!(profiler.top(1).len(), 1);
    assert_eq!(profiler.top(usize::MAX).len(), 1, "one span name `t`");
    assert_eq!(profiler.last_frame_span_count(), 3);
}

#[test]
fn two_clones_share_one_history_and_debug_names_the_state() {
    let a = FrameProfiler::new();
    let b = a.clone();
    with_profiler(&a, || {
        scope!("shared");
    });
    b.end_frame();
    assert_eq!(a.frame_count(), 1);
    assert_eq!(names(&a.last_frame()), ["shared"]);
    b.set_enabled(false);
    assert!(!a.is_enabled());
    let text = format!("{a:?}");
    assert!(
        text.contains("enabled: false") && text.contains("frames: 1"),
        "{text}"
    );
    assert!(FrameProfiler::default().is_enabled());
}

#[test]
fn spans_from_other_threads_count_in_the_shared_frame() {
    let profiler = FrameProfiler::new();
    let dispatch = tracing::Dispatch::new(tracing_subscriber::registry().with(profiler.clone()));
    std::thread::scope(|s| {
        for _ in 0..4 {
            let dispatch = dispatch.clone();
            s.spawn(move || {
                tracing::dispatcher::with_default(&dispatch, || {
                    for _ in 0..5 {
                        scope!("worker");
                    }
                });
            });
        }
    });
    profiler.end_frame();
    assert_eq!(profiler.last_frame_span_count(), 20);
}

/// Span exits on the compute pool must not take the process-wide frame-state lock.
/// `end_frame` folds per-thread totals once. ADR-1378 still requires one `par_chunk` span per chunk;
/// this test counts locks, not spans (those stay in `par_chunk_spans`).
#[test]
fn par_chunk_exits_fold_once_in_end_frame() {
    let profiler = FrameProfiler::new();
    tracing::subscriber::set_global_default(tracing_subscriber::registry().with(profiler.clone()))
        .expect("the only global subscriber in this test binary");

    let n = 4_000usize;
    let data = vec![1u32; n];
    let before = profiler.state_lock_count();
    let sums = regia_core::tasks::par_chunks(&data, 1, |_, chunk| chunk.iter().sum::<u32>());
    assert_eq!(sums.iter().sum::<u32>(), n as u32);
    let after_fanout = profiler.state_lock_count();
    assert_eq!(
        after_fanout, before,
        "chunk exits took the frame-state lock {after_fanout} times (before {before})"
    );
    profiler.end_frame();
    assert_eq!(
        profiler.state_lock_count(),
        before + 1,
        "end_frame folds the per-thread tables once"
    );
    assert_eq!(
        profiler.last_frame_span_count(),
        n as u64,
        "every chunk span was recorded"
    );
}

/// Past the per-frame edge cap, further span exits must not build another call-path buffer.
#[test]
fn flame_paths_stop_allocating_at_the_cap() {
    let profiler = FrameProfiler::new();
    profiler.set_flame_enabled(true);
    let extra = 64usize;
    with_profiler(&profiler, || {
        for _ in 0..MAX_FLAME_EDGES_PER_FRAME {
            scope!("capped");
        }
        let at_cap = profiler.flame_path_allocs();
        for _ in 0..extra {
            scope!("capped");
        }
        assert_eq!(
            profiler.flame_path_allocs(),
            at_cap,
            "built another call-path buffer after the cap ({at_cap} at the cap)"
        );
        profiler.end_frame();
    });
    assert_eq!(profiler.flame_edges_retained(), MAX_FLAME_EDGES_PER_FRAME as u64);
    let graph = profiler.flame_graph(1);
    let node = graph
        .roots
        .iter()
        .find(|r| r.name == "capped")
        .expect("capped span kept");
    assert_eq!(node.calls, MAX_FLAME_EDGES_PER_FRAME as f64);
}

/// `flame_graph` must not copy raw edges while it holds the frame-state lock.
#[test]
fn flame_graph_does_not_copy_edges_under_the_state_lock() {
    let profiler = FrameProfiler::new();
    profiler.set_flame_enabled(true);
    with_profiler(&profiler, || {
        {
            scope!("outer");
            scope!("inner");
        }
        profiler.end_frame();
    });
    let before = profiler.flame_edges_copied_under_lock();
    let graph = profiler.flame_graph(1);
    assert_eq!(
        profiler.flame_edges_copied_under_lock(),
        before,
        "flame_graph copied edges while holding frame state"
    );
    assert!(graph.roots.iter().any(|r| r.name == "outer"), "{graph:?}");
}
