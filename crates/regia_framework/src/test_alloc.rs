// SPDX-License-Identifier: MPL-2.0
//! Counting allocator for this crate's unit tests (one per test binary).
//!
//! Production builds do not include it. A test reads [`count`] around the call it cares about.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Forwards to the system allocator and counts each heap request.
pub(crate) struct Counting;

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

/// Heap requests since the process started, on this thread.
pub(crate) fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

/// Heap requests made while `f` runs.
pub(crate) fn count(f: impl FnOnce()) -> usize {
    let before = allocations();
    f();
    allocations() - before
}
