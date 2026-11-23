// SPDX-License-Identifier: MPL-2.0
//! Ground-borne (structure-borne) sound: impacts and engines shaking the ground, heard through
//! boots, hulls and floors (`docs/specs/16-audio.md` § 3c).
//!
//! One arrival per emitter, straight-line through the ground, from textbook seismology
//! (Aki & Richards, *Quantitative Seismology*, 2nd ed., ch. 5 and 7):
//! - **delay** `r / v`, `v` the material's first-arrival (P) wave speed;
//! - **geometric spreading** `1/√r` for surface (Rayleigh) waves from a source on the surface,
//!   `1/r` for body waves from a deep or buried source, both relative to a source size `r₀`;
//! - **anelastic absorption** `A(f) = exp(−π f r / (Q v))`, applied as a low-pass whose cutoff is
//!   where `A` reaches −3 dB: `f_c = ln √2 · Q v / (π r)` — it falls with range and rises with Q;
//! - a **coda**: the scattered tail after the direct arrival. Its envelope decays as
//!   `exp(−π f t / Q)` (Aki & Chouet 1975, "Origin of coda waves"), so it lasts
//!   `t₆₀ = ln 1000 · Q / (π f)` — long in dry, high-Q rock and regolith (the Apollo seismometers
//!   recorded lunar impacts ringing for tens of minutes: Latham et al. 1970, *Science* 170,
//!   "Passive seismic experiment"), short in wet soil. Clamped to [`MAX_CODA_SECS`] for gameplay.
//!
//! Crossing from one solid into another (terrain → building foundation → floor) transmits the
//! energy fraction `T = 4 Z₁ Z₂ / (Z₁ + Z₂)²` (normal incidence, `Z = ρ v`; Kinsler et al.,
//! *Fundamentals of Acoustics*, 4th ed., § 6.2).

use regia_ecs::prelude::*;
use serde::{Deserialize, Serialize};

use crate::medium::{MAX_CUTOFF_HZ, MIN_CUTOFF_HZ};

/// Longest seismic coda the mixer renders, seconds (real lunar codas last an hour).
pub const MAX_CODA_SECS: f32 = 8.0;
/// Shortest coda, seconds.
pub const MIN_CODA_SECS: f32 = 0.15;
/// Frequency the coda length is quoted at, Hz (the middle of the boot-coupled band).
pub const CODA_REFERENCE_HZ: f32 = 100.0;
/// Frequency the haptic (felt) level is evaluated at, Hz: the band a rumble motor reproduces.
pub const HAPTIC_HZ: f32 = 30.0;

/// A ground (or structure) material: how fast and how far it carries vibration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GroundMaterial {
    /// Name used by levels, terrain layers and physics materials (`"ice"`, `"bedrock"`, …).
    pub name: String,
    /// First-arrival (P) wave speed, m/s.
    pub wave_speed_mps: f32,
    /// Seismic quality factor Q (higher = less absorption, longer ringing).
    pub q: f32,
    /// Density, kg/m³ (acoustic impedance `ρ v` for interface losses).
    pub density_kg_m3: f32,
}

/// `(name, v m/s, Q, ρ kg/m³)` of the built-in materials. Sources: P-wave speeds of soils and
/// rocks from standard engineering-seismology tables (e.g. Telford, Geldart & Sheriff,
/// *Applied Geophysics*, 2nd ed., table 4.4); lunar regolith ~100–300 m/s and Q in the
/// thousands (Latham et al. 1970; Nakamura 1976); Martian near-surface regolith ~120 m/s
/// (Lognonné et al. 2020, *Nature Geoscience* 13, InSight); cold ice Vp ≈ 3.8 km/s (Kohnen 1974).
const PRESETS: &[(&str, f32, f32, f32)] = &[
    ("soil", 600.0, 25.0, 1_700.0),
    ("sand", 300.0, 20.0, 1_600.0),
    ("wet_soil", 1_500.0, 15.0, 1_900.0),
    ("bedrock", 5_000.0, 300.0, 2_700.0),
    ("basalt", 5_500.0, 400.0, 2_900.0),
    ("regolith", 150.0, 2_500.0, 1_500.0),
    ("mars_regolith", 150.0, 500.0, 1_400.0),
    ("airless_rock", 5_000.0, 3_000.0, 2_800.0),
    ("ice", 3_800.0, 200.0, 920.0),
    ("concrete", 3_500.0, 100.0, 2_400.0),
    ("steel", 5_900.0, 1_000.0, 7_850.0),
];

