// SPDX-License-Identifier: MPL-2.0
//! Steady frame starts (ADR-5946): a frame paced on the display starts when its tick is due on the
//! grid of the vsync ticks, not when the tick's callback happens to run.
//!
//! The TEST Pixel 9 Pro behind it: Choreographer delivered each 60 Hz tick 0.3 to 1.0 ms after the
//! tick's own time (now and then 1.3 to 2.4 ms), every frame started when its callback was seen,
//! and so two frame starts in a row were up to 0.7 ms further apart than their ticks. The p99 of
//! the frame interval sat at 1.03x the refresh with nothing wrong, and a handful of late wake-ups
//! or held frames in one 8 s window took it past the 1.05x limit of `presented_p99` (A13).
//!
//! The tests run a phone on virtual time: reading the pacer's clock moves the time on and
//! delivers the ticks that are due, so the pacer's own poll drives the time and no assertion
//! depends on the wall clock.

use std::cell::RefCell;
use std::sync::Arc;
use std::time::Duration;

use regia_platform::frame_pacing::{
    DEFAULT_WAKE_AHEAD_NS, DisplayPacer, FramePacingSettings, PacingCost, PacingHint, PacingMode,
    PacingModel, PacingSource, PacingTraceRow, VsyncTicks,
};

/// What ADR-5946 added to the API. Kept apart so that the tests of this file build against
/// ADR-5461's API alone and show the old behavior failing them.
#[path = "steady_start/new_surface.rs"]
mod new_surface;

const US: u64 = 1_000;
const MS: u64 = 1_000_000;
/// The stand-in panel: 60 Hz.
const PERIOD: u64 = 16_666_667;
/// What one read of the clock moves the virtual time on by.
const STEP: u64 = 2 * US;
/// A frame's work: it ends inside the wake-ahead of its release tick, so the pacer goes straight
/// to its poll.
const WORK: u64 = 14_300 * US;

/// The vsync source and the clock of a phone on virtual time.
struct Phone {
    now: u64,
    ticks: Arc<VsyncTicks>,
    /// The index of the next tick to deliver.
    next: u64,
    /// How long after its own time tick `k` reaches the vsync thread.
    delivery: fn(u64) -> u64,
    /// Ticks from this index on come this much later: the compositor moved its grid.
    shift: Option<(u64, u64)>,
    /// No tick after this index: the display went off.
    last: Option<u64>,
}

impl Phone {
    fn tick_time(&self, k: u64) -> u64 {
        k * PERIOD + self.shift.filter(|(from, _)| k >= *from).map_or(0, |(_, by)| by)
    }

    /// Delivers every tick whose callback has run by now.
    fn deliver(&mut self) {
        while self.last.is_none_or(|last| self.next <= last) {
            let tick = self.tick_time(self.next);
            let at = tick + (self.delivery)(self.next);
            if at > self.now {
                break;
            }
            self.ticks.tick_delivered(tick, at);
            self.next += 1;
        }
    }
}

thread_local! {
    /// The phone of the test on this thread.
    static PHONE: RefCell<Option<Phone>> = const { RefCell::new(None) };
}

fn with_phone<R>(f: impl FnOnce(&mut Phone) -> R) -> R {
    PHONE.with_borrow_mut(|phone| f(phone.as_mut().expect("a phone on this thread")))
}

/// The pacer's clock: every read is `STEP` later, and the ticks due by then have arrived.
fn clock() -> u64 {
    with_phone(|phone| {
        phone.now += STEP;
        phone.deliver();
        phone.now
    })
}

/// The frame's work: the time moves to `to` with the ticks on the way delivered.
fn work_until(to: u64) {
    with_phone(|phone| {
        phone.now = phone.now.max(to);
        phone.deliver();
    });
}

fn settings() -> FramePacingSettings {
    FramePacingSettings {
        mode: PacingMode::Auto,
        cost: PacingCost::Work,
        ..Default::default()
    }
}

