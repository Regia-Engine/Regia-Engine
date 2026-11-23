// SPDX-License-Identifier: MPL-2.0
// Sources: P. Millington, "Game Physics Engine Development" (force generators, explosion force
// fields: a falloff by distance from a detonation point); Kinney & Graham, "Explosive Shocks in
// Air" (scaled-distance blast fits, cited by `ground_shock`). The gameplay law below (radius and
// peak speed from the cube root of the charge, a mass-proportional impulse) is original.

//! One gameplay call that makes an explosion do all of it (ADR-2430).
//!
//! An [`Explosion`] is a point, a radius and a strength. Send it as an [`ExplosionEvent`]
//! (or call [`explode`]) and, on the fixed step before the physics step,
//! [`apply_explosions`]:
//!
//! 1. gives every dynamic rigid body inside the radius an [`regia_physics::ExternalImpulse`]
//!    (linear or inverse-square falloff, proportional to the body's mass, plus a torque from the
//!    offset of the hit point from the center of mass), optionally blocked by line of sight;
//! 2. leaves joints to the solver: the impulse reaches the jointed pair and the existing
//!    `break_force` check removes the joint ([`ExplosionStats::joints_broken`] counts it);
//! 3. forwards one matching `DamageKind::Explosion` to `Destruction` (bodies of a destructible
//!    are skipped by step 1, so a chunk receives the blast once), and pushes the debris already in
//!    flight with the same law as step 1 (ADR-2573);
//! 4. with the `explosion-fx` feature, plays the crash layers at the matching scale and a
//!    ground-shock mount shake at the camera (`fx`);
//! 5. starts a one-shot radial gust for cloth in range.
//!
//! Only an authoritative world applies force to gameplay: a replica (the `Authority` of
//! `Destruction`, or [`ExplosionSettings::authority`] when there is none) plays the effects and
//! pushes its own flying debris, which every process simulates for itself, and nothing else.
//!
//! The system is not part of any default plugin set: the determinism traces do not move until a
//! game adds [`ExplosionPlugin`].

mod apply;
#[cfg(feature = "explosion-fx")]
pub mod fx;
#[cfg(test)]
mod tests;

pub use apply::{
    ExplosionGust, ExplosionPlugin, ExplosionSettings, ExplosionStats, apply_explosions, explode,
};

use regia_app::prelude::*;
use regia_core::math::Vec3;

/// Largest radius an explosion may have (meters).
pub const MAX_RADIUS: f32 = 500.0;
/// Largest charge an explosion may name (kilograms of TNT).
pub const MAX_TNT_KG: f32 = 1_000_000.0;
/// The mass (kg) of the body that receives exactly [`Explosion::impulse`] newton-seconds at the
/// center: lighter bodies get less impulse and the same speed change.
pub const REFERENCE_MASS_KG: f32 = 100.0;
/// No body gains more speed than this from one blast (m/s).
pub const MAX_DELTA_V: f32 = 80.0;
/// Gap kept at the end of an occlusion ray (meters), so a body resting on the wall behind it is
/// not shadowed by the wall it touches.
pub const OCCLUSION_MARGIN: f32 = 0.05;

/// How the blast fades with distance.
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub enum Falloff {
    /// `1 - d / radius`.
    Linear,
    /// `(floor / d)^2` for `d >= floor` (1 closer in), shifted so that it is exactly 0 at the
    /// radius: a blast with a hot center and a long tail, still bounded. `gain` clamps a finite
    /// floor above 0 to at most half the radius, and replaces any other floor with 10 % of the
    /// radius. The stored floor is unchanged; `gameplay.explode`'s result reports that stored
    /// value, not the value `gain` used.
    InverseSquare {
        /// The authored distance inside which the gain stays 1 (meters). `gain` may use a
        /// smaller distance; see the variant docs.
        floor: f32,
    },
}

impl Falloff {
    /// The gain `0..=1` at `distance` from the center for a blast of `radius`. 0 at and beyond
    /// the radius, and for a non-finite input.
    #[must_use]
    pub fn gain(self, distance: f32, radius: f32) -> f32 {
        if !(distance.is_finite() && radius.is_finite()) || radius <= 0.0 || distance >= radius {
            return 0.0;
        }
        let d = distance.max(0.0);
        match self {
            Self::Linear => 1.0 - d / radius,
            Self::InverseSquare { floor } => {
                let floor = if floor.is_finite() && floor > 0.0 {
                    floor.min(radius * 0.5)
                } else {
                    radius * 0.1
                };
                let raw = |x: f32| (floor / x.max(floor)).powi(2);
                let edge = raw(radius);
                ((raw(d) - edge) / (1.0 - edge)).clamp(0.0, 1.0)
            }
        }
    }
}

