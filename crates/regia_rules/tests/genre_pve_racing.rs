// SPDX-License-Identifier: MPL-2.0
//! Co-op waves and races on a hosted session (audit gaps GR-11.1 and GR-11.2, ADR-8695).

use regia_rules::match_flow::{FlowInput, SeatId};
use regia_rules::objective::{Contestant, EndReason, HillVolume, RulesEvent};
use regia_rules::race::{BEST_LAP_COUNTER, Checkpoint, RaceSettings};
use regia_rules::session::{MatchSession, SessionSettings};
use regia_rules::spawn::{RespawnPolicy, RespawnRules};
use regia_rules::teams::SeatRequest;
use regia_rules::waves::{CoopCause, WaveGroup, WaveRow, WaveSettings, WaveStart};

fn group(archetype: &str, count: u32, tag: &str) -> WaveGroup {
    WaveGroup {
        archetype: archetype.into(),
        count,
        spawn_tag: tag.into(),
    }
}

fn seated(settings: SessionSettings, seats: &[(u8, u8)]) -> MatchSession {
    let mut session = MatchSession::new(settings);
    for (team, slot) in seats {
        session
            .apply(&FlowInput::Seat {
                team: *team,
                slot: *slot,
            })
            .expect("seat");
    }
    session.start_now();
    let _ = session.drain_events();
    session
}

fn wave_events(events: &[RulesEvent]) -> Vec<RulesEvent> {
    events
        .iter()
        .filter(|e| {
            matches!(
                e,
                RulesEvent::WaveStarted { .. }
                    | RulesEvent::WaveSpawn { .. }
                    | RulesEvent::WaveCleared { .. }
                    | RulesEvent::CoopEnded { .. }
                    | RulesEvent::MatchOver { .. }
            )
        })
        .cloned()
        .collect()
}

fn spawn(tick: u64, wave: u32, group: u32, archetype: &str, count: u32, tag: &str) -> RulesEvent {
    RulesEvent::WaveSpawn {
        tick,
        wave,
        group,
        archetype: archetype.into(),
        count,
        spawn_tag: tag.into(),
    }
}

#[test]
fn waves_release_groups_under_the_alive_cap_and_clear_in_order() {
    let settings = SessionSettings {
        waves: WaveSettings {
            waves: vec![
                WaveRow {
                    at: WaveStart::AfterCleared { delay_ticks: 0 },
                    groups: vec![group("grunt", 3, "east"), group("brute", 2, "west")],
                    alive_cap: 4,
                },
                WaveRow {
                    at: WaveStart::AfterCleared { delay_ticks: 5 },
                    groups: vec![group("boss", 1, "arena")],
                    alive_cap: 0,
                },
            ],
            ..WaveSettings::default()
        },
        ..SessionSettings::default()
    };
    let mut session = seated(settings, &[(1, 0)]);
    let first = session.step(&[]);
    let t1 = first.flow.match_tick;
    assert_eq!(
        wave_events(&first.events),
        vec![
            RulesEvent::WaveStarted { tick: t1, wave: 0 },
            spawn(t1, 0, 0, "grunt", 3, "east"),
            spawn(t1, 0, 1, "brute", 1, "west"),
        ],
        "the first group goes out whole, the second up to the cap of 4"
    );
    assert_eq!(session.waves.state.alive_total(), 4);
    // Nothing goes out while the cap is full.
    let held = session.step(&[]);
    assert!(wave_events(&held.events).is_empty(), "{:?}", held.events);

    session.note_wave_down(0, 2);
    let step = session.step(&[]);
    assert_eq!(
        wave_events(&step.events),
        vec![spawn(step.flow.match_tick, 0, 1, "brute", 1, "west")],
        "two down make room; only the one brute left goes out"
    );
    assert_eq!(session.waves.state.alive_total(), 3);

    session.note_wave_down(0, 3);
    let cleared = session.step(&[]);
    let tc = cleared.flow.match_tick;
    assert_eq!(
        wave_events(&cleared.events),
        vec![RulesEvent::WaveCleared { tick: tc, wave: 0 }]
    );
    // Wave 1 waits its five ticks after the clear, then starts.
    let mut started_at = None;
    for _ in 0..10 {
        let step = session.step(&[]);
        for event in wave_events(&step.events) {
            if let RulesEvent::WaveStarted { tick, wave: 1 } = event {
                started_at = Some(tick);
            }
        }
        if started_at.is_some() {
            break;
        }
    }
    assert_eq!(started_at, Some(tc + 5));
    assert_eq!(session.waves.state.progress[1].released, vec![1]);
    assert!(!session.board.match_over, "the boss is still alive");
}

