// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/lowg.rs, commit d6ed37677), ADR-8012.
// Portions copied from Direct Order 27 (ir-sim/src/fps.rs, commit d6ed37677), ADR-8012.
// Portions copied from Direct Order 27 (ir-sim/src/pilot_flight.rs, commit d6ed37677), ADR-8012.
// Mark: adapt. `PlayCheck`, `ArcRow`, `arc_row` and the first test are lowg.rs's (lines 1-111, arithmetic unchanged).
// The functions lowg.rs imported from DO27 game modules come along with their arithmetic unchanged:
// `governed_apex_m`, `jump_velocity` (fps.rs lines 443-464) and `air_thrust_scale` with its three air constants
// (pilot_flight.rs lines 25-31 and 181-187). Named issues: the on-foot unit flag (a DO27 character) is gone (always
// the plain suit), the Earth apex is a setting (`EARTH_JUMP_APEX_M`, DO27's 0.5 m x 4.5), and the map body enum is a
// `vacuum` flag. DO27's second test reads its battleground table (game content); the arithmetic it checks per body
// is kept with the bodies' numbers written in. ADR-8349: the `_in` wrappers over `WorldPhysics` are Regia's.

//! Low-g handling (ADR-8079). Mode A ([`LowG::Governor`]) is the default; mode B
//! ([`LowG::Physical`]) is the true arc.
//!
//! Both modes launch and land under the map's gravity. Mode A holds the Earth jump height and the
//! Earth mortar range. Mode B does not.

use crate::ballistic::{self, LowG};
use crate::float::det_clamp;
use crate::hover::jump_gravity;
use crate::math;
use crate::site_physics::{EARTH_G, WorldPhysics, fan_load_scale, jump_impulse_scale};

/// The default suit's Earth jump apex, metres (a 0.5 m standing jump times a suit factor of 4.5):
/// what [`arc_row`] reads. Mode A keeps the apex on every world; mode B scales it by `9.81 / g`.
///
/// A default, not a rule: [`jump_velocity`], [`governed_apex_m`], [`arc_row_for`] and
/// [`arc_row_in`] take `earth_apex_m`, and a game passes its own suit's apex there (ADR-8349).
pub const EARTH_JUMP_APEX_M: f32 = 2.25;

/// Earth's air density, kg/m³: the air at which hover thrust is 1.
pub const EARTH_AIR_KG_M3: f32 = 1.2;
/// Below this density (kg/m³) a world counts as airless: hover hulls get no thrust.
pub const AIRLESS_KG_M3: f32 = 0.02;
/// Most hover thrust scale thick air gives.
pub const AIR_SCALE_MAX: f32 = 2.0;

/// Thrust scale from air density. Zero in vacuum (`vacuum` is the world's own flag: no atmosphere,
/// so hovers do not fly) and below [`AIRLESS_KG_M3`].
pub fn air_thrust_scale(density_kg_m3: f32, vacuum: bool) -> f32 {
    if vacuum || !(density_kg_m3.is_finite() && density_kg_m3 > AIRLESS_KG_M3) {
        return 0.0;
    }
    det_clamp(density_kg_m3 / EARTH_AIR_KG_M3, 0.0, AIR_SCALE_MAX)
}

/// The apex a suit jump of `earth_apex_m` reaches on a world of gravity `g` with no governor.
pub fn jump_apex_m(earth_apex_m: f32, g: f32) -> f32 {
    earth_apex_m * jump_impulse_scale(g).unwrap_or(1.0)
}

/// Apex the suit aims at. Mode A is the Earth height on every world.
pub fn governed_apex_m(earth_apex_m: f32, g: f32, mode: LowG) -> f32 {
    match mode {
        LowG::Governor => earth_apex_m,
        LowG::Physical => jump_apex_m(earth_apex_m, g),
    }
}

/// Launch speed for [`governed_apex_m`] under the jump-feel gravity floor
/// ([`jump_gravity`]).
pub fn jump_velocity(earth_apex_m: f32, g: f32, mode: LowG) -> f32 {
    let g = if g.is_finite() && g > 0.0 { g } else { EARTH_G };
    math::sqrt(2.0 * jump_gravity(g) * governed_apex_m(earth_apex_m, g, mode))
}

