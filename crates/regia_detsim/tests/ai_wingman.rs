// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/wingman.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 1-674, plan row S-13, ADR-8554). Every DO27 test keeps its scenario, numbers and assertions;
// named changes:
// - the unit view is S-12's `AiUnitView` (`AiUnitView::new` plus the fields DO27's helper set): the hull and weapon
//   stems are dropped, a structure is `is_unit: false` with its Re-up role (DO27's repair building is
//   `ReupRole::Repair`), and DO27's cannon table entry is inlined (direct, 120 m/s, 0-150 m);
// - the hover view and the hover step use the neutral names of ADR-8012 A-8, and DO27's default hover parameters (a
//   baseline-game hull, P4) are the engine-authored default hull (`HoverParams::default()`, ADR-8164);
// - line of sight is an S-10 `LineOfSight` (`OpenField` for DO27's `None` terrain);
// - std float methods (`sqrt`, `sin`, `cos`, `max`, `min`) go through `regia_detsim::math` and `regia_detsim::float`
//   (the lockstep clippy config lints tests);
// - `step_real_motor` takes the hover parameters, board and team cache as one `Rig` (clippy's argument limit), the
//   two inline board entries are one `known` helper, and the debug print behind DO27's environment switch is dropped;
// - DO27's game identifiers (hull and building names) are dropped, and `unwrap()` is `expect()` (the crate's clippy
//   config denies `unwrap` in tests too).
// Not in DO27: `wingman_bits_match_the_pinned_hashes` (the cross-arch pins).

//! Wingman acceptance (plan row S-13): a headless, deterministic scenario suite for the public wingman API, the three
//! acceptance gates as plain Rust tests (no game, no GPU, no netcode):
//!
//! - every command resolves to the correct behavior within one second (resolution is synchronous, so the onset is
//!   tick 0, and the real motor shows the unit closing on its goal within 60 ticks);
//! - formation spacing stays within ±20 % of the authored gap, standing and while the leader moves;
//! - no friendly fire into the player's line (the friendly cone is blocked).

use regia_detsim::ai::{
    AiLevel, AiRng, AiUnitView, AiWorld, AmmoSlotView, AmmoView, CalloutId, CoordState, FireGate, HoverView,
    KnownEnemy, OpenField, OptionId, ReupRole, SightSource, Sighting, SightingsBoard, Stance, ThreatEstimate,
    WeaponRole, WingmanCommand, WingmanFormation, WingmanPlan, assign_formation_slots, drive, fire_blocked,
    formation_slots, plan, repair_goal, report, route, slot_clears_leader_lane, spacing_error, wingman_slot,
};
use regia_detsim::float::{det_max, det_min};
use regia_detsim::hover::{HoverParams, HoverState, step};
use regia_detsim::math;

const DT: f32 = 1.0 / 60.0;

fn view(id: u32, team: u8, pos: [f32; 3], yaw: f32) -> AiUnitView {
    let mut u = AiUnitView::new(id, team, pos);
    u.yaw = yaw;
    u.hover = HoverView {
        top_speed: 40.0,
        yaw_rate_at_speed: 1.5,
        brake_accel: 20.0,
        turn_bleed: 0.3,
    };
    u.level = AiLevel::Medium;
    u.radius = 3.9;
    u
}

/// DO27's cannon table entry: direct fire, 120 m/s, 0-150 m.
fn cannon(ammo: Option<(u32, u32)>) -> AmmoSlotView {
    AmmoSlotView {
        role: WeaponRole::Direct,
        projectile_speed: 120.0,
        ammo,
        band: [0.0, 150.0],
    }
}

fn world(units: Vec<AiUnitView>) -> AiWorld {
    AiWorld {
        tick: 0,
        world_seed: 0xD027,
        units,
    }
}

fn dist(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[2] - b[2];
    math::sqrt(dx * dx + dz * dz)
}

// -- acceptance 1: formation spacing within ±20% ---------------------------

#[test]
fn formation_spacing_stays_within_20_percent() {
    for n in [2usize, 3, 5, 8] {
        let line = formation_slots(WingmanFormation::Line, n, [0.0, 0.0, 0.0], 0.0, false);
        for w in line.windows(2) {
            let err = spacing_error(w[0], w[1], 12.0);
            assert!(err.abs() <= 0.20, "line spacing err {err}");
        }
        let col = formation_slots(WingmanFormation::Column, n, [0.0, 0.0, 0.0], 0.0, false);
        for w in col.windows(2) {
            let err = spacing_error(w[0], w[1], 16.0);
            assert!(err.abs() <= 0.20, "column spacing err {err}");
        }
    }
}