#[test]
fn team_wipe_fails_the_match_and_last_wave_cleared_wins() {
    let one_wave = || WaveSettings {
        waves: vec![WaveRow {
            groups: vec![group("grunt", 2, "")],
            ..WaveRow::default()
        }],
        ..WaveSettings::default()
    };
    // A wipe: both players waiting on a 100-tick respawn at once.
    let mut session = seated(
        SessionSettings {
            waves: one_wave(),
            respawn: RespawnRules {
                default: RespawnPolicy::Delay { ticks: 100 },
                ..RespawnRules::default()
            },
            ..SessionSettings::default()
        },
        &[(1, 0), (1, 1)],
    );
    let _ = session.step(&[]);
    session.note_death(SeatId::new(1, 0));
    let step = session.step(&[]);
    assert!(
        !step
            .events
            .iter()
            .any(|e| matches!(e, RulesEvent::CoopEnded { .. })),
        "one player down is not a wipe"
    );
    session.note_death(SeatId::new(1, 1));
    let step = session.step(&[]);
    let tick = step.flow.match_tick;
    assert_eq!(
        wave_events(&step.events),
        vec![
            RulesEvent::CoopEnded {
                tick,
                won: false,
                cause: CoopCause::TeamWipe,
            },
            RulesEvent::MatchOver {
                tick,
                team: 0,
                reason: EndReason::Ended,
            },
        ]
    );
    assert!(session.board.match_over);

    // An instant respawn is never a wipe, even for a lone player.
    let mut solo = seated(
        SessionSettings {
            waves: one_wave(),
            ..SessionSettings::default()
        },
        &[(1, 0)],
    );
    let _ = solo.step(&[]);
    solo.note_death(SeatId::new(1, 0));
    let step = solo.step(&[]);
    assert!(!solo.board.match_over, "{:?}", step.events);

    // A win: the last wave cleared.
    let mut session = seated(
        SessionSettings {
            waves: one_wave(),
            ..SessionSettings::default()
        },
        &[(1, 0)],
    );
    let _ = session.step(&[]);
    session.note_wave_down(0, 2);
    let step = session.step(&[]);
    let tick = step.flow.match_tick;
    assert_eq!(
        wave_events(&step.events),
        vec![
            RulesEvent::WaveCleared { tick, wave: 0 },
            RulesEvent::CoopEnded {
                tick,
                won: true,
                cause: CoopCause::AllWavesCleared,
            },
            RulesEvent::MatchOver {
                tick,
                team: 1,
                reason: EndReason::Ended,
            },
        ]
    );
}

#[test]
fn shared_lives_pool_eliminates_when_empty() {
    let mut session = seated(
        SessionSettings {
            waves: WaveSettings {
                waves: vec![WaveRow {
                    groups: vec![group("grunt", 50, "")],
                    ..WaveRow::default()
                }],
                shared_lives: Some(2),
                ..WaveSettings::default()
            },
            ..SessionSettings::default()
        },
        &[(1, 0), (1, 1)],
    );
    let _ = session.step(&[]);
    let a = SeatId::new(1, 0);
    let b = SeatId::new(1, 1);
    let respawned = |events: &[RulesEvent], seat: SeatId| {
        events.iter().any(|e| {
            matches!(e, RulesEvent::RespawnAt { team, slot, .. } if *team == seat.team && *slot == seat.slot)
        })
    };
    let eliminated = |events: &[RulesEvent], seat: SeatId| {
        events.iter().any(|e| {
            matches!(e, RulesEvent::Eliminated { team, slot, .. } if *team == seat.team && *slot == seat.slot)
        })
    };

    session.note_death(a);
    let step = session.step(&[]);
    assert!(respawned(&step.events, a), "a life from the pool");
    assert_eq!(session.waves.state.lives_left, Some(1));

    session.note_death(b);
    let step = session.step(&[]);
    assert!(respawned(&step.events, b), "the last life from the pool");
    assert_eq!(session.waves.state.lives_left, Some(0));

    session.note_death(a);
    let mut events = session.drain_events();
    events.extend(session.step(&[]).events);
    assert!(eliminated(&events, a), "the pool is empty: {events:?}");
    assert!(session.respawns.is_eliminated(a));
    assert!(!session.board.match_over, "one player still stands");

    session.note_death(b);
    let mut events = session.drain_events();
    let step = session.step(&[]);
    events.extend(step.events);
    assert!(eliminated(&events, b));
    assert!(
        events.iter().any(|e| matches!(
            e,
            RulesEvent::CoopEnded {
                won: false,
                cause: CoopCause::TeamWipe,
                ..
            }
        )),
        "{events:?}"
    );
    assert!(session.board.match_over);
}

