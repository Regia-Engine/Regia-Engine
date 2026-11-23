// SPDX-License-Identifier: MPL-2.0
//! The pacing trace and the work cost (ADR-2320): what `Auto` counts as a frame's cost, the
//! per-frame rows `pacing.trace` reads, and the swapchain-wait hand-off from the renderer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use regia_platform::frame_pacing::{
    DisplayPacer, FramePacingSettings, FramePacingTrace, PacingCost, PacingHint, PacingMode, PacingSim,
    PacingSource, PacingTraceRow, SwapchainSim, TRACE_CAPACITY, VsyncTicks, note_swapchain_wait, simulate,
    take_swapchain_wait,
};

const MS: u64 = 1_000_000;
/// The stand-in panel: 62.5 Hz.
const PERIOD: u64 = 16 * MS;

fn now_zero() -> u64 {
    0
}

/// Ticks from a test thread until `done`.
fn feeder(ticks: Arc<VsyncTicks>, done: Arc<AtomicBool>, first_ns: u64) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut time = first_ns;
        while !done.load(Ordering::Relaxed) {
            ticks.tick(time);
            time += PERIOD;
            std::thread::yield_now();
        }
    })
}

/// A pacer that knows the panel, with a tick thread running.
fn live_pacer() -> (DisplayPacer, Arc<AtomicBool>, std::thread::JoinHandle<()>) {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick(i * PERIOD);
    }
    let pacer = DisplayPacer::new(ticks.clone(), PacingSource::Choreographer, now_zero);
    let done = Arc::new(AtomicBool::new(false));
    let thread = feeder(ticks, done.clone(), 9 * PERIOD);
    (pacer, done, thread)
}

fn settings(cost: PacingCost) -> FramePacingSettings {
    FramePacingSettings {
        mode: PacingMode::Auto,
        cost,
        ..Default::default()
    }
}

/// Frames of 40 ms of which 36 ms sat in the swapchain: 4 ms of work on a 16 ms panel.
fn run_blocked_frames(cost: PacingCost) -> u32 {
    let (mut pacer, done, thread) = live_pacer();
    for _ in 0..12 {
        assert!(
            pacer
                .pace_frame(
                    Duration::from_millis(40),
                    Duration::from_millis(36),
                    None,
                    settings(cost)
                )
                .is_some(),
            "paced on the display"
        );
    }
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");
    pacer.report(PacingMode::Auto).swap_interval
}

/// A hitch longer than the tick channel's buffer (40 ticks arrive before the next drain; the
/// channel keeps the newest 32) must not count the dropped ticks as vsyncs the source skipped:
/// `missed_ticks` is documented as 0 while the ticks keep up.
#[test]
fn ticks_dropped_by_the_channel_are_not_missed_vsyncs() {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick(i * PERIOD);
    }
    let mut pacer = DisplayPacer::new(ticks.clone(), PacingSource::Choreographer, now_zero);
    pacer.skip_frame();
    let first = pacer.trace_row(Duration::ZERO, Duration::ZERO, None);
    assert_eq!(first.missed_ticks, 0);
    for i in 9..=48 {
        ticks.tick(i * PERIOD);
    }
    pacer.skip_frame();
    let row = pacer.trace_row(Duration::ZERO, Duration::ZERO, None);
    assert_eq!(row.ticks, 40, "every tick is counted");
    assert_eq!(
        row.missed_ticks, 0,
        "the display skipped nothing; the channel dropped 8 ticks during the hitch"
    );
    assert_eq!(row.tick_gap_max_ns, PERIOD, "the kept ticks are one period apart");
}

#[test]
fn the_work_cost_leaves_swapchain_blocking_out() {
    // The work fits one vsync with room to spare: the interval stays at 1.
    assert_eq!(run_blocked_frames(PacingCost::Work), 1);
    // The v0.2.10 model counts the blocking and climbs to where 40 ms fits.
    assert_eq!(run_blocked_frames(PacingCost::Frame), 3);
}

#[test]
fn work_is_the_default_cost() {
    assert_eq!(FramePacingSettings::default().cost, PacingCost::Work);
    assert_eq!(PacingCost::Work.name(), "work");
    assert_eq!(PacingCost::Frame.name(), "frame");
}

