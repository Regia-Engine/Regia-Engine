// SPDX-License-Identifier: MPL-2.0
//! Acceptance tests for the game-owned rollback log (spec 120 § 9.5, ADR-7953).
//!
//! A deterministic integer body (position + velocity) is the toy step. Nothing here reads a
//! wall clock or sleeps.

use regia_net::rollback::{RollbackError, RollbackLog, VisualOffset};
use regia_net::{PeerId, TickInputs};

/// Integer body. `step` adds the peer's acceleration to velocity, then velocity to position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Body {
    pos: i64,
    vel: i64,
}

fn inputs(accel: i64) -> TickInputs<i64> {
    let mut map = TickInputs::new();
    map.insert(PeerId(1), accel);
    map
}

fn step(body: &mut Body, _tick: u64, frame: &TickInputs<i64>) {
    let accel = frame.get(&PeerId(1)).copied().unwrap_or(0);
    body.vel += accel;
    body.pos += body.vel;
}

/// The same step, outside the log: the reference trace.
fn reference(mut body: Body, from_exclusive: u64, to: u64, accel_at: impl Fn(u64) -> i64) -> Body {
    let mut tick = from_exclusive.saturating_add(1);
    while tick <= to {
        step(&mut body, tick, &inputs(accel_at(tick)));
        tick = tick.saturating_add(1);
    }
    body
}

fn record_run(log: &mut RollbackLog<Body, i64>, from: u64, to: u64, accel: i64, mut body: Body) -> Body {
    let mut tick = from;
    while tick <= to {
        step(&mut body, tick, &inputs(accel));
        log.record(tick, body, inputs(accel));
        tick = tick.saturating_add(1);
    }
    body
}

fn stored(log: &RollbackLog<Body, i64>) -> Vec<(u64, Body, i64)> {
    log.ticks()
        .map(|tick| {
            let body = *log.state(tick).expect("stored state");
            let accel = log
                .inputs(tick)
                .expect("stored inputs")
                .get(&PeerId(1))
                .copied()
                .unwrap_or(0);
            (tick, body, accel)
        })
        .collect()
}

#[test]
fn toy_step_reconciles_to_the_reference_after_a_one_tick_server_correction() {
    let mut log = RollbackLog::new(16);
    let predicted = record_run(&mut log, 1, 3, 1, Body { pos: 0, vel: 0 });
    assert_eq!(predicted, Body { pos: 6, vel: 3 });

    // The server disagrees about the state after tick 2 only. Replay tick 3 from that state.
    let server = Body { pos: 4, vel: 2 };
    let mut calls = 0u64;
    let newest = log
        .reconcile(2, server, |body, tick, frame| {
            calls += 1;
            assert_eq!(tick, 3, "a one-tick correction replays the single newer tick");
            step(body, tick, frame);
        })
        .expect("tick 2 is inside the ring");
    let expected = reference(server, 2, 3, |_| 1);
    assert_eq!(*newest, expected);
    assert_eq!(log.state(2).copied(), Some(server));
    assert_eq!(log.state(3).copied(), Some(expected));
    assert_eq!(log.state(1).copied(), Some(Body { pos: 1, vel: 1 }));
    assert_eq!(calls, 1);
}

#[test]
fn correction_older_than_the_ring_returns_rollback_too_old_and_changes_nothing() {
    let mut log = RollbackLog::new(3);
    record_run(&mut log, 1, 5, 1, Body { pos: 0, vel: 0 });
    assert_eq!(log.ticks().collect::<Vec<_>>(), vec![3, 4, 5]);
    let before = stored(&log);
    let before_counters = log.counters();

    let mut calls = 0u64;
    let err = log
        .reconcile(2, Body { pos: 99, vel: 99 }, |_, _, _| calls += 1)
        .expect_err("tick 2 has fallen out of a 3-tick ring");
    assert_eq!(err.code(), "rollback_too_old");
    assert!(matches!(
        err,
        RollbackError::RollbackTooOld {
            tick: 2,
            oldest: Some(3)
        }
    ));
    assert_eq!(calls, 0);
    assert_eq!(
        stored(&log),
        before,
        "a too-old correction writes no snapshot and no input"
    );

    let amend = log
        .amend_inputs(2, inputs(50))
        .expect_err("amend of an evicted tick");
    assert_eq!(amend.code(), "rollback_too_old");
    assert_eq!(stored(&log), before);

    let mut empty: RollbackLog<Body, i64> = RollbackLog::new(4);
    let empty_err = empty
        .reconcile(0, Body { pos: 0, vel: 0 }, |_, _, _| calls += 1)
        .expect_err("empty ring");
    assert_eq!(empty_err.code(), "rollback_too_old");
    assert!(empty.is_empty());
    assert_eq!(calls, 0);

    assert_eq!(log.counters().too_old, before_counters.too_old + 2);
}

#[test]
fn replay_calls_the_step_exactly_newest_minus_server_tick_times() {
    let mut log = RollbackLog::new(32);
    record_run(&mut log, 1, 10, 2, Body { pos: 0, vel: 0 });
    let server_tick = 6u64;
    let newest = 10u64;
    let server = Body { pos: 100, vel: -3 };
    let mut calls = Vec::new();
    let got = log
        .reconcile(server_tick, server, |body, tick, frame| {
            calls.push(tick);
            step(body, tick, frame);
        })
        .expect("server tick is stored");
    assert_eq!(calls.len() as u64, newest - server_tick);
    assert_eq!(calls, vec![7, 8, 9, 10]);
    assert_eq!(*got, reference(server, server_tick, newest, |_| 2));
    assert_eq!(log.counters().replays, 1);
    assert_eq!(log.counters().max_depth, newest - server_tick);
}

