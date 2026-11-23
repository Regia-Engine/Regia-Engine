// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/site_physics.rs, commit d6ed37677), ADR-8012.
// Mark: adapt. The functions, `EARTH_G`, `TITAN_G`, `THRUST_FLOOR` and the tests are DO27's. Named changes: the
// map-site constant `BASIN_SCAR_G` (a DO27 map name) is gone and its test checks an Earth-like site by value;
// the thrust field is named as in ADR-8012 A-8 (`HoverParams::thrust_accel`). ADR-8349: `TITAN_G` (one body's
// number) is a deprecated alias for one release and the tests carry their own; `WorldPhysics` and its byte form
// are Regia's.

//! A world's physics as one value, [`WorldPhysics`] (ADR-8349), and its gravity as a weight scale
//! for hover jumps and fan load (ADR-8079).
//!
//! Cruise speeds stay authored. Local `g` only rescales the effective weight that a jump impulse
//! and a hover hull's ride-height sink push against.

use core::fmt;

use crate::ballistic::LowG;

/// Earth surface gravity (m/s²). Scales are 1.0 at this value, and [`WorldPhysics::EARTH`] carries
/// it. The hover step's own default gravity is [`crate::hover::GRAVITY`] (9.8), see there.
pub const EARTH_G: f32 = 9.81;

/// Titan surface gravity (m/s²). About 1.35 m/s².
#[deprecated(
    since = "0.2.23",
    note = "one body's number in engine API (ADR-8349): a game keeps its own bodies' gravity in its own data"
)]
pub const TITAN_G: f32 = 1.35;

/// A world's physics, as every deterministic step reads it (ADR-8349): gravity, air, vacuum and the
/// low-g handling mode, instead of loose `f32`s.
///
/// It is a match setting: every peer of a lockstep match must step the same world, so a lobby (or an
/// agent checking two runs) compares the [`WorldPhysics::to_le_bytes`] of the peers' worlds at match
/// start and refuses a peer whose bytes differ. Compare the bytes, not `==`: `PartialEq` follows
/// IEEE (`-0.0 == 0.0`), the step does not have to.
///
/// [`WorldPhysics::validate`] checks a world read from user content (a map file, a settings
/// message); the steps never validate and keep their own guards (a non-finite gravity reads as the
/// step's default, a non-finite air as vacuum).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldPhysics {
    /// Gravity, m/s², downward. Must be above 0: the ballistic step reads a gravity of 0 or less as
    /// Earth's ([`crate::ballistic::map_g`]), so a zero-g world cannot be stated here.
    /// Valid: above 0, at most 1000.
    pub gravity_m_s2: f32,
    /// Air density, kg/m³ (Earth's is 1.2): the drag of ballistic steps and the hover fans' thrust
    /// scale ([`crate::lowg::air_thrust_scale`]).
    /// Valid: 0 to 1000 (`+0.0`; `-0.0` is `negative_zero`).
    pub air_density_kg_m3: f32,
    /// No atmosphere: no drag and no hover thrust, whatever `air_density_kg_m3` says.
    pub vacuum: bool,
    /// The low-g handling mode of jumps and arcs ([`LowG`]).
    pub low_g: LowG,
}

/// Why a [`WorldPhysics`] (or its bytes) is invalid. [`WorldPhysicsErrorKind::code`] is the
/// protocol code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WorldPhysicsErrorKind {
    /// NaN or infinite (`not_finite`).
    NotFinite,
    /// `-0.0` (`negative_zero`): two worlds that step the same must have the same bytes.
    NegativeZero,
    /// Outside the field's documented range (`out_of_range`).
    OutOfRange,
    /// A flag byte that is neither 0 nor 1 (`bad_flag`), from [`WorldPhysics::from_le_bytes`].
    BadFlag,
}

impl WorldPhysicsErrorKind {
    /// Every kind, in declaration order.
    pub const ALL: [WorldPhysicsErrorKind; 4] = [
        WorldPhysicsErrorKind::NotFinite,
        WorldPhysicsErrorKind::NegativeZero,
        WorldPhysicsErrorKind::OutOfRange,
        WorldPhysicsErrorKind::BadFlag,
    ];

    /// The error code: lowercase ascii, documented in `book/agents/det-hover.md`.
    pub const fn code(self) -> &'static str {
        match self {
            WorldPhysicsErrorKind::NotFinite => "not_finite",
            WorldPhysicsErrorKind::NegativeZero => "negative_zero",
            WorldPhysicsErrorKind::OutOfRange => "out_of_range",
            WorldPhysicsErrorKind::BadFlag => "bad_flag",
        }
    }
}

