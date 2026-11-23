// SPDX-License-Identifier: MPL-2.0
//! Plan row S-14 acceptance (ADR-8102): the match flow starts the next match on an agreed lockstep
//! step that every peer computes from the same inputs, and a capture-the-flag match played on two
//! peers stays bit-identical.

use regia_framework::rules::match_flow::{FlowInput, FlowSettings, Issuer, SeatId};
use regia_framework::rules::objective::{Contestant, ObjectiveSettings, RulesEvent};
use regia_framework::rules::playlist::PlaylistEntry;
use regia_framework::rules::session::{MatchSession, SessionSettings};

const DELAY: u64 = 90;

fn peer() -> MatchSession {
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: DELAY,
            ..FlowSettings::default()
        },
        objectives: ObjectiveSettings {
            capture_limit: 2,
            ..ObjectiveSettings::default()
        },
        ..SessionSettings::default()
    });
    session.playlist.set(vec![
        PlaylistEntry::new("harbor", "ctf"),
        PlaylistEntry::new("quarry", "koth"),
    ]);
    session
}

/// The inputs of one lockstep step, in the order this peer received them. Lockstep peers agree on
/// the step an input applies to, not on its arrival order inside a step.
fn inputs_for(step: u64, reversed: bool) -> Vec<FlowInput> {
    let mut inputs = match step {
        0 => vec![
            FlowInput::Seat { team: 1, slot: 0 },
            FlowInput::Seat { team: 2, slot: 0 },
            FlowInput::Seat { team: 2, slot: 1 },
        ],
        1 => vec![FlowInput::OpenBetween],
        40 => vec![FlowInput::Ready { team: 2, slot: 1 }],
        // The last two readies land on the same step.
        75 => vec![
            FlowInput::Ready { team: 1, slot: 0 },
            FlowInput::Ready { team: 2, slot: 0 },
        ],
        _ => Vec::new(),
    };
    if reversed {
        inputs.reverse();
    }
    inputs
}

#[test]
fn ready_up_starts_on_an_agreed_tick() {
    let mut peers = [peer(), peer()];
    let mut launched_on: [Option<u64>; 2] = [None, None];
    let mut scheduled: [Option<u64>; 2] = [None, None];
    for step in 0..400u64 {
        for (i, session) in peers.iter_mut().enumerate() {
            assert_eq!(session.flow.step, step, "inputs land on the step about to run");
            for input in inputs_for(step, i == 1) {
                session.apply(&input).expect("input applies");
            }
            let report = session.step(&[]);
            for event in &report.events {
                if let RulesEvent::LaunchScheduled { at_step } = event {
                    scheduled[i] = Some(*at_step);
                }
            }
            if report.flow.launched {
                assert!(launched_on[i].is_none(), "one launch per ready-up");
                launched_on[i] = Some(report.flow.step);
            } else if launched_on[i].is_none() && step > 1 {
                assert!(
                    !report.flow.ran,
                    "peer {i}: the match holds until the agreed step"
                );
            }
        }
    }
    // The last ready landed on step 75: the start is that step plus the agreed delay, on every peer.
    assert_eq!(scheduled, [Some(75 + DELAY); 2]);
    assert_eq!(launched_on, [Some(75 + DELAY); 2]);
    assert_eq!(peers[0], peers[1], "both peers hold the same session state");
    let row = peers[0].current.as_ref().expect("a row is playing");
    assert_eq!((row.index, row.entry.map_id.as_str()), (0, "harbor"));
    // The match ran from the start step on: one tick per step after it.
    assert_eq!(peers[0].flow.match_tick, 1 + (400 - (75 + DELAY)));
}