// -- acceptance 2: slots keep out of the player's line of fire -------------

#[test]
fn every_slot_clears_the_leader_lane() {
    for formation in [
        WingmanFormation::Line,
        WingmanFormation::Wedge,
        WingmanFormation::Column,
        WingmanFormation::Loose,
    ] {
        for (i, slot) in formation_slots(formation, 6, [0.0, 0.0, 0.0], 0.0, false)
            .iter()
            .enumerate()
        {
            assert!(
                slot_clears_leader_lane(*slot, [0.0, 0.0, 0.0], 0.0),
                "{formation:?} {i}"
            );
            assert!(slot[2] <= 0.0, "{formation:?} slot {i} is ahead of the leader");
        }
    }
}

// -- acceptance 3: no friendly fire into the player's line -----------------

#[test]
fn friendly_fire_gate_blocks_the_player_in_the_cone() {
    let wingman = [0.0, 0.0, 0.0];
    let enemy = [0.0, 0.0, 100.0];
    assert!(
        fire_blocked(wingman, enemy, &[[0.0, 0.0, 20.0]]),
        "player in cone → blocked"
    );
    assert!(!fire_blocked(wingman, enemy, &[]), "clear line → free");
    assert!(
        !fire_blocked(wingman, enemy, &[[40.0, 0.0, 20.0]]),
        "wide of the cone → free"
    );
}

// -- every command resolves correctly, onset within 1 s ---------------------

#[test]
fn every_command_resolves_to_the_correct_behaviour() {
    let w = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    let leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let enemy = view(50, 2, [100.0, 0.0, 0.0], 0.0);
    let pilot = view(60, 1, [40.0, 0.0, 0.0], 0.0);
    let world = world(vec![w.clone(), leader, enemy, pilot]);

    let cases: Vec<(WingmanCommand, Option<OptionId>, FireGate)> = vec![
        (WingmanCommand::AttackAtWill, None, FireGate::WeaponsFree),
        (
            WingmanCommand::Escort { leader: 1 },
            Some(OptionId::Escort),
            FireGate::WeaponsFree,
        ),
        (
            WingmanCommand::Hold {
                point: [1.0, 0.0, 2.0],
            },
            Some(OptionId::Hold),
            FireGate::WeaponsFree,
        ),
        (
            WingmanCommand::DefendPoint {
                point: [1.0, 0.0, 2.0],
            },
            Some(OptionId::Defend),
            FireGate::WeaponsFree,
        ),
        (
            WingmanCommand::CollectScrap,
            Some(OptionId::CollectScrap),
            FireGate::WeaponsFree,
        ),
        (
            WingmanCommand::Regroup { leader: 1 },
            Some(OptionId::Regroup),
            FireGate::WeaponsFree,
        ),
        (
            WingmanCommand::Retreat,
            Some(OptionId::Retreat),
            FireGate::WeaponsFree,
        ),
        (
            WingmanCommand::PickupPilot { pilot: 60 },
            None,
            FireGate::CeaseFire,
        ),
        (WingmanCommand::CeaseFire, None, FireGate::CeaseFire),
        (WingmanCommand::WeaponsFree, None, FireGate::WeaponsFree),
        (
            WingmanCommand::Stance(Stance::Aggressive),
            Some(OptionId::Push),
            FireGate::WeaponsFree,
        ),
        (
            WingmanCommand::Stance(Stance::Defensive),
            Some(OptionId::Hold),
            FireGate::WeaponsFree,
        ),
        (
            WingmanCommand::Stance(Stance::HoldFire),
            None,
            FireGate::CeaseFire,
        ),
    ];
    for (cmd, bias, fire) in cases {
        let p = plan(&w, &cmd, &world, 0);
        assert_eq!(p.option_bias, bias, "{cmd:?}");
        assert_eq!(p.fire, fire, "{cmd:?}");
    }
}

#[test]
fn attack_my_target_steers_at_the_target_and_fires_only_at_it() {
    let w = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    let enemy = view(50, 2, [100.0, 0.0, 0.0], 0.0);
    let world = world(vec![w.clone(), enemy.clone()]);
    let p = plan(&w, &WingmanCommand::AttackTarget { target: 50 }, &world, 0);
    assert_eq!(p.steer.expect("a steer goal").point, [100.0, 0.0, 0.0]);
    assert_eq!(p.fire, FireGate::TargetOnly(50));
    assert_eq!(p.callout, Some(CalloutId::Engaging));
}

