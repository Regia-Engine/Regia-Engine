// SPDX-License-Identifier: MPL-2.0
//! The two clocks (ADR-1524): a `SimStep` sets the simulation clock (`Time::sim_delta`, the fixed
//! loop) apart from the presentation clock (`Time::delta` in `Update`, `RealTime`). Everything is
//! deterministic: manual time, exact durations, no sleeps and no wall-clock assertions.

use std::time::Duration;

use regia_app::prelude::*;

const RENDER: Duration = Duration::from_millis(40);
const QUARTER: Duration = Duration::from_millis(10);

/// What the systems saw: `Update` deltas, and the `Time::elapsed` of each `FixedUpdate` run.
#[derive(Resource, Default)]
struct Seen {
    update_deltas: Vec<Duration>,
    update_sim_deltas: Vec<Duration>,
    fixed_elapsed: Vec<Duration>,
    fixed_deltas: Vec<Duration>,
}

fn record_update(time: Res<Time>, mut seen: ResMut<Seen>) {
    seen.update_deltas.push(time.delta());
    seen.update_sim_deltas.push(time.sim_delta());
}

fn record_fixed(time: Res<Time>, mut seen: ResMut<Seen>) {
    seen.fixed_elapsed.push(time.elapsed());
    seen.fixed_deltas.push(time.sim_delta());
}

/// A 25 fps manual clock with a 100 Hz fixed loop (one step is exactly 10 ms).
fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(RENDER));
    // `FixedTime` has a private accumulator, so set its knobs in place.
    app.init_resource::<FixedTime>();
    {
        let mut fixed = app.world_mut().resource_mut::<FixedTime>();
        fixed.hz = 100.0;
        fixed.max_substeps = 8;
    }
    app.init_resource::<Seen>();
    app.add_systems(Update, record_update);
    app.add_systems(FixedUpdate, record_fixed);
    app
}

#[test]
fn a_sim_step_slows_the_fixed_loop_and_leaves_the_presentation_clock_alone() {
    let mut plain = app();
    let mut slow = app();
    slow.insert_resource(SimStep(QUARTER));
    for _ in 0..8 {
        plain.update();
        slow.update();
    }
    // The presentation clock is the same in both: `Update` (VFX, shots, HUD) sees 40 ms a frame.
    let plain_seen = plain.world().resource::<Seen>();
    let slow_seen = slow.world().resource::<Seen>();
    assert_eq!(slow_seen.update_deltas, vec![RENDER; 8]);
    assert_eq!(slow_seen.update_deltas, plain_seen.update_deltas);
    assert_eq!(slow.world().resource::<RealTime>().elapsed(), RENDER * 8);
    assert_eq!(slow.world().resource::<Time>().elapsed(), RENDER * 8);
    // The simulation runs a quarter of the steps, never shorter ones.
    assert_eq!(plain_seen.fixed_elapsed.len(), 32);
    assert_eq!(slow_seen.fixed_elapsed.len(), 8);
    assert!(slow_seen.fixed_deltas.iter().all(|d| *d == QUARTER));
    // The fixed clock is the simulation clock: 10, 20, ... 80 ms.
    let expected: Vec<Duration> = (1..=8).map(|i| QUARTER * i).collect();
    assert_eq!(slow_seen.fixed_elapsed, expected);
    assert_eq!(slow_seen.update_sim_deltas, vec![QUARTER; 8]);
    let time = slow.world().resource::<Time>();
    assert_eq!(
        (time.delta(), time.sim_delta(), time.sim_elapsed()),
        (RENDER, QUARTER, QUARTER * 8)
    );
    // Without a SimStep the two clocks are one.
    let time = plain.world().resource::<Time>();
    assert_eq!(
        (time.sim_delta(), time.sim_elapsed()),
        (time.delta(), time.elapsed())
    );
}

#[test]
fn a_sim_step_is_absolute_and_stops_while_paused() {
    let mut app = app();
    app.insert_resource(SimStep(QUARTER));
    app.world_mut().resource_mut::<Time>().time_scale = 0.5;
    app.update();
    {
        let time = app.world().resource::<Time>();
        // `time_scale` scales the presentation step only.
        assert_eq!((time.delta(), time.sim_delta()), (RENDER / 2, QUARTER));
    }
    app.world_mut().resource_mut::<Time>().paused = true;
    app.update();
    let time = app.world().resource::<Time>();
    assert_eq!((time.delta(), time.sim_delta()), (Duration::ZERO, Duration::ZERO));
    assert_eq!(time.sim_elapsed(), QUARTER);
    assert_eq!(app.world().resource::<Seen>().fixed_elapsed.len(), 1);
}

#[test]
fn removing_the_sim_step_puts_the_simulation_back_on_the_frame_delta() {
    let mut app = app();
    app.insert_resource(SimStep(QUARTER));
    app.update();
    app.update();
    app.world_mut().remove_resource::<SimStep>();
    app.update();
    let time = app.world().resource::<Time>();
    assert_eq!(time.sim_delta(), RENDER);
    // Each clock keeps its own sum.
    assert_eq!(time.sim_elapsed(), QUARTER * 2 + RENDER);
    assert_eq!(time.elapsed(), RENDER * 3);
    let seen = app.world().resource::<Seen>();
    assert_eq!(seen.fixed_elapsed.len(), 2 + 4);
    assert_eq!(
        seen.fixed_elapsed.last().copied(),
        Some(QUARTER * 2 + RENDER),
        "the fixed clock carries on from the simulation time"
    );
}

#[test]
fn a_zero_sim_step_freezes_the_simulation_only() {
    let mut app = app();
    app.insert_resource(SimStep(Duration::ZERO));
    for _ in 0..3 {
        app.update();
    }
    let time = app.world().resource::<Time>();
    assert_eq!((time.elapsed(), time.sim_elapsed()), (RENDER * 3, Duration::ZERO));
    let seen = app.world().resource::<Seen>();
    assert!(seen.fixed_elapsed.is_empty());
    assert_eq!(seen.update_deltas, vec![RENDER; 3]);
}
