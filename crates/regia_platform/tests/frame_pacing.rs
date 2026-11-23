// SPDX-License-Identifier: MPL-2.0
//! The Choreographer pacing model on virtual time (N12, ADR-1942): the refresh estimate, the swap
//! interval, slot timing, and the fallbacks that keep a frame from blocking on missing ticks.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regia_platform::frame_pacing::{
    DisplayPacer, FramePacingReport, IntervalStep, MAX_SWAP_INTERVAL, PacingCost, PacingHint, PacingMode,
    PacingModel, PacingSim, PacingSource, PacingStats, VsyncTicks, simulate,
};

const MS: u64 = 1_000_000;

fn sim(refresh_hz: f32, frame_ms: &[f32], frames: u32) -> PacingSim {
    PacingSim {
        refresh_hz,
        frame_ms: frame_ms.to_vec(),
        frames,
        cap_fps: None,
        interval: None,
        refresh_switch: None,
        follow_hint: false,
        swapchain: None,
        cost: PacingCost::Work,
        stretch: 0.0,
        // These runs pin the ADR-1942 hint, the slot's own rate.
        hint: PacingHint::Slot,
    }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.05
}

#[test]
fn the_period_comes_from_the_ticks() {
    for hz in [60.0_f64, 90.0, 120.0, 144.0] {
        let period = (1e9 / hz) as u64;
        let mut model = PacingModel::new();
        assert_eq!(model.period_ns(), None);
        for i in 1..=5 {
            model.on_tick(i * period);
        }
        assert_eq!(model.period_ns(), Some(period), "{hz}");
    }
}

#[test]
fn the_period_survives_jitter_and_skipped_ticks() {
    let period = 16_666_666;
    let mut model = PacingModel::new();
    let mut time = period;
    for i in 0..40u64 {
        time += period;
        // Half the ticks are skipped, and every tick lands up to 200 µs off.
        let jitter = (i * 37 % 5) * 50_000;
        if i % 2 == 0 {
            model.on_tick(time + jitter);
        }
    }
    let estimate = model.period_ns().expect("a period") as f64;
    // Skipping half the ticks in a regular pattern shows only doubled intervals: the estimate
    // sees 33 ms. An irregular skip keeps single intervals in the lower quartile.
    assert!((estimate / 33_333_332.0 - 1.0).abs() < 0.02, "{estimate}");
    let mut model = PacingModel::new();
    let mut time = period;
    for i in 0..40u64 {
        time += period;
        if !i.is_multiple_of(3) {
            model.on_tick(time);
        }
    }
    assert_eq!(model.period_ns(), Some(period), "one tick in three skipped");
}

#[test]
fn repeated_and_older_ticks_teach_nothing_and_gaps_do_not_count() {
    let mut model = PacingModel::new();
    for i in 1..=8 {
        model.on_tick(i * 16 * MS);
    }
    assert_eq!(model.period_ns(), Some(16 * MS));
    model.on_tick(8 * 16 * MS);
    model.on_tick(16 * MS);
    assert_eq!(model.period_ns(), Some(16 * MS));
    model.on_tick(10_000 * MS);
    assert_eq!(model.period_ns(), Some(16 * MS), "a gap is not a refresh rate");
}

#[test]
fn light_frames_run_at_the_panel_rate() {
    let report = simulate(&sim(60.0, &[8.0], 600)).expect("runs");
    assert_eq!(report.swap_interval, 1);
    assert!(report.steps.is_empty(), "{report:?}");
    assert_eq!(report.stats.late, 0);
    assert_eq!(report.stats.frames, 600);
    assert!(near(report.tail_min_gap_ms, 16.66), "{report:?}");
    assert!(near(report.tail_max_gap_ms, 16.66), "{report:?}");
}

