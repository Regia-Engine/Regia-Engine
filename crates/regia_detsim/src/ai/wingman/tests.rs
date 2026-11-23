// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/wingman.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (the `tests` module, lines 900-1319, plan row S-13, ADR-8554). Every DO27 test keeps its scenario,
// numbers and assertions; named changes:
// - the unit view is S-12's `AiUnitView` (hull and weapon stems dropped; DO27's cannon table entry inlined: direct,
//   120 m/s, 0-150 m) and the hover step uses the A-8 names with the engine-authored default hull (ADR-8164);
// - the order-mapping tests run against a fixture order type that implements `WingmanOrder` with DO27's
//   `from_unit_order` body (DO27's order and formation enums, reduced to what the mapping reads): that is
//   the orders-to-plans interface a host fills in. The team-order tests use `TeamDirective`;
// - the hillside test's std `clamp` goes through `det_clamp` (ADR-8085), line of sight is `OpenField`, and `unwrap()`
//   is `expect()` (the crate's clippy config denies `unwrap` in tests too).
// - ADR-8655: `broadcast_team_orders_map_to_the_default_behaviours` expects Follow to map to
//   `WingmanCommand::EscortCommander` (DO27's `Escort { leader: 0 }` named its player's entity id).
// Also from Direct Order 27 (ir-sim/src/ai/mod.rs, commit c9513623f, tag regia-absorb-4), ADR-8012. Mark: adapt
// (lines 1598-1647): `run_commanded_drives_a_wingman_and_biases_its_decision`, over `run_commanded` with an empty
// decision map instead of DO27's whole game pipeline; same assertions.

use super::*;
use crate::ai::{AiLevel, AmmoSlotView, HoverView, OpenField, WeaponRole};
use crate::float::det_clamp;
use crate::hover::{HoverParams, HoverState, step};

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

fn finite_ammo(view: &mut AiUnitView, cur: u32, max: u32) {
    view.ammo.slots = vec![Some(AmmoSlotView {
        role: WeaponRole::Direct,
        projectile_speed: 120.0,
        ammo: Some((cur, max)),
        band: [0.0, 150.0],
    })];
}

fn world(units: Vec<AiUnitView>) -> AiWorld {
    AiWorld {
        tick: 0,
        world_seed: 0xD027,
        units,
    }
}

// -- the orders-to-plans fixture: a host order type ---------------------

/// A host's formation orders (DO27's set): four form-up shapes, two the wingman layer resolves, and the commander
/// layouts the host's sim steers itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FormationKind {
    Front,
    Back,
    Hold,
    Flee,
    Forward,
    Behind,
    Flank,
    Fallback,
    Line,
    Wedge,
    Column,
    Loose,
}

impl FormationKind {
    fn wingman_shape(self) -> Option<WingmanFormation> {
        match self {
            FormationKind::Line | FormationKind::Flank => Some(WingmanFormation::Line),
            FormationKind::Wedge => Some(WingmanFormation::Wedge),
            FormationKind::Column => Some(WingmanFormation::Column),
            FormationKind::Loose => Some(WingmanFormation::Loose),
            _ => None,
        }
    }
}

/// A host's per-unit orders (DO27's set, the variants its mapping reads).
#[derive(Debug, Clone, Copy, PartialEq)]
enum UnitOrder {
    Follow {
        target: u32,
        close: bool,
    },
    AttackTarget {
        entity: u32,
    },
    Attack,
    DefendBuilding {
        entity: u32,
    },
    Formation {
        kind: FormationKind,
        anchor: u32,
    },
    Patrol {
        ax: f32,
        az: f32,
        bx: f32,
        bz: f32,
        toward_b: bool,
    },
    ReUp,
    Stance {
        code: u8,
    },
    CeaseFire,
    WeaponsFree,
    // Orders the host steers itself (their payloads are not read by the mapping).
    Lead,
    GoTo,
    MineArea,
    RescuePilot,
}

