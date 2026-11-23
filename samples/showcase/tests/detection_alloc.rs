// SPDX-License-Identifier: MIT-0
//! `publish_detection` allocates nothing on a frame where the demo's tier pick did not change (fixrev-demo-warm-k,
//! review finding 5).
//!
//! The demo's `Last` system keeps `regia_agent::HostQualityDetection` on its tier pick. It used to build the whole
//! `host` JSON value (about ten string clones) on every frame just to compare it with the stored one. The heap ledger
//! (`regia_test_utils::heap_ledger`, ADR-2987) counts this thread's allocations inside a `Gameplay` scope; the
//! system's runs are compared with as many runs of a system that does nothing (the run call's own cost), so only
//! what the system body allocates is left. `DemoState::frame` changes on every frame (`count_frame` runs just before
//! it), so a plain `is_changed` gate would not help: the test bumps it between runs like the demo does.
//!
//! Its own test binary: it installs the tagging allocator.

#![cfg(feature = "agent")]

use regia::prelude::*;
use regia_agent::HostQualityDetection;
use regia_core::memory::{self, Subsystem};
use regia_render::QualityTier;
use regia_scene::level::LevelTimeOfDay;
use regia_vfx::WeatherState;
use showcase::demo::bench::BenchStatus;
use showcase::demo::credits::Credits;
use showcase::demo::quality::{TierWords, host_detection, publish_detection};
use showcase::demo::{DemoPage, DemoState, TierSource, WeatherSliders};

regia_test_utils::install_tagging_allocator!();

const FRAMES: u64 = 64;

fn state() -> DemoState {
    DemoState {
        page: DemoPage::Controls,
        time_of_day: LevelTimeOfDay::default(),
        weather: WeatherSliders::of(&WeatherState::FAIR),
        tier: QualityTier::High,
        tier_source: TierSource::Detected,
        detection: Some(TierWords {
            tier: QualityTier::High,
            words: "Picked High (a test card).".into(),
            reason: "test".into(),
            rule: "fallback".into(),
            evidence: "guess".into(),
            adapter: "Test GPU".into(),
        }),
        screenshots: 0,
        last_screenshot: None,
        screenshot_error: None,
        bench: BenchStatus::default(),
        credits: Credits::default(),
        frame: 0,
    }
}

/// Allocations this thread made in `FRAMES` runs of `system`, each after `count_frame`'s bump of the frame counter.
fn allocations_over_frames<M>(world: &mut World, system: impl IntoSystem<(), (), M>) -> u64 {
    let mut system = IntoSystem::into_system(system);
    system.initialize(world);
    // The first runs settle (the first publish, lazily built tables).
    for _ in 0..4 {
        world.resource_mut::<DemoState>().frame += 1;
        if let Err(err) = system.run((), world) {
            panic!("system: {err}");
        }
    }
    let before = regia_test_utils::heap_ledger::require_ledger()
        .subsystem(Subsystem::Gameplay)
        .total_allocations;
    for _ in 0..FRAMES {
        world.resource_mut::<DemoState>().frame += 1;
        let ran = memory::scoped(Subsystem::Gameplay, || system.run((), world));
        if let Err(err) = ran {
            panic!("system: {err}");
        }
    }
    regia_test_utils::heap_ledger::require_ledger()
        .subsystem(Subsystem::Gameplay)
        .total_allocations
        - before
}

fn idle() {}

#[test]
fn publishing_an_unchanged_detection_allocates_nothing_per_frame() {
    let mut world = World::new();
    world.insert_resource(state());
    world.insert_resource(HostQualityDetection::default());
    let baseline = allocations_over_frames(&mut world, idle);
    let published = allocations_over_frames(&mut world, publish_detection);
    println!("{FRAMES} frames: publish_detection {published} allocations, an idle system {baseline}");
    assert_eq!(
        world.resource::<HostQualityDetection>().0.as_ref(),
        Some(&host_detection(world.resource::<DemoState>())),
        "published"
    );
    assert!(
        published.saturating_sub(baseline) < FRAMES,
        "fewer allocations than frames: {published} against {baseline} for an idle system"
    );
}

#[test]
fn a_changed_pick_is_still_published() {
    let mut world = World::new();
    world.insert_resource(state());
    world.insert_resource(HostQualityDetection::default());
    let mut system = IntoSystem::into_system(publish_detection);
    system.initialize(&mut world);
    fn run(system: &mut impl System<In = (), Out = ()>, world: &mut World) {
        world.resource_mut::<DemoState>().frame += 1;
        if let Err(err) = system.run((), world) {
            panic!("system: {err}");
        }
    }
    run(&mut system, &mut world);
    run(&mut system, &mut world);
    {
        let mut state = world.resource_mut::<DemoState>();
        state.tier = QualityTier::Low;
        state.tier_source = TierSource::Picked;
    }
    run(&mut system, &mut world);
    let host = world.resource::<HostQualityDetection>().0.clone();
    assert_eq!(
        host.as_ref(),
        Some(&host_detection(world.resource::<DemoState>()))
    );
    assert_eq!(host.as_ref().and_then(|v| v["source"].as_str()), Some("picked"));
    // Words of a new detection with the same tier and source.
    if let Some(words) = world.resource_mut::<DemoState>().detection.as_mut() {
        words.words.push_str(" Again.");
    }
    run(&mut system, &mut world);
    assert_eq!(
        world.resource::<HostQualityDetection>().0.as_ref(),
        Some(&host_detection(world.resource::<DemoState>()))
    );
    // Someone else overwrote the published value: the next run restores it.
    world.resource_mut::<HostQualityDetection>().0 = None;
    run(&mut system, &mut world);
    assert_eq!(
        world.resource::<HostQualityDetection>().0.as_ref(),
        Some(&host_detection(world.resource::<DemoState>()))
    );
}