#[test]
fn a_ready_seat_that_unreadies_in_the_countdown_moves_the_agreed_tick() {
    let mut a = peer();
    let mut b = peer();
    for session in [&mut a, &mut b] {
        for input in inputs_for(0, false) {
            session.apply(&input).expect("seat");
        }
        session.step(&[]);
        session.apply(&FlowInput::OpenBetween).expect("between");
        for slot_input in [
            FlowInput::Ready { team: 1, slot: 0 },
            FlowInput::Ready { team: 2, slot: 0 },
            FlowInput::Ready { team: 2, slot: 1 },
        ] {
            session.apply(&slot_input).expect("ready");
        }
        assert_eq!(session.flow.starts_at, Some(1 + DELAY));
        for _ in 0..30 {
            session.step(&[]);
        }
        session
            .apply(&FlowInput::Unready { team: 2, slot: 1 })
            .expect("unready");
        assert_eq!(session.flow.starts_at, None);
        for _ in 0..10 {
            session.step(&[]);
        }
        session
            .apply(&FlowInput::Ready { team: 2, slot: 1 })
            .expect("ready again");
    }
    assert_eq!(a.flow.starts_at, Some(41 + DELAY));
    assert_eq!(a, b);
}

#[test]
fn a_ctf_match_on_two_peers_stays_bit_identical() {
    fn play() -> (MatchSession, Vec<RulesEvent>) {
        let mut session = peer();
        session.start_now();
        session.board.place_flags(&[1, 2], 120.0, 10).expect("flags");
        let mut events = session.drain_events();
        // A raider per team runs at the enemy flag and back home, wobbling on Z.
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
        (session, events)
    }
    let (a, events_a) = play();
    let (b, events_b) = play();
    assert_eq!(a, b);
    assert_eq!(events_a, events_b);
    assert!(
        events_a.iter().any(|e| matches!(e, RulesEvent::FlagTaken { .. })),
        "the raiders reach the flags: {events_a:?}"
    );
    for (fa, fb) in a.board.objectives.iter().zip(&b.board.objectives) {
        assert_eq!(fa.position.map(f32::to_bits), fb.position.map(f32::to_bits));
    }
}

/// One input with the issuer the session layer would attach. The input bytes do not carry it.
#[derive(Clone)]
struct Stamped {
    issuer: Issuer,
    input: FlowInput,
}

fn stamped(issuer: Issuer, input: FlowInput) -> Stamped {
    Stamped { issuer, input }
}

/// Leaves, join asks and refused guest inputs, one peer receiving each step reversed.
#[test]
fn leaves_join_asks_and_refused_guest_inputs_stay_bit_identical() {
    let host = Issuer::host();
    let g10 = Issuer::guest(SeatId::new(1, 0));
    let g20 = Issuer::guest(SeatId::new(2, 0));
    let g21 = Issuer::guest(SeatId::new(2, 1));
    let g11 = Issuer::guest(SeatId::new(1, 1));
    let g15 = Issuer::guest(SeatId::new(1, 5));
    let script = [
        vec![
            stamped(host, FlowInput::Seat { team: 1, slot: 0 }),
            stamped(host, FlowInput::Seat { team: 2, slot: 0 }),
            stamped(host, FlowInput::Seat { team: 2, slot: 1 }),
            stamped(host, FlowInput::Observe { team: 1, slot: 1 }),
            stamped(host, FlowInput::Observe { team: 1, slot: 5 }),
        ],
        vec![stamped(host, FlowInput::OpenBetween)],
        vec![
            stamped(g21, FlowInput::Leave { team: 2, slot: 1 }),
            stamped(g10, FlowInput::Ready { team: 2, slot: 0 }),
            stamped(g10, FlowInput::Observe { team: 2, slot: 0 }),
            stamped(g10, FlowInput::OverrideAfk),
            stamped(g10, FlowInput::HostPause { on: true }),
            stamped(g10, FlowInput::ApproveJoin { team: 1, slot: 0 }),
            stamped(g10, FlowInput::OpenBetween),
            stamped(g10, FlowInput::Seat { team: 2, slot: 0 }),
        ],
        vec![
            stamped(g10, FlowInput::Ready { team: 1, slot: 0 }),
            stamped(g20, FlowInput::Ready { team: 2, slot: 0 }),
        ],
        vec![
            stamped(g11, FlowInput::Ready { team: 1, slot: 1 }),
            stamped(g15, FlowInput::Ready { team: 1, slot: 5 }),
        ],
    ];
    let mut peers = [peer_delay(2), peer_delay(2)];
    let mut refusals = [Vec::new(), Vec::new()];
    let mut saw_launch = [false; 2];
    for step in 0..8u64 {
        for (i, session) in peers.iter_mut().enumerate() {
            let mut batch = if (step as usize) < script.len() {
                script[step as usize].clone()
            } else {
                Vec::new()
            };
            if i == 1 {
                batch.reverse();
            }
            let mut codes = Vec::new();
            for item in &batch {
                if let Err(err) = session.apply_from(item.issuer, &item.input) {
                    codes.push(err.code().to_string());
                }
            }
            codes.sort();
            refusals[i].push(codes);
            if session.step(&[]).flow.launched {
                saw_launch[i] = true;
            }
        }
        assert_eq!(peers[0], peers[1], "step {step}");
        assert_eq!(
            refusals[0][step as usize], refusals[1][step as usize],
            "step {step}"
        );
    }
    assert_eq!(saw_launch, [true, true]);
    assert!(
        refusals[0]
            .iter()
            .flatten()
            .any(|code| code == "match_not_permitted")
    );
    assert!(!peers[0].flow.seated.contains(&SeatId::new(2, 1)));
    assert_eq!(peers[0].flow.joins.len(), 2);
    assert_eq!(peers[0].flow.paused_by_seat, None);
}