/// A built-in material borrowed from [`PRESETS`]. The propagation hot path folds interfaces
/// through this view so it does not build a [`GroundMaterial`] (and its owned name) per frame.
#[derive(Clone, Copy, Debug)]
pub(crate) struct MaterialView {
    /// Preset name (`"soil"`, `"ice"`, …).
    pub name: &'static str,
    /// First-arrival wave speed, m/s.
    pub wave_speed_mps: f32,
    /// Seismic quality factor Q.
    pub q: f32,
    /// Density, kg/m³.
    pub density_kg_m3: f32,
}

/// The built-in material `name`, if any, without allocating.
#[must_use]
pub(crate) fn material_view(name: &str) -> Option<MaterialView> {
    PRESETS
        .iter()
        .find(|(preset, ..)| *preset == name)
        .map(|&(name, wave_speed_mps, q, density_kg_m3)| MaterialView {
            name,
            wave_speed_mps,
            q,
            density_kg_m3,
        })
}

/// [`material_view`] of `name`, else of `fallback`, else dry soil.
#[must_use]
pub(crate) fn material_view_or(name: &str, fallback: &str) -> MaterialView {
    material_view(name)
        .or_else(|| material_view(fallback))
        .unwrap_or(MaterialView {
            name: "soil",
            wave_speed_mps: 600.0,
            q: 25.0,
            density_kg_m3: 1_700.0,
        })
}

/// Acoustic impedance `ρ v`, rayl.
#[must_use]
pub(crate) fn impedance_of(density_kg_m3: f32, wave_speed_mps: f32) -> f32 {
    density_kg_m3.max(1.0) * wave_speed_mps.max(1.0)
}

/// Amplitude factor `exp(−π f r / (Q v))`.
#[must_use]
pub(crate) fn absorption_of(q: f32, wave_speed_mps: f32, hz: f32, distance: f32) -> f32 {
    let qv = (q * wave_speed_mps).max(1e-3);
    (-std::f32::consts::PI * hz * distance.max(0.0) / qv).exp()
}

/// Low-pass cutoff of the absorption after `distance` metres (−3 dB point), Hz.
#[must_use]
pub(crate) fn absorption_cutoff_of(q: f32, wave_speed_mps: f32, distance: f32) -> f32 {
    let r = distance.max(1e-3);
    let fc = std::f32::consts::LN_2 * 0.5 * q * wave_speed_mps / (std::f32::consts::PI * r);
    fc.clamp(MIN_CUTOFF_HZ, MAX_CUTOFF_HZ)
}

/// Length of the seismic coda (60 dB decay at [`CODA_REFERENCE_HZ`]), seconds, clamped.
#[must_use]
pub(crate) fn coda_secs_of(q: f32) -> f32 {
    let t60 = 1000f32.ln() * q / (std::f32::consts::PI * CODA_REFERENCE_HZ);
    t60.clamp(MIN_CODA_SECS, MAX_CODA_SECS)
}

/// Wet level of the coda (0..1): grows with Q.
#[must_use]
pub(crate) fn coda_mix_of(q: f32) -> f32 {
    let t = ((q.max(10.0) / 10.0).log10() / 300f32.log10()).clamp(0.0, 1.0);
    0.15 + 0.35 * t
}

impl GroundMaterial {
    /// Every built-in material.
    #[must_use]
    pub fn presets() -> Vec<GroundMaterial> {
        PRESETS
            .iter()
            .map(|&(name, v, q, rho)| GroundMaterial {
                name: name.to_owned(),
                wave_speed_mps: v,
                q,
                density_kg_m3: rho,
            })
            .collect()
    }

    /// The built-in material `name`, if any.
    #[must_use]
    pub fn named(name: &str) -> Option<GroundMaterial> {
        PRESETS
            .iter()
            .find(|(n, ..)| *n == name)
            .map(|&(name, v, q, rho)| GroundMaterial {
                name: name.to_owned(),
                wave_speed_mps: v,
                q,
                density_kg_m3: rho,
            })
    }

