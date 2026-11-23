// SPDX-License-Identifier: MPL-2.0
//! Capture zones (ADR-8649, audit gaps GR-6.3 to GR-6.5): integer capture progress, owners, decay,
//! contest rules by numbers, owner scoring, and the domination, hardpoint and attack/defend modes.
//! King of the hill keeps S-14's sampled rule.
//!
//! Every clock is a tick count and every script a fixed list of steps: no wall clock.

use regia_rules::RulesError;
use regia_rules::match_flow::{FlowInput, FlowSettings};
use regia_rules::mode::MatchMode;
use regia_rules::objective::{
    Contestant, EndReason, HillVolume, ObjectiveBoard, ObjectiveKind, ObjectiveSettings, RulesEvent,
};
use regia_rules::phases::{PhaseSettings, TimeUp};
use regia_rules::playlist::PlaylistEntry;
use regia_rules::session::{MatchSession, SessionSettings};
use regia_rules::zones::{Contest, ZoneMode, ZoneSettings};

/// A 10 x 10 x 10 box centered on `x` along the X axis.
fn volume_at(x: f32) -> HillVolume {
    HillVolume {
        min: [x - 5.0, -1.0, -5.0],
        max: [x + 5.0, 9.0, 5.0],
    }
}

/// Zone `n` (0, 1, 2, ...) sits at `x = 100 * n`; this is a point inside it.
fn inside(n: u32) -> [f32; 3] {
    [100.0 * n as f32, 0.0, 0.0]
}

const NOWHERE: [f32; 3] = [-1000.0, 0.0, -1000.0];

fn unit(id: u32, team: u8, position: [f32; 3]) -> Contestant {
    Contestant {
        id,
        team,
        position,
        alive: true,
    }
}

/// `count` living units of `team` at `position`, ids from `first`.
fn squad(first: u32, team: u8, count: u32, position: [f32; 3]) -> Vec<Contestant> {
    (0..count).map(|i| unit(first + i, team, position)).collect()
}

fn zone_settings(n: u32) -> ZoneSettings {
    ZoneSettings {
        capture_ticks: 6,
        owner_scores_every: None,
        ..ZoneSettings::new(volume_at(100.0 * n as f32))
    }
}

/// A domination board with zones `ids[i]` at `inside(i)`, built from `settings(i)`.
fn board_with(ids: &[u32], settings: impl Fn(u32) -> ZoneSettings) -> ObjectiveBoard {
    let mut board = ObjectiveBoard::new(MatchMode::Domination, ObjectiveSettings::default());
    for (n, id) in ids.iter().enumerate() {
        let n = n as u32;
        board.install_zone(*id, settings(n)).expect("zone");
    }
    board
}

/// Run `ticks` zone ticks from `*tick + 1` with the same units, collecting the events.
fn run(
    board: &mut ObjectiveBoard,
    tick: &mut u64,
    ticks: u64,
    units: &[Contestant],
    log: &mut Vec<RulesEvent>,
) {
    for _ in 0..ticks {
        *tick += 1;
        board.zones_tick(*tick, units);
        log.extend(board.drain_events());
    }
}

fn state(board: &ObjectiveBoard, id: u32) -> (u8, u8, u64) {
    let zone = board.zone(id).expect("zone");
    (zone.owner, zone.capturing_team, zone.progress)
}

