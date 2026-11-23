// SPDX-License-Identifier: MPL-2.0
//! Image metrics for scoring a real-time frame against the reference path tracer
//! (`docs/specs/27-renderer-excellence.md` § 2): FLIP, SSIM and RMSE.
//!
//! - **FLIP** (LDR): Andersson, Nilsson, Akenine-Möller, Oskarsson, Åström, Fairchild, "FLIP: A
//!   Difference Evaluator for Alternating Images", HPG 2020. Re-implemented from the paper: colour
//!   pipeline (linearised CIELab opponent space, contrast-sensitivity filters as sums of
//!   Gaussians, Hunt-adjusted HyAB distance, the paper's error mapping) and feature pipeline
//!   (Gaussian edge and point detectors on the achromatic channel), combined as
//!   `ΔE_c^(1 − ΔE_f)`. No code was ported.
//! - **SSIM**: Wang, Bovik, Sheikh, Simoncelli 2004, 11×11 Gaussian window (σ 1.5) on luma.
//! - **RMSE**: on linear radiance, absolute and relative to the reference mean.
//!
//! Inputs to FLIP and SSIM are display-encoded `[0, 1]` RGB (both frames through the same AgX,
//! [`display_encode`]); RMSE takes the linear frames.

/// Pixels per degree of the paper's default viewer (0.7 m from a 0.7 m wide 3840-pixel display).
pub const DEFAULT_PPD: f32 = 67.0;

/// FLIP output: the per-pixel error map and its statistics.
#[derive(Clone, Debug, PartialEq)]
pub struct FlipResult {
    /// Per-pixel error in `[0, 1]`, row-major.
    pub map: Vec<f32>,
    /// Mean error.
    pub mean: f32,
    /// 95th percentile.
    pub p95: f32,
    /// Largest error.
    pub max: f32,
}

/// Everything `seeing.compare` reports for one image pair.
#[derive(Clone, Debug, PartialEq)]
pub struct CompareReport {
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
    /// FLIP on the display-encoded pair.
    pub flip_mean: f32,
    /// FLIP 95th percentile.
    pub flip_p95: f32,
    /// SSIM on display luma.
    pub ssim: f32,
    /// RMSE of linear luminance.
    pub rmse: f32,
    /// RMSE divided by the reference's mean linear luminance.
    pub rel_rmse: f32,
    /// Reference mean linear luminance.
    pub mean_reference: f32,
    /// Test mean linear luminance.
    pub mean_test: f32,
}

impl CompareReport {
    /// JSON object text (no serde dependency in the renderer).
    #[must_use]
    pub fn to_json(&self) -> String {
        // JSON has no inf/NaN: a relative error against a zero reference is `null`.
        let num = |v: f32, exp: bool| {
            if !v.is_finite() {
                "null".to_string()
            } else if exp {
                format!("{v:.6e}")
            } else {
                format!("{v:.6}")
            }
        };
        format!(
            "{{\"width\":{},\"height\":{},\"flip_mean\":{},\"flip_p95\":{},\"ssim\":{},\
             \"rmse\":{},\"rel_rmse\":{},\"mean_reference\":{},\"mean_test\":{}}}",
            self.width,
            self.height,
            num(self.flip_mean, false),
            num(self.flip_p95, false),
            num(self.ssim, false),
            num(self.rmse, true),
            num(self.rel_rmse, false),
            num(self.mean_reference, true),
            num(self.mean_test, true)
        )
    }
}

