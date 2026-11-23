// SPDX-License-Identifier: MPL-2.0
//! Heap allocations of a steady ranked update (ADR-3275 (8)): once the ranking's buffers have
//! grown, 500 instances under the Low tier's caps update with no allocation, the same as the
//! scene without caps.
//!
//! Its own test binary (a counting global allocator, the pattern of `empty_layer_alloc.rs`), CPU
//! only. Fixed frame counts, no wall clock.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_core::math::Vec3;
use regia_render::QualityTier;
use regia_vfx::VfxCamera;
use regia_vfx::VfxScene;
use regia_vfx::format::{VFX_FORMAT_VERSION, VfxSystemDef};
use regia_vfx::quality::significance_caps;
use regia_vfx::scene::VfxLibrary;
use regia_vfx::significance::SignificanceView;
use regia_vfx::sim::{LodInput, SpawnJob};

struct Counting;
thread_local! {
    /// Whether this thread's allocations count (the test thread, while measuring).
    static COUNTED: Cell<bool> = const { Cell::new(false) };
}
static ALLOCS: AtomicU64 = AtomicU64::new(0);

fn note() {
    if COUNTED.try_with(Cell::get).unwrap_or(false) {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
    }
}

// SAFETY: forwards to the system allocator; only counts.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

const DT: f32 = 1.0 / 64.0;

fn crowd(ranked: bool) -> VfxScene {
    let def = VfxSystemDef::from_ron(&format!(
        r#"(
    version: {VFX_FORMAT_VERSION},
    name: "smoke",
    system: (budget: 200),
    emitters: [
        (
            name: "puffs",
            spawn: (mode: Rate(64.0), lifetime: (min: 1.0, max: 1.0)),
            init: (size: (min: 0.2, max: 0.4)),
            render: (mode: Billboard),
        ),
    ],
)"#
    ))
    .expect("fixture");
    let mut library = VfxLibrary::new();
    library.add(def).expect("valid");
    let mut scene = VfxScene::new(library);
    if ranked {
        scene
            .significance_mut()
            .set_caps(significance_caps(QualityTier::Low));
        scene
            .significance_mut()
            .set_view(Some(SignificanceView::from_camera(
                &VfxCamera::looking_at(Vec3::ZERO, Vec3::new(0.0, 0.0, -10.0)),
                1280,
                720,
            )));
    }
    for i in 0..500u32 {
        // Half in front of the camera, half behind it (paused), on a line.
        let side = if i.is_multiple_of(2) { -1.0 } else { 1.0 };
        let at = Vec3::new(0.0, 0.0, side * (2.0 + 0.4 * i as f32));
        scene
            .spawn("smoke", at, Vec3::Z, u64::from(i) + 1)
            .expect("spawn");
    }
    scene
}

/// Allocations of `frames` updates after `warm` ones, each frame's jobs drained into one reused
/// buffer (the host's upload).
fn allocations(scene: &mut VfxScene, warm: u32, frames: u32) -> u64 {
    let lod = LodInput {
        camera_pos: Vec3::ZERO,
    };
    let mut jobs: Vec<SpawnJob> = Vec::with_capacity(4096);
    let mut step = |scene: &mut VfxScene| {
        scene.update(DT, &lod, 100_000);
        for instance in scene.instances_mut() {
            instance.drain_jobs(&mut jobs);
        }
        jobs.clear();
    };
    for _ in 0..warm {
        step(scene);
    }
    let before = ALLOCS.load(Ordering::Relaxed);
    COUNTED.with(|c| c.set(true));
    for _ in 0..frames {
        step(scene);
    }
    COUNTED.with(|c| c.set(false));
    ALLOCS.load(Ordering::Relaxed) - before
}

/// No allocation in 128 steady ranked updates of 500 instances (the ranking's buffers, the scene's
/// death queues and job buffers have all grown during the 160 warm frames), and none in the same
/// scene without caps.
#[test]
fn a_steady_ranked_update_does_not_allocate() {
    let mut plain = crowd(false);
    let baseline = allocations(&mut plain, 160, 128);
    let mut ranked = crowd(true);
    let with_caps = allocations(&mut ranked, 160, 128);
    let stats = ranked.significance().stats();
    assert!(
        stats.enabled && stats.paused == 250 && stats.full + stats.reduced == 32,
        "{stats:?}"
    );
    assert_eq!(baseline, 0, "the scene without caps allocates in steady state");
    assert_eq!(with_caps, 0, "the ranked update allocates in steady state");
}
