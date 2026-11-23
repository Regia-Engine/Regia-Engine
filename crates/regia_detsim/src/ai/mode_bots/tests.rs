// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/mode_bots.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (the `tests` module, lines 865-1097, plan row S-13, ADR-8554). Seven of DO27's eight tests keep their
// scenario, numbers and assertions; `mode_kind_mapping_covers_all_lobby_modes` moved with the mapping to
// `regia_framework::rules::bots` (`bot_mode_covers_every_match_mode` there). The private `dist_xz` reads are DO27's;
// `unwrap()` is `expect()` (the crate's clippy config denies `unwrap` in tests too).
// Not in DO27: `mode_orders_match_the_scripted_sim_order_phase`, `mode_orders_skip_dead_bots_and_sort_by_id`,
// `koth_with_no_hill_holds_in_place` and `scripted_sim_digests_match_the_pins`.

use super::*;

fn hill_objectives() -> Objectives {
    Objectives {
        hill: Some(HillView {
            center: [0.0, 0.0, 0.0],
            min: [-20.0, -1.0, -20.0],
            max: [20.0, 20.0, 20.0],
        }),
        flags: Vec::new(),
        tick: 0,
        world_seed: 7,
    }
}

fn ctf_objectives() -> Objectives {
    Objectives {
        hill: None,
        flags: vec![
            FlagView {
                team: 1,
                base: [-100.0, 0.0, 0.0],
                pos: [-100.0, 0.0, 0.0],
                carrier: None,
                at_base: true,
            },
            FlagView {
                team: 2,
                base: [100.0, 0.0, 0.0],
                pos: [100.0, 0.0, 0.0],
                carrier: None,
                at_base: true,
            },
        ],
        tick: 0,
        world_seed: 7,
    }
}

fn bot(id: u32, team: u8, pos: [f32; 3], hull: f32) -> BotBody {
    BotBody {
        id,
        team,
        pos,
        hull_pct: hull,
        level: AiLevel::Medium,
        carrying_flag: false,
    }
}

#[test]
fn koth_healthy_bot_contests_then_holds() {
    let obj = hill_objectives();
    let outside = bot(1, 1, [60.0, 0.0, 0.0], 1.0);
    let order = koth_order(&outside, &obj, &[], 0, 2);
    assert_eq!(order.koth, Some(KothIntent::Contest));
    assert!(obj.hill.expect("the hill").contains(order.goal));
    let mut sim = ScriptedSim::two_v_two(
        ModeKind::Koth,
        [[60.0, 0.0, 0.0], [70.0, 0.0, 10.0]],
        [[-200.0, 0.0, 0.0], [-210.0, 0.0, 10.0]],
        obj.clone(),
        AiLevel::Medium,
        7,
    );
    sim.run(40);
    // Both team-1 bots reached the hill; team 2 is still walking in, so
    // nobody has taken damage yet and both hold or contest.
    for id in [1u32, 2] {
        let b = sim.bots.iter().find(|b| b.body.id == id).expect("the bot");
        assert!(
            dist_xz(b.body.pos, [0.0, 0.0, 0.0]) < 20.0,
            "bot {id} at {:?}",
            b.body.pos
        );
    }
    let intents: Vec<KothIntent> = [1u32, 2]
        .iter()
        .map(|id| {
            let b = sim.bots.iter().find(|b| b.body.id == *id).expect("the bot");
            let (rank, count) = (if *id == 1 { 0 } else { 1 }, 2);
            koth_order(&b.body, &obj, &[], rank, count)
                .koth
                .expect("a KOTH intent")
        })
        .collect();
    assert!(
        intents
            .iter()
            .all(|i| *i == KothIntent::Hold || *i == KothIntent::Contest)
    );
}

#[test]
fn koth_hurt_bot_rotates_out() {
    let obj = hill_objectives();
    let hurt = bot(1, 1, [5.0, 0.0, 0.0], 0.05);
    let order = koth_order(&hurt, &obj, &[], 0, 2);
    assert_eq!(order.koth, Some(KothIntent::Rotate));
    // Outward: further from the center than it started.
    assert!(dist_xz(order.goal, [0.0, 0.0, 0.0]) > dist_xz(hurt.pos, [0.0, 0.0, 0.0]));
    // Healthy teammate still holds.
    let healthy = bot(2, 1, [5.0, 0.0, 5.0], 1.0);
    assert_eq!(koth_order(&healthy, &obj, &[], 1, 2).koth, Some(KothIntent::Hold));
}

