// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/ai_combat.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 1-805, plan row S-12, ADR-8516). Every DO27 test keeps its scenario, numbers and assertions;
// named changes:
// - DO27's weapon and hull stem tables (game content) are not copied: a slot carries its role, projectile speed and
//   band (S-11's `AmmoSlotView`), and a unit view carries its own `max_hull`, `target_value`, `weapon_dps` and
//   `reup` role. The values are DO27's table entries inlined (a cannon: direct, 120 m/s, 0-150 m, 60 dps; a seeker:
//   200 m/s, 30-200 m, 100 dps; a mortar: 90 m/s, 90-300 m, 80 dps; hull points and target values per test).
// - the fire gate reads S-11's `ShooterView` and `FireTarget` (`AiUnitView::shooter`), and line of sight is an S-10
//   `LineOfSight` (`OpenField` for DO27's `None` terrain);
// - the hover parameters use the neutral names of ADR-8012 A-8 and the engine-authored default hull (ADR-8164) for
//   every field the test does not set;
// - std float methods (`powi`, `sqrt`, `sin`, `cos`, `max`) go through `regia_detsim::math` and
//   `regia_detsim::float` (the lockstep clippy config lints tests too);
// - the `view` helper drops DO27's level argument (every unit the tests read is Hard; DO27's two Medium views feed
//   only the Re-up point, which does not read the level), which keeps it under clippy's argument limit;
// - DO27's game identifiers (hull, weapon and building names) are dropped.
// Not in DO27: `a_far_structure_draws_the_mortar_arc`, `energy_throttle_eases_to_the_wanted_speed`,
// `a_contact_that_left_the_world_view_keeps_the_default_table` and `motor_and_combat_bits_match_the_pinned_hashes`.

//! AI combat acceptance (plan row S-12): the hover energy manager, the fire gate in the combat layer and the mortar
//! arc, deterministically and without a game:
//!
//! - weapon-range discipline (every AI shot is inside a weapon's band; mortars and seekers only in their role);
//! - hit rate: Hard at least twice Easy against a strafing target (same weapons);
//! - a 1v1 hover duel where a strafe-orbit (Hard) driver beats a straight-line pure-pursuit chaser at least 90 % of
//!   the time, with the spin metric at 0;
//! - push and retreat: a unit with a local force advantage pushes, a low-hull or outnumbered unit disengages toward
//!   support;
//! - no one-by-one feeding: a group that pushes together arrives within a short window (spread at most 5 s);
//! - escorts settle in a staggered V clear of a stopped leader, bit for bit the same on every run.

use regia_detsim::ai::{
    ALLY_KEEPOUT_M, AiLevel, AiRng, AiUnitView, AiWorld, AimError, AimParams, AmmoSlotView, AmmoView,
    CoordState, DifficultyProfile, FireTarget, HoverView, KnownEnemy, OpenField, OptionId, Pursuit, ReupRole,
    SightSource, Sighting, SightingsBoard, ThreatEstimate, WeaponRole, aim_error, choose_slot,
    energy_throttle, fire_gate, fire_intent, lead_point, orbit, pursue, reup_needed, reup_point,
    seeker_can_lock, target_priority, unit_output,
};
use regia_detsim::float::det_max;
use regia_detsim::hover::{HoverParams, HoverState, step};
use regia_detsim::math;

const DT: f32 = 1.0 / 60.0;

/// The weapons of these tests (DO27's table entries, inlined).
#[derive(Debug, Clone, Copy, PartialEq)]
enum Weapon {
    /// Direct fire, 120 m/s, 0-150 m, 60 dps.
    Cannon,
    /// Homing, 200 m/s, 30-200 m, 100 dps.
    Seeker,
    /// Lobbed, 90 m/s, 90-300 m, 80 dps.
    Mortar,
}

impl Weapon {
    fn slot(self) -> AmmoSlotView {
        let (role, projectile_speed, band) = match self {
            Weapon::Cannon => (WeaponRole::Direct, 120.0, [0.0, 150.0]),
            Weapon::Seeker => (WeaponRole::Seeker, 200.0, [30.0, 200.0]),
            Weapon::Mortar => (WeaponRole::Mortar, 90.0, [90.0, 300.0]),
        };
        AmmoSlotView {
            role,
            projectile_speed,
            ammo: None,
            band,
        }
    }

    fn dps(self) -> f32 {
        match self {
            Weapon::Cannon => 60.0,
            Weapon::Seeker => 100.0,
            Weapon::Mortar => 80.0,
        }
    }
}

const CANNON_SPEED: f32 = 120.0;

fn ammo(weapons: &[Weapon]) -> AmmoView {
    AmmoView {
        slots: weapons.iter().map(|w| Some(w.slot())).collect(),
    }
}

/// A hull's table entries: hull points and target value (DO27's per-hull numbers, inlined).
#[derive(Debug, Clone, Copy)]
struct Hull {
    max_hull: f32,
    value: f32,
}

/// The heavy hull every test's shooter drives (1000 hull points, value 360).
const HEAVY: Hull = Hull {
    max_hull: 1000.0,
    value: 360.0,
};
/// A light hull (500, value 380).
const LIGHT: Hull = Hull {
    max_hull: 500.0,
    value: 380.0,
};
/// A fast hull (700, value 340).
const FAST: Hull = Hull {
    max_hull: 700.0,
    value: 340.0,
};
/// The team's most valuable structure (900, value 1000).
const HQ: Hull = Hull {
    max_hull: 900.0,
    value: 1000.0,
};

/// A Hard unit (DO27's helper also took the level; every test that reads it uses Hard).
fn view(
    id: u32,
    team: u8,
    hull: Hull,
    pos: [f32; 3],
    yaw: f32,
    vel: [f32; 3],
    weapons: &[Weapon],
) -> AiUnitView {
    let mut u = AiUnitView::new(id, team, pos);
    u.yaw = yaw;
    u.vel = vel;
    u.max_hull = hull.max_hull;
    u.target_value = hull.value;
    u.weapon_dps = weapons.iter().map(|w| w.dps()).sum();
    u.ammo = ammo(weapons);
    u.hover = HoverView {
        top_speed: 40.0,
        yaw_rate_at_speed: 1.5,
        brake_accel: 75.0,
        turn_bleed: 0.3,
    };
    u.level = AiLevel::Hard;
    u.radius = 3.9;
    u
}

