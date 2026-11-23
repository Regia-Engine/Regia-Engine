// SPDX-License-Identifier: MPL-2.0
//! Phases, time limits, overtime, rounds and forfeits (ADR-8485, audit gaps GR-4.6 to GR-4.9).
//!
//! Every clock is a tick count and every script is a fixed list of steps: no wall clock.

use regia_rules::match_flow::{FlowInput, FlowSettings};
use regia_rules::mode::MatchMode;
use regia_rules::objective::{
    Contestant, EndReason, HillVolume, ObjectiveBoard, ObjectiveKind, ObjectiveSettings, RulesEvent,
};
use regia_rules::phases::{MatchPhase, PhaseSettings, RoundSettings, TimeUp};
use regia_rules::playlist::PlaylistEntry;
use regia_rules::session::{MatchSession, SessionSettings};
use regia_rules::zones::ZoneRules;
use serde::Serialize;

const HILL: HillVolume = HillVolume {
    min: [-5.0, -1.0, -5.0],
    max: [5.0, 5.0, 5.0],
};
const OUT: [f32; 3] = [100.0, 0.0, 100.0];
const IN: [f32; 3] = [0.0, 0.0, 0.0];

fn unit(id: u32, team: u8, position: [f32; 3]) -> Contestant {
    Contestant {
        id,
        team,
        position,
        alive: true,
    }
}

/// A king-of-the-hill session scoring one point per tick to the sole occupant, with a high
/// score limit so only the phase rules end it. Seats 1/0 and 2/0 (and 3/0 when `three`).
fn koth(phases: PhaseSettings, three: bool) -> MatchSession {
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 0,
            phases,
            ..FlowSettings::default()
        },
        objectives: ObjectiveSettings {
            hill_accrual_ticks: 1,
            hill_score_limit: 10_000,
            ..ObjectiveSettings::default()
        },
        ..SessionSettings::default()
    });
    session.playlist.set(vec![PlaylistEntry::new("hill", "koth")]);
    let teams: &[u8] = if three { &[1, 2, 3] } else { &[1, 2] };
    for team in teams {
        session
            .apply(&FlowInput::Seat { team: *team, slot: 0 })
            .expect("seat");
    }
    session.start_now();
    session.board.install_hill_volume(9, HILL).expect("hill");
    session
}

/// Step once with `team` alone on the hill (`0`: nobody on it), collecting the events.
fn hold(session: &mut MatchSession, team: u8, events: &mut Vec<RulesEvent>) {
    let units = [
        unit(101, 1, if team == 1 { IN } else { OUT }),
        unit(201, 2, if team == 2 { IN } else { OUT }),
        unit(301, 3, if team == 3 { IN } else { OUT }),
    ];
    events.extend(session.step(&units).events);
}

fn match_over(events: &[RulesEvent]) -> Vec<(u64, u8, EndReason)> {
    events
        .iter()
        .filter_map(|e| match e {
            RulesEvent::MatchOver { tick, team, reason } => Some((*tick, *team, *reason)),
            _ => None,
        })
        .collect()
}

fn phases_seen(events: &[RulesEvent]) -> Vec<(MatchPhase, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            RulesEvent::PhaseChanged { phase, round, .. } => Some((*phase, *round)),
            _ => None,
        })
        .collect()
}

#[test]
fn time_limit_ends_the_match_with_the_leader_winning() {
    let phases = PhaseSettings {
        time_limit_ticks: Some(20),
        ..PhaseSettings::default()
    };
    let mut s = koth(phases, false);
    let mut events = s.drain_events();
    assert_eq!(s.flow.phases.time_left(&phases), Some(20));
    for tick in 1..=30u64 {
        let team = match tick {
            1..=5 => 1,
            6..=8 => 2,
            _ => 0,
        };
        hold(&mut s, team, &mut events);
        if tick == 10 {
            assert_eq!(s.flow.phases.time_left(&phases), Some(10));
            assert!(!s.board.match_over);
        }
    }
    let start = s.flow.match_tick - 30;
    assert_eq!(match_over(&events), [(start + 20, 1, EndReason::TimeLimit)]);
    assert_eq!((s.board.score(1), s.board.score(2)), (5, 3));
    assert_eq!(s.flow.phases.phase, MatchPhase::PostMatch);
    assert_eq!(
        phases_seen(&events),
        [(MatchPhase::Live, 0), (MatchPhase::PostMatch, 0)]
    );
    assert_eq!(s.flow.phases.time_left(&phases), Some(0));
}