// ---------------------------------------------------------------------------
// Races
// ---------------------------------------------------------------------------

/// A 100 m square: start 10 at the origin, then 20 at +x, 30 at +x+z, 40 at +z, back to 10.
fn square(laps: u32, window: u64) -> RaceSettings {
    let gate = |id: u32, x: f32, z: f32, next: u32| Checkpoint {
        id,
        volume: HillVolume {
            min: [x - 3.0, -1.0, z - 3.0],
            max: [x + 3.0, 3.0, z + 3.0],
        },
        next: vec![next],
    };
    RaceSettings {
        checkpoints: vec![
            gate(10, 0.0, 0.0, 20),
            gate(20, 100.0, 0.0, 30),
            gate(30, 100.0, 100.0, 40),
            gate(40, 0.0, 100.0, 10),
        ],
        laps,
        finish_window_ticks: window,
        ..RaceSettings::default()
    }
}

fn car(id: u32, x: f32, z: f32) -> Contestant {
    Contestant {
        id,
        team: 1,
        position: [x, 0.0, z],
        alive: true,
    }
}

fn race_session(race: RaceSettings, racers: &[(SeatId, u32)]) -> MatchSession {
    let seats: Vec<(u8, u8)> = racers.iter().map(|(s, _)| (s.team, s.slot)).collect();
    let mut session = seated(
        SessionSettings {
            race,
            ..SessionSettings::default()
        },
        &seats,
    );
    for (seat, unit) in racers {
        session.race_enter(*seat, *unit);
    }
    session
}

/// The loop's corners after the start, then the start again.
const LOOP: [(f32, f32); 4] = [(100.0, 0.0), (100.0, 100.0), (0.0, 100.0), (0.0, 0.0)];

#[test]
fn race_ranks_by_lap_checkpoint_then_distance() {
    let a = SeatId::new(1, 0);
    let b = SeatId::new(1, 1);
    let c = SeatId::new(1, 2);
    let d = SeatId::new(1, 3);
    let e = SeatId::new(2, 0);
    let mut session = race_session(square(3, 0), &[(a, 1), (b, 2), (c, 3), (d, 4), (e, 5)]);
    let _ = session.step(&[
        car(1, 1.0, 0.0),
        car(2, 1.0, 0.0),
        car(3, 1.0, 0.0),
        car(4, 1.0, 0.0),
        car(5, 1.0, 0.0),
    ]);
    // E runs a whole lap; A, B and D reach gate 20; C is half way to it.
    for (x, z) in LOOP {
        let _ = session.step(&[
            car(1, 100.0, 0.0),
            car(2, 100.0, 0.0),
            car(3, 50.0, 0.0),
            car(4, 100.0, 0.0),
            car(5, x, z),
        ]);
    }
    // A is 30 % of the way to gate 30, B and D 60 % (a dead heat), C 60 % of the way to 20, E is
    // just past the start on lap 2.
    let _ = session.step(&[
        car(1, 100.0, 30.0),
        car(2, 100.0, 60.0),
        car(3, 60.0, 0.0),
        car(4, 100.0, 60.0),
        car(5, 20.0, 0.0),
    ]);
    let order: Vec<SeatId> = session.race_standings().iter().map(|s| s.seat).collect();
    assert_eq!(
        order,
        vec![e, b, d, a, c],
        "laps first, then the checkpoint, then the distance left, then the seat"
    );
    let standings = session.race_standings();
    assert_eq!(standings[0].lap, 1);
    assert_eq!(standings[1].checkpoint_index, 1);
    assert_eq!(standings[1].remaining.num * 10, standings[1].remaining.den * 4);
    assert_eq!(
        standings.iter().map(|s| s.position).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5]
    );
}

