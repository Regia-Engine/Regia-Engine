// SPDX-License-Identifier: MPL-2.0
//! Review proofs for the GR-11.3 / GR-11.4 genre rules.
//!
//! The proofs below lock the behavior the rules promise. On this head
//! `a_restored_shrink_past_i128_stays_on_the_interpolation` fails: the shrink fallback divides
//! before it multiplies.

use regia_rules::RulesError;
use regia_rules::elimination::{EliminationRules, OutCause};
use regia_rules::match_flow::SeatId;
use regia_rules::objective::{Contestant, EndReason, RulesEvent};
use regia_rules::session::{MatchSession, SessionSettings};
use regia_rules::spawn::{RespawnPolicy, RespawnRules};
use regia_rules::zone::{ZoneCircle, ZonePhase, ZoneSchedule, ZoneStage, ZoneState};

fn settings(teams: u8, seats: u8) -> SessionSettings {
    SessionSettings {
        flow: regia_rules::match_flow::FlowSettings {
            max_teams: teams,
            seats_per_team: seats,
            start_delay_ticks: 0,
            ..regia_rules::match_flow::FlowSettings::default()
        },
        objectives: regia_rules::objective::ObjectiveSettings {
            max_teams: teams,
            ..regia_rules::objective::ObjectiveSettings::default()
        },
        respawn: RespawnRules {
            default: RespawnPolicy::Never,
            ..RespawnRules::default()
        },
        ..SessionSettings::default()
    }
}

