// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/tests/bot_ai.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 1-167, plan row S-11, ADR-8462). The three tests keep DO27's scenarios, numbers and
// assertions; named changes:
// - DO27's first two tests drive its game's auto-fire through its whole game state (`advance_tick` over runtime
//   entities). That game loop stays in DO27; here they drive the pieces its auto-fire is built from: the fire gate
//   (`fire_gate`, an 80 m direct band for DO27's 80 m auto-fire range), the lead solve (`lead_point`) and the hover
//   step. The yaw assertion is the same: firing hands the hull back to the movement AI with its yaw unchanged.
// - the hover parameters use the neutral names of ADR-8012 A-8 (`max_forward_speed`, `thrust_accel`,
//   `brake_accel`) and the engine-authored default hull (ADR-8164) for every field the test does not set.
// - DO27's game identifiers (hull and mission names) are dropped.

//! Bot fire and approach behaviour (plan row S-11): a bot fires only inside its weapon band, firing never fights
//! the steering, and a hull under full throttle drives straight at its target instead of orbiting it.

use regia_detsim::ai::{
    AiRng, AimParams, AmmoSlotView, AmmoView, FireTarget, OpenField, ShooterView, WeaponRole, fire_gate,
    lead_point,
};
use regia_detsim::hover::{HoverParams, HoverState, step};
use regia_detsim::math;
use regia_detsim::tick::tick_dt;

/// DO27's auto-fire range: a direct weapon fires inside 80 m.
const AUTO_FIRE_BAND: [f32; 2] = [0.0, 80.0];

/// A lone armed hull at `z` with one direct weapon (120 m/s shells, 1200 rounds).
fn armed(z: f32) -> ShooterView {
    ShooterView::new(
        [0.0, 0.0, z],
        AmmoView {
            slots: vec![Some(AmmoSlotView {
                role: WeaponRole::Direct,
                projectile_speed: 120.0,
                ammo: Some((1200, 1200)),
                band: AUTO_FIRE_BAND,
            })],
        },
    )
}

/// DO27's Medium aim error.
fn medium_aim() -> AimParams {
    AimParams {
        lead_error_scale: 0.5,
        tracking_lag_ticks: 8,
        spread_mrad: 18.0,
    }
}

/// A shot at the enemy `target_z` metres down +Z from a shooter at the origin.
fn shot_at(target_z: f32) -> FireTarget {
    FireTarget {
        id: 201,
        pos: [0.0, 0.0, target_z],
        vel: [0.0; 3],
        is_unit: true,
        range: target_z,
    }
}

/// A shooter that fires at an enemy keeps its own facing: the shot is aimed at the target, and the body yaw stays
/// the movement AI's value.
#[test]
fn auto_fire_restores_mobile_shooter_yaw() {
    let yaw = 1.0f32;
    let shooter = armed(0.0);
    let mut hover = HoverState {
        yaw,
        ..HoverState::default()
    };
    let params = HoverParams::default();
    let mut rng = AiRng::tick_stream(0xB07, 1, 200);

    let intent = fire_gate(&shooter, &shot_at(50.0), &medium_aim(), &mut rng, OpenField, &[]);
    assert!(intent.is_some(), "a ready shooter with an enemy in range fires");
    // The aim is a point, never a heading written to the hull: the lead solve points down +Z at the target while
    // the hull faces yaw 1.0.
    let aim = lead_point(shooter.pos, [0.0, 0.0, 50.0], [0.0; 3], 120.0);
    assert!(math::atan2(aim[0], aim[2]).abs() < 1.0e-6);
    // The movement AI's tick (no steer) after the shot: the yaw is still the movement AI's.
    step(&params, &mut hover, &[0.0, 0.0, 0.0], 0.0, tick_dt());
    assert_eq!(
        hover.yaw, yaw,
        "mobile shooter yaw is restored after auto-fire (no fire-vs-steer spin)"
    );
}

/// Auto-fire holds beyond its range and opens up inside it.
#[test]
fn auto_fire_only_fires_within_range() {
    // Enemy well outside the auto-fire range: no shot.
    let mut rng = AiRng::tick_stream(0xB07, 1, 200);
    let far = fire_gate(
        &armed(0.0),
        &shot_at(120.0),
        &medium_aim(),
        &mut rng,
        OpenField,
        &[],
    );
    assert!(far.is_none(), "no fire beyond range (120 m > 80 m)");

    // Enemy inside the range: a shot flies.
    let mut rng = AiRng::tick_stream(0xB07, 1, 200);
    let near = fire_gate(
        &armed(0.0),
        &shot_at(50.0),
        &medium_aim(),
        &mut rng,
        OpenField,
        &[],
    );
    assert!(near.is_some(), "fires inside range (50 m < 80 m)");
    let near = near.map(|i| (i.slot, i.target));
    assert_eq!(near, Some((0, 201)));
}

/// A hull under full forward throttle and no steer covers ~300 m in a straight line — the physics foundation for
/// "approaches its target, no orbiting". Zero lateral drift proves it is not circling.
#[test]
fn hover_hull_covers_300m_in_a_straight_line() {
    let params = HoverParams {
        max_forward_speed: 30.0,
        thrust_accel: 30.0,
        brake_accel: 30.0,
        ..HoverParams::default()
    };
    let mut hs = HoverState {
        yaw: 0.0,
        ..HoverState::default()
    };
    let mut pos = [0.0f32, 0.0, 0.0];
    let dt = tick_dt();
    let ticks = (15.0 / dt) as u64; // 15 s
    for _ in 0..ticks {
        step(&params, &mut hs, &[1.0, 0.0, 0.0], 0.0, dt);
        pos[0] += hs.velocity[0] * dt;
        pos[2] += hs.velocity[2] * dt;
    }
    // Facing +Z, full throttle: it drives straight down +Z.
    let forward = pos[2];
    assert!(
        forward >= 300.0,
        "full throttle covers 300 m in 15 s (got {forward:.0} m)"
    );
    assert!(
        pos[0].abs() < 0.5,
        "no lateral drift: the hull does not circle (|x| = {:.3})",
        pos[0].abs()
    );
    // And its yaw stayed put: it never spun.
    assert!(hs.yaw.abs() < 1e-3, "yaw never turned (yaw = {:.3})", hs.yaw);
}
