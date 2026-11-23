// SPDX-License-Identifier: MPL-2.0
//! The offline anodize baker: one oxide film on an absorbing metal, reflectance over cos theta,
//! integrated to linear RGB. **Never called per frame**: the viewer reads the baked table from the
//! material library (`anodized.fresnel_lut`), one texture row per color.
//!
//! The model is the textbook single-layer Airy sum (Born and Wolf, *Principles of Optics*, § 1.6.4):
//! ambient (n0 = 1) over a non-absorbing film (n1, Cauchy dispersion) over a metal with complex
//! index N2 = n + ik, both polarizations averaged, averaged over a small thickness spread. Spectra
//! go to XYZ with the analytic CIE 1931 fit of Wyman, Sloan and Shirley (JCGT 2013) and to linear
//! Rec.709, white-balanced so a perfect mirror is (1, 1, 1).

use std::ops::{Add, Div, Mul, Sub};

/// Number of cos theta entries a baked table carries (entry `i` at cos theta = i / (n - 1)).
pub const LUT_ENTRIES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq)]
struct C {
    re: f64,
    im: f64,
}

impl C {
    const fn new(re: f64, im: f64) -> C {
        C { re, im }
    }
    fn norm2(self) -> f64 {
        self.re * self.re + self.im * self.im
    }
    fn sqrt(self) -> C {
        let r = self.norm2().sqrt().sqrt();
        let a = self.im.atan2(self.re) * 0.5;
        C::new(r * a.cos(), r * a.sin())
    }
    fn exp_i(phase: C) -> C {
        // exp(i * phase) for complex phase.
        let m = (-phase.im).exp();
        C::new(m * phase.re.cos(), m * phase.re.sin())
    }
}

impl Add for C {
    type Output = C;
    fn add(self, o: C) -> C {
        C::new(self.re + o.re, self.im + o.im)
    }
}
impl Sub for C {
    type Output = C;
    fn sub(self, o: C) -> C {
        C::new(self.re - o.re, self.im - o.im)
    }
}
impl Mul for C {
    type Output = C;
    fn mul(self, o: C) -> C {
        C::new(self.re * o.re - self.im * o.im, self.re * o.im + self.im * o.re)
    }
}
impl Div for C {
    type Output = C;
    fn div(self, o: C) -> C {
        let d = o.norm2();
        C::new(
            (self.re * o.re + self.im * o.im) / d,
            (self.im * o.re - self.re * o.im) / d,
        )
    }
}

/// A metal's complex index sampled at a few wavelengths (linear interpolation between them).
#[derive(Debug, Clone, PartialEq)]
pub struct MetalNk {
    /// Wavelengths in nanometres, increasing.
    pub wavelength_nm: Vec<f64>,
    /// Real part.
    pub n: Vec<f64>,
    /// Extinction coefficient.
    pub k: Vec<f64>,
}

impl MetalNk {
    /// Complex index at `lambda`. Short `n` or `k`, a repeated wavelength, or a non-finite sample
    /// never indexes out of range and never returns NaN: the usable prefix is interpolated, and a
    /// zero span or a non-finite sample falls back to a finite endpoint.
    fn at(&self, lambda: f64) -> C {
        let nlen = self.wavelength_nm.len().min(self.n.len()).min(self.k.len());
        if nlen == 0 {
            return C::new(1.0, 0.0);
        }
        let sample = |i: usize| {
            let n = self.n[i];
            let k = self.k[i];
            C::new(
                if n.is_finite() { n } else { 1.0 },
                if k.is_finite() { k } else { 0.0 },
            )
        };
        if nlen == 1 {
            return sample(0);
        }
        let w = &self.wavelength_nm[..nlen];
        let i = w.partition_point(|x| *x < lambda).clamp(1, nlen - 1);
        let (a, b) = (i - 1, i);
        let span = w[b] - w[a];
        let t = if span.is_finite() && span > 0.0 && lambda.is_finite() {
            ((lambda - w[a]) / span).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (lo, hi) = (sample(a), sample(b));
        C::new(lo.re + (hi.re - lo.re) * t, lo.im + (hi.im - lo.im) * t)
    }
}

/// An oxide film: Cauchy index `a + b / lambda_um^2`, no absorption.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Oxide {
    /// Cauchy A.
    pub a: f64,
    /// Cauchy B (square micrometres).
    pub b: f64,
}

impl Oxide {
    fn n(&self, lambda_nm: f64) -> f64 {
        let um = lambda_nm * 1e-3;
        self.a + self.b / (um * um)
    }
}

