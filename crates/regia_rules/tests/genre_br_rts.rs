// SPDX-License-Identifier: MPL-2.0
//! Battle-royale zones, elimination placements and strategy victory conditions (GR-11.3, GR-11.4).

use regia_rules::RulesError;
use regia_rules::elimination::{Alliance, EliminationRules, OutCause, Placement};
use regia_rules::match_flow::{FlowInput, FlowSettings, Issuer, SeatId};
use regia_rules::objective::{Contestant, EndReason, ObjectiveSettings, RulesEvent};
use regia_rules::session::{MatchSession, SessionSettings};
use regia_rules::spawn::{RespawnPolicy, RespawnRules};
use regia_rules::victory::{SurrenderRules, VictoryCondition, VictoryRules};
use regia_rules::zone::{ZonePhase, ZoneSchedule, ZoneStage};

fn settings(teams: u8, seats: u8) -> SessionSettings {
    SessionSettings {
        flow: FlowSettings {
            max_teams: teams,
            seats_per_team: seats,
            start_delay_ticks: 0,
            ..FlowSettings::default()
        },
        objectives: ObjectiveSettings {
            max_teams: teams,
            ..ObjectiveSettings::default()
        },
        respawn: RespawnRules {
            default: RespawnPolicy::Never,
            ..RespawnRules::default()
        },
        ..SessionSettings::default()
    }
}

/// A started session with `seats` seated, every seat in order.
fn started(settings: SessionSettings, seats: &[(u8, u8)]) -> MatchSession {
    let mut session = MatchSession::new(settings);
    for &(team, slot) in seats {
        session.apply(&FlowInput::Seat { team, slot }).expect("seat");
    }
    session.start_now();
    session.step(&[]);
    session
}

fn unit(id: u32, team: u8, x: f32, z: f32) -> Contestant {
    Contestant {
        id,
        team,
        position: [x, 0.0, z],
        alive: true,
    }
}

fn eliminated(events: &[RulesEvent]) -> Vec<(u8, u8, OutCause)> {
    events
        .iter()
        .filter_map(|e| match e {
            RulesEvent::TeamEliminated {
                team, place, cause, ..
            } => Some((*team, *place, *cause)),
            _ => None,
        })
        .collect()
}

fn over(events: &[RulesEvent]) -> Option<(u8, EndReason)> {
    events.iter().find_map(|e| match e {
        RulesEvent::MatchOver { team, reason, .. } => Some((*team, *reason)),
        _ => None,
    })
}

fn victory(events: &[RulesEvent]) -> Option<(Vec<u8>, EndReason)> {
    events.iter().find_map(|e| match e {
        RulesEvent::Victory { teams, reason, .. } => Some((teams.clone(), *reason)),
        _ => None,
    })
}

fn zone_schedule(seed: u64) -> ZoneSchedule {
    ZoneSchedule {
        center: [0.0, 0.0],
        radius: 100.0,
        phases: vec![
            ZonePhase {
                wait_ticks: 10,
                shrink_ticks: 100,
                end_radius: 10.0,
                damage_per_tick: 3,
            },
            ZonePhase {
                wait_ticks: 5,
                shrink_ticks: 20,
                end_radius: 2.0,
                damage_per_tick: 7,
            },
        ],
        seed,
    }
}

#[test]
fn zone_centers_are_identical_on_two_peers_from_one_seed() {
    let run = |seed: u64| {
        let mut session = started(
            SessionSettings {
                zone: Some(zone_schedule(seed)),
                ..settings(2, 1)
            },
            &[(1, 0), (2, 0)],
        );
        let units = [unit(1, 1, 3.0, 4.0), unit(2, 2, -60.0, 20.0)];
        let mut events = Vec::new();
        for _ in 0..200 {
            events.extend(session.step(&units).events);
        }
        let centers: Vec<([f32; 2], f32)> = events
            .iter()
            .filter_map(|e| match e {
                RulesEvent::ZonePhase {
                    target_center,
                    target_radius,
                    ..
                } => Some((*target_center, *target_radius)),
                _ => None,
            })
            .collect();
        (session, events, centers)
    };
    let (left, left_events, left_centers) = run(42);
    let (right, right_events, right_centers) = run(42);
    assert_eq!(left.zone_state, right.zone_state, "same seed, same zone state");
    assert_eq!(left_events, right_events, "same events, damage included");
    assert_eq!(left_centers, right_centers);
    assert_eq!(left.zone_state.stage, ZoneStage::Closed);
    // The first `waiting` landed on the setup step; then shrinking, waiting, shrinking, closed.
    assert_eq!(left_centers.len(), 4);
    let (_, _, other) = run(43);
    assert_ne!(left_centers, other, "another seed draws other centers");
}

