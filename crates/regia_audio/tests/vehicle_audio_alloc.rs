// SPDX-License-Identifier: MPL-2.0
//! ADR-3271: the vehicle audio model allocates nothing per step in steady state.
//!
//! Its own test binary: it installs a counting global allocator (the pattern of
//! `tests/bus_meter_alloc.rs`).

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_audio::{ONE_SHOT_CAPACITY, VehicleAudioDef, VehicleAudioInputs, VehicleAudioModel};

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

#[test]
fn stepping_allocates_nothing_after_the_model_is_built() {
    let mut model = VehicleAudioModel::new(VehicleAudioDef::placeholder(), 4).expect("valid");
    let gravel = model.surface_index(Some("gravel"));
    let mut shots = Vec::with_capacity(ONE_SHOT_CAPACITY);
    let mut inputs = VehicleAudioInputs {
        wheel_count: 4,
        ..VehicleAudioInputs::default()
    };
    let dt = 1.0 / 120.0;
    let drive = |model: &mut VehicleAudioModel, step: u32, inputs: &mut VehicleAudioInputs| {
        let t = step as f32 * dt;
        inputs.rpm = 900.0 + (t * 900.0) % 6200.0;
        inputs.throttle = if step % 200 < 150 { 1.0 } else { 0.0 };
        inputs.engine_torque = inputs.throttle * 280.0 - 30.0;
        inputs.upshifts = step / 97;
        inputs.downshifts = step / 301;
        inputs.speed = t * 3.0;
        for (i, wheel) in inputs.wheels[..4].iter_mut().enumerate() {
            wheel.grounded = !step.is_multiple_of(50);
            wheel.slip_angle = (t + i as f32).sin() * 0.25;
            wheel.slip_ratio = (t * 0.7).cos() * 0.2;
            wheel.contact_speed = t * 3.0;
            wheel.compression = if step.is_multiple_of(37) { 0.06 } else { 0.02 };
            wheel.surface = if (step / 120 + i as u32).is_multiple_of(2) {
                gravel
            } else {
                0
            };
        }
        model.step(inputs, dt);
    };
    // Warm-up: the first step snaps and caches the smoothing coefficients.
    for step in 0..10 {
        drive(&mut model, step, &mut inputs);
        model.drain_one_shots(&mut shots);
    }
    let before = allocations();
    for step in 10..2_000 {
        drive(&mut model, step, &mut inputs);
        model.drain_one_shots(&mut shots);
        let _ = model.parameter_digest();
    }
    let after = allocations();
    assert_eq!(after - before, 0, "steps allocated {} times", after - before);
    assert!(
        model.shifts_fired() > 10 && model.bumps_fired() > 10,
        "the run exercised one-shots"
    );
}
