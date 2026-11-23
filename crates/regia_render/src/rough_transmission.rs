// SPDX-License-Identifier: MPL-2.0
//! Rough transmission through a thin dielectric (frosted glass, scuffed visors, ice; ADR-2075).
//!
//! A transmissive [`crate::LabMaterial`] with [`crate::LabMaterial::transmission_roughness`]
//! above 0 spreads what it shows behind it. The light it lets through keeps the clear sheet's
//! weight (`t · (1 − F) · base · volume`); only its direction spreads, over the GGX reflection
//! lobe of the sheet's normal mirrored through the sheet's plane, with the roughness scaled by
//! `0.65 η − 0.35` (Burley 2015, thin-surface BSDF). `shaders/rough_transmission.wgsl` holds the
//! model; the functions here mirror it, and both renderers use the WGSL: the reference path
//! tracer samples the lobe per path, the forward pass averages [`FORWARD_TAPS`] directions of it
//! over the refraction copy of the opaque frame (each tap lands where its direction meets the
//! plane of the background seen straight through, a single-step screen-space march).
//!
//! **Cost.** Nothing for a material without it: the material block is byte for byte unchanged
//! (the roughness rides in the hair parameters of a material without hair) and the refraction
//! pass takes its old path. A rough pane costs [`FORWARD_TAPS`] taps per pixel in that pass.
//!
//! **Needs** a frame that refracts ([`crate::FrameQuality::screen_space_refraction`], on in
//! Desktop High and Ultra); elsewhere a rough pane draws clear. A hair material and a material
//! graph keep clear transmission.

use crate::LabMaterial;

/// Taps the forward refraction pass averages per pixel of a rough pane.
pub const FORWARD_TAPS: u32 = 16;

/// The roughness the shader reads for `material`: `0..=1`, non-finite reads 0, and 0 without
/// transmission, with hair or with a material graph.
#[must_use]
pub fn effective_roughness(material: &LabMaterial) -> f32 {
    if material.transmission.is_nan()
        || material.transmission <= 0.0
        || material.hair.enabled
        || material.surface_graph.is_some()
    {
        return 0.0;
    }
    let r = material.transmission_roughness;
    if r.is_finite() { r.clamp(0.0, 1.0) } else { 0.0 }
}

/// Whether `material` draws rough transmission where the frame refracts.
#[must_use]
pub fn is_on(material: &LabMaterial) -> bool {
    effective_roughness(material) > 0.0
}

/// Writes the effective roughness into `hair_a.x` of a material without hair (whose hair block
/// is all zero), so the material block keeps its size and layout.
pub(crate) fn pack_into_hair(
    material: &LabMaterial,
    mut hair: crate::hair::HairUniform,
) -> crate::hair::HairUniform {
    if !material.hair.enabled {
        hair[0][0] = effective_roughness(material);
    }
    hair
}