#[test]
fn zone_damage_events_only_outside_the_current_circle_during_shrink() {
    let mut session = started(
        SessionSettings {
            zone: Some(zone_schedule(7)),
            ..settings(4, 1)
        },
        &[(1, 0), (2, 0), (3, 0)],
    );
    // The first step started the zone: its first target is known.
    let start = session.zone_state.from.center();
    let target = session.zone_state.to.center();
    assert_eq!(session.zone_state.stage, ZoneStage::Waiting);
    // A sits on the final center of phase 0: inside every circle of the shrink. B starts inside
    // the start circle on the side away from the target, so the shrink leaves it outside. C is
    // dead outside and never takes damage.
    let away = if target[0] > start[0] { -95.0 } else { 95.0 };
    let units = [
        unit(1, 1, target[0], target[1]),
        unit(2, 2, start[0] + away, start[1]),
        Contestant {
            alive: false,
            ..unit(3, 3, 500.0, 500.0)
        },
    ];
    let mut damaged_b = 0;
    let mut damaged_while_shrinking = 0;
    // Ticks 2..=110: the wait ends on tick 11 and the shrink runs to tick 111.
    for _ in 0..109 {
        let step = session.step(&units);
        let stage = session.zone_state.stage;
        for e in &step.events {
            if let RulesEvent::ZoneDamage { unit, amount, .. } = e {
                assert_eq!(*amount, 3, "phase 0's damage");
                assert_ne!(*unit, 1, "a unit inside the circle takes no damage");
                assert_ne!(*unit, 3, "a dead unit takes no damage");
                assert_eq!(*unit, 2);
                assert!(!session.zone_state.inside(units[1].position));
                damaged_b += 1;
                if stage == ZoneStage::Shrinking {
                    damaged_while_shrinking += 1;
                }
            }
        }
        let b_outside = !session.zone_state.inside(units[1].position);
        let b_hit = step
            .events
            .iter()
            .any(|e| matches!(e, RulesEvent::ZoneDamage { unit: 2, .. }));
        assert_eq!(b_outside, b_hit, "damage exactly when B is outside");
        if stage == ZoneStage::Waiting {
            assert!(!b_hit, "B is inside the start circle while it holds");
        }
    }
    assert!(damaged_b > 0, "the shrink left B outside");
    assert_eq!(
        damaged_b, damaged_while_shrinking,
        "phase 0 only damages during its shrink here"
    );
}

#[test]
fn elimination_order_gives_placements_and_last_standing_wins() {
    let mut session = started(
        SessionSettings {
            elimination: EliminationRules {
                last_standing: true,
                ..EliminationRules::default()
            },
            ..settings(4, 1)
        },
        &[(1, 0), (2, 0), (3, 0), (4, 0)],
    );
    session.note_death(SeatId::new(3, 0));
    let first = session.step(&[]).events;
    assert_eq!(eliminated(&first), vec![(3, 4, OutCause::Eliminated)]);
    assert!(over(&first).is_none());
    // Two outs on one tick go in team-id order: 1 before 4.
    session.note_death(SeatId::new(4, 0));
    session.note_death(SeatId::new(1, 0));
    let last = session.step(&[]).events;
    assert_eq!(
        eliminated(&last),
        vec![(1, 3, OutCause::Eliminated), (4, 2, OutCause::Eliminated)]
    );
    assert_eq!(victory(&last), Some((vec![2], EndReason::LastStanding)));
    assert_eq!(over(&last), Some((2, EndReason::LastStanding)));
    assert_eq!(
        session.placements(),
        vec![
            Placement { team: 2, place: 1 },
            Placement { team: 4, place: 2 },
            Placement { team: 1, place: 3 },
            Placement { team: 3, place: 4 },
        ]
    );
    // Nothing more happens once decided.
    session.note_death(SeatId::new(2, 0));
    let after = session.step(&[]).events;
    assert!(eliminated(&after).is_empty());
    assert!(over(&after).is_none());
}

