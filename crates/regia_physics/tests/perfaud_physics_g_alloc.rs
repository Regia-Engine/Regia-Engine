// SPDX-License-Identifier: MPL-2.0
//! Steady-state heap counts for the destruction step and the debris bridge (performance
//! audit, lane perfaud-physics-g).
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `tests/cloth_alloc.rs`).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use regia_physics::PhysicsWorld;
use regia_physics::destruction::{
    Authority, BoxGrid, Damage, DamageKind, DestructibleAsset, DestructiblePose, DestructionMaterial,
    DestructionPhysics, DestructionWorld, GridSupport,
};

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

fn wall() -> DestructibleAsset {
    DestructibleAsset::box_grid(&BoxGrid {
        name: "wall".into(),
        counts: [1, 2, 1],
        cell: [1.0, 1.0, 1.0],
        density: 100.0,
        strength_per_area: 1.0,
        support: GridSupport::Bottom,
        material: DestructionMaterial::default(),
    })
    .expect("wall")
}

/// An intact structure with nothing queued still walks every instance each tick.
#[test]
fn an_idle_destructible_step_allocates_nothing_after_warm_up() {
    const DT: f32 = 1.0 / 60.0;
    let mut world = DestructionWorld::new(Authority::Server);
    let asset = Arc::new(wall());
    for seed in 1..5 {
        world
            .define(Arc::clone(&asset), DestructiblePose::default(), seed)
            .expect("define");
    }
    world.step(DT);
    let before = allocations();
    for _ in 0..32 {
        let report = world.step(DT);
        assert!(report.events.is_empty());
        assert_eq!(report.hits, 0);
    }
    let n = allocations() - before;
    assert_eq!(n, 0, "idle steps allocated {n} times over 32 steps");
}

/// One miss per tick must reuse the damage queue instead of allocating it again.
#[test]
fn a_steady_miss_allocates_nothing_after_warm_up() {
    const DT: f32 = 1.0 / 60.0;
    let mut world = DestructionWorld::new(Authority::Server);
    let id = world
        .define(Arc::new(wall()), DestructiblePose::default(), 1)
        .expect("define");
    let miss = |world: &mut DestructionWorld| {
        world
            .damage(Damage {
                target: Some(id),
                kind: DamageKind::Explosion {
                    center: [1.0e6, 0.0, 0.0],
                    radius: 0.5,
                    impulse: 1.0,
                },
            })
            .expect("queue");
        let report = world.step(DT);
        assert_eq!(report.hits, 0);
        assert_eq!(report.misses, 1);
        assert!(report.events.is_empty());
    };
    for _ in 0..3 {
        miss(&mut world);
    }
    let before = allocations();
    for _ in 0..32 {
        miss(&mut world);
    }
    let n = allocations() - before;
    assert_eq!(n, 0, "miss steps allocated {n} times over 32 steps");
    assert!(
        world
            .instance(id)
            .expect("instance")
            .chunk_states()
            .iter()
            .all(|s| *s == regia_physics::destruction::ChunkState::Attached),
        "a miss must not break the wall"
    );
}

/// Debris that is still inside its lifetime must not rebuild its deque every sync.
#[test]
fn live_debris_sync_allocates_nothing_after_warm_up() {
    const DT: f32 = 1.0 / 60.0;
    let mut physics = PhysicsWorld::default();
    let mut world = DestructionWorld::new(Authority::Server);
    let mut bridge = DestructionPhysics::default();
    let id = world
        .define(Arc::new(wall()), DestructiblePose::default(), 1)
        .expect("define");
    bridge.attach(&mut physics, &world, id).expect("attach");
    world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 1,
                impulse: [0.0, 10_000.0, 0.0],
            },
        })
        .expect("hit");
    world.step(DT);
    bridge.sync(&mut physics, &mut world, DT);
    for _ in 0..4 {
        world.step(DT);
        bridge.sync(&mut physics, &mut world, DT);
    }
    assert!(
        bridge.active().count() >= 1,
        "the broken chunk should be live debris"
    );
    assert_eq!(world.pending_spawns(), 0, "spawns should be drained");
    let before = allocations();
    for _ in 0..32 {
        let report = bridge.sync(&mut physics, &mut world, DT);
        assert!(report.spawned.is_empty());
        assert_eq!(report.expired, 0);
    }
    let n = allocations() - before;
    assert_eq!(n, 0, "sync allocated {n} times over 32 steps with live debris");
}