#[test]
fn ctf_roles_split_attack_defend_in_2v2() {
    assert_eq!(
        assign_ctf_roles(&[1, 2]),
        vec![(1, CtfRole::Attack), (2, CtfRole::Defend)]
    );
    assert_eq!(
        assign_ctf_roles(&[1, 2, 3]),
        vec![(1, CtfRole::Attack), (2, CtfRole::Defend), (3, CtfRole::Escort)]
    );
    // Attacker walks at the enemy flag; defender holds the own flag.
    let obj = ctf_objectives();
    let attacker = bot(1, 1, [0.0, 0.0, 0.0], 1.0);
    let defender = bot(2, 1, [-90.0, 0.0, 0.0], 1.0);
    let team = vec![attacker, defender];
    let atk = ctf_order(&attacker, CtfRole::Attack, &obj, &team, &[]);
    assert_eq!(atk.goal, [100.0, 0.0, 0.0]);
    let dfn = ctf_order(&defender, CtfRole::Defend, &obj, &team, &[]);
    assert_eq!(dfn.goal, [-100.0, 0.0, 0.0]);
    // Carrier runs home.
    let mut carrier = attacker;
    carrier.carrying_flag = true;
    carrier.pos = [100.0, 0.0, 0.0];
    let run = ctf_order(&carrier, CtfRole::Attack, &obj, &team, &[]);
    assert_eq!(run.goal, [-100.0, 0.0, 0.0]);
}

#[test]
fn ctf_escort_shadows_carrier_and_defender_returns_drop() {
    let mut obj = ctf_objectives();
    // Own flag dropped at -40: defender goes to the drop, not the base.
    obj.flags[0].at_base = false;
    obj.flags[0].pos = [-40.0, 0.0, 0.0];
    let defender = bot(2, 1, [-90.0, 0.0, 0.0], 1.0);
    let team = vec![bot(1, 1, [100.0, 0.0, 0.0], 1.0), defender];
    let dfn = ctf_order(&defender, CtfRole::Defend, &obj, &team, &[]);
    assert_eq!(dfn.goal, [-40.0, 0.0, 0.0]);
    // Carrier at the enemy base: escort stands off on the home side.
    let mut carrier_team = team.clone();
    carrier_team[0].carrying_flag = true;
    let escort = bot(3, 1, [80.0, 0.0, 20.0], 1.0);
    carrier_team.push(escort);
    let esc = ctf_order(&escort, CtfRole::Escort, &obj, &carrier_team, &[]);
    assert!(
        esc.goal[0] < 100.0,
        "escort stays home-side of carrier, got {:?}",
        esc.goal
    );
}

#[test]
fn ffa_hunts_nearest_with_spacing_and_runs_pickups() {
    let hunter = bot(1, 1, [0.0, 0.0, 0.0], 1.0);
    let mate = bot(2, 1, [1.0, 0.0, 0.0], 1.0);
    let enemies = vec![
        EnemySight {
            id: 9,
            team: 2,
            pos: [50.0, 0.0, 0.0],
        },
        EnemySight {
            id: 10,
            team: 2,
            pos: [30.0, 0.0, 0.0],
        },
    ];
    let o1 = ffa_order(&hunter, &[mate], &enemies, &[], true);
    assert_eq!(o1.focus_enemy, Some(10));
    // Paired hunters get different goals (lateral spacing by id parity).
    let o2 = ffa_order(&mate, &[hunter], &enemies, &[], true);
    assert_ne!(o1.goal, o2.goal);
    // Nothing in range: run the pickup inside the search radius.
    let far: Vec<EnemySight> = Vec::new();
    let pickups = vec![Pickup {
        pos: [20.0, 0.0, 0.0],
    }];
    let o3 = ffa_order(&hunter, &[], &far, &pickups, true);
    assert_eq!(o3.goal, [20.0, 0.0, 0.0]);
    assert_eq!(o3.focus_enemy, None);
}

fn sim_pair(mode: ModeKind, seed: u64) -> (ScriptedSim, ScriptedSim) {
    let obj = match mode {
        ModeKind::Koth => hill_objectives(),
        ModeKind::Ctf => ctf_objectives(),
        _ => Objectives::empty(0, 7),
    };
    let make = || {
        ScriptedSim::two_v_two(
            mode,
            [[-40.0, 0.0, 0.0], [-50.0, 0.0, 10.0]],
            [[40.0, 0.0, 0.0], [50.0, 0.0, -10.0]],
            obj.clone(),
            AiLevel::Medium,
            seed,
        )
    };
    (make(), make())
}

