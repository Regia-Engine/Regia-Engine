// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.4 (N214, ADR-8031): the relevance update's per-connection work allocates nothing
//! once its buffers are warm. Its own test binary: it installs a counting global allocator (every
//! thread, so the update's workers count too) and holds a single test, so no other test allocates
//! while it measures.
//!
//! The update runs with 64 and with 128 connections whose viewpoints walk (so every connection gains
//! and loses entities each update); the difference between the two counts, per extra connection and
//! update, is what a connection costs. Both counts are past the point where the compute pool's chunk
//! count stops growing (at most `MAX_RELEVANCE_THREADS × 4` chunks), so the per-task cost of the
//! parallel walk cancels out. Counts, not time.

// A counting global allocator is an `unsafe impl` by definition (ADR-8031, as the regia_ai
// allocation tests do); the shared one in `regia_test_utils` would pull the engine into this
// crate's dev graph.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_net::relevance::InterestEntity;
use regia_net::testing::SplitMix64;
use regia_net::*;
use regia_scene::transform::Transform;

struct Counting;

static ALLOCS: AtomicU64 = AtomicU64::new(0);

// SAFETY: forwards every call to the system allocator unchanged; the counter is an atomic.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller's contract for `alloc` is passed through.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from `System` with this layout.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: the caller's contract for `realloc` is passed through.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

const UPDATES: u32 = 20;

/// Allocations of `UPDATES` walking updates with `conns` connections, after a warm-up.
fn walking_updates(conns: u32) -> u64 {
    let mut world = World::new();
    world.insert_resource(NetMode::DedicatedServer);
    install_net_interest(&mut world);
    world
        .resource_mut::<NetProfiles>()
        .insert(
            "near",
            NetProfile {
                relevance: RelevanceShape::Spatial {
                    radius: 80.0,
                    hysteresis: 0.15,
                },
                linger_ticks: 2,
                ..NetProfile::default()
            },
        )
        .unwrap_or_else(|e| panic!("{e}"));
    world.resource_mut::<NetProfiles>().set_default("near");
    let mut rng = SplitMix64::new(9);
    let mut r = move |lo: f32, hi: f32| lo + (hi - lo) * rng.next_f64() as f32;
    let ents: Vec<Entity> = (0..4000u32)
        .map(|i| {
            world
                .spawn((
                    Transform {
                        translation: Vec3::new(r(-500.0, 500.0), 0.0, r(-500.0, 500.0)),
                        ..Transform::default()
                    },
                    NetId(i + 1),
                ))
                .id()
        })
        .collect();
    let peers: Vec<PeerId> = (1..=conns).map(PeerId).collect();
    let viewers: Vec<Entity> = peers
        .iter()
        .map(|peer| {
            world
                .spawn((
                    NetViewpoint {
                        player: NetPlayerId {
                            peer: *peer,
                            local: 0,
                        },
                    },
                    Transform {
                        translation: Vec3::new(r(-400.0, 400.0), 0.0, r(-400.0, 400.0)),
                        ..Transform::default()
                    },
                ))
                .id()
        })
        .collect();
    let mut interest = world.remove_resource::<NetInterest>().unwrap_or_default();
    let list = |structure: bool| -> Vec<InterestEntity> {
        ents.iter()
            .enumerate()
            .map(|(i, e)| InterestEntity {
                id: NetId(i as u32 + 1),
                slot: i as u32,
                entity: *e,
                owner: PeerId::SERVER,
                depth: 0,
                dirty: structure,
                structure,
                predicted: false,
                parent: None,
            })
            .collect()
    };
    let first = list(true);
    let still = list(false);
    let walk = |world: &mut World, tick: u32| {
        for (k, v) in viewers.iter().enumerate() {
            if let Some(mut t) = world.get_mut::<Transform>(*v) {
                // 3 m a tick: every connection gains and loses entities at its edge.
                t.translation.x += if k.is_multiple_of(2) { 3.0 } else { -3.0 };
                t.translation.z += if tick.is_multiple_of(2) { 0.5 } else { -0.5 };
            }
        }
    };
    interest.update(&mut world, &first, &peers, NetTick(0));
    for tick in 1..=10 {
        walk(&mut world, tick);
        interest.update(&mut world, &still, &peers, NetTick(tick));
    }
    let before = ALLOCS.load(Ordering::Relaxed);
    for tick in 11..11 + UPDATES {
        walk(&mut world, tick);
        interest.update(&mut world, &still, &peers, NetTick(tick));
    }
    let used = ALLOCS.load(Ordering::Relaxed) - before;
    let gained: usize = interest.peers.values().map(|p| p.held_count()).sum();
    assert!(
        gained > conns as usize * 5,
        "every connection holds something ({gained})"
    );
    used
}

#[test]
fn a_walking_connection_allocates_nothing_per_update() {
    let few = walking_updates(64);
    let many = walking_updates(128);
    let per_conn = (many.saturating_sub(few)) as f64 / f64::from(64 * UPDATES);
    // The update's own maps grow with the connection count by a node per handful of
    // connections; a list allocated per connection would cost at least 1.
    assert!(
        per_conn < 0.5,
        "{per_conn:.2} allocations per connection per update ({few} with 64 connections, {many} with 128)"
    );
}