/// A pacer with the wake-ahead on a phone whose first eight ticks have arrived.
fn phone(delivery: fn(u64) -> u64, shift: Option<(u64, u64)>, last: Option<u64>) -> DisplayPacer {
    let ticks = VsyncTicks::new();
    PHONE.set(Some(Phone {
        now: 0,
        ticks: ticks.clone(),
        next: 1,
        delivery,
        shift,
        last,
    }));
    work_until(8 * PERIOD + 3 * MS);
    let mut pacer = DisplayPacer::new(ticks, PacingSource::Choreographer, clock);
    pacer.set_wake_ahead(Some(DEFAULT_WAKE_AHEAD_NS));
    // The app's first frames start wherever the launch left them; three short ones put the
    // pacer on its slot, so a test's first frame starts on a tick like every later one.
    let settled = run(&mut pacer, 3, |_| 5 * MS);
    assert_eq!(settled.len(), 3);
    pacer
}

/// Runs `frames` frames of `work(i)` each and returns, per frame, when the pacer released the next
/// one (its start) and the pacer's row. A frame the pacer did not release ends the run.
fn run(pacer: &mut DisplayPacer, frames: usize, work: impl Fn(usize) -> u64) -> Vec<(u64, PacingTraceRow)> {
    let mut out = Vec::with_capacity(frames);
    let mut start = with_phone(|phone| phone.now);
    for i in 0..frames {
        let cost = work(i);
        work_until(start + cost);
        let cost = Duration::from_nanos(cost);
        let waited = pacer.pace_frame(cost, Duration::ZERO, None, settings());
        let row = pacer.trace_row(cost, Duration::ZERO, waited);
        let (Some(woke), Some(_)) = (row.woke_ns, row.release_tick_ns) else {
            out.push((0, row));
            break;
        };
        start = woke;
        out.push((woke, row));
    }
    out
}

/// The intervals between the frame starts of `rows`, from frame `skip` on.
fn intervals(rows: &[(u64, PacingTraceRow)], skip: usize) -> Vec<u64> {
    rows[skip..].windows(2).map(|w| w[1].0 - w[0].0).collect()
}

/// The Pixel's delivery: 0.3 to 1.0 ms after the tick, and every 37th callback 2.4 ms late.
fn pixel_delivery(k: u64) -> u64 {
    if k.is_multiple_of(37) {
        return 2_400 * US;
    }
    300 * US + (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 40) % (700 * US)
}

fn early_delivery(_k: u64) -> u64 {
    100 * US
}

fn steady_delivery(_k: u64) -> u64 {
    500 * US
}

/// The regression test of ADR-5946. It uses nothing newer than ADR-5461's API, and on that code
/// the frame starts follow the callbacks: intervals up to 0.7 ms longer than the period.
#[test]
fn frame_starts_are_one_period_apart_whatever_the_callbacks_do() {
    let mut pacer = phone(pixel_delivery, None, None);
    let rows = run(&mut pacer, 600, |_| WORK);
    assert_eq!(rows.len(), 600);
    let gaps = intervals(&rows, 0);
    let longest = gaps.iter().copied().max().expect("frames");
    let shortest = gaps.iter().copied().min().expect("frames");
    assert!(
        longest <= PERIOD + 100 * US && shortest + 100 * US >= PERIOD,
        "frame starts follow the tick grid: shortest {shortest} ns, longest {longest} ns, period {PERIOD} ns"
    );
    // The bar of A13: the p99 of the interval within 1.05 periods. Nearest rank, as the hitch report.
    let mut sorted = gaps.clone();
    sorted.sort_unstable();
    let p99 = sorted[((sorted.len() - 1) as f64 * 0.99).round() as usize];
    assert!(
        p99 * 1000 <= PERIOD * 1005,
        "p99 {p99} ns is within 1.005 periods"
    );
    // No frame starts before its tick: the display's timeline still leads.
    for (start, row) in &rows {
        let tick = row.release_tick_ns.expect("released");
        assert!(*start >= tick, "start {start} before its tick {tick}");
        assert!(
            *start <= tick + 2 * MS,
            "start {start} long after its tick {tick}"
        );
    }
}

/// The same phone with frames that end early, so the pacer sleeps before it polls (a real
/// sleep of a few milliseconds per frame: fewer frames).
#[test]
fn frames_that_end_early_start_on_the_grid_too() {
    let mut pacer = phone(pixel_delivery, None, None);
    let rows = run(&mut pacer, 40, |i| if i % 2 == 0 { 9 * MS } else { WORK });
    assert_eq!(rows.len(), 40);
    for gap in intervals(&rows, 0) {
        assert!(gap.abs_diff(PERIOD) <= 100 * US, "interval {gap} ns");
    }
}