#[test]
fn sim_is_deterministic_per_seed_and_mode() {
    for mode in [ModeKind::Ffa, ModeKind::Arena, ModeKind::Koth, ModeKind::Ctf] {
        let (mut a, mut b) = sim_pair(mode, 99);
        a.run(60);
        b.run(60);
        assert_eq!(a.digest(), b.digest(), "mode {mode:?} drifted");
    }
    // Different seeds diverge (combat rolls differ).
    let mut a = ScriptedSim::two_v_two(
        ModeKind::Arena,
        [[-10.0, 0.0, 0.0], [-12.0, 0.0, 2.0]],
        [[10.0, 0.0, 0.0], [12.0, 0.0, -2.0]],
        Objectives::empty(0, 1),
        AiLevel::Hard,
        1,
    );
    let mut b = ScriptedSim::two_v_two(
        ModeKind::Arena,
        [[-10.0, 0.0, 0.0], [-12.0, 0.0, 2.0]],
        [[10.0, 0.0, 0.0], [12.0, 0.0, -2.0]],
        Objectives::empty(0, 1),
        AiLevel::Hard,
        2,
    );
    a.run(60);
    b.run(60);
    assert_ne!(a.digest(), b.digest());
}

#[test]
fn difficulty_knobs_only_touch_decisions() {
    // Harder levels react faster, see further, shoot straighter, rotate
    // later, and search further for pickups. Hull and damage are flat.
    assert!(
        reaction_ticks(AiLevel::Hard) < reaction_ticks(AiLevel::Medium)
            && reaction_ticks(AiLevel::Medium) < reaction_ticks(AiLevel::Easy)
    );
    assert!(acquire_range_m(AiLevel::Hard) > acquire_range_m(AiLevel::Easy));
    assert!(aim_for(AiLevel::Hard).spread_mrad < aim_for(AiLevel::Easy).spread_mrad);
    assert!(retreat_hull_pct(AiLevel::Hard) > retreat_hull_pct(AiLevel::Easy));
    assert!(pickup_radius_m(AiLevel::Hard) > pickup_radius_m(AiLevel::Easy));
    assert_eq!(SIM_DAMAGE_PER_STEP, 5.0);
}

// -- Regia additions ------------------------------------------------------

#[test]
fn mode_orders_match_the_scripted_sim_order_phase() {
    // `mode_orders` over every living bot as contacts gives the same orders the scripted sim moves its bots by.
    for mode in [ModeKind::Ffa, ModeKind::Arena, ModeKind::Koth, ModeKind::Ctf] {
        let (mut sim, _) = sim_pair(mode, 5);
        for _ in 0..30 {
            let bots: Vec<BotBody> = sim.bots.iter().map(|b| b.body).collect();
            let contacts: Vec<EnemySight> = bots
                .iter()
                .filter(|b| b.hull_pct > 0.0)
                .map(|b| EnemySight {
                    id: b.id,
                    team: b.team,
                    pos: b.pos,
                })
                .collect();
            let orders = mode_orders(mode, &bots, &contacts, &sim.objectives, &[]);
            sim.step();
            for (id, order) in orders {
                let b = sim.bots.iter().find(|b| b.body.id == id).expect("the bot");
                assert_eq!(b.goal, order.goal, "{mode:?} bot {id} goal");
                assert_eq!(b.focus, order.focus_enemy, "{mode:?} bot {id} focus");
            }
        }
    }
}

#[test]
fn mode_orders_skip_dead_bots_and_sort_by_id() {
    let bots = [
        bot(4, 2, [10.0, 0.0, 0.0], 1.0),
        bot(2, 1, [0.0, 0.0, 0.0], 0.0),
        bot(1, 1, [-5.0, 0.0, 0.0], 1.0),
    ];
    let orders = mode_orders(ModeKind::Arena, &bots, &[], &Objectives::empty(0, 1), &[]);
    assert_eq!(orders.iter().map(|(id, _)| *id).collect::<Vec<_>>(), [1, 4]);
}

#[test]
fn koth_with_no_hill_holds_in_place() {
    let b = bot(1, 1, [3.0, 1.0, -4.0], 1.0);
    let order = koth_order(&b, &Objectives::empty(0, 1), &[], 0, 1);
    assert_eq!(order.goal, b.pos);
    assert_eq!(order.koth, None);
}