#[test]
fn destroy_all_tagged_ends_when_a_team_has_none_left() {
    let victory_rules = VictoryRules {
        conditions: vec![VictoryCondition::DestroyAllTagged {
            tag: "capital".into(),
        }],
        ..VictoryRules::default()
    };
    // Three teams: one stripped team is out, the match goes on.
    let mut three = started(
        SessionSettings {
            victory: victory_rules.clone(),
            ..settings(3, 1)
        },
        &[(1, 0), (2, 0), (3, 0)],
    );
    for team in 1..=3 {
        three.report_tagged(team, "capital", 1).expect("report");
    }
    assert!(over(&three.step(&[]).events).is_none());
    three.report_tagged(3, "capital", 0).expect("report");
    let events = three.step(&[]).events;
    assert_eq!(eliminated(&events), vec![(3, 3, OutCause::Destroyed)]);
    assert!(over(&events).is_none(), "two teams still hold a capital");

    // Two teams: the first with none left loses.
    let mut two = started(
        SessionSettings {
            victory: victory_rules,
            ..settings(2, 1)
        },
        &[(1, 0), (2, 0)],
    );
    two.report_tagged(1, "capital", 2).expect("report");
    // Team 2 never reported a capital: it stays in, as a team that never built one.
    assert!(over(&two.step(&[]).events).is_none());
    two.report_tagged(1, "capital", 0).expect("report");
    let events = two.step(&[]).events;
    assert_eq!(eliminated(&events), vec![(1, 2, OutCause::Destroyed)]);
    assert_eq!(victory(&events), Some((vec![2], EndReason::Destroyed)));
    assert_eq!(over(&events), Some((2, EndReason::Destroyed)));
    assert_eq!(
        two.report_tagged(1, "", 0),
        Err(RulesError::VictorySettings { field: "tag" })
    );
}

#[test]
fn hold_for_resets_when_the_holder_loses_the_objective() {
    let mut session = started(
        SessionSettings {
            victory: VictoryRules {
                conditions: vec![VictoryCondition::HoldFor {
                    objective: 7,
                    ticks: 10,
                }],
                ..VictoryRules::default()
            },
            ..settings(2, 1)
        },
        &[(1, 0), (2, 0)],
    );
    let mut events = Vec::new();
    session.report_holder(7, 1).expect("hold");
    for _ in 0..9 {
        events.extend(session.step(&[]).events);
    }
    // Team 1 loses it one tick short; nobody holds it for a tick, then team 2 takes it.
    session.report_holder(7, 0).expect("lost");
    events.extend(session.step(&[]).events);
    session.report_holder(7, 2).expect("taken");
    for _ in 0..9 {
        events.extend(session.step(&[]).events);
    }
    session.report_holder(7, 2).expect("still held");
    assert!(over(&events).is_none(), "no holder has had it for 10 ticks yet");
    let last = session.step(&[]).events;
    assert_eq!(victory(&last), Some((vec![2], EndReason::Held)));
    assert_eq!(over(&last), Some((2, EndReason::Held)));
    assert_eq!(
        session.placements(),
        vec![Placement { team: 2, place: 1 }, Placement { team: 1, place: 2 }]
    );
}

#[test]
fn allied_teams_share_a_victory() {
    let mut session = started(
        SessionSettings {
            elimination: EliminationRules {
                last_standing: false,
                alliances: vec![Alliance { teams: vec![1, 2] }],
            },
            victory: VictoryRules {
                conditions: vec![VictoryCondition::TeamElimination],
                ..VictoryRules::default()
            },
            ..settings(3, 1)
        },
        &[(1, 0), (2, 0), (3, 0)],
    );
    session.note_death(SeatId::new(1, 0));
    let first = session.step(&[]).events;
    assert_eq!(eliminated(&first), vec![(1, 3, OutCause::Eliminated)]);
    assert!(over(&first).is_none(), "team 2 and team 3 are two sides");
    session.note_death(SeatId::new(3, 0));
    let last = session.step(&[]).events;
    assert_eq!(victory(&last), Some((vec![1, 2], EndReason::LastStanding)));
    assert_eq!(over(&last), Some((2, EndReason::LastStanding)));
    assert_eq!(
        session.placements(),
        vec![
            Placement { team: 1, place: 1 },
            Placement { team: 2, place: 1 },
            Placement { team: 3, place: 2 },
        ],
        "the fallen ally shares the win"
    );
}

