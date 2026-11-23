// SPDX-License-Identifier: MPL-2.0
//! Audit gap GR-7.6: the rules-core side of the logic graph's `rules::*` tasks. Objective locks
//! (`ObjectiveBoard::set_locked`), scripted team scores (`MatchSession::add_team_score`) and
//! scripted seat scores (`MatchSession::add_seat_score`). A board that never locks keeps its
//! serialized bits.

use regia_rules::match_flow::SeatId;
use regia_rules::mode::MatchMode;
use regia_rules::objective::{
    Contestant, HillVolume, ObjectiveBoard, ObjectiveSettings, RulesEvent, ScoreReason,
};
use regia_rules::session::{MatchSession, SessionSettings};
use regia_rules::teams::SeatRequest;

const BASE1: [f32; 3] = [0.0, 0.0, 0.0];
const BASE2: [f32; 3] = [100.0, 0.0, 0.0];

fn unit(id: u32, team: u8, position: [f32; 3]) -> Contestant {
    Contestant {
        id,
        team,
        position,
        alive: true,
    }
}

fn ctf() -> ObjectiveBoard {
    let mut board = ObjectiveBoard::new(MatchMode::CaptureTheFlag, ObjectiveSettings::default());
    board.spawn_flag(1, 1, BASE1).expect("flag 1");
    board.spawn_flag(2, 2, BASE2).expect("flag 2");
    board
}

#[test]
fn a_locked_flag_is_not_taken_until_it_is_unlocked() {
    let mut board = ctf();
    assert!(board.set_locked(2, true));
    assert!(board.is_locked(2));
    // Locking twice keeps one entry.
    assert!(board.set_locked(2, true));
    assert_eq!(board.locked, vec![2]);
    board.ctf_tick(1, &[unit(1001, 1, BASE2)]);
    let flag = board.objective(2).expect("flag");
    assert!(
        flag.at_base && flag.carrier_id.is_none(),
        "a locked flag takes no touch"
    );
    assert!(board.drain_events().is_empty());

    assert!(board.set_locked(2, false));
    assert!(board.locked.is_empty());
    board.ctf_tick(2, &[unit(1001, 1, BASE2)]);
    assert_eq!(board.objective(2).and_then(|f| f.carrier_id), Some(1001));
    assert!(matches!(
        board.drain_events().as_slice(),
        [RulesEvent::FlagTaken {
            flag: 2,
            unit: 1001,
            ..
        }]
    ));
}

#[test]
fn a_carried_flag_locked_on_the_way_home_does_not_capture() {
    let mut board = ctf();
    board.ctf_tick(1, &[unit(1001, 1, BASE2)]);
    assert_eq!(board.objective(2).and_then(|f| f.carrier_id), Some(1001));
    assert!(board.set_locked(2, true));
    // The carrier reaches its own base with its own flag home: no capture while locked.
    board.ctf_tick(2, &[unit(1001, 1, BASE1)]);
    assert_eq!(board.team_scores[1], 0);
    let flag = board.objective(2).expect("flag");
    assert_eq!(flag.carrier_id, Some(1001), "the carrier keeps it");
    assert_eq!(flag.position, BASE1, "and it follows the carrier");
    assert!(board.set_locked(2, false));
    board.ctf_tick(3, &[unit(1001, 1, BASE1)]);
    assert_eq!(board.team_scores[1], 1, "unlocked, it captures");
}

#[test]
fn a_locked_hill_scores_no_points() {
    let mut board = ObjectiveBoard::new(MatchMode::KingOfTheHill, ObjectiveSettings::default());
    let volume = HillVolume {
        min: [-5.0, -1.0, -5.0],
        max: [5.0, 5.0, 5.0],
    };
    board.install_hill_volume(9, volume).expect("hill");
    let period = u64::from(board.settings.effective_hill_accrual_ticks());
    assert!(board.set_locked(9, true));
    board.koth_tick(period, &[unit(1001, 1, [0.0, 0.0, 0.0])]);
    assert_eq!(board.team_scores[1], 0);
    assert!(board.set_locked(9, false));
    board.koth_tick(2 * period, &[unit(1001, 1, [0.0, 0.0, 0.0])]);
    assert_eq!(board.team_scores[1], 1);
}

#[test]
fn locking_an_unknown_objective_changes_nothing() {
    let mut board = ctf();
    assert!(!board.set_locked(77, true));
    assert!(board.locked.is_empty());
    assert!(!board.is_locked(77));
}

#[test]
fn locks_survive_a_round_start_and_a_new_match_clears_them() {
    let mut board = ctf();
    assert!(board.set_locked(1, true));
    board.reset_play();
    assert!(board.is_locked(1));
    board.reset(MatchMode::CaptureTheFlag);
    assert!(board.locked.is_empty());
}

#[test]
fn a_board_without_locks_serializes_as_before() {
    let board = ctf();
    let json = serde_json::to_string(&board).expect("json");
    assert!(!json.contains("locked"), "{json}");
    let mut locked = ctf();
    assert!(locked.set_locked(1, true));
    let json = serde_json::to_string(&locked).expect("json");
    assert!(json.contains("\"locked\":[1]"), "{json}");
    let back: ObjectiveBoard = serde_json::from_str(&json).expect("read back");
    assert!(back.is_locked(1));
}