fn started(settings: SessionSettings, seats: &[(u8, u8)]) -> MatchSession {
    let mut session = MatchSession::new(settings);
    for &(team, slot) in seats {
        session
            .apply(&regia_rules::match_flow::FlowInput::Seat { team, slot })
            .expect("seat");
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

fn zone_damage(events: &[RulesEvent]) -> Vec<(u32, u32)> {
    events
        .iter()
        .filter_map(|event| match event {
            RulesEvent::ZoneDamage { unit, amount, .. } => Some((*unit, *amount)),
            _ => None,
        })
        .collect()
}

fn eliminated(events: &[RulesEvent]) -> Vec<(u8, u8, OutCause)> {
    events
        .iter()
        .filter_map(|event| match event {
            RulesEvent::TeamEliminated {
                team, place, cause, ..
            } => Some((*team, *place, *cause)),
            _ => None,
        })
        .collect()
}

/// A unit ~1e20 m outside a 100 m circle whose center is at -1 m must be outside, and the
/// session must report zone damage for it. `ZoneCircle::contains` squares both axes in `i128`.
/// `to_cm(1e20)` saturates to `i64::MAX` and `to_cm(-1.0)` is `-100`, so each axis differs by
/// more than `2^63` and the sum of squares does not fit in `i128`.
#[test]
fn a_unit_far_outside_the_zone_is_outside_without_panicking() {
    let circle = ZoneCircle::from_world([-1.0, -1.0], 100.0);
    assert!(
        !circle.contains([1.0e20, 0.0, 1.0e20]),
        "a unit at 1e20 m on X and Z is outside a 100 m circle centered at -1 m"
    );
    let schedule = ZoneSchedule {
        center: [-1.0, -1.0],
        radius: 100.0,
        phases: vec![ZonePhase {
            wait_ticks: 10_000,
            shrink_ticks: 0,
            end_radius: 100.0,
            damage_per_tick: 1,
        }],
        seed: 1,
    };
    let mut session = started(
        SessionSettings {
            zone: Some(schedule),
            ..settings(1, 1)
        },
        &[(1, 0)],
    );
    let stepped = session.step(&[unit(7, 1, 1.0e20, 1.0e20)]);
    assert_eq!(zone_damage(&stepped.events), vec![(7, 1)]);
}

/// A finite center past the centimeter range is not a schedule a match can run.
#[test]
fn a_center_past_the_centimeter_range_is_refused() {
    let schedule = ZoneSchedule {
        center: [-1.0e20, -1.0e20],
        radius: 1000.0,
        phases: Vec::new(),
        seed: 1,
    };
    assert_eq!(
        schedule.validate(),
        Err(RulesError::ShrinkZoneSettings { field: "center" })
    );
}

/// Two host defeats on the same tick are one elimination: team-id order, the same places
/// whichever call arrives first. Three teams are still standing, so the pair is places 3 then 2.
#[test]
fn same_tick_defeats_are_in_team_id_order() {
    let places = |order: &[u8]| {
        let mut session = started(
            SessionSettings {
                elimination: EliminationRules {
                    last_standing: true,
                    ..EliminationRules::default()
                },
                ..settings(3, 1)
            },
            &[(1, 0), (2, 0), (3, 0)],
        );
        for &team in order {
            session.defeat_team(team);
        }
        session
            .placements
            .out
            .iter()
            .map(|out| (out.team, out.place))
            .collect::<Vec<_>>()
    };
    let expected = vec![(1, 3), (2, 2)];
    assert_eq!(places(&[2, 1]), expected, "defeat 2 then 1");
    assert_eq!(places(&[1, 2]), expected, "defeat 1 then 2");
}

/// The last two teams lose their only seats on one tick: team-id order, then a draw.
#[test]
fn the_last_two_teams_out_on_one_tick_draw() {
    let mut session = started(
        SessionSettings {
            elimination: EliminationRules {
                last_standing: true,
                ..EliminationRules::default()
            },
            ..settings(2, 1)
        },
        &[(1, 0), (2, 0)],
    );
    session.note_death(SeatId::new(2, 0));
    session.note_death(SeatId::new(1, 0));
    let events = session.step(&[]).events;
    assert_eq!(
        eliminated(&events),
        vec![(1, 2, OutCause::Eliminated), (2, 1, OutCause::Eliminated),]
    );
    let victory = events.iter().find_map(|event| match event {
        RulesEvent::Victory { teams, reason, .. } => Some((teams.clone(), *reason)),
        _ => None,
    });
    assert_eq!(victory, Some((Vec::new(), EndReason::Draw)));
    let over = events.iter().find_map(|event| match event {
        RulesEvent::MatchOver { team, reason, .. } => Some((*team, *reason)),
        _ => None,
    });
    assert_eq!(over, Some((0, EndReason::Draw)));
}

/// Radius 0 is a legal circle: only the center centimeter is inside, and it is damaged once.
#[test]
fn a_zero_radius_zone_damages_everyone_but_the_center_centimeter() {
    let schedule = ZoneSchedule {
        center: [0.0, 0.0],
        radius: 0.0,
        phases: vec![ZonePhase {
            wait_ticks: 100,
            shrink_ticks: 0,
            end_radius: 0.0,
            damage_per_tick: 4,
        }],
        seed: 1,
    };
    assert!(schedule.validate().is_ok());
    let mut session = started(
        SessionSettings {
            zone: Some(schedule),
            ..settings(1, 1)
        },
        &[(1, 0)],
    );
    let stepped = session.step(&[unit(1, 1, 0.0, 0.0), unit(2, 1, 1.0, 0.0)]);
    assert_eq!(zone_damage(&stepped.events), vec![(2, 4)]);
}

/// Two zero-length phases with the same end radius jump on the start tick and damage once,
/// using the phase still in force after the jump (the last phase).
#[test]
fn equal_end_radii_and_zero_waits_damage_once() {
    let schedule = ZoneSchedule {
        center: [0.0, 0.0],
        radius: 10.0,
        phases: vec![
            ZonePhase {
                wait_ticks: 0,
                shrink_ticks: 0,
                end_radius: 10.0,
                damage_per_tick: 5,
            },
            ZonePhase {
                wait_ticks: 0,
                shrink_ticks: 0,
                end_radius: 10.0,
                damage_per_tick: 9,
            },
        ],
        seed: 1,
    };
    assert!(schedule.validate().is_ok());
    let mut session = MatchSession::new(SessionSettings {
        zone: Some(schedule),
        ..settings(1, 1)
    });
    session
        .apply(&regia_rules::match_flow::FlowInput::Seat { team: 1, slot: 0 })
        .expect("seat");
    session.start_now();
    let stepped = session.step(&[unit(1, 1, 0.0, 0.0), unit(2, 1, 40.0, 0.0)]);
    assert_eq!(session.zone_state.stage, regia_rules::zone::ZoneStage::Closed);
    assert_eq!(zone_damage(&stepped.events), vec![(2, 9)]);
}

/// Integer centimeters `elapsed / total` of the way from `from` to `to`.
///
/// The product is taken in `u128`. `(2^64 - 1) * (2^63 + 1)` fits there and does not fit in
/// `i128`, which is the case the shrink fallback claims to handle.
fn interpolated_cm(from: i64, to: i64, elapsed: u64, total: u64) -> i64 {
    let delta = i128::from(to) - i128::from(from);
    let moved = delta.unsigned_abs() * u128::from(elapsed) / u128::from(total);
    let moved = i128::try_from(moved).expect("moved distance fits in i128");
    let signed = if delta.is_negative() { -moved } else { moved };
    i64::try_from(i128::from(from) + signed).expect("interpolated centimeter fits in i64")
}

/// A restored shrink whose `delta * elapsed` does not fit in `i128` stays on the integer
/// interpolation. Divide-first (`delta / total * elapsed`) parks the circle about `2^63` cm
/// short, so a unit at the origin is inside the wrong circle and takes no zone damage.
///
/// `from` is `i64::MIN`, `to` is `i64::MAX` (delta `2^64 - 1`), `elapsed` is `2^63 + 1` and
/// `shrink_ticks` is `elapsed + 1`. The true center X is `9223372036854775805`. The fallback
/// yields `1`. The stage is already `Shrinking` and `elapsed < shrink_ticks`, so the phase does
/// not close and `start` does not replace the circles.
#[test]
fn a_restored_shrink_past_i128_stays_on_the_interpolation() {
    const ELAPSED: u64 = 9_223_372_036_854_775_809;
    const TOTAL: u64 = ELAPSED + 1;
    const EXPECTED_X: i64 = 9_223_372_036_854_775_805;
    let from = ZoneCircle {
        center_cm: [i64::MIN, 0],
        radius_cm: 100,
    };
    let to = ZoneCircle {
        center_cm: [i64::MAX, 0],
        radius_cm: 100,
    };
    let schedule = ZoneSchedule {
        center: [0.0, 0.0],
        radius: 1.0,
        phases: vec![ZonePhase {
            wait_ticks: 0,
            shrink_ticks: TOTAL,
            end_radius: 1.0,
            damage_per_tick: 4,
        }],
        seed: 1,
    };
    assert!(schedule.validate().is_ok());
    assert_eq!(interpolated_cm(i64::MIN, i64::MAX, ELAPSED, TOTAL), EXPECTED_X);
    let mut state = ZoneState {
        started_at: Some(0),
        phase: 0,
        stage: ZoneStage::Shrinking,
        stage_since: 0,
        from,
        to,
        current: from,
        ..ZoneState::default()
    };
    let mut events = Vec::new();
    state.tick(&schedule, ELAPSED, &[unit(7, 1, 0.0, 0.0)], &mut events);
    assert_eq!(state.stage, ZoneStage::Shrinking);
    assert_eq!(state.started_at, Some(0));
    assert_eq!(
        state.current.center_cm,
        [EXPECTED_X, 0],
        "center must be (to - from) * elapsed / total, not divide-first"
    );
    assert_eq!(state.current.radius_cm, 100);
    assert_eq!(zone_damage(&events), vec![(7, 4)]);
}

/// A restored shrink whose span is smaller than `shrink_ticks` can still have a product past
/// `i128`; the divide-first fallback then computed `delta / total == 0` and parked the circle on
/// `from` for the whole phase, jumping on the last tick. A unit inside the true circle was
/// damaged as though it were outside it.
///
/// `from` is `i64::MIN`, `to` is `2^32` (delta `2^63 + 2^32`, under `total`), `elapsed` is
/// `2^64 - 2` and `shrink_ticks` is `2^64 - 1`: `delta * elapsed` is `2^127 + 2^96 - …`, past
/// `i128`, so the old code took the fallback and never moved. On the tick before the phase
/// closes the circle must sit one centimeter short of `to`. The unit's world X `42949668.0` is
/// exact in `f32` and converts to `4294966784` cm (`x * 100.0` rounds down to the `f32` grid),
/// `511` cm from that center: inside the 1000 cm circle, so it must take no damage.
#[test]
fn a_restored_shrink_shorter_than_total_ticks_still_moves() {
    const ELAPSED: u64 = 18_446_744_073_709_551_614;
    const TOTAL: u64 = u64::MAX;
    let from = ZoneCircle {
        center_cm: [i64::MIN, 0],
        radius_cm: 1000,
    };
    let to = ZoneCircle {
        center_cm: [4_294_967_296, 0],
        radius_cm: 1000,
    };
    let schedule = ZoneSchedule {
        center: [0.0, 0.0],
        radius: 1.0,
        phases: vec![ZonePhase {
            wait_ticks: 0,
            shrink_ticks: TOTAL,
            end_radius: 1.0,
            damage_per_tick: 4,
        }],
        seed: 1,
    };
    assert!(schedule.validate().is_ok());
    let mut state = ZoneState {
        started_at: Some(0),
        phase: 0,
        stage: ZoneStage::Shrinking,
        stage_since: 0,
        from,
        to,
        current: from,
        ..ZoneState::default()
    };
    let mut events = Vec::new();
    state.tick(&schedule, ELAPSED, &[unit(7, 1, 42_949_668.0, 0.0)], &mut events);
    assert_eq!(state.stage, ZoneStage::Shrinking);
    assert_eq!(
        state.current.center_cm,
        [4_294_967_295, 0],
        "the circle must sit (to - from) * elapsed / total from `from`, one cm short of `to`, \
         not park on `from` until the last tick"
    );
    assert_eq!(state.current.radius_cm, 1000);
    assert_eq!(
        zone_damage(&events),
        Vec::new(),
        "a unit inside the true circle takes no zone damage"
    );
}