fn tied_at_time_up(on_time_up: TimeUp, three: bool) -> (MatchSession, Vec<RulesEvent>, PhaseSettings) {
    let phases = PhaseSettings {
        time_limit_ticks: Some(12),
        on_time_up,
        ..PhaseSettings::default()
    };
    let mut s = koth(phases, three);
    let mut events = s.drain_events();
    // Teams 1 and 2 hold 3 ticks each; team 3 (when present) holds 1.
    let script: &[u8] = if three {
        &[1, 1, 1, 2, 2, 2, 3, 0, 0, 0, 0, 0]
    } else {
        &[1, 1, 1, 2, 2, 2, 0, 0, 0, 0, 0, 0]
    };
    for team in script {
        hold(&mut s, *team, &mut events);
    }
    (s, events, phases)
}

#[test]
fn a_tie_at_time_up_goes_to_overtime_and_the_next_point_wins() {
    let (mut s, mut events, _) = tied_at_time_up(TimeUp::Overtime { ticks: None }, false);
    assert!(match_over(&events).is_empty(), "a tie does not end the match");
    assert_eq!(s.flow.phases.phase, MatchPhase::Overtime);
    for _ in 0..40 {
        hold(&mut s, 0, &mut events);
    }
    assert!(!s.board.match_over, "overtime without a cap plays on");
    hold(&mut s, 2, &mut events);
    assert_eq!(
        match_over(&events),
        [(s.flow.match_tick, 2, EndReason::TimeLimit)]
    );
    assert_eq!(
        phases_seen(&events),
        [
            (MatchPhase::Live, 0),
            (MatchPhase::Overtime, 0),
            (MatchPhase::PostMatch, 0)
        ]
    );
}

#[test]
fn a_tie_at_time_up_with_draw_ends_level() {
    let (s, events, _) = tied_at_time_up(TimeUp::Draw, false);
    assert_eq!(match_over(&events), [(s.flow.match_tick, 0, EndReason::Draw)]);
    let (s, events, _) = tied_at_time_up(TimeUp::End, false);
    assert_eq!(
        match_over(&events),
        [(s.flow.match_tick, 0, EndReason::TimeLimit)],
        "End stops the clock with no winner"
    );
}

#[test]
fn a_capped_overtime_still_tied_is_a_draw() {
    let (mut s, mut events, phases) = tied_at_time_up(TimeUp::Overtime { ticks: Some(10) }, false);
    assert_eq!(s.flow.phases.time_left(&phases), Some(10));
    for _ in 0..9 {
        hold(&mut s, 0, &mut events);
    }
    assert!(!s.board.match_over);
    hold(&mut s, 0, &mut events);
    assert_eq!(match_over(&events), [(s.flow.match_tick, 0, EndReason::Draw)]);
}

#[test]
fn a_tie_at_time_up_goes_to_sudden_death_and_any_next_point_wins() {
    // Teams 1 and 2 are tied at 3, team 3 trails at 1. In overtime team 3's point leaves no sole
    // leader; in sudden death the next point wins whoever scores it.
    let (mut overtime, mut ot_events, _) = tied_at_time_up(TimeUp::Overtime { ticks: None }, true);
    hold(&mut overtime, 3, &mut ot_events);
    assert!(!overtime.board.match_over, "overtime waits for a sole leader");

    let (mut sudden, mut events, _) = tied_at_time_up(TimeUp::SuddenDeath, true);
    assert_eq!(sudden.flow.phases.phase, MatchPhase::Overtime);
    assert!(match_over(&events).is_empty());
    hold(&mut sudden, 3, &mut events);
    assert_eq!(
        match_over(&events),
        [(sudden.flow.match_tick, 3, EndReason::TimeLimit)]
    );
    assert_eq!(
        (
            sudden.board.score(1),
            sudden.board.score(2),
            sudden.board.score(3)
        ),
        (3, 3, 2)
    );
}