fn world(units: Vec<AiUnitView>, tick: u64, seed: u64) -> AiWorld {
    AiWorld {
        tick,
        world_seed: seed,
        units,
    }
}

fn hypot(dx: f32, dz: f32) -> f32 {
    math::sqrt(dx * dx + dz * dz)
}

// ---------------------------------------------------------------------------
// Weapon-range discipline (at least 80 % of shots inside each weapon's band; mortars and seekers only in their role)
// ---------------------------------------------------------------------------

#[test]
fn fire_gate_never_fires_out_of_band() {
    // The fire gate picks the slot from the band table, so every shot it permits is inside that slot's band: by
    // construction, 100 % >= 80 %.
    let u = view(
        1,
        1,
        HEAVY,
        [0.0, 0.0, 0.0],
        0.0,
        [0.0; 3],
        &[Weapon::Cannon, Weapon::Seeker, Weapon::Mortar],
    );
    let aim = AimParams {
        lead_error_scale: 0.2,
        tracking_lag_ticks: 3,
        spread_mrad: 8.0,
    };
    let at = |z: f32| FireTarget {
        id: 9,
        pos: [0.0, 0.0, z],
        vel: [0.0; 3],
        is_unit: true,
        range: z,
    };
    let mut rng = AiRng::new(3);
    // A target at 500 m: outside every band (direct <= 150, seeker <= 200, mortar <= 300), so no shot.
    let far = fire_gate(&u.shooter(), &at(500.0), &aim, &mut rng, OpenField, &[]);
    assert!(far.is_none(), "no shot beyond every weapon's band");
    // In the direct band: a shot, and the chosen slot is the direct cannon.
    let mut rng = AiRng::new(3);
    let close = fire_gate(&u.shooter(), &at(50.0), &aim, &mut rng, OpenField, &[]);
    let fi = close.expect("in-band shot");
    assert_eq!(fi.slot, 0, "direct cannon inside its band");
    // A mortar shot is only ever chosen beyond the mortar minimum range, and a seeker is only ever chosen against a
    // unit.
    let role = |i: usize| u.ammo.slots[i].map(|s| s.role);
    assert_eq!(role(2), Some(WeaponRole::Mortar));
    assert_eq!(role(1), Some(WeaponRole::Seeker));
}

#[test]
fn seekers_never_fire_at_structures() {
    let u = view(
        1,
        1,
        FAST,
        [0.0, 0.0, 0.0],
        0.0,
        [0.0; 3],
        &[Weapon::Cannon, Weapon::Seeker],
    );
    // At 100 m, a unit target may use the seeker; a structure never does.
    let _unit_pick = choose_slot(&u.ammo, 100.0, true);
    let struct_pick = choose_slot(&u.ammo, 100.0, false);
    if let Some(slot) = struct_pick {
        assert_ne!(
            u.ammo.slots[slot].map(|s| s.role),
            Some(WeaponRole::Seeker),
            "a seeker is never chosen against a structure"
        );
    }
    assert!(!seeker_can_lock(false, 100.0, [30.0, 200.0]));
}

// ---------------------------------------------------------------------------
// Hit rate: Hard at least 2x Easy against a strafing target (same weapons)
// ---------------------------------------------------------------------------

/// One shot's flight against a moving target, with a human-like aim error. `lead` controls whether the shooter
/// predicts the target's motion (lead aim) or fires at the current position (a straight-line chaser); the target
/// itself always moves at `target_vel` during the flight. Returns whether the projectile passes within the target's
/// hit radius.
fn shot_hits(
    shooter: [f32; 3],
    target: [f32; 3],
    target_vel: [f32; 3],
    speed: f32,
    aim_error: AimError,
    seed: u64,
    lead: bool,
) -> bool {
    // The aim point: the ideal intercept when leading, else the current spot.
    let predicted = if lead { target_vel } else { [0.0, 0.0, 0.0] };
    let aim_pt = lead_point(shooter, target, predicted, speed);
    // Lead error: a fixed offset in the direction of the target's motion, scaled by the difficulty's lead-error
    // factor.
    let tv = det_max(
        target_vel[0] * target_vel[0] + target_vel[2] * target_vel[2],
        1.0e-6,
    );
    let (vx, vz) = (target_vel[0] / tv, target_vel[2] / tv);
    let mut aim_pt = [
        aim_pt[0] + vx * aim_error.lead,
        aim_pt[1],
        aim_pt[2] + vz * aim_error.lead,
    ];

    // Spread: a cone in mrad. Two independent random offsets in the plane perpendicular to the shot, each up to
    // `spread_mrad/1000 * range`.
    let range = hypot(target[0] - shooter[0], target[2] - shooter[2]);
    let mut rng = AiRng::new(seed);
    let disp = aim_error.spread_mrad / 1000.0 * range;
    let (a, b) = (
        (rng.next_f32() * 2.0 - 1.0) * disp,
        (rng.next_f32() * 2.0 - 1.0) * disp,
    );
    // Perpendicular axes to the shot direction.
    let dx = aim_pt[0] - shooter[0];
    let dz = aim_pt[2] - shooter[2];
    let d = det_max(hypot(dx, dz), 1.0e-6);
    let (ux, uz) = (dx / d, dz / d);
    // Horizontal perpendicular (rotated 90 degrees).
    let (px, pz) = (-uz, ux);
    aim_pt[0] += px * a;
    aim_pt[2] += pz * a;
    aim_pt[1] += b;

    let dx = aim_pt[0] - shooter[0];
    let dz = aim_pt[2] - shooter[2];
    let d = det_max(hypot(dx, dz), 1.0e-6);
    let vel = [dx / d * speed, 0.0, dz / d * speed];

    let mut pp = shooter;
    let mut tp = target;
    const HIT_R: f32 = 2.0;
    for _ in 0..2000 {
        pp[0] += vel[0] * DT;
        pp[2] += vel[2] * DT;
        tp[0] += target_vel[0] * DT;
        tp[2] += target_vel[2] * DT;
        let ex = pp[0] - tp[0];
        let ez = pp[2] - tp[2];
        if (ex * ex + ez * ez) <= HIT_R * HIT_R {
            return true;
        }
        let tx = pp[0] - shooter[0];
        let tz = pp[2] - shooter[2];
        if tx * tx + tz * tz > 300.0 * 300.0 {
            return false;
        }
    }
    false
}

