// SPDX-License-Identifier: MPL-2.0
//! Audit gaps GR-6.1 and GR-6.2: flag touches on stacked floors (`touch_height`), a dropped flag
//! that nobody can reach (`flag_return_ticks`, the host drop check) and the capture rule
//! (`capture_needs_home_flag`). Every new setting defaults to S-14's rule; the last test pins the
//! bits of S-14's 2000-step capture-the-flag session.

use regia_rules::ctf::DropCheck;
use regia_rules::match_flow::FlowSettings;
use regia_rules::mode::MatchMode;
use regia_rules::objective::{
    AutoReturnCause, Contestant, Objective, ObjectiveBoard, ObjectiveSettings, RulesEvent,
};
use regia_rules::playlist::PlaylistEntry;
use regia_rules::session::{MatchSession, SessionSettings};

/// Team 1's base, on the ground floor.
const BASE1: [f32; 3] = [0.0, 0.0, 0.0];
/// Team 2's base, on an upper floor ten units above a tunnel at height 0.
const BASE2: [f32; 3] = [100.0, 10.0, 0.0];
/// A touch band that separates floors ten units apart.
const BAND: f32 = 2.0;

fn board(settings: ObjectiveSettings) -> ObjectiveBoard {
    let mut board = ObjectiveBoard::new(MatchMode::CaptureTheFlag, settings);
    board.spawn_flag(1, 1, BASE1).expect("flag 1");
    board.spawn_flag(2, 2, BASE2).expect("flag 2");
    board
}

fn banded() -> ObjectiveSettings {
    ObjectiveSettings {
        touch_height: Some(BAND),
        ..ObjectiveSettings::default()
    }
}

fn unit(id: u32, team: u8, position: [f32; 3]) -> Contestant {
    Contestant {
        id,
        team,
        position,
        alive: true,
    }
}

fn dead(id: u32, team: u8, position: [f32; 3]) -> Contestant {
    Contestant {
        alive: false,
        ..unit(id, team, position)
    }
}

fn flag(board: &ObjectiveBoard, id: u32) -> &Objective {
    board.objective(id).expect("flag")
}

fn flag_mut(board: &mut ObjectiveBoard, id: u32) -> &mut Objective {
    board
        .objectives
        .iter_mut()
        .find(|o| o.entity_id == id)
        .expect("flag")
}

#[test]
fn a_unit_on_the_floor_below_cannot_take_the_flag_with_a_touch_height() {
    // The tunnel runs directly under team 2's base: same XZ, ten units down.
    let below = [BASE2[0], 0.0, BASE2[2]];
    let mut b = board(banded());
    b.ctf_tick(1, &[unit(1001, 1, below)]);
    assert!(flag(&b, 2).at_base, "no pickup through the ceiling");
    assert!(flag(&b, 2).carrier_id.is_none());

    // A return through the floor is refused too: team 2's flag lies on the upper floor, a team 2
    // unit stands under it.
    let drop = [60.0, 10.0, 0.0];
    let lying = flag_mut(&mut b, 2);
    lying.at_base = false;
    lying.position = drop;
    b.ctf_tick(2, &[unit(2001, 2, [60.0, 0.0, 0.0])]);
    assert!(flag(&b, 2).is_dropped(), "no return through the floor");
    // On the flag's floor (inside the band) the same unit returns it, and an enemy takes it.
    b.ctf_tick(3, &[unit(2001, 2, [60.0, 11.5, 0.0])]);
    assert!(flag(&b, 2).at_base, "a return inside the band");
    b.ctf_tick(4, &[unit(1001, 1, [BASE2[0], 8.0, BASE2[2]])]);
    assert_eq!(flag(&b, 2).carrier_id, Some(1001), "a pickup inside the band");

    // S-14's rule (`touch_height: None`): the unit in the tunnel takes the flag through the ceiling.
    let mut s14 = board(ObjectiveSettings::default());
    assert_eq!(s14.settings.touch_height, None);
    s14.ctf_tick(1, &[unit(1001, 1, below)]);
    assert_eq!(
        flag(&s14, 2).carrier_id,
        Some(1001),
        "S-14 touches across XZ only"
    );
}

