// SPDX-License-Identifier: MPL-2.0
//! Allocation counts of the AI soak's systems after warmup (ADR-8561).
//!
//! A counting global allocator counts this thread's allocations only. Its own test binary,
//! because of the global allocator (the pattern of `tests/profile_alloc.rs`).

#![allow(
    unsafe_code,
    reason = "ADR-8561: the counting allocator wraps the system allocator and only counts calls"
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

use regia_detsim::golden::DetScenario;
use regia_detsim::scale_soak::ai::{AiSoakConfig, AiSoakObserver, AiSoakScenario, AiSystem};

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    // `try_with`: the allocator also runs while thread-locals are torn down.
    if ARMED.try_with(Cell::get).unwrap_or(false) {
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

/// Allocations per system, read at each system's begin and end.
struct PerSystem {
    open: u64,
    counts: [u64; 10],
}

impl AiSoakObserver for PerSystem {
    fn begin(&mut self, _: AiSystem) {
        self.open = COUNT.with(Cell::get);
    }
    fn end(&mut self, system: AiSystem) {
        self.counts[system as usize] += COUNT.with(Cell::get) - self.open;
    }
}

#[test]
fn ai_soak_systems_allocate_nothing_after_warmup() {
    let mut scenario = AiSoakScenario::new(AiSoakConfig::default()).expect("default settings are valid");
    let run = scenario.default_run();
    scenario.reset(&run);
    for tick in 0..120 {
        scenario.step(tick);
    }
    let mut observer = PerSystem {
        open: 0,
        counts: [0; 10],
    };
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    for tick in 120..720 {
        scenario.step_observed(tick, &mut observer);
    }
    ARMED.with(|a| a.set(false));
    let rows: Vec<String> = AiSystem::ALL
        .iter()
        .zip(observer.counts)
        .map(|(s, n)| format!("{}={n}", s.name()))
        .collect();
    println!("ai-soak-alloc {}", rows.join(" "));
    // The AI soak's own systems reuse reserved buffers: nothing after warmup. Two engine
    // pieces are printed, not asserted: `recycle` grows the id allocator's FIFO when a tick
    // frees more ids than any tick before, and `contacts` (the F-06 broadphase) grows a
    // cell's list when the cell holds more bodies than it ever did. Both are high-water
    // growth whose count depends on the standard library's growth policy.
    for system in [
        AiSystem::Input,
        AiSystem::Build,
        AiSystem::Acquire,
        AiSystem::Target,
        AiSystem::Ally,
        AiSystem::KeepOut,
        AiSystem::Move,
        AiSystem::Shots,
    ] {
        assert_eq!(observer.counts[system as usize], 0, "{}: {rows:?}", system.name());
    }
}