#[test]
fn real_work_over_the_slot_still_raises_the_interval() {
    // 40 ms of work and no blocking: the work cost is the frame cost, and 40 ms needs 3 vsyncs.
    let (mut pacer, done, thread) = live_pacer();
    for _ in 0..12 {
        let _ = pacer.pace_frame(
            Duration::from_millis(40),
            Duration::ZERO,
            None,
            settings(PacingCost::Work),
        );
    }
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");
    assert_eq!(pacer.report(PacingMode::Auto).swap_interval, 3);
}

#[test]
fn a_trace_row_describes_the_frame() {
    let (mut pacer, done, thread) = live_pacer();
    let cost = Duration::from_millis(5);
    let wait = Duration::from_millis(2);
    let waited = pacer.pace_frame(cost, wait, None, settings(PacingCost::Work));
    let row = pacer.trace_row(cost, wait, waited);
    let off = pacer.pace_frame(
        cost,
        wait,
        None,
        FramePacingSettings {
            mode: PacingMode::Off,
            cost: PacingCost::Work,
            ..Default::default()
        },
    );
    let unpaced = pacer.trace_row(cost, wait, off);
    done.store(true, Ordering::Relaxed);
    thread.join().expect("feeder");

    assert!(row.paced, "{row:?}");
    assert_eq!(row.cost_ns, 5 * MS);
    assert_eq!(row.swapchain_wait_ns, 2 * MS);
    assert_eq!(row.period_ns, Some(PERIOD));
    assert_eq!(row.interval, 1);
    assert!(row.release_tick_ns.is_some(), "{row:?}");
    assert!(
        row.ticks >= 8,
        "the preloaded ticks count into the first row: {row:?}"
    );

    assert!(!unpaced.paced, "{unpaced:?}");
    assert_eq!(unpaced.waited_ns, 0);
    assert_eq!(unpaced.release_tick_ns, None, "off releases nothing");
}

#[test]
fn the_trace_keeps_the_newest_rows() {
    let mut trace = FramePacingTrace::default();
    for i in 0..(TRACE_CAPACITY as u64 + 5) {
        trace.push(PacingTraceRow {
            start_ns: i,
            ..PacingTraceRow::default()
        });
    }
    assert_eq!(trace.rows().len(), TRACE_CAPACITY);
    assert_eq!(trace.rows().next().map(|r| r.start_ns), Some(5));
    assert_eq!(
        trace.rows().next_back().map(|r| r.start_ns),
        Some(TRACE_CAPACITY as u64 + 4)
    );
    trace.clear();
    assert_eq!(trace.rows().len(), 0);
}

#[test]
fn swapchain_waits_add_up_until_taken() {
    // The only test in this binary that touches the process-wide sum.
    let _ = take_swapchain_wait();
    note_swapchain_wait(Duration::from_micros(1500));
    note_swapchain_wait(Duration::from_micros(500));
    assert_eq!(take_swapchain_wait(), Duration::from_millis(2));
    assert_eq!(take_swapchain_wait(), Duration::ZERO, "taken once");
}

/// A phone as both test phones behave (ADR-2320): the app holds two buffers, the display keeps a
/// replaced buffer until its next latch, the GPU takes 7 ms a frame, the latch comes 4 ms after
/// each tick, and the compositor follows the rate hint. The CPU work is 6 ms.
fn phone(refresh_hz: f32, cost: PacingCost) -> PacingSim {
    PacingSim {
        refresh_hz,
        frame_ms: vec![6.0],
        frames: 2000,
        cap_fps: None,
        interval: None,
        refresh_switch: None,
        follow_hint: true,
        swapchain: Some(SwapchainSim {
            images: 2,
            gpu_ms: 7.0,
            latch_ms: 4.0,
        }),
        cost,
        stretch: 0.0,
        // The ADR-2320 runs pin the slot's own rate as the hint, as v0.2.10 sent it.
        hint: PacingHint::Slot,
    }
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 0.5
}

#[test]
fn the_frame_cost_collapses_on_a_full_swapchain_and_a_following_panel() {
    // The v0.2.10 model: the swapchain's blocking counts as cost, the interval rises, the hint
    // slows the panel, the blocking grows with the slower ticks, and every frame ends up taking
    // two slots of the 20 fps limit or more: 10 fps at 60 Hz, as both phones measured.
    for (hz, gap_ms, hint) in [(60.0, 100.0, 20.0), (90.0, 133.3, 15.0), (120.0, 166.7, 12.0)] {
        let report = simulate(&phone(hz, PacingCost::Frame)).expect("runs");
        assert!(close(report.tail_mean_gap_ms, gap_ms), "{hz} Hz: {report:?}");
        assert!(
            report.frame_rate_hint.is_some_and(|h| (h - hint).abs() < 0.1),
            "{hz} Hz: {report:?}"
        );
        assert!(report.tail_late > 400, "every frame late: {report:?}");
    }
}