impl WingmanOrder for UnitOrder {
    fn wingman_command(&self) -> Option<WingmanCommand> {
        match self {
            UnitOrder::Follow { target, close } => Some(WingmanCommand::Follow {
                leader: *target,
                formation: WingmanFormation::Wedge,
                close: *close,
            }),
            UnitOrder::AttackTarget { entity } => Some(WingmanCommand::AttackTarget { target: *entity }),
            UnitOrder::Attack => Some(WingmanCommand::AttackAtWill),
            UnitOrder::DefendBuilding { entity } => Some(WingmanCommand::DefendEntity { entity: *entity }),
            UnitOrder::Formation { kind, anchor } => match kind {
                FormationKind::Hold => Some(WingmanCommand::Hold {
                    point: [0.0, 0.0, 0.0],
                }),
                FormationKind::Flee => Some(WingmanCommand::Retreat),
                // The host's sim steers these itself.
                FormationKind::Front
                | FormationKind::Back
                | FormationKind::Forward
                | FormationKind::Behind
                | FormationKind::Flank
                | FormationKind::Fallback => None,
                // Line / Wedge / Column / Loose are form-up shapes: follow the
                // anchor in the chosen formation.
                _ => Some(WingmanCommand::Follow {
                    leader: *anchor,
                    formation: kind.wingman_shape().unwrap_or(WingmanFormation::Wedge),
                    close: false,
                }),
            },
            UnitOrder::Patrol {
                ax,
                az,
                bx,
                bz,
                toward_b,
            } => Some(WingmanCommand::Patrol {
                a: [*ax, 0.0, *az],
                b: [*bx, 0.0, *bz],
                toward_b: *toward_b,
            }),
            // Re-up covers both old runs: a low hull and a dry magazine both
            // become the one retreat-and-refill behavior.
            UnitOrder::ReUp => Some(WingmanCommand::Retreat),
            UnitOrder::Stance { code } => Some(WingmanCommand::Stance(stance_from_code(*code))),
            UnitOrder::CeaseFire => Some(WingmanCommand::CeaseFire),
            UnitOrder::WeaponsFree => Some(WingmanCommand::WeaponsFree),
            _ => None,
        }
    }
}

// -- formation geometry -------------------------------------------------

#[test]
fn line_and_column_spacing_stay_within_20_percent() {
    let anchor = [0.0, 0.0, 0.0];
    for n in [2usize, 3, 5, 8] {
        let line = formation_slots(WingmanFormation::Line, n, anchor, 0.0, false);
        for w in line.windows(2) {
            let err = spacing_error(w[0], w[1], WINGMAN_SPACING_M);
            assert!(err.abs() <= 0.20, "line spacing err {err}");
        }
        let col = formation_slots(WingmanFormation::Column, n, anchor, 0.0, false);
        for w in col.windows(2) {
            let err = spacing_error(w[0], w[1], WINGMAN_COLUMN_GAP_M);
            assert!(err.abs() <= 0.20, "column spacing err {err}");
        }
    }
}

/// A 45 degree hillside falling east to a plain.
fn hillside(x: f32, _z: f32) -> f32 {
    -256.0 + det_clamp(24.0 - x, 0.0, 55.0)
}

#[test]
fn a_wedge_on_open_ground_keeps_the_leader_as_anchor() {
    let flat = |_: f32, _: f32| 3.0;
    let anchor = [10.0, 4.0, 20.0];
    for formation in [
        WingmanFormation::Wedge,
        WingmanFormation::Line,
        WingmanFormation::Column,
    ] {
        assert_eq!(
            level_formation_anchor(formation, 4, anchor, 0.7, false, &flat),
            anchor
        );
    }
}

#[test]
fn a_wedge_on_a_hillside_is_laid_out_on_level_ground() {
    // The leader hovers on the face (ground -232). The raw wedge puts one file 14 m down the slope and one 14 m up.
    let anchor = [0.0, -231.0, 0.0];
    let yaw = 0.0;
    let raw: Vec<[f32; 3]> = formation_slots(WingmanFormation::Wedge, 2, anchor, yaw, false);
    let raw_gap = (hillside(raw[0][0], raw[0][2]) - hillside(raw[1][0], raw[1][2])).abs();
    assert!(
        raw_gap > 20.0,
        "the raw wedge straddles the slope ({raw_gap} m of ground between the files)"
    );
    let at = level_formation_anchor(WingmanFormation::Wedge, 2, anchor, yaw, false, &hillside);
    assert_ne!(at, anchor, "the anchor moved off the slope");
    let slots = formation_slots(WingmanFormation::Wedge, 2, at, yaw, false);
    let base = hillside(at[0], at[2]);
    for slot in &slots {
        assert!(
            (hillside(slot[0], slot[2]) - base).abs() <= WINGMAN_LEVEL_M,
            "slot {slot:?} is off the anchor's level"
        );
    }
    // The authored spacing is untouched: the files are still one wedge pair (28 m) apart.
    let err = spacing_error(slots[0], slots[1], 2.0 * WINGMAN_LANE_CLEAR_M);
    assert!(err.abs() <= 0.20, "spacing error {err}");
    // Deterministic, and the leader's altitude over the ground is kept.
    assert_eq!(
        at,
        level_formation_anchor(WingmanFormation::Wedge, 2, anchor, yaw, false, &hillside)
    );
    assert!((at[1] - base - 1.0).abs() < 1.0e-3);
}

