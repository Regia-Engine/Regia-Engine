// SPDX-License-Identifier: MPL-2.0
//! Spec 103 § 10: the recorder's tick path allocates nothing after warm-up. Counted, not timed: a counting allocator
//! around `NetRecorder::record_frames`, with a file and a ring both running, and whole server ticks with a recorder.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::path::PathBuf;

use regia_ecs::prelude::*;
use regia_net::message::Channel;
use regia_net::model::{NetMode, NetTick};
use regia_net::recorder::{
    NetRecorder, RecorderConfig, RingConfig, recorder_status, start_recording, start_ring, stop_recording,
};
use regia_net::replication::{ReplicateOptions, Replicated, register_replicated};
use regia_net::testing::LoopbackHub;
use regia_net::*;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
    static LARGEST: Cell<usize> = const { Cell::new(0) };
}

fn note(bytes: usize) {
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
    let _ = LARGEST.try_with(|c| c.set(c.get().max(bytes)));
}

struct Counting;

// SAFETY: forwards to the system allocator; the counter is a thread-local `Cell` that never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: the caller's layout goes straight through.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note(layout.size());
        // SAFETY: as `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note(new_size);
        // SAFETY: as `alloc`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: as `alloc`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> u64 {
    ALLOCATIONS.with(Cell::get)
}

#[derive(Component, Clone, Debug, PartialEq, Default, Replicate)]
struct Health {
    hp: i32,
}

/// Spec 103 § 5: a length that lies is refused before anything of that size is allocated. Measured: the largest single
/// request while opening each file. A valid file's own largest is the control.
#[test]
fn a_lying_length_allocates_nothing_of_its_size() {
    let dir = unique_dir("recorder_alloc_lying");
    let path = dir.join("a.netdemo");
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
    start_recording(&mut hub.server.world, &path, RecorderConfig::default()).expect("recorder");
    hub.server.world.spawn((Replicated::default(), Health { hp: 1 }));
    hub.run(5);
    stop_recording(&mut hub.server.world).expect("stops");
    let file = std::fs::read(&path).expect("read");
    let body_len = u32::from_le_bytes([file[12], file[13], file[14], file[15]]) as usize;
    let chunk_at = 20 + body_len;
    let largest_while_opening = |bytes: &[u8]| {
        LARGEST.with(|c| c.set(0));
        let opened = regia_net::recorder::NetDemoReader::open(std::io::Cursor::new(bytes)).is_ok();
        (opened, LARGEST.with(Cell::get))
    };
    let (opened, control) = largest_while_opening(&file);
    assert!(opened);
    for (at, value) in [
        (12usize, u32::MAX),              // the header's length
        (12, (1 << 20) - 1),              // a header length under the limit, past the file
        (chunk_at + 5, u32::MAX),         // a chunk over the limit
        (chunk_at + 5, 60 * 1024 * 1024), // a chunk under the limit, past the file
    ] {
        let mut bad = file.clone();
        bad[at..at + 4].copy_from_slice(&value.to_le_bytes());
        let (opened, largest) = largest_while_opening(&bad);
        assert!(!opened, "{at}");
        assert!(
            largest <= control.max(64 * 1024),
            "{at}: asked for {largest}, a valid file {control}"
        );
    }
}

fn setup(world: &mut World) {
    register_replicated::<Health>(world, ReplicateOptions::default());
}