#[test]
fn the_work_cost_runs_at_the_rate_the_swapchain_allows() {
    // The same phone with the work cost: no hint, the panel keeps its rate, and frames run as
    // fast as the two buffers let them (what pacing off measured on the phones: about 30 fps).
    for (hz, gap_ms) in [(60.0, 33.3), (90.0, 33.3), (120.0, 25.0)] {
        let report = simulate(&phone(hz, PacingCost::Work)).expect("runs");
        assert!(close(report.tail_mean_gap_ms, gap_ms), "{hz} Hz: {report:?}");
        assert!(
            close(report.tail_min_gap_ms, report.tail_max_gap_ms),
            "even frames: {report:?}"
        );
        assert_eq!(report.frame_rate_hint, None, "{hz} Hz: {report:?}");
        assert_eq!(report.tail_late, 0, "held frames are not late: {report:?}");
        assert!(report.stats.held > 1900, "the swapchain set the pace: {report:?}");
        assert!(report.tail_swapchain_wait_ms > 10.0, "{report:?}");
    }
}

#[test]
fn real_work_still_picks_the_interval_behind_a_swapchain() {
    // Three buffers and 20 ms of CPU work on a 60 Hz panel that ignores the hint: the swapchain
    // never fills, and the work alone sets two vsyncs per frame.
    let report = simulate(&PacingSim {
        frame_ms: vec![20.0],
        follow_hint: false,
        swapchain: Some(SwapchainSim {
            images: 3,
            gpu_ms: 7.0,
            latch_ms: 4.0,
        }),
        ..phone(60.0, PacingCost::Work)
    })
    .expect("runs");
    assert_eq!(report.swap_interval, 2, "{report:?}");
    assert!(close(report.tail_mean_gap_ms, 33.3), "{report:?}");
    assert_eq!(report.tail_late, 0, "{report:?}");
}

#[test]
fn a_run_without_a_swapchain_blocks_nothing() {
    // `swapchain: None` blocks nothing: both costs see the frame costs as they are and no frame
    // is held. The two costs are not the same model, though: the runner starts a Work-cost frame
    // that missed its slot's tick at once ([`PacingModel::set_late_start`], ADR-4913, on in
    // `simulate` as in `FramePacer::pace_frame`), so once a frame ends off the tick grid (the
    // first frames of a run, an overrun at a probe's short slot) the reports differ.
    let base = PacingSim {
        swapchain: None,
        frame_ms: vec![8.0, 8.0, 20.0],
        ..phone(120.0, PacingCost::Work)
    };
    let work = simulate(&base).expect("work");
    let frame = simulate(&PacingSim {
        cost: PacingCost::Frame,
        ..base
    })
    .expect("frame");
    assert_eq!(work.tail_swapchain_wait_ms, 0.0);
    assert_eq!(frame.tail_swapchain_wait_ms, 0.0);
    assert_eq!(work.stats.held, 0);
    assert_eq!(frame.stats.held, 0);
    assert_ne!(
        work, frame,
        "the Work cost's late start (ADR-4913) differentiates the runs once a frame overruns a slot"
    );
}

#[test]
fn a_swapchain_outside_its_ranges_is_refused() {
    for chain in [
        SwapchainSim {
            images: 1,
            gpu_ms: 7.0,
            latch_ms: 4.0,
        },
        SwapchainSim {
            images: 9,
            gpu_ms: 7.0,
            latch_ms: 4.0,
        },
        SwapchainSim {
            images: 2,
            gpu_ms: f32::NAN,
            latch_ms: 4.0,
        },
        SwapchainSim {
            images: 2,
            gpu_ms: 7.0,
            latch_ms: -1.0,
        },
        SwapchainSim {
            images: 2,
            gpu_ms: 7.0,
            latch_ms: 2000.0,
        },
    ] {
        let sim = PacingSim {
            swapchain: Some(chain),
            ..phone(60.0, PacingCost::Work)
        };
        assert_eq!(simulate(&sim), None, "{chain:?}");
    }
}