/// The scripted sim's digest after 120 steps of each mode, seed 99 (x86-64 and aarch64 must agree: the orders use
/// `regia_detsim::math` and `det_*` only).
const DIGEST_PINS: [u64; 4] = [
    0xbe7e_5395_32ec_d3b5,
    0x7b07_2d71_981c_ba79,
    0x0a69_02e0_fc9e_14eb,
    0x6aba_8c2c_8194_c2bb,
];

#[test]
fn scripted_sim_digests_match_the_pins() {
    let digests: Vec<u64> = [ModeKind::Ffa, ModeKind::Arena, ModeKind::Koth, ModeKind::Ctf]
        .into_iter()
        .map(|mode| {
            let (mut sim, _) = sim_pair(mode, 99);
            sim.run(120);
            sim.digest()
        })
        .collect();
    println!(
        "regia-mode-bot-pins {}",
        digests
            .iter()
            .map(|d| format!("{d:#018x}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    assert_eq!(digests, DIGEST_PINS);
}

// Review proofs (lane det-ai-squads-modebots-review). These fail on c79c9505b.
// Do not weaken them: a fix makes the assertions hold.

/// Two pickups at the same XZ distance must not change the goal when the slice is reversed.
/// `mode_orders` (and therefore `BotHooks::orders`) never sorts pickups.
#[test]
fn review_proof_equal_distance_pickups_ignore_slice_order() {
    let hunter = bot(1, 1, [0.0, 0.0, 0.0], 1.0);
    let east = Pickup {
        pos: [10.0, 0.0, 0.0],
    };
    let west = Pickup {
        pos: [-10.0, 0.0, 0.0],
    };
    let objectives = Objectives::empty(0, 1);
    let east_first = mode_orders(ModeKind::Ffa, &[hunter], &[], &objectives, &[east, west]);
    let west_first = mode_orders(ModeKind::Ffa, &[hunter], &[], &objectives, &[west, east]);
    assert_eq!(
        east_first[0].1.goal, west_first[0].1.goal,
        "reversing two equidistant pickups changed the goal: {:?} vs {:?}",
        east_first[0].1.goal, west_first[0].1.goal
    );
}

/// The 1 cm hostile tie band must pick the same focus for one set of contacts, in any slice order.
/// `mode_orders` sorts by id first; `ffa_order` does not, and the band is not a total order.
#[test]
fn review_proof_hostile_focus_ignores_contact_order() {
    let hunter = bot(7, 1, [0.0, 0.0, 0.0], 1.0);
    let contacts = [
        EnemySight {
            id: 10,
            team: 2,
            pos: [5.0, 0.0, 0.0],
        },
        EnemySight {
            id: 1,
            team: 2,
            pos: [5.009, 0.0, 0.0],
        },
        EnemySight {
            id: 20,
            team: 2,
            pos: [4.995, 0.0, 0.0],
        },
    ];
    let forward = ffa_order(&hunter, &[], &contacts, &[], true);
    let mut reversed = contacts;
    reversed.reverse();
    let backward = ffa_order(&hunter, &[], &reversed, &[], true);
    assert_eq!(
        forward.focus_enemy, backward.focus_enemy,
        "the same three contacts focused {:?} in slice order and {:?} reversed",
        forward.focus_enemy, backward.focus_enemy
    );
}

// ADR-8655 edge cases beyond the review proofs.

/// Every permutation of contacts that sit inside one another's 1 cm band focuses the same contact, through every
/// order that reads `nearest_hostile` (FFA, KOTH, CTF and `mode_order`), and the id-ordered pick is DO27's scan.
#[test]
fn hostile_focus_is_the_same_for_every_contact_permutation() {
    let contacts = [
        EnemySight {
            id: 1,
            team: 2,
            pos: [5.009, 0.0, 0.0],
        },
        EnemySight {
            id: 10,
            team: 2,
            pos: [5.0, 0.0, 0.0],
        },
        EnemySight {
            id: 20,
            team: 2,
            pos: [4.995, 0.0, 0.0],
        },
        EnemySight {
            id: 30,
            team: 2,
            pos: [0.0, 0.0, 4.989],
        },
    ];
    let hunter = bot(7, 1, [0.0, 0.0, 0.0], 1.0);
    let koth = hill_objectives();
    let ctf = ctf_objectives();
    let focus_of = |slice: &[EnemySight]| {
        [
            ffa_order(&hunter, &[], slice, &[], true).focus_enemy,
            ffa_order(&hunter, &[], slice, &[], false).focus_enemy,
            koth_order(&hunter, &koth, slice, 0, 1).focus_enemy,
            ctf_order(&hunter, CtfRole::Defend, &ctf, &[hunter], slice).focus_enemy,
            mode_order(
                ModeKind::Arena,
                &hunter,
                &[],
                slice,
                &Objectives::empty(0, 1),
                &[],
            )
            .focus_enemy,
        ]
    };
    // Id order: 1 (5.009) is first; 10 (5.0) is not 1 cm nearer and has a higher id; 20 (4.995) is 1.4 cm nearer
    // than 1; 30 (4.989) is within 1 cm of 20 and has a higher id. DO27's scan keeps 20.
    let id_ordered = focus_of(&contacts);
    assert_eq!(id_ordered, [Some(20); 5]);
    let mut perm = contacts;
    for i in 0..24usize {
        // Permutation `i` of the four, by the factorial number system.
        let mut pool: Vec<EnemySight> = contacts.to_vec();
        let mut k = i;
        for (slot, radix) in [6usize, 2, 1, 1].iter().enumerate() {
            perm[slot] = pool.remove(k / radix);
            k %= radix;
        }
        assert_eq!(focus_of(&perm), id_ordered, "permutation {i}: {perm:?}");
    }
}

/// Two sightings of one id (a stale and a fresh position): the hunt goal follows the canonical one, not the
/// first in the slice.
#[test]
fn duplicate_contact_ids_hunt_the_same_sighting_in_any_order() {
    let hunter = bot(2, 1, [0.0, 0.0, 0.0], 1.0);
    let stale = EnemySight {
        id: 5,
        team: 2,
        pos: [0.0, 0.0, 20.0],
    };
    let fresh = EnemySight {
        id: 5,
        team: 2,
        pos: [0.0, 0.0, -20.0],
    };
    let a = ffa_order(&hunter, &[], &[stale, fresh], &[], true);
    let b = ffa_order(&hunter, &[], &[fresh, stale], &[], true);
    assert_eq!(a.focus_enemy, Some(5));
    assert_eq!(a.goal, b.goal);
}

/// The FFA ally keep-out sums its pushes in canonical order: a reversed ally slice gives the same goal bits.
#[test]
fn ffa_ally_keepout_is_the_same_in_any_ally_order() {
    let hunter = bot(2, 1, [0.0, 0.0, 0.0], 1.0);
    let allies = [
        bot(3, 1, [0.3, 0.0, 0.7], 1.0),
        bot(5, 1, [-1.1, 0.0, 0.2], 1.0),
        bot(9, 1, [0.9, 0.0, -1.3], 1.0),
    ];
    let target = [EnemySight {
        id: 40,
        team: 2,
        pos: [30.0, 0.0, 10.0],
    }];
    let forward = ffa_order(&hunter, &allies, &target, &[], false);
    let mut reversed = allies;
    reversed.reverse();
    let backward = ffa_order(&hunter, &reversed, &target, &[], false);
    assert_eq!(forward.goal.map(f32::to_bits), backward.goal.map(f32::to_bits));
}

/// Pickups: equal distances break on the position for every rotation of the slice, a nearer pickup still wins,
/// and the lockstep hook path (`mode_orders`) agrees with the direct `ffa_order`.
#[test]
fn equidistant_pickups_pick_one_for_every_slice_rotation() {
    let hunter = bot(1, 1, [0.0, 0.0, 0.0], 1.0);
    let ring = [
        Pickup {
            pos: [0.0, 0.0, 10.0],
        },
        Pickup {
            pos: [10.0, 0.0, 0.0],
        },
        Pickup {
            pos: [0.0, 0.0, -10.0],
        },
        Pickup {
            pos: [-10.0, 0.0, 0.0],
        },
    ];
    let mut goals = Vec::new();
    for rot in 0..ring.len() {
        let mut order = ring;
        order.rotate_left(rot);
        goals.push(ffa_order(&hunter, &[], &[], &order, true).goal);
        let hooked = mode_orders(ModeKind::Ffa, &[hunter], &[], &Objectives::empty(0, 1), &order);
        goals.push(hooked[0].1.goal);
    }
    assert_eq!(goals[0], [-10.0, 0.0, 0.0], "lowest x of the tied ring");
    assert!(goals.iter().all(|g| *g == goals[0]), "{goals:?}");
    let near = Pickup { pos: [0.0, 0.0, 9.0] };
    assert_eq!(
        ffa_order(&hunter, &[], &[], &[ring[3], near, ring[1]], true).goal,
        near.pos
    );
}
