// SPDX-License-Identifier: MPL-2.0
//! Aerosol colour: the Ångström exponent of the Earth haze (ADR-1408).
//!
//! The level's `aerosol_turbidity` sets the vertical aerosol optical depth, and the Mie
//! coefficients it scales are spectrally flat (grey). Real continental aerosol scatters more at
//! short wavelengths: `τ(λ) = τ₅₅₀ (λ / 550 nm)^−α` with Ångström exponent α ≈ 1.3
//! (Ångström 1929; Eck et al. 1999 measure α 1.0–1.5 for continental and urban aerosol, near 0
//! for desert dust and sea salt). Under a low sun the flat haze takes the colour of the
//! reddened sunlight that lights it, so the whole distance turns sepia. A blue-weighted haze
//! keeps the far hills grey-blue while the sun itself still reddens, as in photographs.
//!
//! Insert [`AerosolSpectrum`] to set α; without it the Mie coefficients stay flat (as before).
//! Agent: `render.get_aerosol` / `render.set_aerosol` (book `agents/low-sun.md`).

use regia_app::prelude::*;

use crate::atmosphere::AtmosphereParameters;

/// Wavelengths the sky's red, green and blue channels stand for (nm), as in the atmosphere
/// tables' Rayleigh coefficients (Hillaire 2020).
pub const AEROSOL_WAVELENGTHS_NM: [f32; 3] = [680.0, 550.0, 440.0];

/// Typical continental Ångström exponent (Eck et al. 1999).
pub const CONTINENTAL_ANGSTROM: f32 = 1.3;

/// The Ångström exponent of the Earth haze. Absent (or 0): spectrally flat Mie, as before.
#[derive(Resource, Debug, Clone, Copy, PartialEq)]
pub struct AerosolSpectrum {
    /// Ångström exponent α, `0..=3` (0 grey, 1.3 continental, near 0 for dust).
    pub angstrom: f32,
}

impl Default for AerosolSpectrum {
    fn default() -> Self {
        Self {
            angstrom: CONTINENTAL_ANGSTROM,
        }
    }
}

impl AerosolSpectrum {
    /// The exponent clamped to `0..=3` (non-finite: 0).
    #[must_use]
    pub fn alpha(&self) -> f32 {
        if self.angstrom.is_finite() {
            self.angstrom.clamp(0.0, 3.0)
        } else {
            0.0
        }
    }
}

/// Per-channel factors `(λ / 550 nm)^−α` for [`AEROSOL_WAVELENGTHS_NM`]: green is 1, so the
/// turbidity (defined at 550 nm) is unchanged.
#[must_use]
pub fn angstrom_factors(alpha: f32) -> [f32; 3] {
    let alpha = if alpha.is_finite() {
        alpha.clamp(0.0, 3.0)
    } else {
        0.0
    };
    AEROSOL_WAVELENGTHS_NM.map(|l| (l / 550.0).powf(-alpha))
}

impl AtmosphereParameters {
    /// Makes the Mie (aerosol) coefficients follow an Ångström law of exponent `alpha` about the
    /// green channel, keeping each channel's single-scattering albedo and the green value (the
    /// turbidity) as they are. `alpha` 0 leaves the coefficients untouched.
    pub fn set_aerosol_angstrom(&mut self, alpha: f32) {
        let f = angstrom_factors(alpha);
        if f == [1.0; 3] {
            return;
        }
        let green_ext = self.mie_extinction[1];
        let green_sca = self.mie_scattering[1];
        let albedo = if green_ext > 0.0 {
            green_sca / green_ext
        } else {
            0.0
        };
        self.mie_extinction = f.map(|k| green_ext * k);
        self.mie_scattering = f.map(|k| green_ext * k * albedo);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn factors_keep_green_and_weight_blue() {
        assert_eq!(angstrom_factors(0.0), [1.0; 3]);
        let f = angstrom_factors(CONTINENTAL_ANGSTROM);
        assert!((f[1] - 1.0).abs() < 1e-6);
        assert!(f[2] > 1.3 && f[2] < 1.4, "blue {}", f[2]);
        assert!(f[0] > 0.7 && f[0] < 0.8, "red {}", f[0]);
        assert_eq!(angstrom_factors(f32::NAN), [1.0; 3]);
    }

    #[test]
    fn angstrom_keeps_turbidity_and_albedo() {
        let mut p = AtmosphereParameters::earth();
        p.set_aerosol_turbidity(0.18);
        let tau = p.aerosol_optical_depth();
        let before = p;
        p.set_aerosol_angstrom(0.0);
        assert_eq!(p, before, "alpha 0 changes nothing");
        p.set_aerosol_angstrom(CONTINENTAL_ANGSTROM);
        assert!((p.mie_extinction[1] * p.mie_scale_height_km - tau).abs() < 1e-6);
        assert!(p.mie_extinction[2] > p.mie_extinction[1] && p.mie_extinction[1] > p.mie_extinction[0]);
        for c in 0..3 {
            let a0 = before.mie_scattering[1] / before.mie_extinction[1];
            assert!((p.mie_scattering[c] / p.mie_extinction[c] - a0).abs() < 1e-5);
        }
        assert_eq!(AerosolSpectrum { angstrom: 9.0 }.alpha(), 3.0);
        assert_eq!(AerosolSpectrum { angstrom: f32::NAN }.alpha(), 0.0);
    }
}