/// A rules file or a peer snapshot can store `locked` out of order. Flag 2 is in the list, so a
/// touch at its base must not take it. `is_locked` binary-searches the vec.
#[test]
fn an_unsorted_lock_list_from_a_rules_file_blocks_the_touch() {
    let mut hostile = ctf();
    hostile.locked = vec![2, 1];
    let json = serde_json::to_string(&hostile).expect("json");
    assert!(
        json.contains("\"locked\":[2,1]"),
        "the file keeps the hostile order: {json}"
    );
    let mut board: ObjectiveBoard = serde_json::from_str(&json).expect("reads");
    board.ctf_tick(1, &[unit(1001, 1, BASE2)]);
    assert_eq!(
        board.objective(2).and_then(|flag| flag.carrier_id),
        None,
        "flag 2 is listed in locked, so the touch must not take it; locked={:?}",
        board.locked
    );
}

/// One unlock of an id the unsorted list already holds must drop that id, and locking it again
/// must not insert a second copy.
#[test]
fn an_unsorted_lock_list_unlocks_without_growing_a_duplicate() {
    let mut board = ctf();
    board.locked = vec![2, 1];
    assert!(board.set_locked(2, false));
    assert!(
        !board.locked.contains(&2),
        "unlock removes 2, got {:?}",
        board.locked
    );
    assert!(!board.is_locked(2));

    board.locked = vec![2, 1];
    assert!(board.set_locked(2, true));
    assert_eq!(
        board.locked.iter().filter(|id| **id == 2).count(),
        1,
        "locking an id the list already holds stays unique, got {:?}",
        board.locked
    );
}

/// A file that repeats an id is still one lock. One unlock clears it.
#[test]
fn duplicate_lock_entries_clear_on_one_unlock() {
    let mut board = ctf();
    board.locked = vec![1, 1];
    assert!(board.set_locked(1, false));
    assert!(
        !board.locked.contains(&1),
        "one unlock clears every copy, got {:?}",
        board.locked
    );
    assert!(!board.is_locked(1));
}

fn session() -> MatchSession {
    let mut session = MatchSession::new(SessionSettings::default());
    session.start_now();
    let _ = session.drain_events();
    session
}

#[test]
fn a_scripted_team_score_clamps_and_refuses_neutral_teams_and_ended_matches() {
    let mut session = session();
    assert_eq!(session.add_team_score(2, 3), Some(3));
    assert_eq!(session.add_team_score(2, -5), Some(0), "never below zero");
    assert_eq!(
        session.add_team_score(2, i64::MAX),
        Some(u32::MAX),
        "never above u32::MAX"
    );
    assert_eq!(session.add_team_score(0, 1), None, "team 0 is neutral");
    assert_eq!(session.add_team_score(200, 1), None, "past max_teams");
    // No event and no match end from a scripted team score.
    assert!(!session.board.match_over);
    assert!(session.drain_events().is_empty());
    session.report_condition(1, regia_rules::objective::EndReason::Ended);
    assert!(session.board.match_over);
    assert_eq!(session.add_team_score(1, 1), None, "nothing scores after the end");
}

#[test]
fn a_scripted_seat_score_lands_on_the_ledger_with_a_scripted_event() {
    let mut session = session();
    let seats = session
        .assign_team(
            &SeatRequest {
                team: Some(1),
                ..SeatRequest::default()
            },
            &[],
        )
        .expect("seat");
    let seat = seats[0];
    let _ = session.drain_events();
    assert_eq!(session.add_seat_score(seat, 5), Some(5));
    assert_eq!(session.add_seat_score(seat, -2), Some(3));
    let row = session.ledgers.row(seat).expect("row");
    assert_eq!((row.score, row.kills, row.objective_points), (3, 0, 0));
    let events = session.drain_events();
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[1],
        RulesEvent::PlayerScored {
            team: 1,
            slot: 0,
            reason: ScoreReason::Scripted,
            score: 3,
            ..
        }
    ));
    // Nobody holds team 2's seat 0: refused, and no row opens there.
    let empty = SeatId::new(2, 0);
    assert_eq!(session.add_seat_score(empty, 1), None);
    assert!(session.ledgers.row(empty).is_none());
}

/// ADR-8731 (v0.7 roll-up): `ObjectiveBoard::set_locked` on a capture zone (ADR-8649) stops it the
/// way the zone mode's own lock does: no progress, no capture, no owner points. Unlocking resumes
/// from the progress it had.
#[test]
fn a_locked_capture_zone_takes_no_progress_until_it_is_unlocked() {
    use regia_rules::zones::ZoneSettings;
    let mut board = ObjectiveBoard::new(MatchMode::Domination, ObjectiveSettings::default());
    let settings = ZoneSettings {
        capture_ticks: 4,
        owner_scores_every: Some(1),
        ..ZoneSettings::new(HillVolume {
            min: [-5.0, -1.0, -5.0],
            max: [5.0, 9.0, 5.0],
        })
    };
    board.install_zone(7, settings).expect("zone");
    let inside = [unit(1001, 1, BASE1)];
    board.zones_tick(1, &inside);
    assert_eq!(board.zone(7).map(|z| z.progress), Some(1));
    assert!(board.set_locked(7, true));
    for tick in 2..10 {
        board.zones_tick(tick, &inside);
    }
    let zone = board.zone(7).expect("zone");
    assert_eq!((zone.owner, zone.progress), (0, 1), "a locked zone does not move");
    assert!(!zone.locked, "the host lock is the board's, not the zone mode's");
    assert_eq!(board.team_scores[1], 0);
    assert!(board.set_locked(7, false));
    for tick in 10..13 {
        board.zones_tick(tick, &inside);
    }
    assert_eq!(board.zone(7).map(|z| z.owner), Some(1), "unlocked, it flips");
}
