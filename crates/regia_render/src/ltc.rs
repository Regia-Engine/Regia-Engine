// SPDX-License-Identifier: MPL-2.0
//! Linearly transformed cosines for the GGX lobe of area lights (lane emissive-local).
//!
//! Heitz, Dupuy, Hill, Neubelt, "Real-Time Polygonal-Light Shading with Linearly Transformed
//! Cosines", SIGGRAPH 2016: a clamped cosine `D_o(ω) = max(ω_z, 0) / π` warped by a 3×3 matrix
//! `M` approximates the cosine-weighted GGX lobe for one view angle and roughness, and the
//! integral of the warped distribution over a polygon equals the clamped-cosine integral over
//! the polygon transformed by `M⁻¹` — a closed form (Lambert's edge sum). Diffuse is the same
//! integral with `M = I`.
//!
//! The table is Regia Engine's own fit of that paper's method (the `fit` module, in tests): for each
//! perceptual roughness (row) and `sqrt(1 − n·v)` (column), `M = R_y(φ) · [[m11, 0, m13],
//! [0, m22, 0], [0, 0, 1]]` with `φ` the lobe's mean direction, `(m11, m22, m13)` minimising
//! `∫ |p − q|³` between the normalised BRDF lobe and the LTC (Nelder–Mead, multiple importance
//! sampling of both distributions). The magnitude is not in the table: the forward pass takes
//! it from the split-sum DFG it already samples (`F0 · A + B`).
//!
//! Each entry stores `M⁻¹ / M⁻¹[1][1]` as `(a, b, c, d)` for `[[a, 0, b], [0, 1, 0], [c, 0, d]]`
//! (row major, applied as `M⁻¹ · v` in the frame `x` = view tangent, `z` = normal).

use regia_core::glam::{Mat3, Vec3};

/// Table edge: rows (perceptual roughness 0..1) and columns (`sqrt(1 − n·v)` 0..1).
pub const LTC_SIZE: u32 = 64;

/// Smallest GGX alpha of the fitted table; rows below it repeat the 0.045 fit. (The forward pass
/// floors α lower, at `MIN_ALPHA` = 0.002025; an area light seen in a smoother surface keeps the
/// 0.045 lobe, the fit's limit.)
pub const LTC_MIN_ALPHA: f32 = 0.045;

static LTC_BYTES: &[u8] = include_bytes!("../data/ltc_ggx.bin");

/// The fitted table, `LTC_SIZE²` entries, row-major by roughness then `sqrt(1 − n·v)`.
/// Empty when the embedded data is malformed (the caller then shades area lights diffuse-only).
#[must_use]
pub fn ltc_table() -> Vec<[f32; 4]> {
    let n = (LTC_SIZE * LTC_SIZE) as usize;
    if LTC_BYTES.len() != n * 16 {
        return Vec::new();
    }
    LTC_BYTES
        .as_chunks::<16>()
        .0
        .iter()
        .map(|c| {
            let f = |i: usize| f32::from_le_bytes([c[i], c[i + 1], c[i + 2], c[i + 3]]);
            [f(0), f(4), f(8), f(12)]
        })
        .collect()
}