#[test]
fn warmup_scores_nothing_and_ends_at_min_seats_or_ticks() {
    // By ticks: one seated team, 30 warm-up ticks.
    let phases = PhaseSettings {
        warmup_ticks: Some(30),
        warmup_min_seats: Some(3),
        ..PhaseSettings::default()
    };
    let mut s = koth(phases, false);
    let mut events = s.drain_events();
    assert_eq!(s.flow.phases.phase, MatchPhase::Warmup);
    for _ in 0..29 {
        hold(&mut s, 1, &mut events);
    }
    assert_eq!(s.flow.phases.phase, MatchPhase::Warmup);
    assert_eq!(s.board.score(1), 0, "the warm-up scores nothing");
    hold(&mut s, 1, &mut events);
    assert_eq!(s.flow.phases.phase, MatchPhase::Live);
    assert_eq!(
        s.board.score(1),
        0,
        "the tick that ends the warm-up scores nothing"
    );
    hold(&mut s, 1, &mut events);
    assert_eq!(s.board.score(1), 1, "live play scores from the next tick");
    assert_eq!(
        phases_seen(&events),
        [(MatchPhase::Warmup, 0), (MatchPhase::Live, 0)]
    );

    // By seats: the third seat ends it at once, long before the ticks run out.
    let phases = PhaseSettings {
        warmup_ticks: Some(10_000),
        warmup_min_seats: Some(3),
        countdown_ticks: 4,
        ..PhaseSettings::default()
    };
    let mut s = koth(phases, false);
    let mut events = s.drain_events();
    for _ in 0..10 {
        hold(&mut s, 1, &mut events);
    }
    assert_eq!(s.flow.phases.phase, MatchPhase::Warmup);
    s.apply(&FlowInput::Seat { team: 3, slot: 0 }).expect("seat");
    hold(&mut s, 1, &mut events);
    assert_eq!(s.flow.phases.phase, MatchPhase::Countdown);
    for _ in 0..4 {
        hold(&mut s, 1, &mut events);
    }
    assert_eq!(s.flow.phases.phase, MatchPhase::Live);
    assert_eq!(s.board.score(1), 0, "the countdown scores nothing");
    hold(&mut s, 1, &mut events);
    assert_eq!(s.board.score(1), 1);
}

/// A capture-the-flag session in rounds; flags of teams 1 and 2 at x = -120 and +120.
fn ctf_rounds(rounds: RoundSettings) -> MatchSession {
    ctf_rounds_with(rounds, None)
}

fn ctf_rounds_with(rounds: RoundSettings, time_limit_ticks: Option<u64>) -> MatchSession {
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 0,
            phases: PhaseSettings {
                rounds: Some(rounds),
                time_limit_ticks,
                ..PhaseSettings::default()
            },
            ..FlowSettings::default()
        },
        objectives: ObjectiveSettings {
            capture_limit: 1,
            ..ObjectiveSettings::default()
        },
        ..SessionSettings::default()
    });
    session.playlist.set(vec![PlaylistEntry::new("harbor", "ctf")]);
    for team in [1, 2] {
        session.apply(&FlowInput::Seat { team, slot: 0 }).expect("seat");
    }
    session.start_now();
    session.board.place_flags(&[1, 2], 120.0, 10).expect("flags");
    session
}

fn flag_base(session: &MatchSession, team: u8) -> [f32; 3] {
    session
        .board
        .objectives
        .iter()
        .find(|o| o.is_flag() && o.team_id == team)
        .map(|o| o.base_position)
        .expect("flag")
}

/// `raider`'s team grabs the enemy flag (one tick) and runs it home (the next tick), reading
/// the bases from the board as a host does.
fn capture(session: &mut MatchSession, raider: u8, events: &mut Vec<RulesEvent>) {
    let enemy = if raider == 1 { 2 } else { 1 };
    let id = u32::from(raider) * 100 + 1;
    let far = [0.0, 0.0, 300.0];
    let other = u32::from(enemy) * 100 + 1;
    events.extend(
        session
            .step(&[
                unit(id, raider, flag_base(session, enemy)),
                unit(other, enemy, far),
            ])
            .events,
    );
    events.extend(
        session
            .step(&[
                unit(id, raider, flag_base(session, raider)),
                unit(other, enemy, far),
            ])
            .events,
    );
}

fn idle(session: &mut MatchSession, steps: u64, events: &mut Vec<RulesEvent>) {
    for _ in 0..steps {
        events.extend(session.step(&[]).events);
    }
}

fn rounds_over(events: &[RulesEvent]) -> Vec<(u32, u8, EndReason)> {
    events
        .iter()
        .filter_map(|e| match e {
            RulesEvent::RoundOver {
                round, team, reason, ..
            } => Some((*round, *team, *reason)),
            _ => None,
        })
        .collect()
}

