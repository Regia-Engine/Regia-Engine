// SPDX-License-Identifier: MPL-2.0
//! Public-behavior and error-path tests for the loop settings, the clocks, the fixed-step
//! accumulator and frame pacing (sprint test-gap closure for `regia_app`). Everything is
//! deterministic: manual time, fixed inputs, no sleeps and no wall-clock assertions.

use std::time::Duration;

use regia_app::prelude::*;
use regia_app::{DEFAULT_THROTTLED_FPS, FramePhases, MAX_DELTA, Phase};

const MS16: Duration = Duration::from_millis(16);

/// A `FixedTime` with the given knobs (its accumulator is private, so go through the world).
fn fixed(hz: f64, max_substeps: u32) -> FixedTime {
    let mut world = World::new();
    world.init_resource::<FixedTime>();
    {
        let mut fixed = world.resource_mut::<FixedTime>();
        fixed.hz = hz;
        fixed.max_substeps = max_substeps;
    }
    world.resource::<FixedTime>().clone()
}

fn manual_app(step: Duration) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(step));
    app
}

// ---- LoopMode names ----

#[test]
fn loop_mode_from_name_covers_every_protocol_name() {
    assert_eq!(
        LoopMode::from_name("continuous", None),
        Some(LoopMode::Continuous)
    );
    assert_eq!(LoopMode::from_name("bench", None), Some(LoopMode::Bench));
    assert_eq!(LoopMode::from_name("lazy", None), Some(LoopMode::Lazy));
    assert_eq!(
        LoopMode::from_name("run_frames", Some(7)),
        Some(LoopMode::RunFrames(7))
    );
    assert_eq!(
        LoopMode::from_name("run_n_then_freeze", Some(3)),
        Some(LoopMode::RunFrames(3))
    );
    // `run_frames` without a frame count, or with zero frames, is not a mode (ADR-1340's audit:
    // at least one frame).
    assert_eq!(LoopMode::from_name("run_frames", None), None);
    assert_eq!(LoopMode::from_name("run_n_then_freeze", Some(0)), None);
    // Other modes ignore a stray frame count.
    assert_eq!(LoopMode::from_name("bench", Some(9)), Some(LoopMode::Bench));
    // Names are exact: no trimming, no case folding, no empty name.
    for bad in [
        "",
        "Bench",
        " bench",
        "bench ",
        "continuous\n",
        "run-frames",
        "freeze",
    ] {
        assert_eq!(LoopMode::from_name(bad, Some(1)), None, "{bad:?}");
    }
}

#[test]
fn loop_mode_name_round_trips_through_from_name() {
    for mode in [
        LoopMode::Continuous,
        LoopMode::Bench,
        LoopMode::Lazy,
        LoopMode::RunFrames(12),
    ] {
        let frames = match mode {
            LoopMode::RunFrames(n) => Some(n),
            _ => None,
        };
        assert_eq!(LoopMode::from_name(mode.name(), frames), Some(mode));
    }
}

// ---- LoopControl ----

#[test]
fn loop_control_defaults_and_per_mode_predicates() {
    let mut control = LoopControl::default();
    assert_eq!(control.mode(), LoopMode::Continuous);
    assert_eq!(control.throttled_fps, DEFAULT_THROTTLED_FPS);
    assert_eq!(control.frames_in_mode(), 0);
    assert!(!control.is_frozen());
    assert!(control.accepts_input());
    assert!(control.paces());

    control.set_mode(LoopMode::Bench);
    assert!(!control.accepts_input(), "bench ignores input");
    assert!(!control.paces(), "bench runs uncapped");

    control.set_mode(LoopMode::Lazy);
    assert!(control.accepts_input());
    assert!(control.paces());
}

#[test]
fn throttle_period_is_clamped_and_falls_back_on_bad_rates() {
    let period = |fps: f32| {
        let mut world = World::new();
        LoopSettings::default().throttled_fps(fps).apply(&mut world);
        world.resource::<LoopControl>().throttle_period()
    };
    assert_eq!(period(10.0), Duration::from_millis(100));
    assert_eq!(period(1.0), Duration::from_secs(1));
    // Above 1 kHz the period floors at 1 ms; below 1 Hz it caps at 1 s.
    assert_eq!(period(1_000.0), Duration::from_millis(1));
    assert_eq!(period(1_000_000.0), Duration::from_millis(1));
    assert_eq!(period(0.5), Duration::from_secs(1));
    // Zero, negative and non-finite rates use the default rate.
    for bad in [0.0, -3.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert_eq!(period(bad), Duration::from_millis(100), "{bad}");
    }
}

