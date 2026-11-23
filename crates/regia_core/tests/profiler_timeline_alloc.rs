// SPDX-License-Identifier: MPL-2.0
//! ADR-7325: the frame profiler's per-thread timeline costs no allocation in a warmed-up steady
//! frame (span exits and `end_frame`), and with it off the same frame allocates nothing either.
//!
//! Its own test binary: it installs the shared counting allocator (ADR-5718) and counts this
//! thread's requests only.

use regia_core::profiling::{FrameProfiler, HISTORY_FRAMES, scope};
use regia_test_utils::alloc_counter::{self, CountingAlloc};
use tracing_subscriber::layer::SubscriberExt;

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

/// The span tree of one steady frame, without `end_frame`: the same names every frame.
fn spans() {
    {
        scope!("steady_outer");
        for _ in 0..4 {
            scope!("steady_inner");
            scope!("steady_leaf");
        }
    }
}

/// One steady frame: a span tree of the same names every frame, then `end_frame`.
fn frame(profiler: &FrameProfiler) {
    spans();
    profiler.end_frame();
}

/// Warms past a full history (every reused buffer has held a frame), then counts 50 frames.
fn steady_allocs(timeline: bool) -> (u64, FrameProfiler) {
    let profiler = FrameProfiler::new();
    profiler.set_timeline_enabled(timeline);
    let subscriber = tracing_subscriber::registry().with(profiler.clone());
    let allocs = tracing::subscriber::with_default(subscriber, || {
        for _ in 0..HISTORY_FRAMES + 10 {
            frame(&profiler);
        }
        let ((), counted) = alloc_counter::measure(|| {
            for _ in 0..50 {
                frame(&profiler);
            }
        });
        counted.allocs
    });
    (allocs, profiler)
}

#[test]
fn a_warmed_up_steady_frame_allocates_nothing_with_the_timeline_on() {
    let (allocs, profiler) = steady_allocs(true);
    assert_eq!(
        allocs, 0,
        "50 steady frames with the timeline on allocated {allocs} times"
    );
    let frame = profiler.timeline_frame(None).expect("a frame");
    assert_eq!(
        frame.span_count(),
        9,
        "the timeline recorded the frame: {frame:?}"
    );
}

#[test]
fn a_steady_frame_allocates_nothing_with_the_timeline_off() {
    let (allocs, profiler) = steady_allocs(false);
    assert_eq!(
        allocs, 0,
        "50 steady frames with the timeline off allocated {allocs} times"
    );
    assert!(
        profiler
            .timeline_frame(None)
            .is_some_and(|f| f.threads.is_empty())
    );
}

/// ADR-7427: a play frame's flame context (the guard the editor holds around `pie_frame`) costs
/// no allocation in a warmed-up steady frame, nested or dropped out of order, with the timeline on.
#[test]
fn a_flame_context_in_a_steady_frame_allocates_nothing() {
    use regia_core::profiling::flame_context;
    let profiler = FrameProfiler::new();
    profiler.set_timeline_enabled(true);
    let subscriber = tracing_subscriber::registry().with(profiler.clone());
    let context_frame = |profiler: &FrameProfiler| {
        let outer = flame_context("steady_play");
        let inner = flame_context("steady_step");
        drop(outer);
        frame(profiler);
        drop(inner);
    };
    let allocs = tracing::subscriber::with_default(subscriber, || {
        for _ in 0..HISTORY_FRAMES + 10 {
            context_frame(&profiler);
        }
        let ((), counted) = alloc_counter::measure(|| {
            for _ in 0..50 {
                context_frame(&profiler);
            }
        });
        counted.allocs
    });
    assert_eq!(
        allocs, 0,
        "50 steady frames under a flame context allocated {allocs} times"
    );
}

/// ADR-7427's review (w8): with flame capture on, a compute worker's root span exit reads the
/// flame context once per root (`current_flame_context`), the hot path the tests above leave
/// unexercised (they run with flame off, or off the compute pool). Warmed up on a
/// `regia-compute`-named thread, the exits (the context read, the path fold and the shard's
/// flame table) allocate nothing. `end_frame` stays outside the counted windows: with flame on
/// it folds a fresh tree for the frame and moves it into an `Arc`, which allocates by design
/// (`end_frame`'s own docs promise a steady frame without allocations only with flame off).
#[test]
fn a_compute_thread_root_exit_with_flame_on_allocates_nothing() {
    use regia_core::profiling::flame_context;
    let (allocs, filed_under_context) = std::thread::Builder::new()
        .name("regia-compute (7)".into())
        .spawn(move || {
            let profiler = FrameProfiler::new();
            profiler.set_timeline_enabled(true);
            profiler.set_flame_enabled(true);
            let subscriber = tracing_subscriber::registry().with(profiler.clone());
            tracing::subscriber::with_default(subscriber, || {
                // Whichever context name the read returns, the root lands under a context (the
                // guards of this file's other tests share the process-wide stack).
                let _context = flame_context("steady_play");
                for _ in 0..HISTORY_FRAMES + 10 {
                    frame(&profiler);
                }
                let mut allocs = 0_u64;
                for _ in 0..50 {
                    let ((), counted) = alloc_counter::measure(spans);
                    allocs += counted.allocs;
                    profiler.end_frame();
                }
                let graph = profiler.flame_graph(1);
                // Whichever context name the read returned (the guards of this file's other
                // tests share the process-wide stack), the compute thread's root span exited
                // under it: that root is the only node with `steady_outer` as a direct child.
                let filed = graph
                    .roots
                    .iter()
                    .any(|r| r.children.iter().any(|c| c.name == "steady_outer"));
                (allocs, filed)
            })
        })
        .expect("spawn the regia-compute thread")
        .join()
        .expect("the compute thread runs");
    assert_eq!(
        allocs, 0,
        "50 steady batches of span exits with flame on allocated {allocs} times"
    );
    assert!(
        filed_under_context,
        "the compute thread's root span was filed under the flame context"
    );
}