#[test]
fn three_zones_capture_with_progress_and_flip_owner() {
    let mut board = board_with(&[10, 20, 30], |n| ZoneSettings {
        owner_scores_every: Some(4),
        ..zone_settings(n)
    });
    // A second, third zone install is accepted (KOTH refuses a second hill).
    assert_eq!(board.zone_ids(), vec![10, 20, 30]);
    assert!(board.objectives.iter().all(|o| o.kind == ObjectiveKind::Zone));
    let (mut tick, mut log) = (0, Vec::new());
    let units = [unit(1, 1, inside(0)), unit(2, 2, inside(1))];
    run(&mut board, &mut tick, 3, &units, &mut log);
    assert_eq!(state(&board, 10), (0, 1, 3), "team 1 halfway on zone 10");
    assert_eq!(state(&board, 20), (0, 2, 3), "team 2 halfway on zone 20");
    assert_eq!(state(&board, 30), (0, 0, 0), "nobody on zone 30");
    assert!(log.is_empty());
    run(&mut board, &mut tick, 3, &units, &mut log);
    assert_eq!(state(&board, 10), (1, 0, 0));
    assert_eq!(state(&board, 20), (2, 0, 0));
    assert_eq!(
        log,
        vec![
            RulesEvent::ZoneCaptured {
                tick: 6,
                zone: 10,
                team: 1,
                previous: 0
            },
            RulesEvent::ZoneCaptured {
                tick: 6,
                zone: 20,
                team: 2,
                previous: 0
            },
        ]
    );
    // Owners score every fourth held tick; the flip tick is the first held tick.
    log.clear();
    run(&mut board, &mut tick, 3, &units, &mut log);
    assert_eq!(board.team_scores[1..3], [1, 1]);
    assert!(log.contains(&RulesEvent::ZonePoint {
        tick: 9,
        zone: 10,
        team: 1,
        score: 1
    }));
    // Team 2 walks onto zone 10 alone and takes it from team 1.
    log.clear();
    let units = [unit(1, 1, NOWHERE), unit(2, 2, inside(0))];
    run(&mut board, &mut tick, 5, &units, &mut log);
    assert_eq!(state(&board, 10), (1, 2, 5), "team 2 one tick short");
    run(&mut board, &mut tick, 1, &units, &mut log);
    assert_eq!(state(&board, 10), (2, 0, 0));
    assert!(log.contains(&RulesEvent::ZoneCaptured {
        tick: 15,
        zone: 10,
        team: 2,
        previous: 1
    }));
    // Zone 20 stayed team 2's while empty and kept scoring.
    assert_eq!(state(&board, 20).0, 2);
    assert_eq!(
        board.score(1),
        2,
        "team 1 scored on held ticks 4 and 8 of zone 10"
    );
}

#[test]
fn a_contested_zone_freezes_and_majority_mode_moves_toward_the_larger_team() {
    let mut board = board_with(&[1, 2, 3], |n| ZoneSettings {
        capture_ticks: 100,
        max_rate_units: 8,
        contest: [Contest::AnyEnemyBlocks, Contest::Majority, Contest::Weighted][n as usize],
        ..zone_settings(n)
    });
    let (mut tick, mut log) = (0, Vec::new());
    // Team 1 alone on every zone for two ticks: one unit each, rate 1.
    let alone: Vec<Contestant> = (0..3).map(|n| unit(10 + n, 1, inside(n))).collect();
    run(&mut board, &mut tick, 2, &alone, &mut log);
    for id in [1, 2, 3] {
        assert_eq!(state(&board, id), (0, 1, 2));
    }
    // Three of team 1 against one of team 2 on every zone.
    let mut crowd = Vec::new();
    for n in 0..3 {
        crowd.extend(squad(100 + 10 * n, 1, 3, inside(n)));
        crowd.push(unit(200 + n, 2, inside(n)));
    }
    run(&mut board, &mut tick, 5, &crowd, &mut log);
    assert_eq!(state(&board, 1), (0, 1, 2), "any enemy freezes the zone");
    assert_eq!(
        state(&board, 2),
        (0, 1, 2 + 5 * 3),
        "majority moves at the larger count"
    );
    assert_eq!(
        state(&board, 3),
        (0, 1, 2 + 5 * 2),
        "weighted moves at the difference"
    );
    // Two against two: majority and weighted freeze too.
    let mut even = Vec::new();
    for n in 0..3 {
        even.extend(squad(100 + 10 * n, 1, 2, inside(n)));
        even.extend(squad(200 + 10 * n, 2, 2, inside(n)));
    }
    run(&mut board, &mut tick, 4, &even, &mut log);
    assert_eq!(state(&board, 1), (0, 1, 2));
    assert_eq!(state(&board, 2), (0, 1, 17));
    assert_eq!(state(&board, 3), (0, 1, 12));
    // Three of team 2 against one of team 1 on the majority zone unwinds team 1 first.
    let mut turned = squad(300, 2, 3, inside(1));
    turned.push(unit(400, 1, inside(1)));
    run(&mut board, &mut tick, 5, &turned, &mut log);
    assert_eq!(state(&board, 2), (0, 1, 2), "17 - 5 * 3");
    run(&mut board, &mut tick, 1, &turned, &mut log);
    assert_eq!(state(&board, 2), (0, 0, 0), "unwound to neutral, no carry-over");
    run(&mut board, &mut tick, 1, &turned, &mut log);
    assert_eq!(state(&board, 2), (0, 2, 3), "now moving toward team 2");
    assert!(log.is_empty(), "no zone flipped: {log:?}");
}