#[test]
fn best_of_five_rounds_swaps_sides_after_round_two_and_ends_on_three_wins() {
    let rounds = RoundSettings {
        rounds_to_win: 3,
        max_rounds: 5,
        swap_sides_every: 2,
        round_end_ticks: 10,
    };
    let mut s = ctf_rounds(rounds);
    let mut events = s.drain_events();
    let west = flag_base(&s, 1);
    let east = flag_base(&s, 2);
    assert_ne!(west, east);
    assert_eq!(s.flow.phases.round, 1);

    for (round, winner) in [(1u32, 1u8), (2, 1), (3, 2), (4, 1)] {
        assert_eq!(s.flow.phases.round, round);
        assert_eq!(s.flow.phases.phase, MatchPhase::Live);
        let swapped = round >= 3;
        assert_eq!(s.flow.phases.side_of(1), if swapped { 2 } else { 1 });
        assert_eq!(s.flow.phases.side_of(2), if swapped { 1 } else { 2 });
        assert_eq!(
            flag_base(&s, 1),
            if swapped { east } else { west },
            "round {round}"
        );
        assert_eq!(
            flag_base(&s, 2),
            if swapped { west } else { east },
            "round {round}"
        );
        capture(&mut s, winner, &mut events);
        if round < 4 {
            assert_eq!(s.flow.phases.phase, MatchPhase::RoundEnd, "round {round}");
            assert!(!s.board.match_over, "a round end is not the match end");
            idle(&mut s, 9, &mut events);
            assert_eq!(
                s.flow.phases.phase,
                MatchPhase::RoundEnd,
                "the hold lasts its ticks"
            );
            idle(&mut s, 1, &mut events);
        }
    }
    assert_eq!(
        rounds_over(&events),
        [
            (1, 1, EndReason::CaptureLimitReached),
            (2, 1, EndReason::CaptureLimitReached),
            (3, 2, EndReason::CaptureLimitReached),
            (4, 1, EndReason::CaptureLimitReached),
        ]
    );
    assert_eq!(
        match_over(&events),
        [(s.flow.match_tick, 1, EndReason::RoundsWon)],
        "one MatchOver for the whole match"
    );
    assert_eq!(
        (s.flow.phases.round_wins_of(1), s.flow.phases.round_wins_of(2)),
        (3, 1)
    );
    assert_eq!(s.flow.phases.phase, MatchPhase::PostMatch);
    assert_eq!(s.board.end_reason, Some(EndReason::RoundsWon));
    let live_rounds: Vec<u32> = phases_seen(&events)
        .into_iter()
        .filter(|(phase, _)| *phase == MatchPhase::Live)
        .map(|(_, round)| round)
        .collect();
    assert_eq!(live_rounds, [1, 2, 3, 4]);
}

#[test]
fn round_end_resets_objectives_but_keeps_round_wins() {
    let rounds = RoundSettings {
        rounds_to_win: 2,
        max_rounds: 3,
        swap_sides_every: 0,
        round_end_ticks: 5,
    };
    let mut s = ctf_rounds_with(rounds, Some(10));
    let mut events = s.drain_events();
    // Round 1: team 2 captures.
    capture(&mut s, 2, &mut events);
    assert_eq!(rounds_over(&events), [(1, 2, EndReason::CaptureLimitReached)]);
    assert_eq!(s.flow.phases.phase, MatchPhase::RoundEnd);
    // Nothing moves in the hold: a raider on the enemy flag picks nothing up.
    let east = flag_base(&s, 2);
    for _ in 0..5 {
        events.extend(s.step(&[unit(101, 1, east)]).events);
    }
    assert!(s.board.objectives.iter().all(|o| o.carrier_id.is_none()));
    assert_eq!(s.flow.phases.phase, MatchPhase::Live);
    assert_eq!(s.flow.phases.round, 2);
    // Round 2: team 1 takes team 2's flag to midfield and holds it there until the time runs out.
    let mid = [0.0, 0.0, 0.0];
    events.extend(s.step(&[unit(101, 1, east)]).events);
    for _ in 0..9 {
        events.extend(s.step(&[unit(101, 1, mid)]).events);
    }
    assert_eq!(
        rounds_over(&events)[1..],
        [(2, 0, EndReason::TimeLimit)],
        "round 2 ends with no winner"
    );
    let carried = s.board.objectives.iter().find(|o| o.team_id == 2).expect("flag");
    assert_eq!(carried.carrier_id, Some(101), "the round ended with the flag out");
    assert_eq!(s.flow.phases.phase, MatchPhase::RoundEnd);
    idle(&mut s, 5, &mut events);
    assert_eq!((s.flow.phases.phase, s.flow.phases.round), (MatchPhase::Live, 3));
    for flag in &s.board.objectives {
        assert!(flag.at_base && flag.carrier_id.is_none(), "{flag:?}");
        assert_eq!(flag.position, flag.base_position);
    }
    assert_eq!(s.board.objectives.len(), 2, "the objectives stay placed");
    assert_eq!(
        (s.board.score(1), s.board.score(2)),
        (0, 0),
        "scores are per round"
    );
    assert_eq!(s.flow.phases.round_wins_of(2), 1, "round wins are kept");
    assert_eq!(s.flow.phases.rounds_played, 2);
    assert_eq!(s.flow.phases.time_left(&s.flow.settings.phases), Some(10));
    assert!(!s.board.match_over);
}