#[test]
fn with_no_level_ground_in_reach_the_leader_stays_the_anchor() {
    let wall = |x: f32, _: f32| x * 2.0;
    let anchor = [0.0, 0.0, 0.0];
    assert_eq!(
        level_formation_anchor(WingmanFormation::Wedge, 2, anchor, 0.0, false, &wall),
        anchor
    );
}

#[test]
fn every_formation_slot_clears_the_leader_lane() {
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
                "{formation:?} slot {i} sits in the leader's lane"
            );
            // And none is dead-ahead of the leader.
            let fwd = slot[2]; // yaw 0 → forward is +Z
            assert!(fwd <= 0.0, "{formation:?} slot {i} is ahead ({fwd})");
        }
    }
}

#[test]
fn formation_slot_turns_with_the_leader() {
    // Leader facing +X (yaw pi/2): forward is +X, right is -Z. The first
    // Column slot sits behind the leader (toward -X).
    let p = wingman_slot(
        WingmanFormation::Column,
        0,
        [10.0, 0.0, 5.0],
        std::f32::consts::FRAC_PI_2,
        false,
    );
    assert!(p[0] < 10.0, "behind the leader's forward (+X), got {p:?}");
}

#[test]
fn group_slots_are_deterministic_and_order_independent() {
    let a = assign_formation_slots(&[9, 3, 7], WingmanFormation::Line, [0.0, 0.0, 0.0], 0.0, false);
    let b = assign_formation_slots(&[3, 7, 9], WingmanFormation::Line, [0.0, 0.0, 0.0], 0.0, false);
    assert_eq!(a, b);
    // A Line spreads laterally by sorted-id slot order, so id 3 sits left
    // of id 7 sits left of id 9 (all at the same distance behind).
    assert!(a[&3][0] < a[&7][0] && a[&7][0] < a[&9][0]);
}

// -- friendly fire ------------------------------------------------------

#[test]
fn friendly_fire_gate_flags_the_player_in_the_cone() {
    let wingman = [0.0, 0.0, 0.0];
    let enemy = [0.0, 0.0, 100.0];
    // The player (commander) is between the wingman and the enemy: blocked.
    assert!(fire_blocked(wingman, enemy, &[[0.0, 0.0, 20.0]]));
    // No friendly in the cone: clear.
    assert!(!fire_blocked(wingman, enemy, &[]));
    // A friendly well off the line: clear.
    assert!(!fire_blocked(wingman, enemy, &[[40.0, 0.0, 20.0]]));
    // A friendly behind the shooter: clear.
    assert!(!fire_blocked(wingman, enemy, &[[0.0, 0.0, -20.0]]));
}

// -- reporting / repair / ammo ------------------------------------------

#[test]
fn report_orders_critical_then_repair_then_ammo() {
    let mut u = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    assert_eq!(report(&u), None, "healthy unit reports nothing");

    finite_ammo(&mut u, 3, 100);
    assert_eq!(report(&u), Some(CalloutId::LowAmmo), "low ammo reports");

    u.hull_pct = 0.20;
    u.ammo.slots = Vec::new();
    assert_eq!(report(&u), Some(CalloutId::NeedRepair));

    u.hull_pct = 0.10;
    assert_eq!(report(&u), Some(CalloutId::HullCritical));
}

#[test]
fn repair_and_ammo_runs_route_to_the_nearest_point() {
    let mut u = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let near = [10.0, 0.0, 0.0];
    let far = [90.0, 0.0, 0.0];
    // Healthy: no repair goal.
    assert!(repair_goal(&u, &[near, far]).is_none());
    u.hull_pct = 0.1;
    assert_eq!(repair_goal(&u, &[near, far]).expect("a repair goal").point, near);

    finite_ammo(&mut u, 1, 100);
    assert_eq!(ammo_goal(&u, &[near, far]).expect("an ammo goal").point, near);
    finite_ammo(&mut u, 80, 100);
    assert!(ammo_goal(&u, &[near, far]).is_none());
}

