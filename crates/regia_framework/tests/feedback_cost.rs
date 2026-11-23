// SPDX-License-Identifier: MPL-2.0
//! Feedback costs nothing when unused: after warm-up, a frame of the plugin's per-frame work (the
//! frame recorder and the prompt system's run condition) allocates nothing. The ring is
//! allocated once when the plugin is added.
//!
//! Its own test binary: it installs a counting global allocator.

use std::time::Duration;

use regia_app::prelude::bevy_ecs::system::System as _;
use regia_app::prelude::*;
use regia_framework::feedback::{FeedbackConfig, FeedbackHost, FeedbackPlugin, record_feedback_frame};
use regia_framework::settings::GameSettings;

use regia_test_utils::alloc_counter::{self, CountingAlloc};

// The shared counting allocator (ADR-3291): this thread's requests.
#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc::new();

fn allocations() -> usize {
    alloc_counter::thread_allocs() as usize
}

#[test]
fn per_frame_work_allocates_nothing_when_unused() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    app.add_plugins(FeedbackPlugin {
        config: FeedbackConfig {
            crash_bundles: false,
            ..FeedbackConfig::default()
        },
    });
    app.insert_resource(GameSettings::default());
    app.world_mut().resource_mut::<FeedbackHost>().set("map", "mars");
    // Warm up: the first frames copy the settings and host fields for crash bundles.
    for _ in 0..5 {
        app.update();
    }
    let world = app.world_mut();
    let mut recorder = IntoSystem::into_system(record_feedback_frame);
    recorder.initialize(world);
    let _ = recorder.run((), world);
    // Advance real time so every run takes the "record a frame" path.
    world
        .resource_mut::<RealTime>()
        .advance_by(Duration::from_millis(16));
    let _ = recorder.run((), world);

    let before = allocations();
    for _ in 0..1000 {
        let _ = recorder.run((), world);
    }
    let after = allocations();
    assert_eq!(
        after - before,
        0,
        "the frame recorder allocated {} times in 1000 frames",
        after - before
    );

    // The prompt system stays idle: its run condition is false with nothing open, bound or clicked.
    let mut condition = IntoSystem::into_system(regia_framework::feedback::feedback_should_run);
    condition.initialize(world);
    assert_eq!(condition.run((), world).ok(), Some(false));
    let before = allocations();
    for _ in 0..1000 {
        let _ = condition.run((), world);
    }
    assert_eq!(allocations() - before, 0, "the run condition allocated");
}
