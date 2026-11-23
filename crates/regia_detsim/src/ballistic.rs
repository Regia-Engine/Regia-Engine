// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/ballistic.rs, commit d6ed37677), ADR-8012.
// Mark: as-is (every function, constant and test body). Named changes in the tests only: the Earth suit apex
// is `lowg::EARTH_JUMP_APEX_M` (DO27 multiplied two of its on-foot constants to the same 2.25 m), and the check
// that DO27's game state uses the same mortar gravity stays in DO27. ADR-8349: the `_in` wrappers over
// `WorldPhysics` are Regia's (the functions they call are unchanged). ADR-8509: `ArcLauncher` is Regia's; the
// mortar constants and functions are deprecated wrappers over `ArcLauncher::LEGACY_MORTAR`, same arithmetic.

//! One ballistic step for jumps, falls, debris, mortars, grenades and pods (ADR-8079).
//!
//! Gravity is the map's. Air drag is zero in vacuum, so a toss on flat ground lands at the speed it
//! launched. Mode A ([`LowG::Governor`], the default) keeps a suit jump and the mortar inside their
//! Earth envelope. Mode B ([`LowG::Physical`]) does not.

use crate::float::{det_max, det_min};
use crate::math;
use crate::site_physics::{EARTH_G, WorldPhysics};

/// 0: the suit governor and variable charges. 1: true arcs, fixed charges.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LowG {
    /// Mode A: jumps and mortar arcs keep their Earth height and range on every world.
    Governor = 0,
    /// Mode B: true arcs under the map's gravity.
    Physical = 1,
}

impl LowG {
    /// The mode from its wire byte; anything but 1 is [`LowG::Governor`].
    pub fn from_u8(raw: u8) -> Self {
        if raw == 1 { Self::Physical } else { Self::Governor }
    }
}

/// Quadratic drag (1/m) against Earth air. Small enough that a no-drag
/// aim still lands inside a 10 m disc. Vacuum is exactly zero.
pub const DRAG_PER_M: f32 = 0.002;
/// Air density (kg/m³) at which [`DRAG_PER_M`] applies.
pub const DRAG_REF_AIR: f32 = 1.2;
/// The mortar's Earth gravity in its own units. It scales with the map.
#[deprecated(
    since = "0.2.23",
    note = "one weapon's number in engine API (ADR-8509): use ArcLauncher::LEGACY_MORTAR.earth_gravity_units, or a game's own ArcLauncher"
)]
pub const EARTH_MORTAR_G: f32 = ArcLauncher::LEGACY_MORTAR.earth_gravity_units;
/// Arc-shell muzzle speed (m/s).
#[deprecated(
    since = "0.2.23",
    note = "one weapon's number in engine API (ADR-8509): use ArcLauncher::LEGACY_MORTAR.muzzle_m_s, or a game's own ArcLauncher"
)]
pub const MORTAR_MUZZLE_M_S: f32 = ArcLauncher::LEGACY_MORTAR.muzzle_m_s;

/// A launcher that fires a fixed-shape arc (ADR-8509): a muzzle speed split into a loft share (up)
/// and a forward share, under a gravity of its own units that scales with the world's.
///
/// It is the engine's form of the old mortar functions: [`ArcLauncher::LEGACY_MORTAR`] holds their
/// numbers, and [`mortar_range_m`], [`mortar_gravity`] and [`mortar_range_in`] are deprecated
/// wrappers over it, bit for bit. A game passes its own launchers.
#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct ArcLauncher {
    /// Muzzle speed at Earth gravity, in the launcher's units per second.
    pub muzzle_m_s: f32,
    /// The arc's gravity on Earth, in the launcher's units ([`ArcLauncher::gravity`] scales it by
    /// the world's gravity over [`EARTH_G`]).
    pub earth_gravity_units: f32,
    /// Share of the muzzle speed that goes up at pitch 0.
    pub loft_share: f32,
    /// Share of the muzzle speed that goes forward at pitch 0.
    pub forward_share: f32,
}

impl ArcLauncher {
    /// The numbers of the old `mortar_*` functions: 120 per s muzzle speed, 32 units of Earth
    /// gravity, 40 % loft and 90 % forward.
    pub const LEGACY_MORTAR: ArcLauncher = ArcLauncher {
        muzzle_m_s: 120.0,
        earth_gravity_units: 32.0,
        loft_share: 0.4,
        forward_share: 0.9,
    };

    /// The charge that keeps the arc inside its Earth range: [`charge_scale`] (the same for every
    /// launcher; 1 in mode B).
    pub fn charge_scale(&self, mode: LowG, g: f32) -> f32 {
        charge_scale(mode, g)
    }