#[test]
fn a_team_with_no_seats_forfeits_after_the_setting() {
    let phases = PhaseSettings {
        forfeit_after_ticks: Some(50),
        ..PhaseSettings::default()
    };
    let mut s = koth(phases, false);
    let mut events = s.drain_events();
    for _ in 0..10 {
        hold(&mut s, 0, &mut events);
    }
    // Team 2 leaves for 30 ticks and comes back: the count starts over.
    s.apply(&FlowInput::Leave { team: 2, slot: 0 }).expect("leave");
    for _ in 0..30 {
        hold(&mut s, 0, &mut events);
    }
    s.apply(&FlowInput::Seat { team: 2, slot: 0 }).expect("seat");
    hold(&mut s, 0, &mut events);
    // Team 2 goes to watch: an observer seat does not count.
    s.apply(&FlowInput::Observe { team: 2, slot: 0 })
        .expect("observe");
    for _ in 0..49 {
        hold(&mut s, 0, &mut events);
    }
    assert!(!s.board.match_over, "49 absent ticks are under the setting");
    hold(&mut s, 0, &mut events);
    assert_eq!(match_over(&events), [(s.flow.match_tick, 1, EndReason::Forfeit)]);
    assert_eq!(s.flow.phases.forfeited, [2]);
}

#[test]
fn the_post_match_intermission_opens_the_ready_gate() {
    let phases = PhaseSettings {
        time_limit_ticks: Some(5),
        post_match_ticks: Some(8),
        ..PhaseSettings::default()
    };
    let mut s = koth(phases, false);
    let mut events = s.drain_events();
    for _ in 0..5 {
        hold(&mut s, 1, &mut events);
    }
    assert_eq!(s.flow.phases.phase, MatchPhase::PostMatch);
    for _ in 0..7 {
        hold(&mut s, 1, &mut events);
    }
    assert!(!s.flow.between);
    hold(&mut s, 1, &mut events);
    assert!(s.flow.between, "the intermission ends in the ready gate");
    assert_eq!(s.board.score(1), 5, "nothing scores after the end");
}

#[test]
fn a_reported_condition_wins_a_tie_on_the_same_tick() {
    // On the tick the time limit runs out, the host reports a kit rule for team 2: the reported
    // condition is first in the order, so team 2 wins although team 1 leads on time.
    let phases = PhaseSettings {
        time_limit_ticks: Some(6),
        ..PhaseSettings::default()
    };
    let mut s = koth(phases, false);
    let mut events = s.drain_events();
    for _ in 0..5 {
        hold(&mut s, 1, &mut events);
    }
    s.report_condition(2, EndReason::Ended);
    hold(&mut s, 1, &mut events);
    assert_eq!(match_over(&events), [(s.flow.match_tick, 2, EndReason::Ended)]);
}

fn fnv(bytes: &[u8], mut hash: u64) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    hash
}

