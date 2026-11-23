// SPDX-License-Identifier: MPL-2.0
//! Debug capture is off by default and costs no allocation; with capture on, a pass and a query
//! still allocate nothing once the buffers are reserved (ADR-8562).
//!
//! A counting global allocator counts this thread's allocations only. Its own test binary,
//! because of the global allocator (the pattern of `tests/profile_alloc.rs`).

#![allow(
    unsafe_code,
    reason = "ADR-8562: the counting allocator wraps the system allocator and only counts calls"
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

use std::ops::ControlFlow;

use regia_detsim::broadphase::{BroadBody, BroadphaseConfig, DetBroadphase};
use regia_detsim::debug::{DebugCapture, DebugKind, DebugSink, DebugWhat};
use regia_detsim::spatial::{DetSpatial, SpatialRow};

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

/// Counts what it is given; never allocates.
#[derive(Default)]
struct CountingSink {
    prims: usize,
    contacts: usize,
    queries: usize,
}

impl DebugSink for CountingSink {
    fn line(&mut self, kind: DebugKind, _a: [f32; 3], _b: [f32; 3]) {
        self.prims += 1;
        if kind == DebugKind::Contact {
            self.contacts += 1;
        }
    }
    fn circle(&mut self, kind: DebugKind, _center: [f32; 3], _radius: f32) {
        self.prims += 1;
        if kind == DebugKind::Query {
            self.queries += 1;
        }
    }
    fn aabb(&mut self, _kind: DebugKind, _min: [f32; 3], _max: [f32; 3]) {
        self.prims += 1;
    }
    fn text(&mut self, _kind: DebugKind, _at: [f32; 3], _text: &str) {
        self.prims += 1;
    }
}

fn bodies() -> Vec<BroadBody> {
    (0..48)
        .map(|i| {
            let x = (i % 8) as f32 * 1.5;
            let z = (i / 8) as f32 * 1.5;
            BroadBody::new([x, 0.0, z], 1.0, [-1.0, 2.0])
        })
        .collect()
}

fn rows(bodies: &[BroadBody]) -> Vec<SpatialRow> {
    bodies
        .iter()
        .enumerate()
        .map(|(slot, body)| SpatialRow {
            slot,
            body: *body,
            group_mask: 1,
            flags: 0,
        })
        .collect()
}

fn queries(index: &mut DetSpatial, hits: &mut Vec<usize>) -> Option<usize> {
    let center = BroadBody::new([4.0, 0.0, 4.0], 0.0, [-1.0, 2.0]);
    let _ = index.within(center, 3.0, 1, |_| ControlFlow::Continue(()));
    index.first_k_within_by(center, 3.0, 1, 4, hits, |slot| slot % 2 == 0);
    index.nearest(center, 1, 10.0, |slot| slot as f32)
}

#[test]
fn debug_emit_is_off_by_default_and_allocates_nothing() {
    let mut grid = DetBroadphase::new(BroadphaseConfig::default());
    let mut index = DetSpatial::default();
    assert_eq!(grid.debug_capture(), None, "broadphase capture is off by default");
    assert_eq!(index.debug_capture(), None, "spatial capture is off by default");
    let mut world = bodies();
    let table = rows(&world);
    let mut hits = Vec::with_capacity(8);
    // Warm-up: the first pass and build size the index buffers.
    grid.for_each_contact(&mut world, |_, _| {});
    index.build(&table).expect("rows");
    let _ = queries(&mut index, &mut hits);

    let mut sink = CountingSink::default();
    let (count, contacts) = allocations(|| {
        grid.for_each_contact(&mut world, |_, _| {});
        index.build(&table).expect("rows");
        let _ = queries(&mut index, &mut hits);
        grid.debug_emit(&mut sink, DebugWhat::ALL);
        index.debug_emit(&mut sink, DebugWhat::ALL);
        grid.stats().contacts
    });
    assert!(contacts > 0, "the scene has contacts to draw");
    assert_eq!(
        count, 0,
        "a pass, a query and debug_emit with capture off allocate nothing"
    );
    assert_eq!(sink.prims, 0, "capture off: debug_emit draws nothing");
    assert_eq!(grid.debug_capture_stats(), Default::default());
}

#[test]
fn debug_capture_on_allocates_nothing_once_reserved() {
    let mut grid = DetBroadphase::new(BroadphaseConfig::default());
    let mut index = DetSpatial::default();
    grid.set_debug_capture(Some(DebugCapture::default()));
    index.set_debug_capture(Some(DebugCapture::default()));
    let mut world = bodies();
    let table = rows(&world);
    let mut hits = Vec::with_capacity(8);
    grid.for_each_contact(&mut world, |_, _| {});
    index.build(&table).expect("rows");
    let _ = queries(&mut index, &mut hits);

    let mut sink = CountingSink::default();
    let (count, contacts) = allocations(|| {
        grid.for_each_contact(&mut world, |_, _| {});
        index.build(&table).expect("rows");
        let _ = queries(&mut index, &mut hits);
        grid.debug_emit(&mut sink, DebugWhat::ALL);
        index.debug_emit(&mut sink, DebugWhat::ALL);
        grid.stats().contacts
    });
    assert_eq!(
        count, 0,
        "capture on: passes, queries and debug_emit allocate nothing"
    );
    assert_eq!(sink.contacts as u64, contacts);
    assert_eq!(sink.queries, 6, "two rounds of three queries are in the ring");
}