#[test]
fn a_capture_needs_the_carrier_inside_the_height_band_of_its_base() {
    // Team 1's base is on an upper floor here; its carrier comes back through the tunnel below.
    let upper1 = [0.0, 10.0, 0.0];
    let mut b = ObjectiveBoard::new(MatchMode::CaptureTheFlag, banded());
    b.spawn_flag(1, 1, upper1).expect("flag 1");
    b.spawn_flag(2, 2, [100.0, 0.0, 0.0]).expect("flag 2");
    b.ctf_tick(1, &[unit(1001, 1, [100.0, 0.0, 0.0])]);
    assert_eq!(flag(&b, 2).carrier_id, Some(1001));
    b.ctf_tick(2, &[unit(1001, 1, [0.0, 0.0, 0.0])]);
    assert_eq!(b.score(1), 0, "no capture from the floor below the base");
    assert_eq!(flag(&b, 2).carrier_id, Some(1001), "the flag stays carried");
    b.ctf_tick(3, &[unit(1001, 1, [0.0, 8.5, 0.0])]);
    assert_eq!(b.score(1), 1, "a capture inside the band");
    assert!(flag(&b, 2).at_base);

    // S-14's rule: the same capture from the tunnel scores.
    let mut s14 = ObjectiveBoard::new(MatchMode::CaptureTheFlag, ObjectiveSettings::default());
    s14.spawn_flag(1, 1, upper1).expect("flag 1");
    s14.spawn_flag(2, 2, [100.0, 0.0, 0.0]).expect("flag 2");
    s14.ctf_tick(1, &[unit(1001, 1, [100.0, 0.0, 0.0])]);
    s14.ctf_tick(2, &[unit(1001, 1, [0.0, 0.0, 0.0])]);
    assert_eq!(s14.score(1), 1, "S-14 captures across XZ only");
}

#[test]
fn a_dropped_flag_returns_after_flag_return_ticks_and_the_timer_resets_on_pickup() {
    const RETURN: u64 = 10;
    let settings = ObjectiveSettings {
        flag_return_ticks: Some(RETURN),
        ..ObjectiveSettings::default()
    };
    let flat2 = [100.0, 0.0, 0.0];
    let mut b = ObjectiveBoard::new(MatchMode::CaptureTheFlag, settings);
    b.spawn_flag(1, 1, BASE1).expect("flag 1");
    b.spawn_flag(2, 2, flat2).expect("flag 2");
    let first_drop = [50.0, 0.0, 0.0];
    let second_drop = [60.0, 0.0, 0.0];

    // Tick 1: A takes team 2's flag. Tick 2: A dies, the flag drops.
    b.ctf_tick(1, &[unit(1001, 1, flat2)]);
    b.ctf_tick(2, &[dead(1001, 1, first_drop)]);
    assert!(flag(&b, 2).is_dropped());
    assert_eq!(flag(&b, 2).dropped_tick, Some(2));
    for tick in 3..=6 {
        b.ctf_tick(tick, &[dead(1001, 1, first_drop)]);
        assert!(flag(&b, 2).is_dropped(), "tick {tick}: still lying");
    }
    // Tick 7: teammate B picks it up (the timer clears). Tick 8: B dies, a new timer starts.
    b.ctf_tick(7, &[unit(1002, 1, first_drop)]);
    assert_eq!(flag(&b, 2).carrier_id, Some(1002));
    assert_eq!(flag(&b, 2).dropped_tick, None, "a pickup clears the timer");
    b.ctf_tick(8, &[dead(1002, 1, second_drop)]);
    assert_eq!(flag(&b, 2).dropped_tick, Some(8));
    for tick in 9..8 + RETURN {
        b.ctf_tick(tick, &[dead(1002, 1, second_drop)]);
        assert!(
            flag(&b, 2).is_dropped(),
            "tick {tick}: the first drop's timer (due on tick 12) no longer counts"
        );
    }
    b.drain_events();
    b.ctf_tick(8 + RETURN, &[dead(1002, 1, second_drop)]);
    let home = flag(&b, 2);
    assert!(home.at_base, "returned on tick 18");
    assert_eq!(home.position, flat2);
    assert_eq!(home.dropped_tick, None);
    assert_eq!(
        b.drain_events(),
        vec![RulesEvent::FlagAutoReturned {
            tick: 8 + RETURN,
            flag: 2,
            flag_team: 2,
            cause: AutoReturnCause::Timeout,
        }]
    );

    // S-14's rule (`None`): a dropped flag lies until a teammate touches it.
    let mut s14 = ObjectiveBoard::new(MatchMode::CaptureTheFlag, ObjectiveSettings::default());
    s14.spawn_flag(2, 2, flat2).expect("flag 2");
    s14.ctf_tick(1, &[unit(1001, 1, flat2)]);
    for tick in 2..2000 {
        s14.ctf_tick(tick, &[dead(1001, 1, first_drop)]);
    }
    assert!(flag(&s14, 2).is_dropped(), "S-14 never returns on its own");
}