#[test]
fn redraw_request_is_taken_once_and_survives_mode_changes() {
    let mut control = LoopControl::default();
    assert!(!control.take_redraw_request());
    control.request_redraw();
    control.request_redraw();
    control.set_mode(LoopMode::Lazy);
    assert!(control.take_redraw_request());
    assert!(!control.take_redraw_request());
}

// ---- LoopSettings ----

#[test]
fn settings_read_on_an_empty_world_gives_the_defaults() {
    let world = World::new();
    let settings = LoopSettings::read(&world);
    assert_eq!(settings, LoopSettings::default());
    assert_eq!(settings.mode, LoopMode::Continuous);
    assert_eq!(settings.max_fps, None);
    assert_eq!(settings.fixed_hz, 60.0);
    assert_eq!(settings.max_substeps, 5);
    assert_eq!(settings.throttled_fps, DEFAULT_THROTTLED_FPS);
}

#[test]
fn settings_apply_inserts_missing_resources() {
    let mut world = World::new();
    LoopSettings::default().max_fps(Some(45.0)).apply(&mut world);
    assert!(world.contains_resource::<FramePacing>());
    assert!(world.contains_resource::<FixedTime>());
    assert!(world.contains_resource::<LoopControl>());
    assert_eq!(LoopSettings::read(&world).max_fps, Some(45.0));
}

#[test]
fn settings_apply_rejects_invalid_values() {
    let mut app = App::new();
    for bad in [Some(0.0), Some(-30.0), Some(f32::NAN), Some(f32::INFINITY), None] {
        LoopSettings::default().max_fps(bad).apply(app.world_mut());
        assert_eq!(LoopSettings::read(app.world()).max_fps, None, "{bad:?}");
    }
    LoopSettings::default()
        .fixed_hz(0.25)
        .max_substeps(0)
        .apply(app.world_mut());
    let read = LoopSettings::read(app.world());
    assert_eq!(read.fixed_hz, 1.0, "sub-1 Hz clamps to 1 Hz");
    assert_eq!(read.max_substeps, 1, "zero substeps would never step");

    LoopSettings::default().fixed_hz(-60.0).apply(app.world_mut());
    assert_eq!(LoopSettings::read(app.world()).fixed_hz, 1.0);
    LoopSettings::default().fixed_hz(f64::NAN).apply(app.world_mut());
    assert!(LoopSettings::read(app.world()).fixed_hz >= 1.0);
}

#[test]
fn settings_apply_restarts_the_frame_count_only_when_the_mode_changes() {
    let mut app = manual_app(MS16);
    LoopSettings::default()
        .run_n_then_freeze(100)
        .apply(app.world_mut());
    for _ in 0..3 {
        app.update();
    }
    assert_eq!(app.world().resource::<LoopControl>().frames_in_mode(), 3);

    // Same mode again (only another knob changes): the count keeps going.
    LoopSettings::read(app.world())
        .throttled_fps(20.0)
        .apply(app.world_mut());
    assert_eq!(app.world().resource::<LoopControl>().frames_in_mode(), 3);
    assert_eq!(app.world().resource::<LoopControl>().throttled_fps, 20.0);

    // A different mode restarts it.
    LoopSettings::read(app.world())
        .mode(LoopMode::Lazy)
        .apply(app.world_mut());
    assert_eq!(app.world().resource::<LoopControl>().frames_in_mode(), 0);
}

#[test]
fn settings_builders_set_their_fields() {
    let s = LoopSettings::default().bench();
    assert_eq!(s.mode, LoopMode::Bench);
    let s = LoopSettings::default().lazy();
    assert_eq!(s.mode, LoopMode::Lazy);
    let s = LoopSettings::default().run_n_then_freeze(4);
    assert_eq!(s.mode, LoopMode::RunFrames(4));
    let s = LoopSettings::default()
        .throttled_fps(2.5)
        .max_substeps(9)
        .fixed_hz(30.0);
    assert_eq!((s.throttled_fps, s.max_substeps, s.fixed_hz), (2.5, 9, 30.0));
}

// ---- RunFrames freeze through the frame loop ----