fn peer_delay(delay: u64) -> MatchSession {
    MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: delay,
            ..FlowSettings::default()
        },
        ..SessionSettings::default()
    })
}

/// ADR-8485: two peers, one receiving each step's inputs reversed, play a king-of-the-hill match
/// through a warm-up, a countdown, rounds that each go to overtime, side swaps and the post-match
/// intermission, and hold the same state on every step.
#[test]
fn two_peers_with_reversed_inputs_agree_through_warmup_rounds_and_overtime() {
    use regia_framework::rules::objective::{EndReason, HillVolume};
    use regia_framework::rules::phases::{MatchPhase, PhaseSettings, RoundSettings, TimeUp};

    fn phased_peer() -> MatchSession {
        let mut session = MatchSession::new(SessionSettings {
            flow: FlowSettings {
                start_delay_ticks: 4,
                phases: PhaseSettings {
                    warmup_ticks: Some(40),
                    warmup_min_seats: Some(3),
                    countdown_ticks: 5,
                    time_limit_ticks: Some(30),
                    on_time_up: TimeUp::Overtime { ticks: Some(20) },
                    rounds: Some(RoundSettings {
                        rounds_to_win: 2,
                        max_rounds: 3,
                        swap_sides_every: 1,
                        round_end_ticks: 6,
                    }),
                    post_match_ticks: Some(10),
                    forfeit_after_ticks: Some(1000),
                },
                ..FlowSettings::default()
            },
            objectives: ObjectiveSettings {
                hill_accrual_ticks: 1,
                hill_score_limit: 10_000,
                ..ObjectiveSettings::default()
            },
            ..SessionSettings::default()
        });
        session.playlist.set(vec![PlaylistEntry::new("quarry", "koth")]);
        session
    }

    fn inputs(step: u64, reversed: bool) -> Vec<FlowInput> {
        let mut out = match step {
            0 => vec![
                FlowInput::Seat { team: 1, slot: 0 },
                FlowInput::Seat { team: 2, slot: 0 },
            ],
            1 => vec![FlowInput::OpenBetween],
            3 => vec![
                FlowInput::Ready { team: 1, slot: 0 },
                FlowInput::Ready { team: 2, slot: 0 },
            ],
            20 => vec![
                FlowInput::Seat { team: 2, slot: 1 },
                FlowInput::Seat { team: 1, slot: 1 },
            ],
            _ => Vec::new(),
        };
        if reversed {
            out.reverse();
        }
        out
    }

    let mut peers = [phased_peer(), phased_peer()];
    let mut logs: [Vec<RulesEvent>; 2] = [Vec::new(), Vec::new()];
    for step in 0..600u64 {
        // Teams take turns on the hill, three steps each: every round ties at the time limit.
        let holder = if step % 6 < 3 { 1 } else { 2 };
        let units = [
            Contestant {
                id: 101,
                team: 1,
                position: if holder == 1 { [0.0; 3] } else { [50.0, 0.0, 0.0] },
                alive: true,
            },
            Contestant {
                id: 201,
                team: 2,
                position: if holder == 2 { [0.0; 3] } else { [50.0, 0.0, 0.0] },
                alive: true,
            },
        ];
        for (i, session) in peers.iter_mut().enumerate() {
            for input in inputs(step, i == 1) {
                session.apply(&input).expect("input applies");
            }
            let report = session.step(&units);
            if report.flow.launched {
                session
                    .board
                    .install_hill_volume(
                        9,
                        HillVolume {
                            min: [-2.0, -1.0, -2.0],
                            max: [2.0, 2.0, 2.0],
                        },
                    )
                    .expect("hill");
            }
            logs[i].extend(report.events);
        }
        assert_eq!(peers[0], peers[1], "step {step}");
    }
    assert_eq!(logs[0], logs[1]);
    let seen: Vec<MatchPhase> = logs[0]
        .iter()
        .filter_map(|e| match e {
            RulesEvent::PhaseChanged { phase, .. } => Some(*phase),
            _ => None,
        })
        .collect();
    for phase in MatchPhase::ALL {
        assert!(seen.contains(&phase), "{phase:?} in {seen:?}");
    }
    let rounds = logs[0]
        .iter()
        .filter(|e| matches!(e, RulesEvent::RoundOver { .. }))
        .count();
    assert!((2..=3).contains(&rounds), "{rounds} rounds");
    assert_eq!(
        logs[0]
            .iter()
            .filter(|e| matches!(
                e,
                RulesEvent::MatchOver {
                    reason: EndReason::RoundsWon | EndReason::Draw,
                    ..
                }
            ))
            .count(),
        1
    );
    assert!(peers[0].flow.between, "the intermission ended in the ready gate");
    // One swap before each round after the first.
    let swapped = (rounds - 1) % 2 == 1;
    assert_eq!(
        peers[0].flow.phases.side_of(1),
        if swapped { 2 } else { 1 },
        "{rounds} rounds"
    );
}

