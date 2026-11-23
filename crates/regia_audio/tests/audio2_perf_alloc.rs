// SPDX-License-Identifier: MPL-2.0
//! Steady-frame allocation counts for the audio2 propagation hot path.
//!
//! Its own binary so it can install a counting global allocator (the pattern of
//! `bus_meter_alloc.rs`). Counts are the measurement: no wall-clock budget.

#![allow(unsafe_code)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use regia_audio::{
    Audio, AudioEmitter, AudioMedium, AudioOcclusion, AudioPropagation, AudioSettings, EmitterCategory,
    EmitterFrame, GroundMaterial, ListenerContact, ListenerCoupling, ListenerFrame, OcclusionRay,
    PropagationContext, Spreading, audio_propagation_sync, db_to_gain, ground_arrival, propagate,
};
use regia_core::math::Vec3;
use regia_ecs::prelude::*;

struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

// SAFETY: forwards to the system allocator and only counts calls on this thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|cell| cell.set(cell.get() + 1));
        // SAFETY: same contract as the caller.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as the caller.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let _ = ALLOCATIONS.try_with(|cell| cell.set(cell.get() + 1));
        // SAFETY: same contract as the caller.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

fn allocations() -> usize {
    ALLOCATIONS.with(Cell::get)
}

fn ray(i: u32) -> OcclusionRay {
    OcclusionRay {
        emitter: Entity::from_raw_u32(i).expect("entity"),
        listener: None,
        from: Vec3::ZERO,
        to: Vec3::new(i as f32, 0.0, 0.0),
    }
}

fn count_during(frames: usize, mut body: impl FnMut()) -> usize {
    let before = allocations();
    for _ in 0..frames {
        body();
    }
    allocations() - before
}

/// Finding 1: a world emitter with no interior and no ground path must not build an
/// `AudioMedium` (the `"soil"` string) on every solve.
#[test]
fn propagate_allocates_nothing_for_air_only_world_emitters() {
    let medium = AudioMedium::earth();
    let listener = ListenerFrame::default();
    let coupling = ListenerCoupling {
        contact: ListenerContact::None,
        ..ListenerCoupling::default()
    };
    let ctx = PropagationContext {
        medium: &medium,
        listener: &listener,
        coupling: &coupling,
        coupling_weight: 0.0,
        last_contact: ListenerContact::None,
        interiors: &[],
    };
    let frame = EmitterFrame {
        position: Vec3::new(30.0, 0.0, 0.0),
        ..EmitterFrame::default()
    };
    let params = AudioEmitter {
        min_distance: 1.0,
        max_distance: 5_000.0,
        ..AudioEmitter::default()
    };
    let propagation = AudioPropagation::for_category(EmitterCategory::Projectile);
    // The interior medium, if cached, is built on the first solve.
    let warmup = propagate(&ctx, &frame, &params, &propagation, false);
    assert!(
        warmup.airborne.is_some(),
        "earth air still carries the projectile"
    );
    assert!(
        warmup.ground.is_none(),
        "an uncoupled listener has no ground path"
    );
    let allocs = count_during(64, || {
        let solved = propagate(&ctx, &frame, &params, &propagation, false);
        assert!(solved.airborne.is_some());
    });
    assert_eq!(
        allocs, 0,
        "64 air-only solves allocated {allocs} times after warmup (interior medium string)"
    );
}

