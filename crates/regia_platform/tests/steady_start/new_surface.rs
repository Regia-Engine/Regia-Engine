// SPDX-License-Identifier: MPL-2.0
//! What ADR-5946 added to the pacing API: the release mode, the grid phase, the model's grid
//! follow and drain run, and the scheduler stamps of the trace.

use std::cell::Cell;
use std::time::Duration;

use regia_platform::frame_pacing::{
    DEFAULT_DELIVERY_NS, DEFAULT_START_OFFSET_NS, FrameSteadyStart, MAX_START_OFFSET_NS, PacingModel,
    PacingRelease, STEADY_DRAIN_RUN, SchedSample, next_grid_phase, parse_schedstat_run_delay,
};

use super::{MS, PERIOD, US, WORK, intervals, phone, pixel_delivery, run, settings};

#[test]
fn the_old_release_follows_the_callbacks() {
    // `release: tick` is ADR-5461's behavior, kept for comparison: the frame starts when its
    // callback is seen, or 0.6 ms past the usual delivery when the callback is late.
    let mut pacer = phone(pixel_delivery, None, None);
    pacer.set_grid_release(false);
    let on_the_grid_before = pacer.grid_releases();
    let rows = run(&mut pacer, 600, |_| WORK);
    let gaps = intervals(&rows, 0);
    let longest = gaps.iter().copied().max().expect("frames");
    assert!(longest > PERIOD + 400 * US, "longest {longest}");
    assert!(
        rows.iter()
            .all(|(_, row)| row.release != Some(PacingRelease::Grid))
    );
    assert_eq!(pacer.grid_releases(), on_the_grid_before);
    assert!(pacer.polled_releases() > 0 && pacer.predicted_releases() > 0);
}

#[test]
fn grid_releases_are_counted_and_named_in_the_trace() {
    let mut pacer = phone(pixel_delivery, None, None);
    let before = pacer.grid_releases();
    let rows = run(&mut pacer, 100, |_| WORK);
    assert!(
        rows.iter()
            .all(|(_, row)| row.release == Some(PacingRelease::Grid)),
        "every frame was released on the grid"
    );
    assert_eq!(pacer.grid_releases() - before, 100);
    assert_eq!((pacer.polled_releases(), pacer.predicted_releases()), (0, 0));
    // About half the releases come before their tick's callback: those rows have no stamp.
    let unstamped = rows
        .iter()
        .filter(|(_, row)| row.tick_delivered_ns.is_none())
        .count();
    assert!((10..=90).contains(&unstamped), "{unstamped} of 100");
    assert_eq!(PacingRelease::Grid.name(), "grid");
    assert_eq!(
        [
            PacingRelease::Slept,
            PacingRelease::Polled,
            PacingRelease::Predicted
        ]
        .map(PacingRelease::name),
        ["slept", "polled", "predicted"]
    );
}

#[test]
fn the_grid_phase_follows_the_usual_delivery_and_ignores_a_stray_callback() {
    // A fixed point, and a slow walk to a steady delay.
    assert_eq!(next_grid_phase(500 * US, 500 * US, PERIOD), 500 * US);
    let mut phase = DEFAULT_DELIVERY_NS;
    for _ in 0..600 {
        phase = next_grid_phase(phase, 900 * US, PERIOD);
    }
    assert!((880 * US..=900 * US).contains(&phase), "{phase}");
    // One callback 30 ms late moves the phase by microseconds, so the frame starts stay a period apart.
    let moved = next_grid_phase(500 * US, 30 * MS, PERIOD);
    assert!(moved > 500 * US && moved <= 510 * US, "{moved}");
    let back = next_grid_phase(500 * US, 0, PERIOD);
    assert!((490 * US..500 * US).contains(&back), "{back}");
    // Never more than a quarter period: the poll window still holds it.
    let mut phase = PERIOD / 4;
    for _ in 0..100 {
        phase = next_grid_phase(phase, PERIOD, PERIOD);
    }
    assert_eq!(phase, PERIOD / 4);
}

thread_local! {
    static PROBES: Cell<u64> = const { Cell::new(0) };
}

/// A scheduler that keeps the thread off its CPU 10 µs between any two samples and takes the CPU
/// away once in that time.
fn probe() -> Option<SchedSample> {
    let n = PROBES.get() + 1;
    PROBES.set(n);
    Some(SchedSample {
        run_delay_ns: n * 10 * US,
        preemptions: n,
        cpu: 7,
    })
}

