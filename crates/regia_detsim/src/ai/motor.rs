// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ai/motor.rs, commit c9513623f, tag regia-absorb-4), ADR-8012.
// Mark: adapt (lines 1-420, plan row S-12, ADR-8516). The steering primitives and the hover energy manager are DO27's
// code apart from rustfmt, doc comments and these named changes:
// - the std float `min`, `max` and `clamp` calls go through `regia_detsim::float::det_*` (ADR-8085: the same bits
//   away from a zero or NaN result; lockstep crates deny the std forms); `crate::math` is libm 0.2.16 (DO27's bits);
// - the hover view's fields use the A-8 names (`accel_brake` -> `brake_accel`);
// - the four tuning constants (`STEER_GAIN`, `STEER_DEADBAND`, `ARRIVE_DIST_M`, `PARK_EPS_M`) are public, so a host
//   can read the values the primitives use (same values);
// - `unit.id % 2 == 0` is written `unit.id.is_multiple_of(2)` (the same test; clippy's form);
// - the tests build the unit with `AiUnitView::new` (DO27's game fields and names are dropped).

//! Motor: steering primitives and the hover energy manager.
//!
//! Pure functions from a goal and the unit's view to a [`SteerCommand`]. They output the same three axes the hover
//! step consumes (`throttle`, `strafe`, `steer`) and never write the body yaw. Because a command is only ever a
//! *rate*, aim never fights steering: a hull keeps its movement heading while its guns track a target.
//!
//! - [`arrive`] and [`arrive_moving`]: drive to a goal (or a slot on a moving anchor) and stop, braking instead of
//!   coasting through the park circle.
//! - [`pursue`] ([`Pursuit::Pure`], [`Pursuit::Lead`], [`Pursuit::Lag`]): chase a target, bleeding throttle when the
//!   nose must swing (slow to out-turn).
//! - [`orbit`], [`kite`]: keep the guns on a target while strafing round it or holding a band edge.
//! - [`evade`]: a break turn away from a threat.
//! - [`formation_slot`]: drive to a slot in an anchor's frame.
//! - [`energy_throttle`]: the speed-versus-turn controller, easing to a share of top speed.
//!
//! A hull that is nearly stationary never turns in place (the anti-spin floor: no turn demand below 3 m/s in
//! [`pursue`], [`orbit`] and [`kite`], and a deadband in [`steer_to`]).
//!
//! Determinism: `crate::math` transcendentals and `crate::float` ordering only, no RNG, no wall clock.

use serde::{Deserialize, Serialize};

use super::view::{AiUnitView, HoverView, SteerCommand};
use crate::float::{det_clamp, det_max, det_min};

/// Signed shortest angular difference `b - a`, wrapped to `(-PI, PI]`.
#[inline]
pub fn signed_angle_diff(a: f32, b: f32) -> f32 {
    (b - a + core::f32::consts::PI).rem_euclid(core::f32::consts::TAU) - core::f32::consts::PI
}

/// Bearing (radians) from `from` to `to` in the XZ plane, using the sim's convention that forward is
/// `(sin yaw, cos yaw)`.
#[inline]
pub fn bearing_to(from: [f32; 3], to: [f32; 3]) -> f32 {
    crate::math::atan2(to[0] - from[0], to[2] - from[2])
}

/// Steer gain: angle error (radians) to turn demand before saturation. Below [`STEER_DEADBAND`] the steer is zeroed
/// so a settled hull never oscillates (the anti-spin floor).
pub const STEER_GAIN: f32 = 2.5;
/// Angle error (radians) below which [`steer_to`] commands no turn.
pub const STEER_DEADBAND: f32 = 0.03;
/// Distance (metres) over which [`throttle_arrive`] ramps throttle to zero ahead of the goal.
pub const ARRIVE_DIST_M: f32 = 25.0;
/// Inside this radius (metres) of the goal the hull is parked (throttle and steer 0).
pub const PARK_EPS_M: f32 = 1.0;

/// Turn demand for facing `desired` from `yaw`, `-1..1`, zero within the deadband. Positive = turn right (yaw
/// increases), matching the hover axis.
#[inline]
pub fn steer_to(yaw: f32, desired: f32) -> f32 {
    let err = signed_angle_diff(yaw, desired);
    if err.abs() <= STEER_DEADBAND {
        0.0
    } else {
        det_clamp(err * STEER_GAIN, -1.0, 1.0)
    }
}