#[test]
fn a_held_frame_that_missed_its_tick_starts_at_once() {
    let mut model = regia_platform::frame_pacing::PacingModel::new();
    for i in 1..=8 {
        model.on_tick(i * PERIOD);
    }
    model.released(8 * PERIOD);
    let latest = 9 * PERIOD;
    model.on_tick(latest);
    // 5 ms after the tick, not held: wait for the next tick, as before.
    model.swapchain_held(0);
    assert!(model.plan(latest + 5 * MS).expect("a plan") > latest);
    // Held back by the swapchain past the tick of its slot: that tick starts it.
    model.swapchain_held(10 * MS);
    assert_eq!(model.plan(latest + 5 * MS), Some(latest));
    model.released(latest);
    assert_eq!(model.stats().held, 1);
    assert_eq!(model.stats().late, 0);
    // Held, but its slot is still ahead (interval 1, the slot's tick not here yet): it waits.
    assert!(model.plan(latest + MS).expect("a plan") > latest);
}

#[test]
fn a_boot_stall_comes_back_in_one_hold() {
    // Ten 300 ms frames (pipeline compiles at start-up) push the slot to the 20 fps limit and the
    // panel follows the hint. Light frames then bring it straight back to the panel's rate after
    // one hold: one rate change down and one back, not one per step of the grid (ADR-2320).
    for hz in [60.0_f32, 90.0, 120.0] {
        let mut costs = vec![300.0; 10];
        costs.extend(std::iter::repeat_n(6.0, 1990));
        let report = simulate(&PacingSim {
            frame_ms: costs,
            swapchain: None,
            ..phone(hz, PacingCost::Work)
        })
        .expect("runs");
        assert_eq!(report.stats.refresh_changes, 2, "{hz} Hz: {report:?}");
        assert_eq!(report.frame_rate_hint, None, "{hz} Hz: {report:?}");
        let panel_ms = 1000.0 / hz;
        assert!(close(report.tail_min_gap_ms, panel_ms), "{hz} Hz: {report:?}");
        assert!(close(report.tail_max_gap_ms, panel_ms), "{hz} Hz: {report:?}");
    }
}

/// A start-up stall, then frames of `base_ms` that grow into `stretch` of the idle time their slot
/// leaves, on a panel that follows the hint and no swapchain (ADR-2339).
fn stretching(refresh_hz: f32, base_ms: f32, stretch: f32, frames: u32, cost: PacingCost) -> PacingSim {
    let mut costs = vec![300.0; 10];
    costs.extend(std::iter::repeat_n(base_ms, frames as usize - 10));
    PacingSim {
        frame_ms: costs,
        frames,
        swapchain: None,
        stretch,
        ..phone(refresh_hz, cost)
    }
}

#[test]
fn a_cost_that_grows_with_the_slot_is_probed_down_to_where_it_fits() {
    // What the Fairphone 5 showed (ADR-2320 (7)): at the slow slot the frames cost more than
    // the shorter slot holds, so the cost alone never lets the slot drop. 24 ms frames that grow
    // into 60 % of the idle time of recent frames cost 39.6 ms at 50 ms apart, 29.6 at 33.3 and
    // 24 when they run back to back. The v0.2.10 model ends with every frame late, at 10 fps,
    // the Fairphone's own number; the probe ends at 30 fps at 60 and 90 Hz (a 33.3 ms slot), and
    // at 40 fps at 120 Hz (a 25 ms slot): late start (ADR-4913, the Work cost starts a frame that
    // missed its slot's tick at once, as `pace_frame` does) leaves shorter gaps, the stretched
    // cost shrinks with them, and 8 frames in a row fit the 25 ms slot, so the probe stays.
    for (hz, want_ms) in [(60.0_f32, 33.33), (90.0, 33.33), (120.0, 25.0)] {
        let old = simulate(&stretching(hz, 24.0, 0.6, 3000, PacingCost::Frame)).expect("frame");
        assert!(old.tail_mean_gap_ms > 99.0, "{hz} Hz, v0.2.10 collapses: {old:?}");
        assert_eq!(old.stats.probes, 0, "{hz} Hz: {old:?}");
        let new = simulate(&stretching(hz, 24.0, 0.6, 3000, PacingCost::Work)).expect("work");
        assert!(close(new.tail_mean_gap_ms, want_ms), "{hz} Hz: {new:?}");
        assert!(
            close(new.tail_min_gap_ms, new.tail_max_gap_ms),
            "even frames: {new:?}"
        );
        assert_eq!(new.tail_late, 0, "{hz} Hz: {new:?}");
        assert!(new.stats.probes >= 1, "{hz} Hz: {new:?}");
        // Settled: a longer run changes nothing more.
        let longer = simulate(&stretching(hz, 24.0, 0.6, 9000, PacingCost::Work)).expect("longer");
        assert_eq!(
            longer.stats,
            regia_platform::frame_pacing::PacingStats {
                frames: 9000,
                ..new.stats
            },
            "{hz} Hz"
        );
    }
}