#[test]
fn the_trace_says_what_the_scheduler_did_to_the_frame_loop() {
    let mut pacer = phone(pixel_delivery, None, None);
    let rows = run(&mut pacer, 3, |_| WORK);
    assert!(
        rows.iter().all(|(_, row)| {
            (
                row.work_run_delay_ns,
                row.wait_run_delay_ns,
                row.preemptions,
                row.wake_cpu,
            ) == (None, None, None, None)
        }),
        "no probe, no stamps"
    );
    pacer.set_sched_probe(Some(probe));
    let rows = run(&mut pacer, 4, |_| WORK);
    // The first frame after the probe is set has no sample from its start.
    assert_eq!(rows[0].1.work_run_delay_ns, None);
    assert_eq!(rows[0].1.wait_run_delay_ns, Some(10 * US));
    for (_, row) in &rows[1..] {
        // One sample when the frame's work ends and one when the wait returns.
        assert_eq!(row.work_run_delay_ns, Some(10 * US), "{row:?}");
        assert_eq!(row.wait_run_delay_ns, Some(10 * US), "{row:?}");
        assert_eq!(row.preemptions, Some(2), "{row:?}");
        assert_eq!(row.wake_cpu, Some(7), "{row:?}");
    }
    // A frame the pacer does not release has no stamps, and the next one no start sample.
    pacer.skip_frame();
    let skipped = pacer.trace_row(Duration::from_nanos(WORK), Duration::ZERO, None);
    assert_eq!(
        (skipped.release, skipped.wake_cpu, skipped.sleep_late_ns),
        (None, None, None)
    );
}

#[test]
fn schedstat_text_gives_the_run_delay() {
    assert_eq!(parse_schedstat_run_delay("9593222 0 1\n"), Some(0));
    assert_eq!(parse_schedstat_run_delay("40194782 123456 17"), Some(123_456));
    for bad in ["", "1 2", "a b c", "1 -2 3", "1.5 2 3"] {
        assert_eq!(parse_schedstat_run_delay(bad), None, "{bad:?}");
    }
}

#[test]
fn the_defaults_are_the_grid_the_grid_follow_and_a_short_drain_run() {
    let steady = FrameSteadyStart::default();
    assert!(steady.grid && steady.grid_follow);
    assert_eq!(steady.start_offset_ns, DEFAULT_START_OFFSET_NS);
    const { assert!(DEFAULT_START_OFFSET_NS.abs() <= MAX_START_OFFSET_NS) };
    assert_eq!(steady.drain_run, STEADY_DRAIN_RUN);
    assert_eq!(STEADY_DRAIN_RUN, 2, "ADR-5564 waited for 8");
    assert_eq!(steady.grid_releases, 0);
}

#[test]
fn a_start_offset_moves_every_frame_start_against_its_tick() {
    for offset in [-3 * MS as i64, 4 * MS as i64] {
        let mut pacer = phone(pixel_delivery, None, None);
        pacer.set_start_offset(offset);
        let rows = run(&mut pacer, 200, |_| WORK);
        assert_eq!(rows.len(), 200, "offset {offset}");
        // The first frame takes the move; after that the starts are a period apart as before.
        for gap in intervals(&rows, 1) {
            assert!(
                gap.abs_diff(PERIOD) <= 100 * US,
                "offset {offset}: interval {gap} ns"
            );
        }
        for (start, row) in &rows[1..] {
            let tick = row.release_tick_ns.expect("released");
            let phase = *start as i64 - tick as i64 - offset;
            assert!(
                (400 * US as i64..=800 * US as i64).contains(&phase),
                "offset {offset}: the frame starts the offset off its tick's due time, {phase} ns"
            );
            assert_eq!(row.release, Some(PacingRelease::Grid));
            if offset < 0 {
                // Ahead of its tick: the callback has not run when the frame starts.
                assert_eq!(row.tick_delivered_ns, None, "{row:?}");
            }
        }
    }
    // More than the limit is the limit.
    let mut pacer = phone(pixel_delivery, None, None);
    pacer.set_start_offset(-3 * MAX_START_OFFSET_NS);
    let rows = run(&mut pacer, 20, |_| 6 * MS);
    let (start, row) = rows[19];
    let ahead = row.release_tick_ns.expect("released") as i64 - start as i64;
    assert!(
        ahead > MAX_START_OFFSET_NS - MS as i64 && ahead < MAX_START_OFFSET_NS,
        "{ahead}"
    );
}