#[test]
fn a_tick_that_arrives_early_waits_for_its_due_time() {
    // Callbacks 0.1 ms after the tick against a phase that starts at 0.5 ms: the frames do not jump
    // to the callback; the phase walks there a few microseconds per tick.
    let mut pacer = phone(early_delivery, None, None);
    let rows = run(&mut pacer, 300, |_| WORK);
    for gap in intervals(&rows, 0) {
        assert!(gap.abs_diff(PERIOD) <= 20 * US, "interval {gap} ns");
    }
    let phase = |i: usize| rows[i].0 - rows[i].1.release_tick_ns.expect("released");
    assert!(
        phase(0) >= 400 * US,
        "the first frames keep the default phase: {}",
        phase(0)
    );
    assert!(phase(299) < phase(0), "and it follows the phone's delivery");
    // While the phase is well past the callbacks, each tick is there before its frame starts: the
    // frame waits for the due time with the tick in hand, and the row has the tick's stamp.
    for (start, row) in &rows[..20] {
        let tick = row.release_tick_ns.expect("released");
        assert_eq!(row.tick_delivered_ns, Some(tick + 100 * US), "{row:?}");
        assert!(*start >= tick + 300 * US, "{row:?}");
    }
    // Later the phase sits on the callbacks, and no frame starts before its tick.
    for (start, row) in &rows {
        assert!(*start >= row.release_tick_ns.expect("released"), "{row:?}");
    }
}

/// The Pixel's panel runs at 120 Hz and gives the app every other vsync; a late present moves the
/// app's ticks by one panel vsync. Ticks from 100 on come `by` later. Returns the frame intervals.
fn intervals_across_a_moved_grid(delivery: fn(u64) -> u64, by: u64) -> Vec<u64> {
    let mut pacer = phone(delivery, Some((100, by)), None);
    let rows = run(&mut pacer, 300, |_| WORK);
    assert_eq!(rows.len(), 300);
    let gaps = intervals(&rows, 0);
    // Every frame after the move is released on a tick of the new grid.
    let after = gaps
        .iter()
        .rposition(|gap| gap.abs_diff(PERIOD) > 100 * US)
        .expect("the move shows");
    for (_, row) in &rows[after + 2..] {
        let tick = row.release_tick_ns.expect("released");
        assert_eq!((tick - by) % PERIOD, 0, "tick {tick} is on the moved grid");
    }
    gaps
}

/// The second regression test of ADR-5946, also on ADR-5461's API alone. Half a period is exactly
/// the tolerance a slot's tick gets, so on the old code a move of half a period, or a few
/// microseconds less, started the next frame at once: two short intervals, one frame too many
/// between two latches, and a display queue one image fuller from then on.
#[test]
fn a_moved_tick_grid_costs_one_long_interval_and_no_extra_frame() {
    for by in [PERIOD / 2 - 30 * US, PERIOD / 2, PERIOD / 2 + 30 * US] {
        let gaps = intervals_across_a_moved_grid(pixel_delivery, by);
        let off: Vec<u64> = gaps
            .iter()
            .copied()
            .filter(|gap| gap.abs_diff(PERIOD) > 100 * US)
            .collect();
        assert_eq!(
            off.len(),
            1,
            "moved by {by} ns: one interval takes the move, got {off:?}"
        );
        assert!(
            off[0].abs_diff(PERIOD + by) <= 100 * US,
            "moved by {by} ns: it is the period plus the move, got {}",
            off[0]
        );
        // As many frames as the display had slots for: 300 frames span 299 periods and the move.
        let span: u64 = gaps.iter().sum();
        assert!(
            span.abs_diff(299 * PERIOD + by) <= 2 * MS,
            "moved by {by} ns: span {span}"
        );
    }
}

/// The same move on a phone whose callbacks all come 0.5 ms after their tick, so nothing but the move
/// shows: a grid that moves a few microseconds more than half a period put the moved tick inside the
/// old slot's tolerance, and the old code started the next frame at once on it. Frame after frame then
/// started at once until the starts were back on a tick: one frame more than the display had slots.
#[test]
fn a_grid_moved_just_past_half_a_period_does_not_squeeze_a_frame_in() {
    let by = PERIOD / 2 + 30 * US;
    let gaps = intervals_across_a_moved_grid(steady_delivery, by);
    let short: Vec<u64> = gaps
        .iter()
        .copied()
        .filter(|gap| *gap + 100 * US < PERIOD)
        .collect();
    assert!(short.is_empty(), "no frame starts early: {short:?}");
    let span: u64 = gaps.iter().sum();
    assert!(
        span.abs_diff(299 * PERIOD + by) <= MS,
        "300 frames take 299 periods and the move, not a frame less: {span} ns"
    );
}

