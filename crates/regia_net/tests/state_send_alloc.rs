// SPDX-License-Identifier: MPL-2.0
//! ADR-8885 (spec 120 § 8.4, N214): what one more connection on the acknowledged state channel
//! allocates per send tick on a loopback server (no transport frame size, as in the N214 bench).
//! Its own test binary: it installs a counting global allocator (every thread, so the gather's
//! workers count too) and holds a single test, so no other test allocates while it measures.
//!
//! The same moving world is sent to 16 and to 32 connections; the difference between the two
//! counts, per extra connection and send tick, is what a connection costs. Before ADR-8885 the frame
//! budget of a link without a transport size was read by building a default `Connection` (its
//! queues and stream tables) twice per connection and send tick. Counts, not time.

// A counting global allocator is an `unsafe impl` by definition (ADR-8031, as
// `relevance_scale_alloc.rs` does); the shared one in `regia_test_utils` would pull the engine into
// this crate's dev graph.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use glam::Vec3;
use regia_ecs::prelude::*;
use regia_net::replication::{ConnWire, ServerReplication, register_replicated};
use regia_net::runtime::{NetNode, net_apply, net_receive, net_send, server_add_peer};
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

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Hp {
    hp: u32,
}

/// Entities every connection holds; each moves every tick.
const ENTITIES: u32 = 48;
/// Measured ticks after the warm-up.
const TICKS: u32 = 24;

/// Allocations of `TICKS` ticks with `conns` acknowledged state-channel connections, after a
/// warm-up, and the send ticks among them.
fn send_ticks(conns: u32) -> (u64, u32) {
    let mut world = World::new();
    world.insert_resource(NetMode::DedicatedServer);
    world.insert_resource(LocalPeer(Some(PeerId::SERVER)));
    register_replicated::<Hp>(&mut world, ReplicateOptions::default());
    regia_net::runtime::ensure_resources(&mut world);
    install_net_interest(&mut world);
    let mut settings = NetSettings::default();
    // Room for every record of every tick: the count is the per-connection cost, not the budget.
    settings.replication.client_rate_bytes = 1 << 22;
    world.insert_resource(settings.clone());
    let ids: Vec<Entity> = (0..ENTITIES)
        .map(|i| {
            world
                .spawn((
                    Replicated::default(),
                    Transform {
                        translation: Vec3::new(i as f32, 0.0, 0.0),
                        ..Transform::default()
                    },
                    Hp { hp: i },
                ))
                .id()
        })
        .collect();
    let peers: Vec<PeerId> = (1..=conns).map(PeerId).collect();
    for peer in &peers {
        server_add_peer(&mut world, *peer);
        world
            .resource_mut::<ServerReplication>()
            .set_wire(*peer, ConnWire::Acked);
        world.spawn((
            NetViewpoint {
                player: NetPlayerId {
                    peer: *peer,
                    local: 0,
                },
            },
            Transform::default(),
        ));
    }
    let every = settings.replication.send_every_ticks.max(1);
    let step = |world: &mut World, t: u32| -> bool {
        for (i, e) in ids.iter().enumerate() {
            if let Some(mut tr) = world.get_mut::<Transform>(*e) {
                tr.translation.z = (t as f32) * 0.25 + i as f32 * 0.01;
            }
        }
        let sends = u64::from(world.resource::<NetNode>().tick.0).is_multiple_of(u64::from(every));
        net_receive(world);
        net_apply(world);
        net_send(world);
        world.resource_mut::<NetNode>().outbox.clear();
        // Every state frame arrives and is acknowledged before the next send tick.
        let mut rep = world.resource_mut::<ServerReplication>();
        for p in &peers {
            rep.ack_all_state(*p);
        }
        sends
    };
    // Warm-up: spawns, buffers at their working size.
    for t in 0..12 {
        step(&mut world, t);
    }
    let before = ALLOCS.load(Ordering::Relaxed);
    let mut sent = 0;
    for t in 12..12 + TICKS {
        sent += u32::from(step(&mut world, t));
    }
    (ALLOCS.load(Ordering::Relaxed) - before, sent)
}

#[test]
fn a_loopback_connection_allocates_little_per_send_tick() {
    let (small, sends_small) = send_ticks(16);
    let (large, sends_large) = send_ticks(32);
    assert_eq!(sends_small, sends_large, "both runs send on the same ticks");
    assert!(sends_small > 0, "the measured ticks include send ticks");
    let per_conn = large.saturating_sub(small) as f64 / (16.0 * f64::from(sends_small));
    eprintln!(
        "allocations: 16 connections {small}, 32 connections {large}, {sends_small} send ticks: \
         {per_conn:.2} per extra connection and send tick"
    );
    assert!(
        per_conn <= PER_CONN_LIMIT,
        "{per_conn:.2} allocations per extra connection and send tick (limit {PER_CONN_LIMIT}; \
         16 connections {small}, 32 connections {large}, {sends_small} send ticks)"
    );
}

/// Allocations one more connection may cost per send tick (its records, frames and send log).
/// Measured on Linux (ADR-8885): 18.29 with the frame budget computed once, 24.25 when every
/// send built a default `Connection` to read it.
const PER_CONN_LIMIT: f64 = 21.0;