#[test]
fn a_cost_that_does_not_grow_is_probed_once_after_a_stall_and_never_without_one() {
    // 40 ms frames after the stall really need the 50 ms slot: one probe of 33.3 ms bounces, and
    // the frame that raised the slot back is no costlier than the rest, so no probe follows.
    let run = |frames| simulate(&stretching(60.0, 40.0, 0.0, frames, PacingCost::Work)).expect("runs");
    let (report, longer) = (run(2000), run(8000));
    assert_eq!(report.stats.probes, 1, "{report:?}");
    assert_eq!(
        longer.stats,
        regia_platform::frame_pacing::PacingStats {
            frames: 8000,
            ..report.stats
        }
    );
    assert!(close(longer.tail_mean_gap_ms, 50.0), "{longer:?}");
    assert_eq!(longer.tail_late, 0, "{longer:?}");
    // Without a stall nothing ever raised the slot above what its frames cost: no probe at all.
    for hz in [60.0, 90.0, 120.0] {
        for ms in [11.0, 20.0, 30.0, 45.0] {
            let steady = simulate(&PacingSim {
                frame_ms: vec![ms],
                swapchain: None,
                ..phone(hz, PacingCost::Work)
            })
            .expect("runs");
            assert_eq!(steady.stats.probes, 0, "{hz} Hz {ms} ms: {steady:?}");
        }
    }
}

#[test]
fn a_stretch_outside_its_range_is_refused() {
    for stretch in [-0.1, 0.96, f32::NAN, f32::INFINITY] {
        assert_eq!(
            simulate(&stretching(60.0, 24.0, stretch, 100, PacingCost::Work)),
            None,
            "{stretch}"
        );
    }
    assert!(simulate(&stretching(60.0, 24.0, 0.95, 100, PacingCost::Work)).is_some());
}

#[test]
fn a_probe_waits_for_the_panel_and_starts_late_frames_at_once() {
    // 62.5 Hz (16 ms); a 300 ms stall raises the slot to the limit, 4 vsyncs (64 ms), and the
    // panel follows the hint.
    let mut model = regia_platform::frame_pacing::PacingModel::new();
    let mut tick = 0;
    for _ in 0..8 {
        tick += PERIOD;
        model.on_tick(tick);
    }
    for _ in 0..4 {
        model.end_frame(300 * MS, None, None);
    }
    let slow = 4 * PERIOD;
    assert!(
        model
            .frame_rate_hint()
            .is_some_and(|h| (h - 1e3 / 64.0).abs() < 0.1)
    );
    for _ in 0..32 {
        tick += slow;
        model.on_tick(tick);
    }
    assert_eq!(model.swap_interval(), 1);
    // 55 ms frames: well below the stall, not under 48 ms with headroom. After a hold, a probe.
    let mut frames = 0;
    while model.stats().probes == 0 {
        model.end_frame(55 * MS, None, None);
        frames += 1;
        assert!(frames <= 60, "no probe");
    }
    assert!(
        model
            .frame_rate_hint()
            .is_some_and(|h| (h - 1e3 / 48.0).abs() < 0.1),
        "asks for 48 ms"
    );
    // The panel is slow to follow. Frames that fit the old 64 ms slot but not the probed one keep
    // the probe open: a late frame starts at once on the tick it missed.
    for _ in 0..20 {
        model.end_frame(55 * MS, None, None);
    }
    model.released(tick);
    tick += slow;
    model.on_tick(tick);
    assert_eq!(model.plan(tick + 20 * MS), Some(tick), "the probe still runs");
    assert_eq!(model.stats().probes, 1);
}