#[test]
fn an_empty_zone_decays_to_neutral() {
    let mut board = board_with(&[1, 2], |n| ZoneSettings {
        capture_ticks: 10,
        decay_ticks: if n == 0 { Some(2) } else { None },
        ..zone_settings(n)
    });
    let (mut tick, mut log) = (0, Vec::new());
    let on = [unit(1, 3, inside(0)), unit(2, 3, inside(1))];
    run(&mut board, &mut tick, 4, &on, &mut log);
    assert_eq!(state(&board, 1), (0, 3, 4));
    assert_eq!(state(&board, 2), (0, 3, 4));
    run(&mut board, &mut tick, 4, &[], &mut log);
    assert_eq!(state(&board, 1), (0, 3, 2), "one unit every two empty ticks");
    assert_eq!(state(&board, 2), (0, 3, 4), "no decay setting holds progress");
    run(&mut board, &mut tick, 4, &[], &mut log);
    assert_eq!(state(&board, 1), (0, 0, 0), "decayed to neutral");
    run(&mut board, &mut tick, 20, &[], &mut log);
    assert_eq!(state(&board, 1), (0, 0, 0));
    // A dead unit inside is the same as an empty zone.
    let dead = Contestant {
        alive: false,
        ..unit(9, 1, inside(1))
    };
    run(&mut board, &mut tick, 6, &[dead], &mut log);
    assert_eq!(state(&board, 2), (0, 3, 4));
    // An owned zone keeps its owner while empty; only partial progress decays.
    let mut owned = board_with(&[5], |n| ZoneSettings {
        capture_ticks: 3,
        decay_ticks: Some(1),
        ..zone_settings(n)
    });
    run(&mut owned, &mut tick, 3, &[unit(1, 1, inside(0))], &mut log);
    assert_eq!(state(&owned, 5), (1, 0, 0));
    run(&mut owned, &mut tick, 2, &[unit(2, 2, inside(0))], &mut log);
    assert_eq!(state(&owned, 5), (1, 2, 2));
    run(&mut owned, &mut tick, 5, &[], &mut log);
    assert_eq!(
        state(&owned, 5),
        (1, 0, 0),
        "back to team 1's, no capture running"
    );
}