// -- command mapping ----------------------------------------------------

#[test]
fn every_wingman_unit_order_maps_onto_the_vocabulary() {
    use FormationKind as FK;
    let map = |o: UnitOrder| WingmanCommand::from_order(&o);
    assert_eq!(
        map(UnitOrder::Follow {
            target: 4,
            close: true
        }),
        Some(WingmanCommand::Follow {
            leader: 4,
            formation: WingmanFormation::Wedge,
            close: true
        })
    );
    assert_eq!(
        map(UnitOrder::AttackTarget { entity: 9 }),
        Some(WingmanCommand::AttackTarget { target: 9 })
    );
    assert_eq!(map(UnitOrder::Attack), Some(WingmanCommand::AttackAtWill));
    assert_eq!(
        map(UnitOrder::DefendBuilding { entity: 7 }),
        Some(WingmanCommand::DefendEntity { entity: 7 })
    );
    assert_eq!(
        map(UnitOrder::Formation {
            kind: FK::Hold,
            anchor: 1
        }),
        Some(WingmanCommand::Hold {
            point: [0.0, 0.0, 0.0]
        })
    );
    assert_eq!(
        map(UnitOrder::Formation {
            kind: FK::Flee,
            anchor: 1
        }),
        Some(WingmanCommand::Retreat)
    );
    // The four form-up shapes follow the anchor in that formation.
    assert_eq!(
        map(UnitOrder::Formation {
            kind: FK::Line,
            anchor: 4
        }),
        Some(WingmanCommand::Follow {
            leader: 4,
            formation: WingmanFormation::Line,
            close: false
        })
    );
    assert_eq!(
        map(UnitOrder::Formation {
            kind: FK::Column,
            anchor: 4
        }),
        Some(WingmanCommand::Follow {
            leader: 4,
            formation: WingmanFormation::Column,
            close: false
        })
    );
    // The fire stance / cease-fire / weapons-free orders map onto the gate.
    assert_eq!(
        map(UnitOrder::Stance { code: 0 }),
        Some(WingmanCommand::Stance(Stance::Aggressive))
    );
    assert_eq!(
        map(UnitOrder::Stance { code: 1 }),
        Some(WingmanCommand::Stance(Stance::Defensive))
    );
    assert_eq!(
        map(UnitOrder::Stance { code: 2 }),
        Some(WingmanCommand::Stance(Stance::HoldFire))
    );
    assert_eq!(map(UnitOrder::CeaseFire), Some(WingmanCommand::CeaseFire));
    assert_eq!(map(UnitOrder::WeaponsFree), Some(WingmanCommand::WeaponsFree));
    assert_eq!(
        map(UnitOrder::Patrol {
            ax: 0.0,
            az: 0.0,
            bx: 5.0,
            bz: 5.0,
            toward_b: true
        }),
        Some(WingmanCommand::Patrol {
            a: [0.0, 0.0, 0.0],
            b: [5.0, 0.0, 5.0],
            toward_b: true
        })
    );
    assert_eq!(map(UnitOrder::ReUp), Some(WingmanCommand::Retreat));
    // Orders the sim steers itself never reach the wingman layer.
    assert_eq!(map(UnitOrder::Lead), None);
    assert_eq!(map(UnitOrder::GoTo), None);
    assert_eq!(map(UnitOrder::MineArea), None);
    // Rescue is sim-steered too (pickup job + hatch boarding).
    assert_eq!(map(UnitOrder::RescuePilot), None);
}

#[test]
fn commander_formations_stay_on_the_sim_layer() {
    use FormationKind as FK;
    // Forward / Behind / Flank / Fall back are steered by the host's
    // commander layout, so they never reach the wingman plan layer — like
    // Front and Back.
    for kind in [
        FK::Forward,
        FK::Behind,
        FK::Flank,
        FK::Fallback,
        FK::Front,
        FK::Back,
    ] {
        assert_eq!(
            WingmanCommand::from_order(&UnitOrder::Formation { kind, anchor: 4 }),
            None,
            "{kind:?} must not produce a wingman plan"
        );
    }
    // The form-up shapes do (Flank aside, which the host lays out).
    for (kind, shape) in [
        (FK::Wedge, WingmanFormation::Wedge),
        (FK::Loose, WingmanFormation::Loose),
    ] {
        assert_eq!(
            WingmanCommand::from_order(&UnitOrder::Formation { kind, anchor: 4 }),
            Some(WingmanCommand::Follow {
                leader: 4,
                formation: shape,
                close: false
            })
        );
    }
}