#[test]
fn surrender_needs_the_threshold_of_the_team_and_respects_the_cooldown() {
    let surrender = SurrenderRules {
        threshold_percent: 60,
        window_ticks: 30,
        cooldown_ticks: 100,
    };
    let seats = [(1, 0), (1, 1), (1, 2), (2, 0)];
    let mut session = started(
        SessionSettings {
            victory: VictoryRules {
                conditions: vec![VictoryCondition::Surrender],
                surrender,
            },
            ..settings(2, 3)
        },
        &seats,
    );
    let vote = |team: u8, slot: u8, yes: bool| FlowInput::SurrenderVote { team, slot, yes };
    let guest = |team: u8, slot: u8| Issuer::guest(SeatId::new(team, slot));
    // Issuer-checked: a seat votes only for itself.
    assert_eq!(
        session.apply_from(guest(2, 0), &vote(1, 0, true)),
        Err(RulesError::NotPermitted)
    );
    // 60 % of three seats is two yes votes.
    session.apply_from(guest(1, 0), &vote(1, 0, true)).expect("yes");
    assert!(eliminated(&session.step(&[]).events).is_empty(), "one of two");
    session.apply_from(guest(1, 1), &vote(1, 1, false)).expect("no");
    assert!(
        eliminated(&session.step(&[]).events).is_empty(),
        "two yes still possible"
    );
    session.apply_from(guest(1, 2), &vote(1, 2, false)).expect("no");
    let failed = session.step(&[]).events;
    let until = failed
        .iter()
        .find_map(|e| match e {
            RulesEvent::SurrenderFailed { team: 1, until, .. } => Some(*until),
            _ => None,
        })
        .expect("two no votes make the threshold unreachable");
    assert_eq!(until, session.flow.match_tick + 100);
    assert_eq!(
        session.apply_from(guest(1, 0), &vote(1, 0, true)),
        Err(RulesError::SurrenderCooldown { team: 1, until })
    );
    // A ballot that never reaches the threshold fails when its window runs out.
    while session.flow.match_tick < until {
        session.step(&[]);
    }
    session
        .apply_from(guest(1, 0), &vote(1, 0, true))
        .expect("open again");
    let mut expired = Vec::new();
    for _ in 0..30 {
        expired.extend(session.step(&[]).events);
    }
    assert!(
        expired
            .iter()
            .any(|e| matches!(e, RulesEvent::SurrenderFailed { team: 1, .. })),
        "the window ran out"
    );
    while session.flow.match_tick < session.victory_state.cooldowns[0].until {
        session.step(&[]);
    }
    // Two yes votes pass: team 1 is out and team 2 wins.
    session.apply_from(guest(1, 0), &vote(1, 0, true)).expect("yes");
    session.apply_from(guest(1, 2), &vote(1, 2, true)).expect("yes");
    let passed = session.step(&[]).events;
    assert_eq!(eliminated(&passed), vec![(1, 2, OutCause::Surrendered)]);
    assert_eq!(victory(&passed), Some((vec![2], EndReason::Surrendered)));
    assert_eq!(over(&passed), Some((2, EndReason::Surrendered)));
    assert_eq!(
        session.apply_from(guest(2, 0), &vote(2, 0, true)),
        Err(RulesError::SurrenderOff),
        "the match is over"
    );

    // Without the surrender condition a vote is refused.
    let mut plain = started(settings(2, 1), &[(1, 0), (2, 0)]);
    assert_eq!(
        plain.apply_from(guest(1, 0), &vote(1, 0, true)),
        Err(RulesError::SurrenderOff)
    );
}

/// Brief name `100_solo_teams_fit_the_board_and_score_vectors` (a Rust name cannot start with a
/// digit).
#[test]
fn hundred_solo_teams_fit_the_board_and_score_vectors() {
    let seats: Vec<(u8, u8)> = (1..=100).map(|team| (team, 0)).collect();
    let mut session = started(
        SessionSettings {
            zone: Some(zone_schedule(3)),
            elimination: EliminationRules {
                last_standing: true,
                ..EliminationRules::default()
            },
            ..settings(100, 1)
        },
        &seats,
    );
    assert_eq!(session.board.team_scores.len(), 101);
    assert_eq!(session.flow.playing_seats().len(), 100);
    let units: Vec<Contestant> = (1..=100u32)
        .map(|id| unit(id, u8::try_from(id).unwrap_or(0), id as f32, 0.0))
        .collect();
    let mut events = Vec::new();
    for team in 1..=99u8 {
        session.note_death(SeatId::new(team, 0));
        events.extend(session.step(&units).events);
    }
    let outs = eliminated(&events);
    assert_eq!(outs.len(), 99);
    for (team, place, _) in &outs {
        assert_eq!(u16::from(*place), 101 - u16::from(*team), "team {team}");
    }
    assert_eq!(over(&events), Some((100, EndReason::LastStanding)));
    let placements = session.placements();
    assert_eq!(placements.len(), 100);
    assert_eq!(placements[0], Placement { team: 100, place: 1 });
    assert_eq!(placements[99], Placement { team: 1, place: 100 });
    assert!(
        events.iter().any(|e| matches!(e, RulesEvent::ZoneDamage { .. })),
        "the zone ran beside the eliminations"
    );
}

