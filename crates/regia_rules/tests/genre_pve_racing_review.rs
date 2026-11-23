// SPDX-License-Identifier: MPL-2.0
//! Review proofs for co-op waves and races (lane rules-genre-pve-racing-review, head f8444889f).
//!
//! The tests named `proof_*` fail on that head. The others lock behavior the review checked and
//! expect to pass: empty waves stay off, one checkpoint is refused and does not panic, seating
//! order does not change race or wave results, a snapshot round-trips, and an unvalidated
//! difficulty percent at the `u32` ceiling does not overflow.

use regia_rules::match_flow::{FlowInput, SeatId};
use regia_rules::objective::{Contestant, HillVolume, RulesEvent};
use regia_rules::race::{Checkpoint, Race, RaceSettings};
use regia_rules::session::{MatchSession, SessionSettings};
use regia_rules::teams::SeatRequest;
use regia_rules::waves::{CoopObjective, CoopSuccess, DifficultyRow, WaveGroup, WaveRow, WaveSettings};

fn group(archetype: &str, count: u32) -> WaveGroup {
    WaveGroup {
        archetype: archetype.into(),
        count,
        spawn_tag: String::new(),
    }
}

fn gate(id: u32, x: f32, half_x: f32, next: Vec<u32>) -> Checkpoint {
    Checkpoint {
        id,
        volume: HillVolume {
            min: [x - half_x, -1.0, -1.0],
            max: [x + half_x, 1.0, 1.0],
        },
        next,
    }
}