#[test]
fn a_late_start_holds_a_tick_that_is_already_there() {
    // Frames 6 ms after their tick's due time whose work ends 1.5 ms after the next tick: that tick
    // has arrived when the wait begins (but for the late callbacks), well before its frame is due.
    // The frame still starts at its due time, not on the tick.
    let offset = 6 * MS as i64;
    let mut pacer = phone(pixel_delivery, None, None);
    pacer.set_start_offset(offset);
    let rows = run(&mut pacer, 80, |_| PERIOD - 5 * MS);
    assert_eq!(rows.len(), 80);
    for gap in intervals(&rows, 2) {
        assert!(gap.abs_diff(PERIOD) <= 100 * US, "interval {gap} ns");
    }
    let stamped = rows[2..]
        .iter()
        .filter(|(_, row)| row.tick_delivered_ns.is_some())
        .count();
    assert_eq!(stamped, 78, "every tick was there before its frame started");
    for (start, row) in &rows[2..] {
        let tick = row.release_tick_ns.expect("released");
        let phase = *start as i64 - tick as i64 - offset;
        assert!((400 * US as i64..=800 * US as i64).contains(&phase), "{phase} ns");
        assert_eq!(row.release, Some(PacingRelease::Grid));
    }
}

#[test]
fn frames_ahead_of_their_tick_keep_their_slot_and_stop_with_the_display() {
    // A frame 3 ms ahead of its tick that takes 2 ms has ended before the tick of its own slot
    // arrives, so the plan's newest tick is two slots before the next release. The pacer reads the
    // newest tick again when the release is due: the frames keep one period.
    let mut pacer = phone(pixel_delivery, None, None);
    pacer.set_start_offset(-3 * MS as i64);
    let rows = run(&mut pacer, 30, |_| 2 * MS);
    assert_eq!(rows.len(), 30);
    for gap in intervals(&rows, 1) {
        assert!(gap.abs_diff(PERIOD) <= 100 * US, "interval {gap} ns");
    }
    // And the display still leads: no tick after the 40th, one frame on the tick that was due
    // next, then none.
    let mut pacer = phone(pixel_delivery, None, Some(40));
    pacer.set_start_offset(-3 * MS as i64);
    let rows = run(&mut pacer, 60, |_| WORK);
    let past: Vec<u64> = rows
        .iter()
        .filter_map(|(_, row)| row.release_tick_ns)
        .filter(|tick| *tick > 40 * PERIOD)
        .collect();
    assert_eq!(past, vec![41 * PERIOD]);
    let released = rows
        .iter()
        .filter(|(_, row)| row.release_tick_ns.is_some())
        .count();
    assert_eq!(rows.len(), released + 1, "the next frame is not released");
    let cost = std::time::Duration::from_nanos(WORK);
    let waited = pacer.pace_frame(cost, std::time::Duration::ZERO, None, settings());
    assert!(waited.is_some(), "it waited for a tick that did not come");
}

const P60: u64 = 16_666_667;

/// A 60 Hz model as the runner sets it up, fed eight ticks and one released frame.
fn model(follow: bool) -> (PacingModel, u64) {
    let mut model = PacingModel::new();
    model.set_late_start(true);
    model.set_queue_drain(true);
    model.set_grid_follow(follow);
    for i in 1..=8 {
        model.on_tick(i * P60);
    }
    model.end_frame(9 * MS, None, None);
    let first = model.plan(8 * P60).expect("a period");
    model.released(first);
    (model, first)
}

#[test]
fn a_frame_released_on_a_real_tick_starts_on_the_moved_tick() {
    // The tick after the release comes half a period after it (the grid moved), a few microseconds
    // either way. The frame ends 9 ms after its release tick, so the moved tick is the next slot's
    // and the frame that is done starts on it: one frame per tick. Without the follow that was a
    // matter of microseconds.
    for jitter in [-30_000_i64, 0, 30_000] {
        let moved = (P60 / 2).checked_add_signed(jitter).expect("in range");
        let mut starts = Vec::new();
        for follow in [false, true] {
            let (mut model, release) = model(follow);
            model.on_tick(release + moved);
            model.swapchain_held(0);
            model.end_frame(9 * MS, None, None);
            let bound = model.plan(release + 9 * MS).expect("a period");
            starts.push((bound, release + moved, model.stats().grid_moves));
        }
        assert_eq!(starts[0].2, 0, "a new model does not follow");
        assert_eq!(starts[1].2, 1);
        assert_eq!(
            starts[1].0, starts[1].1,
            "jitter {jitter}: at once on the moved tick"
        );
        if jitter > 0 {
            assert_eq!(starts[0].0, starts[0].1, "jitter {jitter}");
        } else {
            assert!(
                starts[0].0 > starts[0].1,
                "jitter {jitter}: the old model waited a slot"
            );
        }
    }
}

