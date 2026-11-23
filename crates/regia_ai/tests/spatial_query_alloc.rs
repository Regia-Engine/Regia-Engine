// SPDX-License-Identifier: MPL-2.0
//! Audit finding 6: a spatial-query test stage must reuse its anchor-pair buffer. A filter-only
//! distance over a 441-point lattice, budget 64, measured on ticks that are already inside the
//! test stage. Counts allocations. No wall clock.
//!
//! Its own test binary: it installs a counting global allocator.
//!
//! `World::resource_scope` takes the manager out and puts it back, and Bevy records one
//! `RemovedComponent` message per take. That message buffer doubles on a later tick (one
//! realloc). The measured window is four consecutive ticks after that doubling, still inside
//! the filter stage. A per-item pair clone allocates on every one of those ticks.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_ai::spatial_query::{Pick, SpatialQueryManager, SpatialQueryPlugin, SpatialQueryRequest};
use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_scene::Transform;

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

/// 21 by 21 points. Generation is one unit each; the filter is one unit per point.
const LATTICE_POINTS: u64 = 441;
const BUDGET: u64 = 64;

const QUERY: &str = r#"(
    format_version: 1,
    name: "lattice",
    item_type: Point,
    generator: Lattice(center: Asker, half_size: 10.0, spacing: 1.0),
    tests: [
        Distance(id: "near", purpose: Filter, to: Asker, max: 1000.0),
    ],
)"#;

fn tick(app: &mut App) -> usize {
    let before = allocations();
    app.world_mut()
        .resource_scope(|world, mut manager: Mut<SpatialQueryManager>| manager.tick(world));
    allocations() - before
}

#[test]
fn a_filter_step_reuses_its_pair_buffer() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, SpatialQueryPlugin));
    let owner = app
        .world_mut()
        .spawn(Transform::from_translation(Vec3::ZERO))
        .id();
    let id = {
        let mut manager = app.world_mut().resource_mut::<SpatialQueryManager>();
        manager.config.work_units_per_tick = BUDGET;
        let name = manager.register_template_text(QUERY).expect("template");
        manager.submit(SpatialQueryRequest::new(name, owner).with_pick(Pick::All))
    };
    // Generation is 441 units. A few ticks past that are pure filter evaluations, with the pair
    // buffer already sized by the first of them.
    let mut units = 0u64;
    for _ in 0..32 {
        tick(&mut app);
        units = app
            .world()
            .resource::<SpatialQueryManager>()
            .units_spent(id)
            .expect("still running");
        if units > LATTICE_POINTS + BUDGET {
            break;
        }
    }
    assert!(
        units > LATTICE_POINTS + BUDGET,
        "never reached the test stage (units {units})"
    );
    let mut quiet = 0usize;
    let mut doublings = 0usize;
    for _ in 0..8 {
        assert!(
            app.world().resource::<SpatialQueryManager>().is_active(id),
            "the filter stage ended before four quiet ticks"
        );
        match tick(&mut app) {
            0 => quiet += 1,
            1 => {
                doublings += 1;
                quiet = 0;
            }
            spent => panic!(
                "{spent} allocations in one filter tick of {BUDGET} items; \
                 the pair buffer is being rebuilt"
            ),
        }
        if quiet == 4 {
            break;
        }
    }
    assert_eq!(
        quiet, 4,
        "quiet filter ticks after {doublings} message-buffer doublings"
    );
    assert!(
        doublings <= 1,
        "{doublings} removal-message doublings inside the filter window"
    );
    assert!(
        app.world().resource::<SpatialQueryManager>().is_active(id),
        "the measured ticks included the finishing tick"
    );
}