// -- the unit is moving toward its goal within the first second -------------
// (real motor: hover inertia through the hover step, not a placeholder interpolate)

/// The hover parameters, sighting board and team cache one wingman steps under.
struct Rig<'a> {
    params: &'a HoverParams,
    board: &'a SightingsBoard,
    coord: &'a CoordState,
}

/// Step one wingman through the real motor and the hover step for one tick, moving it. The leader's live pose is
/// read from `world`.
fn step_real_motor(
    unit: &mut AiUnitView,
    hs: &mut HoverState,
    rig: &Rig<'_>,
    cmd: &WingmanCommand,
    slot: usize,
    world: &AiWorld,
    tick: u64,
) {
    let plan = plan(unit, cmd, world, slot);
    let mut rng = AiRng::new(AiRng::mix(world.world_seed ^ tick, unit.id as u64));
    let out = drive(unit, &plan, world, rig.board, rig.coord, OpenField, &mut rng);
    if let Some(s) = out.steer {
        step(rig.params, hs, &[s.throttle, s.strafe, s.steer], 0.0, DT);
    }
    unit.pos[0] += hs.velocity[0] / 60.0;
    unit.pos[2] += hs.velocity[2] / 60.0;
    unit.yaw = hs.yaw;
    unit.vel = hs.velocity;
}

#[test]
fn wingman_moves_toward_its_goal_within_one_second() {
    let w = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    let enemy = view(50, 2, [100.0, 0.0, 0.0], 0.0);
    let world = world(vec![w.clone(), enemy.clone()]);
    let board = SightingsBoard::new(3, 400.0);
    let coord = CoordState::default();
    let params = HoverParams::default();
    let rig = Rig {
        params: &params,
        board: &board,
        coord: &coord,
    };
    let mut unit = w;
    let mut hs = HoverState::default();
    let cmd = WingmanCommand::AttackTarget { target: 50 };
    let start = unit.pos;
    for tick in 0..60 {
        step_real_motor(&mut unit, &mut hs, &rig, &cmd, 0, &world, tick);
    }
    assert!(dist(unit.pos, start) > 0.0, "the real motor moves the wingman");
    assert!(
        unit.pos[0] > 0.0,
        "moving toward the +X target, got {:?}",
        unit.pos
    );
}

// -- group routing is deterministic and order-independent ------------------

#[test]
fn group_routing_is_deterministic_and_order_independent() {
    let a = assign_formation_slots(&[9, 3, 7], WingmanFormation::Line, [0.0, 0.0, 0.0], 0.0, false);
    let b = assign_formation_slots(&[3, 7, 9], WingmanFormation::Line, [0.0, 0.0, 0.0], 0.0, false);
    assert_eq!(a, b);
    // A Line spreads laterally by sorted-id slot order, so id 3 sits left of
    // id 7 sits left of id 9 (all at the same distance behind the leader).
    assert!(a[&3][0] < a[&7][0] && a[&7][0] < a[&9][0], "sorted-id slot order");

    let leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let u2 = view(2, 1, [20.0, 0.0, 0.0], 0.0);
    let u3 = view(3, 1, [-20.0, 0.0, 0.0], 0.0);
    let world = world(vec![leader, u2.clone(), u3.clone()]);
    let plans = route(&WingmanCommand::Regroup { leader: 1 }, &[3, 2], &world);
    assert_eq!(plans.len(), 2);
    assert_ne!(
        plans[&2].steer.expect("a steer goal").point,
        plans[&3].steer.expect("a steer goal").point
    );
}

// -- low-hull / low-ammo reporting and the repair run ----------------------

#[test]
fn low_hull_reports_and_routes_to_the_nearest_repair_point() {
    let mut u = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    assert_eq!(report(&u), None, "healthy unit reports nothing");
    u.hull_pct = 0.10;
    assert_eq!(report(&u), Some(CalloutId::HullCritical));
    let near = [10.0, 0.0, 0.0];
    let far = [90.0, 0.0, 0.0];
    assert_eq!(repair_goal(&u, &[near, far]).expect("a repair goal").point, near);

    // Low ammo is reported only once the hull is no longer critical.
    u.hull_pct = 1.0;
    u.ammo.slots = vec![Some(cannon(Some((2, 100))))];
    assert_eq!(report(&u), Some(CalloutId::LowAmmo));
}