#[test]
fn a_frame_over_the_slot_settles_on_even_frames() {
    let report = simulate(&sim(60.0, &[20.0], 600)).expect("runs");
    assert_eq!(report.swap_interval, 2, "{report:?}");
    assert_eq!(
        report.steps,
        [IntervalStep {
            frame: 3,
            interval: 2
        }],
        "four frames over, then it stays"
    );
    assert_eq!(report.tail_late, 0, "{report:?}");
    assert!(near(report.tail_min_gap_ms, 33.33), "{report:?}");
    assert!(near(report.tail_max_gap_ms, 33.33), "even frames: {report:?}");
}

#[test]
fn a_faster_panel_gets_a_closer_interval() {
    let report = simulate(&sim(120.0, &[20.0], 600)).expect("runs");
    assert_eq!(report.swap_interval, 3, "25 ms: {report:?}");
    assert!(near(report.tail_max_gap_ms, 25.0), "{report:?}");
    let report = simulate(&sim(90.0, &[20.0], 600)).expect("runs");
    assert_eq!(report.swap_interval, 2, "22.2 ms: {report:?}");
}

#[test]
fn a_much_slower_frame_jumps_to_where_it_fits() {
    let report = simulate(&sim(60.0, &[40.0], 300)).expect("runs");
    assert_eq!(report.swap_interval, 3, "{report:?}");
    assert_eq!(
        report.steps.len(),
        1,
        "one jump, not one step at a time: {report:?}"
    );
}

#[test]
fn the_interval_stops_at_twenty_fps() {
    let report = simulate(&sim(60.0, &[70.0], 300)).expect("runs");
    assert_eq!(report.swap_interval, 3, "{report:?}");
    assert!(
        report.tail_late > 0,
        "the frames still run late, and are counted: {report:?}"
    );
    let report = simulate(&sim(120.0, &[200.0], 300)).expect("runs");
    assert_eq!(report.swap_interval, 6, "{report:?}");
}

#[test]
fn the_slot_ceiling_follows_twenty_fps_on_any_panel() {
    // 240 Hz: a 40 ms frame fits the 50 ms slot (12 vsyncs), so it must not stay capped at 8.
    assert_eq!(PacingModel::slot_ceiling(4_166_666), 12);
    assert_eq!(PacingModel::slot_ceiling(16_666_666), 3);
    // 90 Hz: 5 vsyncs, the fewest that hold a 50 ms frame.
    assert_eq!(PacingModel::slot_ceiling(11_111_111), 5);
    assert_eq!(PacingModel::slot_ceiling(u64::MAX), 1);
    let report = simulate(&sim(240.0, &[40.0], 600)).expect("runs");
    assert_eq!(report.swap_interval, 10, "40 ms on 4.17 ms vsyncs: {report:?}");
    assert_eq!(report.tail_late, 0, "{report:?}");
}

#[test]
fn a_rare_spike_leaves_the_interval_alone() {
    let mut costs = vec![8.0; 7];
    costs.push(40.0);
    let report = simulate(&sim(60.0, &costs, 800)).expect("runs");
    assert_eq!(report.swap_interval, 1, "{report:?}");
    assert!(report.steps.is_empty(), "{report:?}");
}

#[test]
fn a_cap_is_a_whole_number_of_vsyncs_and_never_exceeded() {
    for (hz, cap, interval) in [
        (60.0, 30.0, 2),
        (90.0, 30.0, 3),
        (120.0, 30.0, 4),
        (60.0, 45.0, 2),
        (120.0, 45.0, 3),
        (60.2, 60.0, 1),
        (60.0, 90.0, 1),
    ] {
        let report = simulate(&PacingSim {
            cap_fps: Some(cap),
            ..sim(hz, &[2.0], 300)
        })
        .expect("runs");
        assert_eq!(report.swap_interval, interval, "{hz} Hz at {cap}: {report:?}");
        assert!(1000.0 / report.tail_min_gap_ms <= cap + 0.5, "{report:?}");
    }
}

