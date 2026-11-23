// SPDX-License-Identifier: MPL-2.0
//! Roadmap lane 3 (`docs/specs/ui-parity-roadmap.md`, ADR-343): "no allocation per frame for idle
//! tweens". After warm-up, the two tween systems (`advance_tweens`, `apply_ui_transforms`) allocate
//! nothing while every tween has finished and the transforms rest, nor while running tweens sit on a
//! paused clock. Counted with a counting global allocator around the systems' own runs only, on this thread (the
//! pattern of `regia_framework/tests/vehicle_ai_alloc.rs`); fixed 1/60 s steps, no wall clock. One
//! test per binary, so no other test's allocations are counted.

// The counting global allocator is the only unsafe code (the pattern of ADR-3147's counter tests).
#![allow(unsafe_code)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::alloc::{GlobalAlloc, Layout, System as SystemAlloc};
use std::cell::Cell;
use std::time::Duration;

use regia_app::Time;
use regia_app::prelude::*;
use regia_ui::node::{ComputedNode, Opacity};
use regia_ui::tween::{advance_tweens, apply_ui_transforms};
use regia_ui::{Tween, TweenProperty, TweenValue, UiTransform, UiTween, UiTweenFinished};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { SystemAlloc.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { SystemAlloc.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: same contract as the caller's.
        unsafe { SystemAlloc.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

/// `n` nodes with a laid-out rect, an opacity and a resting transform.
fn spawn_nodes(world: &mut World, n: usize) -> Vec<Entity> {
    (0..n)
        .map(|i| {
            let x = (i % 8) as f32 * 60.0;
            let y = (i / 8) as f32 * 40.0;
            world
                .spawn((
                    ComputedNode {
                        rect: [x, y, 50.0, 30.0],
                        clip_rect: [x, y, 50.0, 30.0],
                        scale: 1.0,
                        ..Default::default()
                    },
                    Opacity(1.0),
                    UiTransform::IDENTITY,
                ))
                .id()
        })
        .collect()
}

fn fade(world: &mut World, nodes: &[Entity], to: f32) {
    for &e in nodes {
        world.entity_mut(e).insert(UiTween::new(Tween::to(
            TweenProperty::Opacity,
            TweenValue::Number(to),
            0.2,
        )));
    }
}

#[test]
fn idle_and_paused_tweens_allocate_nothing_per_frame() {
    let mut world = World::new();
    world.insert_resource(Time::default());
    world.init_resource::<Messages<UiTweenFinished>>();
    // `advance_tweens` writes frame events too (`.uianim.ron` v2, ADR-8900); none of these tweens has any.
    world.init_resource::<Messages<regia_ui::UiAnimEventFired>>();
    let nodes = spawn_nodes(&mut world, 64);
    fade(&mut world, &nodes[..32], 0.25);

    let mut advance = IntoSystem::into_system(advance_tweens);
    let mut transforms = IntoSystem::into_system(apply_ui_transforms);
    let _ = advance.initialize(&mut world);
    let _ = transforms.initialize(&mut world);
    // One frame: the clock moves by `dt`, both systems run, messages and trackers roll over. Returns
    // the allocations made inside the two systems.
    let mut frame = |world: &mut World, dt: f32| -> u64 {
        world.resource_mut::<Time>().advance(Duration::from_secs_f32(dt));
        let before = allocations();
        advance.run((), world).expect("advance_tweens");
        transforms.run((), world).expect("apply_ui_transforms");
        let made = allocations() - before;
        world.resource_mut::<Messages<UiTweenFinished>>().update();
        world.clear_trackers();
        made
    };

    // Warm-up: the 0.2 s fades run out (12 frames) and their components go; buffers grow once.
    for _ in 0..60 {
        frame(&mut world, 1.0 / 60.0);
    }
    let still = nodes
        .iter()
        .filter(|&&e| world.get::<UiTween>(e).is_some())
        .count();
    assert_eq!(still, 0, "every fade finished");
    let landed = world.get::<Opacity>(nodes[0]).map(|o| o.0).unwrap_or(f32::NAN);
    assert!((landed - 0.25).abs() < 1.0e-6, "the fade landed: {landed}");

    // Idle: finished tweens and resting transforms.
    let idle: u64 = (0..120).map(|_| frame(&mut world, 1.0 / 60.0)).sum();

    // Paused: tweens in flight on a clock that does not move.
    fade(&mut world, &nodes[32..], 0.5);
    for _ in 0..3 {
        frame(&mut world, 1.0 / 60.0);
    }
    let progress = world.get::<UiTween>(nodes[40]).expect("in flight").progress();
    let paused: u64 = (0..120).map(|_| frame(&mut world, 0.0)).sum();
    let after = world
        .get::<UiTween>(nodes[40])
        .expect("still in flight")
        .progress();

    println!("tween systems: {idle} allocations idle, {paused} paused over 120 frames each");
    assert_eq!(after, progress, "a paused clock does not advance a tween");
    assert_eq!(idle, 0, "allocations per idle frame after warm-up");
    assert_eq!(paused, 0, "allocations per paused frame");
}