#[test]
fn a_drop_into_a_kill_volume_returns_the_flag_at_once() {
    // The host's map data: everything below y = -50 is a kill volume.
    let kill = |pos: [f32; 3]| {
        if pos[1] < -50.0 {
            DropCheck::Return
        } else {
            DropCheck::Keep
        }
    };
    let flat2 = [100.0, 0.0, 0.0];
    let pit = [50.0, -100.0, 0.0];
    let mut b = ObjectiveBoard::new(MatchMode::CaptureTheFlag, ObjectiveSettings::default());
    b.spawn_flag(2, 2, flat2).expect("flag 2");
    b.ctf_tick_with(1, &[unit(1001, 1, flat2)], kill);
    assert_eq!(flag(&b, 2).carrier_id, Some(1001));
    b.drain_events();
    // The carrier falls into the pit; a teammate standing at the same point does not re-pick it.
    b.ctf_tick_with(2, &[dead(1001, 1, pit), unit(1002, 1, pit)], kill);
    assert!(flag(&b, 2).at_base, "home on the drop step");
    assert_eq!(flag(&b, 2).position, flat2);
    let events = b.drain_events();
    assert!(matches!(
        events.as_slice(),
        [
            RulesEvent::FlagDropped { tick: 2, flag: 2, .. },
            RulesEvent::FlagAutoReturned {
                tick: 2,
                flag: 2,
                flag_team: 2,
                cause: AutoReturnCause::DropCheck,
            },
        ]
    ));

    // A drop on solid ground is kept, and a point the host later seals (a collapsed tunnel)
    // returns the flag on the first step the check answers `Return`.
    b.ctf_tick_with(3, &[unit(1001, 1, flat2)], kill);
    let tunnel = [40.0, 0.0, 0.0];
    b.ctf_tick_with(4, &[dead(1001, 1, tunnel)], kill);
    assert!(flag(&b, 2).is_dropped(), "solid ground keeps the flag");
    let mut asked = Vec::new();
    b.ctf_tick_with(5, &[], |pos| {
        asked.push(pos);
        DropCheck::Return
    });
    assert_eq!(asked, vec![tunnel], "asked once for the one lying flag");
    assert!(flag(&b, 2).at_base, "sealed: home at once");

    // `ctf_tick` keeps every drop point (S-14): the same fall leaves the flag in the pit.
    let mut s14 = ObjectiveBoard::new(MatchMode::CaptureTheFlag, ObjectiveSettings::default());
    s14.spawn_flag(2, 2, flat2).expect("flag 2");
    s14.ctf_tick(1, &[unit(1001, 1, flat2)]);
    s14.ctf_tick(2, &[dead(1001, 1, pit)]);
    assert!(flag(&s14, 2).is_dropped());
    assert_eq!(flag(&s14, 2).position, pit);
}

#[test]
fn capture_without_home_flag_when_the_setting_is_off() {
    let off = ObjectiveSettings {
        capture_needs_home_flag: false,
        ..ObjectiveSettings::default()
    };
    let flat2 = [100.0, 0.0, 0.0];
    for (settings, expected) in [(ObjectiveSettings::default(), 0), (off, 1)] {
        let mut b = ObjectiveBoard::new(MatchMode::CaptureTheFlag, settings);
        b.spawn_flag(1, 1, BASE1).expect("flag 1");
        b.spawn_flag(2, 2, flat2).expect("flag 2");
        // Team 2 holds team 1's flag far away; team 1's raider takes team 2's flag home.
        b.ctf_tick(1, &[unit(1001, 1, flat2), unit(2001, 2, BASE1)]);
        assert_eq!(flag(&b, 1).carrier_id, Some(2001));
        assert_eq!(flag(&b, 2).carrier_id, Some(1001));
        b.ctf_tick(2, &[unit(1001, 1, BASE1), unit(2001, 2, [-300.0, 0.0, 0.0])]);
        assert_eq!(
            b.score(1),
            expected,
            "capture_needs_home_flag = {}",
            settings.capture_needs_home_flag
        );
        assert_eq!(
            flag(&b, 1).carrier_id,
            Some(2001),
            "the own flag stays with its carrier"
        );
        if expected == 1 {
            assert!(flag(&b, 2).at_base, "the captured flag goes home");
        }
    }
    assert!(ObjectiveSettings::default().capture_needs_home_flag);
}

fn fnv(hash: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *hash ^= u64::from(*b);
        *hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
}

