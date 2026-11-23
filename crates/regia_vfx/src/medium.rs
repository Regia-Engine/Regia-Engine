// SPDX-License-Identifier: MPL-2.0
//! The level's air for effects: whether fire can burn, smoke can rise and dust can hang, and
//! the gravity debris falls under.
//!
//! A [`VfxMedium`] comes from the level's atmosphere preset and gravity
//! ([`regia_scene::level::LevelSettings`], the same source as `regia_audio::AudioMedium`).
//! Atmosphere-aware effects are the default: [`crate::scene::VfxScene::spawn`] plays
//! `"<name>@airless"` or `"<name>@mars"` when the library has it, and the plugin registers the
//! built-in variants of the showcase explosion and smoke ([`crate::showcase::explosion_in`],
//! [`crate::showcase::smoke_in`]) for the level's medium. A game's own `.vfx.ron` of the same
//! variant name wins.
//!
//! Physics, all public:
//! - **Airless** (Europa, the Moon): no oxygen, so a fireball is only the explosive's own
//!   detonation products, expanding freely and cooling within a few tenths of a second; no
//!   buoyancy and no drag, so ejecta follow ballistic arcs `p = p₀ + v t + ½ g t²` and fall
//!   back (the Apollo lunar-module ascent and the Surveyor/Apollo footage of lunar dust: it
//!   flies in parabolas and settles, it never billows). Europa's surface gravity is
//!   1.315 m/s² (NASA Europa fact sheet).
//! - **Mars**: about 0.6 kPa of CO₂ (0.6 % of Earth's sea-level pressure) and g = 3.71 m/s²
//!   (NASA Mars fact sheet). Little oxygen, so fire dies fast. Thin air is weakly buoyant, but
//!   fine dust (a few µm) settles very slowly, so a dust cloud lingers and drifts with the wind
//!   (rover footage of dust devils and of Ingenuity's rotor dust, NASA/JPL).
//! - **Earth**: the reference look (lit smoke columns, sooty fireballs).

use regia_core::math::Vec3;
use regia_scene::level::{AtmospherePreset, LevelSettings};

/// Europa's surface gravity, m/s² (NASA Europa fact sheet).
pub const EUROPA_GRAVITY: f32 = 1.315;
/// Mars' surface gravity, m/s² (NASA Mars fact sheet).
pub const MARS_GRAVITY: f32 = 3.71;
/// Standard Earth gravity, m/s².
pub const EARTH_GRAVITY: f32 = 9.81;
/// Default near-surface wind on Mars, m/s, when the host gives none: a light breeze of about
/// 1.6 m/s (Viking and InSight measured daytime winds of a few m/s; a light one keeps a dust cloud
/// in view while it drifts).
pub const MARS_DEFAULT_WIND: Vec3 = Vec3::new(1.5, 0.0, 0.5);

/// Which air the level has.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum VfxAtmosphere {
    /// Earth-like air.
    #[default]
    Earth,
    /// Thin CO₂ (Mars).
    Mars,
    /// No air (Europa, the Moon).
    Airless,
}

impl VfxAtmosphere {
    /// From the level's atmosphere preset.
    #[must_use]
    pub fn from_preset(preset: AtmospherePreset) -> Self {
        match preset {
            AtmospherePreset::Earth => Self::Earth,
            AtmospherePreset::Mars => Self::Mars,
            AtmospherePreset::Airless => Self::Airless,
        }
    }

    /// Lower-case name (agent protocol, variant suffix).
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Earth => "earth",
            Self::Mars => "mars",
            Self::Airless => "airless",
        }
    }
}

/// The level's air and gravity, as effects see them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VfxMedium {
    /// Which air.
    pub atmosphere: VfxAtmosphere,
    /// Gravity magnitude, m/s² (debris falls along −Y at this rate).
    pub gravity: f32,
    /// Wind the dust drifts with, m/s (world). Zero when airless.
    pub wind: Vec3,
}

impl Default for VfxMedium {
    fn default() -> Self {
        Self::earth()
    }
}

impl VfxMedium {
    /// Earth air at standard gravity.
    #[must_use]
    pub fn earth() -> Self {
        Self {
            atmosphere: VfxAtmosphere::Earth,
            gravity: EARTH_GRAVITY,
            wind: Vec3::ZERO,
        }
    }

    /// Mars at its surface gravity, with a light default breeze.
    #[must_use]
    pub fn mars() -> Self {
        Self {
            atmosphere: VfxAtmosphere::Mars,
            gravity: MARS_GRAVITY,
            wind: MARS_DEFAULT_WIND,
        }
    }