#[test]
fn a_missed_checkpoint_does_not_count_the_lap() {
    let a = SeatId::new(1, 0);
    let mut session = race_session(square(2, 0), &[(a, 1)]);
    let mut events = Vec::new();
    // 10 -> 20, then a shortcut straight to 40 and back to the start: 30 is skipped.
    for (x, z) in [
        (1.0, 0.0),
        (100.0, 0.0),
        (50.0, 50.0),
        (0.0, 100.0),
        (0.0, 50.0),
        (0.0, 0.0),
    ] {
        events.extend(session.step(&[car(1, x, z)]).events);
    }
    let missed: Vec<(u32, u32)> = events
        .iter()
        .filter_map(|e| match e {
            RulesEvent::RacerMissedCheckpoint {
                checkpoint, expected, ..
            } => Some((*checkpoint, *expected)),
            _ => None,
        })
        .collect();
    assert_eq!(missed, vec![(40, 30), (10, 30)]);
    assert!(
        !events.iter().any(|e| matches!(e, RulesEvent::RacerLap { .. })),
        "a lap that skipped gate 30 does not count"
    );
    assert_eq!(session.race.state.racers[0].lap, 0);
    // Going back for gate 30 picks the lap up again.
    let mut events = Vec::new();
    for (x, z) in [
        (50.0, 50.0),
        (100.0, 100.0),
        (0.0, 100.0),
        (0.0, 50.0),
        (0.0, 0.0),
    ] {
        events.extend(session.step(&[car(1, x, z)]).events);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, RulesEvent::RacerLap { lap: 1, .. }))
    );
    assert_eq!(session.race.state.racers[0].lap, 1);
}