#[test]
fn follow_plan_steers_behind_the_leader() {
    let leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let w = view(2, 1, [30.0, 0.0, 0.0], 0.0);
    let world = world(vec![leader, w.clone()]);
    let p = plan(
        &w,
        &WingmanCommand::Follow {
            leader: 1,
            formation: WingmanFormation::Line,
            close: false,
        },
        &world,
        0,
    );
    assert!(
        p.steer.expect("a steer goal").point[2] < 0.0,
        "slot behind the leader"
    );
    assert_eq!(p.option_bias, Some(OptionId::Regroup));
}

#[test]
fn wedge_slot_stays_at_the_lane_edge() {
    for i in 0..6 {
        let slot = wingman_slot(WingmanFormation::Wedge, i, [0.0, 0.0, 0.0], 0.0, false);
        assert!(slot[2] < 0.0, "behind");
        assert!(
            slot[0].abs() >= 14.0,
            "wingman {} at the lane edge, x={}",
            i,
            slot[0]
        );
    }
}

// ---------------------------------------------------------------------------
// Real motor / combat hook: the acceptance numbers measured against the real motor.
// ---------------------------------------------------------------------------

/// A trusted direct-sight contact on team 1's board.
fn known(entity: u32, pos: [f32; 3], last_seen_tick: u64) -> KnownEnemy {
    KnownEnemy {
        entity,
        sighting: Sighting {
            entity,
            last_pos: pos,
            last_vel: [0.0; 2],
            last_seen_tick,
            first_seen_tick: 0,
            confidence: 1.0,
            source: SightSource::DirectLos,
        },
        threat: ThreatEstimate {
            dps: 60.0,
            range_band: [0.0, 150.0],
            can_reach_me: true,
            incoming: true,
        },
    }
}

/// A world with a leader, three wingmen and a hostile, plus a board that knows
/// the hostile, so fire gates can be exercised end to end.
fn combat_world() -> (AiWorld, SightingsBoard, CoordState) {
    let leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let w2 = view(2, 1, [-30.0, 0.0, -20.0], 0.0);
    let w3 = view(3, 1, [30.0, 0.0, -20.0], 0.0);
    let w4 = view(4, 1, [0.0, 0.0, -40.0], 0.0);
    let hostile = view(50, 2, [0.0, 0.0, 100.0], 0.0);
    let world = world(vec![leader, w2, w3, w4, hostile]);
    let mut board = SightingsBoard::new(3, 400.0);
    board.by_team[1].push(known(50, [0.0, 0.0, 100.0], 0));
    (world, board, CoordState::default())
}

#[test]
fn every_command_resolves_to_a_real_motor_output_on_the_first_tick() {
    // Each movement command drives a non-zero throttle toward its goal on the
    // very first tick (latency 0 ≤ 1 s), through the real motor.
    let leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let w = view(2, 1, [50.0, 0.0, 50.0], 0.0);
    let enemy = view(50, 2, [-100.0, 0.0, -100.0], 0.0);
    let pilot = view(60, 1, [40.0, 0.0, 40.0], 0.0);
    let world = world(vec![w.clone(), leader, enemy, pilot]);
    let board = SightingsBoard::new(3, 400.0);
    let coord = CoordState::default();

    let movements: Vec<WingmanCommand> = vec![
        WingmanCommand::Follow {
            leader: 1,
            formation: WingmanFormation::Line,
            close: false,
        },
        WingmanCommand::AttackTarget { target: 50 },
        WingmanCommand::AttackAtWill,
        WingmanCommand::Escort { leader: 1 },
        WingmanCommand::Hold {
            point: [-200.0, 0.0, -200.0],
        },
        WingmanCommand::DefendPoint {
            point: [-200.0, 0.0, -200.0],
        },
        WingmanCommand::DefendEntity { entity: 1 },
        WingmanCommand::Patrol {
            a: [100.0, 0.0, 0.0],
            b: [-100.0, 0.0, 0.0],
            toward_b: true,
        },
        WingmanCommand::Regroup { leader: 1 },
        WingmanCommand::Retreat,
        WingmanCommand::PickupPilot { pilot: 60 },
    ];
    for cmd in movements {
        let plan = plan(&w, &cmd, &world, 0);
        let mut rng = AiRng::new(AiRng::mix(0xD027, w.id as u64));
        let out = drive(&w, &plan, &world, &board, &coord, OpenField, &mut rng);
        let steer = out.steer.expect("movement command produces steer");
        assert!(
            steer.throttle > 0.0,
            "{cmd:?} should move on the first tick, got {steer:?}"
        );
    }

    // Cease fire and hold-fire stance gate fire off on the first tick.
    for cmd in [
        WingmanCommand::CeaseFire,
        WingmanCommand::Stance(Stance::HoldFire),
    ] {
        let plan = plan(&w, &cmd, &world, 0);
        let mut rng = AiRng::new(AiRng::mix(0xD027, w.id as u64));
        let out = drive(&w, &plan, &world, &board, &coord, OpenField, &mut rng);
        assert!(out.fire.is_none(), "{cmd:?} holds fire");
    }
}