#[test]
fn the_double_hint_keeps_two_vsyncs_per_frame_on_the_panel_grid() {
    // A 120 Hz panel (8.33 ms): forced intervals of 2 to 6 panel vsyncs.
    let panel = 8_333_333;
    let hint_for = |hint: PacingHint, vsyncs: u32| {
        let mut model = regia_platform::frame_pacing::PacingModel::new();
        model.set_hint(hint);
        let mut tick = 0;
        for _ in 0..8 {
            tick += panel;
            model.on_tick(tick);
        }
        model.end_frame(MS, None, Some(vsyncs));
        model.frame_rate_hint().map(f32::round)
    };
    // 2 or 3 vsyncs: no slower rate keeps two vsyncs a frame on the grid, so no hint.
    assert_eq!(hint_for(PacingHint::Double, 2), None);
    assert_eq!(hint_for(PacingHint::Double, 3), None);
    // 4 vsyncs (30 fps): the panel may run at 60, two vsyncs a frame; 6 (20 fps): 40, three of them.
    assert_eq!(hint_for(PacingHint::Double, 4), Some(60.0));
    assert_eq!(hint_for(PacingHint::Double, 6), Some(40.0));
    assert_eq!(
        hint_for(PacingHint::Double, 5),
        None,
        "5 is prime: no slower grid"
    );
    // The v0.2.10 hint asks for the slot's own rate; off asks for nothing.
    assert_eq!(hint_for(PacingHint::Slot, 2), Some(60.0));
    assert_eq!(hint_for(PacingHint::Slot, 4), Some(30.0));
    for vsyncs in 1..=6 {
        assert_eq!(hint_for(PacingHint::Off, vsyncs), None);
    }
    assert_eq!(PacingHint::default(), PacingHint::Double);
    assert_eq!(FramePacingSettings::default().hint, PacingHint::Double);
}

#[test]
fn the_double_hint_keeps_the_phone_out_of_the_collapse() {
    // What the Fairphone 5 measured (ADR-2339): a two-buffer display queue whose blocking lands
    // inside the frame, not in acquire, so the frame's cost includes it (`cost: frame` stands in
    // for that). Hinting the slot's own rate slows the panel and halves the rate again and again:
    // 10 fps. The double hint never slows the panel below two vsyncs a frame: about 30 fps.
    for hz in [60.0_f32, 90.0, 120.0] {
        let old = simulate(&phone(hz, PacingCost::Frame)).expect("slot hint");
        assert!(old.tail_mean_gap_ms > 99.0, "{hz} Hz: {old:?}");
        let new = simulate(&PacingSim {
            hint: PacingHint::Double,
            ..phone(hz, PacingCost::Frame)
        })
        .expect("double hint");
        assert!(new.tail_mean_gap_ms < 34.0, "{hz} Hz: {new:?}");
    }
}

// ---- Tick cadence apart from the frames (ADR-4913) ----

fn off() -> FramePacingSettings {
    FramePacingSettings {
        mode: PacingMode::Off,
        ..Default::default()
    }
}

/// A pacer whose ticks the test feeds by hand: eight ticks one period apart, already drained into
/// one row so the next row starts clean.
fn hand_fed_pacer() -> (DisplayPacer, Arc<VsyncTicks>) {
    let ticks = VsyncTicks::new();
    for i in 1..=8 {
        ticks.tick(i * PERIOD);
    }
    let mut pacer = DisplayPacer::new(ticks.clone(), PacingSource::Choreographer, now_zero);
    let cost = Duration::from_millis(5);
    let first = pacer.pace_frame(cost, Duration::ZERO, None, off());
    let row = pacer.trace_row(cost, Duration::ZERO, first);
    assert_eq!(row.missed_ticks, 0, "{row:?}");
    assert_eq!(row.tick_gap_max_ns, PERIOD, "{row:?}");
    (pacer, ticks)
}