/// Forward/back throttle toward `goal`, ramping to zero inside `park_m` and braking to a stop once inside
/// [`PARK_EPS_M`]. Zero when already parked. `hover` is not read (kept for the signature).
#[inline]
pub fn throttle_arrive(pos: [f32; 3], goal: [f32; 3], hover: &HoverView, park_m: f32) -> f32 {
    let dx = goal[0] - pos[0];
    let dz = goal[2] - pos[2];
    let dist = crate::math::sqrt(dx * dx + dz * dz);
    let _ = hover;
    // Within the park circle the hull is parked: throttle 0, and the steer is zeroed separately by the caller once
    // the bearing is aligned (no spin).
    if dist <= det_max(park_m, PARK_EPS_M) {
        return 0.0;
    }
    // Approach at full throttle, easing off over ARRIVE_DIST_M ahead of park.
    det_clamp((dist - park_m) / det_max(ARRIVE_DIST_M, 1.0), 0.0, 1.0)
}

/// Negative throttle when forward speed would coast through the park circle. `desired_fwd` is the speed the slot
/// already wants along the nose (0 for a stopped goal). A wingman matching a leader who just stopped otherwise
/// coasts, throttle 0, on drag alone: tens of metres, through the slot and into the leader. `None` means the
/// caller's own throttle is fine.
fn coast_brake(unit: &AiUnitView, dist_to_park: f32, desired_fwd: f32) -> Option<f32> {
    let (s, c) = (crate::math::sin(unit.yaw), crate::math::cos(unit.yaw));
    let speed_fwd = unit.vel[0] * s + unit.vel[2] * c;
    let excess = speed_fwd - desired_fwd;
    if excess <= 4.0 {
        return None;
    }
    let brake = det_max(unit.hover.brake_accel, 1.0);
    let brake_dist = excess * excess / (2.0 * brake) * 1.3;
    if dist_to_park <= brake_dist {
        Some(-1.0)
    } else {
        None
    }
}

/// Drive to `goal` and stop within `park_m`. A parked hull (inside the park circle) is fully stopped (throttle and
/// steer both zero), so it never turns in place. A hull still carrying speed brakes instead of coasting through the
/// circle.
pub fn arrive(unit: &AiUnitView, goal: [f32; 3], park_m: f32) -> SteerCommand {
    let dx = goal[0] - unit.pos[0];
    let dz = goal[2] - unit.pos[2];
    let dist = crate::math::sqrt(dx * dx + dz * dz);
    let park = det_max(park_m, PARK_EPS_M);
    let dist_to_park = det_max(dist - park, 0.0);
    if let Some(throttle) = coast_brake(unit, dist_to_park, 0.0) {
        let steer = if dist <= park {
            0.0
        } else {
            steer_to(unit.yaw, bearing_to(unit.pos, goal))
        };
        return SteerCommand {
            throttle,
            strafe: 0.0,
            steer,
        };
    }
    if dist <= park {
        return SteerCommand {
            throttle: 0.0,
            strafe: 0.0,
            steer: 0.0,
        };
    }
    let steer = steer_to(unit.yaw, bearing_to(unit.pos, goal));
    let throttle = throttle_arrive(unit.pos, goal, &unit.hover, park_m);
    SteerCommand {
        throttle,
        strafe: 0.0,
        steer,
    }
}

/// Arrive at a goal attached to a moving anchor (a formation slot): a velocity-tracking controller (match the
/// anchor's velocity plus a proportional pull toward the slot), so a wingman closes on a moving leader and then
/// cruises with it, holding its slot instead of stop-and-go. A zero `anchor_vel` is exactly [`arrive`].
pub fn arrive_moving(unit: &AiUnitView, goal: [f32; 3], park_m: f32, anchor_vel: [f32; 3]) -> SteerCommand {
    if anchor_vel == [0.0; 3] {
        return arrive(unit, goal, park_m);
    }
    let dx = goal[0] - unit.pos[0];
    let dz = goal[2] - unit.pos[2];
    let dist = det_max(crate::math::sqrt(dx * dx + dz * dz), 1.0e-3);
    // Desired velocity: the anchor's velocity plus a proportional pull toward the slot. At the slot the pull is
    // zero and the hull cruises at the anchor's speed; far away it closes at full throttle.
    const GAIN: f32 = 2.0; // 1/s
    let pull = (dist - park_m) * GAIN;
    let vx = anchor_vel[0] + dx / dist * pull;
    let vz = anchor_vel[2] + dz / dist * pull;
    let speed = crate::math::sqrt(vx * vx + vz * vz);
    let desired = crate::math::atan2(vx, vz);
    let steer = steer_to(unit.yaw, desired);
    let (s, c) = (crate::math::sin(unit.yaw), crate::math::cos(unit.yaw));
    let desired_fwd = vx * s + vz * c;
    let park = det_max(park_m, PARK_EPS_M);
    let dist_to_park = det_max(dist - park, 0.0);
    if let Some(throttle) = coast_brake(unit, dist_to_park, desired_fwd) {
        return SteerCommand {
            throttle,
            strafe: 0.0,
            steer,
        };
    }
    let throttle = det_clamp(speed / det_max(unit.hover.top_speed, 1.0), -1.0, 1.0);
    SteerCommand {
        throttle,
        strafe: 0.0,
        steer,
    }
}