#[test]
fn no_friendly_fire_into_the_player_line() {
    // The player stands between the wingman and the hostile; the wingman's fire
    // is gated off through the real combat fire seam, even weapons-free.
    let (world, board, coord) = combat_world();
    // The wingman at (0,0,-40) shoots at the hostile at (0,0,+100); the leader
    // at (0,0,0) is directly in the cone.
    let wingman = world.units.iter().find(|u| u.id == 4).expect("the unit");
    let plan = WingmanPlan {
        steer: None,
        fire: FireGate::WeaponsFree,
        callout: None,
        option_bias: None,
    };
    let mut rng = AiRng::new(AiRng::mix(world.world_seed, wingman.id as u64));
    let out = drive(wingman, &plan, &world, &board, &coord, OpenField, &mut rng);
    assert!(
        out.fire.is_none(),
        "a friendly in the player's line blocks the shot"
    );
}

#[test]
fn attack_my_target_fires_only_at_the_named_target() {
    let (world, board, coord) = combat_world();
    let wingman = world.units.iter().find(|u| u.id == 2).expect("the unit");
    let plan = plan(wingman, &WingmanCommand::AttackTarget { target: 50 }, &world, 0);
    assert_eq!(plan.fire, FireGate::TargetOnly(50));
    let mut rng = AiRng::new(AiRng::mix(world.world_seed, wingman.id as u64));
    let out = drive(wingman, &plan, &world, &board, &coord, OpenField, &mut rng);
    if let Some(fire) = out.fire {
        assert_eq!(fire.target, 50, "attack-my-target shoots only the named target");
    }
}

/// Three wingmen hold formation on a leader moving in a straight line through
/// the real motor and the hover step. They start in their slots (the formation is
/// already formed); the acceptance is holding spacing while the leader moves.
/// Returns the worst adjacent-spacing error and whether every wingman stayed
/// out of the leader's forward lane during the run.
fn follow_while_moving(formation: WingmanFormation, gap: f32) -> (f32, bool) {
    let params = HoverParams::default();
    let mut leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    // The followed leader is the commander's hull (the allied keep-out band).
    leader.is_commander = true;
    let mut wingmen: Vec<AiUnitView> = (0..3)
        .map(|i| {
            let slot = wingman_slot(formation, i, leader.pos, leader.yaw, false);
            view(2 + i as u32, 1, slot, 0.0)
        })
        .collect();
    let mut hs: Vec<HoverState> = (0..3).map(|_| HoverState::default()).collect();
    let board = SightingsBoard::new(3, 400.0);
    let coord = CoordState::default();
    let cmd = WingmanCommand::Follow {
        leader: 1,
        formation,
        close: false,
    };

    let mut max_err = 0.0f32;
    let mut lane_ok = true;
    for tick in 0..1200u64 {
        // The leader advances +Z at a steady 12 m/s (yaw 0 = forward +Z).
        leader.pos[2] += 12.0 / 60.0;
        leader.vel = [0.0, 0.0, 12.0]; // the real read view carries the velocity
        let mut units = vec![leader.clone()];
        units.extend(wingmen.iter().cloned());
        let world = AiWorld {
            tick,
            world_seed: 0xD027,
            units,
        };

        for i in 0..3 {
            let plan = plan(&wingmen[i], &cmd, &world, i);
            let mut rng = AiRng::new(AiRng::mix(world.world_seed ^ tick, wingmen[i].id as u64));
            let out = drive(&wingmen[i], &plan, &world, &board, &coord, OpenField, &mut rng);
            if let Some(s) = out.steer {
                step(&params, &mut hs[i], &[s.throttle, s.strafe, s.steer], 0.0, DT);
            }
            wingmen[i].pos[0] += hs[i].velocity[0] / 60.0;
            wingmen[i].pos[2] += hs[i].velocity[2] / 60.0;
            wingmen[i].yaw = hs[i].yaw;
            wingmen[i].vel = hs[i].velocity;
        }

        // Measure after a short warm-up (the wingmen accelerate to cruise).
        if tick >= 120 {
            for i in 0..2 {
                let err = spacing_error(wingmen[i].pos, wingmen[i + 1].pos, gap);
                max_err = det_max(max_err, err.abs());
            }
            for w in &wingmen {
                lane_ok &= slot_clears_leader_lane(w.pos, leader.pos, leader.yaw);
            }
        }
    }
    (max_err, lane_ok)
}

