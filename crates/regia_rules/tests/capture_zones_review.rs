// SPDX-License-Identifier: MPL-2.0
//! Proof tests for the capture-zones review (lane rules-capture-zones-review).
//!
//! These tests state the behavior the zone rules should have. On reviewed head
//! 9c0014c0ae292c01964d7db1d4d5786027a85082 the `proof_*` tests fail.
//! `a_restored_session_steps_like_the_uninterrupted_one` is a check, not a finding.
//!
//! Every clock is a tick count. No wall clock.

use regia_rules::RulesError;
use regia_rules::match_flow::{FlowInput, FlowSettings};
use regia_rules::mode::MatchMode;
use regia_rules::objective::{Contestant, HillVolume, ObjectiveBoard, RulesEvent};
use regia_rules::phases::{MatchPhase, PhaseSettings, RoundSettings};
use regia_rules::playlist::PlaylistEntry;
use regia_rules::session::{MatchSession, SessionSettings};
use regia_rules::teams::SeatRequest;
use regia_rules::zones::{ZoneMode, ZoneSettings};

fn volume_at(x: f32) -> HillVolume {
    HillVolume {
        min: [x - 5.0, -1.0, -5.0],
        max: [x + 5.0, 9.0, 5.0],
    }
}

fn inside(n: u32) -> [f32; 3] {
    [100.0 * n as f32, 0.0, 0.0]
}

fn unit(id: u32, team: u8, position: [f32; 3]) -> Contestant {
    Contestant {
        id,
        team,
        position,
        alive: true,
    }
}

fn zone_settings(n: u32) -> ZoneSettings {
    ZoneSettings {
        capture_ticks: 6,
        owner_scores_every: None,
        ..ZoneSettings::new(volume_at(100.0 * n as f32))
    }
}

fn board_with(ids: &[u32], settings: impl Fn(u32) -> ZoneSettings) -> ObjectiveBoard {
    let mut board = ObjectiveBoard::new(MatchMode::Domination, Default::default());
    for (n, id) in ids.iter().enumerate() {
        board.install_zone(*id, settings(n as u32)).expect("zone");
    }
    board
}

fn run(board: &mut ObjectiveBoard, tick: &mut u64, ticks: u64, units: &[Contestant]) {
    for _ in 0..ticks {
        *tick += 1;
        board.zones_tick(*tick, units);
        board.drain_events();
    }
}

/// Rounds turn a zone score limit into a round end and reopen the board. Every zone must
/// still take that tick: the phase contract evaluates the end after the objective rules.
#[test]
fn proof_a_round_ending_score_limit_still_ticks_later_zones() {
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 0,
            phases: PhaseSettings {
                time_limit_ticks: Some(10_000),
                rounds: Some(RoundSettings {
                    rounds_to_win: 2,
                    max_rounds: 3,
                    swap_sides_every: 0,
                    round_end_ticks: 2,
                }),
                ..PhaseSettings::default()
            },
            ..FlowSettings::default()
        },
        ..SessionSettings::default()
    });
    session.playlist.set(vec![PlaylistEntry::new("depot", "dom")]);
    let seats = session
        .assign_team(
            &SeatRequest {
                unit: Some(1),
                team: Some(1),
                ..SeatRequest::default()
            },
            &[],
        )
        .expect("seat");
    session.start_now();
    // Same box, so one unit stands in both. Lower entity id ticks first.
    let shared = volume_at(0.0);
    session
        .board
        .install_zone(
            1,
            ZoneSettings {
                capture_ticks: 1,
                owner_scores_every: Some(1),
                ..ZoneSettings::new(shared)
            },
        )
        .expect("zone 1");
    session
        .board
        .install_zone(
            2,
            ZoneSettings {
                capture_ticks: 1,
                owner_scores_every: None,
                ..ZoneSettings::new(shared)
            },
        )
        .expect("zone 2");
    session
        .board
        .set_zone_rules(ZoneMode::Domination, Some(1))
        .expect("score limit");
    let step = session.step(&[unit(1, 1, [0.0, 0.0, 0.0])]);
    let zone1 = *session.board.zone(1).expect("zone 1");
    let zone2 = *session.board.zone(2).expect("zone 2");
    let points = session.ledgers.row(seats[0]).map(|row| row.objective_points);
    assert!(
        step.events
            .iter()
            .any(|event| matches!(event, RulesEvent::RoundOver { team: 1, .. })),
        "the score limit ends the round, not the match: {:?}",
        step.events
    );
    assert_eq!(
        (
            session.board.match_over,
            session.flow.phases.phase,
            zone1.owner,
            zone2.owner,
            points
        ),
        (false, MatchPhase::RoundEnd, 1, 1, Some(2)),
        "zone 2 must capture on the same tick, and the unit is credited for both captures"
    );
}

/// A repeated id in an attack/defend order leaves the only zone locked, so the attackers
/// can never take it. The book says the first zone of the order is unlocked.
#[test]
fn proof_a_repeated_attack_order_is_refused() {
    let mut board = board_with(&[7], zone_settings);
    let before = board.clone();
    let result = board.set_zone_rules(
        ZoneMode::AttackDefend {
            attackers: 1,
            defenders: 2,
            order: vec![7, 7],
        },
        None,
    );
    let locked = board.zone(7).map(|zone| zone.locked);
    assert_eq!(
        result,
        Err(RulesError::ZoneSettings { field: "order" }),
        "order [7, 7] left the zone locked={locked:?}"
    );
    assert_eq!(board, before, "a refused list leaves the board unchanged");
}

