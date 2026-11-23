// SPDX-License-Identifier: MPL-2.0
//! After the decal pool wraps, a steady frame borrows spawn order and ages the pool
//! without allocating (the pattern of `ribbon_alloc.rs`). Its own test binary; the
//! counter is per thread, so the harness's threads do not count.

// The counting allocator is `unsafe` (ADR-1672, the same pattern as `ribbon_alloc.rs`).
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_vfx::{Decal, DecalAnchor, DecalPool, DecalSurface};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn allocations() -> u64 {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

// SAFETY: forwards to the system allocator unchanged; only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract is passed through.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is passed through.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract is passed through.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn decal_at(x: f32) -> Decal {
    Decal {
        position: Vec3::new(x, 0.0, 0.0),
        rotation: Quat::IDENTITY,
        half_size: Vec3::ONE,
        color: [1.0, 1.0, 1.0, 1.0],
        age: 0.0,
        lifetime: None,
    }
}

/// Fill past capacity, then two spawn-order reads. The second read is a steady frame:
/// the pool has already wrapped, and nothing about the set of decals changed.
#[test]
fn wrapped_spawn_order_allocates_nothing_on_the_second_read() {
    let mut pool = DecalPool::new(4);
    for i in 0..6 {
        pool.spawn(decal_at(i as f32));
    }
    assert_eq!(pool.len(), 4, "the pool is full");

    let read = |pool: &DecalPool| {
        let before = allocations();
        let (ordered, surfaces) = pool.in_spawn_order();
        let delta = allocations() - before;
        let xs: Vec<f32> = ordered.iter().map(|d| d.position.x).collect();
        let borrowed = matches!(ordered, std::borrow::Cow::Borrowed(_))
            && matches!(surfaces, std::borrow::Cow::Borrowed(_));
        (delta, xs, borrowed)
    };
    let (_first, xs, _) = read(&pool);
    let (second, xs2, borrowed) = read(&pool);
    assert_eq!(
        xs,
        vec![2.0, 3.0, 4.0, 5.0],
        "oldest first, so the newest draws last"
    );
    assert_eq!(xs2, xs);
    assert_eq!(
        second, 0,
        "second in_spawn_order allocated {second} after the pool had wrapped"
    );
    assert!(borrowed, "a steady read borrows the dense arrays");
}

/// One live decal is enough: aging and one anchored follow allocate a mask today.
/// After a warm call, the next call of each must allocate nothing.
#[test]
fn aging_and_anchor_follow_allocate_nothing_after_warm_up() {
    let mut pool = DecalPool::new(4);
    pool.spawn(decal_at(1.0));
    pool.spawn_anchored(
        decal_at(2.0),
        DecalSurface::from_decal(&decal_at(2.0)),
        DecalAnchor {
            entity: Entity::PLACEHOLDER,
            local_position: Vec3::ZERO,
            local_rotation: Quat::IDENTITY,
        },
    );
    pool.update(1.0 / 60.0);
    let before = allocations();
    pool.update(1.0 / 60.0);
    let update_delta = allocations() - before;
    assert_eq!(update_delta, 0, "update allocated {update_delta}");
    assert_eq!(pool.len(), 2, "permanent decals stay");

    pool.follow_anchors(|_| Some(Mat4::IDENTITY));
    let before = allocations();
    pool.follow_anchors(|_| Some(Mat4::IDENTITY));
    let follow_delta = allocations() - before;
    assert_eq!(follow_delta, 0, "follow_anchors allocated {follow_delta}");
    assert_eq!(pool.len(), 2, "the anchor is still live");
}