/// Reflectance of film-on-metal at one wavelength and incidence cosine, polarizations averaged.
fn film_reflectance(lambda_nm: f64, cos0: f64, film_nm: f64, oxide: Oxide, metal: &MetalNk) -> f64 {
    let n0 = C::new(1.0, 0.0);
    let n1 = C::new(oxide.n(lambda_nm), 0.0);
    let n2 = metal.at(lambda_nm);
    let sin0 = (1.0 - cos0 * cos0).max(0.0).sqrt();
    let cos_in =
        |n: C| (C::new(1.0, 0.0) - (n0 * C::new(sin0, 0.0) / n) * (n0 * C::new(sin0, 0.0) / n)).sqrt();
    let c0 = C::new(cos0, 0.0);
    let c1 = cos_in(n1);
    let c2 = cos_in(n2);
    let rs = |na: C, ca: C, nb: C, cb: C| (na * ca - nb * cb) / (na * ca + nb * cb);
    let rp = |na: C, ca: C, nb: C, cb: C| (nb * ca - na * cb) / (nb * ca + na * cb);
    let beta = n1 * c1 * C::new(2.0 * std::f64::consts::PI * film_nm / lambda_nm, 0.0);
    let phase = C::exp_i(beta * C::new(2.0, 0.0));
    let airy = |r01: C, r12: C| {
        let num = r01 + r12 * phase;
        let den = C::new(1.0, 0.0) + r01 * r12 * phase;
        (num / den).norm2()
    };
    let s = airy(rs(n0, c0, n1, c1), rs(n1, c1, n2, c2));
    let p = airy(rp(n0, c0, n1, c1), rp(n1, c1, n2, c2));
    0.5 * (s + p)
}

fn lobe(l: f64, mu: f64, s1: f64, s2: f64) -> f64 {
    let s = if l < mu { s1 } else { s2 };
    let t = (l - mu) / s;
    (-0.5 * t * t).exp()
}

/// CIE 1931 2-degree color matching functions, the multi-lobe fit of Wyman, Sloan and Shirley 2013.
fn cie_xyz(l: f64) -> [f64; 3] {
    let x = 1.056 * lobe(l, 599.8, 37.9, 31.0) + 0.362 * lobe(l, 442.0, 16.0, 26.7)
        - 0.065 * lobe(l, 501.1, 20.4, 26.2);
    let y = 0.821 * lobe(l, 568.8, 46.9, 40.5) + 0.286 * lobe(l, 530.9, 16.3, 31.1);
    let z = 1.217 * lobe(l, 437.0, 11.8, 36.0) + 0.681 * lobe(l, 459.0, 26.0, 13.8);
    [x, y, z]
}

fn xyz_to_linear_rec709(c: [f64; 3]) -> [f64; 3] {
    [
        3.2406 * c[0] - 1.5372 * c[1] - 0.4986 * c[2],
        -0.9689 * c[0] + 1.8758 * c[1] + 0.0415 * c[2],
        0.0557 * c[0] - 0.2040 * c[1] + 1.0570 * c[2],
    ]
}

/// Integrates a reflectance spectrum to white-balanced linear Rec.709.
fn spectrum_to_rgb(reflectance: impl Fn(f64) -> f64) -> [f32; 3] {
    let mut xyz = [0.0; 3];
    let mut white = [0.0; 3];
    let mut l = 380.0;
    while l <= 780.0 {
        let cmf = cie_xyz(l);
        let r = reflectance(l);
        for c in 0..3 {
            xyz[c] += cmf[c] * r;
            white[c] += cmf[c];
        }
        l += 5.0;
    }
    let rgb = xyz_to_linear_rec709(xyz);
    let w = xyz_to_linear_rec709(white);
    [
        (rgb[0] / w[0]).max(0.0) as f32,
        (rgb[1] / w[1]).max(0.0) as f32,
        (rgb[2] / w[2]).max(0.0) as f32,
    ]
}

/// Inputs of one anodize color.
#[derive(Debug, Clone, PartialEq)]
pub struct AnodizeInputs {
    /// The substrate metal's n, k.
    pub metal: MetalNk,
    /// The oxide's index.
    pub oxide: Oxide,
    /// Mean film thickness in nanometres.
    pub thickness_nm: f64,
    /// Half-width of the uniform thickness spread in nanometres (averaged over 5 samples).
    pub jitter_nm: f64,
}

/// Bakes the `cos theta -> linear RGB` table (`LUT_ENTRIES` entries, cos theta = i / (n - 1)).
pub fn bake_anodize_lut(inputs: &AnodizeInputs) -> Vec<[f32; 3]> {
    let _span = tracing::debug_span!("bake_anodize_lut").entered();
    let taps = [-1.0, -0.5, 0.0, 0.5, 1.0];
    (0..LUT_ENTRIES)
        .map(|i| {
            // cos theta = 0 is grazing; nudge it so the table stays finite.
            let cos0 = (i as f64 / (LUT_ENTRIES - 1) as f64).max(0.02);
            spectrum_to_rgb(|l| {
                taps.iter()
                    .map(|t| {
                        let d = (inputs.thickness_nm + t * inputs.jitter_nm).max(0.0);
                        film_reflectance(l, cos0, d, inputs.oxide, &inputs.metal)
                    })
                    .sum::<f64>()
                    / taps.len() as f64
            })
        })
        .collect()
}