/// 60 Hz ticks as the phone times them: a few tens of microseconds early and late in turn, so the
/// model's period estimate (the lower quartile of the tick intervals) reads a little short, as it
/// did on the TEST Pixel 9 Pro (16.59-16.65 ms at the change of rate).
fn phone_ticks_at_60(model: &mut PacingModel, mut tick: u64, count: u32) -> u64 {
    for k in 0..count {
        tick += if k % 2 == 0 {
            PERIOD - 40_000
        } else {
            PERIOD + 40_000
        };
        model.on_tick(tick);
    }
    tick
}

/// What a frame of the arena cost on the TEST Pixel 9 Pro: 9 ms at one 60 Hz tick a frame, and
/// 14 to 18.6 ms at two (p50 15.0, p90 16.9, p99 18.6; the phone's clocks fall when paced frames
/// leave it idle, ADR-2339).
fn phone_cost(interval: u32, frame: usize) -> u64 {
    const AT_30_FPS: [u64; 8] = [
        15_000_000, 16_900_000, 14_500_000, 15_500_000, 17_200_000, 14_000_000, 15_100_000, 18_600_000,
    ];
    if interval <= 1 {
        9 * MS
    } else {
        AT_30_FPS[frame % AT_30_FPS.len()]
    }
}

/// The third regression test of ADR-5946, on the model alone and on ADR-2339's API. The TEST
/// Pixel 9 Pro gives a new process 120 Hz ticks for its first second or two and 60 Hz ticks after
/// that. A stall in the first frames raised the slot to three of the fast ticks (25 ms), the change
/// of rate kept that slot as two of the slow ticks (33 ms; 25 ms is half way between one and two,
/// and the estimate reading a little short tipped it to two), and at 30 fps the phone's frames cost
/// 15 ms instead of 9: too much to count as fitting one tick, too little for the model's probe.
/// The app then stayed at 30 fps for minutes, after more than half of its cold starts.
#[test]
fn a_slot_between_two_of_the_new_ticks_takes_the_shorter_one() {
    let fast = PERIOD / 2;
    let mut model = PacingModel::new();
    // The Android default (ADR-2339): three ticks have no divisor to hint, so no hint is out
    // and the change of rate is taken as the panel's own.
    model.set_hint(PacingHint::Double);
    let mut tick = 0;
    for _ in 0..8 {
        tick += fast;
        model.on_tick(tick);
    }
    // Four start-up frames of 17 ms on 8.3 ms ticks: the slot rises to three ticks.
    for _ in 0..4 {
        model.end_frame(17 * MS, None, None);
    }
    assert_eq!(model.swap_interval(), 3);
    // The ticks slow to 60 Hz, and the frames cost what the phone's did.
    phone_ticks_at_60(&mut model, tick, 32);
    assert_eq!(model.stats().refresh_changes, 1);
    let mut at_30_fps = 0;
    for frame in 0..1_200 {
        let interval = model.swap_interval();
        at_30_fps += usize::from(interval > 1);
        model.end_frame(phone_cost(interval, frame), None, None);
    }
    assert_eq!(
        (model.swap_interval(), at_30_fps),
        (1, 0),
        "the frames fit one tick, so no frame waits two"
    );
    assert_eq!(model.stats().probes, 1, "the shorter slot is taken on trial");

    // Frames that do need more than a tick go back to the two ticks the old slot rounds to.
    let mut model = PacingModel::new();
    model.set_hint(PacingHint::Double);
    let mut tick = 0;
    for _ in 0..8 {
        tick += fast;
        model.on_tick(tick);
    }
    for _ in 0..4 {
        model.end_frame(22 * MS, None, None);
    }
    assert_eq!(model.swap_interval(), 3);
    phone_ticks_at_60(&mut model, tick, 32);
    assert_eq!(model.swap_interval(), 1, "on trial");
    for _ in 0..200 {
        model.end_frame(22 * MS, None, None);
    }
    assert_eq!(model.swap_interval(), 2, "22 ms frames get their 33 ms back");
    for _ in 0..2_000 {
        model.end_frame(22 * MS, None, None);
    }
    assert_eq!(model.swap_interval(), 2, "and keep it");
    assert_eq!(model.stats().probes, 1, "one trial, not one every hold");
}

