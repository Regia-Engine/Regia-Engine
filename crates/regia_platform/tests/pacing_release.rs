// SPDX-License-Identifier: MPL-2.0
//! The frame timeline's delivery and wake stamps, the wake-ahead and the performance hint
//! target (ADR-5461): where the late frame starts of A13's presented p99 come from, and what the
//! frame loop does about them.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regia_platform::frame_pacing::{
    DEFAULT_DELIVERY_NS, DEFAULT_WAKE_AHEAD_NS, DisplayPacer, FramePacingSettings, FramePerfHint,
    FrameWakeAhead, MAX_WAKE_AHEAD_NS, PERF_HINT_RETARGET_DIVISOR, PREDICTED_RELEASE_MARGIN_NS, PacingCost,
    PacingMode, PacingSource, VsyncTicks, expected_release_tick, next_delivery_estimate, perf_hint_retarget,
    perf_hint_target_ns,
};

const MS: u64 = 1_000_000;
/// The stand-in panel: 62.5 Hz.
const PERIOD: u64 = 16 * MS;
/// The stand-in compositor delivers each tick 0.7 ms after its own time.
const DELIVERY: u64 = 700_000;

fn now_zero() -> u64 {
    0
}

fn settings() -> FramePacingSettings {
    FramePacingSettings {
        mode: PacingMode::Auto,
        cost: PacingCost::Work,
        ..Default::default()
    }
}

#[test]
fn a_waited_tick_comes_with_its_own_delivery_stamp() {
    let ticks = VsyncTicks::new();
    ticks.tick_delivered(100 * MS, 100 * MS + DELIVERY);
    assert_eq!(
        ticks.wait_for_delivered(100 * MS, Duration::ZERO),
        Some((100 * MS, Some(100 * MS + DELIVERY)))
    );
    // An older tick that arrives late does not replace the newest tick's stamp.
    ticks.tick_delivered(90 * MS, 200 * MS);
    assert_eq!(
        ticks.wait_for_delivered(0, Duration::ZERO),
        Some((100 * MS, Some(100 * MS + DELIVERY)))
    );
    // A source that does not stamp (a desktop test feeder) leaves the newest tick without one.
    ticks.tick(116 * MS);
    assert_eq!(
        ticks.wait_for_delivered(0, Duration::ZERO),
        Some((116 * MS, None))
    );
    assert_eq!(
        ticks.wait_for(0, Duration::ZERO),
        Some(116 * MS),
        "wait_for is unchanged"
    );
    assert_eq!(ticks.total(), 3);
    // Nothing at or past the bound yet: no tick, no stamp.
    assert_eq!(ticks.wait_for_delivered(200 * MS, Duration::ZERO), None);
}

/// A pacer on ticks stamped `DELIVERY` after their time, fed from a test thread until `done`.
fn stamped_pacer() -> (DisplayPacer, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick_delivered(i * PERIOD, i * PERIOD + DELIVERY);
    }
    let pacer = DisplayPacer::new(ticks.clone(), PacingSource::Choreographer, now_zero);
    let done = Arc::new(AtomicBool::new(false));
    let feeder_done = done.clone();
    let thread = std::thread::spawn(move || {
        let mut time = 9 * PERIOD;
        while !feeder_done.load(Ordering::Relaxed) {
            ticks.tick_delivered(time, time + DELIVERY);
            time += PERIOD;
            std::thread::yield_now();
        }
    });
    (pacer, done, thread)
}

#[test]
fn a_paced_row_carries_the_delivery_and_wake_stamps() {
    let (mut pacer, done, thread) = stamped_pacer();
    let cost = Duration::from_millis(5);
    let mut rows = Vec::new();
    for _ in 0..6 {
        let waited = pacer.pace_frame(cost, Duration::ZERO, None, settings());
        rows.push(pacer.trace_row(cost, Duration::ZERO, waited));
    }
    pacer.skip_frame();
    let skipped = pacer.trace_row(cost, Duration::ZERO, None);
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");

    for row in &rows {
        assert!(row.paced, "{row:?}");
        let tick = row.release_tick_ns.expect("a paced frame has a release tick");
        // The stamp is the release tick's own: read with the tick, so a newer tick from the
        // feeder never lends it its stamp.
        assert_eq!(row.tick_delivered_ns, Some(tick + DELIVERY), "{row:?}");
        // The pacer's clock (here a stand-in that reads 0) when the wait returned.
        assert_eq!(row.woke_ns, Some(0), "{row:?}");
    }
    assert_eq!(skipped.tick_delivered_ns, None, "a skipped frame has no release");
    assert_eq!(skipped.woke_ns, None);
}

#[test]
fn an_unpaced_frame_has_no_stamps() {
    let (mut pacer, done, thread) = stamped_pacer();
    let cost = Duration::from_millis(5);
    let off = FramePacingSettings {
        mode: PacingMode::Off,
        ..settings()
    };
    let waited = pacer.pace_frame(cost, Duration::ZERO, None, off);
    let row = pacer.trace_row(cost, Duration::ZERO, waited);
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");
    assert!(!row.paced);
    assert_eq!((row.tick_delivered_ns, row.woke_ns), (None, None));
}