    /// The arc's gravity on a world of gravity `g`: `earth_gravity_units * map_g(g) / EARTH_G`.
    pub fn gravity(&self, g: f32) -> f32 {
        self.earth_gravity_units * (map_g(g) / EARTH_G)
    }

    /// Flat-ground range at pitch 0, no drag: `vx * 2 * vy / gravity` with the charged muzzle speed.
    pub fn range_m(&self, mode: LowG, g: f32) -> f32 {
        let v = self.muzzle_m_s * self.charge_scale(mode, g);
        let gravity = self.gravity(g);
        let vy = v * self.loft_share;
        let vx = v * self.forward_share;
        vx * (2.0 * vy / gravity)
    }

    /// [`ArcLauncher::range_m`] on `world` (its low-g mode and gravity).
    pub fn range_in(&self, world: &WorldPhysics) -> f32 {
        self.range_m(world.low_g, world.gravity_m_s2)
    }
}

/// The map's gravity, or Earth's when `g` is not a positive finite number.
pub fn map_g(g: f32) -> f32 {
    if g.is_finite() && g > 0.0 { g } else { EARTH_G }
}

/// Charge that keeps a fixed-shape arc inside the Earth range. 1 in mode B.
pub fn charge_scale(mode: LowG, g: f32) -> f32 {
    match mode {
        LowG::Governor => math::sqrt(map_g(g) / EARTH_G),
        LowG::Physical => 1.0,
    }
}

/// The mortar's gravity on a world of gravity `g`.
#[deprecated(
    since = "0.2.23",
    note = "one weapon in engine API (ADR-8509): use ArcLauncher::LEGACY_MORTAR.gravity(g), the same bits"
)]
pub fn mortar_gravity(g: f32) -> f32 {
    ArcLauncher::LEGACY_MORTAR.gravity(g)
}

/// Flat-ground range of the arc shell at pitch 0 (40% loft, 90% forward), no drag.
#[deprecated(
    since = "0.2.23",
    note = "one weapon in engine API (ADR-8509): use ArcLauncher::LEGACY_MORTAR.range_m(mode, g), the same bits"
)]
pub fn mortar_range_m(mode: LowG, g: f32) -> f32 {
    ArcLauncher::LEGACY_MORTAR.range_m(mode, g)
}

/// Semi-implicit step. `vel` is m/s, +Y up. Debris, grenades, pods, and jumps use this.
pub fn step_debris(mut vel: [f32; 3], g: f32, air: f32, dt: f32) -> [f32; 3] {
    if !(dt.is_finite() && dt > 0.0) {
        return vel;
    }
    vel[1] -= map_g(g) * dt;
    apply_drag(&mut vel, air, dt);
    vel
}

/// Mortar velocity is displacement per tick. Same gravity and drag as [`step_debris`].
pub fn step_per_tick(vel: [f32; 3], g: f32, air: f32, dt: f32) -> [f32; 3] {
    if !(dt.is_finite() && dt > 0.0) {
        return vel;
    }
    let m_s = [vel[0] / dt, vel[1] / dt, vel[2] / dt];
    let next = step_debris(m_s, g, air, dt);
    [next[0] * dt, next[1] * dt, next[2] * dt]
}

fn apply_drag(vel: &mut [f32; 3], air: f32, dt: f32) {
    if !air.is_finite() || air <= 0.0 {
        return;
    }
    let speed = math::sqrt(vel[0] * vel[0] + vel[1] * vel[1] + vel[2] * vel[2]);
    if speed < 1.0e-6 {
        return;
    }
    let drop = det_min(DRAG_PER_M * (air / DRAG_REF_AIR) * speed * dt, 1.0);
    let keep = 1.0 - drop;
    vel[0] *= keep;
    vel[1] *= keep;
    vel[2] *= keep;
}

/// Integrate a vertical toss until it returns to `y0` on flat ground.
/// Returns `(landing speed, peak height)`; the landing speed is positive downward.
///
/// A bounded tool, not a per-tick call: it flies the whole arc, up to 200,000 steps on a low-g
/// world (the cap; past it the speed at the cap is returned). Call it once per world (gravity, air,
/// tick rate) and cache the result; never per tick or per projectile.
pub fn landing_speed(launch_up: f32, g: f32, air: f32, dt: f32) -> (f32, f32) {
    let mut y = 0.0f32;
    let mut vel = [0.0, launch_up, 0.0];
    let mut peak = 0.0f32;
    // A low-g physical arc can hang for minutes. Cap the loop.
    for _ in 0..200_000 {
        let prev_y = y;
        vel = step_debris(vel, g, air, dt);
        y += vel[1] * dt;
        peak = det_max(peak, y);
        if vel[1] <= 0.0 && y <= 0.0 && prev_y > 0.0 {
            return (det_max(-vel[1], 0.0), peak);
        }
    }
    (det_max(-vel[1], 0.0), peak)
}