#[test]
fn finish_order_ties_break_by_tick_then_fraction_and_dnf_after_the_window() {
    let a = SeatId::new(1, 0);
    let c = SeatId::new(1, 1);
    let e = SeatId::new(1, 2);
    let b = SeatId::new(2, 0);
    let d = SeatId::new(2, 1);
    let mut session = race_session(square(1, 10), &[(a, 1), (c, 3), (e, 5), (b, 2), (d, 4)]);
    let all_at = |x: f32, z: f32| -> Vec<Contestant> { (1..=5).map(|id| car(id, x, z)).collect() };
    let _ = session.step(&all_at(1.0, 0.0));
    for (x, z) in &LOOP[..3] {
        let _ = session.step(&all_at(*x, *z));
    }
    // A, B and E cross the line on one step: B deepest past the gate (z -2), A and E level at -1.
    // C waits two steps short of the gate; D stays out of it.
    let mut events = Vec::new();
    let step = session.step(&[
        car(1, 0.0, -1.0),
        car(2, 0.0, -2.0),
        car(3, 0.0, 10.0),
        car(4, 0.0, 20.0),
        car(5, 0.0, -1.0),
    ]);
    let line_tick = step.flow.match_tick;
    events.extend(step.events);
    let _ = session.step(&[car(3, 0.0, 10.0), car(4, 0.0, 20.0)]);
    let step = session.step(&[car(3, 0.0, 1.0), car(4, 0.0, 20.0)]);
    let c_tick = step.flow.match_tick;
    events.extend(step.events);
    let mut over = None;
    for _ in 0..20 {
        let step = session.step(&[car(4, 0.0, 20.0)]);
        events.extend(step.events.clone());
        if session.board.match_over {
            over = Some(step.flow.match_tick);
            break;
        }
    }
    let finished: Vec<(u64, SeatId, u32)> = events
        .iter()
        .filter_map(|e| match e {
            RulesEvent::RacerFinished {
                tick,
                team,
                slot,
                place,
            } => Some((*tick, SeatId::new(*team, *slot), *place)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![
            (line_tick, b, 1),
            (line_tick, a, 2),
            (line_tick, e, 3),
            (c_tick, c, 4),
        ],
        "same tick: farther past the gate first, then the seat; a later tick after"
    );
    let dnf: Vec<(u64, SeatId)> = events
        .iter()
        .filter_map(|e| match e {
            RulesEvent::RacerDnf { tick, team, slot } => Some((*tick, SeatId::new(*team, *slot))),
            _ => None,
        })
        .collect();
    assert_eq!(
        dnf,
        vec![(line_tick + 10, d)],
        "DNF ten ticks after the first finish"
    );
    assert_eq!(over, Some(line_tick + 10));
    assert!(events.iter().any(|e| matches!(
        e,
        RulesEvent::MatchOver {
            team: 2,
            reason: EndReason::Ended,
            ..
        }
    )));
    let order: Vec<SeatId> = session.race_standings().iter().map(|s| s.seat).collect();
    assert_eq!(order, vec![b, a, e, c, d]);
}

#[test]
fn best_lap_reaches_the_ledger() {
    let mut session = MatchSession::new(SessionSettings {
        race: square(3, 0),
        ..SessionSettings::default()
    });
    // Seated with a bound unit: the race enters the seat on its own.
    let seats = session
        .assign_team(
            &SeatRequest {
                team: Some(1),
                slot: Some(0),
                unit: Some(7),
                from: None,
            },
            &[],
        )
        .expect("seat");
    let seat = seats[0];
    session.start_now();
    let _ = session.step(&[car(7, 1.0, 0.0)]);
    let mut laps = Vec::new();
    // Lap 1 with two idle steps per corner, lap 2 with none, lap 3 with one.
    for idle in [2, 0, 1] {
        for (x, z) in LOOP {
            for _ in 0..idle {
                let _ = session.step(&[car(7, x - 10.0, z)]);
            }
            for event in session.step(&[car(7, x, z)]).events {
                if let RulesEvent::RacerLap {
                    lap, lap_ticks, best, ..
                } = event
                {
                    laps.push((lap, lap_ticks, best));
                }
            }
        }
    }
    assert_eq!(laps, vec![(1, 12, true), (2, 4, true), (3, 8, false)]);
    let row = session.ledgers.row(seat).expect("ledger row");
    assert_eq!(row.counters.get(BEST_LAP_COUNTER), Some(&4));
    assert_eq!(session.race.state.racers[0].best_lap_ticks, Some(4));
    assert_eq!(session.race_standings()[0].place, Some(1));
}

/// One scripted match of each genre, on two peers fed the same inputs.
fn genre_script(peer_reverses_units: bool) -> (MatchSession, Vec<Vec<RulesEvent>>) {
    let mut session = seated(
        SessionSettings {
            waves: WaveSettings {
                waves: vec![
                    WaveRow {
                        groups: vec![group("grunt", 6, "east"), group("brute", 3, "west")],
                        alive_cap: 5,
                        ..WaveRow::default()
                    },
                    WaveRow {
                        at: WaveStart::AtTick { tick: 40 },
                        groups: vec![group("boss", 1, "arena")],
                        alive_cap: 0,
                    },
                ],
                shared_lives: Some(3),
                ..WaveSettings::default()
            },
            race: square(2, 30),
            respawn: RespawnRules {
                default: RespawnPolicy::Delay { ticks: 4 },
                ..RespawnRules::default()
            },
            ..SessionSettings::default()
        },
        &[(1, 0), (1, 1), (2, 0)],
    );
    session.race_enter(SeatId::new(1, 0), 1);
    session.race_enter(SeatId::new(1, 1), 2);
    session.race_enter(SeatId::new(2, 0), 3);
    let mut log = Vec::new();
    for step in 0..120u32 {
        if step % 7 == 3 {
            session.note_wave_down(0, 1 + step % 2);
        }
        if step % 31 == 5 {
            session.note_death(SeatId::new(1, u8::from(step % 2 == 0)));
        }
        if step == 90 {
            session.note_wave_down(1, 1);
        }
        let mut units: Vec<Contestant> = (1..=3u32)
            .map(|id| {
                let speed = 2 + id;
                let along = (step * speed) % 400;
                let (x, z) = match along / 100 {
                    0 => (along, 0),
                    1 => (100, along - 100),
                    2 => (300 - along, 100),
                    _ => (0, 400 - along),
                };
                car(id, x as f32, z as f32)
            })
            .collect();
        if peer_reverses_units {
            units.reverse();
        }
        log.push(session.step(&units).events);
    }
    (session, log)
}

#[test]
fn waves_and_race_bit_identical_on_two_peers() {
    let (left, left_log) = genre_script(false);
    let (right, right_log) = genre_script(true);
    assert_eq!(left_log, right_log, "the same events on the same steps");
    assert_eq!(left, right);
    let left_text = serde_json::to_string(&left).expect("serialize");
    let right_text = serde_json::to_string(&right).expect("serialize");
    assert_eq!(left_text, right_text, "the same session bits");
    let all: Vec<&RulesEvent> = left_log.iter().flatten().collect();
    assert!(all.iter().any(|e| matches!(e, RulesEvent::WaveSpawn { .. })));
    assert!(all.iter().any(|e| matches!(e, RulesEvent::RacerLap { .. })));
    assert!(
        !left.race.state.racers.is_empty() && !left.waves.state.progress.is_empty(),
        "both genres ran"
    );
}

// ---------------------------------------------------------------------------
// Edge cases beyond the acceptance list
// ---------------------------------------------------------------------------

#[test]
fn a_branch_counts_either_path_and_ranks_by_list_index() {
    // Start 1 -> 2, which branches to 3 (listed first) or 4, both rejoining 1.
    let gate = |id: u32, x: f32, z: f32, next: Vec<u32>| Checkpoint {
        id,
        volume: HillVolume {
            min: [x - 3.0, -1.0, z - 3.0],
            max: [x + 3.0, 3.0, z + 3.0],
        },
        next,
    };
    let race = RaceSettings {
        checkpoints: vec![
            gate(1, 0.0, 0.0, vec![2]),
            gate(2, 100.0, 0.0, vec![3, 4]),
            gate(3, 100.0, 100.0, vec![1]),
            gate(4, 150.0, 50.0, vec![1]),
        ],
        laps: 2,
        ..RaceSettings::default()
    };
    assert_eq!(race.validate(), Ok(()));
    let a = SeatId::new(1, 0);
    let b = SeatId::new(1, 1);
    let mut session = race_session(race, &[(a, 1), (b, 2)]);
    let mut events = Vec::new();
    for ((ax, az), (bx, bz)) in [
        ((1.0, 0.0), (1.0, 0.0)),
        ((100.0, 0.0), (100.0, 0.0)),
        ((100.0, 100.0), (150.0, 50.0)),
        ((0.0, 0.0), (0.0, 0.0)),
    ] {
        events.extend(session.step(&[car(1, ax, az), car(2, bx, bz)]).events);
    }
    let laps: Vec<(u8, u8, u32)> = events
        .iter()
        .filter_map(|e| match e {
            RulesEvent::RacerLap { team, slot, lap, .. } => Some((*team, *slot, *lap)),
            _ => None,
        })
        .collect();
    assert_eq!(laps, vec![(1, 0, 1), (1, 1, 1)], "either branch closes the lap");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RulesEvent::RacerMissedCheckpoint { .. }))
    );
    // Mid-branch, the alternate listed later ranks ahead (list index 3 above 2).
    let mut session = race_session(
        RaceSettings {
            checkpoints: session.race.settings.checkpoints.clone(),
            laps: 2,
            ..RaceSettings::default()
        },
        &[(a, 1), (b, 2)],
    );
    for ((ax, az), (bx, bz)) in [
        ((1.0, 0.0), (1.0, 0.0)),
        ((100.0, 0.0), (100.0, 0.0)),
        ((100.0, 100.0), (150.0, 50.0)),
    ] {
        let _ = session.step(&[car(1, ax, az), car(2, bx, bz)]);
    }
    let order: Vec<SeatId> = session.race_standings().iter().map(|s| s.seat).collect();
    assert_eq!(order, vec![b, a]);
}

