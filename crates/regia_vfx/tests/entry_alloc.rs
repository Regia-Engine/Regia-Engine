// SPDX-License-Identifier: MPL-2.0
//! A full-tier entry streak allocates nothing on the frame after warm-up (the pattern of
//! `ribbon_alloc.rs`). Its own test binary; the counter is per thread, so the harness's
//! threads do not count.

// The counting allocator is `unsafe` (ADR-1672, the same pattern as `ribbon_alloc.rs`).
#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_core::math::Vec3;
use regia_vfx::entry::{EntryScratch, EntryTrack, TrackPoint};
use regia_vfx::entry_runtime::step_entry_streaks;
use regia_vfx::{EntryStreak, VfxTrails, VfxView};

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<u64> = const { Cell::new(0) };
}

fn allocations() -> u64 {
    ALLOCATIONS.try_with(Cell::get).unwrap_or(0)
}

// SAFETY: forwards to the system allocator unchanged; only counts calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract is passed through.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract is passed through.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
        // SAFETY: the caller's contract is passed through.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// 11 km/s at 40 km, so the default full-tier profile is glowing and the dust window can fill.
fn glowing_track() -> EntryTrack {
    let at = |time: f32, x: f32| TrackPoint {
        time,
        position: Vec3::new(x, 40_000.0, 0.0),
    };
    EntryTrack::new(vec![at(0.0, 0.0), at(60.0, 660_000.0)]).expect("two-point track")
}

fn streak_at_dust_window() -> EntryStreak {
    let mut streak = EntryStreak::new(glowing_track());
    // Default dust life is 24 s. Past that, the train length has stopped growing.
    streak.frame = 30 * 60;
    streak
}

fn step(view: &VfxView, trails: &mut VfxTrails, streak: &mut EntryStreak, scratch: &mut EntryScratch) {
    step_entry_streaks(view, trails, std::iter::once(streak), scratch);
}

#[test]
fn a_warm_full_streak_frame_allocates_nothing() {
    let view = VfxView::default();
    let mut trails = VfxTrails::default();
    let mut streak = streak_at_dust_window();
    let mut scratch = EntryScratch::new();
    for _ in 0..3 {
        step(&view, &mut trails, &mut streak, &mut scratch);
        trails.0.clear();
    }
    let before = allocations();
    step(&view, &mut trails, &mut streak, &mut scratch);
    let delta = allocations() - before;
    assert!(
        trails.0.len() > 100,
        "the dust train produced ribbons, got {}",
        trails.0.len()
    );
    assert_eq!(delta, 0, "warm full-tier step_entry_streaks allocated {delta}");
}

#[test]
fn a_paused_streak_replays_without_allocating() {
    let view = VfxView::default();
    let mut trails = VfxTrails::default();
    let mut streak = streak_at_dust_window();
    streak.playing = false;
    let frame = streak.frame;
    let mut scratch = EntryScratch::new();
    step(&view, &mut trails, &mut streak, &mut scratch);
    let built = trails.0.len();
    trails.0.clear();
    let before = allocations();
    step(&view, &mut trails, &mut streak, &mut scratch);
    let delta = allocations() - before;
    assert_eq!(streak.frame, frame, "a paused streak does not advance");
    assert_eq!(trails.0.len(), built, "the replay appends the same ribbons");
    assert_eq!(delta, 0, "paused replay allocated {delta}");
}
