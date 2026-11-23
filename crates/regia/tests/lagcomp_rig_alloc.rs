// SPDX-License-Identifier: MPL-2.0
//! `rig_entity` on a root that itself carries the rig allocates nothing.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `regia_audio/tests/bus_meter_alloc.rs`).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;

use regia::lagcomp_bridge::rig_entity;
use regia_animation::clip::AnimationClip;
use regia_animation::components::{Animator, spawn_skeleton};
use regia_animation::skeleton::Skeleton;
use regia_app::prelude::*;
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

#[test]
fn a_root_rig_is_found_without_a_heap_allocation() {
    let mut world = World::new();
    let skeleton = Arc::new(
        Skeleton::from_local_bind(
            vec!["root".into()],
            vec![None],
            vec![regia_animation::skeleton::LocalBind::default()],
        )
        .expect("valid skeleton"),
    );
    let binding = spawn_skeleton(&mut world, skeleton);
    let root = world
        .spawn((
            Transform::default(),
            Animator::single(AnimationClip::empty("c", 1.0), true),
            binding,
        ))
        .id();
    // The spawn above allocates. The lookup itself must not.
    let before = allocations();
    let found = rig_entity(&world, root);
    let allocated = allocations() - before;
    assert_eq!(found, Some(root));
    assert_eq!(
        allocated, 0,
        "resolving a rig that sits on the root allocated {allocated} times"
    );
}
