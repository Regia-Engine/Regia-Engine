// SPDX-License-Identifier: MPL-2.0
//! A second `newest_shapes_of` of one exact-tick record reuses the pose scratch.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `regia_audio/tests/bus_meter_alloc.rs`). The first call evaluates the pose. The second call
//! of the same tick must allocate fewer times and return the same positions. `newest_shapes_of`
//! still builds shape reports, so the second call is not zero.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::time::Duration;

use regia::lagcomp_bridge::LagCompBridgePlugin;
use regia::net::lagcomp::{HitShape, HitShapeKind, HitShapes, LagCompHistory, PoseSource, lagcomp_record};
use regia::net::model::{NetId, NetMode, NetTick};
use regia::net::runtime::{NetNode, ensure_resources};
use regia_animation::clip::AnimationClip;
use regia_animation::components::{Animator, spawn_skeleton};
use regia_animation::prelude::AnimationPlugin;
use regia_animation::skeleton::{LocalBind, Skeleton};
use regia_app::prelude::*;
use regia_app::{FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_physics::PhysicsPlugin;
use regia_scene::{ScenePlugin, Transform};

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

fn eight_joints() -> Arc<Skeleton> {
    let names: Vec<String> = (0..8).map(|i| format!("j{i}")).collect();
    let parents: Vec<Option<u32>> = (0..8).map(|i| if i == 0 { None } else { Some(0) }).collect();
    let binds: Vec<LocalBind> = (0..8)
        .map(|i| LocalBind {
            translation: Vec3::new(i as f32 * 0.25, 1.0, 0.0),
            ..LocalBind::default()
        })
        .collect();
    Arc::new(Skeleton::from_local_bind(names, parents, binds).expect("valid skeleton"))
}

#[test]
fn a_second_read_of_one_tick_skips_the_pose_eval() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.add_plugins((
        regia_app::MinimalPlugins,
        ScenePlugin,
        PhysicsPlugin,
        AnimationPlugin,
        LagCompBridgePlugin {
            exact_tick_joints: true,
            ..LagCompBridgePlugin::default()
        },
    ));
    let world = app.world_mut();
    world.insert_resource(NetMode::DedicatedServer);
    ensure_resources(world);
    let binding = spawn_skeleton(world, eight_joints());
    let shapes = HitShapes {
        shapes: (0..8)
            .map(|i| HitShape::new(HitShapeKind::Sphere { radius: 0.05 }, "joint").on_joint(&format!("j{i}")))
            .collect(),
        pose_source: PoseSource::Joints,
    };
    let entity = world
        .spawn((
            NetId(1),
            Transform::from_translation(Vec3::new(1.0, 0.0, 0.0)),
            shapes,
            binding,
            Animator::single(AnimationClip::empty("c", 1.0), true),
        ))
        .id();
    app.finish_plugins();
    for _ in 0..4 {
        app.update();
    }
    {
        let world = app.world_mut();
        world.resource_mut::<NetNode>().tick = NetTick(1);
        lagcomp_record(world);
    }
    let history = app.world().resource::<LagCompHistory>();
    assert_eq!(history.anim_entities(), 1, "the record keeps animation state");
    let before = allocations();
    let first = history.newest_shapes_of(entity);
    let first_allocs = allocations() - before;
    let before = allocations();
    let second = history.newest_shapes_of(entity);
    let second_allocs = allocations() - before;
    eprintln!("pose cache allocs: first={first_allocs} second={second_allocs}");
    assert_eq!(first, second, "both reads pose the same shapes");
    assert!(
        second_allocs < first_allocs,
        "the second read allocated {second_allocs} times; the first, which evaluates, allocated {first_allocs}"
    );
}