/// Whether solid geometry shields a body from the blast.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Reflect)]
pub enum Occlusion {
    /// Nothing shields.
    None,
    /// One ray from the center to the body's center of mass; any static or kinematic collider
    /// in between gives the body zero impulse.
    RayCast,
}

/// One explosion.
#[derive(Debug, Clone, Copy, PartialEq, Reflect)]
pub struct Explosion {
    /// Where it goes off (world).
    pub center: Vec3,
    /// TNT-equivalent charge (kg): sizes the visual effects and the camera shake. 0 = no
    /// effects (the physics still uses `radius` and `impulse`).
    pub tnt_kg: f32,
    /// Reach (meters, > 0, at most [`MAX_RADIUS`]).
    pub radius: f32,
    /// Peak impulse (newton-seconds) a [`REFERENCE_MASS_KG`] body receives at the center. A
    /// body of mass `m` gets `m / REFERENCE_MASS_KG` times this; the same number is handed to
    /// `Destruction` as the chunks' blast impulse.
    pub impulse: f32,
    /// Fade with distance.
    pub falloff: Falloff,
    /// `0..=1`: tilts the push direction toward +Y (0 = straight away from the center).
    pub upward_bias: f32,
    /// Line-of-sight test.
    pub occlusion: Occlusion,
    /// Factor on the torque from the hit-point offset (0 = none).
    pub torque: f32,
    /// Seed of the visual effects and the shake.
    pub seed: u64,
    /// An entity that never shields and is not pushed (the thing that blew up).
    pub ignore: Option<Entity>,
    /// Forward a damage to `Destruction`.
    pub destruction: bool,
    /// Start a one-shot gust for cloth in range.
    pub gust: bool,
    /// Play the effects (the `explosion-fx` feature) in addition to the physics.
    pub fx: bool,
}

/// A rejected explosion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ExplosionError {
    /// The center is not finite.
    #[error("the center must be three finite numbers")]
    BadCenter,
    /// The radius is not in `(0, MAX_RADIUS]`.
    #[error("the radius must be finite, above 0 and at most {MAX_RADIUS} m")]
    BadRadius,
    /// The impulse is negative or not finite.
    #[error("the impulse must be finite and not negative")]
    BadImpulse,
    /// The charge is negative, not finite or above [`MAX_TNT_KG`].
    #[error("tnt_kg must be finite, not negative and at most {MAX_TNT_KG} kg")]
    BadTnt,
    /// `upward_bias` is outside `0..=1`.
    #[error("upward_bias must be in 0..=1")]
    BadUpwardBias,
    /// The torque factor is negative or not finite.
    #[error("torque must be finite and not negative")]
    BadTorque,
    /// A preset name that does not exist.
    #[error("unknown preset (grenade, barrel, bomb, shell)")]
    UnknownPreset,
}

impl ExplosionError {
    /// The stable agent error code (lowercase ascii, digits and `_`).
    #[must_use]
    pub fn code(self) -> &'static str {
        match self {
            Self::BadCenter => "bad_center",
            Self::BadRadius => "bad_radius",
            Self::BadImpulse => "bad_impulse",
            Self::BadTnt => "bad_tnt",
            Self::BadUpwardBias => "bad_upward_bias",
            Self::BadTorque => "bad_torque",
            Self::UnknownPreset => "unknown_preset",
        }
    }
}

impl Default for Explosion {
    fn default() -> Self {
        Self::from_tnt(Vec3::ZERO, 1.0)
    }
}