/// [`step_debris`] on `world`: its gravity, and its air ([`WorldPhysics::drag_air`], 0 in vacuum).
pub fn step_debris_in(vel: [f32; 3], world: &WorldPhysics, dt: f32) -> [f32; 3] {
    step_debris(vel, world.gravity_m_s2, world.drag_air(), dt)
}

/// [`step_per_tick`] on `world`: its gravity, and its air ([`WorldPhysics::drag_air`]).
pub fn step_per_tick_in(vel: [f32; 3], world: &WorldPhysics, dt: f32) -> [f32; 3] {
    step_per_tick(vel, world.gravity_m_s2, world.drag_air(), dt)
}

/// [`mortar_range_m`] on `world` (its low-g mode and gravity).
#[deprecated(
    since = "0.2.23",
    note = "one weapon in engine API (ADR-8509): use ArcLauncher::LEGACY_MORTAR.range_in(world), the same bits"
)]
pub fn mortar_range_in(world: &WorldPhysics) -> f32 {
    ArcLauncher::LEGACY_MORTAR.range_in(world)
}

/// [`landing_speed`] on `world`. A bounded tool, as [`landing_speed`] is: once per world, never per tick.
pub fn landing_speed_in(launch_up: f32, world: &WorldPhysics, dt: f32) -> (f32, f32) {
    landing_speed(launch_up, world.gravity_m_s2, world.drag_air(), dt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lowg::EARTH_JUMP_APEX_M;
    use crate::tick::tick_dt;

    fn earth_apex() -> f32 {
        EARTH_JUMP_APEX_M
    }

    #[test]
    fn vacuum_launch_and_landing_match_on_every_body() {
        let dt = tick_dt();
        let bodies = [
            ("earth", 9.81_f32),
            ("europa", 1.315),
            ("ganymede", 1.428),
            ("mars", 3.71),
            ("titan", 1.35),
            ("iapetus", 0.223),
            ("enceladus", 0.113),
        ];
        for (name, g) in bodies {
            for mode in [LowG::Governor, LowG::Physical] {
                let apex = match mode {
                    LowG::Governor => earth_apex(),
                    LowG::Physical => earth_apex() * (EARTH_G / g),
                };
                let launch = math::sqrt(2.0 * g * apex);
                let (land, peak) = landing_speed(launch, g, 0.0, dt);
                let tol = det_max(g * dt, launch * 0.02);
                assert!(
                    (land - launch).abs() < tol,
                    "{name} {mode:?} launch {launch} land {land}"
                );
                assert!(
                    (peak - apex).abs() < apex * 0.05 + 0.05,
                    "{name} peak {peak} apex {apex}"
                );
            }
            // Air takes energy out. The landing is slower than the launch.
            let launch = math::sqrt(2.0 * g * earth_apex());
            let (land, _) = landing_speed(launch, g, 1.2, dt);
            assert!(land < launch, "{name} drag land {land} launch {launch}");
        }
    }

    #[test]
    #[expect(
        deprecated,
        reason = "DO27's mortar test keeps calling the deprecated wrappers on purpose: they must keep their bits (ADR-8509)"
    )]
    fn mortar_per_tick_matches_the_metre_step_and_mode_a_holds_the_earth_range() {
        let dt = tick_dt();
        let g = 3.71;
        let per = [0.4, 1.2, -0.2];
        let once = step_per_tick(per, mortar_gravity(g), 0.0, dt);
        let m_s = step_debris(
            [per[0] / dt, per[1] / dt, per[2] / dt],
            mortar_gravity(g),
            0.0,
            dt,
        );
        assert!((once[1] - m_s[1] * dt).abs() < 1.0e-5);
        let earth = mortar_range_m(LowG::Governor, EARTH_G);
        for g in [1.315_f32, 1.428, 3.71, 1.35, 0.223, 0.113] {
            let range = mortar_range_m(LowG::Governor, g);
            assert!(
                (range - earth).abs() < 0.05,
                "governor range {range} vs earth {earth} at {g}"
            );
            let physical = mortar_range_m(LowG::Physical, g);
            let scale = EARTH_G / g;
            assert!(
                (physical / earth - scale).abs() < 0.02,
                "physical {physical} scale {scale}"
            );
        }
        assert!((mortar_gravity(EARTH_G) - EARTH_MORTAR_G).abs() < 1.0e-4);
    }
}