/// The S-14 capture-the-flag twin run of `tests/match_rules.rs` (regia_framework), hashed over
/// the board, the events and the clocks. Returns that hash (the board projected onto its S-14
/// shape, [`S14Board`]), the hash of the events and the clocks alone (no board), the events
/// and the session.
fn s14_ctf_run(settings: SessionSettings) -> (u64, u64, Vec<RulesEvent>, MatchSession) {
    let mut session = MatchSession::new(settings);
    session.playlist.set(vec![
        PlaylistEntry::new("harbor", "ctf"),
        PlaylistEntry::new("quarry", "koth"),
    ]);
    session.start_now();
    session.board.place_flags(&[1, 2], 120.0, 10).expect("flags");
    let mut events = session.drain_events();
    let raid = |tick: u64, team: u8| -> [f32; 3] {
        let sign = if team == 1 { 1.0 } else { -1.0 };
        let phase = (tick % 160) as f32;
        let x = if phase < 80.0 {
            -120.0 + 3.0 * phase
        } else {
            120.0 - 3.0 * (phase - 80.0)
        };
        [sign * x, 0.0, 0.5 * (tick % 9) as f32 - 2.0]
    };
    for tick in 0..2000u64 {
        let units = [
            Contestant {
                id: 1001,
                team: 1,
                position: raid(tick, 1),
                alive: tick % 700 != 500,
            },
            Contestant {
                id: 2001,
                team: 2,
                position: raid(tick + 37, 2),
                alive: true,
            },
        ];
        events.extend(session.step(&units).events);
    }
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    let board = serde_json::to_string(&S14Board::of(&session.board)).expect("board");
    let feed = serde_json::to_string(&events).expect("events");
    hash = fnv(board.as_bytes(), hash);
    hash = fnv(feed.as_bytes(), hash);
    hash = fnv(&session.flow.match_tick.to_le_bytes(), hash);
    hash = fnv(&session.flow.step.to_le_bytes(), hash);
    let mut feed_hash = 0xcbf2_9ce4_8422_2325u64;
    feed_hash = fnv(feed.as_bytes(), feed_hash);
    feed_hash = fnv(&session.flow.match_tick.to_le_bytes(), feed_hash);
    feed_hash = fnv(&session.flow.step.to_le_bytes(), feed_hash);
    (hash, feed_hash, events, session)
}

/// The board projected onto the shape S-14 serialized (b72ce320b): its fields, in its order. The
/// fields the board and its settings gained since (each objective's drop tick and zone, the event
/// drop count, the zone rules, the locked zones; touch height, flag return timer, capture rule and
/// the pending-event cap) are left out here, and [`assert_post_s14_fields_off`] checks that every
/// one of them is off or at its default after the run. So the hash compares exactly what S-14
/// could see, and a later field that moved would fail the assert by name instead of re-pinning an
/// opaque value. The event feed is hashed whole, unprojected: a new event in a phases-off run moves
/// the hash.
#[derive(Serialize)]
struct S14Board<'a> {
    mode: MatchMode,
    settings: S14Settings,
    objectives: Vec<S14Objective<'a>>,
    team_scores: &'a [u32],
    match_over: bool,
    match_over_tick: Option<u64>,
    end_reason: Option<EndReason>,
}

/// `ObjectiveSettings` as S-14 serialized it.
#[derive(Serialize)]
struct S14Settings {
    max_teams: u8,
    max_objectives: u16,
    flag_touch_radius: f32,
    capture_limit: u32,
    hill_score_limit: u32,
    hill_accrual_ticks: u32,
}

/// `Objective` as S-14 serialized it.
#[derive(Serialize)]
struct S14Objective<'a> {
    entity_id: u32,
    kind: &'a ObjectiveKind,
    team_id: u8,
    carrier_id: Option<u32>,
    at_base: bool,
    base_position: [f32; 3],
    position: [f32; 3],
    volume: Option<HillVolume>,
}

impl<'a> S14Board<'a> {
    fn of(board: &'a ObjectiveBoard) -> Self {
        let s = &board.settings;
        Self {
            mode: board.mode,
            settings: S14Settings {
                max_teams: s.max_teams,
                max_objectives: s.max_objectives,
                flag_touch_radius: s.flag_touch_radius,
                capture_limit: s.capture_limit,
                hill_score_limit: s.hill_score_limit,
                hill_accrual_ticks: s.hill_accrual_ticks,
            },
            objectives: board
                .objectives
                .iter()
                .map(|o| S14Objective {
                    entity_id: o.entity_id,
                    kind: &o.kind,
                    team_id: o.team_id,
                    carrier_id: o.carrier_id,
                    at_base: o.at_base,
                    base_position: o.base_position,
                    position: o.position,
                    volume: o.volume,
                })
                .collect(),
            team_scores: &board.team_scores,
            match_over: board.match_over,
            match_over_tick: board.match_over_tick,
            end_reason: board.end_reason,
        }
    }
}