#[test]
fn a_skipped_vsync_shows_in_the_row_whatever_the_frame_did() {
    let (mut pacer, ticks) = hand_fed_pacer();
    // Ticks 9 and 10 arrive, 11 and 12 never do, 13 does.
    for i in [9, 10, 13] {
        ticks.tick(i * PERIOD);
    }
    let cost = Duration::from_millis(5);
    let off_frame = pacer.pace_frame(cost, Duration::ZERO, None, off());
    let row = pacer.trace_row(cost, Duration::ZERO, off_frame);
    assert_eq!(row.ticks, 3, "{row:?}");
    assert_eq!(row.missed_ticks, 2, "{row:?}");
    assert_eq!(row.tick_gap_max_ns, 3 * PERIOD, "{row:?}");
    // The next row counts only its own ticks.
    ticks.tick(14 * PERIOD);
    let again = pacer.pace_frame(cost, Duration::ZERO, None, off());
    let next = pacer.trace_row(cost, Duration::ZERO, again);
    assert_eq!((next.missed_ticks, next.tick_gap_max_ns), (0, PERIOD), "{next:?}");
}

#[test]
fn the_gap_of_a_suspend_is_no_skipped_vsync() {
    let (mut pacer, ticks) = hand_fed_pacer();
    pacer.suspend_ticks();
    pacer.resume_ticks();
    // Back 40 periods later (640 ms: past the longest tick interval anyway), then even ticks.
    for i in [48, 49, 50] {
        ticks.tick(i * PERIOD);
    }
    let cost = Duration::from_millis(5);
    let frame = pacer.pace_frame(cost, Duration::ZERO, None, off());
    let row = pacer.trace_row(cost, Duration::ZERO, frame);
    assert_eq!(row.missed_ticks, 0, "{row:?}");
    assert_eq!(row.tick_gap_max_ns, PERIOD, "{row:?}");
}

#[test]
fn the_cap_wait_lands_on_the_newest_row() {
    let mut trace = FramePacingTrace::default();
    trace.note_cap_wait(Duration::from_millis(3));
    assert_eq!(trace.rows().len(), 0, "no row, nothing to note");
    trace.push(PacingTraceRow::default());
    trace.push(PacingTraceRow::default());
    trace.note_cap_wait(Duration::from_millis(7));
    let waits: Vec<u64> = trace.rows().map(|r| r.cap_waited_ns).collect();
    assert_eq!(waits, vec![0, 7 * MS]);
}

/// `n` rows `interval_ms` apart with the given parts, ms; ticks one period apart, `missed`
/// skipped vsyncs per row.
fn rows(
    n: u64,
    interval_ms: u64,
    work: u64,
    swap: u64,
    wait: u64,
    cap: u64,
    missed: u64,
) -> Vec<PacingTraceRow> {
    (0..n)
        .map(|i| PacingTraceRow {
            start_ns: 1_000 * MS + i * interval_ms * MS,
            cost_ns: (work + swap) * MS,
            swapchain_wait_ns: swap * MS,
            waited_ns: wait * MS,
            cap_waited_ns: cap * MS,
            ticks: interval_ms * MS / PERIOD - missed,
            missed_ticks: missed,
            tick_gap_max_ns: (missed + 1) * PERIOD,
            period_ns: Some(PERIOD),
            interval: 1,
            paced: wait > 0,
            ..PacingTraceRow::default()
        })
        .collect()
}

#[test]
fn the_breakdown_names_what_held_the_frames_back() {
    use regia_platform::frame_pacing::{PacingLimit, pacing_breakdown};
    assert_eq!(pacing_breakdown(&[]), None);
    assert_eq!(
        pacing_breakdown(&rows(1, 64, 30, 2, 0, 0, 0)),
        None,
        "one row has no interval"
    );
    let cases = [
        // A slow frame: the work fills the interval.
        (rows(9, 64, 50, 4, 10, 0, 0), PacingLimit::Work),
        // A full display queue: acquire and present block.
        (rows(9, 64, 10, 40, 14, 0, 0), PacingLimit::Swapchain),
        // `Auto` holding a slot longer than the frames need.
        (rows(9, 64, 12, 2, 50, 0, 0), PacingLimit::Wait),
        // The software cap sleeping off a frame the display did not pace.
        (rows(9, 64, 12, 2, 0, 40, 0), PacingLimit::Cap),
        // Time outside the frame: the event loop between frames.
        (rows(9, 64, 12, 2, 0, 0, 0), PacingLimit::Loop),
        // The ticks themselves: 2 of every 4 vsyncs never arrived.
        (rows(9, 64, 12, 2, 50, 0, 2), PacingLimit::Ticks),
    ];
    for (trace, want) in cases {
        let split = pacing_breakdown(&trace).expect("eight intervals");
        assert_eq!(split.limit, want, "{split:?}");
        assert_eq!(split.intervals, 8);
        assert_eq!(split.interval_p50_ns, 64 * MS);
        assert_eq!(split.vsyncs_per_frame, Some(4.0));
        let sum: f64 = split.shares.iter().sum();
        assert!((sum - 1.0).abs() < 1e-9, "shares add up: {split:?}");
    }
    let ticks = pacing_breakdown(&rows(9, 64, 12, 2, 50, 0, 2)).expect("eight intervals");
    assert_eq!((ticks.ticks, ticks.missed_ticks), (16, 16));
    assert_eq!(ticks.tick_gap_max_ns, 3 * PERIOD);
    // One skipped vsync in 32 is jitter, not the limit.
    let mut jitter = rows(9, 64, 12, 2, 50, 0, 0);
    jitter[4].missed_ticks = 1;
    let split = pacing_breakdown(&jitter).expect("eight intervals");
    assert_eq!(split.limit, PacingLimit::Wait, "{split:?}");
    assert_eq!(split.loop_p50_ns, 0);
    assert_eq!(split.wait_p50_ns, 50 * MS);
    assert_eq!(PacingLimit::Ticks.name(), "ticks");
}

