// SPDX-License-Identifier: MPL-2.0
//! Script passes must not allocate a type-path `String` per slot on a steady frame.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `abilities_alloc.rs`).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_app::prelude::*;
use regia_ecs::ecs::schedule::{ScheduleLabel, Schedules, SingleThreadedExecutor};
use regia_framework::script::{Script, ScriptPlugin, ScriptSlot, Scripts};

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

struct Nop;

impl Script for Nop {}

fn app_with(slots: usize) -> App {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, ScriptPlugin))
        .insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_millis(16),
        ));
    if slots > 0 {
        let mut scripts = Scripts::default();
        for index in 0..slots {
            scripts.0.push(ScriptSlot {
                type_path: format!("bench::Script{index:04}::{}", "path".repeat(16)),
                order: (index as i32) % 5,
                instance: Some(Box::new(Nop)),
                started: false,
                enabled: true,
            });
        }
        app.world_mut().spawn(scripts);
    }
    // On this thread, so the thread-local count sees every system of the step.
    for label in [FixedUpdate.intern(), Update.intern(), PostUpdate.intern()] {
        if let Some(schedule) = app.world_mut().resource_mut::<Schedules>().get_mut(label) {
            schedule.set_executor(SingleThreadedExecutor::new());
        }
    }
    app
}

/// Allocations of one `app.update()` after a warm-up update.
fn second_update_allocations(slots: usize) -> usize {
    let mut app = app_with(slots);
    app.update();
    let before = allocations();
    app.update();
    allocations() - before
}

#[test]
fn script_passes_do_not_allocate_a_type_path_per_slot() {
    const SLOTS: usize = 48;
    let empty = second_update_allocations(0);
    let full = second_update_allocations(SLOTS);
    assert!(
        full <= empty.saturating_add(8),
        "second update allocated {full} times with {SLOTS} scripts and {empty} with none; \
         a type path per slot on three passes would add well over {SLOTS}"
    );
}