#[test]
fn run_frames_freezes_gameplay_time_and_reports_frozen_to_the_pacer() {
    let mut app = manual_app(MS16);
    LoopSettings::default()
        .run_n_then_freeze(3)
        .apply(app.world_mut());
    for _ in 0..3 {
        app.update();
    }
    assert!(app.world().resource::<LoopControl>().is_frozen());
    assert!(!app.world().resource::<LoopControl>().paces());
    assert!(app.world().resource::<Time>().paused);
    let elapsed = app.world().resource::<Time>().elapsed();
    // Further frames still run (the window stays responsive) but gameplay time is frozen.
    for _ in 0..4 {
        app.update();
    }
    assert_eq!(app.world().resource::<Time>().elapsed(), elapsed);
    assert_eq!(app.world().resource::<Time>().frame_count(), 7);
    assert_eq!(app.world().resource::<LoopControl>().frames_in_mode(), 3);
}

#[test]
fn leaving_run_frames_thaws_time() {
    let mut app = manual_app(MS16);
    LoopSettings::default()
        .run_n_then_freeze(1)
        .apply(app.world_mut());
    app.update();
    assert!(app.world().resource::<LoopControl>().is_frozen());
    app.world_mut()
        .resource_mut::<LoopControl>()
        .set_mode(LoopMode::Continuous);
    app.update();
    assert!(!app.world().resource::<LoopControl>().is_frozen());
    assert!(!app.world().resource::<Time>().paused);
    let before = app.world().resource::<Time>().elapsed();
    app.update();
    assert_eq!(app.world().resource::<Time>().elapsed(), before + MS16);
}

#[test]
fn bench_mode_pace_frame_never_waits_even_with_a_cap() {
    let mut app = manual_app(MS16);
    app.insert_resource(FramePacing::new(Some(0.5)));
    LoopSettings::read(app.world()).bench().apply(app.world_mut());
    app.update();
    // A 0.5 fps cap would wait 2 s; bench returns at once (the first call never waits either way).
    assert_eq!(app.pace_frame(), Duration::ZERO);
    app.update();
    assert_eq!(app.pace_frame(), Duration::ZERO);
}

// ---- Time ----

#[test]
fn paused_and_suspended_frames_count_but_do_not_advance_gameplay_time() {
    let mut app = manual_app(MS16);
    app.update();
    assert_eq!(app.world().resource::<Time>().elapsed(), MS16);

    app.world_mut().resource_mut::<Time>().paused = true;
    app.update();
    assert_eq!(app.world().resource::<Time>().delta(), Duration::ZERO);
    assert_eq!(app.world().resource::<Time>().elapsed(), MS16);

    app.world_mut().resource_mut::<Time>().paused = false;
    app.world_mut().resource_mut::<Time>().set_suspended(true);
    assert!(app.world().resource::<Time>().is_suspended());
    app.update();
    assert_eq!(app.world().resource::<Time>().delta_secs(), 0.0);
    app.world_mut().resource_mut::<Time>().set_suspended(false);
    app.update();
    assert_eq!(app.world().resource::<Time>().elapsed(), MS16 * 2);
    assert_eq!(app.world().resource::<Time>().frame_count(), 4);
    // Real time never paused.
    assert_eq!(app.world().resource::<RealTime>().elapsed(), MS16 * 4);
}

#[test]
fn negative_and_nan_time_scale_stop_gameplay_time() {
    let mut time = Time::default();
    for scale in [-1.0, -1e9, f32::NAN, 0.0] {
        time.time_scale = scale;
        time.advance(MS16);
        assert_eq!(time.delta(), Duration::ZERO, "{scale}");
    }
    assert_eq!(time.elapsed(), Duration::ZERO);
    assert_eq!(time.frame_count(), 4);
}

#[test]
fn time_scale_scales_the_delta_exactly_for_representable_factors() {
    let mut time = Time::default();
    for (scale, expected_ms) in [(0.5, 10), (2.0, 40)] {
        time.time_scale = scale;
        time.advance(Duration::from_millis(20));
        assert_eq!(time.delta(), Duration::from_millis(expected_ms), "{scale}");
    }
    assert_eq!(time.elapsed(), Duration::from_millis(50));
    assert_eq!(time.elapsed_secs(), 0.05);
}