// ---- A frame late for its slot (ADR-4913) ----

/// A 60 Hz model releasing three-vsync slots: released on the tick at 8 periods, then a frame
/// that ran `work_ms` while ticks 9..=11 arrived. Returns where the next frame may start when the
/// frame ended `after_ns` past the tick at 11 periods (its slot) or, negative, before it.
fn plan_after(late_start: bool, last_tick: u64, end_ns: u64) -> Option<u64> {
    const P60: u64 = 16_666_667;
    let mut model = regia_platform::frame_pacing::PacingModel::new();
    model.set_late_start(late_start);
    for i in 1..=8 {
        model.on_tick(i * P60);
    }
    model.end_frame(45 * MS, None, Some(3));
    model.released(8 * P60);
    for i in 9..=last_tick {
        model.on_tick(i * P60);
    }
    model.end_frame(53 * MS, None, Some(3));
    model.plan(end_ns)
}

#[test]
fn a_frame_that_overran_its_slot_starts_on_the_missed_tick() {
    const P60: u64 = 16_666_667;
    // The TEST Pixel 9 Pro trace: work 53 ms against a 50 ms slot, ended 3.3 ms after the slot's
    // tick. Waiting for the next tick idles 13 ms and shows the frame after 66.7 ms.
    let end = 11 * P60 + 3_300_000;
    assert_eq!(
        plan_after(false, 11, end),
        Some(11 * P60 + P60 / 2),
        "the old wait"
    );
    assert_eq!(plan_after(true, 11, end), Some(11 * P60), "starts at once");
    // A frame that ends before its slot still waits for the slot.
    let early = 10 * P60 + MS;
    assert_eq!(plan_after(true, 10, early), plan_after(false, 10, early));
    assert_eq!(plan_after(true, 10, early), Some(11 * P60 - P60 / 2));
}

#[test]
fn a_frame_started_on_its_missed_tick_still_counts_as_late() {
    const P60: u64 = 16_666_667;
    let run = |late_start: bool, end_ns: u64| {
        let mut model = regia_platform::frame_pacing::PacingModel::new();
        model.set_late_start(late_start);
        for i in 1..=8 {
            model.on_tick(i * P60);
        }
        model.end_frame(45 * MS, None, Some(3));
        let _ = model.plan(8 * P60);
        model.released(8 * P60);
        let last = end_ns / P60;
        for i in 9..=last {
            model.on_tick(i * P60);
        }
        model.end_frame(53 * MS, None, Some(3));
        let bound = model.plan(end_ns).expect("a period");
        // The tick that releases the frame: the bound itself, or the next tick after it.
        let tick = bound.div_ceil(P60) * P60;
        model.released(tick);
        (tick, model.stats().late)
    };
    let late_end = 11 * P60 + 3_300_000;
    // Old wait: released a vsync after the slot, late. Started at once: on the slot's tick, still late.
    assert_eq!(run(false, late_end), (12 * P60, 1));
    assert_eq!(run(true, late_end), (11 * P60, 1));
    // A frame that ended before its slot is on time either way.
    let on_time = 10 * P60 + MS;
    assert_eq!(run(true, on_time), (11 * P60, 0));
    assert_eq!(run(false, on_time), (11 * P60, 0));
}