#[test]
fn hardpoint_rotates_the_active_zone_on_schedule() {
    let mut board = board_with(&[1, 2, 3], zone_settings);
    board
        .set_zone_rules(
            ZoneMode::Hardpoint {
                rotation: vec![1, 2, 3],
                rotate_ticks: 10,
            },
            None,
        )
        .expect("hardpoint");
    let locked = |b: &ObjectiveBoard| -> Vec<bool> {
        [1, 2, 3]
            .iter()
            .map(|id| b.zone(*id).expect("zone").locked)
            .collect()
    };
    assert_eq!(locked(&board), [false, true, true]);
    let (mut tick, mut log) = (0, Vec::new());
    // Team 1 waits on zone 2 and team 2 holds zone 1.
    let units = [unit(1, 1, inside(1)), unit(2, 2, inside(0))];
    run(&mut board, &mut tick, 10, &units, &mut log);
    assert_eq!(state(&board, 2), (0, 0, 0), "a locked zone takes no progress");
    assert_eq!(state(&board, 1).0, 2, "team 2 took the active zone");
    run(&mut board, &mut tick, 1, &units, &mut log);
    assert_eq!(locked(&board), [true, false, true]);
    assert_eq!(
        state(&board, 1),
        (0, 0, 0),
        "the old hardpoint goes back to neutral"
    );
    assert_eq!(
        state(&board, 2),
        (0, 1, 1),
        "the new one takes progress on its first tick"
    );
    run(&mut board, &mut tick, 29, &units, &mut log);
    let activated: Vec<(u64, u32)> = log
        .iter()
        .filter_map(|e| match e {
            RulesEvent::ZoneActivated { tick, zone } => Some((*tick, *zone)),
            _ => None,
        })
        .collect();
    assert_eq!(activated, [(11, 2), (21, 3), (31, 1)], "wraps to the first zone");
    assert_eq!(locked(&board), [false, true, true]);
    assert_eq!(board.zone_rules.active, 0);
}

#[test]
fn attack_defend_unlocks_zones_in_order_and_ends_when_the_last_falls() {
    let mut board = board_with(&[7, 8], |n| ZoneSettings {
        capture_ticks: 5,
        ..zone_settings(n)
    });
    board
        .set_zone_rules(
            ZoneMode::AttackDefend {
                attackers: 1,
                defenders: 2,
                order: vec![7, 8],
            },
            None,
        )
        .expect("attack/defend");
    assert_eq!(state(&board, 7), (2, 0, 0), "the defenders own every zone");
    assert_eq!(state(&board, 8), (2, 0, 0));
    assert!(board.zone(8).expect("zone").locked);
    let (mut tick, mut log) = (0, Vec::new());
    // An attacker on the second zone gets nothing while it is locked.
    run(&mut board, &mut tick, 6, &[unit(1, 1, inside(1))], &mut log);
    assert_eq!(state(&board, 8), (2, 0, 0));
    run(&mut board, &mut tick, 5, &[unit(1, 1, inside(0))], &mut log);
    assert_eq!(state(&board, 7), (1, 0, 0));
    assert!(board.zone(7).expect("zone").locked, "a fallen zone stays locked");
    assert!(!board.zone(8).expect("zone").locked);
    assert_eq!(
        log,
        vec![
            RulesEvent::ZoneCaptured {
                tick: 11,
                zone: 7,
                team: 1,
                previous: 2
            },
            RulesEvent::ZoneActivated { tick: 11, zone: 8 },
        ]
    );
    // A defender on the last zone freezes it; the attackers then take it alone.
    let contested = [unit(1, 1, inside(1)), unit(2, 2, inside(1))];
    run(&mut board, &mut tick, 2, &[unit(1, 1, inside(1))], &mut log);
    run(&mut board, &mut tick, 10, &contested, &mut log);
    assert_eq!(state(&board, 8), (2, 1, 2));
    assert!(!board.match_over);
    run(&mut board, &mut tick, 3, &[unit(1, 1, inside(1))], &mut log);
    assert!(board.match_over);
    assert_eq!(board.end_reason, Some(EndReason::ZonesCaptured));
    assert_eq!(
        log.last(),
        Some(&RulesEvent::MatchOver {
            tick: 26,
            team: 1,
            reason: EndReason::ZonesCaptured
        })
    );
    // Nothing moves after the match is over.
    let frozen = board.clone();
    run(&mut board, &mut tick, 5, &[unit(2, 2, inside(1))], &mut log);
    assert_eq!(board.objectives, frozen.objectives);
}