#[test]
fn manual_time_is_exact_and_unclamped_for_both_clocks() {
    // A manual step above MAX_DELTA is delivered as is: tests and agents choose their own step.
    let step = MAX_DELTA * 4;
    let mut app = manual_app(step);
    app.update();
    app.update();
    let real = app.world().resource::<RealTime>();
    assert_eq!(real.delta(), step);
    assert_eq!(real.raw_delta(), step);
    assert_eq!(real.elapsed(), step * 2);
    assert_eq!(app.world().resource::<Time>().delta(), step);
}

#[test]
fn real_time_reset_clock_zeroes_the_next_delta() {
    let mut real = RealTime::default();
    let t0 = std::time::Instant::now();
    real.tick(t0);
    real.tick(t0 + MS16);
    assert_eq!(real.delta(), MS16);
    real.reset_clock();
    real.tick(t0 + Duration::from_secs(60));
    assert_eq!(real.delta(), Duration::ZERO);
    assert_eq!(real.raw_delta(), Duration::ZERO);
    assert_eq!(real.elapsed(), MS16);
    // A clock that moves backwards saturates to zero instead of panicking.
    real.tick(t0);
    assert_eq!(real.delta(), Duration::ZERO);
}

#[test]
fn real_time_clamps_the_delta_but_keeps_the_raw_stall() {
    let mut real = RealTime::default();
    let t0 = std::time::Instant::now();
    real.tick(t0);
    real.tick(t0 + Duration::from_secs(5));
    assert_eq!(real.delta(), MAX_DELTA);
    assert_eq!(real.raw_delta(), Duration::from_secs(5));
    assert_eq!(real.elapsed(), MAX_DELTA);
}

#[test]
fn time_update_strategy_defaults_to_automatic() {
    assert_eq!(TimeUpdateStrategy::default(), TimeUpdateStrategy::Automatic);
}

// ---- FixedTime ----

#[test]
fn fixed_step_length_follows_hz_and_clamps_below_one_hz() {
    let at = |hz: f64| fixed(hz, 5);
    assert_eq!(at(1.0).step(), Duration::from_secs(1));
    assert_eq!(at(0.1).step(), Duration::from_secs(1));
    assert_eq!(at(0.0).step(), Duration::from_secs(1));
    assert_eq!(at(-10.0).step(), Duration::from_secs(1));
    // A non-finite rate counts as 60 Hz (ADR-1340).
    assert_eq!(at(f64::NAN).step(), Duration::from_secs_f64(1.0 / 60.0));
    assert_eq!(at(f64::INFINITY).step(), Duration::from_secs_f64(1.0 / 60.0));
    assert_eq!(at(100.0).step(), Duration::from_millis(10));
}

#[test]
fn accumulate_returns_whole_steps_and_keeps_the_remainder() {
    let mut fixed = fixed(100.0, 10);
    assert_eq!(fixed.accumulate(Duration::from_millis(25)), 2);
    assert_eq!(fixed.overstep_fraction(), 0.5);
    assert_eq!(fixed.accumulate(Duration::from_millis(5)), 1);
    assert_eq!(fixed.overstep_fraction(), 0.0);
    assert_eq!(fixed.accumulate(Duration::ZERO), 0);
    assert_eq!(fixed.accumulate(Duration::from_millis(9)), 0);
    assert!((fixed.overstep_fraction() - 0.9).abs() < 1e-5);
}

#[test]
fn accumulate_drops_the_excess_beyond_the_substep_cap() {
    let mut fixed = fixed(100.0, 3);
    // 1 s is 100 steps; only 3 run and the backlog is discarded, not carried.
    assert_eq!(fixed.accumulate(Duration::from_secs(1)), 3);
    assert_eq!(fixed.overstep_fraction(), 0.0);
    assert_eq!(fixed.accumulate(Duration::from_millis(10)), 1);
    // Exactly the cap with nothing left over keeps the (zero) remainder.
    assert_eq!(fixed.accumulate(Duration::from_millis(35)), 3);
    assert!(fixed.overstep_fraction() < 1.0);
}

#[test]
fn zero_substeps_never_step_and_never_hoard_time() {
    let mut fixed = fixed(60.0, 0);
    assert_eq!(fixed.accumulate(Duration::from_secs(1)), 0);
    assert_eq!(fixed.overstep_fraction(), 0.0);
}

