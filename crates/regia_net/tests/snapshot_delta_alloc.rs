// SPDX-License-Identifier: MPL-2.0
//! Spec 120 § 8.1 (R1): the standalone snapshot codec's encode cost does not scale with the world.
//! Counted, not timed: a counting allocator (as `recorder_alloc.rs` does) around
//! `SnapshotEncoder::encode` in steady state, and the live bytes an encoder's baseline ring holds.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use glam::{Quat, Vec3};
use regia_net::model::{NetId, NetTick};
use regia_net::snapshot_delta::{
    EntitySnapshot, FieldDef, FieldKind, FieldValue, SnapshotEncoder, SnapshotSchema, WorldSnapshot,
};

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static LIVE: Cell<i64> = const { Cell::new(0) };
}

fn note_alloc(bytes: usize) {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
    let _ = LIVE.try_with(|c| c.set(c.get() + bytes as i64));
}

fn note_free(bytes: usize) {
    let _ = LIVE.try_with(|c| c.set(c.get() - bytes as i64));
}

struct Counting;

// SAFETY: forwards to the system allocator; the counters are thread-local `Cell`s that never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note_alloc(layout.size());
        // SAFETY: the caller's layout goes straight through.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note_alloc(layout.size());
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note_free(layout.size());
        note_alloc(new_size);
        // SAFETY: as `alloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        note_free(layout.size());
        // SAFETY: as `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

fn live_bytes() -> i64 {
    LIVE.with(Cell::get)
}

const SCORE: usize = 1;
const POS: usize = 2;

fn schema() -> SnapshotSchema {
    SnapshotSchema::new(vec![
        FieldDef::new("alive", FieldKind::Bool),
        FieldDef::new("score", FieldKind::U32),
        FieldDef::new(
            "pos",
            FieldKind::Vec3Quantised {
                min: Vec3::splat(-1000.0),
                max: Vec3::splat(1000.0),
                bits: 18,
            },
        ),
        FieldDef::new("rot", FieldKind::QuatSmallestThree { bits: 10 }),
        FieldDef::new(
            "health",
            FieldKind::Float {
                min: 0.0,
                max: 100.0,
                bits: 10,
            },
        ),
        FieldDef::new("team", FieldKind::U8),
        FieldDef::new("state", FieldKind::Enum { variants: 6 }),
        FieldDef::new("ammo", FieldKind::U16),
    ])
    .expect("schema")
}

fn world(count: u32) -> WorldSnapshot {
    WorldSnapshot::new(
        (0..count)
            .map(|i| {
                EntitySnapshot::new(
                    NetId(1 + i * 3),
                    vec![
                        FieldValue::Bool(true),
                        FieldValue::U32(i),
                        FieldValue::Vec3(Vec3::new(i as f32 % 900.0, 1.0, -(i as f32 % 700.0))),
                        FieldValue::Quat(Quat::from_rotation_y(i as f32 * 0.01)),
                        FieldValue::F32(75.0),
                        FieldValue::U8((i % 4) as u8),
                        FieldValue::Enum(i % 6),
                        FieldValue::U16(30),
                    ],
                )
            })
            .collect(),
    )
}

/// Moves `changes` entities (spread over the world) at `tick`, outside any measurement.
fn step(world: &mut WorldSnapshot, tick: u32, changes: usize) {
    let len = world.entities.len();
    for i in 0..changes {
        let at = (tick as usize * 7919 + i * (len / changes.max(1))) % len;
        let entity = &mut world.entities[at];
        entity.fields[SCORE] = FieldValue::U32(tick * 100 + i as u32);
        entity.fields[POS] = FieldValue::Vec3(Vec3::new(tick as f32 % 500.0, i as f32, 3.0));
    }
}

/// Steady state (the ring full, every ack fresh): the most allocations one delta encode made over
/// 20 packets, with `changes` entities moving per tick.
fn steady_encode_allocations(count: u32, changes: usize) -> u64 {
    let mut enc = SnapshotEncoder::new(schema());
    let mut world = world(count);
    let mut ack = None;
    let mut worst = 0;
    for tick in 1..=100u32 {
        step(&mut world, tick, changes);
        let before = allocations();
        let packet = enc.encode(NetTick(tick), &world, ack, 1 << 20).expect("encode");
        let used = allocations() - before;
        if tick > 80 {
            worst = worst.max(used);
        }
        drop(packet);
        ack = enc.last_report().map(|r| r.seq);
    }
    worst
}

/// The baseline ring was a full copy of the world per packet: every encode cloned the acknowledged
/// map (one allocation per entity and more) and stored another. With copy-on-write baselines the
/// allocations per packet follow the records sent, not the world: a 4,000-entity world allocates
/// no more per delta than a 200-entity one, and at most 16 + 6 per changed entity.
#[test]
fn encode_allocations_per_packet_do_not_grow_with_the_world() {
    let changes = 8;
    let small = steady_encode_allocations(200, changes);
    let big = steady_encode_allocations(4000, changes);
    let bound = 16 + 6 * changes as u64;
    eprintln!("delta encode allocations: {small} at 200 entities, {big} at 4,000 (bound {bound})");
    assert!(
        big <= bound,
        "a 4,000-entity delta encode allocated {big} times (bound {bound}, 200 entities: {small})"
    );
    assert!(
        big <= small + 2 * changes as u64,
        "allocations grew with the world: {small} at 200 entities, {big} at 4,000"
    );
}

/// Live bytes of an encoder after 100 packets of a `count`-entity world with `changes` movers.
fn encoder_live_bytes(capacity: usize, count: u32, changes: usize) -> i64 {
    let mut world = world(count);
    let start = live_bytes();
    let mut enc = SnapshotEncoder::with_capacity(schema(), capacity);
    let mut ack = None;
    for tick in 1..=100u32 {
        step(&mut world, tick, changes);
        // Only what the encoder keeps counts: the packet is dropped at once.
        let packet = enc.encode(NetTick(tick), &world, ack, 1 << 20).expect("encode");
        drop(packet);
        ack = enc.last_report().map(|r| r.seq);
    }
    let held = live_bytes() - start;
    drop(enc);
    held
}

/// 64 baselines of a mostly static world used to cost 64 copies of it (about 20 MB per client at
/// 1,000 entities x 8 fields). Shared copy-on-write, the 64-baseline ring holds less than twice
/// what a one-baseline encoder holds.
#[test]
fn sixty_four_baselines_of_a_static_world_cost_less_than_two_copies() {
    let one = encoder_live_bytes(1, 4000, 8);
    let ring = encoder_live_bytes(64, 4000, 8);
    eprintln!("encoder live bytes: one baseline {one}, 64 baselines {ring}");
    assert!(one > 0, "precondition: the encoder holds something ({one} bytes)");
    assert!(
        ring < 2 * one,
        "64 baselines hold {ring} bytes, one baseline {one} bytes ({:.1} copies)",
        ring as f64 / one as f64
    );
}