#[test]
fn broadcast_team_orders_map_to_the_default_behaviours() {
    assert_eq!(
        WingmanCommand::from_team_directive(TeamDirective::Attack),
        WingmanCommand::AttackAtWill
    );
    // ADR-8655: DO27 mapped Follow to `Escort { leader: 0 }` (entity 0 was its player). Here no entity id is
    // reserved, so a missing entity 0 left the wingman forming a wedge on its own pose; the broadcast Follow now
    // escorts whoever is the team's commander, resolved from the world in `plan`.
    assert_eq!(
        WingmanCommand::from_team_directive(TeamDirective::Follow),
        WingmanCommand::EscortCommander
    );
    assert_eq!(
        WingmanCommand::from_team_directive(TeamDirective::Defend),
        WingmanCommand::Hold {
            point: [0.0, 0.0, 0.0]
        }
    );
    assert_eq!(
        WingmanCommand::from_team_directive(TeamDirective::GoTo),
        WingmanCommand::AttackAtWill
    );
    assert_eq!(
        WingmanCommand::from_team_directive(TeamDirective::AttackAt),
        WingmanCommand::AttackAtWill
    );
}

/// The fleet retreat disengages every unit.
#[test]
fn fleet_retreat_maps_to_disengage() {
    assert_eq!(
        WingmanCommand::from_team_directive(TeamDirective::Retreat),
        WingmanCommand::Retreat
    );
}

// -- plan: behavior onset is immediate ----------------------------------

#[test]
fn follow_plan_steers_to_a_slot_behind_the_leader() {
    let leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let w = view(2, 1, [30.0, 0.0, 0.0], 0.0);
    let world = world(vec![leader.clone(), w.clone()]);
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
    let point = p.steer.expect("a steer goal").point;
    assert!(point[2] < 0.0, "slot is behind the leader, got {point:?}");
    assert_eq!(p.fire, FireGate::WeaponsFree);
    assert_eq!(p.option_bias, Some(OptionId::Regroup));
}

#[test]
fn attack_target_plan_steers_at_the_target_and_fires_only_at_it() {
    let w = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    let enemy = view(50, 2, [100.0, 0.0, 0.0], 0.0);
    let world = world(vec![w.clone(), enemy.clone()]);
    let p = plan(&w, &WingmanCommand::AttackTarget { target: 50 }, &world, 0);
    assert_eq!(p.steer.expect("a steer goal").point, [100.0, 0.0, 0.0]);
    assert_eq!(p.fire, FireGate::TargetOnly(50));
    assert_eq!(p.option_bias, Some(OptionId::FocusFire));
    assert_eq!(p.callout, Some(CalloutId::Engaging));
}

#[test]
fn each_command_resolves_to_its_expected_bias_and_fire() {
    let w = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    let leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let enemy = view(50, 2, [100.0, 0.0, 0.0], 0.0);
    let pilot = view(60, 1, [40.0, 0.0, 0.0], 0.0);
    let world = world(vec![w.clone(), leader, enemy, pilot]);

    let cases: Vec<(WingmanCommand, Option<OptionId>, FireGate)> = vec![
        (WingmanCommand::AttackAtWill, None, FireGate::WeaponsFree),
        (
            WingmanCommand::Advance { leader: 1 },
            Some(OptionId::Push),
            FireGate::WeaponsFree,
        ),
        (
            WingmanCommand::Flank { leader: 1 },
            Some(OptionId::Flank),
            FireGate::WeaponsFree,
        ),
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
        // The plan's bias is the command's own.
        assert_eq!(cmd.option_bias(), bias, "{cmd:?}");
    }
    // Retreat steers away from the enemy.
    let p = plan(&w, &WingmanCommand::Retreat, &world, 0);
    assert!(
        p.steer.expect("a steer goal").point[0] < 0.0,
        "retreat heads away from +X enemy"
    );
}

#[test]
fn patrol_plan_targets_the_current_waypoint() {
    let w = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    let world = world(vec![w.clone()]);
    let p = plan(
        &w,
        &WingmanCommand::Patrol {
            a: [10.0, 0.0, 0.0],
            b: [50.0, 0.0, 0.0],
            toward_b: true,
        },
        &world,
        0,
    );
    assert_eq!(p.steer.expect("a steer goal").point, [50.0, 0.0, 0.0]);
}

