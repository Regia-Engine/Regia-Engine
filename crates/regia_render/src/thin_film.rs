// SPDX-License-Identifier: MPL-2.0
//! Thin-film interference over the base specular (`KHR_materials_iridescence`, ADR-1977).
//!
//! A dielectric film on a surface reflects from both of its faces, and the two reflections
//! interfere: the colour shifts with the view angle and the film's thickness (soap, oil, beetle
//! shells, bird feathers). [`ThinFilmFactors`] on a [`crate::LabMaterial`] turns the film on.
//!
//! **Model.** `shaders/thin_film.wgsl` evaluates the Airy reflectance of one film between air and
//! the base, averaged over the visible spectrum in closed form. The derivation is in the shader's
//! header; the functions here mirror it, and [`spectral_reflectance`] is the brute-force integral
//! the tests hold it to. The direct lobes take the film's Fresnel at `l·h`; the environment and
//! the multi-scatter energy take an F0 proxy at `n·v` ([`schlick_f0`]).
//!
//! **Cost.** Nothing for a material without the film: it draws through the standard pipelines and
//! its uniform block is byte for byte unchanged. A filmed material draws through the `THIN_FILM`
//! variant of the forward shader, from the CPU colour loop, with its own group 3 (a small uniform
//! block and the packed map). While that variant builds mid-play the material keeps the plain
//! base Fresnel, so a first use never stalls a frame ([`ThinFilmStats::fallback`]); warm frames
//! build it ahead of time.
//!
//! **Refused draws.** The forward pass draws the film only on a standard surface that binds
//! nothing else in group 3, is not x-rayed, and is not an opaque draw on a Mobile lite tier.
//! A transmissive surface (including per-instance glass, which is itself transmissive), a masked
//! instance, a lightmap receiver, a material graph, an x-ray override and an opaque film on a
//! Mobile lite tier draw without the film and count in [`ThinFilmStats::unsupported`]. The path
//! tracer attaches a film only when the material is not transmissive and has no material graph.
//! [`ThinFilmStats::transmissive`], [`ThinFilmStats::masked`] and [`ThinFilmStats::lightmapped`]
//! stay 0: those routes are not built (ADR-2732 is proposed).

use crate::LabTexture;

/// Default film index of refraction (`iridescenceIor`).
pub const THIN_FILM_DEFAULT_IOR: f32 = 1.3;
/// Default thickness where the thickness map reads 0, nanometres (`iridescenceThicknessMinimum`).
pub const THIN_FILM_DEFAULT_MIN_NM: f32 = 100.0;
/// Default thickness where the thickness map reads 1, and without a map, nanometres
/// (`iridescenceThicknessMaximum`).
pub const THIN_FILM_DEFAULT_MAX_NM: f32 = 400.0;
/// Below this thickness the film fades out (`FADE_NM` in the shader), so 0 nm is the base Fresnel.
pub const THIN_FILM_FADE_NM: f32 = 30.0;

/// Bytes of [`ThinFilmBlock`].
pub(crate) const THIN_FILM_BLOCK_BYTES: u64 = 16;

/// A material's thin film. [`Self::OFF`] (the default) is no film.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ThinFilmFactors {
    /// Share of the surface the film covers, `0..=1`. `0` is no film.
    pub factor: f32,
    /// Index of refraction of the film.
    pub ior: f32,
    /// Thickness where the thickness map reads 0, nanometres.
    pub thickness_min: f32,
    /// Thickness where the thickness map reads 1, and everywhere without a map, nanometres.
    pub thickness_max: f32,
}

impl ThinFilmFactors {
    /// No film; the other fields hold the extension's defaults.
    pub const OFF: Self = Self {
        factor: 0.0,
        ior: THIN_FILM_DEFAULT_IOR,
        thickness_min: THIN_FILM_DEFAULT_MIN_NM,
        thickness_max: THIN_FILM_DEFAULT_MAX_NM,
    };

    /// A material with these factors draws the film.
    #[must_use]
    pub fn is_on(&self) -> bool {
        self.factor > 0.0
    }

