// SPDX-License-Identifier: MPL-2.0
//! The deterministic grid's per-tick obstacle sync allocates nothing once warm (audit gap GR-7):
//! 900 moving discs through `NavGrid::sync_stamps_from` and moving boxes plus a parked polygon
//! through `NavGrid::sync_shapes_from`, counted by a global allocator on the test thread.

#![allow(unsafe_code)] // A `GlobalAlloc` impl is unsafe by definition (ADR-2704).

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_nav::det::grid::{GridSettings, NavGrid, NavStatic, Stamp, StampShape};

/// Allocations (including reallocations) made while a thread had counting on.
static ALLOC_OPS: AtomicU64 = AtomicU64::new(0);

thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

struct CountAlloc;

fn count() {
    if COUNTING.with(Cell::get) {
        ALLOC_OPS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for CountAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: CountAlloc = CountAlloc;

/// Ticks per movement cycle: positions repeat, so one warm cycle has seen every cluster's most
/// regions.
const PERIOD: u32 = 16;
const ENTITIES: u32 = 900;

/// Entity `k`'s disc at `tick`: a small square orbit around a home cell (integer cm, exact).
fn disc(k: u32, tick: u32) -> Stamp {
    let home_x = ((k * 7919) % 1100) as i32 * 100 - 55_000;
    let home_z = ((k * 104_729) % 1100) as i32 * 100 - 55_000;
    let phase = (tick + k) % PERIOD;
    let (dx, dz) = match phase / 4 {
        0 => (phase % 4, 0),
        1 => (4, phase % 4),
        2 => (4 - phase % 4, 4),
        _ => (0, 4 - phase % 4),
    };
    Stamp {
        x_cm: home_x + dx as i32 * 300,
        z_cm: home_z + dz as i32 * 300,
        r_cm: [60, 250, 450][(k % 3) as usize],
    }
}

/// Twelve boxes that turn and slide with the cycle, and one parked polygon.
fn shapes(tick: u32, out: &mut Vec<(u32, StampShape)>) {
    let phase = (tick % PERIOD) as i16;
    for k in 0..12u32 {
        out.push((
            k,
            StampShape::OrientedBox {
                x_cm: (k as i32 * 9000) - 50_000 + i32::from(phase) * 150,
                z_cm: 20_000 - (k as i32 * 3000),
                half_x_cm: 1200,
                half_z_cm: 400,
                cos_sin_q: [16384 - phase * 200, phase * 900],
            },
        ));
    }
    let parked = vec![[-3000, -3000], [2500, -2800], [3200, 2600], [-2600, 3100]];
    out.push((100, StampShape::ConvexPolygon(parked)));
}

fn grid() -> NavGrid {
    let settings = Arc::new(GridSettings::three_class());
    let stat = NavStatic::build(None, settings).expect("flat canvas");
    NavGrid::new(Arc::new(stat)).expect("grid")
}

#[test]
fn sync_from_allocates_nothing_after_warmup() {
    let mut fast = grid();
    let mut slow = grid();
    let mut discs: Vec<(u32, Stamp)> = Vec::with_capacity(ENTITIES as usize);
    let mut boxes: Vec<(u32, StampShape)> = Vec::with_capacity(16);
    // Warm-up: two whole cycles (every buffer reaches its largest size), checked against the
    // allocating `sync_stamps` on a second grid.
    for tick in 0..2 * PERIOD {
        discs.clear();
        // Reverse id order: the sync sorts in place.
        discs.extend((0..ENTITIES).rev().map(|k| (k, disc(k, tick))));
        let changed = fast.sync_stamps_from(&mut discs);
        let sync = slow.sync_stamps((0..ENTITIES).map(|k| (k, disc(k, tick))).collect());
        assert_eq!(changed, sync.changed);
        assert_eq!(fast.dirty_clusters(), sync.dirty_clusters.as_slice());
        boxes.clear();
        shapes(tick, &mut boxes);
        fast.sync_shapes_from(&mut boxes);
        slow.sync_shapes_from(&mut boxes.clone());
        assert_eq!(fast.state_hash(), slow.state_hash(), "tick {tick}");
    }
    // The parked polygon's buffer is reused: rebuild the set by editing it in place from now on.
    let mut relabeled = 0usize;
    ALLOC_OPS.store(0, Ordering::Relaxed);
    for tick in 2 * PERIOD..3 * PERIOD {
        COUNTING.with(|c| c.set(true));
        for (k, slot) in (0..ENTITIES).rev().zip(discs.iter_mut()) {
            *slot = (k, disc(k, tick));
        }
        let changed = fast.sync_stamps_from(&mut discs);
        relabeled += fast.dirty_clusters().len();
        let phase = (tick % PERIOD) as i16;
        for (id, shape) in boxes.iter_mut() {
            if let StampShape::OrientedBox { x_cm, cos_sin_q, .. } = shape {
                *x_cm = (*id as i32 * 9000) - 50_000 + i32::from(phase) * 150;
                *cos_sin_q = [16384 - phase * 200, phase * 900];
            }
        }
        fast.sync_shapes_from(&mut boxes);
        relabeled += fast.dirty_clusters().len();
        COUNTING.with(|c| c.set(false));
        assert!(changed, "tick {tick}: the discs move every tick");
    }
    let ops = ALLOC_OPS.load(Ordering::Relaxed);
    assert!(relabeled > 1000, "the cycle relabels clusters: {relabeled}");
    assert_eq!(ops, 0, "allocations in one warm cycle of 900 discs and 13 shapes");
}