/// Pursuit mode. `Lead` points at the target's predicted position after `lead_time`; `Lag` points behind the target
/// so the pursuer turns inside it (the hover-dogfight staple); `Pure` aims straight at the current position (a
/// straight-line chaser has only this).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pursuit {
    /// Aim at the target's current position.
    Pure,
    /// Aim at the target's position `lead_time` seconds ahead.
    Lead,
    /// Aim behind the target, along its velocity.
    Lag,
}

/// Aim point for a pursuit mode, in the XZ plane (Y copied from the target).
fn pursuit_point(target_pos: [f32; 3], target_vel: [f32; 3], lead_time: f32, mode: Pursuit) -> [f32; 3] {
    match mode {
        Pursuit::Pure => target_pos,
        Pursuit::Lead => [
            target_pos[0] + target_vel[0] * lead_time,
            target_pos[1],
            target_pos[2] + target_vel[2] * lead_time,
        ],
        Pursuit::Lag => {
            // Trail the target's velocity vector so we fall in behind it.
            let (vx, vz) = (target_vel[0], target_vel[2]);
            let spd = crate::math::sqrt(vx * vx + vz * vz);
            if spd > 1.0e-3 {
                let lag = lead_time * spd;
                [
                    target_pos[0] - vx / spd * lag,
                    target_pos[1],
                    target_pos[2] - vz / spd * lag,
                ]
            } else {
                target_pos
            }
        }
    }
}

/// Pursue a target. `lead_time` is the projectile time of flight for `Lead` and the trail time for `Lag`; `Pure`
/// ignores it. Energy management: the throttle eases off when a large turn is needed (slow to out-turn), the
/// signature of a hover dogfight.
pub fn pursue(
    unit: &AiUnitView,
    target_pos: [f32; 3],
    target_vel: [f32; 3],
    mode: Pursuit,
    lead_time: f32,
) -> SteerCommand {
    let aim = pursuit_point(target_pos, target_vel, lead_time, mode);
    let bearing = bearing_to(unit.pos, aim);
    // A pursue against a target on top of (or circling) the hull must not turn it in place: zero the turn demand
    // while nearly stationary, exactly like orbit/kite. The energy throttle still nudges it forward so it can turn
    // once it has steerage.
    let steer = no_pivot(unit, steer_to(unit.yaw, bearing));
    let err = signed_angle_diff(unit.yaw, bearing).abs();
    // Energy: at full speed the hull turns slowest; bleed throttle when we need to swing the nose, keep full
    // throttle when already aligned.
    let energy = det_clamp(1.0 - det_min(err / core::f32::consts::FRAC_PI_2, 1.0), 0.3, 1.0);
    SteerCommand {
        throttle: energy,
        strafe: 0.0,
        steer,
    }
}

/// Orbit `target` at `radius`: keep guns on the target while strafing laterally around it (`dir` = +1 clockwise,
/// -1 counter-clockwise). Throttle adjusts the radius, strafe drives the circle.
pub fn orbit(unit: &AiUnitView, target_pos: [f32; 3], radius: f32, dir: f32) -> SteerCommand {
    let bearing = bearing_to(unit.pos, target_pos);
    let steer = steer_to(unit.yaw, bearing);
    let dx = unit.pos[0] - target_pos[0];
    let dz = unit.pos[2] - target_pos[2];
    let dist = crate::math::sqrt(dx * dx + dz * dz);
    // Radial correction: close when outside the orbit, back off inside it.
    let radial = det_clamp((dist - radius) / det_max(radius, 1.0), -1.0, 1.0);
    let throttle = det_clamp(radial * 0.6, -0.6, 0.8);
    // A hold-range hull that is effectively stationary holds its heading instead of pivoting in place (a parked hull
    // never turns in place). In a packed melee a jammed hull still fires but the body does not spin.
    let steer = no_pivot(unit, steer);
    SteerCommand {
        throttle,
        strafe: det_clamp(dir, -1.0, 1.0),
        steer,
    }
}