#[test]
fn hard_hits_at_least_twice_easy_against_a_strafing_target() {
    let shooter = [0.0, 0.0, 0.0];
    let target = [0.0, 0.0, 100.0];
    let strafe = [8.0, 0.0, 0.0]; // 8 m/s lateral
    let speed = CANNON_SPEED;
    let easy_aim = DifficultyProfile::easy().aim;
    let hard_aim = DifficultyProfile::hard().aim;
    // The target's travel during the shell's flight: 8 m/s x (100 m / 120 m/s).
    let lead_distance = 8.0 * (100.0 / speed);
    let n = 240;
    let easy_hits = (0..n)
        .filter(|s| {
            let e = aim_error(&easy_aim, &mut AiRng::new(*s as u64), lead_distance);
            shot_hits(shooter, target, strafe, speed, e, *s as u64, true)
        })
        .count();
    let hard_hits = (0..n)
        .filter(|s| {
            let e = aim_error(&hard_aim, &mut AiRng::new(*s as u64), lead_distance);
            shot_hits(shooter, target, strafe, speed, e, *s as u64, true)
        })
        .count();
    assert!(hard_hits > 0, "Hard hits at least some shots");
    assert!(
        hard_hits >= 2 * easy_hits,
        "Hard ({hard_hits}) should hit >= 2x Easy ({easy_hits}) against a strafing target"
    );
}

// ---------------------------------------------------------------------------
// 1v1 hover duel: Hard (strafe orbit + lead) beats a straight-line chaser
// ---------------------------------------------------------------------------

/// True when `target` is within the shooter's forward arc (`half_cos` = the cosine of the half-angle; hover guns
/// fire forward only).
fn in_forward_arc(shooter_pos: [f32; 3], shooter_yaw: f32, target_pos: [f32; 3], half_cos: f32) -> bool {
    let (s, c) = (math::sin(shooter_yaw), math::cos(shooter_yaw));
    let dx = target_pos[0] - shooter_pos[0];
    let dz = target_pos[2] - shooter_pos[2];
    let d = det_max(hypot(dx, dz), 1.0e-6);
    (s * dx + c * dz) / d >= half_cos
}

/// The duel hull's hover parameters (DO27's duel numbers, A-8 names).
fn duel_params() -> HoverParams {
    HoverParams {
        max_forward_speed: 40.0,
        max_reverse_speed: 15.0,
        max_strafe_speed: 20.0,
        thrust_accel: 30.0,
        brake_accel: 75.0,
        thrust_drag: 1.0,
        coast_drag: 4.0,
        yaw_rate_at_rest: 3.45,
        yaw_rate_at_speed: 1.47,
        yaw_response: 1.43,
        yaw_response_at_speed: 3.33,
        ..HoverParams::default()
    }
}

/// The group-push and escort hulls' hover parameters (DO27's numbers, A-8 names).
fn push_params() -> HoverParams {
    HoverParams {
        max_forward_speed: 40.0,
        thrust_accel: 30.0,
        brake_accel: 75.0,
        thrust_drag: 1.0,
        coast_drag: 4.0,
        yaw_rate_at_rest: 3.45,
        yaw_rate_at_speed: 1.47,
        yaw_response: 1.43,
        ..HoverParams::default()
    }
}