/// S-14's 2000-step capture-the-flag session (`regia_framework/tests/match_rules.rs`), hashed:
/// every event, every flag's state and position bits, the scores, the end tick and the match tick.
fn s14_run_hash(objectives: ObjectiveSettings) -> u64 {
    let mut session = MatchSession::new(SessionSettings {
        flow: FlowSettings {
            start_delay_ticks: 90,
            ..FlowSettings::default()
        },
        objectives,
        ..SessionSettings::default()
    });
    session.playlist.set(vec![
        PlaylistEntry::new("harbor", "ctf"),
        PlaylistEntry::new("quarry", "koth"),
    ]);
    session.start_now();
    session.board.place_flags(&[1, 2], 120.0, 10).expect("flags");
    let mut events: Vec<RulesEvent> = session.drain_events();
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
    for event in &events {
        fnv(&mut hash, format!("{event:?}").as_bytes());
    }
    for o in &session.board.objectives {
        fnv(&mut hash, &o.entity_id.to_le_bytes());
        fnv(&mut hash, &[o.team_id, u8::from(o.at_base)]);
        fnv(&mut hash, &o.carrier_id.unwrap_or(u32::MAX).to_le_bytes());
        for axis in o.position {
            fnv(&mut hash, &axis.to_bits().to_le_bytes());
        }
    }
    for s in &session.board.team_scores {
        fnv(&mut hash, &s.to_le_bytes());
    }
    fnv(
        &mut hash,
        &session.board.match_over_tick.unwrap_or(u64::MAX).to_le_bytes(),
    );
    fnv(&mut hash, &session.flow.match_tick.to_le_bytes());
    hash
}

/// The hash `s14_run_hash` gives on the base this lane started from (b72ce320b), before any of
/// these settings existed.
const S14_TWIN_RUN_HASH: u64 = 0xdf1d_8ac0_a1df_77af;

#[test]
fn defaults_keep_the_s14_twin_run_bits() {
    let explicit = ObjectiveSettings {
        capture_limit: 2,
        touch_height: None,
        flag_return_ticks: None,
        capture_needs_home_flag: true,
        ..ObjectiveSettings::default()
    };
    let defaults = ObjectiveSettings {
        capture_limit: 2,
        ..ObjectiveSettings::default()
    };
    assert_eq!(explicit, defaults, "the S-14 values are the defaults");
    assert_eq!(s14_run_hash(explicit), S14_TWIN_RUN_HASH);
    // A wide band and a timer longer than the run change nothing on a flat map either.
    let flat_band = ObjectiveSettings {
        touch_height: Some(1000.0),
        flag_return_ticks: Some(1_000_000),
        ..defaults
    };
    assert_eq!(s14_run_hash(flat_band), S14_TWIN_RUN_HASH);
}