#[test]
fn formation_spacing_stays_within_20_percent_while_moving() {
    let (line_err, line_lane) = follow_while_moving(WingmanFormation::Line, 12.0);
    assert!(
        line_err <= 0.20,
        "Line spacing while moving drifted {:.1}%",
        line_err * 100.0
    );
    assert!(line_lane, "a Line wingman entered the leader's fire lane");

    let (col_err, col_lane) = follow_while_moving(WingmanFormation::Column, 16.0);
    assert!(
        col_err <= 0.20,
        "Column spacing while moving drifted {:.1}%",
        col_err * 100.0
    );
    assert!(col_lane, "a Column wingman entered the leader's fire lane");
}

/// Live path (the one the match runs): `WingmanCommand::Escort` through
/// `plan` + `drive`, not the utility scorer. Ten escorts start piled on the
/// leader and must settle clear.
fn escort_drive_settle(cmd_of: impl Fn(u32) -> WingmanCommand) -> Vec<[f32; 3]> {
    let params = HoverParams::default();
    let mut leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    leader.is_commander = true;
    leader.hover.brake_accel = params.brake_accel;
    let mut escorts: Vec<(HoverState, AiUnitView)> = (0..10u32)
        .map(|i| {
            let a = i as f32 * 0.61;
            let mut u = view(2 + i, 1, [math::sin(a) * 4.0, 0.0, math::cos(a) * 4.0], 0.0);
            u.hover.brake_accel = params.brake_accel;
            (HoverState::default(), u)
        })
        .collect();
    let board = SightingsBoard::new(3, 400.0);
    let coord = CoordState::default();
    for tick in 0..1800u64 {
        let mut units = vec![leader.clone()];
        units.extend(escorts.iter().map(|(_, u)| u.clone()));
        let world = AiWorld {
            tick,
            world_seed: 0xD027,
            units,
        };
        let mut steers = Vec::with_capacity(escorts.len());
        for (i, (_, u)) in escorts.iter().enumerate() {
            let cmd = cmd_of(1);
            let plan = plan(u, &cmd, &world, i);
            let mut rng = AiRng::new(AiRng::mix(world.world_seed ^ tick, u.id as u64));
            let out = drive(u, &plan, &world, &board, &coord, OpenField, &mut rng);
            steers.push(out.steer);
        }
        for (i, steer) in steers.into_iter().enumerate() {
            if let Some(s) = steer {
                step(
                    &params,
                    &mut escorts[i].0,
                    &[s.throttle, s.strafe, s.steer],
                    0.0,
                    DT,
                );
            }
            escorts[i].1.pos[0] += escorts[i].0.velocity[0] / 60.0;
            escorts[i].1.pos[2] += escorts[i].0.velocity[2] / 60.0;
            escorts[i].1.yaw = escorts[i].0.yaw;
            escorts[i].1.vel = escorts[i].0.velocity;
        }
    }
    escorts.into_iter().map(|(_, u)| u.pos).collect()
}

#[test]
fn ten_escorts_on_the_live_order_settle_clear_of_the_leader() {
    let keep = 3.9 + 3.9 + 2.0;
    for cmd in [
        WingmanCommand::Escort { leader: 1 },
        WingmanCommand::Regroup { leader: 1 },
    ] {
        let pos = escort_drive_settle(|_| cmd);
        for (i, p) in pos.iter().enumerate() {
            let d = dist(*p, [0.0, 0.0, 0.0]);
            assert!(
                d >= keep,
                "{cmd:?} escort {i} parked {d:.1} m from the leader (< {keep:.1})"
            );
        }
        for i in 0..pos.len() {
            for j in (i + 1)..pos.len() {
                let d = dist(pos[i], pos[j]);
                assert!(d >= 7.8, "{cmd:?} escorts {i}/{j} overlap at {d:.1} m");
            }
        }
    }
}

#[test]
fn separation_moves_a_hull_whose_nose_points_at_the_leader() {
    let mut leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    leader.is_commander = true;
    // 4 m in front, nose pointed back at the leader. The escape is behind
    // the nose: throttle must reverse (or strafe) on this tick, not sit at 0.
    let wing = view(2, 1, [0.0, 0.0, 4.0], std::f32::consts::PI);
    let world = world(vec![leader, wing.clone()]);
    let board = SightingsBoard::new(3, 400.0);
    let coord = CoordState::default();
    let plan = plan(&wing, &WingmanCommand::Escort { leader: 1 }, &world, 0);
    let mut rng = AiRng::new(0xD027);
    let out = drive(&wing, &plan, &world, &board, &coord, OpenField, &mut rng);
    let steer = out.steer.expect("separation steer");
    assert!(
        steer.throttle < -0.5 || steer.strafe.abs() > 0.5,
        "a hull on the nose must translate out, got {steer:?}"
    );
}