/// The duel: the Hard driver strafe-orbits and leads its shots; the chaser is a straight-line pure-pursuit driver
/// at full throttle that fires at the target's *current* position (no lead). Both fire only into their forward arc,
/// so the orbiting driver keeps its guns on the chaser while the chaser's un-led shots miss a laterally moving hull.
fn run_duel(seed: u64) -> bool {
    let params = duel_params();
    let weapons = [Weapon::Cannon];
    let mut hard = view(1, 1, LIGHT, [0.0, 0.0, -60.0], 0.0, [0.0; 3], &weapons);
    let mut chaser = view(
        2,
        2,
        LIGHT,
        [0.0, 0.0, 60.0],
        core::f32::consts::PI,
        [0.0; 3],
        &weapons,
    );
    let mut hard_hs = HoverState {
        yaw: 0.0,
        ..HoverState::default()
    };
    let mut chaser_hs = HoverState {
        yaw: core::f32::consts::PI,
        ..HoverState::default()
    };
    let mut hard_hull = 500.0f32;
    let mut chaser_hull = 500.0f32;
    let mut hard_cd = 0u32;
    let mut chaser_cd = 0u32;
    let speed = CANNON_SPEED;
    // Same small spread for both; lead-vs-no-lead is the differentiator.
    let aim = AimError {
        lead: 0.0,
        tracking_lag_ticks: 0,
        spread_mrad: 10.0,
    };
    const HALF_COS: f32 = 0.5; // +-60 degrees

    for tick in 0..3600u64 {
        hard.pos[1] = 0.0;
        hard.yaw = hard_hs.yaw;
        hard.vel = hard_hs.velocity;
        chaser.pos[1] = 0.0;
        chaser.yaw = chaser_hs.yaw;
        chaser.vel = chaser_hs.velocity;

        // Hard: strafe-orbit (lag pursuit + lateral orbit, the hover dogfight move): it keeps guns on the target
        // while strafing sideways, so the chaser's un-led shots miss a laterally moving hull. Chaser: pure pursuit at
        // full throttle, a straight-line chaser.
        let lead = 100.0 / speed;
        let hard_steer = orbit(&hard, chaser.pos, 50.0, 1.0);
        let chaser_steer = pursue(&chaser, hard.pos, hard.vel, Pursuit::Pure, lead);

        step(
            &params,
            &mut hard_hs,
            &[hard_steer.throttle, hard_steer.strafe, hard_steer.steer],
            0.0,
            DT,
        );
        step(
            &params,
            &mut chaser_hs,
            &[chaser_steer.throttle, chaser_steer.strafe, chaser_steer.steer],
            0.0,
            DT,
        );
        hard.pos[0] += hard_hs.velocity[0] * DT;
        hard.pos[2] += hard_hs.velocity[2] * DT;
        chaser.pos[0] += chaser_hs.velocity[0] * DT;
        chaser.pos[2] += chaser_hs.velocity[2] * DT;

        hard_cd = hard_cd.saturating_sub(1);
        chaser_cd = chaser_cd.saturating_sub(1);
        // Hard: leads the shot (passes the target's velocity).
        if hard_cd == 0 && in_forward_arc(hard.pos, hard_hs.yaw, chaser.pos, HALF_COS) {
            let d = hypot(chaser.pos[0] - hard.pos[0], chaser.pos[2] - hard.pos[2]);
            if d <= 150.0
                && shot_hits(
                    hard.pos,
                    chaser.pos,
                    chaser.vel,
                    speed,
                    aim,
                    seed ^ tick ^ 1,
                    true,
                )
            {
                chaser_hull -= 40.0;
                hard_cd = 12;
            } else if d <= 150.0 {
                hard_cd = 12;
            }
        }
        // Chaser: no lead (fires at the current position).
        if chaser_cd == 0 && in_forward_arc(chaser.pos, chaser_hs.yaw, hard.pos, HALF_COS) {
            let d = hypot(hard.pos[0] - chaser.pos[0], hard.pos[2] - chaser.pos[2]);
            if d <= 150.0 && shot_hits(chaser.pos, hard.pos, hard.vel, speed, aim, seed ^ tick ^ 2, false) {
                hard_hull -= 40.0;
                chaser_cd = 12;
            } else if d <= 150.0 {
                chaser_cd = 12;
            }
        }
        if hard_hull <= 0.0 || chaser_hull <= 0.0 {
            break;
        }
        // Spin metric: neither driver may spin in place.
        let hard_speed = hypot(hard_hs.velocity[0], hard_hs.velocity[2]);
        let chaser_speed = hypot(chaser_hs.velocity[0], chaser_hs.velocity[2]);
        assert!(
            !(hard_hs.yaw_rate.abs() >= 1.0 && hard_speed < 3.0),
            "Hard driver spun in place at tick {tick}"
        );
        assert!(
            !(chaser_hs.yaw_rate.abs() >= 1.0 && chaser_speed < 3.0),
            "chaser spun in place at tick {tick}"
        );
    }
    hard_hull > 0.0 && chaser_hull <= 0.0
}

#[test]
fn hard_lag_pursuit_beats_a_straight_line_chaser() {
    let wins = (1..=20u64).filter(|s| run_duel(*s)).count();
    assert!(
        wins >= 18,
        "lag-pursuit (Hard) should beat a pure chaser >= 90% of seeds, won {wins}/20"
    );
}

// ---------------------------------------------------------------------------
// Push / retreat: force advantage pushes, low hull / outnumbered disengages
// ---------------------------------------------------------------------------

fn coord_with_focus(team: u8, focus: Option<u32>) -> CoordState {
    let mut c = CoordState::default();
    c.set_focus(team, focus);
    c
}

