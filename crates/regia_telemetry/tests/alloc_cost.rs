// SPDX-License-Identifier: MPL-2.0
//! The per-frame telemetry system allocates nothing and costs well under 0.05 ms per frame
//! over 1000 frames (a summary every 10 s and events are not per-frame work; this run produces
//! none).
//!
//! Its own test binary: it installs a counting global allocator.

use std::time::{Duration, Instant};

use regia_app::RealTime;
use regia_app::prelude::bevy_ecs::system::System as _;
use regia_app::prelude::*;
use regia_render::RuntimeRenderer;
use regia_telemetry::{SpanProfiler, Telemetry, TelemetryOptions, telemetry_frame};

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn allocations() -> usize {
    alloc_counter::thread_allocs() as usize
}

/// Budget per frame (the owner's bound).
const BUDGET_MS: f64 = 0.05;

#[test]
fn a_thousand_frames_allocate_nothing_and_stay_under_budget() {
    let mut world = World::new();
    world.insert_resource(RealTime::default());
    world.insert_resource(RuntimeRenderer::default());
    world.insert_resource(SpanProfiler(regia_core::profiling::FrameProfiler::new()));
    world.insert_resource(Telemetry::new(TelemetryOptions {
        // Summaries are periodic events, not per-frame work; keep them out of this window.
        summary_interval: Duration::from_secs(3600),
        ..TelemetryOptions::default()
    }));
    let mut system = IntoSystem::into_system(telemetry_frame);
    system.initialize(&mut world);
    // Warm-up: the median history fills, the change keys settle.
    for i in 0..100u64 {
        world
            .resource_mut::<RealTime>()
            .advance_by(Duration::from_micros(16_000 + (i % 7) * 150));
        let _ = system.run((), &mut world);
    }

    // Ten batches of 100 frames; the cheapest batch is compared with the budget. A sum over all
    // frames counted any time the test thread was descheduled (one 50 ms preemption spent the whole
    // budget), which is the machine, not the telemetry (audit-v035-flaky H4).
    let mut best_batch = Duration::MAX;
    let before = allocations();
    for batch in 0..10u64 {
        let start = Instant::now();
        for k in 0..100u64 {
            let i = batch * 100 + k;
            // Real time moves every frame (with jitter), so every frame takes the full path.
            world
                .resource_mut::<RealTime>()
                .advance_by(Duration::from_micros(16_000 + (i % 7) * 150));
            let _ = system.run((), &mut world);
        }
        best_batch = best_batch.min(start.elapsed());
    }
    let allocated = allocations() - before;
    assert_eq!(
        allocated, 0,
        "the telemetry frame allocated {allocated} times in 1000 frames"
    );

    let per_frame_ms = best_batch.as_secs_f64() * 1000.0 / 100.0;
    println!(
        "telemetry frame: {:.4} ms per frame (budget {BUDGET_MS} ms)",
        per_frame_ms
    );
    assert!(
        per_frame_ms <= BUDGET_MS,
        "telemetry costs {per_frame_ms:.4} ms per frame (budget {BUDGET_MS} ms)"
    );
    let t = world.resource::<Telemetry>();
    assert_eq!(t.counts().frames, 1100);
    assert_eq!(t.counts().hitches, 0);
}
