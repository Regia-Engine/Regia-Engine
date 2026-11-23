// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.4 (N214, ADR-8031): queuing a targeted broadcast costs one payload copy per
//! recipient and nothing per queued message: no holder list per message, no recipient list per
//! message, no scan of a peer's queue per message (the per-target cap is a counter). Its own test
//! binary: it installs a counting global allocator and holds a single test. Counts, not time.

// A counting global allocator is an `unsafe impl` by definition (ADR-8031, as the regia_ai
// allocation tests do); the shared one in `regia_test_utils` would pull the engine into this
// crate's dev graph.
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, Ordering};

use regia_ecs::ecs::system::SystemState;
use regia_ecs::prelude::*;
use regia_net::message::register_net_message;
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

#[derive(Clone, Debug, PartialEq, NetMessage)]
#[net(direction = "broadcast", channel = "unreliable")]
struct Hit {
    n: u32,
}

const PEERS: u32 = 64;
const TARGETS: u32 = 32;
const PER_TARGET: u32 = 8;

fn tick(world: &mut World) {
    net_receive(world);
    net_apply(world);
    net_send(world);
    world.resource_mut::<NetNode>().outbox.clear();
    let mut rep = world.resource_mut::<ServerReplication>();
    for p in 1..=PEERS {
        rep.ack_all_state(PeerId(p));
    }
}

/// Allocations of one off-tick `net_send` with `messages` targeted broadcasts queued per target.
fn off_tick(world: &mut World, targets: &[Entity], per_target: u32) -> u64 {
    // The next tick must not send state: the gather would be counted too.
    let every = world
        .resource::<NetSettings>()
        .replication
        .send_every_ticks
        .max(2);
    while world.resource::<NetNode>().tick.0.is_multiple_of(every) {
        tick(world);
    }
    let mut sender: SystemState<NetSender> = SystemState::new(world);
    {
        let mut s = sender
            .get_mut(world)
            .unwrap_or_else(|err| panic!("sender: {err:?}"));
        for n in 0..per_target {
            for e in targets {
                s.broadcast(*e, Hit { n })
                    .unwrap_or_else(|err| panic!("broadcast: {err:?}"));
            }
        }
    }
    sender.apply(world);
    let before = ALLOCS.load(Ordering::Relaxed);
    net_send(world);
    let used = ALLOCS.load(Ordering::Relaxed) - before;
    world.resource_mut::<NetNode>().outbox.clear();
    used
}

#[test]
fn a_targeted_broadcast_allocates_one_payload_per_recipient() {
    let mut world = World::new();
    world.insert_resource(NetMode::DedicatedServer);
    world.insert_resource(LocalPeer(Some(PeerId::SERVER)));
    register_replicated::<Hp>(&mut world, ReplicateOptions::default());
    register_net_message::<Hit>(&mut world);
    regia_net::runtime::ensure_resources(&mut world);
    let mut settings = NetSettings::default();
    settings.replication.send_every_ticks = 2;
    settings.replication.spawns_per_send = 100_000;
    settings.replication.client_rate_bytes = 50_000_000;
    world.insert_resource(settings);
    let targets: Vec<Entity> = (0..TARGETS)
        .map(|i| {
            world
                .spawn((Replicated::default(), Transform::default(), Hp { hp: i }))
                .id()
        })
        .collect();
    for p in 1..=PEERS {
        server_add_peer(&mut world, PeerId(p));
        world
            .resource_mut::<ServerReplication>()
            .set_wire(PeerId(p), ConnWire::Acked);
    }
    for _ in 0..6 {
        tick(&mut world);
    }
    for e in &targets {
        let id = *world.get::<NetId>(*e).unwrap_or_else(|| panic!("no id"));
        for p in 1..=PEERS {
            assert!(
                world.resource::<ServerReplication>().conn_holds(PeerId(p), id),
                "peer {p} holds every target"
            );
        }
    }
    // Warm the queues once, then measure an empty off tick and a full one.
    off_tick(&mut world, &targets, PER_TARGET);
    tick(&mut world);
    let empty = off_tick(&mut world, &targets, 0);
    tick(&mut world);
    let full = off_tick(&mut world, &targets, PER_TARGET);
    let deliveries = f64::from(TARGETS * PER_TARGET * PEERS);
    let per_delivery = full.saturating_sub(empty) as f64 / deliveries;
    // One payload copy per (message, peer); the queue's own growth adds a little.
    assert!(
        per_delivery < 1.5,
        "{per_delivery:.2} allocations per delivered message ({full} for {deliveries} deliveries, {empty} empty)"
    );
}