#[test]
fn attack_defend_defenders_win_at_time_up() {
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 0,
            phases: PhaseSettings {
                time_limit_ticks: Some(30),
                on_time_up: TimeUp::End,
                ..PhaseSettings::default()
            },
            ..FlowSettings::default()
        },
        ..SessionSettings::default()
    });
    session.playlist.set(vec![PlaylistEntry::new("depot", "dom")]);
    for team in [1, 2] {
        session.apply(&FlowInput::Seat { team, slot: 0 }).expect("seat");
    }
    session.start_now();
    assert_eq!(session.board.mode, MatchMode::Domination);
    session
        .board
        .install_zone(
            7,
            ZoneSettings {
                capture_ticks: 1000,
                ..zone_settings(0)
            },
        )
        .expect("zone");
    session
        .board
        .set_zone_rules(
            ZoneMode::AttackDefend {
                attackers: 1,
                defenders: 2,
                order: vec![7],
            },
            None,
        )
        .expect("rules");
    let mut events = Vec::new();
    for _ in 0..40 {
        events.extend(session.step(&[unit(1, 1, inside(0))]).events);
    }
    let over: Vec<&RulesEvent> = events
        .iter()
        .filter(|e| matches!(e, RulesEvent::MatchOver { .. }))
        .collect();
    assert_eq!(over.len(), 1, "{over:?}");
    assert!(
        matches!(
            over[0],
            RulesEvent::MatchOver {
                team: 2,
                reason: EndReason::TimeLimit,
                ..
            }
        ),
        "{over:?}"
    );
    assert!(
        session.board.zone(7).expect("zone").progress > 0,
        "attackers were capturing"
    );
}

#[test]
fn capture_rate_caps_at_max_rate_units() {
    let mut board = board_with(&[1, 2, 3], |n| ZoneSettings {
        capture_ticks: 100,
        max_rate_units: [3, 0, 10][n as usize],
        ..zone_settings(n)
    });
    let mut units = Vec::new();
    for n in 0..3 {
        units.extend(squad(10 * n, 1, 5, inside(n)));
    }
    let (mut tick, mut log) = (0, Vec::new());
    run(&mut board, &mut tick, 1, &units, &mut log);
    assert_eq!(state(&board, 1).2, 3, "five units, capped at three");
    assert_eq!(state(&board, 2).2, 1, "a zero cap reads as one");
    assert_eq!(state(&board, 3).2, 5, "five units under a cap of ten");
    run(&mut board, &mut tick, 2, &units, &mut log);
    assert_eq!(state(&board, 1).2, 9);
    assert_eq!(state(&board, 2).2, 3);
    assert_eq!(state(&board, 3).2, 15);
    // Progress never overshoots: the flip lands on the tick that fills the bar.
    let mut short = board_with(&[4], |n| ZoneSettings {
        capture_ticks: 7,
        max_rate_units: 3,
        ..zone_settings(n)
    });
    run(&mut short, &mut tick, 2, &squad(0, 4, 5, inside(0)), &mut log);
    assert_eq!(state(&short, 4), (0, 4, 6));
    run(&mut short, &mut tick, 1, &squad(0, 4, 5, inside(0)), &mut log);
    assert_eq!(state(&short, 4), (4, 0, 0));
}

/// A domination session with three zones (one per contest rule, owners scoring, a score limit).
fn domination_peer() -> MatchSession {
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 0,
            ..FlowSettings::default()
        },
        ..SessionSettings::default()
    });
    session
        .playlist
        .set(vec![PlaylistEntry::new("depot", "domination")]);
    for team in [1, 2, 3] {
        session.apply(&FlowInput::Seat { team, slot: 0 }).expect("seat");
    }
    session.start_now();
    for (n, contest) in [Contest::AnyEnemyBlocks, Contest::Majority, Contest::Weighted]
        .into_iter()
        .enumerate()
    {
        let n = n as u32;
        session
            .board
            .install_zone(
                40 - n,
                ZoneSettings {
                    capture_ticks: 9,
                    decay_ticks: Some(3),
                    contest,
                    max_rate_units: 2,
                    owner_scores_every: Some(5),
                    ..zone_settings(n)
                },
            )
            .expect("zone");
    }
    session
        .board
        .set_zone_rules(ZoneMode::Domination, Some(60))
        .expect("rules");
    session
}