/// Two peers that seat, assign and score the same way hold the same ledger.
#[test]
fn twin_peers_hold_the_same_ledger() {
    use regia_framework::rules::teams::SeatRequest;

    fn play() -> MatchSession {
        let mut session = peer_delay(0);
        session
            .apply(&FlowInput::Seat { team: 1, slot: 0 })
            .expect("seat");
        let seats = session
            .assign_team(
                &SeatRequest {
                    unit: Some(7),
                    ..SeatRequest::default()
                },
                &[SeatRequest {
                    unit: Some(8),
                    ..SeatRequest::default()
                }],
            )
            .expect("party");
        assert_eq!(seats[0].team, seats[1].team);
        session.flow.match_tick = 4;
        session.note_hit(seats[0], seats[1]);
        session.flow.match_tick = 10;
        session.credit_kill(seats[1], seats[0]);
        session.step(&[]);
        session
    }
    let left = play();
    let right = play();
    assert_eq!(left, right);
    assert_eq!(left.ledgers.rows.len(), right.ledgers.rows.len());
    assert!(left.ledgers.rows.iter().any(|row| row.kills == 1));
    assert!(left.ledgers.rows.iter().any(|row| row.deaths == 1));
}

#[test]
fn match_rules_ctf_koth_3600_matches_the_committed_hash() {
    use regia_framework::rules::proof::MatchRulesCtfKoth3600;
    use regia_test_utils::det_golden::{self as golden, DetRegistry, DetScenario, GoldenMode, REPORT_PREFIX};

    let mut registry = DetRegistry::new();
    registry
        .register(golden::engine_golden_dir(), || {
            Box::new(MatchRulesCtfKoth3600::new()) as Box<dyn DetScenario>
        })
        .expect("match_rules_ctf_koth_3600 registers");
    assert!(
        registry.get(MatchRulesCtfKoth3600::new().name()).is_some(),
        "no scenario"
    );
    let mut failed = Vec::new();
    for report in registry.check_all(GoldenMode::from_env()) {
        println!("{REPORT_PREFIX}{}", report.to_json());
        if !report.passed() {
            failed.push(report.to_string());
        }
    }
    assert!(
        failed.is_empty(),
        "determinism golden differs on {}:\n{}",
        golden::current_target(),
        failed.join("\n")
    );
}