    /// The shader's block: non-finite values read as the defaults, out-of-range ones clamp.
    #[must_use]
    pub(crate) fn pack(&self) -> ThinFilmBlock {
        let or = |value: f32, default: f32| if value.is_finite() { value } else { default };
        ThinFilmBlock {
            params: [
                or(self.factor, 0.0).clamp(0.0, 1.0),
                or(self.ior, THIN_FILM_DEFAULT_IOR).max(1.0),
                or(self.thickness_min, THIN_FILM_DEFAULT_MIN_NM).max(0.0),
                or(self.thickness_max, THIN_FILM_DEFAULT_MAX_NM).max(0.0),
            ],
        }
    }
}

impl Default for ThinFilmFactors {
    fn default() -> Self {
        Self::OFF
    }
}

/// The film's uniform block (`ThinFilmBlock` in `lab_forward.wgsl`): coverage, film IOR,
/// thickness at map 0, thickness at map 1.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub(crate) struct ThinFilmBlock {
    pub(crate) params: [f32; 4],
}

/// What the forward pass did with thin-film materials in the last frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ThinFilmStats {
    /// Filmed materials with a GPU entry.
    pub materials: usize,
    /// Visible draws shaded with the film.
    pub drawn: usize,
    /// Visible draws that kept the base Fresnel while the variant builds.
    pub fallback: usize,
    /// Draws of a filmed material the variant cannot shade (a material graph, transmission,
    /// per-instance glass, a mask, a lightmap receiver, an x-ray override, an opaque or masked
    /// film on a lite tier).
    pub unsupported: usize,
    /// Of [`Self::drawn`]: transmissive surfaces. The shipped forward pass does not draw that
    /// route, so this stays 0.
    pub transmissive: usize,
    /// Of [`Self::drawn`]: masked instances. The shipped forward pass does not draw that route,
    /// so this stays 0.
    pub masked: usize,
    /// Of [`Self::drawn`]: lightmap receivers. The shipped forward pass does not draw that route,
    /// so this stays 0.
    pub lightmapped: usize,
}

/// The film's coverage in R and thickness in G, as one image for the shader's group 3 map:
/// `coverage.r` and `thickness.g` (the channels `KHR_materials_iridescence` names) at the larger
/// image's size. A missing input reads 1, which is what the extension says of a missing texture.
/// `None` without either.
#[must_use]
pub fn pack_thin_film_map(
    coverage: Option<&LabTexture>,
    thickness: Option<&LabTexture>,
) -> Option<LabTexture> {
    let (width, height) = match (coverage, thickness) {
        (Some(a), Some(b)) => (a.width.max(b.width), a.height.max(b.height)),
        (Some(a), None) | (None, Some(a)) => (a.width, a.height),
        (None, None) => return None,
    };
    let (width, height) = (width.max(1), height.max(1));
    let read = |t: Option<&LabTexture>, u: f32, v: f32, channel: usize| -> u8 {
        let Some(t) = t else { return 255 };
        let (w, h) = (t.width.max(1) as usize, t.height.max(1) as usize);
        let x = ((u * w as f32) as usize).min(w - 1);
        let y = ((v * h as f32) as usize).min(h - 1);
        t.rgba.get((y * w + x) * 4 + channel).copied().unwrap_or(255)
    };
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for y in 0..height {
        let v = (y as f32 + 0.5) / height as f32;
        for x in 0..width {
            let u = (x as f32 + 0.5) / width as f32;
            rgba.extend_from_slice(&[read(coverage, u, v, 0), read(thickness, u, v, 1), 0, 255]);
        }
    }
    Some(LabTexture { width, height, rgba })
}

// ---------------------------------------------------------------------------
// CPU mirror of `shaders/thin_film.wgsl`
// ---------------------------------------------------------------------------