fn luma(c: [f32; 3]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// The runtime's display transform on linear radiance: exposure at `ev100`, AgX, default look.
#[must_use]
pub fn display_encode(linear: &[[f32; 3]], ev100: f32) -> Vec<[f32; 3]> {
    let exposure = crate::exposure_from_ev100(ev100);
    linear
        .iter()
        .map(|c| {
            let e = c.map(|v| {
                if v.is_finite() {
                    (v * exposure).clamp(0.0, 1.0e4)
                } else {
                    0.0
                }
            });
            crate::tonemap::display_look(
                crate::tonemap::tonemap_agx(e),
                crate::DEFAULT_LOOK_CONTRAST,
                crate::DEFAULT_LOOK_SATURATION,
            )
        })
        .collect()
}

// ---- Colour spaces (sRGB primaries, D65) ----

fn srgb_to_linear(c: f32) -> f32 {
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

const WHITE: [f32; 3] = [0.950_47, 1.0, 1.088_83];

fn linear_to_xyz(c: [f32; 3]) -> [f32; 3] {
    [
        0.412_456_4 * c[0] + 0.357_576_1 * c[1] + 0.180_437_5 * c[2],
        0.212_672_9 * c[0] + 0.715_152_2 * c[1] + 0.072_175 * c[2],
        0.019_333_9 * c[0] + 0.119_192 * c[1] + 0.950_304_1 * c[2],
    ]
}

fn xyz_to_linear(c: [f32; 3]) -> [f32; 3] {
    [
        3.240_454_2 * c[0] - 1.537_138_5 * c[1] - 0.498_531_4 * c[2],
        -0.969_266 * c[0] + 1.876_010_8 * c[1] + 0.041_556 * c[2],
        0.055_643_4 * c[0] - 0.204_025_9 * c[1] + 1.057_225_2 * c[2],
    ]
}

/// Linearised CIELab ("YyCxCz"): CIELab without the cube root.
fn xyz_to_ycxcz(c: [f32; 3]) -> [f32; 3] {
    let (x, y, z) = (c[0] / WHITE[0], c[1] / WHITE[1], c[2] / WHITE[2]);
    [116.0 * y - 16.0, 500.0 * (x - y), 200.0 * (y - z)]
}

fn ycxcz_to_xyz(c: [f32; 3]) -> [f32; 3] {
    let y = (c[0] + 16.0) / 116.0;
    let x = c[1] / 500.0 + y;
    let z = y - c[2] / 200.0;
    [x * WHITE[0], y * WHITE[1], z * WHITE[2]]
}

fn lab_f(t: f32) -> f32 {
    let d = 6.0f32 / 29.0;
    if t > d * d * d {
        t.cbrt()
    } else {
        t / (3.0 * d * d) + 4.0 / 29.0
    }
}

fn xyz_to_lab(c: [f32; 3]) -> [f32; 3] {
    let (fx, fy, fz) = (
        lab_f(c[0] / WHITE[0]),
        lab_f(c[1] / WHITE[1]),
        lab_f(c[2] / WHITE[2]),
    );
    [116.0 * fy - 16.0, 500.0 * (fx - fy), 200.0 * (fy - fz)]
}

/// Hunt adjustment of the chroma by lightness.
fn hunt(lab: [f32; 3]) -> [f32; 3] {
    [lab[0], 0.01 * lab[0] * lab[1], 0.01 * lab[0] * lab[2]]
}

fn hyab(a: [f32; 3], b: [f32; 3]) -> f32 {
    (a[0] - b[0]).abs() + ((a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

// ---- Filters ----

/// 2-D convolution with clamp-to-edge borders.
fn convolve(src: &[f32], w: usize, h: usize, kernel: &[f32], radius: usize) -> Vec<f32> {
    let size = 2 * radius + 1;
    let mut out = vec![0.0f32; w * h];
    for y in 0..h {
        for x in 0..w {
            let mut acc = 0.0f32;
            for ky in 0..size {
                let sy = (y as isize + ky as isize - radius as isize).clamp(0, h as isize - 1) as usize;
                let row = &src[sy * w..sy * w + w];
                let krow = &kernel[ky * size..ky * size + size];
                for (kx, k) in krow.iter().enumerate() {
                    let sx = (x as isize + kx as isize - radius as isize).clamp(0, w as isize - 1) as usize;
                    acc += k * row[sx];
                }
            }
            out[y * w + x] = acc;
        }
    }
    out
}

/// Contrast-sensitivity filters (the paper's sum-of-Gaussians CSF fits) for the Y, Cx, Cz
/// channels, normalised to unit sum, and their shared radius.
fn csf_kernels(ppd: f32) -> ([Vec<f32>; 3], usize) {
    // (a1, b1, a2, b2) per channel: achromatic, red-green, blue-yellow.
    let params = [
        (1.0f32, 0.0047f32, 0.0f32, 1.0e-5f32),
        (1.0, 0.0053, 0.0, 1.0e-5),
        (34.1, 0.04, 13.5, 0.025),
    ];
    let pi2 = std::f32::consts::PI * std::f32::consts::PI;
    let b_max = 0.04f32;
    let radius = (3.0 * (b_max / (2.0 * pi2)).sqrt() * ppd).ceil().max(1.0) as usize;
    let size = 2 * radius + 1;
    let kernels = params.map(|(a1, b1, a2, b2)| {
        let mut k = vec![0.0f32; size * size];
        for y in 0..size {
            for x in 0..size {
                let dx = (x as f32 - radius as f32) / ppd;
                let dy = (y as f32 - radius as f32) / ppd;
                let r2 = dx * dx + dy * dy;
                let g = |a: f32, b: f32| a * (std::f32::consts::PI / b).sqrt() * (-pi2 * r2 / b).exp();
                k[y * size + x] = g(a1, b1) + g(a2, b2);
            }
        }
        let sum: f32 = k.iter().sum();
        k.iter_mut().for_each(|v| *v /= sum);
        k
    });
    (kernels, radius)
}

/// Edge (first-derivative) and point (second-derivative) Gaussian detectors along X, with
/// positive and negative lobes each normalised to ±1.
fn feature_kernels(ppd: f32) -> (Vec<f32>, Vec<f32>, usize) {
    let sigma = 0.5 * 0.082 * ppd;
    let radius = (3.0 * sigma).ceil().max(1.0) as usize;
    let size = 2 * radius + 1;
    let mut edge = vec![0.0f32; size * size];
    let mut point = vec![0.0f32; size * size];
    for y in 0..size {
        for x in 0..size {
            let dx = x as f32 - radius as f32;
            let dy = y as f32 - radius as f32;
            let g = (-(dx * dx + dy * dy) / (2.0 * sigma * sigma)).exp();
            edge[y * size + x] = -dx * g;
            point[y * size + x] = (dx * dx / (sigma * sigma) - 1.0) * g;
        }
    }
    for k in [&mut edge, &mut point] {
        let pos: f32 = k.iter().filter(|v| **v > 0.0).sum();
        let neg: f32 = -k.iter().filter(|v| **v < 0.0).sum::<f32>();
        for v in k.iter_mut() {
            if *v > 0.0 {
                *v /= pos;
            } else if *v < 0.0 {
                *v /= neg;
            }
        }
    }
    (edge, point, radius)
}

fn transpose_kernel(k: &[f32], radius: usize) -> Vec<f32> {
    let size = 2 * radius + 1;
    let mut t = vec![0.0; size * size];
    for y in 0..size {
        for x in 0..size {
            t[x * size + y] = k[y * size + x];
        }
    }
    t
}

struct Features {
    edge: Vec<f32>,
    point: Vec<f32>,
}

fn features(y: &[f32], w: usize, h: usize, kernels: &(Vec<f32>, Vec<f32>, usize)) -> Features {
    let (edge_x, point_x, radius) = kernels;
    let edge_y = transpose_kernel(edge_x, *radius);
    let point_y = transpose_kernel(point_x, *radius);
    let ex = convolve(y, w, h, edge_x, *radius);
    let ey = convolve(y, w, h, &edge_y, *radius);
    let px = convolve(y, w, h, point_x, *radius);
    let py = convolve(y, w, h, &point_y, *radius);
    Features {
        edge: ex.iter().zip(&ey).map(|(a, b)| (a * a + b * b).sqrt()).collect(),
        point: px.iter().zip(&py).map(|(a, b)| (a * a + b * b).sqrt()).collect(),
    }
}

fn filtered_hunt_lab(srgb: &[[f32; 3]], w: usize, h: usize, csf: &([Vec<f32>; 3], usize)) -> Vec<[f32; 3]> {
    let opp: Vec<[f32; 3]> = srgb
        .iter()
        .map(|c| xyz_to_ycxcz(linear_to_xyz(c.map(|v| srgb_to_linear(v.clamp(0.0, 1.0))))))
        .collect();
    let mut channels = Vec::with_capacity(3);
    for k in 0..3 {
        let plane: Vec<f32> = opp.iter().map(|c| c[k]).collect();
        channels.push(convolve(&plane, w, h, &csf.0[k], csf.1));
    }
    (0..w * h)
        .map(|i| {
            let rgb = xyz_to_linear(ycxcz_to_xyz([channels[0][i], channels[1][i], channels[2][i]]));
            hunt(xyz_to_lab(linear_to_xyz(rgb.map(|v| v.clamp(0.0, 1.0)))))
        })
        .collect()
}

fn normalized_luminance(srgb: &[[f32; 3]]) -> Vec<f32> {
    srgb.iter()
        .map(|c| {
            let ycc = xyz_to_ycxcz(linear_to_xyz(c.map(|v| srgb_to_linear(v.clamp(0.0, 1.0)))));
            (ycc[0] + 16.0) / 116.0
        })
        .collect()
}

/// LDR-FLIP between display-encoded `reference` and `test` (`[0, 1]` sRGB-encoded RGB,
/// `width × height`, row-major) for a viewer at `ppd` pixels per degree.
///
/// # Panics
///
/// Never on matching sizes; mismatched sizes return an all-ones map.
#[must_use]
pub fn flip(reference: &[[f32; 3]], test: &[[f32; 3]], width: u32, height: u32, ppd: f32) -> FlipResult {
    let (w, h) = (width as usize, height as usize);
    let n = w * h;
    if reference.len() != n || test.len() != n || n == 0 {
        return FlipResult {
            map: vec![1.0; n],
            mean: 1.0,
            p95: 1.0,
            max: 1.0,
        };
    }
    let ppd = ppd.max(1.0);
    // Colour pipeline.
    let csf = csf_kernels(ppd);
    let ref_lab = filtered_hunt_lab(reference, w, h, &csf);
    let test_lab = filtered_hunt_lab(test, w, h, &csf);
    let lab = |c: [f32; 3]| hunt(xyz_to_lab(linear_to_xyz(c)));
    let c_max = hyab(lab([0.0, 1.0, 0.0]), lab([0.0, 0.0, 1.0])).powf(0.7);
    let (pc, pt) = (0.4f32, 0.95f32);
    // Feature pipeline.
    let fk = feature_kernels(ppd);
    let fr = features(&normalized_luminance(reference), w, h, &fk);
    let ft = features(&normalized_luminance(test), w, h, &fk);
    let mut map = Vec::with_capacity(n);
    for i in 0..n {
        let de = hyab(ref_lab[i], test_lab[i]).powf(0.7);
        let ec = if de < pc * c_max {
            pt / (pc * c_max) * de
        } else {
            pt + (de - pc * c_max) / (c_max - pc * c_max) * (1.0 - pt)
        };
        let df = (fr.edge[i] - ft.edge[i])
            .abs()
            .max((fr.point[i] - ft.point[i]).abs());
        let ef = (df / std::f32::consts::SQRT_2).max(0.0).powf(0.5);
        map.push(ec.clamp(0.0, 1.0).powf(1.0 - ef.clamp(0.0, 1.0)));
    }
    let mean = map.iter().map(|v| f64::from(*v)).sum::<f64>() / n as f64;
    let mut sorted = map.clone();
    sorted.sort_by(f32::total_cmp);
    let p95 = sorted[((n as f32 * 0.95) as usize).min(n - 1)];
    let max = sorted[n - 1];
    FlipResult {
        map,
        mean: mean as f32,
        p95,
        max,
    }
}

/// Mean SSIM of the luma of two display-encoded images (11×11 Gaussian window, σ 1.5).
#[must_use]
pub fn ssim(a: &[[f32; 3]], b: &[[f32; 3]], width: u32, height: u32) -> f32 {
    let (w, h) = (width as usize, height as usize);
    if a.len() != w * h || b.len() != w * h || w == 0 || h == 0 {
        return 0.0;
    }
    let radius = 5usize;
    let size = 2 * radius + 1;
    let mut k = vec![0.0f32; size * size];
    for y in 0..size {
        for x in 0..size {
            let (dx, dy) = (x as f32 - 5.0, y as f32 - 5.0);
            k[y * size + x] = (-(dx * dx + dy * dy) / (2.0 * 1.5 * 1.5)).exp();
        }
    }
    let s: f32 = k.iter().sum();
    k.iter_mut().for_each(|v| *v /= s);
    let la: Vec<f32> = a.iter().map(|c| luma(*c)).collect();
    let lb: Vec<f32> = b.iter().map(|c| luma(*c)).collect();
    let mu_a = convolve(&la, w, h, &k, radius);
    let mu_b = convolve(&lb, w, h, &k, radius);
    let aa: Vec<f32> = la.iter().map(|v| v * v).collect();
    let bb: Vec<f32> = lb.iter().map(|v| v * v).collect();
    let ab: Vec<f32> = la.iter().zip(&lb).map(|(x, y)| x * y).collect();
    let s_aa = convolve(&aa, w, h, &k, radius);
    let s_bb = convolve(&bb, w, h, &k, radius);
    let s_ab = convolve(&ab, w, h, &k, radius);
    let (c1, c2) = (0.01f32 * 0.01, 0.03f32 * 0.03);
    let mut total = 0.0f64;
    for i in 0..w * h {
        let (ma, mb) = (mu_a[i], mu_b[i]);
        let va = (s_aa[i] - ma * ma).max(0.0);
        let vb = (s_bb[i] - mb * mb).max(0.0);
        let cov = s_ab[i] - ma * mb;
        let v = ((2.0 * ma * mb + c1) * (2.0 * cov + c2)) / ((ma * ma + mb * mb + c1) * (va + vb + c2));
        total += f64::from(v);
    }
    (total / (w * h) as f64) as f32
}

/// RMSE of linear luminance, and the same relative to the reference's mean luminance.
#[must_use]
pub fn rmse(reference: &[[f32; 3]], test: &[[f32; 3]]) -> (f32, f32) {
    if reference.len() != test.len() || reference.is_empty() {
        return (f32::INFINITY, f32::INFINITY);
    }
    let n = reference.len() as f64;
    let mut se = 0.0f64;
    let mut mean = 0.0f64;
    for (r, t) in reference.iter().zip(test) {
        let d = f64::from(luma(*r) - luma(*t));
        se += d * d;
        mean += f64::from(luma(*r));
    }
    let rmse = (se / n).sqrt();
    let mean = mean / n;
    (
        rmse as f32,
        if mean > 0.0 {
            (rmse / mean) as f32
        } else {
            f32::INFINITY
        },
    )
}

/// Scores linear `test` against linear `reference`: FLIP and SSIM on both frames through the
/// runtime's display transform at `ev100`, RMSE on the linear luminance.
#[must_use]
pub fn compare_linear(
    reference: &[[f32; 3]],
    test: &[[f32; 3]],
    width: u32,
    height: u32,
    ev100: f32,
    ppd: f32,
) -> (CompareReport, FlipResult) {
    let _span = tracing::info_span!("compare_linear").entered();
    let ra = display_encode(reference, ev100);
    let ta = display_encode(test, ev100);
    let f = flip(&ra, &ta, width, height, ppd);
    let s = ssim(&ra, &ta, width, height);
    let (e, rel) = rmse(reference, test);
    let mean =
        |v: &[[f32; 3]]| (v.iter().map(|c| f64::from(luma(*c))).sum::<f64>() / v.len().max(1) as f64) as f32;
    (
        CompareReport {
            width,
            height,
            flip_mean: f.mean,
            flip_p95: f.p95,
            ssim: s,
            rmse: e,
            rel_rmse: rel,
            mean_reference: mean(reference),
            mean_test: mean(test),
        },
        f,
    )
}

/// Decodes RGBA8 sRGB pixels to display-encoded `[0, 1]` RGB (alpha dropped), the input
/// [`flip`] and [`ssim`] take.
#[must_use]
pub fn srgb8_to_unit(rgba: &[u8]) -> Vec<[f32; 3]> {
    rgba.as_chunks::<4>()
        .0
        .iter()
        .map(|p| {
            [
                f32::from(p[0]) / 255.0,
                f32::from(p[1]) / 255.0,
                f32::from(p[2]) / 255.0,
            ]
        })
        .collect()
}

/// Scores two already display-encoded RGBA8 frames (screenshots, look-CI renders of
/// successive builds): FLIP and SSIM on the sRGB values, RMSE on the sRGB-decoded linear
/// luminance. Sizes must match (`width × height × 4` bytes each); a mismatch scores as maximal
/// FLIP and zero SSIM.
#[must_use]
pub fn compare_srgb8(
    reference: &[u8],
    test: &[u8],
    width: u32,
    height: u32,
    ppd: f32,
) -> (CompareReport, FlipResult) {
    let _span = tracing::info_span!("compare_srgb8").entered();
    let ra = srgb8_to_unit(reference);
    let ta = srgb8_to_unit(test);
    let f = flip(&ra, &ta, width, height, ppd);
    let s = ssim(&ra, &ta, width, height);
    let linear = |v: &[[f32; 3]]| -> Vec<[f32; 3]> { v.iter().map(|c| c.map(srgb_to_linear)).collect() };
    let (rl, tl) = (linear(&ra), linear(&ta));
    let (e, rel) = rmse(&rl, &tl);
    let mean =
        |v: &[[f32; 3]]| (v.iter().map(|c| f64::from(luma(*c))).sum::<f64>() / v.len().max(1) as f64) as f32;
    (
        CompareReport {
            width,
            height,
            flip_mean: f.mean,
            flip_p95: f.p95,
            ssim: s,
            rmse: e,
            rel_rmse: rel,
            mean_reference: mean(&rl),
            mean_test: mean(&tl),
        },
        f,
    )
}

/// Colour-maps a FLIP error map (black → magenta → orange → white) as RGBA8.
#[must_use]
pub fn flip_heatmap_rgba8(map: &[f32]) -> Vec<u8> {
    let stops: [[f32; 3]; 5] = [
        [0.0, 0.0, 0.0],
        [0.32, 0.07, 0.48],
        [0.85, 0.20, 0.42],
        [0.99, 0.65, 0.20],
        [1.0, 1.0, 0.95],
    ];
    let mut out = Vec::with_capacity(map.len() * 4);
    for &e in map {
        let t = e.clamp(0.0, 1.0) * 4.0;
        let i = (t.floor() as usize).min(3);
        let f = t - i as f32;
        for (a, b) in stops[i].iter().zip(&stops[i + 1]) {
            out.push(((a + (b - a) * f) * 255.0 + 0.5) as u8);
        }
        out.push(255);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn checker(w: u32, h: u32, a: f32, b: f32) -> Vec<[f32; 3]> {
        (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                if (x / 4 + y / 4) % 2 == 0 { [a; 3] } else { [b; 3] }
            })
            .collect()
    }

    #[test]
    fn identical_images_score_zero_flip_and_unit_ssim() {
        let img = checker(32, 24, 0.2, 0.8);
        let f = flip(&img, &img, 32, 24, DEFAULT_PPD);
        assert!(f.mean < 1e-6, "{}", f.mean);
        assert!((ssim(&img, &img, 32, 24) - 1.0).abs() < 1e-5);
        assert_eq!(rmse(&img, &img).0, 0.0);
    }

    #[test]
    fn black_versus_white_is_maximal() {
        let black = vec![[0.0; 3]; 16 * 16];
        let white = vec![[1.0; 3]; 16 * 16];
        let f = flip(&black, &white, 16, 16, DEFAULT_PPD);
        assert!(f.mean > 0.95, "{}", f.mean);
    }

    #[test]
    fn flip_grows_with_the_difference_and_is_symmetric() {
        let base = vec![[0.5, 0.4, 0.3]; 24 * 24];
        let small: Vec<[f32; 3]> = base.iter().map(|c| [c[0] + 0.02, c[1], c[2]]).collect();
        let large: Vec<[f32; 3]> = base.iter().map(|c| [c[0] + 0.2, c[1], c[2]]).collect();
        let fs = flip(&base, &small, 24, 24, DEFAULT_PPD).mean;
        let fl = flip(&base, &large, 24, 24, DEFAULT_PPD).mean;
        assert!(fs < fl, "{fs} {fl}");
        let back = flip(&small, &base, 24, 24, DEFAULT_PPD).mean;
        assert!((fs - back).abs() < 1e-4);
    }

    #[test]
    fn features_flag_a_missing_edge() {
        // Same mean colour; one image has a thin line the other lacks.
        let w = 32;
        let flat = vec![[0.5; 3]; (w * w) as usize];
        let mut line = flat.clone();
        for y in 0..w {
            line[(y * w + 16) as usize] = [0.9; 3];
        }
        let f = flip(&flat, &line, w, w, DEFAULT_PPD);
        let at_line = f.map[(10 * w + 16) as usize];
        let away = f.map[(10 * w + 2) as usize];
        assert!(at_line > 0.3 && away < 0.05, "{at_line} {away}");
    }

    #[test]
    fn ssim_drops_with_noise_and_rmse_is_relative() {
        let a = checker(32, 32, 0.3, 0.6);
        let b: Vec<[f32; 3]> = a
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let n = if i % 3 == 0 { 0.1 } else { -0.05 };
                [c[0] + n, c[1] + n, c[2] + n]
            })
            .collect();
        assert!(ssim(&a, &b, 32, 32) < 0.95);
        let (abs, rel) = rmse(&a, &b);
        assert!(abs > 0.0 && (rel - abs / 0.45).abs() < 1e-3);
    }

    #[test]
    fn report_json_has_no_infinities() {
        let r = CompareReport {
            width: 1,
            height: 1,
            flip_mean: 0.5,
            flip_p95: 0.5,
            ssim: 1.0,
            rmse: 1.0,
            rel_rmse: f32::INFINITY,
            mean_reference: 0.0,
            mean_test: 1.0,
        };
        let json = r.to_json();
        assert!(json.contains("\"rel_rmse\":null"), "{json}");
        assert!(!json.contains("inf"));
    }

    #[test]
    fn srgb8_pairs_score_like_their_unit_decoding() {
        let w = 24u32;
        let a: Vec<u8> = (0..w * w).flat_map(|i| [(i % 251) as u8, 90, 40, 255]).collect();
        let mut b = a.clone();
        for px in b.as_chunks_mut::<4>().0.iter_mut().step_by(7) {
            px[0] = px[0].saturating_add(60);
        }
        let (same, _) = compare_srgb8(&a, &a, w, w, DEFAULT_PPD);
        assert!(same.flip_mean < 1e-6 && (same.ssim - 1.0).abs() < 1e-5 && same.rmse == 0.0);
        let (diff, map) = compare_srgb8(&a, &b, w, w, DEFAULT_PPD);
        let direct = flip(&srgb8_to_unit(&a), &srgb8_to_unit(&b), w, w, DEFAULT_PPD);
        assert!((diff.flip_mean - direct.mean).abs() < 1e-6 && diff.flip_mean > 0.0);
        assert_eq!(map.map.len(), (w * w) as usize);
        assert!(diff.ssim < 1.0 && diff.mean_test > diff.mean_reference);
        let (bad, _) = compare_srgb8(&a, &b[4..], w, w, DEFAULT_PPD);
        assert!(bad.flip_mean > 0.99 && bad.ssim == 0.0);
    }

    #[test]
    fn heatmap_ends() {
        let px = flip_heatmap_rgba8(&[0.0, 1.0]);
        assert_eq!(&px[0..4], &[0, 0, 0, 255]);
        assert_eq!(&px[4..8], &[255, 255, 242, 255]);
    }
}