/// Fan load and hover thrust for one body. Independent of the jump mode.
#[derive(Debug, Clone, Copy)]
pub struct PlayCheck {
    /// Fan load scale (`g / 9.81`; 1 for an invalid `g`).
    pub fan: f32,
    /// Hover thrust scale from the air ([`air_thrust_scale`]).
    pub thrust: f32,
}

/// The fan load and hover thrust of a world with gravity `g`, air density `air` and the vacuum flag.
pub fn play_check(g: f32, air: f32, vacuum: bool) -> PlayCheck {
    PlayCheck {
        fan: fan_load_scale(g).unwrap_or(1.0),
        thrust: air_thrust_scale(air, vacuum),
    }
}

/// One mode's jump and the no-drag mortar envelope.
#[derive(Debug, Clone, Copy)]
pub struct ArcRow {
    /// Jump apex, metres.
    pub apex_m: f32,
    /// Time in the air, seconds.
    pub hang_s: f32,
    /// Landing speed (equal to the launch speed), m/s.
    pub landing_m_s: f32,
    /// Flat-ground mortar range, metres.
    pub mortar_range_m: f32,
}

/// [`arc_row_for`] with the default suit jump ([`EARTH_JUMP_APEX_M`]).
pub fn arc_row(mode: LowG, g: f32) -> ArcRow {
    arc_row_for(mode, g, EARTH_JUMP_APEX_M)
}

/// The jump and mortar envelope of `mode` on a world of gravity `g`, for a suit whose Earth jump
/// apex is `earth_apex_m`.
pub fn arc_row_for(mode: LowG, g: f32, earth_apex_m: f32) -> ArcRow {
    let g = if g.is_finite() && g > 0.0 { g } else { EARTH_G };
    let launch = jump_velocity(earth_apex_m, g, mode);
    ArcRow {
        apex_m: governed_apex_m(earth_apex_m, g, mode),
        hang_s: 2.0 * launch / jump_gravity(g),
        landing_m_s: launch,
        mortar_range_m: ballistic::ArcLauncher::LEGACY_MORTAR.range_m(mode, g),
    }
}

/// [`play_check`] on `world`: its gravity, air density and vacuum flag.
pub fn play_check_in(world: &WorldPhysics) -> PlayCheck {
    play_check(world.gravity_m_s2, world.air_density_kg_m3, world.vacuum)
}

/// [`jump_velocity`] on `world` (its gravity and low-g mode), for a suit whose Earth apex is
/// `earth_apex_m`.
pub fn jump_velocity_in(world: &WorldPhysics, earth_apex_m: f32) -> f32 {
    jump_velocity(earth_apex_m, world.gravity_m_s2, world.low_g)
}