#[test]
fn a_coasting_escort_brakes_before_the_leader_when_the_leader_stops() {
    let params = HoverParams::default();
    let mut leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    leader.is_commander = true;
    leader.hover.brake_accel = params.brake_accel;
    // Rank-0 slot, yaw 0: beside and behind. Cruise velocity is toward the
    // leader (+Z), the case where a stop used to coast the hull up the nose.
    let mut wing = view(2, 1, [15.7, 0.0, -19.6], 0.0);
    wing.hover.brake_accel = params.brake_accel;
    wing.vel = [0.0, 0.0, 25.0];
    let mut hs = HoverState {
        velocity: wing.vel,
        ..HoverState::default()
    };
    let board = SightingsBoard::new(3, 400.0);
    let coord = CoordState::default();
    let cmd = WingmanCommand::Escort { leader: 1 };
    let mut nearest = dist(wing.pos, leader.pos);
    for tick in 0..180u64 {
        let world = AiWorld {
            tick,
            world_seed: 0xD027,
            units: vec![leader.clone(), wing.clone()],
        };
        let plan = plan(&wing, &cmd, &world, 0);
        let mut rng = AiRng::new(AiRng::mix(0xD027 ^ tick, 2));
        let out = drive(&wing, &plan, &world, &board, &coord, OpenField, &mut rng);
        if let Some(s) = out.steer {
            step(&params, &mut hs, &[s.throttle, s.strafe, s.steer], 0.0, DT);
        }
        wing.pos[0] += hs.velocity[0] / 60.0;
        wing.pos[2] += hs.velocity[2] / 60.0;
        wing.yaw = hs.yaw;
        wing.vel = hs.velocity;
        nearest = det_min(nearest, dist(wing.pos, leader.pos));
    }
    assert!(
        nearest >= 3.9 + 3.9 + 2.0,
        "coasting escort closed to {nearest:.1} m"
    );
}

// ---------------------------------------------------------------------------
// Re-up routing, formation orders, fall-back fire
// ---------------------------------------------------------------------------

/// A structure of `team` that gives the Re-up `reup`.
fn building(id: u32, team: u8, reup: ReupRole, pos: [f32; 3]) -> AiUnitView {
    let mut u = view(id, team, pos, 0.0);
    u.is_unit = false;
    u.reup = reup;
    u
}

#[test]
fn retreat_routes_to_the_reup_point_and_keeps_guns_free() {
    let w = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    let hangar = building(3, 1, ReupRole::Repair, [50.0, 0.0, 0.0]);
    let enemy = view(50, 2, [-100.0, 0.0, -100.0], 0.0);
    let world = world(vec![w.clone(), hangar, enemy]);
    let p = plan(&w, &WingmanCommand::Retreat, &world, 0);
    assert_eq!(p.fire, FireGate::WeaponsFree, "falling back keeps the guns free");
    assert_eq!(
        p.steer.expect("a steer goal").point,
        [50.0, 0.0, 0.0],
        "retreat walks to the Re-up point"
    );
    // Without a Re-up point it still steps 40 m away from the threat.
    let world = self::world(vec![w.clone(), view(50, 2, [0.0, 0.0, 10.0], 0.0)]);
    let p = plan(&w, &WingmanCommand::Retreat, &world, 0);
    assert_eq!(p.steer.expect("a steer goal").point, [0.0, 0.0, -40.0]);
}

#[test]
fn hold_without_a_point_holds_position() {
    let w = view(2, 1, [30.0, 0.0, -10.0], 0.0);
    let world = world(vec![view(1, 1, [0.0, 0.0, 0.0], 0.0), w.clone()]);
    let p = plan(
        &w,
        &WingmanCommand::Hold {
            point: [0.0, 0.0, 0.0],
        },
        &world,
        0,
    );
    assert_eq!(
        p.steer.expect("a steer goal").point,
        [30.0, 0.0, -10.0],
        "an unset Hold holds where the unit stands, never the map origin"
    );
}