/// Reflectance of the bare metal at normal incidence as linear RGB (a check against measured F0).
pub fn bare_metal_f0(metal: &MetalNk) -> [f32; 3] {
    spectrum_to_rgb(|l| film_reflectance(l, 1.0, 0.0, Oxide { a: 1.0, b: 0.0 }, metal))
}

/// Height of the anodize texture for `resident_rows` rows. Zero rows still allocate one row so the
/// bind group has a texture.
#[cfg(any(test, feature = "gpu"))]
pub(crate) fn lut_texture_height(resident_rows: usize) -> usize {
    resident_rows.max(1)
}

/// Value stored beside the uploaded anodize texture.
///
/// A placeholder built for zero resident rows must not use the same stamp as one real row, or the
/// next frame skips the upload and the metal samples black.
#[cfg(any(test, feature = "gpu"))]
pub(crate) fn lut_upload_stamp(resident_rows: usize) -> usize {
    resident_rows
}

/// Whether `uploaded_rows` already matches the table `ensure_lut_texture` would build.
#[cfg(any(test, feature = "gpu"))]
pub(crate) fn lut_upload_is_current(uploaded_rows: Option<usize>, resident_rows: usize) -> bool {
    uploaded_rows == Some(lut_upload_stamp(resident_rows))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ti() -> MetalNk {
        MetalNk {
            wavelength_nm: vec![450.0, 550.0, 650.0],
            n: vec![1.95, 2.15, 2.35],
            k: vec![2.45, 2.85, 3.25],
        }
    }

    #[test]
    fn zero_thickness_film_is_the_bare_metal() {
        let bare = bare_metal_f0(&ti());
        let lut = bake_anodize_lut(&AnodizeInputs {
            metal: ti(),
            oxide: Oxide { a: 2.15, b: 0.035 },
            thickness_nm: 0.0,
            jitter_nm: 0.0,
        });
        let last = lut[LUT_ENTRIES - 1];
        for c in 0..3 {
            assert!((last[c] - bare[c]).abs() < 1e-4, "{last:?} vs {bare:?}");
        }
        // Metal reflectance rises toward grazing.
        assert!(lut[1][1] > last[1]);
    }

    #[test]
    fn white_balance_maps_a_perfect_mirror_to_white() {
        let rgb = spectrum_to_rgb(|_| 1.0);
        for c in rgb {
            assert!((c - 1.0).abs() < 1e-5, "{rgb:?}");
        }
    }

    #[test]
    fn thin_films_are_colored_and_differ_by_thickness() {
        let bake = |d| {
            bake_anodize_lut(&AnodizeInputs {
                metal: ti(),
                oxide: Oxide { a: 2.15, b: 0.035 },
                thickness_nm: d,
                jitter_nm: 3.0,
            })[LUT_ENTRIES - 1]
        };
        let blue = bake(40.0);
        let gold = bake(122.0);
        assert!(blue[2] > blue[0], "a ~40 nm film reads blue: {blue:?}");
        assert!(
            gold[0] > gold[2],
            "a ~122 nm film (second order) reads warm: {gold:?}"
        );
    }

    #[test]
    fn short_nk_does_not_panic_and_stays_finite() {
        let metal = MetalNk {
            wavelength_nm: vec![450.0, 550.0, 650.0],
            n: vec![2.0],
            k: vec![],
        };
        let rgb = bare_metal_f0(&metal);
        assert!(rgb.iter().all(|c| c.is_finite()), "{rgb:?}");
    }

    #[test]
    fn duplicate_wavelengths_stay_finite() {
        let metal = MetalNk {
            wavelength_nm: vec![450.0, 450.0, 650.0],
            n: vec![2.0, 2.2, 2.4],
            k: vec![3.0, 3.1, 3.2],
        };
        let rgb = bare_metal_f0(&metal);
        assert!(rgb.iter().all(|c| c.is_finite() && *c >= 0.0), "{rgb:?}");
    }

    #[test]
    fn empty_lut_frame_does_not_count_as_one_row() {
        let stamp_after_empty = lut_upload_stamp(0);
        assert!(
            !lut_upload_is_current(Some(stamp_after_empty), 1),
            "placeholder stamp {stamp_after_empty} must not satisfy one resident row"
        );
        assert_eq!(lut_texture_height(0), 1);
        assert!(lut_upload_is_current(Some(lut_upload_stamp(1)), 1));
        assert!(lut_upload_is_current(Some(lut_upload_stamp(0)), 0));
        assert!(!lut_upload_is_current(Some(lut_upload_stamp(0)), 2));
    }

    #[test]
    fn complex_arithmetic() {
        let a = C::new(3.0, 4.0);
        assert!((a.norm2() - 25.0).abs() < 1e-12);
        let s = C::new(-4.0, 0.0).sqrt();
        assert!((s.re).abs() < 1e-12 && (s.im - 2.0).abs() < 1e-12);
        let q = a / a;
        assert!((q.re - 1.0).abs() < 1e-12 && q.im.abs() < 1e-12);
    }
}
