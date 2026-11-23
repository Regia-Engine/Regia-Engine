// SPDX-License-Identifier: MPL-2.0
//! Recording an ECS desync row allocates nothing once warm (ADR-8080): the cached query states,
//! the reused sort buffer and the full 240-row ring. `EcsDesyncHash::record` and the system
//! `record_desync_hashes` (which takes the resource out of the world and puts it back, so each step
//! ends with the frame's `clear_trackers`, as `App::update` does) are both counted, over more steps
//! than the ring holds.
//!
//! Its own test binary: it installs the shared counting global allocator.

use regia_app::prelude::*;
use regia_ecs::ecs::query::QueryState;
use regia_framework::desync_hash::{EcsDesyncHash, RING_TICKS, Systems, record_desync_hashes};
use regia_test_utils::alloc_counter::{self, CountingAlloc};
use serde::Serialize;

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

#[derive(Component, Serialize)]
struct Body {
    pos: [f32; 3],
    vel: [f32; 3],
    cargo: Vec<u16>,
}

#[derive(Component, Serialize)]
struct Hp(u32);

#[derive(Component)]
struct Marker;

#[derive(Resource, Serialize)]
struct Rng(u64);

const SYSTEMS: Systems<3> = Systems::new(["movement", "combat", "rng"]);

fn world() -> World {
    let mut world = World::new();
    world.insert_resource(Rng(9));
    for i in 0..200u16 {
        let f = f32::from(i);
        let e = world
            .spawn(Body {
                pos: [f, -f, 0.5],
                vel: [0.0, -0.0, 1.0],
                cargo: vec![i, i + 1],
            })
            .id();
        if i.is_multiple_of(3) {
            world.entity_mut(e).insert(Hp(u32::from(i)));
        }
        if i.is_multiple_of(5) {
            world.entity_mut(e).insert(Marker);
        }
    }
    world
}

fn hash() -> EcsDesyncHash<3> {
    EcsDesyncHash::new(SYSTEMS)
        .component::<Body>("movement")
        .component::<Hp>("combat")
        .resource::<Rng>("rng")
}

type Bodies = QueryState<&'static mut Body>;

/// One lockstep-like step: values change, no entity or archetype does. The query state is made
/// before counting starts.
fn step(world: &mut World, q: &mut Bodies) {
    for mut body in q.iter_mut(world) {
        body.pos[0] += 1.0;
    }
    world.resource_mut::<Rng>().0 = world.resource::<Rng>().0.wrapping_mul(6364136223846793005);
}

#[test]
fn the_counter_sees_a_planted_allocation() {
    let (kept, stats) = alloc_counter::measure(|| vec![1u8; 32]);
    assert_eq!(kept.len(), 32);
    assert!(stats.allocs >= 1, "the counting allocator must see a Vec");
}

#[test]
fn recording_a_row_allocates_nothing_once_warm() {
    let mut world = world();
    let mut h = hash();
    // Warm: the query states and the sort buffer are created on the first fold.
    h.record(&world, 1);
    let mut q = world.query::<&mut Body>();
    let ((), stats) = alloc_counter::measure(|| {
        for tick in 2..=(RING_TICKS as u64 + 60) {
            step(&mut world, &mut q);
            h.record(&world, tick);
        }
    });
    assert_eq!(stats.allocs, 0, "record allocated: {stats:?}");
    assert_eq!(h.ring().len(), RING_TICKS);
}

#[test]
fn the_record_system_allocates_nothing_once_warm() {
    let mut world = world();
    hash().insert_into(&mut world);
    // Warm for two frames: the removal log is double-buffered, and each buffer allocates once.
    for _ in 0..2 {
        record_desync_hashes::<3>(&mut world);
        world.clear_trackers();
    }
    let mut q = world.query::<&mut Body>();
    let ((), stats) = alloc_counter::measure(|| {
        for _ in 0..(RING_TICKS + 60) {
            step(&mut world, &mut q);
            record_desync_hashes::<3>(&mut world);
            // The end of a frame (`App::update` does this): `resource_scope` takes the resource
            // component out and puts it back, which logs a removal; without the drain that log grows
            // (7 doublings over 300 steps, measured without this line).
            world.clear_trackers();
        }
    });
    assert_eq!(stats.allocs, 0, "the record system allocated: {stats:?}");
    let h = world.resource::<EcsDesyncHash<3>>();
    assert_eq!(h.recorded(), RING_TICKS as u64 + 62);
    assert_eq!(h.last().map(|row| row.tick), Some(RING_TICKS as u64 + 62));
}
