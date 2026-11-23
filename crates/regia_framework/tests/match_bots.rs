// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/mode_bots.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 1086-1096): `mode_kind_mapping_covers_all_lobby_modes` is `bot_mode_covers_every_match_mode`
// over Regia's `MatchMode` (DO27's commander mode is `Custom`, which has no bot set). The other tests are new.

//! Plan row S-13 acceptance (ADR-8554): the mode-bot hooks read the S-14 match core. A capture-the-flag carrier
//! picked up by the rules is routed home by the bot orders and scores through the rules; a king-of-the-hill team is
//! sent to the hill and scores there; every match mode maps to a bot behavior set.

use regia_framework::rules::bots::{
    AiLevel, BotBody, BotHooks, CtfRole, KothIntent, ModeKind, ModeOrder, bot_mode, contacts_of,
};
use regia_framework::rules::mode::MatchMode;
use regia_framework::rules::objective::{
    Contestant, HillVolume, ObjectiveBoard, ObjectiveSettings, RulesEvent,
};

const STEP_M: f32 = 2.0;

fn unit(id: u32, team: u8, position: [f32; 3]) -> Contestant {
    Contestant {
        id,
        team,
        position,
        alive: true,
    }
}

fn hooks() -> BotHooks {
    BotHooks {
        level: AiLevel::Medium,
        world_seed: 7,
        free_for_all: false,
    }
}

/// Move `pos` up to `step` metres toward `goal` across XZ, landing on it when closer (the scripted bots' walk).
fn walk(pos: &mut [f32; 3], goal: [f32; 3], step: f32) {
    let dx = goal[0] - pos[0];
    let dz = goal[2] - pos[2];
    let d2 = dx * dx + dz * dz;
    if d2 <= step * step {
        pos[0] = goal[0];
        pos[2] = goal[2];
        return;
    }
    let inv = step / regia_detsim::math::sqrt(d2);
    pos[0] += dx * inv;
    pos[2] += dz * inv;
}

fn order_of(orders: &[(u32, ModeOrder)], id: u32) -> ModeOrder {
    orders
        .iter()
        .find(|(o, _)| *o == id)
        .map(|(_, o)| *o)
        .unwrap_or_else(|| panic!("no order for bot {id}: {orders:?}"))
}

/// One bot step: every living contestant's order from the hooks, then every bot walks toward its goal.
fn bot_step(board: &ObjectiveBoard, tick: u64, units: &mut [Contestant]) -> Vec<(u32, ModeOrder)> {
    let hooks = hooks();
    let bots: Vec<BotBody> = hooks.bodies(board, units, |_| 1.0);
    let orders = hooks.orders(board, tick, &bots, &contacts_of(&bots), &[]);
    for u in units.iter_mut() {
        if let Some((_, order)) = orders.iter().find(|(id, _)| *id == u.id) {
            walk(&mut u.position, order.goal, STEP_M);
        }
    }
    orders
}

#[test]
fn mode_bot_hooks_route_a_ctf_carrier() {
    let mut board = ObjectiveBoard::new(MatchMode::CaptureTheFlag, ObjectiveSettings::default());
    // Team 1's flag at (-100, 0, 0), team 2's at (100, 0, 0).
    assert_eq!(board.place_flags(&[1, 2], 100.0, 900), Ok(2));
    // Unit 1 stands on team 2's flag; unit 2 guards home; team 2 is far out on the flank.
    let mut units = vec![
        unit(1, 1, [100.0, 0.0, 0.0]),
        unit(2, 1, [-90.0, 0.0, 0.0]),
        unit(3, 2, [100.0, 0.0, 400.0]),
        unit(4, 2, [110.0, 0.0, 400.0]),
    ];
    board.ctf_tick(1, &units);
    assert_eq!(board.objective(901).and_then(|f| f.carrier_id), Some(1));

    let hooks = hooks();
    assert_eq!(hooks.mode(&board), Some(ModeKind::Ctf));
    let objectives = hooks.objectives(&board, 1);
    assert_eq!(objectives.flags.len(), 2);
    assert_eq!(
        objectives.flags[1].carrier,
        Some(1),
        "the bot view sees the rules' carrier"
    );
    let bots = hooks.bodies(&board, &units, |_| 1.0);
    assert_eq!(
        bots.iter().map(|b| (b.id, b.carrying_flag)).collect::<Vec<_>>(),
        [(1, true), (2, false), (3, false), (4, false)]
    );

    // The carrier (team 1's attacker) runs the flag to its own base; the defender holds the home flag; team 2's
    // defender chases its carried flag.
    let orders = hooks.orders(&board, 1, &bots, &contacts_of(&bots), &[]);
    let carrier = order_of(&orders, 1);
    assert_eq!(carrier.ctf, Some(CtfRole::Attack));
    assert_eq!(carrier.goal, [-100.0, 0.0, 0.0], "the carrier is routed home");
    assert_eq!(order_of(&orders, 2).ctf, Some(CtfRole::Defend));
    assert_eq!(order_of(&orders, 2).goal, [-100.0, 0.0, 0.0]);
    assert_eq!(order_of(&orders, 4).ctf, Some(CtfRole::Defend));
    assert_eq!(
        order_of(&orders, 4).goal,
        [100.0, 0.0, 0.0],
        "team 2 chases its carried flag"
    );

    // Let the bots play it out: the carrier reaches home within 100 steps of 2 m and the rules score it.
    let mut captured = None;
    for tick in 2..200u64 {
        bot_step(&board, tick, &mut units);
        board.ctf_tick(tick, &units);
        if let Some(RulesEvent::FlagCaptured {
            unit, team, score, ..
        }) = board
            .events
            .iter()
            .find(|e| matches!(e, RulesEvent::FlagCaptured { .. }))
        {
            captured = Some((tick, *unit, *team, *score));
            break;
        }
    }
    let (tick, unit, team, score) = captured.expect("the routed carrier captures");
    assert_eq!((unit, team, score), (1, 1, 1));
    assert!(
        tick <= 102,
        "the carrier took the straight route home (captured on tick {tick})"
    );
    assert_eq!(board.score(1), 1);
}