/// The first invalid field of a [`WorldPhysics`], in field order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldPhysicsError {
    /// The field's name (`"gravity_m_s2"`, `"air_density_kg_m3"`, `"vacuum"`, `"low_g"`).
    pub field: &'static str,
    /// What is wrong with it.
    pub kind: WorldPhysicsErrorKind,
    /// The raw bits it held (`f32::to_bits` for a number, the byte for a flag).
    pub bits: u32,
}

impl fmt::Display for WorldPhysicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}: world field `{}` (bits {:#010x})",
            self.kind.code(),
            self.field,
            self.bits
        )
    }
}

impl std::error::Error for WorldPhysicsError {}

/// Largest valid gravity, m/s².
const WORLD_GRAVITY_MAX: f32 = 1000.0;
/// Largest valid air density, kg/m³.
const WORLD_AIR_MAX: f32 = 1000.0;

impl WorldPhysics {
    /// Earth: [`EARTH_G`] (9.81 m/s²), 1.2 kg/m³ of air ([`crate::lowg::EARTH_AIR_KG_M3`]), mode A.
    pub const EARTH: WorldPhysics = WorldPhysics {
        gravity_m_s2: EARTH_G,
        air_density_kg_m3: crate::lowg::EARTH_AIR_KG_M3,
        vacuum: false,
        low_g: LowG::Governor,
    };

    /// Length of [`WorldPhysics::to_le_bytes`].
    pub const BYTES: usize = 10;

    /// `Ok` when gravity is finite and in `(0, 1000]`, the air density is finite, not `-0.0`, and in
    /// `[0, 1000]`; otherwise the first bad field. Call it when a world is loaded, never per tick.
    /// [`WorldPhysics::EARTH`] is valid.
    pub fn validate(&self) -> Result<(), WorldPhysicsError> {
        check_number("gravity_m_s2", self.gravity_m_s2, |g| {
            g > 0.0 && g <= WORLD_GRAVITY_MAX
        })?;
        check_number("air_density_kg_m3", self.air_density_kg_m3, |a| {
            (0.0..=WORLD_AIR_MAX).contains(&a)
        })
    }

    /// The air density a drag step reads: 0 in vacuum, otherwise `air_density_kg_m3`.
    pub fn drag_air(&self) -> f32 {
        if self.vacuum { 0.0 } else { self.air_density_kg_m3 }
    }

    /// The world as 10 bytes, for the lockstep settings handshake: gravity (`f32::to_bits`,
    /// little-endian), air density (the same), `vacuum` (0 or 1), `low_g` (its wire byte, 0 or 1), in
    /// that order. The raw bits, so `-0.0` and every NaN payload stay distinct. A peer whose bytes
    /// differ from the host's must be refused at match start: it would step another world and desync
    /// silently. The layout is fixed; a change to it is a sim-version change (ADR-8019).
    pub fn to_le_bytes(&self) -> [u8; WorldPhysics::BYTES] {
        let mut out = [0_u8; WorldPhysics::BYTES];
        out[0..4].copy_from_slice(&self.gravity_m_s2.to_bits().to_le_bytes());
        out[4..8].copy_from_slice(&self.air_density_kg_m3.to_bits().to_le_bytes());
        out[8] = u8::from(self.vacuum);
        out[9] = self.low_g as u8;
        out
    }

    /// The world [`WorldPhysics::to_le_bytes`] wrote, bit for bit, or why the bytes are not a valid
    /// world: a flag byte other than 0 or 1 is `bad_flag`, and the world must pass
    /// [`WorldPhysics::validate`].
    pub fn from_le_bytes(bytes: [u8; WorldPhysics::BYTES]) -> Result<WorldPhysics, WorldPhysicsError> {
        let word = |at: usize| u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]);
        let flag = |field: &'static str, b: u8| match b {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WorldPhysicsError {
                field,
                kind: WorldPhysicsErrorKind::BadFlag,
                bits: u32::from(b),
            }),
        };
        let world = WorldPhysics {
            gravity_m_s2: f32::from_bits(word(0)),
            air_density_kg_m3: f32::from_bits(word(4)),
            vacuum: flag("vacuum", bytes[8])?,
            low_g: if flag("low_g", bytes[9])? {
                LowG::Physical
            } else {
                LowG::Governor
            },
        };
        world.validate()?;
        Ok(world)
    }
}

impl Default for WorldPhysics {
    fn default() -> Self {
        WorldPhysics::EARTH
    }
}

/// One number's check: finite, not `-0.0`, then `in_range`.
fn check_number(
    field: &'static str,
    v: f32,
    in_range: impl Fn(f32) -> bool,
) -> Result<(), WorldPhysicsError> {
    let kind = if !v.is_finite() {
        Some(WorldPhysicsErrorKind::NotFinite)
    } else if v.to_bits() == (-0.0_f32).to_bits() {
        Some(WorldPhysicsErrorKind::NegativeZero)
    } else if !in_range(v) {
        Some(WorldPhysicsErrorKind::OutOfRange)
    } else {
        None
    };
    match kind {
        Some(kind) => Err(WorldPhysicsError {
            field,
            kind,
            bits: v.to_bits(),
        }),
        None => Ok(()),
    }
}