/// Every board and session field outside [`S14Board`] is off or at its default after the run:
/// the projection leaves out nothing a phases-off match used.
fn assert_post_s14_fields_off(session: &MatchSession) {
    let board = &session.board;
    let defaults = ObjectiveSettings::default();
    assert_eq!(
        (
            board.settings.touch_height,
            board.settings.flag_return_ticks,
            board.settings.capture_needs_home_flag,
            board.settings.max_pending_events,
        ),
        (
            defaults.touch_height,
            defaults.flag_return_ticks,
            defaults.capture_needs_home_flag,
            defaults.max_pending_events,
        ),
        "the post-S-14 objective settings keep their defaults"
    );
    assert_eq!(board.events_dropped, 0, "no event was dropped");
    assert_eq!(board.zone_rules, ZoneRules::default(), "no zone rules");
    assert!(board.locked.is_empty(), "no locked zone");
    for objective in &board.objectives {
        assert!(objective.zone.is_none(), "no capture zone");
        assert!(
            objective.dropped_tick.is_none() || board.settings.flag_return_ticks.is_none(),
            "a drop tick only matters to the return timer, which is off"
        );
    }
    let settings = session.settings();
    let rust = SessionSettings::default();
    assert_eq!(
        (
            &settings.waves,
            &settings.race,
            &settings.zone,
            &settings.elimination,
            &settings.victory,
        ),
        (
            &rust.waves,
            &rust.race,
            &rust.zone,
            &rust.elimination,
            &rust.victory
        ),
        "the genre sections are off"
    );
}

/// The hash of the S-14 run on the base (b72ce320b), before phases existed: the board projected
/// onto its S-14 shape ([`S14Board`]), the events and the clocks (ADR-8851: the pin ignores the
/// sections that are off instead of re-pinning; [`assert_board_fields_known`] pins the rest of the
/// board by name).
const S14_CTF_HASH: u64 = 0x4189_afe5_866f_69b0;

/// The hash of the S-14 run's events and clocks alone, as on the base (b72ce320b, before phases existed).
/// Phase-off behavior must leave this unchanged; a new serialized board field does not move it.
const S14_CTF_FEED_HASH: u64 = 0x65d6_e31d_6c8c_798a;

/// Everything the serialized board holds outside the [`S14Board`] projection after the phases-off
/// run, as JSON (ADR-8851). The v0.6 freeze re-pinned the whole board (`0x1cd8_db9c_792e_0949`,
/// lane v06-freeze-reds-h) because exactly these five fields joined it, each with its feature off:
/// the objective-height-return settings (ADR-8506) and the proof suite's event cap. Pinning them by
/// value here keeps that check (a changed default fails) without an opaque hash that moves with
/// every later field.
fn post_s14_board_remainder() -> serde_json::Value {
    serde_json::json!({
        "settings": {
            "touch_height": null,
            "flag_return_ticks": null,
            "capture_needs_home_flag": true,
            "max_pending_events": 4096,
        },
        "events_dropped": 0,
    })
}

/// The keys of `full` that `s14` lacks, with their values; nested objects listed in `nested` are
/// compared the same way, one level down. Every `s14` key must be in `full` (a renamed S-14 field
/// fails here).
fn remainder(
    full: &serde_json::Value,
    s14: &serde_json::Value,
    nested: &[&str],
    at: &str,
) -> serde_json::Map<String, serde_json::Value> {
    let (Some(full), Some(s14)) = (full.as_object(), s14.as_object()) else {
        panic!("{at}: the board does not serialize as an object");
    };
    for key in s14.keys() {
        assert!(full.contains_key(key), "{at}: the S-14 field `{key}` is gone");
    }
    let mut rest = serde_json::Map::new();
    for (key, value) in full {
        if nested.contains(&key.as_str()) {
            let inner = remainder(value, &s14[key], &[], &format!("{at}.{key}"));
            if !inner.is_empty() {
                rest.insert(key.clone(), serde_json::Value::Object(inner));
            }
        } else if !s14.contains_key(key) {
            rest.insert(key.clone(), value.clone());
        }
    }
    rest
}