fn car(id: u32, x: f32) -> Contestant {
    Contestant {
        id,
        team: 1,
        position: [x, 0.0, 0.0],
        alive: true,
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

/// Start at x = 0 (id 1), one gate at x = 10 (id 2), back to the start. One lap.
fn line(window: u64) -> RaceSettings {
    RaceSettings {
        checkpoints: vec![gate(1, 0.0, 1.0, vec![2]), gate(2, 10.0, 1.0, vec![1])],
        laps: 1,
        finish_window_ticks: window,
        wrong_way_ticks: 0,
    }
}

fn race_events(events: &[RulesEvent]) -> Vec<RulesEvent> {
    events
        .iter()
        .filter(|event| {
            matches!(
                event,
                RulesEvent::RacerCheckpoint { .. }
                    | RulesEvent::RacerLap { .. }
                    | RulesEvent::RacerMissedCheckpoint { .. }
                    | RulesEvent::RacerWrongWay { .. }
                    | RulesEvent::RacerFinished { .. }
                    | RulesEvent::RacerDnf { .. }
            )
        })
        .cloned()
        .collect()
}

fn places(session: &MatchSession) -> Vec<(SeatId, Option<u32>, bool)> {
    session
        .race
        .state
        .racers
        .iter()
        .map(|racer| (racer.seat, racer.place, racer.dnf))
        .collect()
}

/// Window 0 waits for every racer. A seat that has left is not racing, so the finisher must be
/// able to close the match.
#[test]
fn proof_leaver_does_not_hold_a_zero_window_race_open() {
    let a = SeatId::new(1, 0);
    let b = SeatId::new(1, 1);
    let mut session = race_session(line(0), &[(a, 1), (b, 2)]);
    let _ = session.step(&[car(1, 0.0), car(2, 0.0)]);
    let _ = session.step(&[car(1, 10.0), car(2, 10.0)]);
    session
        .apply(&FlowInput::Leave { team: 1, slot: 1 })
        .expect("leave");
    let mut over = false;
    for _ in 0..5 {
        let _ = session.step(&[car(1, 0.0)]);
        if session.board.match_over {
            over = true;
            break;
        }
    }
    assert!(
        over,
        "A finished and B had left, but the match stayed open: {:?}",
        places(&session)
    );
    let left = session.race.state.racers.iter().find(|racer| racer.seat == b);
    assert!(
        left.is_none() || left.is_some_and(|racer| racer.dnf || racer.finished_at.is_some()),
        "the seat that left is still a live racer: {left:?}"
    );
}

/// A window never starts until someone finishes. The only racer leaving must still end the race.
#[test]
fn proof_solo_leaver_before_the_first_finish_does_not_hang() {
    let a = SeatId::new(1, 0);
    let mut session = race_session(line(10), &[(a, 1)]);
    let _ = session.step(&[car(1, 0.0)]);
    let _ = session.step(&[car(1, 10.0)]);
    session
        .apply(&FlowInput::Leave { team: 1, slot: 0 })
        .expect("leave");
    for _ in 0..30 {
        let _ = session.step(&[]);
        if session.board.match_over {
            return;
        }
    }
    let racer = session.race.state.racers.iter().find(|racer| racer.seat == a);
    assert!(
        session.race.state.racers.is_empty()
            || racer.is_some_and(|racer| racer.dnf || racer.finished_at.is_some()),
        "30 steps after the only racer left, the race is still open: {racer:?}"
    );
}

/// The host may still report the unit after the seat leaves. That row must not take a place.
#[test]
fn proof_leaver_unit_does_not_keep_scoring() {
    let a = SeatId::new(1, 0);
    let b = SeatId::new(1, 1);
    let mut session = race_session(line(0), &[(a, 1), (b, 2)]);
    let _ = session.step(&[car(1, 0.0), car(2, 0.0)]);
    let _ = session.step(&[car(1, 10.0), car(2, 10.0)]);
    session
        .apply(&FlowInput::Leave { team: 1, slot: 1 })
        .expect("leave");
    // B is a meter past the line, A is on the center. Both volumes contain them.
    let _ = session.step(&[car(1, 0.0), car(2, -1.0)]);
    let left = session
        .race
        .state
        .racers
        .iter()
        .find(|racer| racer.seat == b)
        .expect("row");
    assert!(
        left.dnf && left.place.is_none(),
        "the seat that left took a finish place: {:?}",
        places(&session)
    );
}

/// A same-team slot move keeps the old `SeatId` in the race and enters the new one at lap 0.
#[test]
fn proof_slot_move_does_not_leave_the_old_seat_racing() {
    let from = SeatId::new(1, 0);
    let mut session = race_session(line(0), &[(from, 7)]);
    let _ = session.step(&[car(7, 0.0)]);
    let _ = session.step(&[car(7, 10.0)]);
    session
        .assign_team(
            &SeatRequest {
                team: Some(1),
                slot: Some(1),
                unit: Some(7),
                from: Some(from),
            },
            &[],
        )
        .expect("slot move");
    let _ = session.step(&[car(7, 10.0)]);
    let old = session.race.state.racers.iter().find(|racer| racer.seat == from);
    assert!(
        old.is_none() || old.is_some_and(|racer| racer.dnf || racer.finished_at.is_some()),
        "the seat that moved is still a live racer, so the race cannot end: {old:?}"
    );
}

/// Three steps backward from x = 2 cross the segment origin. The third must flag wrong-way.
#[test]
fn proof_wrong_way_past_the_segment_origin_still_counts() {
    let a = SeatId::new(1, 0);
    let mut race = line(0);
    race.wrong_way_ticks = 3;
    let mut session = race_session(race, &[(a, 1)]);
    let mut flagged = Vec::new();
    // Forward to x = 2 (segment 0 -> 10). Then x = 1, 0, -4, -8, -12.
    for x in [2.0, 1.0, 0.0, -4.0, -8.0, -12.0] {
        let step = session.step(&[car(1, x)]);
        if step
            .events
            .iter()
            .any(|event| matches!(event, RulesEvent::RacerWrongWay { .. }))
        {
            flagged.push(x);
        }
    }
    assert_eq!(
        flagged,
        vec![-4.0],
        "driving backward past the segment origin froze the counter: {:?}",
        session.race.state.racers[0].backwards
    );
    assert!(session.race_standings()[0].wrong_way);
}

/// Same-step finishers from a short branch and a long branch. The one farther past the gate in
/// millimeters places first (book: "how far past the finish gate", ADR-8695 signed projection).
#[test]
fn proof_finish_order_uses_meters_past_the_gate_not_segment_fraction() {
    // CP1 finish x in [-25, 25], center 0, next 2.
    // CP2 splitter at -60, next 3 (short, center -30) or 4 (long, center -500).
    // Both rejoin CP1. A finishes 2 m past the line, B finishes 24 m past it.
    let race = RaceSettings {
        checkpoints: vec![
            gate(1, 0.0, 25.0, vec![2]),
            gate(2, -60.0, 2.0, vec![3, 4]),
            gate(3, -30.0, 2.0, vec![1]),
            gate(4, -500.0, 2.0, vec![1]),
        ],
        laps: 1,
        finish_window_ticks: 0,
        wrong_way_ticks: 0,
    };
    assert_eq!(race.validate(), Ok(()));
    let a = SeatId::new(1, 0);
    let b = SeatId::new(1, 1);
    let mut session = race_session(race, &[(a, 1), (b, 2)]);
    let _ = session.step(&[car(1, -60.0), car(2, -60.0)]);
    let _ = session.step(&[car(1, -30.0), car(2, -500.0)]);
    let step = session.step(&[car(1, 2.0), car(2, 24.0)]);
    let finished: Vec<(SeatId, u32)> = step
        .events
        .iter()
        .filter_map(|event| match event {
            RulesEvent::RacerFinished {
                team, slot, place, ..
            } => Some((SeatId::new(*team, *slot), *place)),
            _ => None,
        })
        .collect();
    assert_eq!(
        finished,
        vec![(b, 1), (a, 2)],
        "B is 24 m past the line and A is 2 m past it; fractions of unequal last segments reversed them: {:?}",
        places(&session)
    );
}

#[test]
fn zero_waves_stay_off_through_a_death_and_an_objective() {
    let mut session = seated(
        SessionSettings {
            waves: WaveSettings {
                shared_lives: Some(1),
                fail_on_team_wipe: true,
                success: CoopSuccess::ObjectiveReached,
                ..WaveSettings::default()
            },
            ..SessionSettings::default()
        },
        &[(1, 0)],
    );
    assert!(session.waves.is_off());
    session.note_death(SeatId::new(1, 0));
    session.note_coop_objective(CoopObjective::Reached);
    let step = session.step(&[]);
    assert!(!session.board.match_over, "{:?}", step.events);
    assert_eq!(session.waves.state.lives_left, Some(1));
    assert!(session.waves.state.ended.is_none());
}

#[test]
fn one_checkpoint_is_refused_and_an_unvalidated_course_does_not_panic() {
    let settings = RaceSettings {
        checkpoints: vec![gate(1, 0.0, 1.0, vec![1])],
        laps: 1,
        ..RaceSettings::default()
    };
    assert!(settings.validate().is_err(), "one checkpoint is not a course");
    let mut race = Race::new(settings);
    let seat = SeatId::new(1, 0);
    race.enter(seat, 1);
    let mut events = Vec::new();
    race.step(1, &[car(1, 5.0)], &mut |event| events.push(event));
    race.step(2, &[car(1, 0.0)], &mut |event| events.push(event));
    assert!(events.is_empty(), "{events:?}");
    assert_eq!(race.state.racers[0].lap, 0);
    assert!(race.state.ended_at.is_none());
}

#[test]
fn race_and_wave_results_ignore_seat_insertion_and_unit_order() {
    let waves = WaveSettings {
        waves: vec![WaveRow {
            groups: vec![group("grunt", 2)],
            alive_cap: 0,
            ..WaveRow::default()
        }],
        ..WaveSettings::default()
    };
    let build = |seats: &[(u8, u8)], reverse_units: bool| {
        let mut session = seated(
            SessionSettings {
                race: line(0),
                waves: waves.clone(),
                ..SessionSettings::default()
            },
            seats,
        );
        // The same unit id for the same seat. Reversing only changes enter order and slice order.
        let mut racers: Vec<(SeatId, u32)> = seats
            .iter()
            .map(|(team, slot)| {
                (
                    SeatId::new(*team, *slot),
                    u32::from(*team) * 10 + u32::from(*slot) + 1,
                )
            })
            .collect();
        if reverse_units {
            racers.reverse();
        }
        for (seat, unit) in &racers {
            session.race_enter(*seat, *unit);
        }
        let mut race_log = Vec::new();
        let mut wave_log = Vec::new();
        for step in 0..4u32 {
            let x = if step < 2 { 0.0 } else { 10.0 };
            let mut units: Vec<Contestant> = racers.iter().map(|(_, unit)| car(*unit, x)).collect();
            if reverse_units {
                units.reverse();
            }
            let events = session.step(&units).events;
            race_log.push(race_events(&events));
            wave_log.push(
                events
                    .into_iter()
                    .filter(|event| {
                        matches!(
                            event,
                            RulesEvent::WaveStarted { .. }
                                | RulesEvent::WaveSpawn { .. }
                                | RulesEvent::WaveCleared { .. }
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
        (
            session.race_standings(),
            session.race.state.racers.clone(),
            race_log,
            wave_log,
        )
    };
    let forward = build(&[(1, 0), (2, 0), (1, 1)], false);
    let reverse = build(&[(1, 1), (2, 0), (1, 0)], true);
    assert_eq!(forward.0, reverse.0, "standings");
    assert_eq!(forward.1, reverse.1, "racer rows");
    assert_eq!(forward.2, reverse.2, "race events");
    assert_eq!(forward.3, reverse.3, "wave events");
}

#[test]
fn a_mid_race_snapshot_restores_waves_and_the_race() {
    let mut session = seated(
        SessionSettings {
            waves: WaveSettings {
                waves: vec![WaveRow {
                    groups: vec![group("grunt", 3)],
                    ..WaveRow::default()
                }],
                ..WaveSettings::default()
            },
            race: line(0),
            ..SessionSettings::default()
        },
        &[(1, 0)],
    );
    session.race_enter(SeatId::new(1, 0), 4);
    let _ = session.step(&[car(4, 0.0)]);
    let _ = session.step(&[car(4, 10.0)]);
    let text = serde_json::to_string(&session).expect("serialize");
    let mut restored: MatchSession = serde_json::from_str(&text).expect("restore");
    let left = session.step(&[car(4, 0.0)]);
    let right = restored.step(&[car(4, 0.0)]);
    assert_eq!(race_events(&left.events), race_events(&right.events));
    assert_eq!(session.race, restored.race);
    assert_eq!(session.waves, restored.waves);
    assert_eq!(session.board.match_over, restored.board.match_over);
}

#[test]
fn an_unvalidated_u32_difficulty_percent_does_not_panic() {
    // `u32::MAX * 1000` fits in `u64`. The director saturates the spawn count at `u32::MAX`.
    let mut session = seated(
        SessionSettings {
            waves: WaveSettings {
                waves: vec![WaveRow {
                    groups: vec![group("grunt", u32::MAX)],
                    alive_cap: u32::MAX,
                    ..WaveRow::default()
                }],
                difficulty: vec![DifficultyRow {
                    players: 1,
                    count_percent: 1000,
                    cap_percent: 1000,
                }],
                ..WaveSettings::default()
            },
            ..SessionSettings::default()
        },
        &[(1, 0)],
    );
    let step = session.step(&[]);
    let counts: Vec<u32> = step
        .events
        .iter()
        .filter_map(|event| match event {
            RulesEvent::WaveSpawn { count, .. } => Some(*count),
            _ => None,
        })
        .collect();
    assert_eq!(counts, vec![u32::MAX]);
}