#[test]
fn input_permutations_within_a_step_never_change_state() {
    use regia_framework::rules::objective::{self, Contestant};
    use regia_test_utils::det_golden::scenarios::SplitMix64;

    let mut rng = SplitMix64::new(0xC7F0_0011);
    for case in 0..1000u32 {
        let mut a = peer_delay(0);
        let mut b = peer_delay(0);
        let mut seats = vec![
            FlowInput::Seat { team: 1, slot: 0 },
            FlowInput::Seat { team: 1, slot: 1 },
            FlowInput::Seat { team: 2, slot: 0 },
            FlowInput::Seat { team: 2, slot: 2 },
        ];
        let extra = (rng.next_u64() % 4) as usize;
        for n in 0..extra {
            seats.push(FlowInput::Seat {
                team: 1 + (n as u8 % 2),
                slot: 3 + n as u8,
            });
        }
        apply_permuted(&mut a, &mut b, &seats, &mut rng);
        assert_eq!(a.step(&[]), b.step(&[]), "case {case}: seat step");
        assert_eq!(a, b, "case {case}: seats");

        a.apply(&FlowInput::OpenBetween).expect("open");
        b.apply(&FlowInput::OpenBetween).expect("open");
        let readies: Vec<FlowInput> = a
            .flow
            .seated
            .iter()
            .map(|seat| FlowInput::Ready {
                team: seat.team,
                slot: seat.slot,
            })
            .collect();
        apply_permuted(&mut a, &mut b, &readies, &mut rng);
        assert_eq!(a.step(&[]), b.step(&[]), "case {case}: ready step");
        assert_eq!(a, b, "case {case}: readies");

        a.board
            .reset(regia_framework::rules::mode::MatchMode::CaptureTheFlag);
        b.board
            .reset(regia_framework::rules::mode::MatchMode::CaptureTheFlag);
        a.board.place_flags(&[1, 2], 20.0, 10).expect("flags");
        b.board.place_flags(&[1, 2], 20.0, 10).expect("flags");
        let mut units = Vec::new();
        for n in 0..4u32 {
            let x = f32::from(u16::try_from(rng.next_u64() % 400).unwrap_or(0)) / 10.0 - 20.0;
            let z = f32::from(u16::try_from(rng.next_u64() % 400).unwrap_or(0)) / 10.0 - 20.0;
            units.push(Contestant {
                id: 100 + n,
                team: 1 + (n % 2) as u8,
                position: [x, 0.0, z],
                alive: !rng.next_u64().is_multiple_of(5),
            });
        }
        let mut left = units.clone();
        let mut right = units;
        shuffle(&mut left, &mut rng);
        shuffle(&mut right, &mut rng);
        objective::host_unit_order(&mut left);
        objective::host_unit_order(&mut right);
        assert_eq!(left, right, "case {case}: host order");
        assert_eq!(a.step(&left), b.step(&right), "case {case}: unit step");
        assert_eq!(a, b, "case {case}: after units");
    }
}

