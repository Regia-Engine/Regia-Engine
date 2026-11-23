// SPDX-License-Identifier: MIT-0
//! Headless smoke test for `audio_lab`: the guided tour on the offline audio backend with manual
//! 16 ms steps (the same app `audio_lab --headless` builds), no window and no audio device.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use audio_lab::{ALL, AudioLabPlugin, ExitAfterFrames, LabState, Scenario};
use regia::prelude::*;
use regia_audio::AudioSettings;

/// The headless app `main` builds for `--headless`.
fn headless_app() -> App {
    let mut app = App::new();
    app.add_plugins(MinimalPlugins)
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)))
        .insert_resource(AudioSettings {
            headless: true,
            ..AudioSettings::default()
        })
        .add_plugins(AudioLabPlugin { auto_advance: true });
    app
}

#[test]
fn tour_starts_on_orbit_and_time_advances() {
    let mut app = headless_app();
    for _ in 0..30 {
        app.update();
    }
    let state = app.world().resource::<LabState>();
    assert_eq!(state.scenario, Scenario::Orbit);
    assert!(state.elapsed > 0.0, "scenario clock runs: {}", state.elapsed);
    assert!(
        state.elapsed < Scenario::Orbit.duration(),
        "30 frames stay inside the first scenario"
    );
}

#[test]
fn tour_visits_every_scenario_in_order() {
    let mut app = headless_app();
    app.update();
    for (i, scenario) in ALL.iter().enumerate() {
        let current = app.world().resource::<LabState>().scenario;
        assert_eq!(current, *scenario, "scenario {i}");
        // Jump the clock to the end of the scenario: the next update advances the tour.
        app.world_mut().resource_mut::<LabState>().elapsed = scenario.duration();
        app.update();
    }
    // Seven advances wrap around to the first scenario.
    assert_eq!(app.world().resource::<LabState>().scenario, ALL[0]);
}

#[test]
fn exit_after_frames_ends_the_run_successfully() {
    let mut app = headless_app();
    app.insert_resource(ExitAfterFrames(120));
    assert!(matches!(app.run(), AppExit::Success));
}