    /// `name`, else `fallback`, else dry soil (never fails: unknown names log once per call
    /// site in the caller and fall back).
    #[must_use]
    pub fn named_or(name: &str, fallback: &str) -> GroundMaterial {
        Self::named(name)
            .or_else(|| Self::named(fallback))
            .unwrap_or_else(|| GroundMaterial {
                name: "soil".into(),
                wave_speed_mps: 600.0,
                q: 25.0,
                density_kg_m3: 1_700.0,
            })
    }

    /// Acoustic impedance `ρ v`, rayl.
    #[must_use]
    pub fn impedance(&self) -> f32 {
        impedance_of(self.density_kg_m3, self.wave_speed_mps)
    }

    /// Amplitude factor of the anelastic absorption at `hz` after `distance` metres:
    /// `exp(−π f r / (Q v))`.
    #[must_use]
    pub fn absorption(&self, hz: f32, distance: f32) -> f32 {
        absorption_of(self.q, self.wave_speed_mps, hz, distance)
    }

    /// Low-pass cutoff of the absorption after `distance` metres (−3 dB point), Hz.
    #[must_use]
    pub fn absorption_cutoff_hz(&self, distance: f32) -> f32 {
        absorption_cutoff_of(self.q, self.wave_speed_mps, distance)
    }

    /// Length of the seismic coda (60 dB decay at [`CODA_REFERENCE_HZ`]), seconds, clamped.
    #[must_use]
    pub fn coda_secs(&self) -> f32 {
        coda_secs_of(self.q)
    }

    /// Wet level of the coda (0..1): grows with Q (dry rock rings, wet soil thuds).
    #[must_use]
    pub fn coda_mix(&self) -> f32 {
        coda_mix_of(self.q)
    }
}

/// How the ground wave spreads from its source.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, Reflect)]
#[reflect(Default)]
pub enum Spreading {
    /// Surface waves from a source on the ground: amplitude `∝ 1/√r`.
    #[default]
    Surface,
    /// Body waves from a deep or buried source: amplitude `∝ 1/r`.
    Body,
}

impl Spreading {
    /// Amplitude factor at `distance` for a source of size `reference_m` (1 inside it).
    #[must_use]
    pub fn factor(self, distance: f32, reference_m: f32) -> f32 {
        let r0 = reference_m.max(0.01);
        let ratio = r0 / distance.max(r0);
        match self {
            Self::Surface => ratio.sqrt(),
            Self::Body => ratio,
        }
    }
}

/// Energy fraction transmitted from a medium of impedance `z1` into one of `z2` at normal
/// incidence: `4 z₁ z₂ / (z₁ + z₂)²` (1 when they match).
#[must_use]
pub fn interface_transmission(z1: f32, z2: f32) -> f32 {
    // NaN reads as the floor; the ceiling keeps `(z1 + z2)²` finite.
    let bound = |z: f32| if z.is_nan() { 1e-3 } else { z.clamp(1e-3, 1e12) };
    let (z1, z2) = (bound(z1), bound(z2));
    4.0 * z1 * z2 / ((z1 + z2) * (z1 + z2))
}

/// One ground arrival at the listener's feet (before the listener's coupling filter).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroundArrival {
    /// Arrival delay, seconds (`r / v`).
    pub delay_secs: f32,
    /// Broadband amplitude: source level × coupling × spreading × interfaces (no absorption).
    pub amplitude: f32,
    /// Absorption low-pass cutoff, Hz.
    pub cutoff_hz: f32,
    /// Coda length, seconds.
    pub coda_secs: f32,
    /// Coda wet level 0..1.
    pub coda_mix: f32,
    /// Amplitude in the haptic band ([`HAPTIC_HZ`], absorption included): what a rumble motor
    /// should reproduce.
    pub felt: f32,
}

/// The ground arrival through `material` over `distance` metres from a source of size
/// `reference_m` whose ground-borne level is `amplitude` (linear, coupling already applied).
#[must_use]
pub fn ground_arrival(
    material: &GroundMaterial,
    distance: f32,
    spreading: Spreading,
    reference_m: f32,
    amplitude: f32,
) -> GroundArrival {
    arrival_from(
        material.wave_speed_mps,
        material.q,
        distance,
        spreading,
        reference_m,
        amplitude,
    )
}