// -- real motor: a command reaches its goal within one second ------------

#[test]
fn drive_uses_the_real_motor_and_moves_within_one_second() {
    let w = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    let enemy = view(50, 2, [100.0, 0.0, 0.0], 0.0);
    let world = world(vec![w.clone(), enemy.clone()]);
    let board = SightingsBoard::new(3, 400.0);
    let coord = CoordState::default();
    let p = plan(&w, &WingmanCommand::AttackTarget { target: 50 }, &world, 0);

    // The real motor drives the hull through hover inertia, not a linear
    // placeholder step: within the first second the wingman is moving.
    let params = HoverParams::default();
    let mut hs = HoverState::default();
    let mut pos = w.pos;
    let mut moved_in_first_second = false;
    for tick in 0..60 {
        let mut unit = w.clone();
        unit.pos = pos;
        unit.yaw = hs.yaw;
        unit.vel = hs.velocity;
        let mut rng = AiRng::new(AiRng::mix(world.world_seed ^ (tick as u64), unit.id as u64));
        let out = drive(&unit, &p, &world, &board, &coord, OpenField, &mut rng);
        let before = pos;
        if let Some(s) = out.steer {
            step(
                &params,
                &mut hs,
                &[s.throttle, s.strafe, s.steer],
                0.0,
                1.0 / 60.0,
            );
        }
        pos[0] += hs.velocity[0] / 60.0;
        pos[2] += hs.velocity[2] / 60.0;
        if dist_xz(pos, before) > 1.0e-6 {
            moved_in_first_second = true;
        }
    }
    assert!(
        moved_in_first_second,
        "the real motor moves the wingman immediately"
    );
    assert!(pos[0] > 0.0, "moving toward the +X target, got x={}", pos[0]);
}

#[test]
fn route_resolves_a_group_deterministically() {
    let leader = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let a = view(2, 1, [20.0, 0.0, 0.0], 0.0);
    let b = view(3, 1, [-20.0, 0.0, 0.0], 0.0);
    let world = world(vec![leader, a.clone(), b.clone()]);
    let cmd = WingmanCommand::Regroup { leader: 1 };
    let plans = route(&cmd, &[3, 2], &world);
    assert_eq!(plans.len(), 2);
    // Slot index is assigned by sorted id: unit 2 gets slot 0, unit 3 slot 1.
    let p2 = plans[&2];
    let p3 = plans[&3];
    assert!(p2.steer.is_some() && p3.steer.is_some());
    assert_ne!(
        p2.steer.expect("a steer goal").point,
        p3.steer.expect("a steer goal").point
    );
}

// -- Regia additions ------------------------------------------------------

#[test]
fn formation_names_and_callout_keys_are_stable() {
    for (name, shape) in [
        ("line", WingmanFormation::Line),
        ("form_wedge", WingmanFormation::Wedge),
        ("column", WingmanFormation::Column),
        ("form_loose", WingmanFormation::Loose),
    ] {
        assert_eq!(WingmanFormation::parse(name), Some(shape));
    }
    assert_eq!(WingmanFormation::parse("vee"), None);
    assert!(!formation_is_spaced(WingmanFormation::Loose));
    assert!(formation_is_spaced(WingmanFormation::Column));
    let names: Vec<&str> = CalloutId::ALL.iter().map(|c| c.name()).collect();
    assert_eq!(
        names,
        [
            "ack",
            "engaging",
            "hull_critical",
            "need_repair",
            "low_ammo",
            "enemy_behind",
            "incoming"
        ]
    );
}

#[test]
fn a_command_is_its_own_order_and_unknown_ids_get_no_plan() {
    let cmd = WingmanCommand::Hold {
        point: [1.0, 0.0, 1.0],
    };
    assert_eq!(WingmanCommand::from_order(&cmd), Some(cmd));
    let w = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    let plans = route(&cmd, &[2, 77], &world(vec![w]));
    assert_eq!(plans.keys().copied().collect::<Vec<_>>(), [2]);
}

// -- the wingman override of a combat run (DO27's ai/mod.rs pipeline test, lines 1598-1647) ---------------------

