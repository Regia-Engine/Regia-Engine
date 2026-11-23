// SPDX-License-Identifier: MPL-2.0
//! Steady-state allocation bounds for code that landed after the v040 audio audit.
//!
//! Its own test binary: a counting global allocator (the pattern of
//! `tests/vehicle_audio_alloc.rs`). Counts are per thread.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_audio::voice_chat::{Member, MembershipTable, VoiceChannel, VoiceChannelKind};
use regia_audio::{ImpactBank, ImpactBankDef, ImpactContact, ImpactSet};
use regia_core::math::Vec3;

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

fn proximity_crowd(n: u16) -> MembershipTable {
    let mut table = MembershipTable::default();
    table.channels.insert(
        1,
        VoiceChannel {
            id: 1,
            name: "nearby".to_string(),
            kind: VoiceChannelKind::Proximity,
            radius: 50.0,
            spatial: true,
            priority: 2,
        },
    );
    for slot in 0..n {
        let mut blocked = std::collections::BTreeSet::new();
        // A real block list, of someone who is not in this channel, so both paths keep
        // every listener. Cloning the member still has to allocate the tree.
        blocked.insert(9_000);
        let mut muted = std::collections::BTreeSet::new();
        muted.insert(9_001);
        table.members.insert(
            slot,
            Member {
                channels: std::iter::once(1).collect(),
                position: [f32::from(slot) * 0.5, 0.0, 0.0],
                blocked,
                muted,
                excluded: false,
            },
        );
    }
    table
}

/// A warm proximity route must not clone the membership table. `recipients` already walks it
/// by reference and allocates only the result list; the cached path has to stay within that.
#[test]
fn proximity_route_allocates_no_more_than_the_uncached_walk() {
    const N: u16 = 24;
    let mut table = proximity_crowd(N);
    let warm = table.recipients_at(0, 1, 1_000);
    assert_eq!(warm.len(), (N - 1) as usize, "every other member is in range");
    let before = allocations();
    for _ in 0..20 {
        let got = table.recipients_at(0, 1, 1_050);
        assert_eq!(got.len(), (N - 1) as usize);
    }
    let cached = allocations() - before;
    let before = allocations();
    for _ in 0..20 {
        let got = table.recipients(0, 1);
        assert_eq!(got.len(), (N - 1) as usize);
    }
    let plain = allocations() - before;
    assert!(
        cached <= plain,
        "20 cached routes allocated {cached} times; 20 uncached walks allocated {plain}"
    );
}

fn impact_bank() -> ImpactBank {
    let mut def = ImpactBankDef::default();
    def.materials.insert(
        "wood".into(),
        ImpactSet {
            scrape: Some("scrape_loop".into()),
            roll: Some("roll_loop".into()),
            breaks: vec![
                "break_0".into(),
                "break_1".into(),
                "break_2".into(),
                "break_3".into(),
            ],
            ..ImpactSet::new(vec![
                "hit_0".into(),
                "hit_1".into(),
                "hit_2".into(),
                "hit_3".into(),
            ])
        },
    );
    def.destruction_materials.insert("crate".into(), "wood".into());
    ImpactBank::new(def)
}

fn contact() -> ImpactContact<'static> {
    ImpactContact {
        body: 7,
        material_a: "wood",
        material_b: "wood",
        impulse_ns: 12.0,
        normal_speed_m_s: 4.0,
        position: Vec3::new(1.0, 0.0, 0.0),
    }
}

/// Planning an impact owns three strings (the chosen sound and the two material names).
/// It must not clone the rest of the set.
#[test]
fn plan_impact_allocates_only_the_play_record() {
    let mut bank = impact_bank();
    let first = bank
        .plan_impact(&contact())
        .expect("the contact is above the minimums");
    assert_eq!(first.materials, ("wood".to_string(), "wood".to_string()));
    assert!(first.sound.starts_with("hit_"));
    let before = allocations();
    let play = bank
        .plan_impact(&contact())
        .expect("the second impact still resolves");
    let n = allocations() - before;
    assert_eq!(play.materials.0, "wood");
    assert!(play.sound.starts_with("hit_"), "{}", play.sound);
    assert!(
        n <= 3,
        "plan_impact allocated {n} times; the record needs the sound name and two material names"
    );
}

/// A break owns the tag, the mapped material and the chosen sound, and nothing else from the set.
#[test]
fn plan_break_allocates_only_the_play_record() {
    let mut bank = impact_bank();
    let before = allocations();
    let play = bank
        .plan_break("crate", 20.0, Vec3::new(0.0, 1.0, 0.0), 99)
        .expect("the break resolves");
    let n = allocations() - before;
    assert_eq!(play.kind, "break");
    assert_eq!(play.materials.0, "crate");
    assert_eq!(play.materials.1, "wood");
    assert!(play.sound.starts_with("break_"), "{}", play.sound);
    assert!(
        n <= 3,
        "plan_break allocated {n} times; the record needs the tag, the material and the sound"
    );
}