#[test]
fn the_wake_ahead_predicts_the_tick_the_pacer_waits_for() {
    let latest = 100 * MS;
    // The next grid tick at or past the bound.
    assert_eq!(expected_release_tick(latest, PERIOD, 110 * MS), Some(116 * MS));
    assert_eq!(
        expected_release_tick(latest, PERIOD, 116 * MS),
        Some(116 * MS),
        "on the grid"
    );
    assert_eq!(
        expected_release_tick(latest, PERIOD, 117 * MS),
        Some(132 * MS),
        "a slot of two"
    );
    // A bound the newest tick already meets: that tick.
    assert_eq!(expected_release_tick(latest, PERIOD, 90 * MS), Some(latest));
    assert_eq!(expected_release_tick(latest, 0, 110 * MS), None, "no period");
    assert_eq!(
        expected_release_tick(u64::MAX - 1, PERIOD, u64::MAX),
        None,
        "no overflow"
    );
}

#[test]
fn the_delivery_estimate_is_a_running_mean_a_stray_callback_cannot_drag() {
    assert_eq!(next_delivery_estimate(500_000, 500_000, PERIOD), 500_000);
    assert_eq!(next_delivery_estimate(500_000, 1_300_000, PERIOD), 600_000);
    // A callback 30 ms late counts as a quarter period.
    assert_eq!(
        next_delivery_estimate(500_000, 30 * MS, PERIOD),
        (7 * 500_000 + PERIOD / 4) / 8
    );
    // It settles on a steady delay.
    let mut est = DEFAULT_DELIVERY_NS;
    for _ in 0..200 {
        est = next_delivery_estimate(est, 800_000, PERIOD);
    }
    assert!((790_000..=800_000).contains(&est), "{est}");
}

#[test]
fn a_wake_ahead_on_a_clock_that_does_not_match_never_stalls_the_frames() {
    // The pacer's stand-in clock reads 0 and the ticks are far ahead of it: the wake-ahead must not
    // sleep towards them or poll forever, and the stamps stay the release tick's own.
    let (mut pacer, done, thread) = stamped_pacer();
    pacer.set_wake_ahead(Some(MAX_WAKE_AHEAD_NS * 10));
    let cost = Duration::from_millis(5);
    let mut rows = Vec::new();
    for _ in 0..6 {
        let waited = pacer.pace_frame(cost, Duration::ZERO, None, settings());
        rows.push(pacer.trace_row(cost, Duration::ZERO, waited));
    }
    pacer.set_wake_ahead(None);
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");
    for row in &rows {
        assert!(row.paced, "{row:?}");
        let tick = row.release_tick_ns.expect("released");
        assert_eq!(row.tick_delivered_ns, Some(tick + DELIVERY), "{row:?}");
    }
    assert!(pacer.polled_releases() <= rows.len() as u64);
}

#[test]
fn a_late_callback_is_overtaken_inside_the_poll_window() {
    // The predicted release fires after the usual delivery plus the margin and before the poll
    // gives up at the due time plus the lead: the margin must sit inside the default lead, and stay
    // above the measured delivery jitter (p99 under 1 ms against a 0.5 ms mean, ADR-5461).
    const { assert!(PREDICTED_RELEASE_MARGIN_NS < DEFAULT_WAKE_AHEAD_NS) };
    const { assert!(PREDICTED_RELEASE_MARGIN_NS >= 500_000) };
    // A frame released 0.6 ms past the usual delivery after one released at the usual time is
    // inside 1.05 refresh intervals on a 60 Hz panel.
    let period = 16_666_667_u64;
    assert!(period + PREDICTED_RELEASE_MARGIN_NS < period + period / 20);
    assert_eq!(FrameWakeAhead::default().predicted_releases, 0);
}

#[test]
fn the_wake_ahead_is_on_by_default() {
    let wake = FrameWakeAhead::default();
    assert_eq!(wake.lead_ns, Some(DEFAULT_WAKE_AHEAD_NS));
    const { assert!(DEFAULT_WAKE_AHEAD_NS <= MAX_WAKE_AHEAD_NS) };
    assert_eq!(wake.polled_releases, 0);
}

#[test]
fn the_performance_hint_targets_the_slot() {
    let period = 16_666_667;
    assert_eq!(perf_hint_target_ns(Some(period), 1), Some(period));
    assert_eq!(perf_hint_target_ns(Some(period), 2), Some(2 * period));
    assert_eq!(perf_hint_target_ns(None, 1), None, "no period estimate yet");
    assert_eq!(perf_hint_target_ns(Some(0), 1), None);
    assert_eq!(perf_hint_target_ns(Some(period), 0), None);
}

#[test]
fn the_performance_hint_target_is_sent_again_only_when_the_slot_moves() {
    let period = 16_666_667;
    assert!(
        perf_hint_retarget(None, period),
        "the first target is always sent"
    );
    // The period estimate wobbles by microseconds: no update.
    assert!(!perf_hint_retarget(Some(period), period + 5_000));
    assert!(!perf_hint_retarget(
        Some(period),
        period - period / PERF_HINT_RETARGET_DIVISOR
    ));
    // A new swap interval or a new panel rate: update.
    assert!(perf_hint_retarget(Some(period), 2 * period));
    assert!(perf_hint_retarget(Some(period), 8_333_333));
}

#[test]
fn the_performance_hint_is_off_by_default() {
    // ADR-5461: it did not help the TEST Pixel 9 Pro's late starts; an app or agent turns it on.
    let hint = FramePerfHint::default();
    assert!(!hint.enabled);
    assert!(!hint.active && !hint.unavailable);
    assert_eq!((hint.target_ns, hint.reports, hint.failures), (None, 0, 0));
}