#[test]
fn enable_fixed_step_runs_exactly_one_fixed_update_per_frame() {
    #[derive(Resource, Default)]
    struct Fixed(u32);
    for hz in [50.0, 100.0, 200.0, 250.0] {
        let mut app = App::new();
        app.init_resource::<Fixed>();
        enable_fixed_step(&mut app, hz);
        app.add_systems(FixedUpdate, |mut n: ResMut<Fixed>| n.0 += 1);
        for frame in 1..=20u32 {
            app.update();
            assert_eq!(app.world().resource::<Fixed>().0, frame, "{hz} Hz");
            assert_eq!(app.world().resource::<FramePhases>().fixed_steps, 1);
        }
        assert_eq!(app.world().resource::<FixedTime>().max_substeps, 1);
    }
}

#[test]
fn enable_fixed_step_banks_a_half_scale_and_freezes_at_zero() {
    #[derive(Resource, Default)]
    struct Fixed(u32);
    let mut app = App::new();
    app.init_resource::<Fixed>();
    // 1 Hz keeps a half step an exact number of nanoseconds. 60 Hz does not.
    enable_fixed_step(&mut app, 1.0);
    app.world_mut().resource_mut::<Time>().time_scale = 0.5;
    app.add_systems(FixedUpdate, |mut n: ResMut<Fixed>| n.0 += 1);
    let mut seen = Vec::new();
    for _ in 0..4 {
        app.update();
        seen.push(app.world().resource::<Fixed>().0);
    }
    assert_eq!(seen, vec![0, 1, 1, 2]);

    let mut app = App::new();
    app.init_resource::<Fixed>();
    enable_fixed_step(&mut app, 60.0);
    app.world_mut().resource_mut::<Time>().time_scale = 0.0;
    app.add_systems(FixedUpdate, |mut n: ResMut<Fixed>| n.0 += 1);
    for _ in 0..4 {
        app.update();
        assert_eq!(app.world().resource::<Fixed>().0, 0);
    }
}

#[test]
fn enable_fixed_step_clamps_a_sub_one_hz_request() {
    let mut app = App::new();
    enable_fixed_step(&mut app, 0.0);
    assert_eq!(app.world().resource::<FixedTime>().hz, 1.0);
    assert_eq!(
        *app.world().resource::<TimeUpdateStrategy>(),
        TimeUpdateStrategy::ManualDuration(Duration::from_secs(1))
    );
    // A non-finite rate counts as 60 Hz (ADR-1340).
    enable_fixed_step(&mut app, f64::NAN);
    assert_eq!(app.world().resource::<FixedTime>().hz, 60.0);
}

#[test]
fn frame_phases_report_the_fixed_steps_of_a_long_frame() {
    // 50 ms at 100 Hz is five steps, which is the default cap.
    let mut app = manual_app(Duration::from_millis(50));
    LoopSettings::default().fixed_hz(100.0).apply(app.world_mut());
    app.update();
    let phases = *app.world().resource::<FramePhases>();
    assert_eq!(phases.fixed_steps, 5);
    assert_eq!(phases.frame, 1);
    app.update();
    assert_eq!(app.world().resource::<FramePhases>().frame, 2);
    assert!(phases.total_ms() >= phases.get(Phase::Update));
}

// ---- FramePacing ----

#[test]
fn pacing_period_is_none_without_a_valid_cap() {
    for fps in [None, Some(0.0), Some(-1.0), Some(f32::NAN), Some(f32::INFINITY)] {
        assert_eq!(FramePacing::new(fps).period(), None, "{fps:?}");
    }
    assert_eq!(
        FramePacing::new(Some(100.0)).period(),
        Some(Duration::from_millis(10))
    );
    assert_eq!(FramePacing::default().target_fps, None);
}

#[test]
fn pacing_without_a_period_returns_at_once_and_forgets_its_deadline() {
    let mut pacing = FramePacing::new(None);
    assert_eq!(pacing.wait(), Duration::ZERO);
    assert_eq!(pacing.wait_for(None), Duration::ZERO);
    // The first wait with a period only records the frame start.
    assert_eq!(pacing.wait_for(Some(Duration::from_secs(3600))), Duration::ZERO);
    // Passing `None` resets, so the next timed wait is a first wait again (no hour-long sleep).
    assert_eq!(pacing.wait_for(None), Duration::ZERO);
    assert_eq!(pacing.wait_for(Some(Duration::from_secs(3600))), Duration::ZERO);
}