/// Keep `band_edge` metres from `target` while firing. Faces the target (so the guns stay on it) and strafes
/// laterally to dodge return fire, throttling back when closed on and forward when out of range. The lateral strafe
/// keeps the hull moving (speed > 0), so a kiting hull never spins in place while it tracks a circling target.
pub fn kite(unit: &AiUnitView, target_pos: [f32; 3], band_edge: f32) -> SteerCommand {
    let bearing = bearing_to(unit.pos, target_pos);
    let steer = steer_to(unit.yaw, bearing);
    let dx = target_pos[0] - unit.pos[0];
    let dz = target_pos[2] - unit.pos[2];
    let dist = crate::math::sqrt(dx * dx + dz * dz);
    let err = dist - band_edge;
    let throttle = det_clamp(err / det_max(band_edge, 1.0), -1.0, 1.0);
    let dir = if unit.id.is_multiple_of(2) { 1.0 } else { -1.0 };
    let steer = no_pivot(unit, steer);
    SteerCommand {
        throttle,
        strafe: dir,
        steer,
    }
}

/// Zero a turn demand while the hull is nearly stationary, so a hold-range hull that is jammed (or parked between
/// options) never spins in place. A moving hull keeps its full steering. Threshold = 3 m/s.
#[inline]
fn no_pivot(unit: &AiUnitView, steer: f32) -> f32 {
    let spd2 = unit.vel[0] * unit.vel[0] + unit.vel[2] * unit.vel[2];
    if spd2 < 9.0 { 0.0 } else { steer }
}

/// Break turn away from `threat`: full throttle and the hardest turn toward the side away from the threat's
/// bearing, to spoil the firing solution.
pub fn evade(unit: &AiUnitView, threat: [f32; 3]) -> SteerCommand {
    let bearing = bearing_to(unit.pos, threat);
    let err = signed_angle_diff(unit.yaw, bearing);
    // Turn hard away from the threat (the far side of our facing).
    let away = bearing
        + if err >= 0.0 {
            core::f32::consts::FRAC_PI_2
        } else {
            -core::f32::consts::FRAC_PI_2
        };
    let steer = steer_to(unit.yaw, away);
    SteerCommand {
        throttle: 1.0,
        strafe: 0.0,
        steer,
    }
}

/// Drive to a formation slot: `anchor` plus `right`/`forward` metres in the anchor's frame (forward =
/// `(sin yaw, cos yaw)`, right = `(cos yaw, -sin yaw)`), parking within 2 m.
pub fn formation_slot(
    unit: &AiUnitView,
    anchor: [f32; 3],
    anchor_yaw: f32,
    right: f32,
    forward: f32,
) -> SteerCommand {
    let (s, c) = (crate::math::sin(anchor_yaw), crate::math::cos(anchor_yaw));
    let goal = [
        anchor[0] + s * forward + c * right,
        anchor[1],
        anchor[2] + c * forward - s * right,
    ];
    arrive(unit, goal, 2.0)
}