#[test]
fn the_interval_drops_only_after_the_frames_have_fit_for_a_while() {
    let mut costs = vec![20.0; 300];
    costs.extend(std::iter::repeat_n(8.0, 300));
    let report = simulate(&sim(60.0, &costs, 600)).expect("runs");
    assert_eq!(report.swap_interval, 1, "{report:?}");
    assert_eq!(report.steps.len(), 2, "{report:?}");
    let drop = report.steps[1];
    assert_eq!(drop.interval, 1);
    assert!((359..=361).contains(&drop.frame), "the 60-frame hold: {drop:?}");
}

#[test]
fn a_bounce_doubles_the_wait_so_the_interval_does_not_flap() {
    // A scene that alternates: long enough light stretches to drop, then heavy again.
    let mut costs = vec![8.0; 70];
    costs.extend(std::iter::repeat_n(20.0, 10));
    let flapping = simulate(&sim(60.0, &costs, 2400)).expect("runs");
    // Without the back-off every 80-frame cycle would change twice: 60 changes.
    assert!(
        flapping.steps.len() <= 8,
        "{} changes: {flapping:?}",
        flapping.steps.len()
    );
}

#[test]
fn a_refresh_change_keeps_the_slot() {
    let report = simulate(&PacingSim {
        refresh_switch: Some((200, 120.0)),
        ..sim(60.0, &[20.0], 900)
    })
    .expect("runs");
    assert_eq!(report.stats.refresh_changes, 1, "{report:?}");
    assert!(near(report.period_ms, 8.33), "{report:?}");
    // 2 vsyncs of 16.7 ms is 4 of 8.3 ms; then 3 hold the 20 ms frames.
    assert_eq!(report.swap_interval, 3, "{report:?}");
    assert!(report.steps.iter().any(|s| s.interval == 3), "{report:?}");
    assert!(near(report.tail_max_gap_ms, 25.0), "{report:?}");

    // 3 vsyncs of 60 Hz is 50 ms. At 240 Hz that slot is 12 vsyncs, not the forced-interval cap of 8.
    let switched = simulate(&PacingSim {
        refresh_switch: Some((200, 240.0)),
        ..sim(60.0, &[70.0], 400)
    })
    .expect("60 to 240");
    assert_eq!(switched.stats.refresh_changes, 1, "{switched:?}");
    assert_eq!(switched.swap_interval, 12, "the 50 ms slot: {switched:?}");
}

#[test]
fn a_fast_panel_holds_a_40_ms_frame_inside_the_20_fps_slot() {
    // 40 ms needs 10 vsyncs of 240 Hz (41.7 ms). The 20 fps limit is 12 vsyncs (50 ms), so the
    // run must be allowed past the forced-interval cap of 8, and the frames then fit.
    let report = simulate(&sim(240.0, &[40.0], 120)).expect("runs");
    assert_eq!(report.swap_interval, 10, "{report:?}");
    assert!(report.swap_interval > MAX_SWAP_INTERVAL, "{report:?}");
    assert_eq!(report.tail_late, 0, "{report:?}");
}

#[test]
fn a_forced_interval_wins_and_is_clamped() {
    let report = simulate(&PacingSim {
        interval: Some(3),
        ..sim(60.0, &[2.0], 120)
    })
    .expect("runs");
    assert_eq!(report.swap_interval, 3);
    assert!(near(report.tail_max_gap_ms, 50.0), "{report:?}");
    let report = simulate(&PacingSim {
        interval: Some(99),
        ..sim(60.0, &[2.0], 60)
    })
    .expect("runs");
    assert_eq!(report.swap_interval, MAX_SWAP_INTERVAL);
}

#[test]
fn a_frame_that_ends_just_after_a_tick_takes_that_tick() {
    let mut model = PacingModel::new();
    for i in 1..=8 {
        model.on_tick(i * 16 * MS);
    }
    model.released(8 * 16 * MS);
    let latest = 9 * 16 * MS;
    model.on_tick(latest);
    assert_eq!(model.plan(latest + MS), Some(latest), "1 ms after the tick");
    let past = model.plan(latest + 3 * MS).expect("a plan");
    assert!(past > latest, "3 ms after the tick waits for the next one");
}

