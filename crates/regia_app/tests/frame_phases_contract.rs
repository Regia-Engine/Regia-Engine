// SPDX-License-Identifier: MPL-2.0
//! Contract tests for the published frame-phase record.

use regia_app::prelude::*;
use regia_app::{App, FramePhases, PHASE_COUNT, PHASE_NAMES, Phase};

#[test]
fn phase_names_and_discriminants_match_the_published_order() {
    assert_eq!(PHASE_NAMES.len(), PHASE_COUNT);
    let expected = [
        "outside_update",
        "host_schedules",
        "pacing",
        "first",
        "pre_update",
        "state_transition",
        "fixed_update",
        "update",
        "post_update",
        "extract",
        "render",
        "last",
        "clear_trackers",
        "event_wait",
    ];
    let discriminants = [
        Phase::OutsideUpdate,
        Phase::HostSchedules,
        Phase::Pacing,
        Phase::First,
        Phase::PreUpdate,
        Phase::StateTransition,
        Phase::FixedUpdate,
        Phase::Update,
        Phase::PostUpdate,
        Phase::Extract,
        Phase::Render,
        Phase::Last,
        Phase::ClearTrackers,
        Phase::EventWait,
    ];

    assert_eq!(PHASE_NAMES, expected);
    assert_eq!(
        discriminants.map(|phase| phase as usize),
        std::array::from_fn(|i| i)
    );
}

#[derive(Resource, Default)]
struct ScheduleObservations {
    update_runs: u64,
    last_runs: Vec<(u64, u64)>,
}

fn observe_update(mut seen: ResMut<ScheduleObservations>) {
    seen.update_runs += 1;
}

fn observe_last(phases: Res<FramePhases>, mut seen: ResMut<ScheduleObservations>) {
    let update_runs = seen.update_runs;
    seen.last_runs.push((update_runs, phases.frame));
}

#[test]
fn update_and_last_run_and_last_reads_the_previous_frame_record() {
    let mut app = App::new();
    app.init_resource::<ScheduleObservations>()
        .add_systems(Update, observe_update)
        .add_systems(Last, observe_last);

    for _ in 0..4 {
        app.update();
    }

    let seen = app.world().resource::<ScheduleObservations>();
    assert_eq!(seen.update_runs, 4);
    assert_eq!(seen.last_runs, [(1, 0), (2, 1), (3, 2), (4, 3)]);
    assert_eq!(app.world().resource::<FramePhases>().frame, 4);
}