#[test]
fn everything_off_leaves_the_session_as_before() {
    let mut session = started(settings(2, 1), &[(1, 0), (2, 0)]);
    session.note_death(SeatId::new(1, 0));
    let mut events = Vec::new();
    for _ in 0..50 {
        events.extend(session.step(&[unit(1, 1, 5000.0, 0.0)]).events);
    }
    assert!(events.iter().all(|e| !matches!(
        e,
        RulesEvent::ZonePhase { .. }
            | RulesEvent::ZoneDamage { .. }
            | RulesEvent::TeamEliminated { .. }
            | RulesEvent::Victory { .. }
    )));
    assert!(over(&events).is_none(), "a seat elimination alone ends nothing");
    assert!(session.placements().is_empty());
}

#[test]
fn surrender_votes_in_any_order_within_a_step_give_one_state() {
    let make = || {
        started(
            SessionSettings {
                victory: VictoryRules {
                    conditions: vec![VictoryCondition::Surrender],
                    surrender: SurrenderRules {
                        threshold_percent: 100,
                        ..SurrenderRules::default()
                    },
                },
                ..settings(2, 3)
            },
            &[(1, 0), (1, 1), (1, 2), (2, 0)],
        )
    };
    let votes = [
        FlowInput::SurrenderVote {
            team: 1,
            slot: 2,
            yes: true,
        },
        FlowInput::SurrenderVote {
            team: 1,
            slot: 0,
            yes: true,
        },
        FlowInput::SurrenderVote {
            team: 1,
            slot: 1,
            yes: true,
        },
    ];
    let mut a = make();
    let mut b = make();
    for v in &votes {
        a.apply(v).expect("vote");
    }
    for v in votes.iter().rev() {
        b.apply(v).expect("vote");
    }
    assert_eq!(a.victory_state, b.victory_state);
    let left = a.step(&[]).events;
    let right = b.step(&[]).events;
    assert_eq!(eliminated(&left), eliminated(&right));
    assert_eq!(over(&left), Some((2, EndReason::Surrendered)));
    assert_eq!(a, b);
}

#[test]
fn a_restored_session_mid_shrink_and_mid_ballot_steps_like_the_original() {
    let mut session = started(
        SessionSettings {
            zone: Some(zone_schedule(11)),
            elimination: EliminationRules {
                last_standing: true,
                ..EliminationRules::default()
            },
            victory: VictoryRules {
                conditions: vec![VictoryCondition::Surrender],
                ..VictoryRules::default()
            },
            ..settings(3, 2)
        },
        &[(1, 0), (1, 1), (2, 0), (3, 0)],
    );
    let units = [
        unit(1, 1, 0.0, 0.0),
        unit(2, 1, 90.0, 0.0),
        unit(3, 2, -90.0, 0.0),
        unit(4, 3, 0.0, 95.0),
    ];
    for _ in 0..40 {
        session.step(&units);
    }
    session.note_death(SeatId::new(3, 0));
    session
        .apply(&FlowInput::SurrenderVote {
            team: 1,
            slot: 0,
            yes: true,
        })
        .expect("vote");
    let text = serde_json::to_string(&session).expect("serialize");
    let mut restored: MatchSession = serde_json::from_str(&text).expect("restore");
    // Pending events are not part of the snapshot: drain both before comparing.
    let _ = session.drain_events();
    assert_eq!(restored, session);
    for _ in 0..80 {
        assert_eq!(restored.step(&units), session.step(&units));
    }
    assert_eq!(restored, session);
    assert_eq!(session.zone_state.stage, ZoneStage::Shrinking);
}