/// Units at step `step`: each wanders between the three zones and nowhere on its own schedule.
fn wanderers(step: u64) -> Vec<Contestant> {
    (0..12u32)
        .map(|i| {
            let team = (i % 3) as u8 + 1;
            let phase = (step + u64::from(i) * 7) / (5 + u64::from(i % 4));
            let spot = (phase + u64::from(i)) % 4;
            let position = if spot == 3 { NOWHERE } else { inside(spot as u32) };
            Contestant {
                id: 1000 + i,
                team,
                position,
                alive: !(step + u64::from(i)).is_multiple_of(11),
            }
        })
        .collect()
}

#[test]
fn zones_bit_identical_on_two_peers_with_reversed_inputs() {
    let mut peers = [domination_peer(), domination_peer()];
    let mut logs: [Vec<RulesEvent>; 2] = [Vec::new(), Vec::new()];
    for step in 0..1500u64 {
        let forward = wanderers(step);
        let mut reversed = forward.clone();
        reversed.reverse();
        logs[0].extend(peers[0].step(&forward).events);
        logs[1].extend(peers[1].step(&reversed).events);
        assert_eq!(peers[0], peers[1], "step {step}");
    }
    assert_eq!(logs[0], logs[1]);
    let captures = logs[0]
        .iter()
        .filter(|e| matches!(e, RulesEvent::ZoneCaptured { .. }))
        .count();
    let points = logs[0]
        .iter()
        .filter(|e| matches!(e, RulesEvent::ZonePoint { .. }))
        .count();
    assert!(captures >= 3, "{captures} captures");
    assert!(points >= 10, "{points} points");
    assert!(
        logs[0].iter().any(|e| matches!(
            e,
            RulesEvent::MatchOver {
                reason: EndReason::ZoneScoreLimitReached,
                ..
            }
        )),
        "the score limit ended the match"
    );
    // The serialized boards agree byte for byte.
    let a = serde_json::to_string(&peers[0].board).expect("json");
    let b = serde_json::to_string(&peers[1].board).expect("json");
    assert_eq!(a, b);
}

#[test]
fn koth_defaults_unchanged() {
    // The settings S-14 shipped.
    let defaults = ObjectiveSettings::default();
    assert_eq!(defaults.hill_score_limit, 180);
    assert_eq!(defaults.hill_accrual_ticks, 60);
    // One hill per match, still.
    let hill = volume_at(0.0);
    let mut koth = ObjectiveBoard::new(MatchMode::KingOfTheHill, defaults);
    koth.install_hill_volume(1, hill).expect("hill");
    assert_eq!(koth.install_hill_volume(2, hill), Err(RulesError::HillExists));
    // A board with no zones serializes as before (no `zone` or `zone_rules` key) and the zone step
    // leaves it untouched.
    let json = serde_json::to_value(&koth).expect("json");
    assert!(json.get("zone_rules").is_none());
    assert!(json["objectives"][0].get("zone").is_none());
    assert_eq!(json["objectives"][0]["kind"], "hill");
    let before = koth.clone();
    koth.zones_tick(60, &[unit(1, 1, inside(0))]);
    assert_eq!(koth, before);
    // Sampled accrual: presence between samples counts for nothing (S-14's rule), with or without
    // a zone on the same board.
    let mut with_zone = koth.clone();
    with_zone
        .install_zone(9, zone_settings(5))
        .expect("a zone beside the hill");
    for board in [&mut koth, &mut with_zone] {
        for tick in 1..=120u64 {
            let at = if tick == 60 || tick == 120 {
                NOWHERE
            } else {
                inside(0)
            };
            board.koth_tick(tick, &[unit(1, 1, at)]);
        }
        assert_eq!(board.score(1), 0, "away on both sample ticks scores nothing");
        board.koth_tick(180, &[unit(1, 1, inside(0))]);
        assert_eq!(board.score(1), 1);
    }
    let points = |b: &mut ObjectiveBoard| {
        b.drain_events()
            .into_iter()
            .filter(|e| matches!(e, RulesEvent::HillPoint { .. }))
            .count()
    };
    assert_eq!(points(&mut koth), points(&mut with_zone));
    // A session with phases off scores one hill point per period (one koth step per tick).
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 0,
            ..FlowSettings::default()
        },
        ..SessionSettings::default()
    });
    session.playlist.set(vec![PlaylistEntry::new("hill", "koth")]);
    session
        .apply(&FlowInput::Seat { team: 1, slot: 0 })
        .expect("seat");
    session.start_now();
    session.board.install_hill_volume(9, hill).expect("hill");
    for _ in 0..130 {
        session.step(&[unit(1, 1, inside(0))]);
    }
    assert_eq!(session.board.score(1), 2, "two periods, two points");
}