/// Lobes: X, X, X, Y, Y, Z, Z.
const LOBE_MU: [f32; 7] = [1.63928, 2.24077, 1.75523, 1.79778, 1.66969, 2.31791, 2.209];
const LOBE_SIGMA: [f32; 7] = [0.07153, 0.10022, 0.0787, 0.12961, 0.06761, 0.0175, 0.11156];
const WEIGHT_X: [f32; 3] = [0.5339, 0.16276, 0.30334];
const WEIGHT_Y: [f32; 2] = [0.92041, 0.07959];
const WEIGHT_Z: [f32; 2] = [0.03131, 0.96869];
const TO_RGB: [[f32; 3]; 3] = [
    [3.079954, -1.537138, -0.542816],
    [-0.921258, 1.876011, 0.045247],
    [0.052887, -0.204026, 1.151139],
];

fn to_rgb(v: [f32; 7]) -> [f32; 3] {
    let xyz = [
        WEIGHT_X[0] * v[0] + WEIGHT_X[1] * v[1] + WEIGHT_X[2] * v[2],
        WEIGHT_Y[0] * v[3] + WEIGHT_Y[1] * v[4],
        WEIGHT_Z[0] * v[5] + WEIGHT_Z[1] * v[6],
    ];
    TO_RGB.map(|row| row[0] * xyz[0] + row[1] * xyz[1] + row[2] * xyz[2])
}

fn dielectric_fresnel(cos_1: f32, cos_2: f32, n1: f32, n2: f32) -> f32 {
    let rs = (n1 * cos_1 - n2 * cos_2) / (n1 * cos_1 + n2 * cos_2).max(1e-6);
    let rp = (n2 * cos_1 - n1 * cos_2) / (n2 * cos_1 + n1 * cos_2).max(1e-6);
    0.5 * (rs * rs + rp * rp)
}

/// The two interface reflectances per channel and the sign of their amplitude product.
struct Interfaces {
    r1: f32,
    r2: [f32; 3],
    sign: [f32; 3],
    /// Cosine inside the film.
    c2: f32,
}

fn interfaces(cos_i: f32, n2: f32, f0: [f32; 3]) -> Interfaces {
    let c1 = cos_i.clamp(0.0, 1.0);
    let c2 = (1.0 - (1.0 - c1 * c1) / (n2 * n2)).max(0.0).sqrt();
    let r1 = dielectric_fresnel(c1, c2, 1.0, n2);
    let x = 1.0 - c2;
    let x5 = x * x * x * x * x;
    let mut r2 = [0.0; 3];
    let mut sign = [0.0; 3];
    for k in 0..3 {
        let root = f0[k].clamp(0.0, 0.9998).sqrt();
        let n3 = (1.0 + root) / (1.0 - root);
        let r23 = (n3 - n2) / (n3 + n2);
        let normal = r23 * r23;
        r2[k] = normal + (1.0 - normal) * x5;
        sign[k] = if n3 >= n2 { 1.0 } else { -1.0 };
    }
    Interfaces { r1, r2, sign, c2 }
}