/// The serialized board is the [`S14Board`] projection plus exactly [`post_s14_board_remainder`]
/// (ADR-8851). A field the board gains later, or a post-S-14 field whose serialized value moves
/// with the phases off, fails here by name.
fn assert_board_fields_known(board: &ObjectiveBoard) {
    let full = serde_json::to_value(board).expect("board");
    let s14 = serde_json::to_value(S14Board::of(board)).expect("S-14 board");
    let mut rest = remainder(&full, &s14, &["settings"], "board");
    let (Some(objectives), Some(s14_objectives)) =
        (full["objectives"].as_array(), s14["objectives"].as_array())
    else {
        panic!("board.objectives is not a list");
    };
    assert_eq!(objectives.len(), s14_objectives.len());
    for (index, (objective, s14_objective)) in objectives.iter().zip(s14_objectives).enumerate() {
        let inner = remainder(
            objective,
            s14_objective,
            &[],
            &format!("board.objectives[{index}]"),
        );
        assert!(
            inner.is_empty(),
            "board.objectives[{index}] serializes fields outside the S-14 shape: {inner:?}"
        );
    }
    rest.remove("objectives");
    assert_eq!(
        serde_json::Value::Object(rest),
        post_s14_board_remainder(),
        "the board outside the S-14 projection (ADR-8851): add a new field to \
         `post_s14_board_remainder` and `assert_post_s14_fields_off` only with its feature off"
    );
}

#[test]
fn all_phase_settings_off_keeps_s14_bits() {
    assert!(PhaseSettings::default().is_off());
    assert_eq!(FlowSettings::default().phases, PhaseSettings::default());
    let settings = SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 90,
            ..FlowSettings::default()
        },
        objectives: ObjectiveSettings {
            capture_limit: 2,
            ..ObjectiveSettings::default()
        },
        ..SessionSettings::default()
    };
    let (hash, feed_hash, events, session) = s14_ctf_run(settings);
    assert_board_fields_known(&session.board);
    assert_post_s14_fields_off(&session);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, RulesEvent::PhaseChanged { .. } | RulesEvent::RoundOver { .. })),
        "no phase events with the phases off"
    );
    assert_eq!(
        feed_hash, S14_CTF_FEED_HASH,
        "phases off: the events and clocks are the base's: {feed_hash:#018x}"
    );
    assert_eq!(hash, S14_CTF_HASH, "{hash:#018x}");
}

/// ADR-8851: the guard that replaced the whole-board re-pin still catches a post-S-14 field whose
/// serialized value moves (here the event cap), by name.
#[test]
#[should_panic(expected = "the board outside the S-14 projection")]
fn the_s14_board_guard_names_a_moved_post_s14_field() {
    let (_, _, _, mut session) = s14_ctf_run(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 90,
            ..FlowSettings::default()
        },
        objectives: ObjectiveSettings {
            capture_limit: 2,
            ..ObjectiveSettings::default()
        },
        ..SessionSettings::default()
    });
    assert_board_fields_known(&session.board);
    session.board.settings.max_pending_events = 8192;
    assert_board_fields_known(&session.board);
}

#[test]
fn a_new_session_opens_in_its_warmup_and_settings_parse_from_json() {
    let phases: PhaseSettings = serde_json::from_str(
        r#"{ "warmup_ticks": 600, "on_time_up": { "kind": "overtime", "ticks": 3600 },
             "rounds": { "rounds_to_win": 3, "max_rounds": 5 } }"#,
    )
    .expect("phases parse");
    assert_eq!(phases.on_time_up, TimeUp::Overtime { ticks: Some(3600) });
    let rounds = phases.rounds.expect("rounds");
    assert_eq!(
        (rounds.swap_sides_every, rounds.round_end_ticks),
        (0, RoundSettings::default().round_end_ticks),
        "missing round fields keep their defaults"
    );
    let sudden: TimeUp = serde_json::from_str(r#"{ "kind": "sudden_death" }"#).expect("parse");
    assert_eq!(sudden, TimeUp::SuddenDeath);

    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            phases,
            ..FlowSettings::default()
        },
        ..SessionSettings::default()
    });
    assert_eq!(session.flow.phases.phase, MatchPhase::Warmup);
    assert_eq!(session.flow.phases.round, 1);
    let first = session.step(&[]);
    assert!(
        !first
            .events
            .iter()
            .any(|e| matches!(e, RulesEvent::PhaseChanged { .. })),
        "the opening phase of a new session is silent; start_now announces a match"
    );
    session.start_now();
    let launch = session.drain_events();
    let opened: Vec<_> = launch
        .iter()
        .filter(|e| matches!(e, RulesEvent::PhaseChanged { .. }))
        .collect();
    assert_eq!(opened.len(), 1, "{launch:?}");
    let text = serde_json::to_string(&session).expect("serialize");
    let back: MatchSession = serde_json::from_str(&text).expect("deserialize");
    assert_eq!(back.flow.phases, session.flow.phases);
    assert_eq!(back.flow.settings, session.flow.settings);
}
