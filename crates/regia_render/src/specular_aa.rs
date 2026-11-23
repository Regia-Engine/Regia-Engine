// SPDX-License-Identifier: MPL-2.0
//! Specular anti-aliasing: the parameters and CPU mirrors of the shader terms that widen the
//! GGX lobe by the variance of the normals one pixel covers, `α'² = α² + 2σ²` (α = perceptual
//! roughness², the engine's convention). [`crate::FrameQuality::specular_aa`] turns them on.
//!
//! - **Geometric** (`shaders/pbr_lighting.wgsl::specular_aa_roughness`; Kaplanyan & Hoffman 2016,
//!   Tokuyoshi & Kaplanyan 2019, the Filament `normalFiltering` form): σ² is the screen-space
//!   derivative energy of the shading normal times [`SCREEN_VARIANCE`], and the added term is
//!   clamped to [`THRESHOLD`]. Catches curvature and normal detail that changes from pixel to
//!   pixel. [`screen_space_roughness`] is the CPU mirror; the terrain shader has its own copy.
//! - **Normal-map variance** (`toksvig_roughness`; Toksvig 2005, "Mipmapping normal maps"): a
//!   filtered normal-map sample averages unit normals (the RGBA8 mips store the averages
//!   unnormalised, and trilinear and anisotropic filtering average further), so its length
//!   `|N̄| < 1` measures their spread. Toksvig's effective exponent
//!   `s' = s|N̄| / (|N̄| + s(1 − |N̄|))` gives `1/s' = 1/s + (1 − |N̄|)/|N̄|`; with the
//!   Beckmann/GGX mapping `α² ≈ 2/s` that is `α'² = α² + 2σ²`, `σ² = (1 − |N̄|)/|N̄|`
//!   ([`toksvig_variance`]), less [`TOKSVIG_FLOOR`] for 8-bit quantisation. Catches the
//!   sub-pixel normal detail the derivatives cannot see. [`toksvig_roughness`] is the CPU mirror.
//!
//! A two-channel compressed normal map (BC5, z rebuilt in the shader) loses `|N̄|`; the cook
//! bakes the same variance into the packed ORM's roughness mips instead
//! (`regia_asset_pipeline`, Toksvig at mip generation, ADR-091).

/// Screen-space variance of the geometric specular AA kernel (Filament's default, 0.15).
pub const SCREEN_VARIANCE: f32 = 0.15;

/// Clamp on the squared-roughness the geometric term may add (Filament's default, 0.2).
/// Suppresses derivative noise at silhouettes and on very bumpy normals.
pub const THRESHOLD: f32 = 0.2;

/// Variance subtracted from the Toksvig term: one RGBA8 texel's length is off 1 by up to
/// √3 · 1/255 ≈ 0.007 from quantisation (σ² ≈ 0.007), so `|N̄| ≥ 0.99` reads as no spread and a
/// magnified map does not roughen.
pub const TOKSVIG_FLOOR: f32 = 0.01;

/// CPU mirror of `pbr_lighting.wgsl::specular_aa_roughness`: perceptual roughness widened by
/// the screen-space variance of the shading normal (`dn_dx`, `dn_dy` are its per-pixel
/// derivatives). Never lowers the roughness.
#[must_use]
pub fn screen_space_roughness(
    perceptual: f32,
    dn_dx: [f32; 3],
    dn_dy: [f32; 3],
    variance: f32,
    threshold: f32,
) -> f32 {
    let dot = |v: [f32; 3]| v[0] * v[0] + v[1] * v[1] + v[2] * v[2];
    let sigma2 = variance * (dot(dn_dx) + dot(dn_dy));
    let kernel = (2.0 * sigma2).min(threshold);
    if kernel <= 0.0 {
        return perceptual;
    }
    let a = perceptual * perceptual;
    perceptual.max((a * a + kernel).clamp(0.0, 1.0).sqrt().sqrt())
}

/// Toksvig variance `σ² = (1 − |N̄|)/|N̄|` of the normals whose mean vector has length
/// `mean_length` (1 = all parallel). The length is clamped to `1e-4..=1`.
#[must_use]
pub fn toksvig_variance(mean_length: f32) -> f32 {
    let l = mean_length.clamp(1e-4, 1.0);
    (1.0 - l) / l
}