#[test]
fn pacing_with_a_zero_period_never_waits() {
    let mut pacing = FramePacing::new(None);
    for _ in 0..3 {
        assert_eq!(pacing.wait_for(Some(Duration::ZERO)), Duration::ZERO);
    }
}

#[test]
fn pace_frame_without_a_pacing_resource_is_a_no_op() {
    let mut app = App::new();
    let _removed = app.world_mut().remove_resource::<FramePacing>();
    assert_eq!(app.pace_frame(), Duration::ZERO);
}

// ---- edge cases (grok review 11-02) ----

#[test]
fn set_mode_run_frames_while_frozen_runs_the_requested_frames() {
    let mut app = manual_app(MS16);
    LoopSettings::default()
        .run_n_then_freeze(2)
        .apply(app.world_mut());
    for _ in 0..2 {
        app.update();
    }
    assert!(app.world().resource::<LoopControl>().is_frozen());
    app.world_mut()
        .resource_mut::<LoopControl>()
        .set_mode(LoopMode::RunFrames(1));
    // The frame the request is made in ran paused: it thaws, it does not count.
    app.update();
    assert!(!app.world().resource::<LoopControl>().is_frozen());
    assert!(!app.world().resource::<Time>().paused);
    assert_eq!(app.world().resource::<LoopControl>().frames_in_mode(), 0);
    let before = app.world().resource::<Time>().elapsed();
    // The one requested frame advances gameplay time, then the loop freezes again.
    app.update();
    assert_eq!(app.world().resource::<Time>().elapsed(), before + MS16);
    assert!(app.world().resource::<LoopControl>().is_frozen());
    assert!(app.world().resource::<Time>().paused);
}

#[test]
fn run_frames_zero_is_one_frame() {
    let mut control = LoopControl::default();
    control.set_mode(LoopMode::RunFrames(0));
    assert_eq!(control.mode(), LoopMode::RunFrames(1));
    let mut app = manual_app(MS16);
    LoopSettings::default()
        .mode(LoopMode::RunFrames(0))
        .apply(app.world_mut());
    assert_eq!(LoopSettings::read(app.world()).mode, LoopMode::RunFrames(1));
    // Applying the same settings again must not restart the count.
    app.update();
    LoopSettings::default()
        .mode(LoopMode::RunFrames(0))
        .apply(app.world_mut());
    assert_eq!(app.world().resource::<LoopControl>().frames_in_mode(), 1);
    assert!(app.world().resource::<LoopControl>().is_frozen());
}

#[test]
fn fixed_clock_continues_from_the_last_step_after_the_substep_cap_discards_time() {
    // Finding 1 of the app-correctness audit (Grok, integ/v040): a frame longer than
    // `max_substeps` steps discards the backlog, and the next frame must not treat the discarded
    // interval as consumed by the fixed clock. 100 ms at 100 Hz with a 3-step cap runs three
    // steps (10/20/30 ms); the next 10 ms frame's step continues at 40 ms, not at 110 ms.
    #[derive(Resource, Default)]
    struct Seen(Vec<(Duration, Duration)>);
    let mut app = App::new();
    app.init_resource::<Seen>();
    LoopSettings::default()
        .fixed_hz(100.0)
        .max_substeps(3)
        .apply(app.world_mut());
    app.add_systems(FixedUpdate, |time: Res<Time>, mut seen: ResMut<Seen>| {
        seen.0.push((time.delta(), time.elapsed()));
    });
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(100)));
    app.update();
    assert_eq!(
        app.world().resource::<Seen>().0,
        [
            (Duration::from_millis(10), Duration::from_millis(10)),
            (Duration::from_millis(10), Duration::from_millis(20)),
            (Duration::from_millis(10), Duration::from_millis(30)),
        ],
        "the capped frame runs three steps and drops the 70 ms backlog"
    );
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(10)));
    app.update();
    assert_eq!(
        app.world().resource::<Seen>().0.last().copied(),
        Some((Duration::from_millis(10), Duration::from_millis(40))),
        "the fixed clock continues where the last run step ended, not 110 ms ahead"
    );
}