#[test]
fn a_late_frame_is_counted_and_starts_a_new_timeline() {
    let mut model = PacingModel::new();
    for i in 1..=8 {
        model.on_tick(i * 16 * MS);
    }
    model.released(8 * 16 * MS);
    model.released(9 * 16 * MS);
    assert_eq!(model.stats().late, 0, "the next vsync is on time");
    model.released(12 * 16 * MS);
    assert_eq!(model.stats().late, 1, "two vsyncs missed");
    model.released(13 * 16 * MS);
    assert_eq!(model.stats().late, 1, "on time again from the late start");
}

#[test]
fn nonsense_runs_are_refused() {
    assert!(simulate(&sim(0.0, &[8.0], 10)).is_none());
    assert!(simulate(&sim(f32::NAN, &[8.0], 10)).is_none());
    assert!(simulate(&sim(60.0, &[], 10)).is_none());
    assert!(simulate(&sim(60.0, &[-1.0], 10)).is_none());
    // A fuzzed cost must be refused, not walked a tick at a time.
    assert!(simulate(&sim(60.0, &[1e12], 10)).is_none());
    assert!(
        simulate(&sim(240.0, &[10_000.0], 20_000)).is_none(),
        "too many ticks"
    );
    assert!(
        simulate(&PacingSim {
            cap_fps: Some(0.0),
            ..sim(60.0, &[8.0], 10)
        })
        .is_none()
    );
}

#[test]
fn rates_no_display_has_are_refused_before_they_overflow_the_clock() {
    for hz in [1e-30, 0.5, 1000.0, 1e30] {
        assert!(simulate(&sim(hz, &[8.0], 10)).is_none(), "{hz}");
        let switched = PacingSim {
            refresh_switch: Some((2, hz)),
            ..sim(60.0, &[8.0], 10)
        };
        assert!(simulate(&switched).is_none(), "switch to {hz}");
    }
}

#[test]
fn a_cap_below_the_lowest_rate_runs_at_the_lowest_rate() {
    // The runner's cap stops at 1 fps (`MIN_PACING_FPS`). Without the same limit here a tiny cap
    // walks billions of ticks inside one frame.
    let run = |cap| {
        simulate(&PacingSim {
            cap_fps: Some(cap),
            ..sim(60.0, &[2.0], 20)
        })
        .expect("runs")
    };
    assert_eq!(run(1.0).swap_interval, 60);
    assert_eq!(run(1e-30).swap_interval, 60);
}

#[test]
fn the_wait_for_a_tick_always_covers_the_slot() {
    let period = 16_666_666;
    let mut model = PacingModel::new();
    assert_eq!(model.wait_limit(), None, "no period yet");
    for i in 1..=8 {
        model.on_tick(i * period);
    }
    // One period of margin past a whole slot: the wait starts after the frame, so it never
    // needs a full slot, and a tick that is merely late is not a stall.
    assert_eq!(
        model.wait_limit(),
        Some(Duration::from_nanos(2 * period)),
        "at one vsync per frame"
    );
    for (cap_fps, forced, interval) in [
        (None, Some(MAX_SWAP_INTERVAL), MAX_SWAP_INTERVAL),
        (Some(5.0_f64), None, 12),
        (Some(1.0), None, 60),
    ] {
        model.end_frame(0, cap_fps.map(|fps| (1e9 / fps) as u64), forced);
        assert_eq!(model.swap_interval(), interval);
        let slot = Duration::from_nanos(u64::from(interval) * period);
        assert!(model.wait_limit().expect("a limit") >= slot, "{cap_fps:?}");
    }
}

fn now_zero() -> u64 {
    0
}

/// Ticks from a test thread until `done`; a real pacer on the other side.
fn feeder(ticks: Arc<VsyncTicks>, done: Arc<AtomicBool>, first_ns: u64) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut time = first_ns;
        while !done.load(Ordering::Relaxed) {
            ticks.tick(time);
            time += 16 * MS;
            std::thread::yield_now();
        }
    })
}