/// The table as `Rgba32Float` texel bytes (row 0 = roughness 0), for the forward pass's
/// `ltc_mat` texture (read with `textureLoad` and blended by hand, so no filtering feature).
/// A malformed embed uploads the identity (a diffuse-shaped lobe).
#[must_use]
pub fn ltc_texture_bytes() -> Vec<u8> {
    let mut table = ltc_table();
    if table.is_empty() {
        table = vec![[1.0, 0.0, 0.0, 1.0]; (LTC_SIZE * LTC_SIZE) as usize];
    }
    let mut out = Vec::with_capacity(table.len() * 16);
    for e in table {
        for v in e {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
    out
}

/// Bilinear lookup of the table at `perceptual` roughness and `n·v` (CPU mirror of the
/// shader's `ltc_inverse`).
#[must_use]
pub fn ltc_inverse(table: &[[f32; 4]], perceptual: f32, no_v: f32) -> Mat3 {
    if table.len() != (LTC_SIZE * LTC_SIZE) as usize {
        return Mat3::IDENTITY;
    }
    let s = (LTC_SIZE - 1) as f32;
    let x = (1.0 - no_v.clamp(0.0, 1.0)).sqrt() * s;
    let y = perceptual.clamp(0.0, 1.0) * s;
    let (x0, y0) = (x.floor().min(s - 1.0), y.floor().min(s - 1.0));
    let (fx, fy) = (x - x0, y - y0);
    let at = |cx: f32, cy: f32| table[(cy as usize) * LTC_SIZE as usize + cx as usize];
    let mut e = [0.0f32; 4];
    for (k, v) in e.iter_mut().enumerate() {
        let top = at(x0, y0)[k] * (1.0 - fx) + at(x0 + 1.0, y0)[k] * fx;
        let bot = at(x0, y0 + 1.0)[k] * (1.0 - fx) + at(x0 + 1.0, y0 + 1.0)[k] * fx;
        *v = top * (1.0 - fy) + bot * fy;
    }
    minv_from_entry(e)
}

/// `[[a, 0, b], [0, 1, 0], [c, 0, d]]` (row major) as a glam column-major matrix.
#[must_use]
pub fn minv_from_entry(e: [f32; 4]) -> Mat3 {
    Mat3::from_cols(Vec3::new(e[0], 0.0, e[2]), Vec3::Y, Vec3::new(e[1], 0.0, e[3]))
}

/// Edge term of Lambert's polygon formula for unit vectors `a → b`: `θ · (a × b) / sin θ`.
#[must_use]
pub fn edge_vector(a: Vec3, b: Vec3) -> Vec3 {
    let x = a.dot(b).clamp(-1.0, 1.0);
    // θ / sin θ, with its series near θ = 0 (acos loses precision there).
    let k = if x > 0.9999 {
        1.0 + (1.0 - x) / 3.0
    } else {
        x.acos() / (1.0 - x * x).max(1e-12).sqrt()
    };
    a.cross(b) * k
}

/// Clamped-cosine integral (`(1/π) ∫ max(cos θ, 0) dω`, the form factor) of the planar convex
/// polygon `points` (relative to the shading point, any winding), clipped to `z ≥ 0`. Signed:
/// positive when the polygon winds clockwise seen from the origin (its `(p1 − p0) × (p2 − p0)`
/// normal points away from the origin's side … see [`quad_form_factor`]). Mirror of
/// `lab_forward.wgsl` `ltc_polygon`.
#[must_use]
pub fn polygon_integral(points: &[Vec3; 4]) -> f32 {
    let mut sum = Vec3::ZERO;
    let mut exit = Vec3::ZERO;
    let mut entry = Vec3::ZERO;
    let (mut have_exit, mut have_entry) = (false, false);
    for i in 0..4 {
        let a = points[i];
        let b = points[(i + 1) % 4];
        let (ua, ub) = (a.z >= 0.0, b.z >= 0.0);
        if ua && ub {
            sum += edge_vector(a.normalize_or_zero(), b.normalize_or_zero());
        } else if ua != ub {
            let t = a.z / (a.z - b.z);
            let c = a + (b - a) * t;
            let c = Vec3::new(c.x, c.y, 0.0);
            if ua {
                sum += edge_vector(a.normalize_or_zero(), c.normalize_or_zero());
                exit = c;
                have_exit = true;
            } else {
                sum += edge_vector(c.normalize_or_zero(), b.normalize_or_zero());
                entry = c;
                have_entry = true;
            }
        }
    }
    if have_exit && have_entry {
        sum += edge_vector(exit.normalize_or_zero(), entry.normalize_or_zero());
    }
    -sum.z / (2.0 * std::f32::consts::PI)
}

/// Form factor of the quad `centre ± ex ± ey` as seen from the origin with normal `+z`, front
/// face = `ex × ey`: positive when the origin is in front, negative behind.
#[must_use]
pub fn quad_form_factor(centre: Vec3, ex: Vec3, ey: Vec3) -> f32 {
    polygon_integral(&[
        centre - ex - ey,
        centre + ex - ey,
        centre + ex + ey,
        centre - ex + ey,
    ])
}

/// GGX `f · cos θ_l` with no Fresnel, in the frame `z` = normal (the engine's `d_ggx` and
/// `v_smith_ggx_correlated`).
#[must_use]
pub fn ggx_cos(v: Vec3, l: Vec3, alpha: f32) -> f32 {
    if v.z <= 0.0 || l.z <= 0.0 {
        return 0.0;
    }
    let h = (v + l).normalize_or_zero();
    let a2 = alpha * alpha;
    let f = (h.z * a2 - h.z) * h.z + 1.0;
    let d = a2 / (std::f32::consts::PI * f * f);
    let gv = l.z * (v.z * v.z * (1.0 - a2) + a2).sqrt();
    let gl = v.z * (l.z * l.z * (1.0 - a2) + a2).sqrt();
    d * 0.5 / (gv + gl) * l.z
}

#[cfg(test)]
pub(crate) mod fit {
    //! Regia Engine's fit of the LTC table (see the module docs). Runs only in tests: the
    //! `regenerate_ltc_table` test (ignored) writes `data/ltc_ggx.bin`.

    use super::*;

    /// One fitted lobe.
    #[derive(Clone, Copy, Debug)]
    pub struct Lobe {
        pub m: Mat3,
        pub minv: Mat3,
        pub det_inv: f32,
    }

    impl Lobe {
        pub fn new(phi: f32, p: [f32; 3]) -> Self {
            let (s, c) = phi.sin_cos();
            let r = Mat3::from_cols(Vec3::new(c, 0.0, -s), Vec3::Y, Vec3::new(s, 0.0, c));
            let k = Mat3::from_cols(
                Vec3::new(p[0].abs().max(1e-5), 0.0, 0.0),
                Vec3::new(0.0, p[1].abs().max(1e-5), 0.0),
                Vec3::new(p[2], 0.0, 1.0),
            );
            let m = r * k;
            let minv = m.inverse();
            Self {
                m,
                minv,
                det_inv: minv.determinant().abs(),
            }
        }

        pub fn eval(&self, l: Vec3) -> f32 {
            let lo = self.minv * l;
            let len = lo.length();
            if len <= 0.0 {
                return 0.0;
            }
            let z = lo.z / len;
            if z <= 0.0 {
                return 0.0;
            }
            z / std::f32::consts::PI * self.det_inv / (len * len * len)
        }

        pub fn sample(&self, u1: f32, u2: f32) -> Vec3 {
            let r = u1.sqrt();
            let phi = 2.0 * std::f32::consts::PI * u2;
            let lo = Vec3::new(r * phi.cos(), r * phi.sin(), (1.0 - u1).max(0.0).sqrt());
            (self.m * lo).normalize_or_zero()
        }
    }

    fn ggx_sample(v: Vec3, alpha: f32, u1: f32, u2: f32) -> (Vec3, f32) {
        let phi = 2.0 * std::f32::consts::PI * u2;
        let t2 = alpha * alpha * u1 / (1.0 - u1).max(1e-7);
        let cos_t = 1.0 / (1.0 + t2).sqrt();
        let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
        let h = Vec3::new(sin_t * phi.cos(), sin_t * phi.sin(), cos_t);
        let l = 2.0 * v.dot(h) * h - v;
        (l, ggx_pdf(v, l, alpha))
    }

    fn ggx_pdf(v: Vec3, l: Vec3, alpha: f32) -> f32 {
        let h = (v + l).normalize_or_zero();
        let a2 = alpha * alpha;
        let f = (h.z * a2 - h.z) * h.z + 1.0;
        let d = a2 / (std::f32::consts::PI * f * f);
        d * h.z.max(0.0) / (4.0 * v.dot(h).abs().max(1e-7))
    }

    /// A fixed, stratified 2D point set (deterministic jitter) so the objective is smooth
    /// enough for Nelder–Mead.
    fn strata(n: u32) -> Vec<(f32, f32)> {
        let mut out = Vec::with_capacity((n * n) as usize);
        for j in 0..n {
            for i in 0..n {
                let h = (i.wrapping_mul(73_856_093) ^ j.wrapping_mul(19_349_663)) as f32;
                let jx = (h * 0.618_034).fract() * 0.5 + 0.25;
                let jy = (h * 0.754_878).fract() * 0.5 + 0.25;
                out.push(((i as f32 + jx) / n as f32, (j as f32 + jy) / n as f32));
            }
        }
        out
    }

    /// Normalisation and mean direction (azimuth in the x–z plane) of the GGX lobe.
    pub fn lobe_stats(v: Vec3, alpha: f32, pts: &[(f32, f32)]) -> (f32, f32) {
        let mut norm = 0.0f64;
        let mut avg = Vec3::ZERO;
        for &(u1, u2) in pts {
            let (l, pdf) = ggx_sample(v, alpha, u1, u2);
            if pdf > 0.0 && l.z > 0.0 {
                let w = ggx_cos(v, l, alpha) / pdf;
                norm += f64::from(w);
                avg += l * w;
            }
        }
        let norm = (norm / pts.len() as f64) as f32;
        (norm, avg.x.atan2(avg.z))
    }

    pub fn error(lobe: &Lobe, v: Vec3, alpha: f32, norm: f32, pts: &[(f32, f32)]) -> f64 {
        let mut e = 0.0f64;
        let target = |l: Vec3| ggx_cos(v, l, alpha) / norm.max(1e-9);
        for &(u1, u2) in pts {
            let l = lobe.sample(u1, u2);
            let (p, q) = (target(l), lobe.eval(l));
            let pdf = q + ggx_pdf(v, l, alpha) * f32::from(u8::from(l.z > 0.0));
            if pdf > 0.0 {
                e += f64::from((p - q).abs().powi(3) / pdf);
            }
            let (l, pdf_b) = ggx_sample(v, alpha, u1, u2);
            if l.z > 0.0 {
                let (p, q) = (target(l), lobe.eval(l));
                let pdf = pdf_b + q;
                if pdf > 0.0 {
                    e += f64::from((p - q).abs().powi(3) / pdf);
                }
            }
        }
        e / pts.len() as f64
    }

    fn nelder_mead(f: impl Fn([f32; 3]) -> f64, start: [f32; 3], step: f32, iters: usize) -> [f32; 3] {
        let mut s: Vec<([f32; 3], f64)> = Vec::with_capacity(4);
        s.push((start, f(start)));
        for k in 0..3 {
            let mut p = start;
            p[k] += step * p[k].abs().max(0.05);
            s.push((p, f(p)));
        }
        let lerp = |a: [f32; 3], b: [f32; 3], t: f32| {
            [
                a[0] + (b[0] - a[0]) * t,
                a[1] + (b[1] - a[1]) * t,
                a[2] + (b[2] - a[2]) * t,
            ]
        };
        for _ in 0..iters {
            s.sort_by(|a, b| a.1.total_cmp(&b.1));
            if (s[3].1 - s[0].1).abs() <= 1e-9 * s[0].1.abs().max(1e-12) {
                break;
            }
            let c = [
                (s[0].0[0] + s[1].0[0] + s[2].0[0]) / 3.0,
                (s[0].0[1] + s[1].0[1] + s[2].0[1]) / 3.0,
                (s[0].0[2] + s[1].0[2] + s[2].0[2]) / 3.0,
            ];
            let worst = s[3];
            let r = lerp(c, worst.0, -1.0);
            let fr = f(r);
            if fr < s[0].1 {
                let e = lerp(c, worst.0, -2.0);
                let fe = f(e);
                s[3] = if fe < fr { (e, fe) } else { (r, fr) };
            } else if fr < s[2].1 {
                s[3] = (r, fr);
            } else {
                let k = lerp(c, worst.0, if fr < worst.1 { -0.5 } else { 0.5 });
                let fk = f(k);
                if fk < worst.1.min(fr) {
                    s[3] = (k, fk);
                } else {
                    let best = s[0].0;
                    for item in s.iter_mut().skip(1) {
                        item.0 = lerp(best, item.0, 0.5);
                        item.1 = f(item.0);
                    }
                }
            }
        }
        s.sort_by(|a, b| a.1.total_cmp(&b.1));
        s[0].0
    }

    /// View vector for table column `x` (`sqrt(1 − n·v)`), kept off exact grazing.
    pub fn view_at(col: u32) -> Vec3 {
        let x = col as f32 / (LTC_SIZE - 1) as f32;
        let cos_t = (1.0 - x * x).max(0.02);
        Vec3::new((1.0 - cos_t * cos_t).sqrt(), 0.0, cos_t)
    }

    /// GGX alpha for table row `y`.
    pub fn alpha_at(row: u32) -> f32 {
        let p = row as f32 / (LTC_SIZE - 1) as f32;
        (p * p).max(LTC_MIN_ALPHA)
    }

    /// Fits one row (one roughness), sweeping the view from normal to grazing and starting
    /// each fit from the previous one. Returns the stored entries.
    pub fn fit_row(row: u32, samples: u32, cols: u32) -> Vec<[f32; 4]> {
        let pts = strata(samples);
        let alpha = alpha_at(row);
        let mut params = [alpha, alpha, 0.0];
        let mut out = Vec::with_capacity(LTC_SIZE as usize);
        for col in 0..cols.min(LTC_SIZE) {
            let v = view_at(col);
            let (norm, phi) = lobe_stats(v, alpha, &pts);
            let phi = if col == 0 { 0.0 } else { phi };
            params = nelder_mead(
                |p| error(&Lobe::new(phi, p), v, alpha, norm, &pts),
                params,
                0.1,
                400,
            );
            out.push(entry(&Lobe::new(phi, params)));
        }
        out
    }

    pub fn entry(lobe: &Lobe) -> [f32; 4] {
        // Row-major entries of M⁻¹ (glam is column major: col(j)[i] = M[i][j]).
        let m = lobe.minv;
        let s = m.y_axis.y;
        [m.x_axis.x / s, m.z_axis.x / s, m.x_axis.z / s, m.z_axis.z / s]
    }

    /// The whole table, rows fitted in parallel.
    pub fn fit_table(samples: u32) -> Vec<[f32; 4]> {
        let threads = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(LTC_SIZE as usize);
        let rows: Vec<Vec<[f32; 4]>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..threads)
                .map(|t| {
                    scope.spawn(move || {
                        (0..LTC_SIZE)
                            .filter(|r| *r as usize % threads == t)
                            .map(|r| (r, fit_row(r, samples, LTC_SIZE)))
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            let mut all: Vec<(u32, Vec<[f32; 4]>)> = handles
                .into_iter()
                .flat_map(|h| h.join().unwrap_or_default())
                .collect();
            all.sort_by_key(|(r, _)| *r);
            all.into_iter().map(|(_, row)| row).collect()
        });
        rows.into_iter().flatten().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Brute-force `∫_quad f · cos dω` by area sampling of the quad (receiver at the origin,
    /// normal +z, view `v`).
    fn brute_force_spec(v: Vec3, alpha: f32, centre: Vec3, ex: Vec3, ey: Vec3, n: u32) -> f32 {
        let normal = ex.cross(ey).normalize();
        let area = 4.0 * ex.length() * ey.length();
        let mut sum = 0.0f64;
        for j in 0..n {
            for i in 0..n {
                let s = (i as f32 + 0.5) / n as f32 * 2.0 - 1.0;
                let t = (j as f32 + 0.5) / n as f32 * 2.0 - 1.0;
                let p = centre + ex * s + ey * t;
                let d2 = p.length_squared();
                let l = p / d2.sqrt();
                let cos_light = normal.dot(-l).abs();
                sum += f64::from(ggx_cos(v, l, alpha) * cos_light / d2);
            }
        }
        (sum * f64::from(area) / f64::from(n * n)) as f32
    }

    fn brute_force_ff(centre: Vec3, ex: Vec3, ey: Vec3, n: u32) -> f32 {
        let normal = ex.cross(ey).normalize();
        let area = 4.0 * ex.length() * ey.length();
        let mut sum = 0.0f64;
        for j in 0..n {
            for i in 0..n {
                let s = (i as f32 + 0.5) / n as f32 * 2.0 - 1.0;
                let t = (j as f32 + 0.5) / n as f32 * 2.0 - 1.0;
                let p = centre + ex * s + ey * t;
                let d2 = p.length_squared();
                let l = p / d2.sqrt();
                sum += f64::from(l.z.max(0.0) * normal.dot(-l).abs() / d2);
            }
        }
        (sum * f64::from(area) / f64::from(n * n) / std::f64::consts::PI) as f32
    }

    #[test]
    fn form_factor_of_a_parallel_square_matches_the_closed_form() {
        // Unit-distance square of half side h above the receiver, facing it.
        for h in [0.1f32, 0.5, 2.0] {
            let ff = quad_form_factor(Vec3::Z, Vec3::X * h, -Vec3::Y * h);
            // Closed form for a square of side 2h centred above at distance 1 (sum of four
            // corner rectangles h × h): F = 4 · (1/2π) [x/√(1+x²) atan(y/√(1+x²)) + (x↔y)].
            let a = h / (1.0 + h * h).sqrt();
            let expect = 4.0 / (2.0 * std::f32::consts::PI) * 2.0 * a * (h / (1.0 + h * h).sqrt()).atan();
            assert!(
                (ff - expect).abs() < 1e-3 * expect.max(1e-3),
                "h {h}: {ff} vs {expect}"
            );
            let bf = brute_force_ff(Vec3::Z, Vec3::X * h, -Vec3::Y * h, 256);
            assert!((ff - bf).abs() < 0.01 * bf, "h {h}: {ff} vs brute force {bf}");
        }
    }

    #[test]
    fn form_factor_clips_at_the_horizon_and_signs_the_back_face() {
        // A vertical quad straddling the horizon plane: half of it is below.
        let c = Vec3::new(0.0, 1.0, 0.0);
        let ex = Vec3::X * 0.5;
        let ey = Vec3::Z * 0.5;
        let ff = quad_form_factor(c, ex, ey);
        let bf = brute_force_ff(c, ex, ey, 256);
        assert!((ff.abs() - bf).abs() < 0.01 * bf, "{ff} vs {bf}");
        // Front face toward the origin is positive, the flipped winding negative.
        let front = quad_form_factor(Vec3::Z, Vec3::X * 0.3, -Vec3::Y * 0.3);
        let back = quad_form_factor(Vec3::Z, Vec3::X * 0.3, Vec3::Y * 0.3);
        assert!(front > 0.0 && back < 0.0 && (front + back).abs() < 1e-5);
        // Entirely below the horizon: nothing.
        assert_eq!(quad_form_factor(-Vec3::Z, Vec3::X * 0.3, -Vec3::Y * 0.3), 0.0);
    }

    #[test]
    fn the_embedded_table_reproduces_brute_force_ggx_over_quads() {
        let table = ltc_table();
        assert_eq!(table.len(), (LTC_SIZE * LTC_SIZE) as usize, "embedded LTC table");
        let cases = [
            // (perceptual, view angle°, centre, ex, ey)
            (
                0.5f32,
                0.0f32,
                Vec3::new(0.0, 0.0, 1.0),
                Vec3::X * 0.5,
                -Vec3::Y * 0.5,
            ),
            (
                0.5,
                45.0,
                Vec3::new(-1.0, 0.0, 1.0),
                Vec3::X * 0.4,
                -Vec3::Y * 0.4,
            ),
            (0.7, 60.0, Vec3::new(-1.5, 0.3, 0.6), Vec3::Z * 0.3, Vec3::Y * 0.6),
            (0.9, 30.0, Vec3::new(0.5, 0.5, 1.0), Vec3::X * 1.0, -Vec3::Y * 1.0),
            (
                0.35,
                50.0,
                Vec3::new(-1.2, 0.0, 1.0),
                Vec3::X * 0.3,
                -Vec3::Y * 0.3,
            ),
        ];
        for (perceptual, deg, c, ex, ey) in cases {
            let t = deg.to_radians();
            let v = Vec3::new(t.sin(), 0.0, t.cos());
            let alpha = (perceptual * perceptual).max(LTC_MIN_ALPHA);
            let minv = ltc_inverse(&table, perceptual, v.z);
            let pts = [c - ex - ey, c + ex - ey, c + ex + ey, c - ex + ey].map(|p| minv * p);
            let shape = polygon_integral(&pts).abs();
            let pts_all = (0..512u32)
                .flat_map(|j| (0..512u32).map(move |i| ((i as f32 + 0.5) / 512.0, (j as f32 + 0.5) / 512.0)))
                .collect::<Vec<_>>();
            let (norm, _) = fit::lobe_stats(v, alpha, &pts_all);
            let ltc = shape * norm;
            let bf = brute_force_spec(v, alpha, c, ex, ey, 512);
            let rel = (ltc - bf).abs() / bf.max(1e-6);
            assert!(
                rel < 0.12,
                "rough {perceptual} view {deg}°: LTC {ltc} vs brute force {bf} ({:.1} %)",
                rel * 100.0
            );
        }
    }

    #[test]
    fn a_refit_of_sample_cells_matches_the_embedded_table() {
        let table = ltc_table();
        assert_eq!(table.len(), (LTC_SIZE * LTC_SIZE) as usize);
        // Row 40 (perceptual 0.63) refitted from normal incidence (the sweep starts there):
        // the stored entries are this fitter's.
        let row = fit::fit_row(40, 24, 17);
        for col in [0u32, 8, 16] {
            let stored = table[(40 * LTC_SIZE + col) as usize];
            let fresh = row[col as usize];
            for k in 0..4 {
                let tol = 0.05 * stored[k].abs().max(0.2);
                assert!(
                    (stored[k] - fresh[k]).abs() < tol,
                    "cell (40, {col}) entry {k}: {} vs {}",
                    stored[k],
                    fresh[k]
                );
            }
        }
    }

    /// Writes `data/ltc_ggx.bin` (`REGIA_LTC_OUT=<path>` overrides). Run with
    /// `cargo test -p regia_render --release --lib ltc::tests::regenerate_ltc_table -- --ignored`.
    #[test]
    #[ignore = "regenerates the embedded LTC table (minutes)"]
    fn regenerate_ltc_table() {
        let table = fit::fit_table(24);
        let mut bytes = Vec::with_capacity(table.len() * 16);
        for e in &table {
            for v in e {
                bytes.extend_from_slice(&v.to_le_bytes());
            }
        }
        let path = regia_core::brand::env_var("REGIA_LTC_OUT")
            .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/data/ltc_ggx.bin").to_string());
        std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("write {path}: {e}"));
    }
}

/// Below this tangent of a sphere's angular radius the forward pass leaves the LTC tables for the representative
/// point: the polygon's edge sum is `f32` noise much below it (`LTC_SPHERE_MIN_TAN` in `lab_forward.wgsl`).
pub const LTC_SPHERE_MIN_TAN: f32 = 0.02;

/// Form factor of a sphere of `radius` at `to_center` (tangent frame, z = the normal) through `m`, the identity for
/// the clamped cosine or [`ltc_inverse`] for the GGX lobe: a regular 12-gon with the area of the sphere's silhouette
/// disk, clipped to `z ≥ 0` after `m` (ADR-2592). Mirrors `ltc_sphere` in `forward_ltc.wgsl`. Multiplied by the
/// sphere's radiance and the lobe's magnitude, this is the light it reflects (`π` is already inside).
#[must_use]
pub fn sphere_form_factor(m: Mat3, to_center: Vec3, radius: f32) -> f32 {
    let d2 = to_center.length_squared();
    let axis = to_center * d2.max(1e-12).sqrt().recip();
    let tan_r = radius * (d2 - radius * radius).max(1e-4 * d2).sqrt().recip() * 1.023_326_7;
    let up = if axis.z.abs() > 0.9 { Vec3::X } else { Vec3::Z };
    let e1 = axis.cross(up).normalize_or_zero() * tan_r;
    let e2 = axis.cross(e1);
    let first = m * (axis + e1);
    let (mut a, mut c, mut s) = (first, 1.0f32, 0.0f32);
    let mut sum = Vec3::ZERO;
    let (mut exit_p, mut entry_p) = (None, None);
    let unit = |p: Vec3| p * p.length_squared().max(1e-20).sqrt().recip();
    for i in 0..12u32 {
        let c2 = c * 0.866_025_4 - s * 0.5;
        s = s * 0.866_025_4 + c * 0.5;
        c = c2;
        let b = if i < 11 {
            m * (axis + e1 * c + e2 * s)
        } else {
            first
        };
        let (ua, ub) = (a.z >= 0.0, b.z >= 0.0);
        if ua && ub {
            sum += edge_vector(unit(a), unit(b));
        } else if ua != ub {
            let t = a.z / (a.z - b.z);
            let k = (a + (b - a) * t).truncate().extend(0.0);
            if ua {
                sum += edge_vector(unit(a), unit(k));
                exit_p = Some(k);
            } else {
                sum += edge_vector(unit(k), unit(b));
                entry_p = Some(k);
            }
        }
        a = b;
    }
    if let (Some(x), Some(e)) = (exit_p, entry_p) {
        sum += edge_vector(unit(x), unit(e));
    }
    sum.z.abs() / (2.0 * std::f32::consts::PI)
}

/// ADR-2592: the LTC-integrated sphere light ([`sphere_form_factor`]) against closed forms, a brute-force GGX integral
/// over the sphere and the representative-point form (`light_source`, ADR-1367).
#[cfg(test)]
mod sphere_study {
    use super::*;
    use regia_core::glam::Quat;

    /// `∫ ggx_cos dω` over the cone toward a sphere at `c` (unit-radiance light), stratified cone sampling.
    fn reference(v: Vec3, c: Vec3, r: f32, alpha: f32) -> f64 {
        let d = c.length();
        let axis = c / d;
        let cos_max = (1.0 - (r * r) / (d * d)).max(0.0).sqrt();
        let (u, w) = {
            let t = axis.any_orthonormal_vector();
            (t, axis.cross(t))
        };
        let n = 384usize;
        let omega = 2.0 * std::f64::consts::PI * f64::from(1.0 - cos_max);
        let mut sum = 0.0f64;
        for i in 0..n {
            for j in 0..n {
                let cos_t = 1.0 - (1.0 - cos_max) * ((i as f32 + 0.5) / n as f32);
                let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
                let phi = std::f32::consts::TAU * (j as f32 + 0.5) / n as f32;
                let l = axis * cos_t + (u * phi.cos() + w * phi.sin()) * sin_t;
                sum += f64::from(ggx_cos(v, l, alpha));
            }
        }
        sum * omega / (n * n) as f64
    }

    /// Directional albedo of `ggx_cos` (no Fresnel): the magnitude the forward pass takes from its DFG.
    fn albedo(v: Vec3, alpha: f32) -> f64 {
        let n = 512usize;
        let mut sum = 0.0f64;
        for i in 0..n {
            for j in 0..n {
                let cos_t = (i as f32 + 0.5) / n as f32;
                let sin_t = (1.0 - cos_t * cos_t).sqrt();
                let phi = std::f32::consts::TAU * (j as f32 + 0.5) / n as f32;
                let l = Vec3::new(sin_t * phi.cos(), sin_t * phi.sin(), cos_t);
                sum += f64::from(ggx_cos(v, l, alpha));
            }
        }
        sum * 2.0 * std::f64::consts::PI / (n * n) as f64
    }

    fn ltc_sphere(table: &[[f32; 4]], v: Vec3, c: Vec3, r: f32, perceptual: f32, alb: f64) -> f64 {
        f64::from(sphere_form_factor(ltc_inverse(table, perceptual, v.z), c, r)) * alb
    }

    fn representative(v: Vec3, c: Vec3, r: f32, alpha: f32) -> f64 {
        let n = Vec3::Z;
        let refl = (n * 2.0 * n.dot(v) - v).normalize();
        let l_spec = crate::light_source::sphere_light_dir(c, refl, r, alpha);
        let ldir = c.normalize();
        let nol = ldir.z.max(1e-4);
        let nol_spec = l_spec.z.max(0.0);
        let h = (l_spec + v).normalize();
        let hc = (ldir + v).normalize();
        let gain = crate::light_source::sphere_light_gain(
            alpha,
            r,
            c,
            refl,
            h.z.max(0.0),
            hc.z.max(0.0),
            nol_spec / nol,
        );
        let brdf = if nol_spec > 0.0 {
            f64::from(ggx_cos(v, l_spec, alpha)) / f64::from(nol_spec)
        } else {
            0.0
        };
        let d2 = c.length_squared();
        brdf * f64::from(gain) * f64::from(nol) * f64::from(std::f32::consts::PI * r * r / d2)
    }

    #[test]
    fn ltc_sphere_against_the_representative_point_and_a_brute_force_integral() {
        let table = ltc_table();
        assert_eq!(table.len(), (LTC_SIZE * LTC_SIZE) as usize);
        let mut rows = Vec::new();
        let (mut sum_ltc, mut sum_rep, mut count) = (0.0f64, 0.0f64, 0usize);
        for perceptual in [0.25f32, 0.4, 0.6, 0.8, 1.0] {
            let alpha = (perceptual * perceptual).max(1e-3);
            let (mut e_ltc, mut e_rep, mut n) = (0.0f64, 0.0f64, 0usize);
            for theta_v in [0.2f32, 0.7, 1.1] {
                let v = Vec3::new(theta_v.sin(), 0.0, theta_v.cos());
                let alb = albedo(v, alpha);
                for (r, d) in [(0.1f32, 2.0f32), (0.2, 2.0), (0.2, 1.0)] {
                    // Light near the mirror direction and 25° off it, well above the horizon.
                    for off in [0.0f32, 0.44] {
                        let refl = Vec3::new(-v.x, 0.0, v.z);
                        let dir = (Quat::from_rotation_y(off) * refl).normalize();
                        if dir.z < 0.35 {
                            continue;
                        }
                        let c = dir * d;
                        let want = reference(v, c, r, alpha);
                        if want <= 1e-9 {
                            continue;
                        }
                        let ltc = ltc_sphere(&table, v, c, r, perceptual, alb);
                        let rep = representative(v, c, r, alpha);
                        e_ltc += (ltc / want - 1.0).abs();
                        e_rep += (rep / want - 1.0).abs();
                        n += 1;
                    }
                }
            }
            sum_ltc += e_ltc;
            sum_rep += e_rep;
            count += n;
            rows.push(format!(
                "roughness {perceptual}: {n} cases, LTC sphere {:.3}, representative point {:.3}",
                e_ltc / n.max(1) as f64,
                e_rep / n.max(1) as f64
            ));
        }
        let (ltc, rep) = (sum_ltc / count as f64, sum_rep / count as f64);
        for row in &rows {
            eprintln!("{row}");
        }
        eprintln!(
            "mean relative error of the specular integral: LTC sphere {ltc:.4}, representative point {rep:.4}"
        );
        assert!(ltc.is_finite() && rep.is_finite() && count > 50);
        assert!(ltc < 0.05, "the LTC sphere tracks the integral: {ltc}");
        assert!(
            ltc < rep,
            "and closer on the whole than the representative point: {ltc} / {rep}"
        );
    }

    /// The geometry of `regia_pt/tests/lights_ies_area_frame.rs`'s glossy-metal row on the CPU: a sphere of radius
    /// 0.5 centred 0.4 above a floor (so it cuts the floor), seen from (0, 5, 5), perceptual roughness 0.3. Prints the
    /// LTC form and the representative point against the brute-force integral over the floor, binned by the
    /// reference's size, so the frame test's numbers can be read as the method's or the shader's. Asserts the
    /// highlight and the floor's energy; the far tail is the LTC fit's known under-read (ADR-2592 Known issues).
    #[test]
    fn the_glossy_frame_geometry_on_the_cpu() {
        /// (reference, LTC, representative point, n·v, distance to the centre).
        type Row = (f64, f64, f64, f32, f32);
        let table = ltc_table();
        let eye = Vec3::new(0.0, 5.0, 5.0);
        let centre = Vec3::new(0.0, 0.4, 0.0);
        let r = 0.5f32;
        let perceptual = 0.3f32;
        let alpha = (perceptual * perceptual).max(0.002_025);
        let mut rows = Vec::new();
        for iz in -30..=30 {
            for ix in -40..=40 {
                let p = Vec3::new(ix as f32 * 0.1, 0.0, iz as f32 * 0.1);
                let ray = (p - eye).normalize();
                let along = (centre - eye).dot(ray);
                if (eye + ray * along - centre).length() < r + 0.1 {
                    continue;
                }
                // Tangent frame: x toward the view in the floor, z = up.
                let v_world = (eye - p).normalize();
                let t1 = Vec3::new(v_world.x, 0.0, v_world.z).normalize_or_zero();
                let t1 = if t1 == Vec3::ZERO { Vec3::X } else { t1 };
                let n = Vec3::Y;
                let t2 = n.cross(t1);
                let local = |w: Vec3| Vec3::new(w.dot(t1), w.dot(t2), w.dot(n));
                let v = local(v_world);
                let c = local(centre - p);
                if c.length() <= r {
                    continue;
                }
                let want = {
                    // A coarser `reference`: 96² samples per point.
                    let d = c.length();
                    let axis = c / d;
                    let cos_max = (1.0 - (r * r) / (d * d)).max(0.0).sqrt();
                    let u = axis.any_orthonormal_vector();
                    let w = axis.cross(u);
                    let k = 96usize;
                    let omega = 2.0 * std::f64::consts::PI * f64::from(1.0 - cos_max);
                    let mut sum = 0.0f64;
                    for i in 0..k {
                        for j in 0..k {
                            let cos_t = 1.0 - (1.0 - cos_max) * ((i as f32 + 0.5) / k as f32);
                            let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
                            let phi = std::f32::consts::TAU * (j as f32 + 0.5) / k as f32;
                            let l = axis * cos_t + (u * phi.cos() + w * phi.sin()) * sin_t;
                            sum += f64::from(ggx_cos(v, l, alpha));
                        }
                    }
                    sum * omega / (k * k) as f64
                };
                let alb = albedo(v, alpha);
                let ltc = ltc_sphere(&table, v, c, r, perceptual, alb);
                let rep = representative(v, c, r, alpha);
                rows.push((want, ltc, rep, v.z, c.length()));
            }
        }
        let mean = rows.iter().map(|r| r.0).sum::<f64>() / rows.len().max(1) as f64;
        let peak = rows.iter().map(|r| r.0).fold(0.0f64, f64::max);
        let stats = |label: &str, keep: &dyn Fn(&Row) -> bool| {
            let kept: Vec<_> = rows.iter().filter(|r| r.0 >= 0.01 * mean && keep(r)).collect();
            let n = kept.len().max(1) as f64;
            let mre =
                |f: &dyn Fn(&Row) -> f64| kept.iter().map(|r| (f(r) / r.0 - 1.0).abs()).sum::<f64>() / n;
            let bias = |f: &dyn Fn(&Row) -> f64| kept.iter().map(|r| f(r) / r.0 - 1.0).sum::<f64>() / n;
            let want: f64 = kept.iter().map(|r| r.0).sum();
            let energy = |f: &dyn Fn(&Row) -> f64| {
                kept.iter().map(|r| (f(r) - r.0).abs()).sum::<f64>() / want.max(1e-30)
            };
            eprintln!(
                "{label}: {} pts, LTC MRE {:.3} bias {:+.3} energy {:.3}; representative MRE {:.3} bias {:+.3} energy {:.3}",
                kept.len(),
                mre(&|r| r.1),
                bias(&|r| r.1),
                energy(&|r| r.1),
                mre(&|r| r.2),
                bias(&|r| r.2),
                energy(&|r| r.2),
            );
            (mre(&|r| r.1), energy(&|r| r.1), energy(&|r| r.2))
        };
        let all = stats("glossy floor, all", &|_| true);
        let highlight = stats("highlight (reference ≥ 10 % of peak)", &|r| r.0 >= 0.1 * peak);
        stats("tail (reference < 10 % of peak)", &|r| r.0 < 0.1 * peak);
        stats("near (centre within 1.5)", &|r| r.4 < 1.5);
        stats("far (centre beyond 1.5)", &|r| r.4 >= 1.5);
        assert!(rows.len() > 1000 && all.0.is_finite());
        // Measured 11-05: the highlight within 1.4 %, the floor's energy within 3.1 % (the representative point
        // 47 %). The LTC fit under-reads the far glossy tail (the per-point mean error is 66 %, bias -62 %, on
        // values under a tenth of the peak): the frame test's glossy row measures the same thing.
        assert!(highlight.0 < 0.03, "LTC in the highlight: {}", highlight.0);
        assert!(all.1 < 0.05, "LTC energy over the floor: {}", all.1);
        assert!(
            all.1 < all.2,
            "LTC {} against the representative point {}",
            all.1,
            all.2
        );
    }

    /// `(1/π) ∫ max(cos θ, 0) dω` over the cone toward the sphere: the Lambert form factor by brute force.
    fn cosine_reference(c: Vec3, r: f32) -> f64 {
        let d = c.length();
        let axis = c / d;
        let cos_max = (1.0 - (r * r) / (d * d)).max(0.0).sqrt();
        let u = axis.any_orthonormal_vector();
        let w = axis.cross(u);
        let n = 512usize;
        let omega = 2.0 * std::f64::consts::PI * f64::from(1.0 - cos_max);
        let mut sum = 0.0f64;
        for i in 0..n {
            for j in 0..n {
                let cos_t = 1.0 - (1.0 - cos_max) * ((i as f32 + 0.5) / n as f32);
                let sin_t = (1.0 - cos_t * cos_t).max(0.0).sqrt();
                let phi = std::f32::consts::TAU * (j as f32 + 0.5) / n as f32;
                let l = axis * cos_t + (u * phi.cos() + w * phi.sin()) * sin_t;
                sum += f64::from(l.z.max(0.0));
            }
        }
        sum * omega / (n * n) as f64 / std::f64::consts::PI
    }

    #[test]
    fn the_cosine_form_factor_is_the_closed_form_above_the_horizon() {
        // Fully above the horizon a sphere's form factor is exactly sin²σ cos θ (Nusselt's analogy).
        // The last row sits at `LTC_SPHERE_MIN_TAN`: the edge sum must still be sound where the shader stops using it.
        for (theta, d, r) in [
            (0.0f32, 3.0f32, 0.2f32),
            (0.6, 2.0, 0.3),
            (1.1, 4.0, 0.2),
            (0.3, 1.0, 0.5),
            (0.4, 1.0, LTC_SPHERE_MIN_TAN),
        ] {
            let c = Vec3::new(theta.sin(), 0.0, theta.cos()) * d;
            let want = (r * r) / (d * d) * theta.cos();
            let got = sphere_form_factor(Mat3::IDENTITY, c, r);
            assert!(
                (got / want - 1.0).abs() < 0.002,
                "θ {theta} d {d} r {r}: {got} vs {want}"
            );
        }
    }

    #[test]
    fn the_cosine_form_factor_clips_a_sphere_at_the_horizon() {
        // Centres at and just below the horizon: the point-light cosine says 0, the sphere still lights the point.
        for (z, d, r) in [
            (0.0f32, 1.0f32, 0.3f32),
            (-0.1, 1.0, 0.3),
            (0.1, 1.0, 0.3),
            (0.05, 2.0, 0.5),
        ] {
            let c = Vec3::new((d * d - z * z).sqrt(), 0.0, z);
            let want = cosine_reference(c, r);
            let got = f64::from(sphere_form_factor(Mat3::IDENTITY, c, r));
            assert!(want > 1e-3, "{want}");
            assert!(
                (got / want - 1.0).abs() < 0.01,
                "z {z} d {d} r {r}: {got} vs {want}"
            );
        }
        // Wholly below the horizon: nothing.
        assert_eq!(
            sphere_form_factor(Mat3::IDENTITY, Vec3::new(0.2, 0.0, -1.0), 0.3),
            0.0
        );
    }

    #[test]
    fn a_point_inside_the_sphere_sees_about_the_whole_hemisphere() {
        let got = sphere_form_factor(Mat3::IDENTITY, Vec3::new(0.0, 0.0, 0.1), 0.5);
        assert!((got - 1.0).abs() < 0.05, "{got}");
        assert!(sphere_form_factor(Mat3::IDENTITY, Vec3::new(0.0, 0.0, 0.5), 0.5).is_finite());
    }

    #[test]
    fn the_silhouette_frame_is_stable_near_the_pole() {
        // The tangent picks a different helper axis at |axis.z| = 0.9; the value must not jump there.
        let at = |z: f32| {
            let x = (1.0 - z * z).sqrt();
            sphere_form_factor(Mat3::IDENTITY, Vec3::new(x, 0.0, z) * 2.0, 0.3)
        };
        assert!((at(0.8999) / at(0.9001) - 1.0).abs() < 0.002);
    }
}