#[test]
fn wrong_way_is_flagged_after_the_set_steps_and_cleared_going_forward() {
    let a = SeatId::new(1, 0);
    let mut race = square(1, 0);
    race.wrong_way_ticks = 3;
    let mut session = race_session(race, &[(a, 1)]);
    let mut flagged = Vec::new();
    // Forward to x = 50, then back four steps, then forward again.
    for x in [1.0, 20.0, 50.0, 45.0, 40.0, 35.0, 30.0, 40.0] {
        let step = session.step(&[car(1, x, 0.0)]);
        if step
            .events
            .iter()
            .any(|e| matches!(e, RulesEvent::RacerWrongWay { .. }))
        {
            flagged.push(x);
        }
        if x == 35.0 {
            assert!(session.race_standings()[0].wrong_way);
        }
    }
    assert_eq!(flagged, vec![35.0], "flagged once, on the third backward step");
    assert!(
        !session.race_standings()[0].wrong_way,
        "gaining ground clears the flag"
    );
}

#[test]
fn difficulty_rows_scale_counts_and_caps_by_seated_players() {
    let settings = |players: &[(u8, u8)]| {
        seated(
            SessionSettings {
                waves: WaveSettings {
                    waves: vec![WaveRow {
                        groups: vec![group("grunt", 4, "")],
                        alive_cap: 2,
                        ..WaveRow::default()
                    }],
                    difficulty: vec![
                        regia_rules::waves::DifficultyRow {
                            players: 1,
                            count_percent: 100,
                            cap_percent: 100,
                        },
                        regia_rules::waves::DifficultyRow {
                            players: 3,
                            count_percent: 150,
                            cap_percent: 200,
                        },
                    ],
                    ..WaveSettings::default()
                },
                ..SessionSettings::default()
            },
            players,
        )
    };
    let mut duo = settings(&[(1, 0), (1, 1)]);
    let _ = duo.step(&[]);
    assert_eq!(duo.waves.state.progress[0].planned, vec![4]);
    assert_eq!(duo.waves.state.progress[0].alive_cap, 2);
    assert_eq!(duo.waves.state.alive_total(), 2);
    let mut trio = settings(&[(1, 0), (1, 1), (1, 2)]);
    let _ = trio.step(&[]);
    assert_eq!(trio.waves.state.progress[0].planned, vec![6]);
    assert_eq!(trio.waves.state.progress[0].alive_cap, 4);
    assert_eq!(trio.waves.state.alive_total(), 4);
}