/// CPU mirror of `thin_film.wgsl::reflectance`: the film's RGB reflectance at `cos_i` from air,
/// `thickness` nanometres of index `ior` over a base of normal-incidence reflectance `f0`.
#[must_use]
pub fn reflectance(cos_i: f32, ior: f32, thickness: f32, f0: [f32; 3]) -> [f32; 3] {
    let n2 = ior.max(1.0);
    let Interfaces { r1, r2, sign, c2 } = interfaces(cos_i, n2, f0);
    let p = std::f32::consts::TAU * 2.0 * n2 * thickness.max(0.0) * c2 * 0.001;
    let mut c = [0.0; 7];
    let mut e = [0.0; 7];
    for j in 0..7 {
        c[j] = (p * LOBE_MU[j]).cos();
        let pe = p * LOBE_SIGMA[j];
        e[j] = (-0.5 * pe * pe).exp();
    }
    // Harmonic `n` of lobe `j`: Chebyshev's recurrence for the cosine, e^(n²) for the Gaussian.
    let harmonic = |n: i32| -> [f32; 3] {
        to_rgb(std::array::from_fn(|j| {
            let (mut before, mut now) = (1.0, c[j]);
            for _ in 1..n {
                (before, now) = (now, 2.0 * c[j] * now - before);
            }
            now * e[j].powi(n * n)
        }))
    };
    let (s1, s2, s3, s4, s5) = (harmonic(1), harmonic(2), harmonic(3), harmonic(4), harmonic(5));
    std::array::from_fn(|k| {
        let a = (r1 * r2[k]).sqrt();
        let big_k = (1.0 - r1) * (1.0 - r2[k]) / (1.0 - r1 * r2[k]).max(1e-5);
        let m1 = -sign[k] * a;
        let m2 = m1 * m1;
        let m3 = m2 * m1;
        let m4 = m2 * m2;
        // Harmonic 5 and every one past it: the rest of the geometric series. The ratio nears 1
        // when both interfaces nearly totally reflect; the floor keeps the tail finite there
        // instead of 0 × inf = NaN (audit wave 5, render L1).
        let m5 = m4 * m1 / (1.0 - m1).max(1e-3);
        let sum = m1 * s1[k] + m2 * s2[k] + m3 * s3[k] + m4 * s4[k] + m5 * s5[k];
        (1.0 - big_k * (1.0 + 2.0 * sum)).clamp(0.0, 1.0)
    })
}

/// CPU mirror of `thin_film.wgsl::weight`: the authored coverage, faded out below
/// [`THIN_FILM_FADE_NM`].
#[must_use]
pub fn weight(coverage: f32, thickness: f32) -> f32 {
    let t = (thickness / THIN_FILM_FADE_NM).clamp(0.0, 1.0);
    coverage.clamp(0.0, 1.0) * t * t * (3.0 - 2.0 * t)
}

fn schlick(cos_i: f32, f0: f32) -> f32 {
    let x = 1.0 - cos_i.clamp(0.0, 1.0);
    f0 + (1.0 - f0) * (x * x * x * x * x)
}

/// CPU mirror of `thin_film.wgsl::fresnel`: Schlick of `f0` mixed with the film by `cover`.
#[must_use]
pub fn fresnel(cos_i: f32, f0: [f32; 3], cover: f32, ior: f32, thickness: f32) -> [f32; 3] {
    let film = reflectance(cos_i, ior, thickness, f0);
    std::array::from_fn(|k| {
        let base = schlick(cos_i, f0[k]);
        base + (film[k] - base) * cover
    })
}

/// What crosses a filmed transmissive sheet per channel at `no_v`, before the base colour and the
/// volume: `1 - F` with the film's Fresnel over a dielectric of `f0` (`fs_transmit`, and `pt.wgsl`'s
/// `transmit`). `cover` 0 is the clear sheet's `1 - schlick(no_v, f0)`.
#[must_use]
pub fn transmitted(no_v: f32, f0: f32, cover: f32, ior: f32, thickness: f32) -> [f32; 3] {
    fresnel(no_v, [f0; 3], cover, ior, thickness).map(|f| 1.0 - f)
}

/// CPU mirror of `thin_film.wgsl::schlick_f0`: the F0 that gives the film's reflectance at `no_v`
/// through Schlick's curve, mixed with `f0` by `cover`.
#[must_use]
pub fn schlick_f0(no_v: f32, f0: [f32; 3], cover: f32, ior: f32, thickness: f32) -> [f32; 3] {
    let x = 1.0 - no_v.clamp(0.0, 1.0);
    let fc = x * x * x * x * x;
    let film = reflectance(no_v, ior, thickness, f0);
    std::array::from_fn(|k| {
        let proxy = ((film[k] - fc) / (1.0 - fc).max(0.05)).clamp(0.0, 1.0);
        f0[k] + (proxy - f0[k]) * cover
    })
}

// ---------------------------------------------------------------------------
// The reference the fit answers to
// ---------------------------------------------------------------------------

