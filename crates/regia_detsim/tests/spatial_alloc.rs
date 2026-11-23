// SPDX-License-Identifier: MPL-2.0
//! Allocation proof for spatial build, refiling and queries (ADR-8318).
//! Counting harness adapted from Regia hash_alloc.rs (ADR-8047).

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
fn queries_allocate_nothing_after_warmup() {
    use regia_detsim::broadphase::{BroadBody, BroadphaseConfig};
    use regia_detsim::spatial::{DetSpatial, SpatialRow};
    use std::ops::ControlFlow;
    let mut rows: Vec<_> = (0..900)
        .map(|slot| SpatialRow {
            slot,
            body: BroadBody::new(
                [(slot % 30) as f32 * 30.0, 0.0, (slot / 30) as f32 * 30.0],
                0.0,
                [-1.0, 1.0],
            ),
            group_mask: 1,
            flags: 0,
        })
        .collect();
    let mut index = DetSpatial::new(BroadphaseConfig {
        cell_size: 40.0,
        ..Default::default()
    });
    index.build(&rows).expect("valid test slot");
    let mut out = Vec::with_capacity(900);
    let q = rows[0].body;
    let (count, ()) = allocations(|| {
        for tick in 0..50 {
            // Change both occupancy and grid dimensions after warming once.
            for row in &mut rows {
                row.body.pos[0] = (row.slot % 30) as f32 * (1 + tick % 2) as f32;
            }
            index.build(&rows).expect("valid test slot");
            for slot in 0..900 {
                index.update(slot, [400.0, 400.0]).expect("valid test slot");
                index.update(slot, [f32::NAN, 0.0]).expect("valid test slot");
                index.update(slot, [0.0, 0.0]).expect("valid test slot");
            }
            let _ = index.within(q, 80.0, 1, |_| ControlFlow::Continue(()));
            let _ = index.nearest(q, 1, 80.0, |_| 0.0);
            index.first_k_within(q, 80.0, 1, 24, &mut out);
            let _ = index.within_mut(q, 80.0, 1, |index, slot| {
                index.update(slot, [20.0, 20.0]).expect("valid test slot");
                ControlFlow::Continue(())
            });
        }
    });
    assert_eq!(count, 0);
}
#[test]
fn counter_sees_a_planted_allocation() {
    let (count, values) = allocations(|| vec![1_u8; 16]);
    assert_eq!(count, 1);
    assert_eq!(values.len(), 16);
}

#[test]
fn segment_and_shape_queries_allocate_nothing_after_warmup() {
    use regia_detsim::broadphase::{BroadBody, BroadphaseConfig};
    use regia_detsim::spatial::{DetSpatial, SpatialRow};
    use std::ops::ControlFlow;
    let rows: Vec<_> = (0..900)
        .map(|slot| SpatialRow {
            slot,
            body: BroadBody::new(
                [(slot % 30) as f32 * 30.0, 0.0, (slot / 30) as f32 * 30.0],
                0.5,
                [-1.0, 1.0],
            ),
            group_mask: 1,
            flags: 0,
        })
        .collect();
    let history: Vec<[f32; 2]> = rows
        .iter()
        .map(|r| [r.body.pos[0] + 3.0, r.body.pos[2]])
        .collect();
    let project = |slot: usize, p: &[f32; 2]| SpatialRow {
        slot,
        body: BroadBody::new([p[0], 0.0, p[1]], 0.5, [-1.0, 1.0]),
        group_mask: 1,
        flags: 0,
    };
    let mut index = DetSpatial::new(BroadphaseConfig {
        cell_size: 40.0,
        ..Default::default()
    });
    let mut rewind = DetSpatial::new(BroadphaseConfig {
        cell_size: 40.0,
        ..Default::default()
    });
    index.build(&rows).expect("valid test slot");
    rewind.build_from(&history, project).expect("valid history");
    let q = rows[0].body;
    let (count, ()) = allocations(|| {
        for tick in 0..50 {
            index.build(&rows).expect("valid test slot");
            rewind
                .build_from(&history[..900 - tick], project)
                .expect("valid history");
            for shot in 0..200 {
                let len = (shot * 2) as f32;
                let to = [q.pos[0] + len, q.pos[2] + len * 0.5];
                let _ =
                    index.along_segment(q, to, (shot % 3) as f32 * 30.0, 1, |_| ControlFlow::Continue(()));
                let _ = index.first_along_segment(q, to, 0.0, 1, |slot| slot > 10);
                let _ = rewind.along_segment(q, to, 0.0, 1, |_| ControlFlow::Continue(()));
            }
            for cone in 0..50 {
                let _ = index.within_sector(q, [1.0, cone as f32 * 0.1], 0.9, 120.0, 1, |_| {
                    ControlFlow::Continue(())
                });
                let _ = index.within_box(q, [0.0, 0.0], [cone as f32 * 5.0, 100.0], 1, |_| {
                    ControlFlow::Continue(())
                });
            }
            let _ = index.along_segment(q, [f32::NAN, 0.0], 0.0, 1, |_| ControlFlow::Continue(()));
        }
    });
    assert_eq!(count, 0);
}
