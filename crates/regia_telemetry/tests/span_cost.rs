// SPDX-License-Identifier: MPL-2.0
//! What the optional frame-profiler layer (`regia::frame_profiler()`, the hitch event's CPU
//! spans) costs per span on this machine; `do27_bench` prints spans per frame, so the product is
//! its per-frame cost. Prints only; timing assertions on shared machines flake.

use std::time::Instant;

use tracing_subscriber::layer::SubscriberExt;

#[test]
fn frame_profiler_cost_per_span() {
    let profiler = regia_core::profiling::FrameProfiler::new();
    let with = tracing_subscriber::registry().with(profiler.clone());
    let without = tracing_subscriber::registry();
    let run = |n: u32| {
        let start = Instant::now();
        for _ in 0..n {
            let _outer = tracing::info_span!("outer").entered();
            let _inner = tracing::info_span!("inner").entered();
        }
        start.elapsed().as_secs_f64() * 1e9 / f64::from(2 * n)
    };
    let base = tracing::subscriber::with_default(without, || {
        run(10_000);
        run(200_000)
    });
    let prof = tracing::subscriber::with_default(with, || {
        run(10_000);
        let ns = run(200_000);
        profiler.end_frame();
        ns
    });
    println!(
        "span enter+exit: {base:.0} ns bare registry, {prof:.0} ns with the frame profiler (+{:.0} ns per span)",
        prof - base
    );
}

/// What the per-thread timeline (ADR-7325) adds per span while it is on, against the same
/// profiler with it off; one `end_frame` per 1,000 spans keeps every frame under the cap. Prints only.
#[test]
fn frame_profiler_timeline_cost_per_span() {
    let profiler = regia_core::profiling::FrameProfiler::new();
    let subscriber = tracing_subscriber::registry().with(profiler.clone());
    let run = |n: u32| {
        let start = Instant::now();
        for i in 0..n {
            let _outer = tracing::info_span!("outer").entered();
            let _inner = tracing::info_span!("inner").entered();
            if i % 500 == 499 {
                profiler.end_frame();
            }
        }
        start.elapsed().as_secs_f64() * 1e9 / f64::from(2 * n)
    };
    let (off, on) = tracing::subscriber::with_default(subscriber, || {
        run(10_000);
        let off = run(200_000);
        profiler.set_timeline_enabled(true);
        run(10_000);
        let on = run(200_000);
        (off, on)
    });
    println!(
        "span enter+exit with the frame profiler: {off:.0} ns timeline off, {on:.0} ns timeline on (+{:.0} ns per span)",
        on - off
    );
}

/// What per-system timing (bevy's `trace` system spans through the frame profiler, read by
/// telemetry's `top_systems`) costs per frame for a 300-system schedule. Prints only.
#[test]
fn ecs_system_timing_cost_per_frame() {
    use regia_ecs::prelude::*;
    const SYSTEMS: usize = 300;
    const FRAMES: u32 = 400;
    let build = || {
        let mut world = World::new();
        let mut schedule = Schedule::default();
        // Single-threaded: the spans' cost, not the thread pool's scheduling, is measured.
        schedule.set_executor(regia_ecs::ecs::schedule::SingleThreadedExecutor::new());
        for _ in 0..SYSTEMS {
            schedule.add_systems(|| {
                std::hint::black_box(1u32);
            });
        }
        schedule.run(&mut world);
        (world, schedule)
    };
    let run = |world: &mut World, schedule: &mut Schedule| {
        let start = Instant::now();
        for _ in 0..FRAMES {
            schedule.run(world);
        }
        start.elapsed().as_secs_f64() * 1000.0 / f64::from(FRAMES)
    };
    // Systems capture the dispatcher when they are built, so each case builds its own schedule.
    let base = tracing::subscriber::with_default(tracing_subscriber::registry(), || {
        let (mut world, mut schedule) = build();
        run(&mut world, &mut schedule);
        run(&mut world, &mut schedule)
    });
    let profiler = regia_core::profiling::FrameProfiler::new();
    let mut scratch = Vec::new();
    let (timed, take_ms) =
        tracing::subscriber::with_default(tracing_subscriber::registry().with(profiler.clone()), || {
            let (mut world, mut schedule) = build();
            run(&mut world, &mut schedule);
            let ms = run(&mut world, &mut schedule);
            let start = Instant::now();
            regia_core::profiling::take_system_times(&mut scratch);
            (ms, start.elapsed().as_secs_f64() * 1000.0)
        });
    assert!(
        regia_core::profiling::system_count() > 0,
        "bevy_ecs `trace` system spans reach the frame profiler"
    );
    // The closures share one type, so they are recorded under one name: the count is not SYSTEMS.
    assert!(!scratch.is_empty(), "take_system_times returns the timed systems");
    println!(
        "{SYSTEMS} systems: {base:.4} ms per frame bare registry, {timed:.4} ms with per-system timing \
         (+{:.4} ms); telemetry's take_system_times {take_ms:.4} ms",
        timed - base
    );
}
