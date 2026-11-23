// SPDX-License-Identifier: MPL-2.0
//! The desync fold allocates nothing (plan row F-04, ADR-8047): `Mix` over every serde shape,
//! `system_hashes`, `SystemRing::record` once the ring is full, and `DesyncHash::subsystem_hashes`
//! into a sized `Vec`. A counting global allocator counts this thread's allocations only, so other
//! test threads do not disturb the count. Its own test binary, because of the global allocator (the
//! pattern of `regia_physics/tests/aircraft_alloc.rs`).

#![allow(unsafe_code)]
// clippy 1.99 reports `missing_const_for_thread_local` on aarch64-linux-android only, at the
// `thread_local!` below whose initializers are already `const { }` (a false positive in the
// expansion; Linux and Windows clippy are clean). `expect` fails the build once it stops (ADR-8047).
#![cfg_attr(
    target_os = "android",
    expect(
        clippy::missing_const_for_thread_local,
        reason = "false positive on Android: the initializers are already const"
    )
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;

use regia_detsim::hash::{self, DesyncHash, Mix, RING_TICKS, SystemFold, SystemRing, Systems};
use serde::Serialize;

struct Counting;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

fn note() {
    // `try_with`: the allocator also runs while thread-locals are torn down.
    let armed = ARMED.try_with(Cell::get).unwrap_or(false);
    if armed {
        let _ = COUNT.try_with(|c| c.set(c.get() + 1));
    }
}

// SAFETY: forwards every call to the system allocator unchanged; the only addition is a
// thread-local counter update, which neither allocates nor touches the returned memory.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations made on this thread while `f` runs.
fn allocations<R>(f: impl FnOnce() -> R) -> (u64, R) {
    COUNT.with(|c| c.set(0));
    ARMED.with(|a| a.set(true));
    let out = f();
    ARMED.with(|a| a.set(false));
    (COUNT.with(Cell::get), out)
}

#[derive(Clone, Serialize)]
enum Duty {
    Idle,
    Escort(u32),
    Patrol { from: [i32; 2], to: [i32; 2] },
    Hold(u8, f32),
}

#[derive(Clone, Serialize)]
struct Body {
    id: u32,
    position: [f32; 3],
    heading: f64,
    duty: Duty,
    cargo: Option<Vec<u16>>,
    label: String,
    tag: (),
}

#[derive(Clone, Serialize)]
struct World {
    bodies: Vec<Body>,
    stock: BTreeMap<u32, (u16, bool)>,
    names: BTreeMap<String, i64>,
    seed: u128,
    bytes: Vec<u8>,
}

fn world() -> World {
    let duties = [
        Duty::Idle,
        Duty::Escort(7),
        Duty::Patrol {
            from: [1, 2],
            to: [3, 4],
        },
        Duty::Hold(2, -0.0),
    ];
    World {
        bodies: (0..400u32)
            .map(|i| Body {
                id: i,
                position: [i as f32, 0.5, -(i as f32)],
                heading: f64::from(i) * 0.25,
                duty: duties[i as usize % 4].clone(),
                cargo: (i % 3 == 0).then(|| vec![1, 2, 3]),
                label: format!("body-{i}"),
                tag: (),
            })
            .collect(),
        stock: (0..64u32).map(|i| (i, (i as u16, i % 2 == 0))).collect(),
        names: [("alpha", 1i64), ("beta", -2)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
        seed: u128::MAX / 3,
        bytes: vec![9; 37],
    }
}

impl SystemFold<3> for World {
    const SYSTEMS: Systems<3> = Systems::new(["units", "economy", "rng"]);
    fn fold_systems(&self, mix: &mut [Mix; 3]) {
        for body in &self.bodies {
            mix[0].u64(u64::from(body.id));
            mix[0].fold(body);
        }
        mix[1].fold(&self.stock);
        mix[1].fold(&self.names);
        mix[2].fold(&self.seed);
        mix[2].fold(&serde_bytes_like(&self.bytes));
    }
}

/// A `Serialize` that calls `serialize_bytes` (the byte-string path).
fn serde_bytes_like(bytes: &[u8]) -> Bytes<'_> {
    Bytes(bytes)
}

struct Bytes<'a>(&'a [u8]);

impl Serialize for Bytes<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_bytes(self.0)
    }
}

impl DesyncHash for World {
    fn world_hash(&self, _tick: u64) -> u64 {
        hash::system_hashes(self).combined()
    }
    fn subsystem_hashes(&self, _tick: u64, out: &mut Vec<(&'static str, u64)>) {
        hash::write_rows(&Self::SYSTEMS, &hash::system_hashes(self), out);
    }
}

#[test]
fn mix_fold_allocates_nothing() {
    let w = world();
    let (count, word) = allocations(|| {
        let mut mix = Mix::new(1);
        mix.fold(&w);
        mix.finish()
    });
    assert_eq!(count, 0, "Mix::fold allocated");
    assert_ne!(word, 0);
}

#[test]
fn system_fold_and_full_ring_allocate_nothing() {
    let w = world();
    let mut ring = SystemRing::<3>::new();
    for tick in 0..RING_TICKS as u64 {
        ring.record(tick, &w);
    }
    let (count, ()) = allocations(|| {
        for tick in RING_TICKS as u64..RING_TICKS as u64 + 600 {
            ring.record(tick, &w);
        }
        // A rewind replaces the tail in place.
        ring.record(RING_TICKS as u64 + 550, &w);
    });
    assert_eq!(count, 0, "a fold per tick allocated");
    // Ticks 600..=839 were held; 790..=839 are replaced by one row at 790.
    assert_eq!(ring.len(), RING_TICKS - 49);
}

#[test]
fn desync_hash_rows_allocate_nothing_once_sized() {
    let w = world();
    let mut rows = Vec::with_capacity(hash::MAX_SYSTEMS);
    let (count, world_hash) = allocations(|| {
        let mut last = 0;
        for tick in 0..100 {
            w.subsystem_hashes(tick, &mut rows);
            last = w.world_hash(tick);
        }
        last
    });
    assert_eq!(count, 0, "DesyncHash allocated");
    assert_eq!(rows.len(), 3);
    assert_eq!(world_hash, hash::system_hashes(&w).combined());
}

/// The counter itself works (a planted allocation is seen), so a zero above is real.
#[test]
fn the_counter_sees_a_planted_allocation() {
    let (count, v) = allocations(|| vec![1u8; 16]);
    assert_eq!(count, 1);
    assert_eq!(v.len(), 16);
}