#[test]
fn the_pacer_releases_on_live_ticks() {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick(i * 16 * MS);
    }
    let mut pacer = DisplayPacer::new(ticks.clone(), PacingSource::Choreographer, now_zero);
    let done = Arc::new(AtomicBool::new(false));
    let thread = feeder(ticks.clone(), done.clone(), 9 * 16 * MS);
    for _ in 0..20 {
        assert!(
            pacer
                .pace(Duration::from_millis(1), None, PacingMode::Auto)
                .is_some(),
            "paced on the display"
        );
    }
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");
    let report: FramePacingReport = pacer.report(PacingMode::Auto);
    assert_eq!(report.source, PacingSource::Choreographer);
    assert_eq!(report.stats.frames, 20);
    assert_eq!(report.timeouts, 0);
    assert_eq!(report.swap_interval, 1);
    assert!(
        (report.refresh_hz.expect("rate") - 62.5).abs() < 0.1,
        "{report:?}"
    );
}

#[test]
fn no_ticks_means_the_software_cap_runs_the_frame() {
    let ticks = VsyncTicks::new();
    let mut pacer = DisplayPacer::new(ticks, PacingSource::Choreographer, now_zero);
    assert_eq!(pacer.pace(Duration::ZERO, None, PacingMode::Auto), None);
    let report = pacer.report(PacingMode::Auto);
    assert_eq!(report.source, PacingSource::Software, "no period yet");
    assert_eq!(report.refresh_hz, None);
}

#[test]
fn off_leaves_the_frame_to_the_software_cap_and_keeps_learning() {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick(i * 16 * MS);
    }
    let mut pacer = DisplayPacer::new(ticks, PacingSource::Choreographer, now_zero);
    assert_eq!(pacer.pace(Duration::ZERO, None, PacingMode::Off), None);
    assert_eq!(pacer.report(PacingMode::Off).source, PacingSource::Software);
    assert!(pacer.report(PacingMode::Off).refresh_hz.is_some());
    assert_eq!(pacer.report(PacingMode::Auto).source, PacingSource::Choreographer);
}

#[test]
fn a_frame_the_pacer_skipped_makes_nothing_late() {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick(i * 16 * MS);
    }
    let mut pacer = DisplayPacer::new(ticks.clone(), PacingSource::Choreographer, now_zero);
    assert!(pacer.pace(Duration::ZERO, None, PacingMode::Auto).is_some());
    for i in 9..=60 {
        ticks.tick(i * 16 * MS);
    }
    assert_eq!(pacer.pace(Duration::ZERO, None, PacingMode::Off), None);
    pacer.skip_frame();
    let done = Arc::new(AtomicBool::new(false));
    let thread = feeder(ticks, done.clone(), 61 * 16 * MS);
    assert!(pacer.pace(Duration::ZERO, None, PacingMode::Auto).is_some());
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");
    assert_eq!(
        pacer.report(PacingMode::Auto).stats.late,
        0,
        "{:?}",
        pacer.report(PacingMode::Auto)
    );
}

#[test]
fn stalled_ticks_stop_blocking_and_a_new_tick_brings_pacing_back() {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick(i * 16 * MS);
    }
    let mut pacer = DisplayPacer::new(ticks.clone(), PacingSource::Choreographer, now_zero);
    // The latest tick releases the first frame. Then the display goes off: each frame waits out
    // one timeout, and after three the frames stop waiting.
    for _ in 0..4 {
        assert!(pacer.pace(Duration::ZERO, None, PacingMode::Auto).is_some());
    }
    assert_eq!(pacer.report(PacingMode::Auto).timeouts, 3);
    assert_eq!(
        pacer.pace(Duration::ZERO, None, PacingMode::Auto),
        None,
        "software from here"
    );
    assert_eq!(pacer.report(PacingMode::Auto).source, PacingSource::Software);

    for i in 0..8 {
        ticks.tick((1000 + i * 16) * MS);
    }
    let done = Arc::new(AtomicBool::new(false));
    let thread = feeder(ticks.clone(), done.clone(), 1200 * MS);
    let paced = pacer.pace(Duration::ZERO, None, PacingMode::Auto);
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");
    assert!(paced.is_some(), "the ticks are back");
    assert_eq!(pacer.report(PacingMode::Auto).source, PacingSource::Choreographer);
}