/// A repeated hardpoint id locks the zone `rotation[0]` was supposed to unlock.
#[test]
fn proof_a_repeated_hardpoint_rotation_is_refused() {
    let mut board = board_with(&[7], zone_settings);
    let before = board.clone();
    let result = board.set_zone_rules(
        ZoneMode::Hardpoint {
            rotation: vec![7, 7],
            rotate_ticks: 10,
        },
        None,
    );
    let locked = board.zone(7).map(|zone| zone.locked);
    assert_eq!(
        result,
        Err(RulesError::ZoneSettings { field: "rotation" }),
        "rotation [7, 7] left the zone locked={locked:?}"
    );
    assert_eq!(board, before, "a refused list leaves the board unchanged");
}

/// Switching to attack/defend grants the defenders a fresh zone. Team 1's hold timer and
/// an unfinished capture bar must not carry over.
#[test]
fn proof_attack_defend_does_not_keep_the_previous_hold() {
    let mut board = board_with(&[7], |_| ZoneSettings {
        capture_ticks: 1,
        owner_scores_every: Some(10),
        ..zone_settings(0)
    });
    let mut tick = 0;
    run(&mut board, &mut tick, 9, &[unit(1, 1, inside(0))]);
    assert_eq!(board.zone(7).expect("zone").owner, 1);
    assert_eq!(board.zone(7).expect("zone").held_ticks, 9);
    assert_eq!(board.score(1), 0, "the tenth held tick has not happened");
    board
        .set_zone_rules(
            ZoneMode::AttackDefend {
                attackers: 1,
                defenders: 2,
                order: vec![7],
            },
            None,
        )
        .expect("attack/defend");
    let zone = *board.zone(7).expect("zone");
    assert_eq!(
        (zone.owner, zone.held_ticks, zone.progress, zone.capturing_team),
        (2, 0, 0, 0),
        "defenders own a fresh zone; the next tick must not score team 1's ninth hold"
    );
}

/// A partial capture bar belongs to the team that was pushing. Granting the zone to the
/// defenders must clear it, or the next push unwinds that leftover bar.
#[test]
fn proof_attack_defend_does_not_keep_a_partial_capture_bar() {
    let mut board = board_with(&[8], |_| ZoneSettings {
        capture_ticks: 10,
        owner_scores_every: None,
        ..zone_settings(0)
    });
    let mut tick = 0;
    run(&mut board, &mut tick, 4, &[unit(1, 1, inside(0))]);
    assert_eq!(board.zone(8).expect("zone").capturing_team, 1);
    assert_eq!(board.zone(8).expect("zone").progress, 4);
    board
        .set_zone_rules(
            ZoneMode::AttackDefend {
                attackers: 1,
                defenders: 2,
                order: vec![8],
            },
            None,
        )
        .expect("attack/defend");
    let zone = *board.zone(8).expect("zone");
    assert_eq!(
        (zone.owner, zone.held_ticks, zone.progress, zone.capturing_team),
        (2, 0, 0, 0),
        "a partial bar for team 1 must not remain on a zone just given to the defenders"
    );
}

/// Snapshot after a step, then one more step, matches a session that was not snapshotted.
#[test]
fn a_restored_session_steps_like_the_uninterrupted_one() {
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 0,
            phases: PhaseSettings {
                time_limit_ticks: Some(10_000),
                rounds: Some(RoundSettings {
                    rounds_to_win: 2,
                    max_rounds: 3,
                    swap_sides_every: 0,
                    round_end_ticks: 2,
                }),
                ..PhaseSettings::default()
            },
            ..FlowSettings::default()
        },
        ..SessionSettings::default()
    });
    session
        .apply(&FlowInput::Seat { team: 1, slot: 0 })
        .expect("seat");
    session.playlist.set(vec![PlaylistEntry::new("depot", "dom")]);
    session.start_now();
    session
        .board
        .install_zone(
            4,
            ZoneSettings {
                capture_ticks: 6,
                owner_scores_every: Some(4),
                decay_ticks: Some(2),
                ..zone_settings(0)
            },
        )
        .expect("zone");
    session.board.install_zone(5, zone_settings(1)).expect("zone");
    session
        .board
        .set_zone_rules(
            ZoneMode::Hardpoint {
                rotation: vec![4, 5],
                rotate_ticks: 10,
            },
            None,
        )
        .expect("hardpoint");
    let units = [unit(1, 1, inside(0)), unit(2, 2, inside(1))];
    for _ in 0..4 {
        session.step(&units);
    }
    let text = serde_json::to_string(&session).expect("serialize");
    let mut restored: MatchSession = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(
        restored.board.zone(4).map(|zone| zone.progress),
        session.board.zone(4).map(|zone| zone.progress)
    );
    let live = session.step(&units);
    let back = restored.step(&units);
    assert_eq!(back, live);
    assert_eq!(restored, session);
}