/// GGX alpha of the transmission lobe (`thin_alpha` in the shader):
/// `((0.65 ior − 0.35) · roughness)²`, at least `1e-4`.
#[must_use]
pub fn thin_alpha(roughness: f32, ior: f32) -> f32 {
    let scale = (0.65 * ior.max(1.0) - 0.35).max(0.05);
    let r = roughness.clamp(0.0, 1.0) * scale;
    (r * r).max(1e-4)
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn normalize(a: [f32; 3]) -> [f32; 3] {
    let l = dot(a, a).sqrt().max(1e-20);
    [a[0] / l, a[1] / l, a[2] / l]
}

/// The transmitted direction for the uniform pair `u` (`sample_direction` in the shader). `wo`
/// points from the surface to the viewer and `n` is the sheet's unit normal on the viewer's side.
#[must_use]
pub fn sample_direction(wo: [f32; 3], n: [f32; 3], alpha: f32, u: [f32; 2]) -> [f32; 3] {
    let a2 = alpha * alpha;
    let cos2 = (1.0 - u[0]) / (1.0 + (a2 - 1.0) * u[0]).max(1e-7);
    let cos_t = cos2.clamp(0.0, 1.0).sqrt();
    let sin_t = (1.0 - cos2).clamp(0.0, 1.0).sqrt();
    let phi = std::f32::consts::TAU * u[1];
    let s = if n[2] >= 0.0 { 1.0 } else { -1.0 };
    let a = -1.0 / (s + n[2]);
    let b = n[0] * n[1] * a;
    let tx = [1.0 + s * n[0] * n[0] * a, s * b, -s * n[0]];
    let ty = [b, s + n[1] * n[1] * a, -n[1]];
    let (c, si) = (sin_t * phi.cos(), sin_t * phi.sin());
    let h = normalize([
        tx[0] * c + ty[0] * si + n[0] * cos_t,
        tx[1] * c + ty[1] * si + n[1] * cos_t,
        tx[2] * c + ty[2] * si + n[2] * cos_t,
    ]);
    let k = 2.0 * dot(wo, h);
    let r = [k * h[0] - wo[0], k * h[1] - wo[1], k * h[2] - wo[2]];
    let m = 2.0 * dot(n, r);
    let t = [r[0] - m * n[0], r[1] - m * n[1], r[2] - m * n[2]];
    if dot(n, t) > -1e-4 {
        return [-wo[0], -wo[1], -wo[2]];
    }
    normalize(t)
}

/// The `i`-th of `count` taps of the forward pass's pattern, rotated by `rotation` (`tap` in
/// the shader): a Fibonacci lattice over the unit square.
#[must_use]
pub fn tap(i: u32, count: u32, rotation: [f32; 2]) -> [f32; 2] {
    let k = i as f32;
    let base = [(k + 0.5) / count as f32, (k * 0.618_034).fract()];
    [(base[0] + rotation[0]).fract(), (base[1] + rotation[1]).fract()]
}

/// Root-mean-square angle, radians, between the straight-through direction and the lobe at
/// normal incidence for `roughness` and `ior`, over the forward pass's unrotated
/// [`FORWARD_TAPS`] taps: a size for agents and tools (`material.rough_transmission`).
#[must_use]
pub fn spread_radians(roughness: f32, ior: f32) -> f32 {
    let alpha = thin_alpha(roughness, ior);
    let (wo, n) = ([0.0, 0.0, 1.0], [0.0, 0.0, 1.0]);
    let mut sum = 0.0f32;
    for i in 0..FORWARD_TAPS {
        let t = sample_direction(wo, n, alpha, tap(i, FORWARD_TAPS, [0.0, 0.0]));
        let c = (-t[2]).clamp(-1.0, 1.0);
        sum += c.acos().powi(2);
    }
    (sum / FORWARD_TAPS as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alpha_scales_with_ior_and_roughness() {
        // η = 1.5: scale 0.625.
        assert!((thin_alpha(1.0, 1.5) - 0.625f32.powi(2)).abs() < 1e-6);
        assert!((thin_alpha(0.4, 1.5) - (0.4f32 * 0.625).powi(2)).abs() < 1e-6);
        assert_eq!(thin_alpha(0.0, 1.5), 1e-4);
        assert!(thin_alpha(0.5, 2.0) > thin_alpha(0.5, 1.33));
        // Out-of-range inputs clamp.
        assert_eq!(thin_alpha(3.0, 1.5), thin_alpha(1.0, 1.5));
        assert_eq!(thin_alpha(0.5, 0.2), thin_alpha(0.5, 1.0));
    }

    #[test]
    fn the_first_tap_and_a_smooth_lobe_go_straight_through() {
        let wo = normalize([0.3, 0.1, 0.9]);
        let n = [0.0, 0.0, 1.0];
        let t = sample_direction(wo, n, 0.3, [0.0, 0.7]);
        for k in 0..3 {
            assert!((t[k] + wo[k]).abs() < 1e-5, "{t:?} vs {wo:?}");
        }
        let t = sample_direction(wo, n, 1e-4, [0.6, 0.2]);
        for k in 0..3 {
            assert!((t[k] + wo[k]).abs() < 1e-3, "{t:?}");
        }
    }

    #[test]
    fn every_direction_leaves_through_the_far_side() {
        for &n in &[
            [0.0, 0.0, 1.0],
            normalize([0.2, -0.5, 0.8]),
            [0.0, 0.0, -1.0],
            [1.0, 0.0, 0.0],
        ] {
            let wo = normalize([n[0] + 0.3, n[1] - 0.2, n[2] + 0.1]);
            assert!(dot(wo, n) > 0.0);
            for i in 0..64 {
                let u = tap(i, 64, [0.37, 0.71]);
                let t = sample_direction(wo, n, thin_alpha(1.0, 1.5), u);
                assert!(dot(t, n) < 0.0, "n {n:?} u {u:?} t {t:?}");
                assert!((dot(t, t) - 1.0).abs() < 1e-4);
            }
        }
    }

    #[test]
    fn the_spread_grows_with_roughness() {
        let mut last = 0.0;
        for r in [0.05, 0.2, 0.4, 0.7, 1.0] {
            let s = spread_radians(r, 1.5);
            assert!(s > last, "roughness {r}: {s} after {last}");
            last = s;
        }
        assert!(spread_radians(0.0, 1.5) < 0.02);
    }

    #[test]
    fn taps_are_stratified_in_the_first_number() {
        let mut first: Vec<f32> = (0..FORWARD_TAPS)
            .map(|i| tap(i, FORWARD_TAPS, [0.0, 0.0])[0])
            .collect();
        first.sort_by(f32::total_cmp);
        for (i, v) in first.iter().enumerate() {
            let lo = i as f32 / FORWARD_TAPS as f32;
            assert!(*v >= lo && *v < lo + 1.0 / FORWARD_TAPS as f32);
        }
    }

    #[test]
    fn the_material_block_is_unchanged_without_the_feature() {
        let clear = LabMaterial {
            transmission: 1.0,
            ..LabMaterial::EMPTY
        };
        assert_eq!(pack_into_hair(&clear, [[0.0; 4]; 5]), [[0.0; 4]; 5]);
        // Roughness without transmission packs nothing either.
        let opaque = LabMaterial {
            transmission_roughness: 0.5,
            ..LabMaterial::EMPTY
        };
        assert_eq!(pack_into_hair(&opaque, [[0.0; 4]; 5]), [[0.0; 4]; 5]);
        let frosted = LabMaterial {
            transmission: 1.0,
            transmission_roughness: 0.4,
            ..LabMaterial::EMPTY
        };
        assert_eq!(pack_into_hair(&frosted, [[0.0; 4]; 5])[0][0], 0.4);
        let nan = LabMaterial {
            transmission_roughness: f32::NAN,
            ..frosted.clone()
        };
        assert_eq!(effective_roughness(&nan), 0.0);
        let mut hair = frosted;
        hair.hair.enabled = true;
        assert_eq!(effective_roughness(&hair), 0.0);
    }
}