    /// No air, at `gravity` m/s² (Europa: [`EUROPA_GRAVITY`]).
    #[must_use]
    pub fn airless(gravity: f32) -> Self {
        Self {
            atmosphere: VfxAtmosphere::Airless,
            gravity: sane_gravity(gravity, EUROPA_GRAVITY),
            wind: Vec3::ZERO,
        }
    }

    /// The medium of a level: its atmosphere preset and the magnitude of its gravity.
    #[must_use]
    pub fn from_level(level: &LevelSettings) -> Self {
        let atmosphere = VfxAtmosphere::from_preset(level.atmosphere.preset);
        let g = level.gravity.length();
        match atmosphere {
            VfxAtmosphere::Earth => Self {
                gravity: sane_gravity(g, EARTH_GRAVITY),
                ..Self::earth()
            },
            VfxAtmosphere::Mars => Self {
                gravity: sane_gravity(g, MARS_GRAVITY),
                ..Self::mars()
            },
            VfxAtmosphere::Airless => Self::airless(g),
        }
    }

    /// The variant suffix effects use in this medium (`"@airless"`, `"@mars"`), or `None` on
    /// Earth, whose effects are the base names.
    #[must_use]
    pub fn variant_suffix(&self) -> Option<&'static str> {
        match self.atmosphere {
            VfxAtmosphere::Earth => None,
            VfxAtmosphere::Mars => Some("@mars"),
            VfxAtmosphere::Airless => Some("@airless"),
        }
    }

    /// The name `base` resolves to in this medium (`explosion` → `explosion@airless`).
    #[must_use]
    pub fn variant_name(&self, base: &str) -> Option<String> {
        self.variant_suffix().map(|suffix| format!("{base}{suffix}"))
    }

    /// Gravity as a world acceleration (m/s², −Y).
    #[must_use]
    pub fn gravity_vec(&self) -> (f32, f32, f32) {
        (0.0, -self.gravity, 0.0)
    }
}

fn sane_gravity(g: f32, fallback: f32) -> f32 {
    if g.is_finite() && g > 0.01 { g } else { fallback }
}

/// Ballistic position after `t` seconds from `p0` at velocity `v0` under `gravity` m/s² (−Y):
/// `p₀ + v₀ t + ½ g t²`. The airless presets' debris follows this (no drag).
#[must_use]
pub fn ballistic_position(p0: Vec3, v0: Vec3, gravity: f32, t: f32) -> Vec3 {
    p0 + v0 * t + Vec3::new(0.0, -0.5 * gravity * t * t, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level_presets_pick_the_medium_and_its_gravity() {
        let mut level = LevelSettings::default();
        assert_eq!(VfxMedium::from_level(&level), VfxMedium::earth());
        level.atmosphere.preset = AtmospherePreset::Airless;
        level.gravity = Vec3::new(0.0, -1.315, 0.0);
        let m = VfxMedium::from_level(&level);
        assert_eq!(m.atmosphere, VfxAtmosphere::Airless);
        assert!((m.gravity - 1.315).abs() < 1e-6);
        assert_eq!(m.wind, Vec3::ZERO);
        assert_eq!(m.variant_name("explosion").as_deref(), Some("explosion@airless"));
        level.atmosphere.preset = AtmospherePreset::Mars;
        level.gravity = Vec3::ZERO;
        let m = VfxMedium::from_level(&level);
        assert_eq!(m.atmosphere, VfxAtmosphere::Mars);
        assert!(
            (m.gravity - MARS_GRAVITY).abs() < 1e-6,
            "zero gravity falls back to Mars'"
        );
        assert_eq!(VfxMedium::earth().variant_name("smoke"), None);
    }

    #[test]
    fn ballistic_apex_and_range_match_the_closed_form() {
        // 8 m/s at 45° on Europa: apex v²sin²θ/2g, range v²sin2θ/g.
        let v = 8.0f32;
        let s = std::f32::consts::FRAC_1_SQRT_2;
        let v0 = Vec3::new(v * s, v * s, 0.0);
        let g = EUROPA_GRAVITY;
        let t_apex = v * s / g;
        let apex = ballistic_position(Vec3::ZERO, v0, g, t_apex);
        assert!((apex.y - v * v * 0.5 / (2.0 * g)).abs() < 1e-3);
        let land = ballistic_position(Vec3::ZERO, v0, g, 2.0 * t_apex);
        assert!(land.y.abs() < 1e-3);
        assert!((land.x - v * v / g).abs() < 1e-2);
    }
}
