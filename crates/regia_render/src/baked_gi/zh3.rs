// SPDX-License-Identifier: MPL-2.0
//! Hallucinated ZH3 evaluation of the baked L1 radiance SH (ADR-1377): the CPU mirror of
//! `bgi_zh3_irradiance` in `baked_gi.wgsl`; keep the two in step.
//!
//! The volume stores L1 SH of radiance per channel (`c0 = ∫L·Y00`, `c1 = ∫L·Y1·ω`, a vector).
//! The clamped-cosine convolution (Ramamoorthi & Hanrahan 2001) gives
//! `E(n)/π = Y00·c0 + (2/3)·Y1·(c1·n) + (1/4)·Σ c2m·Y2m(n) + …`; L1 stops after band 1, which
//! under a narrow source reads about 25 % low toward it and rings negative behind it.
//!
//! **ZH3 from L1** (after Roughton, Sloan, Silvennoinen, Iwanicki and Shirley, "ZH3: Quadratic
//! Zonal Harmonics", I3D 2024, the variant that stores nothing more and "hallucinates" the
//! zonal coefficient from the L1 band): the missing band 2 is replaced by one zonal harmonic
//! along the luminance L1 direction `a` (shared by the three channels, so the color does not
//! shift with the direction), with a coefficient `z = c0·f(t)` estimated per channel from the
//! channel's L1 projected on that axis, `t = |c1·a| / (√3·c0)` (0 for uniform light, 1 for a
//! single direction). Its convolution adds `(1/4)·z·Y20(a·n)` to `E/π`.
//!
//! **The curve** `f` is ours: the paper fits a curve to a set of environments; this one is
//! derived in closed form. For a uniform cap of light of half-angle `α ≤ 90°` (`μ = cos α`),
//! `t = (1 + μ)/2` and the true zonal coefficient is `c2 = √5·c0·t·(2t − 1)`, so
//! `f(t) = √5·max(t·(2t − 1), 0)` makes the evaluation the exact band-2 projection of every
//! such cap, from a hemisphere (`t = ½`, `f = 0`) to a point (`t = 1`, `f = √5`). Below `t = ½`
//! (wider than a hemisphere, or light from several sides) the term is 0 and ZH3 is L1 exactly:
//! uniform light (the furnace), a sky or any field L1 already holds without ringing is
//! unchanged, bit for bit. The result is clamped at 0, as L1's; a single direction still dips
//! to `−4 %` of its peak before the clamp where the true irradiance is 0 (no quadratic in
//! `a·n` both matches a point source and stays non-negative).

use regia_asset::baked_gi::{SH_Y00, SH_Y1, ShRgb};

use super::{V3, dot};

/// `BGI_ZH3_EVAL` = 5 / (16·√π): with `z = √5·c0·g`, `(1/4)·z·Y20(x) = EVAL·c0·g·(3x² − 1)`
/// (`Y20(x) = √(5 / 16π)·(3x² − 1)`).
const ZH3_EVAL: f32 = 0.176_309_24;
/// `BGI_SQRT3`: `t = |c1·a| / (√3·c0)`.
const SQRT3: f32 = 1.732_050_8;
/// `BGI_ZH3_AXIS_MIN2`: below this squared length of the luminance L1 there is no axis, and
/// the evaluation is L1.
const ZH3_AXIS_MIN2: f32 = 1e-20;
/// Luminance weights of the shared axis (Rec. 709; `bgi_relight` uses the same).
const LUMA: [f32; 3] = [0.2126, 0.7152, 0.0722];

/// `f(t) / √5` (`bgi_zh3_ratio`): `max(t·(2t − 1), 0)` with `t` clamped to `0..=1`.
#[must_use]
fn zonal_ratio(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    (t * (2.0 * t - 1.0)).max(0.0)
}

/// The shared zonal axis (unit luminance L1 direction) of `sh`, or `None` without one.
fn zonal_axis(sh: &ShRgb) -> Option<V3> {
    let lum: V3 =
        std::array::from_fn(|i| LUMA[0] * sh[0][1 + i] + LUMA[1] * sh[1][1 + i] + LUMA[2] * sh[2][1 + i]);
    let l2 = dot(lum, lum);
    (l2 > ZH3_AXIS_MIN2).then(|| {
        let k = 1.0 / l2.sqrt();
        [lum[0] * k, lum[1] * k, lum[2] * k]
    })
}