impl Explosion {
    /// An explosion sized from a charge: reach `12 m * cbrt(kg)` (at least 1.5 m), peak speed
    /// change `25 m/s * cbrt(kg)` (at most [`MAX_DELTA_V`]), linear falloff, a 0.2 upward bias,
    /// occlusion on. The radius is not clamped to [`MAX_RADIUS`]. Above about 72 338 kg,
    /// `(12 * cbrt(kg))` passes 500 m while `tnt_kg` is still within [`MAX_TNT_KG`], and
    /// [`Explosion::validate`] then returns [`ExplosionError::BadRadius`]. The agent method
    /// answers `bad_radius` for that charge.
    #[must_use]
    pub fn from_tnt(center: Vec3, tnt_kg: f32) -> Self {
        let kg = if tnt_kg.is_finite() { tnt_kg.max(0.0) } else { 0.0 };
        let c = kg.cbrt();
        Self {
            center,
            tnt_kg,
            radius: (12.0 * c).max(1.5),
            impulse: (25.0 * c).min(MAX_DELTA_V) * REFERENCE_MASS_KG,
            falloff: Falloff::Linear,
            upward_bias: 0.2,
            occlusion: Occlusion::RayCast,
            torque: 1.0,
            seed: 0x6578_706c,
            ignore: None,
            destruction: true,
            gust: true,
            fx: true,
        }
    }

    /// A hand grenade: 0.2 kg.
    #[must_use]
    pub fn grenade(center: Vec3) -> Self {
        Self::from_tnt(center, 0.2)
    }

    /// A fuel or powder barrel: 5 kg, inverse-square falloff with a 1.5 m hot core.
    #[must_use]
    pub fn barrel(center: Vec3) -> Self {
        Self {
            falloff: Falloff::InverseSquare { floor: 1.5 },
            upward_bias: 0.35,
            ..Self::from_tnt(center, 5.0)
        }
    }

    /// An aerial bomb: 250 kg, inverse-square falloff.
    #[must_use]
    pub fn bomb(center: Vec3) -> Self {
        Self {
            falloff: Falloff::InverseSquare { floor: 6.0 },
            upward_bias: 0.15,
            ..Self::from_tnt(center, 250.0)
        }
    }

    /// An artillery shell: 15 kg, linear falloff, a low upward bias.
    #[must_use]
    pub fn shell(center: Vec3) -> Self {
        Self {
            upward_bias: 0.1,
            ..Self::from_tnt(center, 15.0)
        }
    }

    /// The preset of that name (`grenade`, `barrel`, `bomb`, `shell`).
    ///
    /// # Errors
    /// [`ExplosionError::UnknownPreset`].
    pub fn preset(name: &str, center: Vec3) -> Result<Self, ExplosionError> {
        match name {
            "grenade" => Ok(Self::grenade(center)),
            "barrel" => Ok(Self::barrel(center)),
            "bomb" => Ok(Self::bomb(center)),
            "shell" => Ok(Self::shell(center)),
            _ => Err(ExplosionError::UnknownPreset),
        }
    }

    /// The preset names.
    pub const PRESETS: [&'static str; 4] = ["grenade", "barrel", "bomb", "shell"];

    /// Checks every field.
    ///
    /// # Errors
    /// The first [`ExplosionError`] found.
    pub fn validate(&self) -> Result<(), ExplosionError> {
        if !self.center.is_finite() {
            return Err(ExplosionError::BadCenter);
        }
        if !(self.radius.is_finite() && self.radius > 0.0 && self.radius <= MAX_RADIUS) {
            return Err(ExplosionError::BadRadius);
        }
        if !(self.impulse.is_finite() && self.impulse >= 0.0) {
            return Err(ExplosionError::BadImpulse);
        }
        if !(self.tnt_kg.is_finite() && (0.0..=MAX_TNT_KG).contains(&self.tnt_kg)) {
            return Err(ExplosionError::BadTnt);
        }
        if !(self.upward_bias.is_finite() && (0.0..=1.0).contains(&self.upward_bias)) {
            return Err(ExplosionError::BadUpwardBias);
        }
        if !(self.torque.is_finite() && self.torque >= 0.0) {
            return Err(ExplosionError::BadTorque);
        }
        Ok(())
    }

    /// Peak speed change (m/s) of a body at the center: `impulse / REFERENCE_MASS_KG`, at most
    /// [`MAX_DELTA_V`].
    #[must_use]
    pub fn peak_delta_v(&self) -> f32 {
        (self.impulse / REFERENCE_MASS_KG).min(MAX_DELTA_V)
    }

    /// The push direction for a body whose center of mass is at `com`: away from the center,
    /// tilted by [`Self::upward_bias`] (straight up when the body is at the center).
    #[must_use]
    pub fn direction_to(&self, com: Vec3) -> Vec3 {
        let away = (com - self.center).normalize_or(Vec3::Y);
        (away + Vec3::Y * self.upward_bias).normalize_or(Vec3::Y)
    }
}

/// A request to detonate, read by [`apply_explosions`] on the next fixed step.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct ExplosionEvent(pub Explosion);