fn apply_permuted(
    a: &mut MatchSession,
    b: &mut MatchSession,
    inputs: &[FlowInput],
    rng: &mut regia_test_utils::det_golden::scenarios::SplitMix64,
) {
    let mut reversed = inputs.to_vec();
    shuffle(&mut reversed, rng);
    for input in inputs {
        a.apply(input).expect("input applies");
    }
    for input in &reversed {
        b.apply(input).expect("permuted input applies");
    }
}

fn shuffle<T>(items: &mut [T], rng: &mut regia_test_utils::det_golden::scenarios::SplitMix64) {
    if items.len() < 2 {
        return;
    }
    let mut i = items.len() - 1;
    while i > 0 {
        let j = (rng.next_u64() as usize) % (i + 1);
        items.swap(i, j);
        i -= 1;
    }
}

#[test]
fn undrained_events_stop_at_the_cap_and_count_drops() {
    use regia_framework::rules::match_flow::MatchFlow;
    use regia_framework::rules::mode::MatchMode;
    use regia_framework::rules::objective::{Contestant, HillVolume, ObjectiveBoard};

    assert_eq!(
        ObjectiveSettings::default().max_pending_events,
        4096,
        "the default cap is S-14's unbounded log until a host stops draining"
    );
    assert_eq!(ObjectiveSettings::default().effective_max_pending_events(), 4096);

    let mut board = ObjectiveBoard::new(
        MatchMode::KingOfTheHill,
        ObjectiveSettings {
            max_pending_events: 4,
            hill_score_limit: 10_000,
            hill_accrual_ticks: 1,
            ..ObjectiveSettings::default()
        },
    );
    board
        .install_hill_volume(
            1,
            HillVolume {
                min: [-1.0, -1.0, -1.0],
                max: [1.0, 1.0, 1.0],
            },
        )
        .expect("hill");
    let units = [Contestant {
        id: 7,
        team: 1,
        position: [0.0, 0.0, 0.0],
        alive: true,
    }];
    for tick in 1..=10 {
        board.koth_tick(tick, &units);
    }
    assert_eq!(board.events.len(), 4, "the board keeps the newest events");
    assert_eq!(board.events_dropped, 6, "oldest-first drops are counted");
    match board.events.first() {
        Some(RulesEvent::HillPoint { tick: 7, .. }) => {}
        other => panic!("the oldest kept hill point is tick 7, got {other:?}"),
    }

    let mut flags = ObjectiveBoard::new(
        MatchMode::CaptureTheFlag,
        ObjectiveSettings {
            max_pending_events: 8,
            capture_limit: 1_000,
            ..ObjectiveSettings::default()
        },
    );
    flags.place_flags(&[1, 2], 6.0, 1).expect("flags");
    // Alternate a pickup and a death-drop on the enemy base. Each state change emits one event,
    // so forty ticks pass the cap of 8.
    for tick in 0..40u64 {
        flags.ctf_tick(
            tick,
            &[Contestant {
                id: 1,
                team: 1,
                position: [6.0, 0.0, 0.0],
                alive: tick.is_multiple_of(2),
            }],
        );
    }
    assert_eq!(
        flags.events.len(),
        8,
        "undrained ctf_tick keeps the newest events"
    );
    assert!(
        flags.events_dropped > 0,
        "undrained ctf_tick drops the oldest events"
    );

    let mut flow = MatchFlow::new(FlowSettings::default());
    flow.max_pending_events = 3;
    for slot in 0..8u8 {
        flow.apply(&FlowInput::Seat { team: 1, slot }).expect("seat");
        flow.apply(&FlowInput::Leave { team: 1, slot }).expect("leave");
    }
    assert_eq!(flow.events.len(), 3);
    assert_eq!(flow.events_dropped, 5);

    let mut drained = MatchSession::default();
    for _ in 0..3 {
        let _ = drained.step(&[]);
    }
    assert_eq!(drained.board.events_dropped, 0);
    assert_eq!(drained.flow.events_dropped, 0);
    assert!(drained.board.events.is_empty());
    assert!(drained.flow.events.is_empty());
}