/// Irradiance / π toward the unit direction `n` from the L1 radiance SH `sh` (per channel `c0`
/// and the vector `c1`, the units of [`ShRgb`]) with the hallucinated ZH3 term, clamped at 0
/// (`bgi_zh3_irradiance`). Uniform light and any channel whose L1 on the shared axis is at most
/// a hemisphere's read as the plain L1 evaluation, bit for bit.
#[must_use]
pub fn zh3_irradiance_over_pi(sh: &ShRgb, n: [f32; 3]) -> [f32; 3] {
    let a0 = SH_Y00;
    let a1 = SH_Y1 * (2.0 / 3.0);
    let axis = zonal_axis(sh);
    std::array::from_fn(|ch| {
        let c0 = sh[ch][0];
        let c1 = [sh[ch][1], sh[ch][2], sh[ch][3]];
        // The L1 evaluation, term for term as `channel_irradiance`'s linear form.
        let lin = a0 * c0 + a1 * dot(c1, n);
        let Some(a) = axis else {
            return lin.max(0.0);
        };
        if c0 <= 0.0 {
            return lin.max(0.0);
        }
        let g = zonal_ratio(dot(c1, a).abs() / (SQRT3 * c0));
        if g <= 0.0 {
            return lin.max(0.0);
        }
        let x = dot(a, n);
        (lin + ZH3_EVAL * c0 * g * (3.0 * x * x - 1.0)).max(0.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::baked_gi::channel_irradiance;

    /// `n` directions spread evenly over the sphere (a Fibonacci lattice).
    fn sphere(n: usize) -> Vec<V3> {
        let golden = std::f32::consts::PI * (3.0 - 5.0f32.sqrt());
        (0..n)
            .map(|i| {
                let y = 1.0 - 2.0 * (i as f32 + 0.5) / n as f32;
                let r = (1.0 - y * y).max(0.0).sqrt();
                let phi = golden * i as f32;
                [r * phi.cos(), y, r * phi.sin()]
            })
            .collect()
    }

    /// The L1 SH (one channel: `c0`, `c1`) of radiance 1 inside a cap of half-angle
    /// `acos(mu)` around `axis`: `c0 = Y00·2π(1 − μ)`, `c1 = Y1·π(1 − μ²)·axis`.
    fn cap_sh(mu: f32, axis: V3) -> [f32; 4] {
        let pi = std::f32::consts::PI;
        let c0 = SH_Y00 * 2.0 * pi * (1.0 - mu);
        let k = SH_Y1 * pi * (1.0 - mu * mu);
        [c0, axis[0] * k, axis[1] * k, axis[2] * k]
    }

    /// The exact irradiance / π of that cap toward a direction at cosine `t` from its axis:
    /// `(1/π)∫cap max(n·l, 0) dl`, integrated numerically over the cap.
    fn cap_truth(mu: f32, t: f32) -> f64 {
        let (mu, t) = (f64::from(mu), f64::from(t));
        let n = [(1.0 - t * t).max(0.0).sqrt(), 0.0, t];
        let th_max = mu.acos();
        let (rings, segs) = (160usize, 96usize);
        let (dth, dph) = (th_max / rings as f64, std::f64::consts::TAU / segs as f64);
        let mut sum = 0.0;
        for i in 0..rings {
            let th = (i as f64 + 0.5) * dth;
            let (s, c) = th.sin_cos();
            for j in 0..segs {
                let ph = (j as f64 + 0.5) * dph;
                let d = n[0] * s * ph.cos() + n[2] * c;
                if d > 0.0 {
                    sum += d * s * dth * dph;
                }
            }
        }
        sum / std::f64::consts::PI
    }

    /// RMS error over the sphere (uniform in the cosine from the axis) of `eval` against the
    /// cap's exact irradiance, relative to its peak.
    fn cap_error(mu: f32, eval: impl Fn(&[f32; 4], V3) -> f32) -> f64 {
        let sh = cap_sh(mu, [0.0, 0.0, 1.0]);
        let steps = 48;
        let (mut sq, mut peak) = (0.0f64, 0.0f64);
        for k in 0..steps {
            let t = -1.0 + 2.0 * (k as f32 + 0.5) / steps as f32;
            let n = [(1.0 - t * t).max(0.0).sqrt(), 0.0, t];
            let truth = cap_truth(mu, t);
            peak = peak.max(truth);
            let e = f64::from(eval(&sh, n)) - truth;
            sq += e * e;
        }
        (sq / f64::from(steps)).sqrt() / peak
    }

    fn grey(c: &[f32; 4]) -> ShRgb {
        [*c, *c, *c]
    }

    fn zh3_grey(c: &[f32; 4], n: V3) -> f32 {
        zh3_irradiance_over_pi(&grey(c), n)[0]
    }

    fn l1(c: &[f32; 4], n: V3, dering: bool) -> f32 {
        channel_irradiance(c[0], [c[1], c[2], c[3]], n, dering)
    }

    /// The furnace: uniform radiance has no L1 band, so ZH3 adds nothing and reads the L1
    /// evaluation bit for bit (with or without de-ringing) toward every direction.
    #[test]
    fn zh3_on_a_uniform_field_is_l1_exactly() {
        for c0 in [0.0f32, 1e-6, 0.5, 3.0, 4.0e4] {
            let sh: ShRgb = [
                [c0, 0.0, 0.0, 0.0],
                [0.5 * c0, 0.0, 0.0, 0.0],
                [2.0 * c0, 0.0, 0.0, 0.0],
            ];
            for d in sphere(257) {
                let z = zh3_irradiance_over_pi(&sh, d);
                for ch in 0..3 {
                    let c = sh[ch];
                    let want = l1(&c, d, false);
                    assert_eq!(z[ch].to_bits(), want.to_bits(), "c0 {c0} ch {ch} toward {d:?}");
                    assert_eq!(want.to_bits(), l1(&c, d, true).to_bits());
                }
            }
        }
        // Uniform light of radiance 1: E/π = 1 everywhere (Y00·c0 = Y00²·4π = 1).
        let unit = 4.0 * std::f32::consts::PI * SH_Y00;
        for d in sphere(64) {
            assert!((zh3_grey(&[unit, 0.0, 0.0, 0.0], d) - 1.0).abs() < 1e-5);
        }
    }

    /// A single direction of light (a cap of 2°, near a point): ZH3 is closer to the exact
    /// clamped-cosine convolution than L1, plain or de-ringed (ADR-861), and its peak toward the
    /// source is within 7 % where L1's is ~25 % low.
    #[test]
    fn zh3_beats_l1_on_a_single_direction_lobe() {
        let mu = 2.0f32.to_radians().cos();
        let zh3 = cap_error(mu, zh3_grey);
        let plain = cap_error(mu, |c, n| l1(c, n, false));
        let dering = cap_error(mu, |c, n| l1(c, n, true));
        assert!(zh3 < 0.6 * plain, "zh3 {zh3:.4} vs L1 {plain:.4}");
        assert!(zh3 < dering, "zh3 {zh3:.4} vs de-ringed L1 {dering:.4}");
        let sh = cap_sh(mu, [0.0, 0.0, 1.0]);
        let truth = cap_truth(mu, 1.0);
        let peak = f64::from(zh3_grey(&sh, [0.0, 0.0, 1.0]));
        let peak_l1 = f64::from(l1(&sh, [0.0, 0.0, 1.0], false));
        assert!((peak / truth - 1.0).abs() < 0.07, "zh3 peak {peak} vs {truth}");
        assert!(peak_l1 < 0.8 * truth, "L1 peak {peak_l1} vs {truth}");
    }

    /// Across caps from 2° to a hemisphere ZH3 is never worse than L1 (and exact for the
    /// hemisphere, where L1 already is); past the hemisphere it is L1.
    #[test]
    fn zh3_is_never_worse_than_l1_on_uniform_caps() {
        for deg in [2.0f32, 10.0, 30.0, 45.0, 60.0, 75.0, 90.0, 120.0, 150.0] {
            let mu = deg.to_radians().cos();
            let zh3 = cap_error(mu, zh3_grey);
            let plain = cap_error(mu, |c, n| l1(c, n, false));
            assert!(zh3 <= plain + 1e-4, "{deg}°: zh3 {zh3:.4} vs L1 {plain:.4}");
            if deg >= 90.0 {
                let sh = cap_sh(mu, [0.0, 0.0, 1.0]);
                for d in sphere(97) {
                    let (z, l) = (zh3_grey(&sh, d), l1(&sh, d, false));
                    assert!(
                        (z - l).abs() <= 1e-6 * l.abs().max(1.0),
                        "{deg}° toward {d:?}: {z} vs {l}"
                    );
                }
            }
        }
    }

    /// The axis is shared and the term scales with each channel's own L0, so a colored source
    /// keeps its color in every direction where the light arrives, and a channel with no light
    /// reads 0.
    #[test]
    fn zh3_keeps_the_color_of_a_colored_source() {
        let mu = 10.0f32.to_radians().cos();
        let axis = [0.6, 0.0, 0.8];
        let c = cap_sh(mu, axis);
        let scale = [1.0f32, 0.4, 0.0];
        let sh: ShRgb = std::array::from_fn(|ch| c.map(|v| v * scale[ch]));
        for d in sphere(129) {
            let e = zh3_irradiance_over_pi(&sh, d);
            assert_eq!(e[2], 0.0);
            if e[0] > 1e-4 {
                assert!((e[1] / e[0] - 0.4).abs() < 1e-4, "toward {d:?}: {e:?}");
            }
        }
    }

    /// The curve: 0 up to a hemisphere's ratio, √5·t(2t − 1) past it, the band-2 coefficient of
    /// a cap; the evaluation constant is 5 / (16√π).
    #[test]
    fn the_zonal_curve_is_the_cap_band_two_coefficient() {
        assert_eq!(zonal_ratio(0.0), 0.0);
        assert_eq!(zonal_ratio(0.5), 0.0);
        assert_eq!(zonal_ratio(1.0), 1.0);
        assert_eq!(zonal_ratio(2.0), 1.0);
        let pi = std::f64::consts::PI;
        assert!((f64::from(ZH3_EVAL) - 5.0 / (16.0 * pi.sqrt())).abs() < 1e-7);
        for deg in [5.0f64, 30.0, 60.0, 89.0] {
            let mu = deg.to_radians().cos();
            let t = (1.0 + mu) / 2.0;
            // c2 / c0 of the cap: √(5/4π)·π·μ(1 − μ²) / (Y00·2π(1 − μ)) = (√5/2)·μ(1 + μ).
            let c2_over_c0 = 5.0f64.sqrt() / 2.0 * mu * (1.0 + mu);
            let got = 5.0f64.sqrt() * f64::from(zonal_ratio(t as f32));
            assert!((got - c2_over_c0).abs() < 1e-5, "{deg}°: {got} vs {c2_over_c0}");
        }
    }

    /// The shader writes the same constants and the same expression (`bgi_zh3_irradiance`).
    #[test]
    fn the_shader_mirrors_this_module() {
        let wgsl = include_str!("../../shaders/baked_gi.wgsl");
        for text in [
            format!("const BGI_ZH3_EVAL: f32 = {ZH3_EVAL:?};"),
            format!("const BGI_ZH3_AXIS_MIN2: f32 = {ZH3_AXIS_MIN2:?};"),
            format!(
                "const BGI_LUMA: vec3<f32> = vec3<f32>({:?}, {:?}, {:?});",
                LUMA[0], LUMA[1], LUMA[2]
            ),
            "let t = clamp(abs(dot(l1, axis)) / (BGI_SQRT3 * l0), 0.0, 1.0);".to_owned(),
            "let g = max(t * (2.0 * t - 1.0), 0.0);".to_owned(),
            "return max(lin + BGI_ZH3_EVAL * l0 * g * (3.0 * x * x - 1.0), 0.0);".to_owned(),
        ] {
            assert!(wgsl.contains(&text), "baked_gi.wgsl lacks {text}");
        }
        assert!(wgsl.contains("const BGI_SQRT3: f32 = 1.7320508;"));
    }
}