/// Perceptual roughness after adding normal variance `sigma2` to the lobe:
/// `(r⁴ + 2σ²)^¼`, at most 1, never below `perceptual`.
#[must_use]
pub fn widen_roughness(perceptual: f32, sigma2: f32) -> f32 {
    let p = perceptual.clamp(0.0, 1.0);
    if sigma2 <= 0.0 {
        return p;
    }
    let a2 = p * p * p * p;
    p.max((a2 + 2.0 * sigma2).min(1.0).sqrt().sqrt())
}

/// CPU mirror of `pbr_lighting.wgsl::toksvig_roughness`: perceptual roughness widened by the
/// spread of a filtered normal-map sample of length `normal_length`, less `floor`.
#[must_use]
pub fn toksvig_roughness(perceptual: f32, normal_length: f32, floor: f32) -> f32 {
    let sigma2 = toksvig_variance(normal_length) - floor;
    if sigma2 <= 0.0 {
        return perceptual;
    }
    widen_roughness(perceptual, sigma2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toksvig_variance_is_zero_for_parallel_normals_and_grows_with_spread() {
        assert_eq!(toksvig_variance(1.0), 0.0);
        assert!(toksvig_variance(0.9) > 0.0);
        assert!(toksvig_variance(0.5) > toksvig_variance(0.9));
        // (1 − 0.5) / 0.5 = 1.
        assert!((toksvig_variance(0.5) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn widen_roughness_matches_the_closed_form_and_never_lowers() {
        // r = 0.5: α² = 0.0625; σ² = 0.1 → α'² = 0.2625 → r' = 0.2625^¼.
        let r = widen_roughness(0.5, 0.1);
        assert!((r - 0.2625f32.powf(0.25)).abs() < 1e-5, "{r}");
        assert_eq!(widen_roughness(0.3, 0.0), 0.3);
        assert_eq!(widen_roughness(0.9, 10.0), 1.0);
        for p in [0.0, 0.1, 0.5, 0.99] {
            assert!(widen_roughness(p, 0.01) >= p);
        }
    }

    #[test]
    fn screen_space_term_is_clamped_by_the_threshold() {
        let flat = screen_space_roughness(0.2, [0.0; 3], [0.0; 3], SCREEN_VARIANCE, THRESHOLD);
        assert!((flat - 0.2).abs() < 1e-6);
        // Huge derivatives: α'² = α² + threshold.
        let wild = screen_space_roughness(
            0.2,
            [10.0, 0.0, 0.0],
            [0.0, 10.0, 0.0],
            SCREEN_VARIANCE,
            THRESHOLD,
        );
        let expect = (0.2f32.powi(4) + THRESHOLD).sqrt().sqrt();
        assert!((wild - expect).abs() < 1e-5, "{wild} vs {expect}");
        // Small derivatives: 2 · 0.15 · (0.01 + 0.01) = 0.006.
        let small = screen_space_roughness(0.2, [0.1, 0.0, 0.0], [0.0, 0.1, 0.0], SCREEN_VARIANCE, THRESHOLD);
        let expect = (0.2f32.powi(4) + 0.006).sqrt().sqrt();
        assert!((small - expect).abs() < 1e-5, "{small} vs {expect}");
    }

    #[test]
    fn toksvig_ignores_quantisation_and_roughens_short_normals() {
        // One quantised unit normal: unchanged.
        assert_eq!(toksvig_roughness(0.2, 0.995, TOKSVIG_FLOOR), 0.2);
        assert_eq!(toksvig_roughness(0.2, 1.0, TOKSVIG_FLOOR), 0.2);
        // |N̄| = 0.9: σ² = 0.111 − 0.01.
        let r = toksvig_roughness(0.2, 0.9, TOKSVIG_FLOOR);
        let expect = widen_roughness(0.2, 1.0 / 0.9 - 1.0 - TOKSVIG_FLOOR);
        assert!((r - expect).abs() < 1e-6 && r > 0.5, "{r}");
    }
}