#[test]
fn the_new_settings_read_from_json_and_default_when_missing() {
    let s14: ObjectiveSettings = serde_json::from_str(r#"{ "capture_limit": 2 }"#).expect("old settings");
    assert_eq!(s14.touch_height, None);
    assert_eq!(s14.flag_return_ticks, None);
    assert!(s14.capture_needs_home_flag);
    let tunnels: ObjectiveSettings = serde_json::from_str(
        r#"{ "touch_height": 2.5, "flag_return_ticks": 1800, "capture_needs_home_flag": false }"#,
    )
    .expect("new settings");
    assert_eq!(tunnels.touch_height, Some(2.5));
    assert_eq!(tunnels.flag_return_ticks, Some(1800));
    assert!(!tunnels.capture_needs_home_flag);
    // A flag at home serializes as before (no `dropped_tick` key); a lying one carries its timer.
    let mut b = board(ObjectiveSettings::default());
    let home = serde_json::to_value(flag(&b, 2)).expect("json");
    assert!(home.get("dropped_tick").is_none());
    b.ctf_tick(1, &[unit(1001, 1, BASE2)]);
    b.ctf_tick(2, &[dead(1001, 1, [50.0, 10.0, 0.0])]);
    let lying = serde_json::to_value(flag(&b, 2)).expect("json");
    assert_eq!(lying["dropped_tick"], 2);
    let back: Objective = serde_json::from_value(lying).expect("back");
    assert_eq!(&back, flag(&b, 2));
    let event = serde_json::to_value(RulesEvent::FlagAutoReturned {
        tick: 9,
        flag: 2,
        flag_team: 2,
        cause: AutoReturnCause::DropCheck,
    })
    .expect("json");
    assert_eq!(event["kind"], "flag_auto_returned");
    assert_eq!(event["cause"], AutoReturnCause::DropCheck.as_str());
}

#[test]
fn a_zero_return_timer_returns_on_the_drop_step_unless_a_teammate_repicks() {
    let settings = ObjectiveSettings {
        flag_return_ticks: Some(0),
        ..ObjectiveSettings::default()
    };
    let flat2 = [100.0, 0.0, 0.0];
    let death = [40.0, 0.0, 0.0];
    let mut b = ObjectiveBoard::new(MatchMode::CaptureTheFlag, settings);
    b.spawn_flag(2, 2, flat2).expect("flag 2");
    b.ctf_tick(1, &[unit(1001, 1, flat2)]);
    // A teammate on the death point re-picks the flag before the timer runs at the end of the step.
    b.ctf_tick(2, &[dead(1001, 1, death), unit(1002, 1, death)]);
    assert_eq!(flag(&b, 2).carrier_id, Some(1002));
    b.ctf_tick(3, &[dead(1002, 1, death)]);
    assert!(flag(&b, 2).at_base, "nobody near: home on the drop step");
}

#[test]
fn a_flag_left_lying_by_hand_starts_its_timer_on_the_next_step() {
    // A flag set lying by the host (or loaded from a session saved before the timer existed) has no
    // drop tick: the first step that sees it starts the timer.
    let settings = ObjectiveSettings {
        flag_return_ticks: Some(5),
        ..ObjectiveSettings::default()
    };
    let mut b = ObjectiveBoard::new(MatchMode::CaptureTheFlag, settings);
    b.spawn_flag(2, 2, [100.0, 0.0, 0.0]).expect("flag 2");
    let lying = flag_mut(&mut b, 2);
    lying.at_base = false;
    lying.position = [10.0, 0.0, 0.0];
    for tick in 100..105 {
        b.ctf_tick(tick, &[]);
        assert!(flag(&b, 2).is_dropped(), "tick {tick}");
    }
    assert_eq!(flag(&b, 2).dropped_tick, Some(100));
    b.ctf_tick(105, &[]);
    assert!(flag(&b, 2).at_base);
}

#[test]
fn the_session_passes_the_drop_check_through_and_twin_peers_agree() {
    fn play() -> (MatchSession, Vec<RulesEvent>) {
        let mut session = MatchSession::new(SessionSettings {
            objectives: ObjectiveSettings {
                touch_height: Some(BAND),
                flag_return_ticks: Some(120),
                capture_limit: 50,
                ..ObjectiveSettings::default()
            },
            ..SessionSettings::default()
        });
        session.playlist.set(vec![PlaylistEntry::new("tunnels", "ctf")]);
        session.start_now();
        session
            .board
            .spawn_flag(1, 1, [-100.0, 0.0, 0.0])
            .expect("flag 1");
        session
            .board
            .spawn_flag(2, 2, [100.0, 10.0, 0.0])
            .expect("flag 2");
        let mut events = session.drain_events();
        for tick in 0..3000u64 {
            let phase = (tick % 200) as f32;
            let x = if phase < 100.0 {
                -100.0 + 2.0 * phase
            } else {
                100.0 - 2.0 * (phase - 100.0)
            };
            // Team 1 walks the upper floor on even laps and the tunnel on odd ones.
            let y = if (tick / 200) % 2 == 0 { 10.0 } else { 0.0 };
            let units = [
                Contestant {
                    id: 1001,
                    team: 1,
                    position: [x, y, 0.0],
                    alive: tick % 333 != 300,
                },
                Contestant {
                    id: 2001,
                    team: 2,
                    position: [-x, 10.0 - y, 0.5 * (tick % 5) as f32],
                    alive: tick % 451 != 400,
                },
            ];
            let report = session.step_with(&units, |pos| {
                if pos[0] > 60.0 && pos[0] < 70.0 {
                    DropCheck::Return
                } else {
                    DropCheck::Keep
                }
            });
            events.extend(report.events);
        }
        (session, events)
    }
    let (a, events_a) = play();
    let (b, events_b) = play();
    assert_eq!(a, b);
    assert_eq!(events_a, events_b);
    assert!(events_a.iter().any(|e| matches!(e, RulesEvent::FlagTaken { .. })));
    assert!(
        events_a
            .iter()
            .any(|e| matches!(e, RulesEvent::FlagDropped { .. }))
    );
    for (fa, fb) in a.board.objectives.iter().zip(&b.board.objectives) {
        assert_eq!(fa.position.map(f32::to_bits), fb.position.map(f32::to_bits));
    }
}