#[test]
fn advance_screens_ahead_and_flank_sides_off() {
    let leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let w = view(2, 1, [30.0, 0.0, 0.0], 0.0);
    let world = world(vec![leader, w.clone()]);
    let a = plan(&w, &WingmanCommand::Advance { leader: 1 }, &world, 0);
    let ap = a.steer.expect("a steer goal").point;
    assert!(ap[2] > 0.0, "Forward screens ahead of the anchor, got {ap:?}");
    assert_eq!(a.fire, FireGate::WeaponsFree);
    assert_eq!(a.option_bias, Some(OptionId::Push));
    let f = plan(&w, &WingmanCommand::Flank { leader: 1 }, &world, 0);
    let fp = f.steer.expect("a steer goal").point;
    assert!(fp[0].abs() >= 14.0, "Flank sits off to the side, got {fp:?}");
    assert_eq!(fp[2], 0.0, "Flank stays abreast of the anchor");
    assert_eq!(f.fire, FireGate::WeaponsFree);
    assert_eq!(f.option_bias, Some(OptionId::Flank));
    assert!(slot_clears_leader_lane(fp, [0.0, 0.0, 0.0], 0.0));
}

#[test]
fn damaged_unit_under_orders_walks_to_reup_but_keeps_firing() {
    // AttackAtWill at 0.2 hull with a repair structure behind and a trusted contact in
    // band ahead: the body walks to the structure, the guns keep firing.
    let mut w = view(2, 1, [0.0, 0.0, 0.0], std::f32::consts::PI);
    w.hull_pct = 0.2;
    w.ammo = AmmoView {
        slots: vec![Some(cannon(None))],
    };
    let hangar = building(3, 1, ReupRole::Repair, [0.0, 0.0, -80.0]);
    let enemy = view(50, 2, [0.0, 0.0, 60.0], 0.0);
    let world = AiWorld {
        tick: 30,
        world_seed: 0xD027,
        units: vec![w.clone(), hangar, enemy],
    };
    let mut board = SightingsBoard::new(3, 400.0);
    board.by_team[1].push(known(50, [0.0, 0.0, 60.0], 30));
    let coord = CoordState::default();
    let p = plan(&w, &WingmanCommand::AttackAtWill, &world, 0);
    let mut rng = AiRng::new(7);
    let out = drive(&w, &p, &world, &board, &coord, OpenField, &mut rng);
    let steer = out.steer.expect("the Re-up override steers the body");
    assert!(
        steer.throttle > 0.0,
        "the damaged body walks to the repair structure, got {steer:?}"
    );
    assert!(out.fire.is_some(), "falling back keeps firing");
}

// ---------------------------------------------------------------------------
// Not in DO27: the cross-arch pins
// ---------------------------------------------------------------------------

/// FNV-1a over the bits of a stream of floats.
fn fnv(bits: impl IntoIterator<Item = f32>) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for x in bits {
        for b in x.to_bits().to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// `[formation slots and the moving-follow spacing errors, the escort settle]`, recorded on x86-64 Linux.
/// The escort settle is re-pinned for ADR-8876 (`MATH_BITS_VERSION` 1, the canonical `math::atan2`; was
/// `0x9e83_e46c_48fa_c412`); the formation hash did not move.
const WINGMAN_PINS: [u64; 2] = [0xfee9_20f3_34b3_d27e, 0xa9b9_920f_5799_9e2f];

/// The wingman bits, pinned: every formation's slots over eight headings (close and open), the worst spacing error
/// of the moving Line and Column follows, and the ten-escort settle on the live order. A mismatch on another
/// architecture is a cross-arch determinism bug, not a re-pin.
#[test]
fn wingman_bits_match_the_pinned_hashes() {
    let mut slot_bits = Vec::new();
    for k in 0..8u32 {
        let yaw = k as f32 * 0.7 - 2.4;
        for formation in [
            WingmanFormation::Line,
            WingmanFormation::Wedge,
            WingmanFormation::Column,
            WingmanFormation::Loose,
        ] {
            for slot in formation_slots(formation, 6, [3.0, 1.0, -2.0], yaw, k.is_multiple_of(2)) {
                slot_bits.extend(slot);
            }
        }
    }
    let (line_err, _) = follow_while_moving(WingmanFormation::Line, 12.0);
    let (col_err, _) = follow_while_moving(WingmanFormation::Column, 16.0);
    slot_bits.extend([line_err, col_err]);
    let settle = escort_drive_settle(|leader| WingmanCommand::Escort { leader });
    let pins = [fnv(slot_bits), fnv(settle.into_iter().flatten())];
    println!("regia-wingman-pins {:#018x} {:#018x}", pins[0], pins[1]);
    assert_eq!(pins, WINGMAN_PINS);
}