/// Explicit speed-versus-turn controller (the energy manager). `desired_frac` is a share of top speed (capped to
/// `-1..1`); returns a throttle that eases the hull to it without overshooting the cap.
pub fn energy_throttle(unit: &AiUnitView, desired_frac: f32) -> f32 {
    let fwd = unit.vel[0] * crate::math::sin(unit.yaw) + unit.vel[2] * crate::math::cos(unit.yaw);
    let top = det_max(unit.hover.top_speed, 1.0);
    let want = det_clamp(desired_frac, -1.0, 1.0) * top;
    det_clamp((want - fwd) / top, -1.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit(yaw: f32, pos: [f32; 3]) -> AiUnitView {
        let mut u = AiUnitView::new(1, 1, pos);
        u.yaw = yaw;
        u.hover = HoverView {
            top_speed: 40.0,
            yaw_rate_at_speed: 1.5,
            brake_accel: 20.0,
            turn_bleed: 0.3,
        };
        u
    }

    #[test]
    fn signed_angle_diff_wraps() {
        assert!((signed_angle_diff(0.0, 1.0) - 1.0).abs() < 1e-5);
        assert!((signed_angle_diff(1.0, 0.0) + 1.0).abs() < 1e-5);
        // Crossing the +-PI seam stays in (-PI, PI].
        let d = signed_angle_diff(3.0, -3.0);
        assert!(d.abs() <= core::f32::consts::PI);
    }

    #[test]
    fn steer_is_zero_when_aligned_and_saturates_when_opposed() {
        let u = unit(0.0, [0.0, 0.0, 0.0]);
        assert_eq!(steer_to(u.yaw, 0.0), 0.0, "aligned: no turn (no spin)");
        assert_eq!(steer_to(0.0, core::f32::consts::FRAC_PI_2), 1.0, "turn right");
        assert_eq!(steer_to(0.0, -core::f32::consts::FRAC_PI_2), -1.0, "turn left");
    }

    #[test]
    fn arrive_brakes_near_the_goal_and_stops() {
        let far = unit(0.0, [0.0, 0.0, -100.0]);
        let goal = [0.0, 0.0, 0.0];
        let cmd = arrive(&far, goal, 5.0);
        assert_eq!(cmd.throttle, 1.0, "far: full throttle");
        assert_eq!(cmd.steer, 0.0, "already facing +Z (goal bearing 0)");

        let at_goal = unit(0.0, [0.0, 0.0, 2.0]);
        let cmd = arrive(&at_goal, goal, 5.0);
        assert_eq!(cmd.throttle, 0.0, "inside park: parked");
        assert_eq!(cmd.steer, 0.0, "parked: no turn (no spin)");

        // Still carrying cruise speed inside the park circle: brake, or the hull coasts through the slot.
        let mut fast = unit(0.0, [0.0, 0.0, 2.0]);
        fast.vel = [0.0, 0.0, 25.0];
        let cmd = arrive(&fast, goal, 5.0);
        assert_eq!(cmd.throttle, -1.0, "residual speed inside the park circle brakes");
    }

    #[test]
    fn pursue_lead_points_ahead_of_a_moving_target() {
        // Target crossing at 10 m/s; a lead pursuit aims ahead of it. The pursuer must be moving (speed >= 3 m/s) so
        // the anti-spin `no_pivot` lets it turn: a stationary hull holding a lead point is exactly the spin in place
        // the floor prevents.
        let mut u = unit(0.0, [0.0, 0.0, 0.0]);
        u.vel = [0.0, 0.0, 20.0]; // already translating
        let target = [0.0, 0.0, 100.0];
        let vel = [10.0, 0.0, 0.0]; // crossing +X
        let cmd = pursue(&u, target, vel, Pursuit::Lead, 1.0);
        // The aim point is offset toward +X, so the steer turns toward +X (right).
        assert!(
            cmd.steer > 0.0,
            "lead pursuit turns toward the crossing target, steer={}",
            cmd.steer
        );
    }

    #[test]
    fn evade_turns_away_from_the_threat() {
        // Threat dead ahead (bearing 0). Evade turns hard to one side.
        let u = unit(0.0, [0.0, 0.0, 0.0]);
        let threat = [0.0, 0.0, 50.0];
        let cmd = evade(&u, threat);
        assert_eq!(cmd.throttle, 1.0);
        assert_ne!(cmd.steer, 0.0, "evade commands a break turn");
    }

    /// Not in DO27: a stationary hull never pivots under orbit, kite or pursue, and a moving one keeps its turn.
    #[test]
    fn a_stationary_hull_never_pivots() {
        let target = [40.0, 0.0, 0.0]; // abeam: a full turn demand
        let still = unit(0.0, [0.0; 3]);
        assert_eq!(orbit(&still, target, 30.0, 1.0).steer, 0.0);
        assert_eq!(kite(&still, target, 30.0).steer, 0.0);
        assert_eq!(pursue(&still, target, [0.0; 3], Pursuit::Pure, 0.0).steer, 0.0);
        let mut moving = still.clone();
        moving.vel = [0.0, 0.0, 5.0];
        assert_eq!(orbit(&moving, target, 30.0, 1.0).steer, 1.0);
        assert_eq!(kite(&moving, target, 30.0).steer, 1.0);
    }

    /// Not in DO27: a moving anchor makes the slot controller match its velocity, and a stopped one is `arrive`.
    #[test]
    fn arrive_moving_tracks_the_anchor() {
        let u = unit(0.0, [0.0, 0.0, 0.0]);
        let goal = [0.0, 0.0, 2.0];
        assert_eq!(arrive_moving(&u, goal, 2.5, [0.0; 3]), arrive(&u, goal, 2.5));
        // At the slot with the anchor cruising +Z at 20 m/s: half throttle (20 / 40), no turn.
        let cmd = arrive_moving(&u, [0.0, 0.0, 2.5], 2.5, [0.0, 0.0, 20.0]);
        assert_eq!(cmd.throttle, 0.5);
        assert_eq!(cmd.steer, 0.0);
    }
}