#[test]
fn a_suspend_keeps_the_period_and_drops_the_timeline() {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick(i * 16 * MS);
    }
    let mut pacer = DisplayPacer::new(ticks.clone(), PacingSource::Choreographer, now_zero);
    assert!(pacer.pace(Duration::ZERO, None, PacingMode::Auto).is_some());
    pacer.suspend_ticks();
    pacer.resume_ticks();
    let after = pacer.report(PacingMode::Auto);
    assert!(after.refresh_hz.is_some(), "the panel did not change");
    ticks.tick(5000 * MS);
    let done = Arc::new(AtomicBool::new(false));
    let thread = feeder(ticks.clone(), done.clone(), 5016 * MS);
    let paced = pacer.pace(Duration::ZERO, None, PacingMode::Auto);
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");
    assert!(paced.is_some());
    assert_eq!(
        pacer.report(PacingMode::Auto).stats.late,
        0,
        "the gap of the suspend is not a late frame"
    );
}

#[test]
fn the_compositor_hint_follows_the_interval() {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick(i * 16 * MS);
    }
    let mut pacer = DisplayPacer::new(ticks.clone(), PacingSource::Choreographer, now_zero);
    assert_eq!(pacer.frame_rate_hint(), None, "running at the panel rate");
    let done = Arc::new(AtomicBool::new(false));
    let thread = feeder(ticks, done.clone(), 9 * 16 * MS);
    assert!(
        pacer
            .pace(Duration::ZERO, None, PacingMode::Interval(2))
            .is_some()
    );
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");
    let hint = pacer.frame_rate_hint().expect("a hint");
    assert!((hint - 31.25).abs() < 0.1, "{hint}");
}

/// Ticks at `period` from `start` until `model` has seen `count` of them; returns the last.
fn run_ticks(model: &mut PacingModel, start: u64, period: u64, count: u64) -> u64 {
    let mut tick = start;
    for _ in 0..count {
        tick += period;
        model.on_tick(tick);
    }
    tick
}

#[test]
fn a_panel_that_follows_the_hint_stays_there() {
    // 11 ms frames on a 120 Hz panel run at 60: the interval goes to 2 and the hint to 60. When
    // the panel follows, the interval falls to 1, and the hint must not go with it: clearing it
    // sends the panel back to 120 and the whole thing round again.
    for (hz, frame_ms, want_ms) in [(120.0, 11.0, 16.67), (120.0, 20.0, 25.0), (144.0, 11.0, 13.89)] {
        let report = simulate(&PacingSim {
            follow_hint: true,
            ..sim(hz, &[frame_ms], 2_000)
        })
        .expect("a run");
        assert_eq!(report.stats.refresh_changes, 1, "{hz} {frame_ms}: {report:?}");
        assert!(
            (report.tail_min_gap_ms - want_ms).abs() < 0.1,
            "{hz} {frame_ms}: {report:?}"
        );
        assert!(
            (report.tail_max_gap_ms - want_ms).abs() < 0.1,
            "{hz} {frame_ms}: {report:?}"
        );
        assert_eq!(report.tail_late, 0, "{hz} {frame_ms}");
        let hint = report.frame_rate_hint.expect("the hint stays");
        assert!((1000.0 / hint - want_ms).abs() < 0.1, "{hint}");
    }
}