#[test]
fn an_unpause_while_frozen_simulates_nothing() {
    // Finding 2 of the app-correctness audit: clearing `Time::paused` after the freeze must not
    // leak the frame's gameplay time or a fixed step; the freeze pause is restored before the
    // clocks advance. 20 ms frames are longer than the default 60 Hz step, so an unpause that
    // reached `Time::advance` would both advance `elapsed` and run a `FixedUpdate`.
    let mut app = manual_app(Duration::from_millis(20));
    LoopSettings::default()
        .run_n_then_freeze(1)
        .apply(app.world_mut());
    app.update();
    assert!(app.world().resource::<Time>().paused);
    #[derive(Resource, Default)]
    struct FixedRuns(u32);
    app.init_resource::<FixedRuns>();
    app.add_systems(FixedUpdate, |mut runs: ResMut<FixedRuns>| runs.0 += 1);
    let elapsed = app.world().resource::<Time>().elapsed();
    let runs = app.world().resource::<FixedRuns>().0;
    app.world_mut().resource_mut::<Time>().paused = false;
    app.update();
    assert!(
        app.world().resource::<Time>().paused,
        "the freeze owns the pause again"
    );
    assert_eq!(
        app.world().resource::<Time>().elapsed(),
        elapsed,
        "the unpause frame advances no gameplay time"
    );
    assert_eq!(
        app.world().resource::<FixedRuns>().0,
        runs,
        "the unpause frame runs no fixed step"
    );
    app.update();
    assert_eq!(app.world().resource::<Time>().elapsed(), elapsed);
    // Thawing still unpauses.
    app.world_mut()
        .resource_mut::<LoopControl>()
        .set_mode(LoopMode::Continuous);
    app.update();
    assert!(!app.world().resource::<Time>().paused);
}

#[test]
fn applied_rates_are_the_ones_the_pacer_honors() {
    let mut app = App::new();
    LoopSettings::default().max_fps(Some(0.1)).apply(app.world_mut());
    assert_eq!(LoopSettings::read(app.world()).max_fps, Some(1.0));
    LoopSettings::default()
        .max_fps(Some(100_000.0))
        .apply(app.world_mut());
    assert_eq!(LoopSettings::read(app.world()).max_fps, Some(10_000.0));
    for (given, stored) in [
        (-5.0, DEFAULT_THROTTLED_FPS),
        (f32::NAN, DEFAULT_THROTTLED_FPS),
        (100_000.0, 1000.0),
        (0.2, 1.0),
    ] {
        LoopSettings::default()
            .throttled_fps(given)
            .apply(app.world_mut());
        assert_eq!(LoopSettings::read(app.world()).throttled_fps, stored, "{given}");
    }
}

#[test]
fn a_direct_hz_change_drops_the_bank_and_stores_the_honored_rate() {
    let mut f = FixedTime::default(); // 60 Hz
    assert_eq!(f.accumulate(Duration::from_millis(10)), 0);
    f.hz = 200.0; // 5 ms steps: the 10 ms banked at 60 Hz must not become two steps
    assert_eq!(
        f.overstep_fraction(),
        0.0,
        "a bank from another rate is not a fraction of the new step"
    );
    assert_eq!(f.accumulate(Duration::ZERO), 0);
    let mut f = FixedTime::default();
    f.hz = 0.5;
    f.accumulate(Duration::ZERO);
    assert_eq!(f.hz, 1.0);
    f.hz = f64::NAN;
    f.accumulate(Duration::ZERO);
    assert_eq!(f.hz, 60.0);
}

#[test]
fn fixed_update_elapsed_never_passes_the_frame_clock_after_a_rewind() {
    let mut app = manual_app(Duration::from_millis(9));
    app.world_mut().init_resource::<FixedTime>();
    app.update(); // banks 9 ms at 60 Hz
    app.world_mut()
        .resource_mut::<Time>()
        .set_clocks(Duration::ZERO, Duration::ZERO);
    app.world_mut().resource_mut::<FixedTime>().max_substeps = 5;
    #[derive(Resource, Default)]
    struct Seen(Vec<(Duration, Duration)>);
    app.init_resource::<Seen>();
    app.add_systems(FixedUpdate, |time: Res<Time>, mut seen: ResMut<Seen>| {
        seen.0.push((time.elapsed(), time.sim_elapsed()))
    });
    app.update();
    let seen = &app.world().resource::<Seen>().0;
    assert!(!seen.is_empty(), "the bank plus 9 ms makes a step");
    let frame = app.world().resource::<Time>().sim_elapsed();
    for (elapsed, _) in seen {
        assert!(*elapsed <= frame, "{elapsed:?} past the frame clock {frame:?}");
    }
}