#[test]
fn run_commanded_drives_a_wingman_and_biases_its_decision() {
    let mk = |id: u32, team: u8, pos: [f32; 3]| {
        let mut u = view(id, team, pos, 0.0);
        u.ammo.slots = vec![Some(AmmoSlotView {
            role: WeaponRole::Direct,
            projectile_speed: 120.0,
            ammo: None,
            band: [0.0, 150.0],
        })];
        u
    };
    let world = AiWorld {
        tick: 0,
        world_seed: 7,
        units: vec![mk(1, 1, [0.0, 0.0, 0.0]), mk(2, 1, [60.0, 0.0, 0.0])],
    };
    let board = SightingsBoard::new(3, crate::ai::RADAR_RANGE_M);
    let mut decisions = BTreeMap::new();
    let orders = [(
        2u32,
        WingmanCommand::Follow {
            leader: 1,
            formation: WingmanFormation::Line,
            close: false,
        },
        0usize,
    )];
    let outputs = run_commanded(
        &world,
        &board,
        &CoordState::default(),
        OpenField,
        &mut decisions,
        &orders,
    );
    let idx = world.units.iter().position(|u| u.id == 2).expect("the unit");
    assert!(
        outputs[idx].steer.is_some(),
        "a commanded wingman steers via the real motor"
    );
    assert_eq!(
        decisions.get(&2).and_then(|d| d.current),
        Some(OptionId::Regroup),
        "the command biases the decision"
    );
    // The uncommanded leader keeps its own (empty) decision.
    assert_eq!(decisions.get(&1).and_then(|d| d.current), None);
}

#[test]
fn apply_commands_replaces_only_the_commanded_outputs() {
    let world = world(vec![
        view(1, 1, [0.0, 0.0, 0.0], 0.0),
        view(2, 1, [60.0, 0.0, 0.0], 0.0),
        view(3, 1, [-60.0, 0.0, 0.0], 0.0),
    ]);
    let board = SightingsBoard::new(3, 400.0);
    let targets = TeamTargets::build(&world, &board);
    let coord = CoordState::default();
    let mut outputs = vec![AiUnitOutput::default(); 3];
    let orders = [
        (3u32, WingmanCommand::CeaseFire, 0usize),
        (
            2u32,
            WingmanCommand::Hold {
                point: [70.0, 0.0, 0.0],
            },
            1usize,
        ),
    ];
    let biases = apply_commands(&world, &targets, &coord, OpenField, &orders, &mut outputs);
    // World order, and only commands with an option.
    assert_eq!(biases, [(2, OptionId::Hold)]);
    assert_eq!(outputs[0], AiUnitOutput::default(), "the leader is untouched");
    assert!(outputs[1].steer.is_some(), "the held unit steers to its point");
    assert!(outputs[2].steer.is_none() && outputs[2].fire.is_none());
    let mut d = Decision {
        current: Some(OptionId::Push),
        held_ticks: 9,
        challenger: Some(OptionId::Kite),
        challenger_ticks: 2,
    };
    commit_bias(&mut d, OptionId::Hold);
    assert_eq!(
        d,
        Decision {
            current: Some(OptionId::Hold),
            ..Decision::default()
        }
    );
}

// Review proofs (lane det-ai-squads-modebots-review). These fail on c79c9505b.
// Do not weaken them: a fix makes the assertions hold.

/// Two repair pads at the same XZ distance must not change the run when the slice is reversed.
#[test]
fn review_proof_repair_goal_ignores_point_order() {
    let mut unit = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    unit.hull_pct = 0.1;
    let east = [10.0, 0.0, 0.0];
    let west = [-10.0, 0.0, 0.0];
    let east_first = repair_goal(&unit, &[east, west]);
    let west_first = repair_goal(&unit, &[west, east]);
    assert_eq!(
        east_first.map(|g| g.point),
        west_first.map(|g| g.point),
        "reversing two equidistant repair pads changed the goal"
    );
}

/// `TeamDirective::Follow` is documented as following the commander. The commander here is entity 1
/// (`is_commander`); entity 0 is not in the world.
#[test]
fn review_proof_team_follow_escorts_the_commander() {
    let mut commander = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    commander.is_commander = true;
    let wing = view(2, 1, [30.0, 0.0, 0.0], 0.0);
    let world = world(vec![commander, wing.clone()]);
    let followed = plan(
        &wing,
        &WingmanCommand::from_team_directive(TeamDirective::Follow),
        &world,
        0,
    );
    let escorted = plan(&wing, &WingmanCommand::Escort { leader: 1 }, &world, 0);
    assert_eq!(
        followed.steer.map(|g| g.point),
        escorted.steer.map(|g| g.point),
        "Follow escorted missing entity 0 instead of the commander"
    );
}

// ADR-8655 edge cases beyond the review proofs.