/// [`ground_arrival`] for a [`MaterialView`], so the per-frame solver does not build a name.
#[must_use]
pub(crate) fn ground_arrival_view(
    material: MaterialView,
    distance: f32,
    spreading: Spreading,
    reference_m: f32,
    amplitude: f32,
) -> GroundArrival {
    arrival_from(
        material.wave_speed_mps,
        material.q,
        distance,
        spreading,
        reference_m,
        amplitude,
    )
}

fn arrival_from(
    wave_speed_mps: f32,
    q: f32,
    distance: f32,
    spreading: Spreading,
    reference_m: f32,
    amplitude: f32,
) -> GroundArrival {
    let v = wave_speed_mps.max(1.0);
    let amplitude = amplitude.max(0.0) * spreading.factor(distance, reference_m);
    GroundArrival {
        delay_secs: distance.max(0.0) / v,
        amplitude,
        cutoff_hz: absorption_cutoff_of(q, wave_speed_mps, distance),
        coda_secs: coda_secs_of(q),
        coda_mix: coda_mix_of(q),
        felt: amplitude * absorption_of(q, wave_speed_mps, HAPTIC_HZ, distance),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_resolve_and_fall_back() {
        assert_eq!(
            GroundMaterial::named("ice").map(|m| m.wave_speed_mps),
            Some(3_800.0)
        );
        assert!(GroundMaterial::named("lava").is_none());
        assert_eq!(GroundMaterial::named_or("lava", "bedrock").name, "bedrock");
        assert_eq!(GroundMaterial::named_or("lava", "nope").name, "soil");
        assert_eq!(GroundMaterial::presets().len(), PRESETS.len());
    }

    #[test]
    fn surface_waves_spread_as_one_over_root_r_and_body_waves_as_one_over_r() {
        assert!((Spreading::Surface.factor(100.0, 1.0) - 0.1).abs() < 1e-6);
        assert!((Spreading::Body.factor(100.0, 1.0) - 0.01).abs() < 1e-6);
        assert_eq!(Spreading::Surface.factor(0.5, 1.0), 1.0, "inside the source");
    }

    #[test]
    fn q_absorption_cutoff_is_the_minus_3_db_point() {
        let ice = GroundMaterial::named("ice").expect("ice");
        let fc = ice.absorption_cutoff_hz(500.0);
        // ln√2 · Q v / (π r) = 0.3466 · 200 · 3800 / (π · 500) ≈ 167.7 Hz.
        assert!((fc - 167.7).abs() < 1.0, "{fc}");
        assert!((ice.absorption(fc, 500.0) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-3);
        // Farther is darker; higher Q is brighter.
        assert!(ice.absorption_cutoff_hz(1_000.0) < fc);
        let rock = GroundMaterial::named("airless_rock").expect("rock");
        assert!(rock.absorption_cutoff_hz(500.0) > fc);
    }

    #[test]
    fn dry_high_q_ground_rings_longer_than_wet_soil() {
        let regolith = GroundMaterial::named("regolith").expect("regolith");
        let wet = GroundMaterial::named("wet_soil").expect("wet soil");
        assert_eq!(
            regolith.coda_secs(),
            MAX_CODA_SECS,
            "lunar-like regolith rings to the cap"
        );
        assert!(wet.coda_secs() < 0.5, "{}", wet.coda_secs());
        assert!(regolith.coda_mix() > wet.coda_mix());
    }

    #[test]
    fn impedance_mismatch_loses_energy() {
        let ice = GroundMaterial::named("ice").expect("ice");
        let steel = GroundMaterial::named("steel").expect("steel");
        assert!((interface_transmission(ice.impedance(), ice.impedance()) - 1.0).abs() < 1e-6);
        let t = interface_transmission(ice.impedance(), steel.impedance());
        assert!(t < 0.5 && t > 0.1, "ice → steel transmits {t}");
        // Solid into air loses nearly everything.
        assert!(interface_transmission(steel.impedance(), 413.0) < 1e-4);
    }

    #[test]
    fn arrival_combines_delay_spreading_and_absorption() {
        let ice = GroundMaterial::named("ice").expect("ice");
        let a = ground_arrival(&ice, 500.0, Spreading::Surface, 10.0, 1.0);
        assert!((a.delay_secs - 500.0 / 3_800.0).abs() < 1e-6);
        assert!((a.amplitude - (10.0f32 / 500.0).sqrt()).abs() < 1e-6);
        assert!(a.felt < a.amplitude && a.felt > 0.5 * a.amplitude);
    }
}