#[test]
fn a_frame_released_on_a_predicted_tick_gives_the_moved_tick_its_slot() {
    // The frame was released on the tick the grid promised (one period after the last real tick),
    // and the compositor then issued that tick half a period later. The frame in flight took its
    // slot: the next frame waits for the tick after it. Without the follow the next frame started
    // at once on the late tick when it came a few microseconds past half a period: one frame more
    // than the compositor had ticks.
    for jitter in [-30_000_i64, 0, 30_000] {
        let moved = (P60 / 2).checked_add_signed(jitter).expect("in range");
        for follow in [false, true] {
            let (mut model, release) = model(follow);
            // The frame of `release` ends and the next one is released on the predicted tick.
            model.swapchain_held(0);
            model.end_frame(9 * MS, None, None);
            let predicted = release + P60;
            model.released(predicted);
            // That frame runs, and the real tick comes `moved` after its predicted time.
            let late_tick = predicted + moved;
            model.on_tick(late_tick);
            model.swapchain_held(0);
            model.end_frame(9 * MS, None, None);
            let bound = model.plan(predicted + 9 * MS).expect("a period");
            if follow {
                assert_eq!(model.stats().grid_moves, 1);
                assert!(
                    bound > late_tick,
                    "jitter {jitter}: a whole slot from the late tick"
                );
                assert!(bound <= late_tick + P60, "jitter {jitter}");
            } else if jitter > 0 {
                assert_eq!(bound, late_tick, "jitter {jitter}: one frame too many");
            } else {
                assert!(bound > late_tick, "jitter {jitter}");
            }
        }
    }
}

#[test]
fn ticks_on_the_grid_and_small_wobbles_move_nothing() {
    // Ticks a whole number of periods apart are the same grid, whatever the gap.
    {
        let (mut model, release) = model(true);
        for k in [1, 2, 5, 9] {
            model.on_tick(release + k * P60 + 40 * US);
        }
        assert_eq!(model.stats().grid_moves, 0);
    }
    // A compositor that wakes the app a few milliseconds earlier for a while (its early offsets)
    // moves the grid, and no frame is lost to it: each tick is still the next slot's tick.
    let (mut model, mut release) = model(true);
    let mut now = release;
    for (i, step) in [P60 - 6 * MS, P60, P60 + 6 * MS, P60, P60 - 6 * MS, P60 + 6 * MS]
        .into_iter()
        .enumerate()
    {
        // The frame takes 9 ms; the next tick comes `step` after the last one.
        let tick = now + step;
        model.on_tick(tick);
        model.swapchain_held(0);
        model.end_frame(9 * MS, None, None);
        let bound = model.plan(release + 9 * MS).expect("a period");
        assert!(
            bound <= tick,
            "step {i}: the frame starts on the very next tick, bound {bound} tick {tick}"
        );
        model.released(tick);
        release = tick;
        now = tick;
    }
    assert_eq!(model.stats().late, 0);
    assert!(model.stats().grid_moves >= 4);
}

#[test]
fn the_drain_run_can_be_shortened() {
    let stuck_frames_to_first_drain = |run: Option<u32>| {
        let (mut model, mut release) = model(false);
        if let Some(run) = run {
            model.set_drain_run(run);
        }
        let mut fed = release;
        for frame in 1..=40_u32 {
            // A frame of the stuck queue: 9 ms of work, 8 ms blocked, done just after its slot's tick.
            let end = release + P60 + 300 * US;
            while fed + P60 <= end {
                fed += P60;
                model.on_tick(fed);
            }
            model.swapchain_held(8 * MS);
            model.end_frame(9 * MS, None, None);
            let bound = model.plan(end).expect("a period");
            let tick = bound.div_ceil(P60) * P60;
            while fed < tick {
                fed += P60;
                model.on_tick(fed);
            }
            model.released(tick);
            release = tick;
            if model.stats().drains > 0 {
                return frame;
            }
        }
        0
    };
    assert_eq!(
        stuck_frames_to_first_drain(None),
        8,
        "ADR-5564's run for a new model"
    );
    assert_eq!(stuck_frames_to_first_drain(Some(STEADY_DRAIN_RUN)), 2);
    assert_eq!(stuck_frames_to_first_drain(Some(0)), 1, "at least one");
}