/// [`arc_row_for`] on `world` (its gravity and low-g mode), for a suit whose Earth apex is
/// `earth_apex_m`.
pub fn arc_row_in(world: &WorldPhysics, earth_apex_m: f32) -> ArcRow {
    arc_row_for(world.low_g, world.gravity_m_s2, earth_apex_m)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Impacts slower than this do no harm (DO27's on-foot rule, used here as the bound mode A keeps).
    const FALL_SAFE_M_S: f32 = 8.0;

    #[test]
    fn both_modes_conserve_the_jump_and_mode_a_holds_the_earth_envelope() {
        let earth = arc_row(LowG::Governor, EARTH_G);
        assert!((earth.apex_m - 2.25).abs() < 0.01);
        let earth_launch = crate::math::sqrt(2.0 * EARTH_G * 2.25);
        assert!((earth.landing_m_s - earth_launch).abs() < 0.05);
        let bodies = [
            ("europa", 1.315_f32),
            ("ganymede", 1.428),
            ("mars", 3.71),
            ("titan", 1.35),
            ("iapetus", 0.223),
            ("enceladus", 0.113),
        ];
        for (name, g) in bodies {
            let a = arc_row(LowG::Governor, g);
            let b = arc_row(LowG::Physical, g);
            assert!((a.apex_m - 2.25).abs() < 0.02, "{name} A apex {}", a.apex_m);
            assert!(
                (a.landing_m_s - crate::math::sqrt(2.0 * jump_gravity(g) * 2.25)).abs() < 0.02,
                "{name} A land {}",
                a.landing_m_s
            );
            assert!(a.landing_m_s < FALL_SAFE_M_S, "{name} A should not fall-damage");
            assert!(
                (a.mortar_range_m - earth.mortar_range_m).abs() < 0.05,
                "{name} A mortar {}",
                a.mortar_range_m
            );
            let scale = EARTH_G / g;
            assert!(
                (b.apex_m - 2.25 * scale).abs() < 0.2,
                "{name} B apex {}",
                b.apex_m
            );
            assert!(
                (b.landing_m_s - crate::math::sqrt(2.0 * jump_gravity(g) * b.apex_m)).abs() < 0.05,
                "{name} B land {}",
                b.landing_m_s
            );
            assert!(
                (b.hang_s - 2.0 * b.landing_m_s / jump_gravity(g)).abs() < 0.05,
                "{name} B hang {}",
                b.hang_s
            );
            assert!(
                (b.mortar_range_m / earth.mortar_range_m - scale).abs() < 0.02,
                "{name} B mortar {}",
                b.mortar_range_m
            );
            assert!((a.hang_s - 2.0 * a.landing_m_s / jump_gravity(g)).abs() < 0.05);
        }
        let enc = arc_row(LowG::Physical, 0.113);
        assert!((190.0..200.0).contains(&enc.apex_m), "enceladus B {}", enc.apex_m);
        assert!(
            (16.0..17.0).contains(&enc.hang_s),
            "enceladus B hang {}",
            enc.hang_s
        );
        assert!(
            (48.0..49.0).contains(&enc.landing_m_s),
            "enceladus B land {}",
            enc.landing_m_s
        );
        let held = arc_row(LowG::Governor, 0.113);
        assert!(held.hang_s < 2.0, "enceladus A hang {}", held.hang_s);
        assert!(
            held.landing_m_s < FALL_SAFE_M_S,
            "enceladus A land {}",
            held.landing_m_s
        );
    }

    /// DO27 ran this over its battleground table; the bodies' gravity, air and vacuum flag are written
    /// in here, and the expectations per body are DO27's.
    #[test]
    fn worlds_rerun_hover_thrust_and_fan_load() {
        let worlds = [
            ("europa", 1.315_f32, 0.30_f32, false),
            ("mars", 3.71, 0.60, false),
            ("earth", 9.81, 0.99, false),
            ("ganymede", 1.428, 0.35, false),
            ("iapetus", 0.223, 0.0, true),
            ("titan", 1.35, 5.3, false),
            ("enceladus", 0.113, 0.0, true),
        ];
        for (body, g, air, vacuum) in worlds {
            let check = play_check(g, air, vacuum);
            if vacuum {
                assert_eq!(check.thrust, 0.0, "{body}");
            } else if body == "titan" {
                assert_eq!(check.thrust, 2.0, "titan hits the cap");
            } else if body == "earth" {
                assert!((check.thrust - 0.825).abs() < 0.01, "earth {}", check.thrust);
            } else if body == "europa" {
                assert!((check.thrust - 0.25).abs() < 0.001, "{}", check.thrust);
            } else if body == "ganymede" {
                assert!((check.thrust - 0.2917).abs() < 0.01, "{}", check.thrust);
            } else if body == "mars" {
                assert!((check.thrust - 0.5).abs() < 0.001, "{}", check.thrust);
            }
            assert!((check.fan - g / EARTH_G).abs() < 0.001, "{body} fan");
        }
        // A thin atmosphere below the airless floor flies like vacuum.
        assert_eq!(air_thrust_scale(0.01, false), 0.0);
        assert_eq!(air_thrust_scale(f32::NAN, false), 0.0);
    }
}