#[test]
fn mode_bot_hooks_send_a_koth_team_to_the_hill() {
    let settings = ObjectiveSettings {
        hill_accrual_ticks: 10,
        ..ObjectiveSettings::default()
    };
    let mut board = ObjectiveBoard::new(MatchMode::KingOfTheHill, settings);
    let hill = HillVolume {
        min: [-20.0, -1.0, -20.0],
        max: [20.0, 20.0, 20.0],
    };
    assert_eq!(board.install_hill_volume(50, hill), Ok(()));
    // Team 1 starts 60 m out, team 2 far away (still walking in).
    let mut units = vec![
        unit(1, 1, [60.0, 0.0, 0.0]),
        unit(2, 1, [70.0, 0.0, 10.0]),
        unit(3, 2, [-400.0, 0.0, 0.0]),
        unit(4, 2, [-410.0, 0.0, 10.0]),
    ];
    let hooks = hooks();
    assert_eq!(hooks.mode(&board), Some(ModeKind::Koth));
    let objectives = hooks.objectives(&board, 0);
    assert_eq!(objectives.hill.map(|h| h.center), Some([0.0, 9.5, 0.0]));

    let first = bot_step(&board, 0, &mut units);
    assert_eq!(order_of(&first, 1).koth, Some(KothIntent::Contest));
    for tick in 1..=60u64 {
        let orders = bot_step(&board, tick, &mut units);
        board.koth_tick(tick, &units);
        if tick == 60 {
            assert_eq!(order_of(&orders, 1).koth, Some(KothIntent::Hold));
            assert_eq!(order_of(&orders, 2).koth, Some(KothIntent::Hold));
        }
    }
    assert!(
        board.score(1) >= 3,
        "team 1 held the hill (score {})",
        board.score(1)
    );
    assert_eq!(board.score(2), 0, "team 2 never reached it");

    // A hurt holder rotates out and frees its slot.
    let bots = hooks.bodies(&board, &units, |c| if c.id == 1 { 0.05 } else { 1.0 });
    let orders = hooks.orders(&board, 61, &bots, &contacts_of(&bots), &[]);
    assert_eq!(order_of(&orders, 1).koth, Some(KothIntent::Rotate));
}

#[test]
fn bot_mode_covers_every_match_mode() {
    assert_eq!(bot_mode(MatchMode::FreeForAll, false), Some(ModeKind::Ffa));
    assert_eq!(bot_mode(MatchMode::FreeForAll, true), Some(ModeKind::Ffa));
    assert_eq!(bot_mode(MatchMode::TeamDeathmatch, true), Some(ModeKind::Ffa));
    assert_eq!(bot_mode(MatchMode::TeamDeathmatch, false), Some(ModeKind::Arena));
    assert_eq!(bot_mode(MatchMode::KingOfTheHill, false), Some(ModeKind::Koth));
    assert_eq!(bot_mode(MatchMode::CaptureTheFlag, false), Some(ModeKind::Ctf));
    assert_eq!(
        bot_mode(MatchMode::Custom, false),
        None,
        "a game's own mode keeps its own bots"
    );
    assert_eq!(
        bot_mode(MatchMode::Domination, false),
        None,
        "capture-zone domination has no stock bot set"
    );
    for mode in MatchMode::ALL {
        let board = ObjectiveBoard::new(mode, ObjectiveSettings::default());
        assert_eq!(hooks().mode(&board), bot_mode(mode, false));
    }
}

#[test]
fn dead_and_neutral_contestants_get_no_bot_order() {
    let board = ObjectiveBoard::new(MatchMode::TeamDeathmatch, ObjectiveSettings::default());
    let mut dead = unit(2, 1, [5.0, 0.0, 0.0]);
    dead.alive = false;
    let units = [
        unit(3, 2, [30.0, 0.0, 0.0]),
        dead,
        unit(1, 1, [0.0, 0.0, 0.0]),
        unit(9, 0, [1.0, 0.0, 0.0]),
    ];
    let hooks = hooks();
    let bots = hooks.bodies(&board, &units, |_| 1.0);
    assert_eq!(
        bots.iter().map(|b| b.id).collect::<Vec<_>>(),
        [1, 3],
        "living fighters, in id order"
    );
    let orders = hooks.orders(&board, 0, &bots, &contacts_of(&bots), &[]);
    assert_eq!(orders.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [1, 3]);
    // Arena: unit 1 hunts unit 3, flanking it by id parity.
    assert_eq!(order_of(&orders, 1).focus_enemy, Some(3));
}
