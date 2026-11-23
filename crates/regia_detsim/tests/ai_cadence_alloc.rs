// SPDX-License-Identifier: MPL-2.0
//! Allocation proof for the think cadence due lists (ADR-8560): once warm, `AiCore::due_units` and
//! `AiCore::decide` allocate nothing, capped or not. Counting harness adapted from Regia hash_alloc.rs (ADR-8047).

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

use regia_detsim::ai::{
    AiAgent, AiCadence, AiCore, ArgMax, CadenceChannel, CadenceTier, ChannelId, OptionId, PhaseRule,
};

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

#[derive(Clone, Copy)]
struct Unit {
    id: u32,
    tier: u8,
}

impl AiAgent for Unit {
    fn entity_id(&self) -> u32 {
        self.id
    }

    fn cadence_tier(&self) -> u8 {
        self.tier
    }
}

#[test]
fn warm_due_lists_allocate_nothing() {
    let cadence = AiCadence::default()
        .with_tiers(vec![
            CadenceTier::new(1),
            CadenceTier::new(2),
            CadenceTier::new(4),
        ])
        .and_then(|c| {
            c.with_channel(CadenceChannel::new("route", 30).with_phase(PhaseRule::Hashed { salt: 2 }))
        })
        .and_then(|c| c.with_channel(CadenceChannel::new("target", 4).with_max_per_tick(Some(40))))
        .expect("valid settings");
    let route = cadence.channel_id("route").expect("added");
    let target = cadence.channel_id("target").expect("added");
    let mut core = AiCore::new(7, cadence, ArgMax);
    // 900 units, many with clustered ids, in a scrambled order; tiers change with the tick.
    let mut units: Vec<Unit> = (0..900u32)
        .map(|i| Unit {
            id: if i.is_multiple_of(3) { i * 10 } else { i * 7 + 1 },
            tier: 0,
        })
        .collect();
    units.reverse();
    let mut out = Vec::new();
    let run = |core: &mut AiCore<ArgMax>, units: &mut [Unit], tick: u64, out: &mut Vec<usize>| -> u64 {
        for u in units.iter_mut() {
            u.tier = ((u.id as u64 + tick / 50) % 3) as u8;
        }
        let mut ran = 0u64;
        for ch in [ChannelId::SIGHT, route, target] {
            let r = core.due_units(ch, tick, units, out).expect("known channel");
            ran += u64::from(r.ran);
        }
        let r = core.decide(tick, units, |_, _, s| s.push((OptionId::Hold, 1.0)));
        ran + u64::from(r.due)
    };
    // Warm: one call fills every buffer's capacity (the decide decision map grows once per new unit).
    for tick in 0..120u64 {
        run(&mut core, &mut units, tick, &mut out);
    }
    let (count, ran) = allocations(|| {
        let mut ran = 0u64;
        for tick in 120..1_320u64 {
            ran += run(&mut core, &mut units, tick, &mut out);
        }
        ran
    });
    assert!(ran > 10_000, "the channels did work: {ran}");
    assert!(
        !core.queued(target).expect("known channel").is_empty(),
        "the capped channel queued units"
    );
    assert_eq!(count, 0, "warm due lists allocated {count} times");
}
