// SPDX-License-Identifier: MPL-2.0
//! A steady wheel-effects step allocates nothing (no GPU, no wall clock). Its own test binary:
//! it installs a counting global allocator (the pattern of `empty_layer_alloc.rs`). The warm-up
//! fills the stamp pool to its cap and wraps the ring, then the counted steps run on a circle
//! that keeps laying and retiring stamps.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_core::math::Vec3;
use regia_render::quality::QualityTier;
use regia_vfx::vehicle_fx::{
    VehicleFx, VehicleFxConfig, VehicleFxLimits, WheelOutput, WheelSample, WheelSurface,
};

struct Counting;
thread_local! {
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

const DT: f32 = 1.0 / 120.0;

fn wheel(key: u64, i: usize, surface: WheelSurface) -> WheelSample {
    let (radius, speed) = (20.0f32, 15.0f32);
    let a = speed / radius * DT * i as f32 + key as f32;
    WheelSample {
        key,
        grounded: true,
        contact: Vec3::new(radius * a.sin(), 0.0, radius * (1.0 - a.cos())),
        normal: Vec3::Y,
        heading: Vec3::new((a + 0.5).cos(), 0.0, (a + 0.5).sin()),
        roll_speed: 11.0,
        surface,
        half_width: 0.12,
    }
}

#[test]
fn a_steady_step_does_not_allocate() {
    let mut fx = VehicleFx::new(VehicleFxConfig {
        limits: VehicleFxLimits::for_tier(QualityTier::Low),
        ..VehicleFxConfig::default()
    });
    let mut out: Vec<WheelOutput> = Vec::new();
    let mut step = |fx: &mut VehicleFx, i: usize| {
        let wheels = [
            wheel(1, i, WheelSurface::Asphalt),
            wheel(2, i, WheelSurface::Gravel),
            wheel(3, i, WheelSurface::Water),
            wheel(4, i, WheelSurface::Mud),
        ];
        fx.step(DT, &wheels, &mut out);
    };
    // Warm-up: the pool is full and has wrapped, every scratch buffer has its size.
    for i in 0..120 * 40 {
        step(&mut fx, i);
    }
    assert_eq!(fx.stats().live, 48);
    let laid = fx.stats().emitted;
    COUNTED.with(|c| c.set(true));
    let before = ALLOCS.load(Ordering::Relaxed);
    for i in 120 * 40..120 * 60 {
        step(&mut fx, i);
    }
    let after = ALLOCS.load(Ordering::Relaxed);
    COUNTED.with(|c| c.set(false));
    assert!(
        fx.stats().emitted > laid + 100,
        "the counted steps kept laying stamps"
    );
    assert_eq!(after - before, 0, "steady steps allocated");
}