/// Review finding 5 of the teams line: a unit the host lists twice counts once in a zone and is
/// credited once for a hill point or a zone capture.
#[test]
fn a_unit_listed_twice_counts_and_is_credited_once() {
    use regia_rules::teams::SeatRequest;

    // The zone count: the same id twice is one unit, so the rate is 1, not 2.
    let mut board = board_with(&[1], |n| ZoneSettings {
        capture_ticks: 10,
        max_rate_units: 4,
        ..zone_settings(n)
    });
    let (mut tick, mut log) = (0, Vec::new());
    let twice = [unit(7, 1, inside(0)), unit(7, 1, inside(0))];
    run(&mut board, &mut tick, 3, &twice, &mut log);
    assert_eq!(state(&board, 1), (0, 1, 3));
    // Bound seats: a hill point and a zone capture credit unit 7 once each.
    for mode in ["koth", "dom"] {
        let mut session = MatchSession::new(SessionSettings {
            flow: FlowSettings {
                start_delay_ticks: 0,
                ..FlowSettings::default()
            },
            objectives: ObjectiveSettings {
                hill_accrual_ticks: 1,
                ..ObjectiveSettings::default()
            },
            ..SessionSettings::default()
        });
        session.playlist.set(vec![PlaylistEntry::new("depot", mode)]);
        let seats = session
            .assign_team(
                &SeatRequest {
                    unit: Some(7),
                    team: Some(1),
                    ..SeatRequest::default()
                },
                &[],
            )
            .expect("seat");
        session.start_now();
        if mode == "koth" {
            session
                .board
                .install_hill_volume(9, volume_at(0.0))
                .expect("hill");
        } else {
            session
                .board
                .install_zone(
                    9,
                    ZoneSettings {
                        capture_ticks: 1,
                        ..zone_settings(0)
                    },
                )
                .expect("zone");
        }
        // Step until the first hill point or capture (tick 0 never accrues on a hill).
        for _ in 0..5 {
            let events = session.step(&twice).events;
            if events
                .iter()
                .any(|e| matches!(e, RulesEvent::HillPoint { .. } | RulesEvent::ZoneCaptured { .. }))
            {
                break;
            }
        }
        let row = session.ledgers.row(seats[0]).expect("row");
        assert_eq!(row.objective_points, 1, "{mode}");
    }
}