#[test]
fn visual_offset_reaches_zero_after_n_ticks_and_is_monotonic() {
    for (delta, span) in [(10i64, 5u32), (-9, 4), (1, 8), (0, 3), (7, 0)] {
        let mut offset = VisualOffset::new(delta, span);
        let mut seen = vec![offset.value()];
        let steps = if span == 0 { 1 } else { span };
        for _ in 0..steps {
            offset.advance();
            seen.push(offset.value());
        }
        assert_eq!(
            *seen.last().expect("at least the start"),
            0,
            "delta {delta} span {span}: {seen:?}"
        );
        for pair in seen.windows(2) {
            assert!(
                pair[1].unsigned_abs() <= pair[0].unsigned_abs(),
                "not monotonic for delta {delta} span {span}: {seen:?}"
            );
        }
        offset.advance();
        assert_eq!(offset.value(), 0, "stays zero");
    }

    let hide = VisualOffset::from_correction(15, 4, 3);
    assert_eq!(
        hide.apply(4),
        15,
        "the first presented position is the predicted one"
    );
    let mut hide = hide;
    hide.advance();
    hide.advance();
    hide.advance();
    assert_eq!(
        hide.apply(4),
        4,
        "after N ticks the presented position is the corrected one"
    );
    assert!(hide.is_zero());
}

#[test]
fn a_late_input_amended_for_a_past_tick_is_used_in_the_corrected_run() {
    let mut log = RollbackLog::new(16);
    record_run(&mut log, 1, 5, 1, Body { pos: 0, vel: 0 });
    log.amend_inputs(4, inputs(7))
        .expect("tick 4 is still in the ring");

    let server_tick = 3u64;
    let server = log.state(server_tick).copied().expect("tick 3 was recorded");
    let mut seen = Vec::new();
    let got = log
        .reconcile(server_tick, server, |body, tick, frame| {
            seen.push((tick, frame.get(&PeerId(1)).copied().unwrap_or(0)));
            step(body, tick, frame);
        })
        .expect("reconcile");
    assert_eq!(
        seen,
        vec![(4, 7), (5, 1)],
        "the amended tick 4 input is the one replayed"
    );
    let expected = reference(server, 3, 5, |tick| if tick == 4 { 7 } else { 1 });
    assert_eq!(*got, expected);
    assert_eq!(log.inputs(4).and_then(|m| m.get(&PeerId(1)).copied()), Some(7));
}

#[test]
fn the_ring_never_grows_past_its_capacity() {
    let mut log = RollbackLog::<Body, i64>::new(8);
    for tick in 0..100u64 {
        log.record(
            tick,
            Body {
                pos: i64::try_from(tick).unwrap_or(0),
                vel: 0,
            },
            inputs(0),
        );
        assert!(log.len() <= 8);
        let state_bytes = log.len() * std::mem::size_of::<Body>();
        let bound = usize::try_from(log.capacity_ticks()).unwrap_or(usize::MAX) * std::mem::size_of::<Body>();
        assert!(state_bytes <= bound);
    }
    assert_eq!(log.len(), 8);
    assert_eq!(log.oldest_tick(), Some(92));
    assert_eq!(log.newest_tick(), Some(99));
    assert_eq!(RollbackLog::<Body, i64>::new(0).capacity_ticks(), 0);
    let mut none = RollbackLog::<Body, i64>::new(0);
    none.record(1, Body { pos: 1, vel: 0 }, inputs(1));
    assert!(none.is_empty(), "a zero capacity retains nothing");
}

#[test]
fn a_missing_tick_returns_rollback_not_recorded_and_writes_nothing() {
    let mut log = RollbackLog::new(16);
    record_run(&mut log, 1, 3, 1, Body { pos: 0, vel: 0 });
    log.record(5, Body { pos: 9, vel: 1 }, inputs(1));
    let before = stored(&log);
    let before_counters = log.counters();
    let mut calls = 0u64;

    let ahead = log
        .reconcile(4, Body { pos: 1, vel: 1 }, |_, _, _| calls += 1)
        .expect_err("tick 4 was never stored");
    assert_eq!(ahead.code(), "rollback_not_recorded");

    let gap = log
        .reconcile(3, Body { pos: 1, vel: 1 }, |_, _, _| calls += 1)
        .expect_err("tick 4 is a gap before the newest tick");
    assert_eq!(gap.code(), "rollback_not_recorded");
    assert!(matches!(gap, RollbackError::NotRecorded { tick: 4 }));
    assert_eq!(calls, 0);
    assert_eq!(stored(&log), before);
    assert_eq!(log.counters(), before_counters);
}

#[test]
fn correcting_only_the_newest_tick_is_not_a_replay() {
    let mut log = RollbackLog::new(8);
    record_run(&mut log, 1, 4, 1, Body { pos: 0, vel: 0 });
    let corrected = Body { pos: 20, vel: 5 };
    let mut calls = 0u64;
    let newest = log
        .reconcile(4, corrected, |_, _, _| calls += 1)
        .expect("the newest tick is stored");
    assert_eq!(calls, 0);
    assert_eq!(*newest, corrected);
    assert_eq!(log.counters().replays, 0);
    assert_eq!(log.counters().max_depth, 0);
}
