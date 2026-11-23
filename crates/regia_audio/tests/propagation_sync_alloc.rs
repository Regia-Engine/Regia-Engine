// SPDX-License-Identifier: MPL-2.0
//! The propagation sync system copies nothing while the world is unchanged.
//!
//! `World::run_system_once` builds system state on every call, so the test reuses one schedule
//! and measures the second run. The default executor is multi-threaded: each `Schedule::run`
//! allocates an async task and a queue block on the calling thread even when the system
//! allocates nothing. `SingleThreadedExecutor` runs the system on this thread, so the count is
//! the system.
//!
//! The executor can still spend an allocation per system it runs, whatever the system does: the
//! workspace enables `bevy_ecs/trace`, whose per-system span formats the system's name, and in a
//! workspace build `tracing/log` (pulled in by the Wayland and breakpad dependencies) makes a
//! span evaluate its fields when no tracing dispatcher is installed, as in this test process
//! (ADR-5554). So the count is taken against the same schedule running a system that does
//! nothing: the propagation sync must add exactly zero.

#![allow(unsafe_code)]

include!("support/alloc_count.rs");

use regia_audio::{
    Audio, AudioInteriorVolume, AudioListener, AudioPropagation, AudioSettings, ListenerCoupling,
    audio_propagation_sync,
};
use regia_ecs::prelude::*;

fn world() -> World {
    let mut world = World::new();
    world.insert_resource(Audio::new(&AudioSettings {
        headless: true,
        ..AudioSettings::default()
    }));
    world.spawn((AudioListener { hrtf: false }, ListenerCoupling::default()));
    world.spawn(AudioInteriorVolume::default());
    world.spawn(AudioPropagation::default());
    world
}

/// Allocations of the second run, on this thread, of a schedule that `add` gives one system.
fn second_run_allocations(add: impl FnOnce(&mut Schedule)) -> usize {
    let mut world = world();
    let mut schedule = Schedule::default();
    add(&mut schedule);
    schedule.set_executor(regia_ecs::ecs::schedule::SingleThreadedExecutor::new());
    schedule.run(&mut world);
    let before = allocations();
    schedule.run(&mut world);
    allocations() - before
}

fn nothing() {}

#[test]
fn an_unchanged_world_syncs_propagation_without_allocating() {
    let executor = second_run_allocations(|schedule| {
        schedule.add_systems(nothing);
    });
    let allocs = second_run_allocations(|schedule| {
        schedule.add_systems(audio_propagation_sync);
    });
    assert_eq!(
        allocs.saturating_sub(executor),
        0,
        "the second propagation sync allocated {allocs} times, {executor} of them the executor's \
         own (a system that does nothing)"
    );
    assert!(
        allocs >= executor,
        "the sync allocated less than an empty system: {allocs} < {executor}"
    );
}
