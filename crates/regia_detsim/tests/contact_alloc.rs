// SPDX-License-Identifier: MPL-2.0
//! Allocation proof for the contact pass (ADR-8563): once the broadphase's buffers and the ram list
//! have grown to a match's size, `contact::resolve_all` allocates nothing.
//! Counting harness adapted from Regia hash_alloc.rs (ADR-8047), as in spatial_alloc.rs.

#![allow(unsafe_code)]
// clippy 1.99 reports `missing_const_for_thread_local` on aarch64-linux-android only, at the
// `thread_local!` below whose initializers are already `const { }` (a false positive in the
// expansion; Linux and Windows clippy are clean). `expect` fails the build once it stops (ADR-8047).
#![cfg_attr(
    target_os = "android",
    expect(
        clippy::missing_const_for_thread_local,
        reason = "false positive on Android: the initializers are already const"
    )
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    // `try_with`: the allocator also runs while thread-locals are torn down.
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

/// Allocations made on this thread while `f` runs.
fn allocations<R>(f: impl FnOnce() -> R) -> (u64, R) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (COUNT.with(Cell::get), out)
}

#[test]
fn contact_passes_allocate_nothing_after_warmup() {
    use regia_detsim::contact::ContactPile;
    use regia_detsim::golden::DetScenario;

    // Warm up with one whole 900-body match, then replay it: the same match needs no buffer the
    // first run did not already grow (reset reuses the vectors' capacity).
    let mut pile = ContactPile::with_bodies("contact_pile_900", 900);
    let run = pile.default_run();
    for tick in 0..run.ticks {
        pile.step(tick);
    }
    let warm = pile.totals();
    assert!(warm.pairs > 100_000 && warm.rams > 0, "{warm:?}");
    let (count, totals) = allocations(|| {
        pile.reset(&run);
        for tick in 0..run.ticks {
            pile.step(tick);
        }
        pile.totals()
    });
    assert_eq!(totals, warm, "the replay is the same match");
    assert_eq!(count, 0, "allocations in 600 ticks of 900 bodies after warmup");
}

#[test]
fn counter_sees_a_planted_allocation() {
    let (count, values) = allocations(|| vec![1_u8; 16]);
    assert_eq!(count, 1);
    assert_eq!(values.len(), 16);
}