fn lobe(l: f64, mu: f64, below: f64, above: f64) -> f64 {
    let s = if l < mu { below } else { above };
    (-0.5 * ((l - mu) / s).powi(2)).exp()
}

/// CIE 1931 colour matching functions at `l` nanometres, in the multi-lobe analytic form of Wyman,
/// Sloan & Shirley, JCGT 2(2), 2013.
fn cmf(l: f64) -> [f64; 3] {
    [
        1.056 * lobe(l, 599.8, 37.9, 31.0) + 0.362 * lobe(l, 442.0, 16.0, 26.7)
            - 0.065 * lobe(l, 501.1, 20.4, 26.2),
        0.821 * lobe(l, 568.8, 46.9, 40.5) + 0.286 * lobe(l, 530.9, 16.3, 31.1),
        1.217 * lobe(l, 437.0, 11.8, 36.0) + 0.681 * lobe(l, 459.0, 26.0, 13.8),
    ]
}

/// The film's RGB reflectance by brute force: the exact Airy formula at each wavelength from 340 to
/// 900 nm, under an equal-energy spectrum, through the CIE curves (`cmf`) to the same RGB as
/// [`reflectance`]. It takes the same interface model, so the difference between the two is the
/// lobe fit and the truncation of the series alone.
#[must_use]
pub fn spectral_reflectance(cos_i: f32, ior: f32, thickness: f32, f0: [f32; 3]) -> [f32; 3] {
    let n2 = ior.max(1.0);
    let Interfaces { r1, r2, sign, c2 } = interfaces(cos_i, n2, f0);
    let opd = 2.0 * f64::from(n2) * f64::from(thickness.max(0.0)) * f64::from(c2);
    let mut out = [0.0f32; 3];
    for k in 0..3 {
        let (r1, r2k) = (f64::from(r1), f64::from(r2[k]));
        let a = (r1 * r2k).sqrt() * f64::from(sign[k]);
        let mut sum = [0.0f64; 3];
        let mut white = [0.0f64; 3];
        let mut l = 340.0f64;
        while l < 900.0 {
            let d = std::f64::consts::TAU * opd / l;
            let r = (r1 + r2k + 2.0 * a * d.cos()) / (1.0 + r1 * r2k + 2.0 * a * d.cos());
            let c = cmf(l);
            for i in 0..3 {
                sum[i] += c[i] * r;
                white[i] += c[i];
            }
            l += 0.25;
        }
        let xyz: [f64; 3] = std::array::from_fn(|i| sum[i] / white[i]);
        let row = TO_RGB[k].map(f64::from);
        out[k] = (row[0] * xyz[0] + row[1] * xyz[1] + row[2] * xyz[2]).clamp(0.0, 1.0) as f32;
    }
    out
}