#[test]
fn a_capped_app_on_a_panel_that_follows_the_hint_stays_there() {
    let report = simulate(&PacingSim {
        cap_fps: Some(30.0),
        follow_hint: true,
        ..sim(120.0, &[5.0], 2_000)
    })
    .expect("a run");
    assert_eq!(report.stats.refresh_changes, 1, "{report:?}");
    assert!(near(report.tail_min_gap_ms, 33.33), "{report:?}");
    assert!(near(report.tail_max_gap_ms, 33.33), "{report:?}");
    assert_eq!(report.frame_rate_hint.map(f32::round), Some(30.0));
}

#[test]
fn a_hint_the_panel_ignores_changes_nothing() {
    let free = simulate(&sim(120.0, &[11.0], 2_000)).expect("a run");
    assert_eq!(free.stats.refresh_changes, 0);
    assert_eq!(free.swap_interval, 2);
    assert_eq!(free.frame_rate_hint.map(f32::round), Some(60.0));
    // Cheap frames on a panel that never moved drop the hint with the interval.
    let cheap = simulate(&sim(120.0, &[4.0], 200)).expect("a run");
    assert_eq!(cheap.frame_rate_hint, None);
}

#[test]
fn cheap_frames_bring_the_panel_back() {
    let (fast, slow) = (8_333_333, 16_666_666);
    let mut model = PacingModel::new();
    let mut tick = run_ticks(&mut model, 0, fast, 8);
    // Heavy frames: the interval rises and the hint asks for 60.
    for _ in 0..4 {
        model.end_frame(11 * MS, None, None);
    }
    assert_eq!(model.swap_interval(), 2);
    assert_eq!(model.frame_rate_hint().map(f32::round), Some(60.0));
    // The panel follows.
    tick = run_ticks(&mut model, tick, slow, 32);
    assert_eq!(model.swap_interval(), 1);
    assert_eq!(model.frame_rate_hint().map(f32::round), Some(60.0));
    // A frame that needs the slow slot holds the hint however long it lasts.
    for _ in 0..5_000 {
        model.end_frame(11 * MS, None, None);
    }
    assert_eq!(model.frame_rate_hint().map(f32::round), Some(60.0));
    // Frames that fit the panel's own rate, with headroom, for long enough, let go of it.
    let mut frames = 0;
    while model.frame_rate_hint().is_some() {
        model.end_frame(5 * MS, None, None);
        frames += 1;
        assert!(frames <= 960, "the hint never let go");
    }
    assert!(frames >= 60, "released too soon: {frames}");
    // The panel returns, and the slot is the short one: no second trip.
    run_ticks(&mut model, tick, fast, 32);
    assert_eq!(model.swap_interval(), 1);
    assert_eq!(model.frame_rate_hint(), None);
}

#[test]
fn a_cap_holds_the_hint_however_cheap_the_frames() {
    let (fast, slow) = (8_333_333, 16_666_666);
    let mut model = PacingModel::new();
    let cap = Some(16_666_666);
    let tick = run_ticks(&mut model, 0, fast, 8);
    model.end_frame(MS, cap, None);
    assert_eq!(model.frame_rate_hint().map(f32::round), Some(60.0));
    run_ticks(&mut model, tick, slow, 32);
    for _ in 0..5_000 {
        model.end_frame(MS, cap, None);
    }
    assert_eq!(model.swap_interval(), 1);
    assert_eq!(model.frame_rate_hint().map(f32::round), Some(60.0));
}

#[test]
fn a_panel_change_that_is_not_ours_is_the_new_panel() {
    let (fast, slow) = (8_333_333, 16_666_666);
    let mut model = PacingModel::new();
    // 60 Hz, no hint: the user or the system moves the panel to 120, and later back.
    let tick = run_ticks(&mut model, 0, slow, 8);
    let tick = run_ticks(&mut model, tick, fast, 32);
    assert_eq!(model.swap_interval(), 2, "the slot is kept");
    for _ in 0..200 {
        model.end_frame(MS, None, None);
    }
    assert_eq!(model.swap_interval(), 1);
    assert_eq!(model.frame_rate_hint(), None);
    run_ticks(&mut model, tick, slow, 32);
    assert_eq!(model.swap_interval(), 1);
    assert_eq!(model.frame_rate_hint(), None, "60 Hz is simply the panel now");
    for _ in 0..2_000 {
        model.end_frame(MS, None, None);
    }
    assert_eq!(model.frame_rate_hint(), None);
}