/// Finding 3: boots + in-range machinery resolves the ground preset without rebuilding the
/// material or the interface chain, and the arrival gain matches `ground_arrival`.
///
/// `Propagation.ground_material` stays `Option<String>`, so each solve owns that one name.
/// Everything else on this path (preset lookup, the interface chain) must not allocate.
#[test]
fn propagate_machinery_matches_ground_arrival_without_allocating() {
    let medium = AudioMedium::earth();
    let listener = ListenerFrame::default();
    let coupling = ListenerCoupling::default();
    let ctx = PropagationContext {
        medium: &medium,
        listener: &listener,
        coupling: &coupling,
        coupling_weight: 1.0,
        last_contact: ListenerContact::Boots,
        interiors: &[],
    };
    let distance = 10.0;
    let frame = EmitterFrame {
        position: Vec3::new(distance, 0.0, 0.0),
        ..EmitterFrame::default()
    };
    let params = AudioEmitter {
        min_distance: 1.0,
        max_distance: 5_000.0,
        ..AudioEmitter::default()
    };
    let propagation = AudioPropagation::for_category(EmitterCategory::Machinery);
    let material = GroundMaterial::named("soil").expect("soil");
    let ear = ListenerContact::Boots.filter().expect("boots");
    let arrival = ground_arrival(
        &material,
        distance,
        Spreading::Surface,
        propagation.source_size_m,
        propagation.effective_coupling() * db_to_gain(propagation.ground_level_db),
    );
    let expected = (arrival.amplitude * db_to_gain(ear.gain_db)).clamp(0.0, 1.0);
    let _ = propagate(&ctx, &frame, &params, &propagation, false);
    let mut gain = 0.0;
    let allocs = count_during(32, || {
        let solved = propagate(&ctx, &frame, &params, &propagation, false);
        gain = solved.ground.expect("ground path").gain;
        assert_eq!(solved.ground_material.as_deref(), Some("soil"));
    });
    assert!(
        (gain - expected).abs() < 1e-5,
        "machinery gain {gain} != ground_arrival {expected}"
    );
    assert_eq!(
        allocs, 32,
        "32 machinery solves allocated {allocs} times after warmup; only the returned ground name may allocate"
    );
}

/// Finding 4: unchanged `AudioPropagation` rows are not cloned again on later frames.
#[test]
fn propagation_sync_allocates_nothing_while_emitters_are_unchanged() {
    fn world_with(count: u32) -> (World, Schedule) {
        let mut world = World::new();
        world.insert_resource(Audio::new(&AudioSettings {
            headless: true,
            ..AudioSettings::default()
        }));
        for _ in 0..count {
            world.spawn(AudioPropagation {
                ground: Some("bedrock".into()),
                ..AudioPropagation::for_category(EmitterCategory::Machinery)
            });
        }
        let mut schedule = Schedule::default();
        schedule.add_systems(audio_propagation_sync);
        // First runs build the schedule state and the emitter map.
        schedule.run(&mut world);
        schedule.run(&mut world);
        (world, schedule)
    }

    let (mut idle_world, mut idle_schedule) = world_with(0);
    let baseline = count_during(8, || idle_schedule.run(&mut idle_world));
    let (mut world, mut schedule) = world_with(128);
    let steady = count_during(8, || schedule.run(&mut world));
    assert_eq!(
        steady,
        baseline,
        "128 unchanged emitters allocated {} times over 8 frames (baseline {baseline})",
        steady.saturating_sub(baseline)
    );
}

/// Finding 5: queueing the same candidates plus extra rays keeps one set and one merge buffer.
#[test]
fn occlusion_queue_allocates_nothing_after_the_scratch_is_warm() {
    // Integration tests cannot use struct-update syntax: the other fields are private.
    let mut occlusion = AudioOcclusion::default();
    occlusion.rays_per_frame = 16;
    let candidates: Vec<OcclusionRay> = (1..=300).map(ray).collect();
    occlusion.set_extra_rays((1_000..1_004).map(ray).collect());
    for ray in &candidates {
        occlusion.report(ray.emitter, 0.4);
        occlusion.report_thickness(ray.emitter, 1.5);
    }
    occlusion.queue(&candidates);
    let allocs = count_during(20, || occlusion.queue(&candidates));
    assert_eq!(occlusion.pending().len(), 16, "the ray budget still applies");
    assert_eq!(
        allocs, 0,
        "20 queues of 300 candidates plus extra rays allocated {allocs} times after warmup"
    );
    // A dropped candidate loses its report; one that stayed keeps it.
    let kept: Vec<OcclusionRay> = candidates
        .into_iter()
        .filter(|ray| (ray.to.x as u32).is_multiple_of(2))
        .collect();
    let kept_emitter = kept[0].emitter;
    let dropped = ray(1);
    occlusion.queue(&kept);
    assert_eq!(occlusion.reported(kept_emitter), 0.4);
    assert_eq!(occlusion.reported_thickness(kept_emitter), 1.5);
    assert_eq!(occlusion.reported(dropped.emitter), 0.0);
    assert_eq!(occlusion.reported_thickness(dropped.emitter), 0.0);
}