/// A model on 120 Hz ticks whose first four frames cost `stall` each.
fn model_after_a_stall_at_120(stall: u64) -> (PacingModel, u64) {
    let mut model = PacingModel::new();
    model.set_hint(PacingHint::Double);
    let mut tick = 0;
    for _ in 0..8 {
        tick += PERIOD / 2;
        model.on_tick(tick);
    }
    for _ in 0..4 {
        model.end_frame(stall, None, None);
    }
    (model, tick)
}

/// What the trial at a change of rate leaves alone (this holds on ADR-5461's code too): a slot that
/// is a whole count of the new ticks is kept without a trial, whether the period estimate reads a
/// little short (the phone's ticks) or a little long (a 59.9 Hz panel). Two ticks of 120 Hz are one
/// of 60 Hz, four are two.
#[test]
fn a_slot_that_is_a_whole_count_of_the_new_ticks_is_kept_without_a_trial() {
    for (stall, fast_ticks, slow_ticks) in [(12 * MS, 2, 1), (30 * MS, 4, 2)] {
        for long in [false, true] {
            let (mut model, mut tick) = model_after_a_stall_at_120(stall);
            assert_eq!(model.swap_interval(), fast_ticks, "{stall} ns frames");
            if long {
                for _ in 0..32 {
                    tick += PERIOD + 30_000;
                    model.on_tick(tick);
                }
            } else {
                phone_ticks_at_60(&mut model, tick, 32);
            }
            assert_eq!(model.stats().refresh_changes, 1);
            assert_eq!(
                (model.swap_interval(), model.stats().probes),
                (slow_ticks, 0),
                "{fast_ticks} ticks of 120 Hz, estimate long: {long}"
            );
            for _ in 0..300 {
                model.end_frame(stall, None, None);
            }
            assert_eq!(
                (model.swap_interval(), model.stats().probes),
                (slow_ticks, 0),
                "{fast_ticks} ticks of 120 Hz, estimate long: {long}: the frames keep their slot"
            );
        }
    }
}

/// The trial is a probe, so a model that does not probe (`cost: frame`) rounds the slot to the
/// nearer count of the new ticks as it always did: the phone's start-up ends at two ticks, and its
/// 15 ms frames stay there (this holds on ADR-5461's code too).
#[test]
fn a_model_that_does_not_probe_keeps_the_nearer_slot() {
    let (mut model, tick) = model_after_a_stall_at_120(17 * MS);
    model.set_probe(false);
    assert_eq!(model.swap_interval(), 3);
    phone_ticks_at_60(&mut model, tick, 32);
    assert_eq!(model.stats().refresh_changes, 1);
    assert_eq!(model.swap_interval(), 2, "25 ms is nearer two ticks of 16.6 ms");
    for frame in 0..1_200 {
        model.end_frame(phone_cost(model.swap_interval(), frame), None, None);
    }
    assert_eq!((model.swap_interval(), model.stats().probes), (2, 0));
}

#[test]
fn a_display_that_stops_ticking_stops_the_frames() {
    // No tick after the 40th: one frame may start on the tick that was due next, and then the
    // frames wait for a tick that does not come (three timed-out waits of two periods each, on
    // the wall clock) and go back to the software cap.
    let mut pacer = phone(pixel_delivery, None, Some(40));
    let rows = run(&mut pacer, 60, |_| WORK);
    let released = rows
        .iter()
        .filter(|(_, row)| row.release_tick_ns.is_some())
        .count();
    let last_tick = 40 * PERIOD;
    let past: Vec<u64> = rows
        .iter()
        .filter_map(|(_, row)| row.release_tick_ns)
        .filter(|tick| *tick > last_tick)
        .collect();
    assert_eq!(
        past,
        vec![41 * PERIOD],
        "one predicted tick past the last real one"
    );
    assert_eq!(rows.len(), released + 1, "the next frame is not released");
    let cost = Duration::from_nanos(WORK);
    let mut unpaced = 0;
    for _ in 0..4 {
        unpaced += u32::from(pacer.pace_frame(cost, Duration::ZERO, None, settings()).is_none());
    }
    assert!(unpaced >= 1, "after the stall the software cap paces the frames");
}