#[test]
fn the_hint_is_worked_out_again_after_a_pause() {
    let (fast, slow) = (8_333_333, 16_666_666);
    let mut model = PacingModel::new();
    let tick = run_ticks(&mut model, 0, fast, 8);
    for _ in 0..4 {
        model.end_frame(11 * MS, None, None);
    }
    let tick = run_ticks(&mut model, tick, slow, 32);
    assert_eq!(model.frame_rate_hint().map(f32::round), Some(60.0));
    // The new surface has no preference: the panel is back at 120 until the hint arrives again.
    model.restart();
    run_ticks(&mut model, tick + 1_000 * MS, fast, 32);
    assert_eq!(model.swap_interval(), 2, "the slot survives the pause");
    assert_eq!(model.frame_rate_hint().map(f32::round), Some(60.0));
}

#[test]
fn a_forced_interval_counts_panel_vsyncs_on_a_following_panel() {
    // Interval 2 at 120 Hz hints 60. Once the ticks follow, 2 of them would be 30 fps, and that
    // hint would halve them again.
    let report = simulate(&PacingSim {
        interval: Some(2),
        follow_hint: true,
        ..sim(120.0, &[5.0], 500)
    })
    .expect("a run");
    assert_eq!(report.stats.refresh_changes, 1, "{report:?}");
    assert!(near(report.tail_min_gap_ms, 16.67), "{report:?}");
    assert!(near(report.tail_max_gap_ms, 16.67), "{report:?}");
    assert_eq!(report.frame_rate_hint.map(f32::round), Some(60.0));
}

#[test]
fn every_rate_and_cost_settles_with_a_following_panel() {
    for hz in [60.0, 90.0, 120.0, 144.0] {
        for frame_ms in [2.0, 7.0, 9.0, 11.0, 14.0, 18.0, 30.0, 45.0] {
            for cap_fps in [None, Some(30.0), Some(60.0)] {
                let run = |frames| {
                    simulate(&PacingSim {
                        cap_fps,
                        follow_hint: true,
                        ..sim(hz, &[frame_ms], frames)
                    })
                    .expect("a run")
                };
                let case = format!("{hz} Hz, {frame_ms} ms, cap {cap_fps:?}");
                let (report, longer) = (run(1_000), run(8_000));
                // A heavy frame can step down through a few rates at the start. After that
                // nothing moves, however long the run: a bounce would keep adding changes.
                assert_eq!(
                    longer.stats,
                    PacingStats {
                        frames: 8_000,
                        ..report.stats
                    },
                    "{case}"
                );
                assert_eq!(longer.tail_late, 0, "{case}: {longer:?}");
                let spread = longer.tail_max_gap_ms - longer.tail_min_gap_ms;
                assert!(spread < 0.1, "{case}: {longer:?}");
            }
        }
    }
}

/// Audit v033b (Med): `wait_for` with a timeout too large for `Instant` (`Duration::MAX`) returned
/// `None` at once, even with the tick already there; it now waits without a deadline.
#[test]
fn an_unbounded_wait_returns_the_tick() {
    let ticks = VsyncTicks::new();
    ticks.tick(100 * MS);
    assert_eq!(ticks.wait_for(50 * MS, Duration::MAX), Some(100 * MS));
    assert_eq!(ticks.wait_for(50 * MS, Duration::ZERO), Some(100 * MS));
    assert_eq!(ticks.wait_for(200 * MS, Duration::ZERO), None);
}

#[test]
fn a_refresh_switch_to_a_slower_panel_right_after_warmup_terminates() {
    // The switch makes the period (200 ms) larger than the tick clock after warmup (~150 ms).
    let mut run = sim(60.0, &[8.0], 2);
    run.refresh_switch = Some((0, 5.0));
    assert!(simulate(&run).is_some());
}
