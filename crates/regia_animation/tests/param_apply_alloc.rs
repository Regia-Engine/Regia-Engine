// SPDX-License-Identifier: MPL-2.0
//! A second decode of the same parameter lanes must not allocate a new key per parameter.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `regia_audio/tests/bus_meter_alloc.rs`).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_animation::ParameterSet;
use regia_animation::pose_eval::ParamLanes;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

#[test]
fn a_second_apply_reuses_the_parameter_map() {
    let mut set = ParameterSet::new();
    set.set_float("speed", 1.0);
    set.set_int("combo", 2);
    set.set_bool("crouch", true);
    set.set_trigger("jump");
    let lanes = ParamLanes::capture(&set, None);
    let mut decoded = lanes.to_set();
    let expected = lanes.to_set();
    // One apply settles the map the first decode built.
    lanes.layout.apply(&lanes.values, &mut decoded);
    let before = allocations();
    lanes.layout.apply(&lanes.values, &mut decoded);
    let allocated = allocations() - before;
    assert_eq!(
        allocated, 0,
        "the second apply allocated {allocated} times; the map and its keys should already exist"
    );
    assert_eq!(decoded.float("speed"), expected.float("speed"));
    assert_eq!(decoded.int("combo"), expected.int("combo"));
    assert_eq!(decoded.bool("crouch"), expected.bool("crouch"));
    assert_eq!(decoded.trigger("jump"), expected.trigger("jump"));
}
