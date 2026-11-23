// SPDX-License-Identifier: MPL-2.0
//! Queries that follow the drawn surface allocate nothing for the drawn bodies in a steady scene
//! (ADR-3251): the water plugin's cache holds them, so a follow-the-tier query costs the same
//! allocations as a plain one. Before the cache each call rebuilt the drawn bodies (a vector and
//! a clone of every parked ocean's body). Counted per thread; no wall clock.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_app::prelude::*;
use regia_ecs::ecs::system::SystemState;
use regia_terrain::water_body::{WaterQuery, cache_drawn_water, drawn_cache_rebuilds};
use regia_terrain::{FftOcean, Swell, WaterBody, WaterClock, WaterLook, WaterSettings, WaterTier};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: every method forwards to `System` with the caller's own arguments; counting touches only
// a thread-local `Cell` and never allocates.
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

/// An ocean plane (parked on Low: its drawn body is a clone with the stand-in swell), a swell
/// plane with more waves than Low draws (its drawn body is a truncated clone) and a lake.
/// `pre_drawn` spawns the bodies as Low draws them instead, the baseline a plain query over the
/// same surfaces gives.
fn world(follow: bool, pre_drawn: bool) -> World {
    let mut world = World::new();
    world.insert_resource(WaterSettings {
        tier: Some(WaterTier::Low),
        queries_follow_tier: follow,
        ..WaterSettings::default()
    });
    world.insert_resource(WaterClock {
        tick: 77,
        ..WaterClock::default()
    });
    let mut sea = WaterBody::plane([0.0, 0.0, 0.0], [300.0, 300.0], WaterLook::ocean());
    sea.ocean = Some(FftOcean::default());
    let mut swell = WaterBody::plane([600.0, 0.0, 0.0], [200.0, 200.0], WaterLook::ocean());
    swell.swell = Some(Swell::open_sea([1.0, 0.2], 0.8, 20.0, 32));
    let lake = WaterBody::plane([0.0, 40.0, 900.0], [20.0, 20.0], WaterLook::lake());
    for body in [sea, swell, lake] {
        if pre_drawn {
            world.spawn(body.drawn_for(WaterTier::Low, true).into_owned());
        } else {
            world.spawn(body);
        }
    }
    world
}

const POINTS: [(f32, f32); 4] = [(1.0, 2.0), (-80.0, 33.0), (610.0, -5.0), (3.0, 905.0)];

/// Allocations of `rounds` rounds of surface and ray queries at the points, after one warm round,
/// and the sum of the last round's answers.
fn query_allocations(world: &mut World, rounds: usize) -> (usize, f32) {
    let mut state: SystemState<WaterQuery> = SystemState::new(world);
    let query = state.get(world).expect("query");
    let round = || {
        let mut sum = 0.0;
        for &(x, z) in &POINTS {
            if let Some((_, s)) = query.surface(x, z) {
                sum += s.height;
            }
            // Half weight: a moved level must not cancel between the height and the distance.
            if let Some((_, hit)) = query.raycast([x, 60.0, z], [0.0, -1.0, 0.0], 200.0) {
                sum += 0.5 * hit.distance;
            }
        }
        sum
    };
    let mut last = round();
    let before = allocations();
    for _ in 0..rounds {
        last = round();
    }
    (allocations() - before, last)
}

#[test]
fn follow_tier_queries_allocate_nothing_for_the_drawn_bodies() {
    // The plain query over the surfaces Low draws: what the surface and ray code itself costs.
    let mut plain = world(false, true);
    let (base, expected) = query_allocations(&mut plain, 50);

    // Following the tier without the cache: every call rebuilds the drawn bodies.
    let mut uncached = world(true, false);
    let (rebuilt, a) = query_allocations(&mut uncached, 50);
    assert!(
        rebuilt >= base + 50 * POINTS.len(),
        "uncached {rebuilt} vs plain {base}"
    );
    assert_eq!(a, expected, "following the tier answers the drawn surfaces");

    // With the cache the plugin keeps: the same allocations as the plain query, same answers.
    let mut cached = world(true, false);
    let cache = cached.register_system(cache_drawn_water);
    cached.run_system(cache).expect("cache");
    cached.flush();
    let rebuilds = drawn_cache_rebuilds();
    cached.run_system(cache).expect("cache");
    assert_eq!(
        drawn_cache_rebuilds(),
        rebuilds,
        "a steady scene rebuilt a cached body"
    );
    let (steady, b) = query_allocations(&mut cached, 50);
    assert_eq!(
        steady, base,
        "cached follow-the-tier queries allocated {steady}, plain ones {base}"
    );
    assert_eq!(a, b, "the cache changed an answer");

    // A changed body is not answered from a stale cache: the rebuild path takes over until the
    // cache catches up, and the answer moves with the body.
    let entity = {
        let mut q = cached.query::<(Entity, &WaterBody)>();
        q.iter(&cached)
            .find(|(_, b)| b.ocean.is_some())
            .map(|(e, _)| e)
            .expect("sea")
    };
    if let Some(mut body) = cached.get_mut::<WaterBody>(entity)
        && let regia_terrain::WaterShape::Plane { center, .. } = &mut body.shape
    {
        center[1] = 1.5;
    }
    let (_, moved) = query_allocations(&mut cached, 1);
    assert_ne!(moved, b, "a moved body answered from its stale cache");
    cached.run_system(cache).expect("cache");
    assert!(
        drawn_cache_rebuilds() > rebuilds,
        "the changed body was not rebuilt"
    );
}