#[test]
fn at_tick_waves_start_without_waiting_for_a_clear() {
    let mut session = seated(
        SessionSettings {
            waves: WaveSettings {
                waves: vec![
                    WaveRow {
                        groups: vec![group("grunt", 2, "")],
                        ..WaveRow::default()
                    },
                    WaveRow {
                        at: WaveStart::AtTick { tick: 5 },
                        groups: vec![group("flyer", 1, "sky")],
                        alive_cap: 0,
                    },
                ],
                ..WaveSettings::default()
            },
            ..SessionSettings::default()
        },
        &[(1, 0)],
    );
    let mut started = Vec::new();
    for _ in 0..8 {
        for event in session.step(&[]).events {
            if let RulesEvent::WaveStarted { tick, wave } = event {
                started.push((tick, wave));
            }
        }
    }
    assert_eq!(started.len(), 2);
    assert_eq!(
        started[1],
        (5, 1),
        "wave 1 starts at tick 5 with wave 0 still alive"
    );
    assert_eq!(session.waves.state.alive_total(), 3);
}

#[test]
fn the_director_waits_through_the_warmup() {
    use regia_rules::match_flow::FlowSettings;
    use regia_rules::phases::PhaseSettings;
    let mut session = seated(
        SessionSettings {
            flow: FlowSettings {
                phases: PhaseSettings {
                    warmup_ticks: Some(10),
                    ..PhaseSettings::default()
                },
                ..FlowSettings::default()
            },
            waves: WaveSettings {
                waves: vec![WaveRow {
                    groups: vec![group("grunt", 1, "")],
                    ..WaveRow::default()
                }],
                ..WaveSettings::default()
            },
            ..SessionSettings::default()
        },
        &[(1, 0)],
    );
    let mut first_spawn = None;
    for _ in 0..30 {
        let step = session.step(&[]);
        if first_spawn.is_none()
            && step
                .events
                .iter()
                .any(|e| matches!(e, RulesEvent::WaveSpawn { .. }))
        {
            first_spawn = Some(step.flow.match_tick);
        }
    }
    let tick = first_spawn.expect("the wave goes out after the warm-up");
    assert!(tick >= 10, "no spawn during the warm-up (got tick {tick})");
}