#[test]
fn push_advances_and_retreat_withdraws() {
    let mut units = vec![
        view(1, 1, HEAVY, [0.0, 0.0, -60.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(
            2,
            2,
            HEAVY,
            [0.0, 0.0, 60.0],
            core::f32::consts::PI,
            [0.0; 3],
            &[Weapon::Cannon],
        ),
    ];
    let mut board = SightingsBoard::new(3, 400.0);
    // Seed the board with a direct sighting of each other.
    let seen = || KnownEnemy {
        entity: 0,
        sighting: Sighting {
            entity: 0,
            last_pos: [0.0; 3],
            last_vel: [0.0; 2],
            last_seen_tick: 0,
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
    };
    let mut k1 = seen();
    k1.entity = 2;
    k1.sighting.entity = 2;
    k1.sighting.last_pos = [0.0, 0.0, 60.0];
    let mut k2 = seen();
    k2.entity = 1;
    k2.sighting.entity = 1;
    k2.sighting.last_pos = [0.0, 0.0, -60.0];
    board.by_team[1].push(k1);
    board.by_team[2].push(k2);

    let coord = CoordState::default();
    let w = world(units.clone(), 0, 1);
    let mut rng = AiRng::new(1);

    // Push: unit 1 advances toward the enemy (throttle > 0).
    let out = unit_output(
        &units[0],
        &w,
        &board,
        &coord,
        OpenField,
        Some(OptionId::Push),
        &mut rng,
    );
    assert!(out.steer.expect("Push steers").throttle > 0.0, "Push advances");

    // Retreat: a low-hull unit withdraws (moves away / toward support). With a lone unit there is no support
    // centroid, so it evades (a break turn away).
    units[0].hull_pct = 0.2;
    let w = world(units.clone(), 0, 1);
    let mut rng = AiRng::new(1);
    let out = unit_output(
        &units[0],
        &w,
        &board,
        &coord,
        OpenField,
        Some(OptionId::Retreat),
        &mut rng,
    );
    assert!(out.steer.is_some(), "Retreat still steers (fighting withdrawal)");
}

/// A direct sighting of `id` at `pos`, last seen at `last_seen`, first seen at `first_seen`, with `dps`.
fn sighted(id: u32, pos: [f32; 3], last_seen: u64, first_seen: u64, dps: f32) -> KnownEnemy {
    KnownEnemy {
        entity: id,
        sighting: Sighting {
            entity: id,
            last_pos: pos,
            last_vel: [0.0; 2],
            last_seen_tick: last_seen,
            first_seen_tick: first_seen,
            confidence: 1.0,
            source: SightSource::DirectLos,
        },
        threat: ThreatEstimate {
            dps,
            range_band: [0.0, 150.0],
            can_reach_me: true,
            incoming: true,
        },
    }
}

#[test]
fn target_priority_focuses_the_commander_target_and_values_low_ttk() {
    // A damaged light hull (low time-to-kill) out-scores a distant full-hull HQ structure, but the commander's focus
    // id wins decisively.
    let mut units = vec![
        view(1, 1, HEAVY, [0.0, 0.0, 0.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(10, 2, LIGHT, [0.0, 0.0, 40.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(11, 2, HQ, [0.0, 0.0, 120.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
    ];
    units[1].hull_pct = 0.1; // damaged light hull
    let mut board = SightingsBoard::new(3, 400.0);
    for (id, pos) in [(10, [0.0, 0.0, 40.0]), (11, [0.0, 0.0, 120.0])] {
        board.by_team[1].push(sighted(id, pos, 0, 0, 60.0));
    }
    let w = world(units, 0, 1);
    let coord = CoordState::default();
    let t = target_priority(&w.units[0], &w, &board, &coord).expect("a target");
    // Without a focus, the low-TTK light hull wins over the distant HQ.
    assert_eq!(t.id, 10, "low time-to-kill beats a distant structure");
    // With the focus on the HQ, it wins decisively.
    let coord = coord_with_focus(1, Some(11));
    let t = target_priority(&w.units[0], &w, &board, &coord).expect("a target");
    assert_eq!(t.id, 11, "the commander's focus id wins");
}

// ---------------------------------------------------------------------------
// No one-by-one feeding: a group that pushes together arrives together
// ---------------------------------------------------------------------------

#[test]
fn a_group_push_arrives_within_five_seconds() {
    // Three allies start side by side and push the same target: they arrive at weapon range together (spread << 5
    // s), never trickling in one at a time.
    let params = push_params();
    let starts = [-12.0f32, 0.0, 12.0];
    let mut arrivals = [None; 3];
    for (i, sx) in starts.iter().enumerate() {
        let mut pos = [*sx, 0.0, -140.0];
        let mut hs = HoverState {
            yaw: 0.0,
            ..HoverState::default()
        };
        let target = [0.0, 0.0, 0.0];
        let mut arrival_tick = None;
        for tick in 0..3600u64 {
            let u = view(
                1 + i as u32,
                1,
                HEAVY,
                pos,
                hs.yaw,
                hs.velocity,
                &[Weapon::Cannon],
            );
            let steer = pursue(&u, target, [0.0; 3], Pursuit::Pure, 0.0);
            step(
                &params,
                &mut hs,
                &[steer.throttle, steer.strafe, steer.steer],
                0.0,
                DT,
            );
            pos[0] += hs.velocity[0] * DT;
            pos[2] += hs.velocity[2] * DT;
            let d = hypot(pos[0] - target[0], pos[2] - target[2]);
            if d <= 150.0 {
                arrival_tick = Some(tick);
                break;
            }
        }
        arrivals[i] = arrival_tick;
    }
    let all: Vec<u64> = arrivals
        .iter()
        .map(|a| a.expect("arrives at weapon range"))
        .collect();
    let spread = all.iter().max().copied().unwrap_or(0) - all.iter().min().copied().unwrap_or(0);
    let spread_s = spread as f32 * DT;
    assert!(
        spread_s <= 5.0,
        "group arrival spread {spread_s:.1}s exceeds 5 s (feeding)"
    );
}

// ---------------------------------------------------------------------------
// Escorts form a per-wingman staggered V on the leader, never a shared point (the cockpit-view pile)
// ---------------------------------------------------------------------------

const HEAVY_R: f32 = 3.9;

fn dist2d(a: [f32; 3], b: [f32; 3]) -> f32 {
    hypot(a[0] - b[0], a[2] - b[2])
}

/// One 30 s escort/regroup settle: a stopped commander (the leader) plus ten escorts that start piled on top of it
/// (the bug's worst case). Returns the final positions, leader first. The leader never steers (the player's hull).
fn escort_settle(option: OptionId, seed: u64) -> Vec<[f32; 3]> {
    let params = push_params();
    let board = SightingsBoard::new(3, 400.0);
    let coord = CoordState::default();
    let leader_pos = [0.0, 0.0, 0.0];
    // Ten escorts on a 4 m ring round the leader's centre: well inside every keep-out, so the first thing each must
    // do is break clear.
    let mut escorts: Vec<(HoverState, [f32; 3])> = (0..10usize)
        .map(|i| {
            let a = i as f32 * 0.61;
            (
                HoverState {
                    yaw: 0.0,
                    ..HoverState::default()
                },
                [math::sin(a) * 4.0, 0.0, math::cos(a) * 4.0],
            )
        })
        .collect();

    for tick in 0..1800u64 {
        let mut units = vec![view(1, 1, HEAVY, leader_pos, 0.0, [0.0; 3], &[Weapon::Cannon])];
        units[0].is_commander = true;
        for (i, (hs, pos)) in escorts.iter().enumerate() {
            units.push(view(
                2 + i as u32,
                1,
                HEAVY,
                *pos,
                hs.yaw,
                hs.velocity,
                &[Weapon::Cannon],
            ));
        }
        let w = world(units.clone(), tick, seed);
        // Phase 1 (immutable): every escort's steer from the combat layer.
        let mut steers = Vec::with_capacity(escorts.len());
        for u in &units[1..] {
            let mut rng = AiRng::new(AiRng::mix(seed ^ tick, u.id as u64));
            let out = unit_output(u, &w, &board, &coord, OpenField, Some(option), &mut rng);
            steers.push(out.steer);
        }
        // Phase 2 (mutable): integrate.
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
            escorts[i].1[0] += escorts[i].0.velocity[0] * DT;
            escorts[i].1[2] += escorts[i].0.velocity[2] * DT;
        }
    }

    let mut final_pos = vec![leader_pos];
    final_pos.extend(escorts.iter().map(|(_, p)| *p));
    final_pos
}

#[test]
fn ten_escorts_settle_clear_of_a_stopped_leader() {
    let keepout = 2.0 * HEAVY_R + ALLY_KEEPOUT_M;
    for option in [OptionId::Escort, OptionId::Regroup] {
        let pos = escort_settle(option, 7);
        let leader = pos[0];
        for (i, p) in pos.iter().enumerate().skip(1) {
            let d = dist2d(*p, leader);
            assert!(
                d >= keepout,
                "{option:?} escort {i} parked {d:.1} m from the stopped leader (< {keepout:.1} m keep-out)"
            );
        }
        for i in 1..pos.len() {
            for j in (i + 1)..pos.len() {
                let d = dist2d(pos[i], pos[j]);
                assert!(
                    d >= 2.0 * HEAVY_R,
                    "{option:?} escorts {i}/{j} overlap: {d:.1} m apart (< {} m)",
                    2.0 * HEAVY_R
                );
            }
        }
    }
}

#[test]
fn escort_settle_is_deterministic() {
    for option in [OptionId::Escort, OptionId::Regroup] {
        let a = escort_settle(option, 0xD027);
        let b = escort_settle(option, 0xD027);
        for (pa, pb) in a.iter().zip(b.iter()) {
            assert_eq!(
                pa[0].to_bits(),
                pb[0].to_bits(),
                "{option:?} settle is bit-exact (x)"
            );
            assert_eq!(
                pa[2].to_bits(),
                pb[2].to_bits(),
                "{option:?} settle is bit-exact (z)"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Target priority table, Re-up thresholds, fall-back fire
// ---------------------------------------------------------------------------

fn team1_board(entries: Vec<KnownEnemy>) -> SightingsBoard {
    let mut board = SightingsBoard::new(3, 400.0);
    board.by_team[1].extend(entries);
    board
}

/// A structure of `team` at `pos` with the Re-up role `reup` (DO27's repair and supply buildings).
fn building(id: u32, team: u8, reup: ReupRole, pos: [f32; 3]) -> AiUnitView {
    let mut u = view(id, team, HQ, pos, 0.0, [0.0; 3], &[]);
    u.is_unit = false;
    u.reup = reup;
    u
}

/// A dry finite slot: the Re-up rule reads the worst finite fraction.
fn dry(u: &mut AiUnitView) {
    let mut slot = Weapon::Cannon.slot();
    slot.ammo = Some((0, 100));
    u.ammo.slots = vec![Some(slot)];
}

#[test]
fn target_priority_answers_threats_in_range_first() {
    // A far high-value HQ (250 m: outside the cannon band) must not pull the AI off a near threat it can actually
    // hit (40 m, in band).
    let units = vec![
        view(1, 1, HEAVY, [0.0, 0.0, 0.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(10, 2, FAST, [0.0, 0.0, 40.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(11, 2, HQ, [0.0, 0.0, 250.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
    ];
    let w = world(units, 0, 1);
    let board = team1_board(vec![
        sighted(10, [0.0, 0.0, 40.0], 0, 0, 5.0),
        sighted(11, [0.0, 0.0, 250.0], 0, 0, 60.0),
    ]);
    let t = target_priority(&w.units[0], &w, &board, &CoordState::default()).expect("a target");
    assert_eq!(t.id, 10, "an in-range threat beats a far valuable structure");
}

#[test]
fn target_priority_finishes_damaged_targets() {
    // Two identical fast hulls at equal range: the damaged one is finished first.
    let mut units = vec![
        view(1, 1, HEAVY, [0.0, 0.0, 0.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(10, 2, FAST, [40.0, 0.0, 0.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(11, 2, FAST, [0.0, 0.0, 40.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
    ];
    units[1].hull_pct = 0.1;
    let w = world(units, 0, 1);
    let board = team1_board(vec![
        sighted(10, [40.0, 0.0, 0.0], 0, 0, 60.0),
        sighted(11, [0.0, 0.0, 40.0], 0, 0, 60.0),
    ]);
    let t = target_priority(&w.units[0], &w, &board, &CoordState::default()).expect("a target");
    assert_eq!(t.id, 10, "the damaged target is finished first");
}

#[test]
fn target_priority_ignores_stale_sightings() {
    // Tick 1000, last seen at tick 0: older than the Hard trust window (120 ticks), so there is no target: the AI
    // holds instead of chasing into the fog. A stale focus id does not win either.
    let units = vec![
        view(1, 1, HEAVY, [0.0, 0.0, 0.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(10, 2, FAST, [0.0, 0.0, 40.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
    ];
    let w = world(units, 1000, 1);
    let board = team1_board(vec![sighted(10, [0.0, 0.0, 40.0], 0, 0, 60.0)]);
    assert!(target_priority(&w.units[0], &w, &board, &CoordState::default()).is_none());
    let coord = coord_with_focus(1, Some(10));
    assert!(
        target_priority(&w.units[0], &w, &board, &coord).is_none(),
        "a stale focus is not honoured"
    );
}

#[test]
fn reup_triggers_at_the_documented_thresholds() {
    // Hull at or below 0.30, or any finite slot at or below 0.20, breaks off.
    assert!(!reup_needed(0.31, 1.0), "healthy hull stays on task");
    assert!(reup_needed(0.30, 1.0), "hull at the floor breaks off");
    assert!(!reup_needed(1.0, 0.21), "enough ammo stays on task");
    assert!(reup_needed(1.0, 0.20), "dry ammo breaks off");
    assert!(reup_needed(0.10, 0.10), "both low breaks off");
}

#[test]
fn reup_point_prefers_repair_then_supply() {
    // The repair yard sits farther than the supply depot: low hull still walks to the yard (repair wins),
    // dry-but-healthy walks to the depot.
    let mut me = view(1, 1, HEAVY, [0.0, 0.0, 0.0], 0.0, [0.0; 3], &[Weapon::Cannon]);
    me.hull_pct = 0.2;
    let yard = building(2, 1, ReupRole::Repair, [100.0, 0.0, 0.0]);
    let depot = building(3, 1, ReupRole::Supply, [10.0, 0.0, 0.0]);
    let w = world(vec![me.clone(), yard, depot], 0, 1);
    assert_eq!(
        reup_point(&w.units[0], &w),
        Some([100.0, 0.0, 0.0]),
        "low hull walks to repair"
    );
    me.hull_pct = 1.0;
    dry(&mut me);
    let w = world(
        vec![
            me.clone(),
            building(2, 1, ReupRole::Repair, [100.0, 0.0, 0.0]),
            building(3, 1, ReupRole::Supply, [10.0, 0.0, 0.0]),
        ],
        0,
        1,
    );
    assert_eq!(
        reup_point(&w.units[0], &w),
        Some([10.0, 0.0, 0.0]),
        "dry ammo walks to supply"
    );
    let w = world(vec![me], 0, 1);
    assert_eq!(reup_point(&w.units[0], &w), None, "no Re-up point held");
}

#[test]
fn fall_back_keeps_firing_while_withdrawing() {
    // Retreat with a live trusted target in band: the unit steers AND shoots.
    let units = vec![
        view(1, 1, HEAVY, [0.0, 0.0, 0.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(
            10,
            2,
            FAST,
            [0.0, 0.0, 60.0],
            core::f32::consts::PI,
            [0.0; 3],
            &[Weapon::Cannon],
        ),
    ];
    // Tick 30: reacted (Hard delay 12) and freshly seen (trust 120).
    let w = world(units.clone(), 30, 1);
    let board = team1_board(vec![sighted(10, [0.0, 0.0, 60.0], 30, 0, 60.0)]);
    let mut rng = AiRng::new(1);
    let out = unit_output(
        &units[0],
        &w,
        &board,
        &CoordState::default(),
        OpenField,
        Some(OptionId::Retreat),
        &mut rng,
    );
    assert!(out.steer.is_some(), "a fighting withdrawal still steers");
    assert!(out.fire.is_some(), "falling back keeps firing");
}

#[test]
fn retreat_without_a_target_still_withdraws_to_support() {
    // Empty board, no Re-up point: Retreat walks to the ally centroid instead of stopping in the open, and holds
    // fire with nothing to shoot.
    let units = vec![
        view(1, 1, HEAVY, [0.0, 0.0, 0.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(2, 1, HEAVY, [0.0, 0.0, -50.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
    ];
    let w = world(units.clone(), 0, 1);
    let board = SightingsBoard::new(3, 400.0);
    let mut rng = AiRng::new(1);
    let out = unit_output(
        &units[0],
        &w,
        &board,
        &CoordState::default(),
        OpenField,
        Some(OptionId::Retreat),
        &mut rng,
    );
    assert!(out.steer.is_some(), "target-less retreat still withdraws");
    assert!(out.fire.is_none(), "nothing seen, nothing shot");
}

#[test]
fn low_hull_overrides_push_with_a_reup_withdrawal() {
    // A Push decision at 0.2 hull becomes a withdrawal to the repair yard behind (facing -Z, so throttle > 0 walks
    // to it), still firing at the trusted contact ahead.
    let mut me = view(
        1,
        1,
        HEAVY,
        [0.0, 0.0, 0.0],
        core::f32::consts::PI,
        [0.0; 3],
        &[Weapon::Cannon],
    );
    me.hull_pct = 0.2;
    let yard = building(2, 1, ReupRole::Repair, [0.0, 0.0, -100.0]);
    let foe = view(10, 2, FAST, [0.0, 0.0, 60.0], 0.0, [0.0; 3], &[Weapon::Cannon]);
    let units = vec![me, yard, foe];
    let w = world(units.clone(), 30, 1);
    let board = team1_board(vec![sighted(10, [0.0, 0.0, 60.0], 30, 0, 60.0)]);
    let mut rng = AiRng::new(1);
    let out = unit_output(
        &units[0],
        &w,
        &board,
        &CoordState::default(),
        OpenField,
        Some(OptionId::Push),
        &mut rng,
    );
    let steer = out.steer.expect("the Re-up override steers");
    assert!(
        steer.throttle > 0.0,
        "low hull breaks off Push toward the repair yard, got {steer:?}"
    );
    assert!(out.fire.is_some(), "the withdrawal keeps firing");
}

#[test]
fn fire_intent_refuses_stale_and_unreacted_contacts() {
    let units = vec![
        view(1, 1, HEAVY, [0.0, 0.0, 0.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
        view(10, 2, FAST, [0.0, 0.0, 60.0], 0.0, [0.0; 3], &[Weapon::Cannon]),
    ];
    let coord = CoordState::default();
    // Stale: in band but last seen long ago (tick 1000, trust 120).
    let w = world(units.clone(), 1000, 1);
    let board = team1_board(vec![sighted(10, [0.0, 0.0, 60.0], 0, 0, 60.0)]);
    let mut rng = AiRng::new(1);
    assert!(
        fire_intent(&units[0], &w, &board, &coord, OpenField, &mut rng, None).is_none(),
        "no shots at stale sightings"
    );
    // Fresh but unreacted (tick 5 < Hard delay 12).
    let w = world(units.clone(), 5, 1);
    let board = team1_board(vec![sighted(10, [0.0, 0.0, 60.0], 5, 5, 60.0)]);
    let mut rng = AiRng::new(1);
    assert!(
        fire_intent(&units[0], &w, &board, &coord, OpenField, &mut rng, None).is_none(),
        "no snapshot shots before the reaction delay"
    );
    // Fresh and reacted: shoots.
    let w = world(units.clone(), 30, 1);
    let board = team1_board(vec![sighted(10, [0.0, 0.0, 60.0], 30, 0, 60.0)]);
    let mut rng = AiRng::new(1);
    assert!(
        fire_intent(&units[0], &w, &board, &coord, OpenField, &mut rng, None).is_some(),
        "trusted reacted contact is engaged"
    );
}

// ---------------------------------------------------------------------------
// Not in DO27: the mortar arc, the energy manager, the default table and the pins
// ---------------------------------------------------------------------------

/// A structure beyond the mortar range draws the mortar (the lobbed arc) from a unit holding at its stand-off, and
/// a hull at the same range draws the direct cannon; the stand-off is the far edge of the chosen weapon's band.
#[test]
fn a_far_structure_draws_the_mortar_arc() {
    let shooter = view(
        1,
        1,
        HEAVY,
        [0.0, 0.0, 0.0],
        0.0,
        [0.0; 3],
        &[Weapon::Cannon, Weapon::Mortar],
    );
    let mut site = building(10, 2, ReupRole::None, [0.0, 0.0, 120.0]);
    site.target_value = HQ.value;
    let w = world(vec![shooter.clone(), site], 30, 1);
    let board = team1_board(vec![sighted(10, [0.0, 0.0, 120.0], 30, 0, 0.0)]);
    let mut rng = AiRng::new(1);
    let out = unit_output(
        &shooter,
        &w,
        &board,
        &CoordState::default(),
        OpenField,
        Some(OptionId::Hold),
        &mut rng,
    );
    let fire = out.fire.expect("a structure in the mortar band is shelled");
    assert_eq!(fire.slot, 1, "the mortar lobs at a structure beyond 90 m");
    // Hold parks at 85 % of the mortar band's far edge (255 m): the target is at 120 m, so the unit backs off.
    let steer = out.steer.expect("Hold steers to its stand-off");
    assert!(
        steer.throttle > 0.0 || steer.steer != 0.0,
        "Hold moves to its stand-off: {steer:?}"
    );

    // The same range against a hull: never the mortar.
    let foe = view(10, 2, FAST, [0.0, 0.0, 120.0], 0.0, [0.0; 3], &[Weapon::Cannon]);
    let w = world(vec![shooter.clone(), foe], 30, 1);
    let mut rng = AiRng::new(1);
    let out = unit_output(
        &shooter,
        &w,
        &board,
        &CoordState::default(),
        OpenField,
        Some(OptionId::Hold),
        &mut rng,
    );
    assert_eq!(
        out.fire.map(|f| f.slot),
        Some(0),
        "a hull at 120 m draws the cannon"
    );
}

/// The energy manager eases a hull toward a share of its top speed and never past the cap.
#[test]
fn energy_throttle_eases_to_the_wanted_speed() {
    let mut u = view(1, 1, HEAVY, [0.0; 3], 0.0, [0.0; 3], &[Weapon::Cannon]);
    assert_eq!(energy_throttle(&u, 1.0), 1.0, "from rest, full throttle");
    u.vel = [0.0, 0.0, 20.0];
    assert_eq!(energy_throttle(&u, 0.5), 0.0, "at half of 40 m/s, hold");
    assert!(energy_throttle(&u, 0.25) < 0.0, "above the wanted speed, brake");
    assert_eq!(energy_throttle(&u, 3.0), 0.5, "the wanted share is capped at 1");
}

/// A contact that is on the board but not in the world view scores with the default hull table (500 points, value
/// 100) and counts as a unit, as in DO27 (an empty stem).
#[test]
fn a_contact_that_left_the_world_view_keeps_the_default_table() {
    let me = view(1, 1, HEAVY, [0.0; 3], 0.0, [0.0; 3], &[Weapon::Cannon]);
    let w = world(vec![me], 0, 1);
    let board = team1_board(vec![sighted(40, [0.0, 0.0, 50.0], 0, 0, 30.0)]);
    let t = target_priority(&w.units[0], &w, &board, &CoordState::default()).expect("a target");
    assert_eq!(t.id, 40);
    assert!(t.is_unit);
    assert_eq!(t.max_hull, 500.0);
    assert_eq!(t.value, 100.0);
}

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

/// The motor and combat bits, pinned: a sweep of every motor primitive over a ring of headings and speeds, and the
/// escort settle. A mismatch on another architecture is a cross-arch determinism bug, not a re-pin.
#[test]
fn motor_and_combat_bits_match_the_pinned_hashes() {
    let mut motor_bits = Vec::new();
    for i in 0..24u32 {
        let yaw = i as f32 * 0.29 - 3.0;
        let speed = (i % 6) as f32 * 4.0;
        let mut u = view(
            i,
            1,
            HEAVY,
            [i as f32 * 1.5, 0.0, -(i as f32)],
            yaw,
            [math::sin(yaw) * speed, 0.0, math::cos(yaw) * speed],
            &[Weapon::Cannon],
        );
        u.hover.brake_accel = 20.0;
        let target = [30.0, 0.0, 40.0];
        let tvel = [6.0, 0.0, -3.0];
        for cmd in [
            regia_detsim::ai::arrive(&u, target, 5.0),
            regia_detsim::ai::arrive_moving(&u, target, 3.0, tvel),
            pursue(&u, target, tvel, Pursuit::Lead, 0.8),
            pursue(&u, target, tvel, Pursuit::Lag, 0.8),
            orbit(&u, target, 40.0, 1.0),
            regia_detsim::ai::kite(&u, target, 70.0),
            regia_detsim::ai::evade(&u, target),
            regia_detsim::ai::formation_slot(&u, target, 0.7, 6.0, -8.0),
        ] {
            motor_bits.extend([cmd.throttle, cmd.strafe, cmd.steer]);
        }
        motor_bits.push(energy_throttle(&u, 0.6));
    }
    let settle: Vec<f32> = [OptionId::Escort, OptionId::Regroup]
        .into_iter()
        .flat_map(|o| escort_settle(o, 0xD027))
        .flat_map(|p| [p[0], p[2]])
        .collect();
    let got = [fnv(motor_bits), fnv(settle)];
    println!(
        "regia-ai-combat-pins motor={:#018x} settle={:#018x}",
        got[0], got[1]
    );
    assert_eq!(got, PINS, "motor and combat bits moved");
}

/// Recorded on x86-64 Linux (the build server, Rust 1.99); every platform must reproduce them. Re-pinned for
/// ADR-8876 (`MATH_BITS_VERSION` 1, the canonical `math::atan2`; were `0xd3bc_be84_78b6_eb63`,
/// `0x8719_823c_2696_7cb9`).
const PINS: [u64; 2] = [0x68d9_f94f_ee24_534b, 0x451d_4487_6a53_1945];