/// With rounds on, a round start returns every zone to neutral and restarts the zone mode.
#[test]
fn a_round_start_returns_zones_to_neutral() {
    use regia_rules::phases::{MatchPhase, RoundSettings};

    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 0,
            phases: PhaseSettings {
                time_limit_ticks: Some(20),
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
    for team in [1, 2] {
        session.apply(&FlowInput::Seat { team, slot: 0 }).expect("seat");
    }
    session.start_now();
    session
        .board
        .install_zone(
            1,
            ZoneSettings {
                capture_ticks: 3,
                owner_scores_every: Some(1),
                ..zone_settings(0)
            },
        )
        .expect("zone");
    let mut saw_owner = false;
    let mut checked = false;
    for _ in 0..60 {
        let events = session.step(&[unit(1, 1, inside(0))]).events;
        let zone = *session.board.zone(1).expect("zone");
        saw_owner |= zone.owner == 1;
        let round_two = events.iter().any(|e| {
            matches!(
                e,
                RulesEvent::PhaseChanged {
                    phase: MatchPhase::Live,
                    round: 2,
                    ..
                }
            )
        });
        if round_two {
            assert!(saw_owner, "team 1 owned the zone in round 1");
            assert_eq!(zone.owner, 0, "neutral again at the start of round 2");
            assert!(zone.progress <= 1, "{zone:?}");
            checked = true;
            break;
        }
    }
    assert!(checked, "round 2 started");
}

/// The last attack/defend capture ends the match after the whole pass: a zone with a higher id
/// on the same box still takes that tick, and the match ends once (`MatchOver` is the last event).
#[test]
fn the_last_attack_capture_still_ticks_later_zones() {
    let mut board = board_with(&[3, 9], |_| ZoneSettings {
        capture_ticks: 1,
        owner_scores_every: None,
        ..ZoneSettings::new(volume_at(0.0))
    });
    board
        .set_zone_rules(
            ZoneMode::AttackDefend {
                attackers: 1,
                defenders: 2,
                order: vec![3],
            },
            None,
        )
        .expect("attack/defend");
    let (mut tick, mut log) = (0, Vec::new());
    run(&mut board, &mut tick, 1, &[unit(1, 1, inside(0))], &mut log);
    assert!(board.match_over);
    assert_eq!(
        state(&board, 9),
        (1, 0, 0),
        "zone 9 is not in the order and still flips"
    );
    assert_eq!(
        log,
        vec![
            RulesEvent::ZoneCaptured {
                tick: 1,
                zone: 3,
                team: 1,
                previous: 2
            },
            RulesEvent::ZoneCaptured {
                tick: 1,
                zone: 9,
                team: 1,
                previous: 0
            },
            RulesEvent::MatchOver {
                tick: 1,
                team: 1,
                reason: EndReason::ZonesCaptured
            },
        ]
    );
}

/// Two zones reach the score limit on the same tick for different teams: both points count, and
/// the lower entity id decides the one match end.
#[test]
fn two_score_limits_on_one_tick_end_the_match_once_for_the_lowest_id() {
    let mut board = board_with(&[4, 5], |n| ZoneSettings {
        capture_ticks: 1,
        owner_scores_every: Some(1),
        ..zone_settings(n)
    });
    board
        .set_zone_rules(ZoneMode::Domination, Some(1))
        .expect("score limit");
    let (mut tick, mut log) = (0, Vec::new());
    let units = [unit(1, 1, inside(1)), unit(2, 2, inside(0))];
    run(&mut board, &mut tick, 1, &units, &mut log);
    assert!(board.match_over);
    assert_eq!(board.end_reason, Some(EndReason::ZoneScoreLimitReached));
    assert_eq!(board.team_scores.get(1..3), Some(&[1, 1][..]));
    let ends: Vec<&RulesEvent> = log
        .iter()
        .filter(|event| matches!(event, RulesEvent::MatchOver { .. }))
        .collect();
    assert_eq!(
        ends,
        vec![&RulesEvent::MatchOver {
            tick: 1,
            team: 2,
            reason: EndReason::ZoneScoreLimitReached
        }]
    );
    assert_eq!(log.last(), ends.first().copied());
}

/// A rotation that comes back to a zone (`[1, 2, 1]`) is refused like `[7, 7]`: the later copy
/// would lock the active zone. The board does not change.
#[test]
fn a_rotation_that_repeats_a_zone_is_refused() {
    let mut board = board_with(&[1, 2], zone_settings);
    let before = board.clone();
    assert_eq!(
        board.set_zone_rules(
            ZoneMode::Hardpoint {
                rotation: vec![1, 2, 1],
                rotate_ticks: 10,
            },
            None,
        ),
        Err(RulesError::ZoneSettings { field: "rotation" })
    );
    assert_eq!(
        board.set_zone_rules(
            ZoneMode::AttackDefend {
                attackers: 1,
                defenders: 2,
                order: vec![2, 1, 2],
            },
            None,
        ),
        Err(RulesError::ZoneSettings { field: "order" })
    );
    assert_eq!(board, before);
}