/// Rust and WGSL struct rows of this module (`shader_contract`, ADR-3002).
pub(crate) fn contract_rows() -> Vec<crate::ContractRow> {
    vec![crate::contract_row!(
        "lab_forward.wgsl",
        "ThinFilmBlock",
        ThinFilmBlock { params }
    )]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Audit wave 5 (render M1): with the film on, the skin's second specular lobe takes the
    /// film's Fresnel like the first, at both the sun and the local-light sites of
    /// `lab_forward.wgsl`. A static check of the shader source: every plain second lobe is the
    /// `#else` arm of a `THIN_FILM` film lobe.
    #[test]
    fn the_skin_second_lobe_carries_the_film() {
        let source = include_str!("../shaders/lab_forward.wgsl");
        let lines: Vec<&str> = source.lines().map(str::trim).collect();
        let plain: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("let iso2 = regia::pbr::brdf::isotropic_lobes("))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(plain.len(), 2, "the sun and the local-light sites");
        for i in plain {
            assert_eq!(lines[i - 1], "#else", "line {}", i + 1);
            assert!(
                lines[i - 2].starts_with("let iso2 = regia::pbr::brdf::isotropic_lobes_film("),
                "line {}",
                i - 1
            );
            assert_eq!(lines[i - 3], "#ifdef THIN_FILM", "line {}", i - 2);
            assert_eq!(lines[i + 1], "#endif", "line {}", i + 2);
        }
    }

    /// Audit wave 5 (render L1): with both interfaces near total reflection (a huge film IOR, a
    /// mirror base, grazing light) the reflectance stays finite and in [0, 1].
    #[test]
    fn reflectance_stays_finite_where_the_series_ratio_nears_one() {
        for ior in [1.0, 1.33, 3.0, 200.0, 1.0e4] {
            for cos_i in [0.0, 1.0e-4, 1.0e-2, 0.5, 1.0] {
                for f0 in [[0.0; 3], [0.04; 3], [1.0; 3], [0.95, 0.64, 0.54]] {
                    for thickness in [0.0, 100.0, 350.0, 1200.0] {
                        let r = reflectance(cos_i, ior, thickness, f0);
                        assert!(
                            r.iter().all(|c| c.is_finite() && (0.0..=1.0).contains(c)),
                            "ior {ior} cos {cos_i} f0 {f0:?} t {thickness}: {r:?}"
                        );
                    }
                }
            }
        }
    }

    fn close(a: [f32; 3], b: [f32; 3], bound: f32) -> bool {
        (0..3).all(|k| (a[k] - b[k]).abs() <= bound)
    }

    #[test]
    fn the_shader_constants_equal_the_mirror() {
        let wgsl = include_str!("../shaders/thin_film.wgsl");
        let number = |v: f32| format!("{v:?}");
        let lists = [
            LOBE_MU[..4]
                .iter()
                .copied()
                .map(number)
                .collect::<Vec<_>>()
                .join(", "),
            LOBE_MU[4..]
                .iter()
                .copied()
                .map(number)
                .collect::<Vec<_>>()
                .join(", "),
            LOBE_SIGMA[..4]
                .iter()
                .copied()
                .map(number)
                .collect::<Vec<_>>()
                .join(", "),
            LOBE_SIGMA[4..]
                .iter()
                .copied()
                .map(number)
                .collect::<Vec<_>>()
                .join(", "),
            WEIGHT_X.map(number).join(", "),
            WEIGHT_Y.map(number).join(", "),
            WEIGHT_Z.map(number).join(", "),
            TO_RGB[0].map(number).join(", "),
            TO_RGB[1].map(number).join(", "),
            TO_RGB[2].map(number).join(", "),
        ];
        for list in &lists {
            assert!(wgsl.contains(list.as_str()), "{list} is not in thin_film.wgsl");
        }
        assert!(wgsl.contains(&format!("FADE_NM: f32 = {THIN_FILM_FADE_NM:?}")));
    }

    #[test]
    fn a_wavelength_independent_reflectance_stays_white() {
        for row in TO_RGB {
            assert!((row.iter().sum::<f32>() - 1.0).abs() < 2e-6, "{row:?}");
        }
        assert!((WEIGHT_X.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert!((WEIGHT_Y.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        assert!((WEIGHT_Z.iter().sum::<f32>() - 1.0).abs() < 1e-6);
        // A thick film averages every interference away: the same in R, G and B.
        let thick = reflectance(0.8, 1.4, 20_000.0, [0.04; 3]);
        assert!(close(thick, [thick[0]; 3], 1e-4), "{thick:?}");
    }

    /// The fit against the brute-force spectral integral over the ranges a material authors,
    /// dielectric to metal. Grazing on a reflective base the interfaces reflect most and the
    /// harmonics past the kept ones weigh most, so that rim has its own, wider bound.
    #[test]
    fn the_closed_form_matches_the_spectral_integral() {
        let (mut worst, mut worst_grazing) = (0.0f32, 0.0f32);
        let mut mean = 0.0f32;
        let mut count = 0.0f32;
        for f0 in [[0.04; 3], [0.2; 3], [0.95, 0.64, 0.54], [0.56, 0.57, 0.58]] {
            for ior in [1.2f32, 1.3, 1.5, 1.8, 2.2] {
                for thickness in (0..=24).map(|i| i as f32 * 50.0) {
                    for cos_i in [1.0f32, 0.85, 0.7, 0.5, 0.3, 0.15] {
                        let fast = reflectance(cos_i, ior, thickness, f0);
                        let slow = spectral_reflectance(cos_i, ior, thickness, f0);
                        for k in 0..3 {
                            let e = (fast[k] - slow[k]).abs();
                            if cos_i < 0.3 {
                                worst_grazing = worst_grazing.max(e);
                            } else {
                                worst = worst.max(e);
                            }
                            mean += e;
                            count += 1.0;
                        }
                    }
                }
            }
        }
        mean /= count;
        eprintln!("thin film fit: worst {worst:.4}, at the rim {worst_grazing:.4}, mean {mean:.5}");
        assert!(
            worst < 0.012 && worst_grazing < 0.05 && mean < 0.003,
            "worst {worst}, at the rim {worst_grazing}, mean {mean}"
        );
    }

    /// Hand values: a quarter-wave film at normal incidence reflects
    /// `((ns - n²) / (ns + n²))²` of its design wavelength (Born & Wolf ch. 7.6). Green sits near
    /// the design wavelength of 552 nm, on glass of index 1.52: MgF2 (n = 1.38) gives 0.0126, a
    /// high-index film (n = 2.2) 0.2725; bare glass is 0.0426.
    #[test]
    fn quarter_wave_films_match_the_textbook_values() {
        let glass = ((1.52f32 - 1.0) / 2.52).powi(2);
        let quarter = |n: f32| ((1.52 - n * n) / (1.52 + n * n)).powi(2);
        assert!((quarter(1.38) - 0.0126).abs() < 1e-3);
        assert!((quarter(2.2) - 0.2725).abs() < 1e-3);
        for (n, nm) in [(1.38f32, 100.0f32), (2.2, 62.7)] {
            for r in [
                spectral_reflectance(1.0, n, nm, [glass; 3]),
                reflectance(1.0, n, nm, [glass; 3]),
            ] {
                assert!((r[1] - quarter(n)).abs() < 0.005, "n {n}: {r:?}");
            }
        }
    }

    #[test]
    fn at_normal_incidence_no_thickness_is_the_base() {
        for f0 in [0.04f32, 0.2, 0.6, 0.9] {
            for ior in [1.2f32, 1.5, 2.0] {
                let r = reflectance(1.0, ior, 0.0, [f0; 3]);
                assert!(close(r, [f0; 3], 2e-3), "f0 {f0} ior {ior}: {r:?}");
            }
        }
    }

    #[test]
    fn the_fade_makes_zero_thickness_exactly_the_base_fresnel() {
        assert_eq!(weight(1.0, 0.0), 0.0);
        assert_eq!(weight(0.5, 500.0), 0.5);
        for cos_i in [1.0f32, 0.6, 0.2] {
            let f = fresnel(cos_i, [0.04; 3], weight(1.0, 0.0), 1.3, 0.0);
            assert_eq!(f, [schlick(cos_i, 0.04); 3]);
        }
    }

    #[test]
    fn no_cover_is_schlick_and_full_cover_is_the_film() {
        let f0 = [0.9, 0.6, 0.3];
        assert_eq!(fresnel(0.5, f0, 0.0, 1.3, 300.0), f0.map(|v| schlick(0.5, v)));
        assert_eq!(
            fresnel(0.5, f0, 1.0, 1.3, 300.0),
            reflectance(0.5, 1.3, 300.0, f0)
        );
        assert_eq!(schlick_f0(0.5, f0, 0.0, 1.3, 300.0), f0);
    }

    #[test]
    fn the_light_that_crosses_a_film_is_what_it_does_not_reflect() {
        // No cover is the clear sheet, exactly.
        let clear = 1.0 - schlick(0.7, 0.04);
        assert_eq!(transmitted(0.7, 0.04, 0.0, 1.33, 350.0), [clear; 3]);
        // A full film tints it by channel and stays in range.
        let film = transmitted(0.7, 0.04, 1.0, 1.8, 350.0);
        assert!((film[0] - film[2]).abs() > 1e-3, "{film:?}");
        assert!(film.iter().all(|c| (0.0..=1.0).contains(c)), "{film:?}");
        let reflected = fresnel(0.7, [0.04; 3], 1.0, 1.8, 350.0);
        assert!(close(film, reflected.map(|f| 1.0 - f), 0.0), "{film:?}");
    }

    #[test]
    fn the_film_colour_shifts_with_the_view_and_the_thickness() {
        let f0 = [0.04; 3];
        let head_on = reflectance(1.0, 1.33, 350.0, f0);
        let side = reflectance(0.4, 1.33, 350.0, f0);
        assert!(!close(head_on, side, 0.01), "{head_on:?} {side:?}");
        let thin = reflectance(1.0, 1.33, 200.0, f0);
        assert!(!close(head_on, thin, 0.01), "{head_on:?} {thin:?}");
        // Grazing: everything reflects, film or no film.
        let grazing = reflectance(0.0, 1.33, 350.0, f0);
        assert!(close(grazing, [1.0; 3], 1e-3), "{grazing:?}");
    }

    #[test]
    fn the_reflectance_stays_in_range_for_any_input() {
        for cos_i in [-1.0f32, 0.0, 0.5, 1.0, 2.0] {
            for ior in [0.5f32, 1.0, 1.3, 3.0, 8.0] {
                for thickness in [-10.0f32, 0.0, 1.0, 400.0, 1e6] {
                    for f0 in [[0.0; 3], [1.0; 3], [0.5; 3]] {
                        for v in reflectance(cos_i, ior, thickness, f0) {
                            assert!(v.is_finite() && (0.0..=1.0).contains(&v), "{v}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn the_schlick_proxy_gives_back_the_film_at_n_dot_v() {
        let f0 = [0.04; 3];
        for no_v in [1.0f32, 0.7, 0.4] {
            let proxy = schlick_f0(no_v, f0, 1.0, 1.4, 320.0);
            let film = reflectance(no_v, 1.4, 320.0, f0);
            let back = proxy.map(|p| schlick(no_v, p));
            assert!(close(back, film, 1e-4), "{back:?} {film:?}");
        }
    }

    #[test]
    fn pack_clamps_and_replaces_non_finite_values() {
        let block = ThinFilmFactors {
            factor: 4.0,
            ior: f32::NAN,
            thickness_min: -5.0,
            thickness_max: f32::INFINITY,
        }
        .pack();
        assert_eq!(block.params, [1.0, 1.3, 0.0, 400.0]);
        assert!(!ThinFilmFactors::OFF.is_on());
        assert!(
            !ThinFilmFactors {
                factor: -1.0,
                ..ThinFilmFactors::OFF
            }
            .is_on()
        );
        assert_eq!(size_of::<ThinFilmBlock>() as u64, THIN_FILM_BLOCK_BYTES);
    }

    #[test]
    fn the_packed_map_reads_coverage_red_and_thickness_green() {
        let coverage = LabTexture {
            width: 2,
            height: 1,
            rgba: vec![10, 200, 200, 200, 20, 200, 200, 200],
        };
        let thickness = LabTexture {
            width: 1,
            height: 1,
            rgba: vec![200, 77, 200, 200],
        };
        let packed = pack_thin_film_map(Some(&coverage), Some(&thickness)).expect("packed");
        assert_eq!((packed.width, packed.height), (2, 1));
        assert_eq!(packed.rgba, vec![10, 77, 0, 255, 20, 77, 0, 255]);
        // A missing texture reads 1 (full coverage, the maximum thickness).
        let alone = pack_thin_film_map(None, Some(&thickness)).expect("packed");
        assert_eq!(alone.rgba, vec![255, 77, 0, 255]);
        assert!(pack_thin_film_map(None, None).is_none());
    }
}