#[inline]
fn valid_g(g: f32) -> bool {
    g.is_finite() && g > 0.0
}

/// Local gravity relative to Earth: `g / 9.81`.
///
/// Returns `None` when `g` is non-finite or `<= 0`.
#[inline]
pub fn gravity_scale(g: f32) -> Option<f32> {
    if !valid_g(g) {
        return None;
    }
    Some(g / EARTH_G)
}

/// Jump impulse weight scale: `9.81 / g`.
///
/// Lower gravity → larger scale → same authored push hops higher.
/// Returns `None` when `g` is non-finite or `<= 0`.
#[inline]
pub fn jump_impulse_scale(g: f32) -> Option<f32> {
    if !valid_g(g) {
        return None;
    }
    Some(EARTH_G / g)
}

/// Hover fan / ride-height sink load scale: `g / 9.81`.
///
/// Lower gravity → fans work less hard for the same authored ride height.
/// Returns `None` when `g` is non-finite or `<= 0`.
#[inline]
pub fn fan_load_scale(g: f32) -> Option<f32> {
    gravity_scale(g)
}

/// Lowest share of the authored thrust a hover hull keeps on a low-g world. A floor of 0.6 still
/// felt like driving in tar; handling is the same on every world, so the floor is 1.0: low gravity
/// changes the drift, jumps and mortars, never the drive.
pub const THRUST_FLOOR: f32 = 1.0;

/// Thrust scale the sim applies to [`HoverParams::thrust_accel`](crate::hover::HoverParams::thrust_accel):
/// the fan load, never below [`THRUST_FLOOR`], never above 1 for a normal world (heavier worlds keep
/// their authored `>1` load). Pure and deterministic. `None` when `g` is invalid.
#[inline]
pub fn thrust_scale(g: f32) -> Option<f32> {
    fan_load_scale(g).map(|scale| if scale < THRUST_FLOOR { THRUST_FLOOR } else { scale })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Titan surface gravity (m/s²), about 1.35: the body DO27's tests use (ADR-8349 moved it here).
    const TITAN_G: f32 = 1.35;

    #[test]
    fn earth_scales_are_one() {
        assert_eq!(gravity_scale(EARTH_G), Some(1.0));
        assert_eq!(jump_impulse_scale(EARTH_G), Some(1.0));
        assert_eq!(fan_load_scale(EARTH_G), Some(1.0));
    }

    #[test]
    fn an_earth_like_site_g_is_unity() {
        let site_g = 9.81_f32;
        assert_eq!(site_g, EARTH_G);
        assert_eq!(gravity_scale(site_g), Some(1.0));
        assert_eq!(jump_impulse_scale(site_g), Some(1.0));
        assert_eq!(fan_load_scale(site_g), Some(1.0));
    }

    #[test]
    fn titan_g_scales_fan_and_jump() {
        let fan = fan_load_scale(1.35).expect("Titan g is valid");
        let jump = jump_impulse_scale(1.35).expect("Titan g is valid");
        // 1.35/9.81 ≈ 0.1376 → about 0.138; 9.81/1.35 ≈ 7.266 → about 7.27
        assert!((fan - 0.138).abs() < 0.001, "fan_load={fan}");
        assert!((jump - 7.27).abs() < 0.01, "jump_impulse={jump}");
        assert_eq!(gravity_scale(TITAN_G), fan_load_scale(TITAN_G));
    }

    #[test]
    fn rejects_non_finite_and_non_positive() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, 0.0, -1.0] {
            assert!(gravity_scale(bad).is_none(), "gravity_scale({bad})");
            assert!(jump_impulse_scale(bad).is_none(), "jump_impulse_scale({bad})");
            assert!(fan_load_scale(bad).is_none(), "fan_load_scale({bad})");
        }
    }

    #[test]
    fn thrust_floor_keeps_low_g_hulls_responsive() {
        assert_eq!(thrust_scale(EARTH_G), Some(1.0));
        assert_eq!(thrust_scale(TITAN_G), Some(THRUST_FLOOR));
        assert_eq!(thrust_scale(1.315), Some(THRUST_FLOOR));
        let mars = thrust_scale(3.71).expect("Mars g is valid");
        assert_eq!(mars, THRUST_FLOOR);
        assert!(thrust_scale(0.0).is_none());
        // Reference hull: accel 30 m/s2, cruise 40 m/s. Cruise inside 1.5 s on every world.
        for g in [0.1_f32, 1.315, 1.35, 3.71, 9.81] {
            let accel = 30.0 * thrust_scale(g).expect("valid g");
            assert!(40.0 / accel <= 1.5, "g={g}: {} s to cruise", 40.0 / accel);
        }
    }
}