/// The ammo run breaks equal distances like the repair run, and a nearer pad still wins over the tie order.
#[test]
fn ammo_goal_ignores_point_order_and_keeps_the_nearest() {
    let mut unit = view(2, 1, [0.0, 0.0, 0.0], 0.0);
    unit.ammo.slots = vec![Some(AmmoSlotView {
        role: WeaponRole::Direct,
        projectile_speed: 120.0,
        ammo: Some((0, 100)),
        band: [0.0, 150.0],
    })];
    let pads = [
        [0.0, 0.0, 10.0],
        [0.0, 0.0, -10.0],
        [10.0, 0.0, 0.0],
        [-10.0, 0.0, 0.0],
    ];
    let mut picks = Vec::new();
    for rot in 0..pads.len() {
        let mut order = pads;
        order.rotate_left(rot);
        picks.push(ammo_goal(&unit, &order).map(|g| g.point));
        order.reverse();
        picks.push(ammo_goal(&unit, &order).map(|g| g.point));
    }
    assert!(picks[0].is_some(), "a dry unit gets an ammo run");
    assert!(picks.iter().all(|p| *p == picks[0]), "{picks:?}");
    let near = [3.0, 0.0, 0.0];
    assert_eq!(
        ammo_goal(&unit, &[pads[0], near, pads[1]]).map(|g| g.point),
        Some(near)
    );
}

/// Attack at will steers to the nearest enemy; at equal distance the lower entity id wins, whatever the world
/// slice order.
#[test]
fn attack_at_will_breaks_equal_distance_on_the_lower_id() {
    let me = view(1, 1, [0.0, 0.0, 0.0], 0.0);
    let east = view(9, 2, [10.0, 0.0, 0.0], 0.0);
    let west = view(4, 2, [-10.0, 0.0, 0.0], 0.0);
    let forward = world(vec![me.clone(), west.clone(), east.clone()]);
    let backward = world(vec![east, me.clone(), west]);
    let a = plan(&me, &WingmanCommand::AttackAtWill, &forward, 0);
    let b = plan(&me, &WingmanCommand::AttackAtWill, &backward, 0);
    assert_eq!(a.steer.map(|g| g.point), Some([-10.0, 0.0, 0.0]));
    assert_eq!(a.steer.map(|g| g.point), b.steer.map(|g| g.point));
}

/// With no commander on the team, the broadcast Follow escorts the lowest-id allied hull (the combat layer's
/// escort leader), never an enemy commander and never the unit itself.
#[test]
fn team_follow_without_a_commander_escorts_the_lowest_id_ally() {
    let mut enemy_commander = view(1, 2, [50.0, 0.0, 50.0], 0.0);
    enemy_commander.is_commander = true;
    let low = view(3, 1, [0.0, 0.0, 0.0], 0.0);
    let high = view(7, 1, [20.0, 0.0, 0.0], 0.0);
    let wing = view(5, 1, [30.0, 0.0, 0.0], 0.0);
    let w = world(vec![enemy_commander, low, wing.clone(), high]);
    let followed = plan(&wing, &WingmanCommand::EscortCommander, &w, 1);
    let escorted = plan(&wing, &WingmanCommand::Escort { leader: 3 }, &w, 1);
    assert_eq!(followed, escorted);
}

/// The commander itself, and a unit with no allied hull, hold where they stand on the broadcast Follow.
#[test]
fn team_follow_holds_for_the_commander_and_a_lone_unit() {
    let mut commander = view(1, 1, [4.0, 0.0, 6.0], 0.0);
    commander.is_commander = true;
    let wing = view(2, 1, [30.0, 0.0, 0.0], 0.0);
    let w = world(vec![commander.clone(), wing]);
    let held = plan(&commander, &WingmanCommand::EscortCommander, &w, 0);
    assert_eq!(held.steer.map(|g| g.point), Some([4.0, 0.0, 6.0]));
    assert_eq!(held.option_bias, Some(OptionId::Hold));

    let lone = view(8, 3, [-5.0, 0.0, 2.0], 0.0);
    let w = world(vec![commander, lone.clone()]);
    let held = plan(&lone, &WingmanCommand::EscortCommander, &w, 0);
    assert_eq!(held.steer.map(|g| g.point), Some([-5.0, 0.0, 2.0]));
    assert_eq!(
        WingmanCommand::EscortCommander.option_bias(),
        Some(OptionId::Escort)
    );
}
