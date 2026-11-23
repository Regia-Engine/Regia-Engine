// SPDX-License-Identifier: MPL-2.0
//! The tick ring allocates nothing on the profiled path (ADR-8498).
//!
//! A counting global allocator counts this thread's allocations only. Its own test binary,
//! because of the global allocator (the pattern of `tests/hash_alloc.rs`).

#![allow(
    unsafe_code,
    reason = "ADR-8498: the counting allocator wraps the system allocator and only counts calls"
)]
#![cfg_attr(
    target_os = "android",
    expect(
        clippy::missing_const_for_thread_local,
        reason = "false positive on Android: the initializers are already const"
    )
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_detsim::hash::Systems;
use regia_detsim::profile::{ClockKind, ManualClock, ProfileSettings, TickProfiler};

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    let armed = ARMED.try_with(Cell::get).unwrap_or(false);
    if armed {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }
}

// SAFETY: forwards every call to the system allocator unchanged; the only addition is a
// thread-local counter update, which neither allocates nor touches the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations<R>(f: impl FnOnce() -> R) -> (u64, R) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (COUNT.with(Cell::get), out)
}

fn push(profiler: &mut TickProfiler<2, ManualClock>, tick: u64) {
    profiler.clock_mut().set(0);
    profiler.begin_tick();
    profiler.begin("a").expect("a");
    profiler.clock_mut().advance(10);
    profiler.end("a").expect("a");
    profiler.begin("b").expect("b");
    profiler.clock_mut().advance(20);
    profiler.end("b").expect("b");
    profiler.end_tick(tick);
}

#[test]
fn profile_ring_allocates_nothing_after_warmup() {
    let mut profiler = TickProfiler::new(
        Systems::new(["a", "b"]),
        ManualClock::new(ClockKind::Wall),
        ProfileSettings {
            ring_ticks: 8,
            ..ProfileSettings::default()
        },
    )
    .expect("ring");
    // The first `tracing::enabled!` can register a callsite. That stays outside the count.
    push(&mut profiler, 0);
    push(&mut profiler, 1);
    let (count, _) = allocations(|| push(&mut profiler, 2));
    assert_eq!(count, 0, "begin/end/end_tick allocate after the ring is reserved");
}