/// A fresh directory: pid and an atomic counter, so parallel tests never share one.
fn unique_dir(tag: &str) -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("{tag}-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

#[test]
fn the_tick_path_allocates_nothing_after_warm_up() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
    let dir = unique_dir("recorder_alloc");
    start_recording(
        &mut hub.server.world,
        &dir.join("a.netdemo"),
        RecorderConfig {
            max_bytes: 0,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    start_ring(
        &mut hub.server.world,
        RingConfig {
            max_bytes: 1 << 20,
            seconds: 5.0,
            keyframe_every_s: 1.0,
            ..RingConfig::default()
        },
    )
    .expect("starts");
    let frame = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10].repeat(40);
    let small = vec![9u8; 24];
    let key = vec![3u8; 4096];
    let mut rec = hub
        .server
        .world
        .remove_resource::<NetRecorder>()
        .expect("recorder");
    // Warm-up: the header goes out, the largest tick below sets the buffers' working size, the writer's blocks cycle.
    let mut tick = 0u32;
    for _ in 0..4096 {
        rec.record_frames(
            NetTick(tick),
            Some(&key),
            &[(Channel::ReliableOrdered, &frame), (Channel::Unreliable, &small)],
        );
        tick += 1;
    }
    let grows = rec.status().scratch_grows;
    let before = allocations();
    for t in 0..20_000u32 {
        let keyframe = t.is_multiple_of(60).then_some(key.as_slice());
        let frames: &[(Channel, &[u8])] = if t.is_multiple_of(5) {
            &[(Channel::ReliableOrdered, &frame), (Channel::Unreliable, &small)]
        } else {
            &[(Channel::ReliableOrdered, &small)]
        };
        rec.record_frames(NetTick(tick), keyframe, frames);
        tick += 1;
    }
    let spent = allocations() - before;
    assert_eq!(
        spent, 0,
        "{spent} allocations in 20,000 ticks of the recorder's path"
    );
    assert_eq!(rec.status().scratch_grows, grows, "no working buffer grew");
    hub.server.world.insert_resource(rec);
    let summary = stop_recording(&mut hub.server.world).expect("stops");
    assert_eq!(summary.ticks, u64::from(tick));
}

/// A whole server tick with a recorder holds a flat allocation count: per tick it costs what one more connection
/// costs, and it does not grow. The number itself is the gather's, spec 72's budget.
#[test]
fn whole_ticks_with_a_recorder_do_not_grow() {
    let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
    let dir = unique_dir("recorder_alloc_whole");
    start_recording(
        &mut hub.server.world,
        &dir.join("a.netdemo"),
        RecorderConfig {
            keyframe_every_s: 1000.0, // clamps to MAX_SEGMENT_TICKS: no keyframe in the measured stretch
            max_bytes: 0,
            ..RecorderConfig::default()
        },
    )
    .expect("starts");
    let units: Vec<Entity> = (0..16)
        .map(|hp| {
            hub.server
                .world
                .spawn((Replicated::default(), Health { hp }))
                .id()
        })
        .collect();
    let drive = |hub: &mut LoopbackHub, n: usize| {
        for _ in 0..n {
            for e in &units {
                if let Some(mut h) = hub.server.world.get_mut::<Health>(*e) {
                    h.hp += 1;
                }
            }
            hub.server.tick();
            hub.server
                .world
                .resource_mut::<regia_net::runtime::NetNode>()
                .outbox
                .clear();
        }
    };
    drive(&mut hub, 400);
    let window = |hub: &mut LoopbackHub| {
        let before = allocations();
        drive(hub, 200);
        allocations() - before
    };
    let first = window(&mut hub);
    let second = window(&mut hub);
    // A small allowance for hash maps that rehash inside the gather: growth per tick would far exceed it.
    assert!(
        second <= first + 200,
        "allocations grew between windows: {first} then {second}"
    );
    assert!(recorder_status(&hub.server.world).ticks >= 800);
    stop_recording(&mut hub.server.world).expect("stops");
    // For the report: what the recorder adds per tick (`--nocapture`).
    let without = {
        let mut hub = LoopbackHub::new(NetMode::DedicatedServer, 1, setup);
        let units: Vec<Entity> = (0..16)
            .map(|hp| {
                hub.server
                    .world
                    .spawn((Replicated::default(), Health { hp }))
                    .id()
            })
            .collect();
        let mut drive = |n: usize| {
            for _ in 0..n {
                for e in &units {
                    if let Some(mut h) = hub.server.world.get_mut::<Health>(*e) {
                        h.hp += 1;
                    }
                }
                hub.server.tick();
                hub.server
                    .world
                    .resource_mut::<regia_net::runtime::NetNode>()
                    .outbox
                    .clear();
            }
        };
        drive(400);
        let before = allocations();
        drive(200);
        allocations() - before
    };
    println!(
        "allocations per tick, 16 changing entities, no clients: {:.1} without a recorder, {:.1} with one",
        without as f64 / 200.0,
        second as f64 / 200.0
    );
}
