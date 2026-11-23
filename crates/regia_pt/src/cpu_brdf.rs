// SPDX-License-Identifier: MPL-2.0
//! CPU evaluation of the direct BRDF for the validation tests (directional albedo, a plane's
//! radiance under a sun). The tracer itself always runs the WGSL.
//!
//! What this evaluates: isotropic GGX, dielectric F0 0.04 mixed toward the base color by
//! `metallic`, diffuse `base * (1 − metallic) / π`, plus optional thin film, clearcoat and
//! multi-scatter compensation. It does not read [`PtMaterial::ior`], `transmission`,
//! `anisotropy`, `anisotropy_rotation` or `unlit`.

use regia_render::BrdfEnergy;

use crate::scene::PtMaterial;

const PI: f32 = std::f32::consts::PI;
/// The smallest GGX α (perceptual roughness 0.045 squared), as `pbr_lighting.wgsl` `MIN_ALPHA`.
const MIN_ALPHA: f32 = 0.002_025;

/// The forward pass's DFG table, decoded (32×32: scale, bias, single-scatter albedo).
#[derive(Clone, Debug)]
pub struct DfgTable {
    size: u32,
    texels: Vec<[f32; 4]>,
}

impl Default for DfgTable {
    fn default() -> Self {
        let lut = regia_render::build_brdf_lut();
        Self {
            size: lut.width,
            texels: lut
                .rgba
                .as_chunks::<4>()
                .0
                .iter()
                .map(|p| p.map(|c| f32::from(c) / 255.0))
                .collect(),
        }
    }
}

impl DfgTable {
    /// Bilinear clamp-to-edge sample (the hardware sampler's footprint): scale, bias and the
    /// single-scatter albedo `E(μ)` (B channel) and its hemispherical average `E_avg` (A).
    #[must_use]
    pub fn sample(&self, no_v: f32, perceptual: f32) -> [f32; 4] {
        let n = self.size;
        let fx = (no_v * n as f32 - 0.5).clamp(0.0, (n - 1) as f32);
        let fy = (perceptual * n as f32 - 0.5).clamp(0.0, (n - 1) as f32);
        let (x0, y0) = (fx.floor() as u32, fy.floor() as u32);
        let (x1, y1) = ((x0 + 1).min(n - 1), (y0 + 1).min(n - 1));
        let (tx, ty) = (fx - x0 as f32, fy - y0 as f32);
        let at = |x: u32, y: u32| self.texels[(y * n + x) as usize];
        std::array::from_fn(|k| {
            let a = at(x0, y0)[k] + (at(x1, y0)[k] - at(x0, y0)[k]) * tx;
            let b = at(x0, y1)[k] + (at(x1, y1)[k] - at(x0, y1)[k]) * tx;
            a + (b - a) * ty
        })
    }
}

fn d_ggx(no_h: f32, a: f32) -> f32 {
    let a2 = a * a;
    let f = (no_h * a2 - no_h) * no_h + 1.0;
    a2 / (PI * f * f)
}

fn v_smith(no_v: f32, no_l: f32, a: f32) -> f32 {
    let a2 = a * a;
    let gv = no_l * (no_v * no_v * (1.0 - a2) + a2).sqrt();
    let gl = no_v * (no_l * no_l * (1.0 - a2) + a2).sqrt();
    0.5 / (gv + gl)
}

fn schlick(vo_h: f32, f0: f32) -> f32 {
    f0 + (1.0 - f0) * (1.0 - vo_h).powi(5)
}

/// BRDF × cos for one channel (`k`) of `m` at n·v, with the light at polar angle `cos_l` and
/// azimuth `phi` relative to the view's azimuth ([`BrdfEnergy::Legacy`], the shipped BRDF).
/// Isotropic, F0 0.04 for a dielectric, and it ignores `ior`, `transmission`, `anisotropy`
/// and `unlit` (see the module note).
#[must_use]
pub fn brdf_cos(m: &PtMaterial, dfg: &DfgTable, no_v: f32, cos_l: f32, phi: f32, k: usize) -> f32 {
    brdf_cos_energy(m, dfg, BrdfEnergy::Legacy, no_v, cos_l, phi, k)
}

/// [`brdf_cos`] under the energy model `energy` (ADR-2632): [`BrdfEnergy::Compensated`] mirrors
/// `pbr_brdf.wgsl::with_compensation` (`regia_render::brdf_energy::compensated_terms`).
#[must_use]
pub fn brdf_cos_energy(
    m: &PtMaterial,
    dfg: &DfgTable,
    energy: BrdfEnergy,
    no_v: f32,
    cos_l: f32,
    phi: f32,
    k: usize,
) -> f32 {
    if cos_l <= 0.0 {
        return 0.0;
    }
    let base = m.base_color[k];
    if m.lambert {
        return base / PI * cos_l;
    }
    let metallic = m.metallic.clamp(0.0, 1.0);
    let perceptual = m.roughness.clamp(0.0, 1.0);
    let rough = (perceptual * perceptual).max(MIN_ALPHA);
    let diffuse = base * (1.0 - metallic);
    let f0_base = 0.04 + (base - 0.04) * metallic;
    let no_v = no_v.max(1e-5);
    // The film's Fresnel over the base, and the F0 proxy of the terms shaped like Schlick's.
    let film = m.thin_film.map(|film| {
        let f0_rgb: [f32; 3] = std::array::from_fn(|c| 0.04 + (m.base_color[c] - 0.04) * metallic);
        let nm = film.thickness_max.max(0.0);
        (f0_rgb, film.factor, film.ior, nm)
    });
    let f0 = film.map_or(f0_base, |(rgb, factor, ior, nm)| {
        let cover = regia_render::thin_film::weight(factor, nm);
        regia_render::thin_film::schlick_f0(no_v, rgb, cover, ior, nm)[k]
    });
    let sin_v = (1.0 - no_v * no_v).max(0.0).sqrt();
    let sin_l = (1.0 - cos_l * cos_l).max(0.0).sqrt();
    let v = [sin_v, 0.0, no_v];
    let l = [sin_l * phi.cos(), sin_l * phi.sin(), cos_l];
    let h = [v[0] + l[0], v[1] + l[1], v[2] + l[2]];
    let hl = (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt();
    let h = [h[0] / hl, h[1] / hl, h[2] / hl];
    let no_h = h[2].max(0.0);
    let lo_h = (l[0] * h[0] + l[1] * h[1] + l[2] * h[2]).max(0.0);
    let fr = film.map_or(schlick(lo_h, f0_base), |(rgb, factor, ior, nm)| {
        let cover = regia_render::thin_film::weight(factor, nm);
        regia_render::thin_film::fresnel(lo_h, rgb, cover, ior, nm)[k]
    });
    let mut spec = d_ggx(no_h, rough) * v_smith(no_v, cos_l, rough) * fr;
    let mut diff = diffuse / PI * (1.0 - fr);
    if energy == BrdfEnergy::Compensated {
        // Kulla & Conty's reciprocal multiple scattering and the diffuse coupled to the whole
        // specular layer at both cosines (`regia_render::brdf_energy`).
        let so = dfg.sample(no_v, perceptual);
        let si = dfg.sample(cos_l, perceptual);
        let (ms, coupled) =
            regia_render::brdf_energy::compensated_terms(f0, perceptual, so, si, m.multi_scatter);
        spec += ms;
        diff = diffuse * coupled;
    } else if m.multi_scatter {
        // Mirrors `pbr_brdf.wgsl::with_multi_scatter`: Turquin's energy compensation of the
        // single-scatter lobe and the diffuse under the specular layer's albedo at n·v.
        let s = dfg.sample(no_v, perceptual);
        let e_o = s[2].clamp(0.05, 1.0);
        let e_avg = s[3].clamp(0.05, 1.0);
        let favg = f0 + (1.0 - f0) / 21.0;
        let fms = favg * e_avg / (1.0 - favg * (1.0 - e_avg));
        spec *= 1.0 + fms * (1.0 - e_o) / e_o;
        let es_o = (f0 * s[0] + s[1]).clamp(0.0, 1.0);
        diff = diffuse / PI * (1.0 - es_o);
    }
    if m.clearcoat > 1e-4 {
        let coat_att = 1.0 - m.clearcoat * schlick(no_v, 0.04);
        spec *= coat_att;
        diff *= coat_att;
        let ca = (m.clearcoat_roughness * m.clearcoat_roughness).max(MIN_ALPHA);
        spec += m.clearcoat * d_ggx(no_h, ca) * v_smith(no_v, cos_l, ca) * schlick(lo_h, 0.04);
    }
    (diff + spec) * cos_l
}

/// [`brdf_cos`] for world vectors: unit normal `n`, unit view `v` (toward the eye) and unit
/// light direction `l`.
#[must_use]
pub fn brdf_cos_vec(m: &PtMaterial, dfg: &DfgTable, n: [f32; 3], v: [f32; 3], l: [f32; 3], k: usize) -> f32 {
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let no_v = dot(n, v);
    let cos_l = dot(n, l);
    let proj = |x: [f32; 3]| {
        let d = dot(n, x);
        let p = [x[0] - n[0] * d, x[1] - n[1] * d, x[2] - n[2] * d];
        let len = dot(p, p).sqrt();
        if len > 1e-9 {
            [p[0] / len, p[1] / len, p[2] / len]
        } else {
            [0.0; 3]
        }
    };
    let (pv, pl) = (proj(v), proj(l));
    let c = dot(pv, pl).clamp(-1.0, 1.0);
    let phi = if dot(pv, pv) == 0.0 || dot(pl, pl) == 0.0 {
        0.0
    } else {
        c.acos()
    };
    brdf_cos(m, dfg, no_v, cos_l, phi, k)
}

/// Directional albedo ∫ f·cos dω of one channel at n·v: midpoint quadrature over the light
/// hemisphere in (cos θ, φ) (`n_theta` × `n_phi` cells) and over GGX-distributed half vectors (the
/// same counts), combined with balance-heuristic weights (Veach 1997 § 9.2, deterministic grids).
/// The half-vector grid resolves a polished lobe (α down to `MIN_ALPHA`) that the hemisphere grid
/// alone misses; the hemisphere grid keeps the diffuse part.
#[must_use]
pub fn directional_albedo(
    m: &PtMaterial,
    dfg: &DfgTable,
    no_v: f32,
    k: usize,
    n_theta: u32,
    n_phi: u32,
) -> f64 {
    directional_albedo_energy(m, dfg, BrdfEnergy::Legacy, no_v, k, n_theta, n_phi)
}

/// [`directional_albedo`] under the energy model `energy` ([`brdf_cos_energy`]).
#[must_use]
pub fn directional_albedo_energy(
    m: &PtMaterial,
    dfg: &DfgTable,
    energy: BrdfEnergy,
    no_v: f32,
    k: usize,
    n_theta: u32,
    n_phi: u32,
) -> f64 {
    let n = f64::from(n_theta) * f64::from(n_phi);
    let perceptual = m.roughness.clamp(0.0, 1.0);
    let a = f64::from((perceptual * perceptual).max(MIN_ALPHA));
    let no_v = f64::from(no_v.clamp(1e-5, 1.0));
    let v = [(1.0 - no_v * no_v).max(0.0).sqrt(), 0.0, no_v];
    // Density over the light direction of the half-vector grid: D(h) (n·h) / (4 v·h).
    let pdf_h = |h: [f64; 3]| {
        let vo_h = (v[0] * h[0] + v[1] * h[1] + v[2] * h[2]).max(1e-9);
        let a2 = a * a;
        let f = (h[2] * a2 - h[2]) * h[2] + 1.0;
        let d = a2 / (std::f64::consts::PI * f * f);
        d * h[2] / (4.0 * vo_h)
    };
    let uniform = n / std::f64::consts::TAU;
    let eval = |l: [f64; 3]| -> f64 {
        if l[2] <= 0.0 {
            return 0.0;
        }
        let h = [v[0] + l[0], v[1] + l[1], v[2] + l[2]];
        let len = (h[0] * h[0] + h[1] * h[1] + h[2] * h[2]).sqrt().max(1e-12);
        let h = [h[0] / len, h[1] / len, h[2] / len];
        let phi = l[1].atan2(l[0]);
        // Contribution / (sum of the two grids' densities): the balance heuristic.
        f64::from(brdf_cos_energy(
            m,
            dfg,
            energy,
            no_v as f32,
            l[2] as f32,
            phi as f32,
            k,
        )) / (uniform + n * pdf_h(h))
    };
    let mut sum = 0.0f64;
    for i in 0..n_theta {
        let cos_l = 1.0 - (f64::from(i) + 0.5) / f64::from(n_theta);
        let sin_l = (1.0 - cos_l * cos_l).max(0.0).sqrt();
        for j in 0..n_phi {
            let phi = (f64::from(j) + 0.5) / f64::from(n_phi) * std::f64::consts::TAU;
            sum += eval([sin_l * phi.cos(), sin_l * phi.sin(), cos_l]);
        }
    }
    for i in 0..n_theta {
        // GGX inverse CDF of the half vector's polar angle (Walter et al. 2007, eq. 35).
        let xi = (f64::from(i) + 0.5) / f64::from(n_theta);
        let cos_h = ((1.0 - xi) / (1.0 + (a * a - 1.0) * xi)).max(0.0).sqrt();
        let sin_h = (1.0 - cos_h * cos_h).max(0.0).sqrt();
        for j in 0..n_phi {
            let phi = (f64::from(j) + 0.5) / f64::from(n_phi) * std::f64::consts::TAU;
            let h = [sin_h * phi.cos(), sin_h * phi.sin(), cos_h];
            let vo_h = v[0] * h[0] + v[1] * h[1] + v[2] * h[2];
            if vo_h <= 0.0 {
                continue;
            }
            let l = [
                2.0 * vo_h * h[0] - v[0],
                2.0 * vo_h * h[1] - v[1],
                2.0 * vo_h * h[2] - v[2],
            ];
            sum += eval(l);
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_film_changes_the_specular_by_channel_and_no_film_stays_the_old_brdf() {
        let dfg = DfgTable::default();
        let plain = PtMaterial {
            base_color: [0.02, 0.02, 0.02, 1.0],
            roughness: 0.3,
            ..PtMaterial::default()
        };
        let film = PtMaterial {
            thin_film: Some(crate::scene::PtThinFilm {
                factor: 1.0,
                ior: 1.33,
                thickness_min: 100.0,
                thickness_max: 340.0,
                map: None,
            }),
            ..plain.clone()
        };
        // The mirror direction: the specular peak, where the Fresnel shows.
        let (no_v, cos_l) = (0.8f32, 0.8f32);
        let rgb = |m: &PtMaterial| [0, 1, 2].map(|k| brdf_cos(m, &dfg, no_v, cos_l, std::f32::consts::PI, k));
        let (a, b) = (rgb(&plain), rgb(&film));
        assert!((a[0] - a[2]).abs() < 1e-6, "{a:?}");
        assert!((b[0] - b[2]).abs() > 1e-3, "the film is not neutral: {b:?}");
        let mut off = film.clone();
        if let Some(f) = off.thin_film.as_mut() {
            f.factor = 0.0;
        }
        assert_eq!(rgb(&off), a);
    }

    /// The environment takes a Schlick F0 proxy for the film. Its split sum stays close to the
    /// film lobe's real directional albedo (the film's Fresnel at each half vector), so the
    /// reflected sky has the film's colour at roughly the film's level.
    #[test]
    fn the_film_f0_proxy_tracks_the_film_lobes_directional_albedo() {
        let dfg = DfgTable::default();
        let mut worst = 0.0f64;
        let mut total = 0.0f64;
        let mut count = 0.0f64;
        for roughness in [0.15f32, 0.3, 0.5] {
            for nm in [150.0f32, 250.0, 350.0, 450.0, 600.0] {
                for no_v in [0.95f32, 0.7, 0.5] {
                    let m = PtMaterial {
                        base_color: [0.0, 0.0, 0.0, 1.0],
                        roughness,
                        thin_film: Some(crate::scene::PtThinFilm {
                            factor: 1.0,
                            ior: 1.33,
                            thickness_min: nm,
                            thickness_max: nm,
                            map: None,
                        }),
                        ..PtMaterial::default()
                    };
                    let proxy = regia_render::thin_film::schlick_f0(no_v, [0.04; 3], 1.0, 1.33, nm);
                    let s = dfg.sample(no_v, roughness);
                    for (k, f0) in proxy.iter().enumerate() {
                        let exact = directional_albedo(&m, &dfg, no_v, k, 96, 48);
                        let split = f64::from(f0 * s[0] + s[1]);
                        let e = (exact - split).abs();
                        worst = worst.max(e);
                        total += e;
                        count += 1.0;
                    }
                }
            }
        }
        let mean = total / count;
        eprintln!("film F0 proxy against the directional albedo: worst {worst:.4}, mean {mean:.4}");
        assert!(worst < 0.09 && mean < 0.03, "worst {worst}, mean {mean}");
    }

    #[test]
    fn lambert_like_base_is_bounded_and_metal_compensation_helps() {
        let dfg = DfgTable::default();
        let white = PtMaterial::diffuse([1.0; 3]);
        let a = directional_albedo(&white, &dfg, 0.7, 0, 256, 128);
        assert!(a > 0.9 && a < 1.05, "{a}");
        let mut metal = PtMaterial {
            base_color: [1.0; 4],
            metallic: 1.0,
            roughness: 1.0,
            ..PtMaterial::default()
        };
        let single = directional_albedo(&metal, &dfg, 0.7, 0, 256, 128);
        metal.multi_scatter = true;
        let multi = directional_albedo(&metal, &dfg, 0.7, 0, 256, 128);
        assert!(multi > single, "{multi} vs {single}");
        assert!(single < 0.95, "rough single-scatter GGX loses energy: {single}");
    }

    // Analytic ground truth. These check the shared BRDF against closed forms and invariants
    // from the literature, not against the path tracer (which runs the same BRDF, so an error in
    // it cancels in every PT comparison). Sources: `docs/provenance.md`, row "BRDF analytic cases".

    /// `∫ D(h) cos θ_h dω_h = 1` over the hemisphere (the GGX normalization, Walter et al. 2007).
    #[test]
    fn ggx_distribution_integrates_to_one() {
        for a in [0.25f32, 0.5, 0.8, 1.0] {
            let steps = 200_000;
            let mut sum = 0.0f64;
            for i in 0..steps {
                let c = (f64::from(i) + 0.5) / f64::from(steps);
                sum += f64::from(d_ggx(c as f32, a)) * c;
            }
            let integral = sum / f64::from(steps) * std::f64::consts::TAU;
            assert!((integral - 1.0).abs() < 2e-3, "alpha {a}: {integral}");
        }
    }

    /// The height-correlated Smith visibility `V = G2 / (4 n·l n·v)`: `G2 -> 1` as α -> 0, and the
    /// closed form `0.5 / (n·l + n·v)` at α = 1 (Heitz 2014, eq. 99 specialized).
    #[test]
    fn smith_visibility_has_its_published_limits() {
        for (no_v, no_l) in [(0.9f32, 0.8f32), (0.3, 0.7), (0.05, 0.95)] {
            let g2 = v_smith(no_v, no_l, 1e-4) * 4.0 * no_v * no_l;
            assert!((g2 - 1.0).abs() < 1e-3, "smooth G2 {g2} at {no_v}, {no_l}");
            let v1 = v_smith(no_v, no_l, 1.0);
            assert!((v1 - 0.5 / (no_v + no_l)).abs() < 1e-5, "alpha 1: {v1}");
        }
    }

    /// Schlick's Fresnel against the exact unpolarized dielectric Fresnel for n = 1.5: equal at
    /// normal incidence (F0 = ((n-1)/(n+1))² = 0.04), 1 at grazing, and within 0.04 between
    /// (the approximation's known error band).
    #[test]
    fn schlick_tracks_the_exact_dielectric_fresnel() {
        let n = 1.5f64;
        let f0 = ((n - 1.0) / (n + 1.0)).powi(2) as f32;
        assert!((f0 - 0.04).abs() < 1e-6);
        assert!((schlick(1.0, f0) - f0).abs() < 1e-7);
        assert!((schlick(0.0, f0) - 1.0).abs() < 1e-7);
        for deg in 0..90 {
            let ci = f64::from(deg).to_radians().cos();
            let st = f64::from(deg).to_radians().sin() / n;
            let ct = (1.0 - st * st).sqrt();
            let rs = ((ci - n * ct) / (ci + n * ct)).powi(2);
            let rp = ((n * ci - ct) / (n * ci + ct)).powi(2);
            let exact = (rs + rp) / 2.0;
            let err = (f64::from(schlick(ci as f32, f0)) - exact).abs();
            assert!(err < 0.04, "{deg} deg: schlick off by {err}");
        }
    }

    /// Helmholtz reciprocity `f(v, l) = f(l, v)` of the single-scatter lobe (no clearcoat, no
    /// multi-scatter compensation, which are not reciprocal by construction).
    #[test]
    fn single_scatter_lobe_is_reciprocal() {
        let dfg = DfgTable::default();
        for (metallic, roughness) in [(0.0f32, 0.2f32), (0.0, 0.7), (1.0, 0.3), (1.0, 0.9)] {
            let m = PtMaterial {
                base_color: [0.8, 0.6, 0.3, 1.0],
                metallic,
                roughness,
                ..PtMaterial::default()
            };
            for (no_v, no_l, phi) in [(0.9f32, 0.4f32, 0.7f32), (0.2, 0.8, 2.5), (0.5, 0.5, 3.0)] {
                for k in 0..3 {
                    let fwd = brdf_cos(&m, &dfg, no_v, no_l, phi, k) / no_l;
                    let rev = brdf_cos(&m, &dfg, no_l, no_v, phi, k) / no_v;
                    assert!(
                        (fwd - rev).abs() <= 1e-4 * fwd.abs().max(1.0),
                        "m {metallic} r {roughness} at {no_v},{no_l},{phi}: {fwd} vs {rev}"
                    );
                }
            }
        }
    }

    /// The ideal Lambertian reflects exactly its albedo (`∫ ρ/π cos dω = ρ`).
    #[test]
    fn lambert_returns_its_albedo() {
        let dfg = DfgTable::default();
        let lambert = directional_albedo(&PtMaterial::lambert([0.5; 3]), &dfg, 0.6, 0, 128, 64);
        assert!((lambert - 0.5).abs() < 5e-3, "{lambert}");
    }

    /// Energy conservation of the compensated lobe (`multi_scatter`, the runtime default), white
    /// base colour. Metals conserve at every view angle. Dielectrics conserve away from grazing;
    /// at grazing views the diffuse term (scaled by the light-half-vector Fresnel, not by the
    /// view's albedo) adds to a strong specular and the total exceeds 1: measured 2026-10-30
    /// (white, 128x64 quadrature, checked at 1024x512): 1.12 at roughness 0.1 and n·v 0.05, 1.08 at
    /// roughness 0.4 and n·v 0.1. The grazing bound below pins that excess so it cannot grow; it
    /// is a finding for the renderer lane (ADR-1837), not a tolerance to widen.
    #[test]
    fn compensated_lobe_conserves_energy_except_at_grazing_dielectrics() {
        let dfg = DfgTable::default();
        for metallic in [0.0f32, 1.0] {
            for roughness in [0.1f32, 0.4, 0.8, 1.0] {
                for no_v in [0.05f32, 0.1, 0.3, 0.5, 0.95] {
                    let m = PtMaterial {
                        base_color: [1.0; 4],
                        metallic,
                        roughness,
                        multi_scatter: true,
                        ..PtMaterial::default()
                    };
                    let a = directional_albedo(&m, &dfg, no_v, 0, 128, 64);
                    let bound = if metallic == 1.0 {
                        1.01
                    } else if no_v >= 0.5 {
                        1.02
                    } else if no_v >= 0.3 {
                        1.04
                    } else {
                        1.14
                    };
                    assert!(
                        a <= bound,
                        "metallic {metallic} roughness {roughness} n.v {no_v}: albedo {a} over {bound}"
                    );
                }
            }
        }
    }

    // ADR-2632: `BrdfEnergy::Compensated`. The legacy test above keeps its bounds; these are the
    // compensated mode's own, tighter ones.

    /// Stated tolerance of the compensated white furnace: the 8-bit, 32×32 DFG table (one step is
    /// 1/255 ≈ 0.004 per channel, and the bilinear lookup between texels) and the split sum's
    /// Fresnel share stand in for the lobe's exact albedo.
    const COMPENSATED_FURNACE_TOLERANCE: f64 = 0.01;

    fn white(metallic: f32, roughness: f32, multi_scatter: bool) -> PtMaterial {
        PtMaterial {
            base_color: [1.0; 4],
            metallic,
            roughness,
            multi_scatter,
            ..PtMaterial::default()
        }
    }

    /// The white furnace under the compensated model: at every view angle (n·v 0.05 to 0.95),
    /// roughness (0.05 to 1), metallic 0, 0.5 and 1, with and without the material's
    /// multi-scatter flag, the directional albedo is at most 1 + the stated tolerance. The legacy
    /// model reaches 1.12 (multi-scatter) and 1.68 (without) at grazing dielectrics.
    #[test]
    fn compensated_white_furnace_stays_at_or_under_one_everywhere() {
        let dfg = DfgTable::default();
        let mut worst = (0.0f64, String::new());
        let mut rows = Vec::new();
        for multi_scatter in [true, false] {
            for metallic in [0.0f32, 0.5, 1.0] {
                for roughness in [0.05f32, 0.1, 0.2, 0.4, 0.6, 0.8, 1.0] {
                    let mut row = format!("ms {multi_scatter:5} m {metallic:.1} r {roughness:.2}:");
                    for no_v in [0.05f32, 0.1, 0.2, 0.3, 0.5, 0.7, 0.95] {
                        let m = white(metallic, roughness, multi_scatter);
                        let a =
                            directional_albedo_energy(&m, &dfg, BrdfEnergy::Compensated, no_v, 0, 192, 96);
                        row.push_str(&format!(" {a:.3}"));
                        if a > worst.0 {
                            worst = (
                                a,
                                format!("ms {multi_scatter} m {metallic} r {roughness} n.v {no_v}"),
                            );
                        }
                    }
                    rows.push(row);
                }
            }
        }
        eprintln!(
            "compensated furnace (n.v 0.05 0.1 0.2 0.3 0.5 0.7 0.95):\n{}",
            rows.join("\n")
        );
        eprintln!("worst {:.4} at {}", worst.0, worst.1);
        assert!(
            worst.0 <= 1.0 + COMPENSATED_FURNACE_TOLERANCE,
            "compensated furnace over 1: {:.4} at {}",
            worst.0,
            worst.1
        );
    }

    /// The compensated model gives back what single scattering loses: a white dielectric and a
    /// white metal with the multi-scatter flag stay close to 1 (not just under it), so the fix
    /// does not darken by trading the excess for a deficit.
    #[test]
    fn compensated_white_materials_with_multi_scatter_stay_near_one() {
        let dfg = DfgTable::default();
        for metallic in [0.0f32, 1.0] {
            for roughness in [0.1f32, 0.4, 0.8, 1.0] {
                for no_v in [0.1f32, 0.3, 0.5, 0.95] {
                    let m = white(metallic, roughness, true);
                    let a = directional_albedo_energy(&m, &dfg, BrdfEnergy::Compensated, no_v, 0, 192, 96);
                    assert!(a >= 0.95, "m {metallic} r {roughness} n.v {no_v}: albedo {a}");
                }
            }
        }
    }

    /// The tightened bound of the legacy test's grazing rows: where the legacy model is pinned
    /// at 1.14, the compensated one stays under 1 + the tolerance.
    #[test]
    fn compensated_grazing_dielectrics_meet_the_tight_bound() {
        let dfg = DfgTable::default();
        for roughness in [0.1f32, 0.4, 0.8, 1.0] {
            for no_v in [0.05f32, 0.1] {
                let m = white(0.0, roughness, true);
                let legacy = directional_albedo(&m, &dfg, no_v, 0, 128, 64);
                let fixed = directional_albedo_energy(&m, &dfg, BrdfEnergy::Compensated, no_v, 0, 128, 64);
                assert!(
                    fixed <= 1.0 + COMPENSATED_FURNACE_TOLERANCE,
                    "r {roughness} n.v {no_v}: compensated {fixed} (legacy {legacy})"
                );
            }
        }
        // The legacy finding is still there: the default BRDF did not move.
        let grazing = directional_albedo(&white(0.0, 0.1, true), &dfg, 0.05, 0, 128, 64);
        assert!(grazing > 1.05, "legacy grazing white dielectric: {grazing}");
        let single = directional_albedo(&white(0.0, 0.4, false), &dfg, 0.05, 0, 128, 64);
        // Measured 1.211 (128x64 quadrature, 2026-11-04).
        assert!(
            single > 1.15,
            "legacy single-scatter grazing white dielectric: {single}"
        );
    }

    /// Helmholtz reciprocity of the whole compensated lobe, multi-scatter included (the legacy
    /// test above excludes it): `f(v, l) = f(l, v)` for dielectrics, metals and a coloured base.
    #[test]
    fn compensated_lobe_is_reciprocal_with_multi_scatter() {
        let dfg = DfgTable::default();
        for (metallic, roughness) in [(0.0f32, 0.2f32), (0.0, 0.7), (1.0, 0.3), (1.0, 0.9), (0.5, 0.5)] {
            for multi_scatter in [true, false] {
                let m = PtMaterial {
                    base_color: [0.8, 0.6, 0.3, 1.0],
                    metallic,
                    roughness,
                    multi_scatter,
                    ..PtMaterial::default()
                };
                for (no_v, no_l, phi) in [
                    (0.9f32, 0.4f32, 0.7f32),
                    (0.2, 0.8, 2.5),
                    (0.5, 0.5, 3.0),
                    (0.07, 0.6, 1.0),
                ] {
                    for k in 0..3 {
                        let fwd =
                            brdf_cos_energy(&m, &dfg, BrdfEnergy::Compensated, no_v, no_l, phi, k) / no_l;
                        let rev =
                            brdf_cos_energy(&m, &dfg, BrdfEnergy::Compensated, no_l, no_v, phi, k) / no_v;
                        assert!(
                            (fwd - rev).abs() <= 1e-4 * fwd.abs().max(1.0),
                            "m {metallic} r {roughness} ms {multi_scatter} at {no_v},{no_l},{phi}: {fwd} vs {rev}"
                        );
                    }
                }
            }
        }
        // The legacy multi-scatter lobe is not reciprocal (why the legacy test leaves it out).
        let m = white(0.0, 0.7, true);
        let fwd = brdf_cos(&m, &dfg, 0.2, 0.8, 2.5, 0) / 0.8;
        let rev = brdf_cos(&m, &dfg, 0.8, 0.2, 2.5, 0) / 0.2;
        assert!((fwd - rev).abs() > 1e-3, "{fwd} vs {rev}");
    }

    /// The legacy entry points are the legacy model exactly (the default frame's BRDF).
    #[test]
    fn the_legacy_entry_points_are_the_legacy_model() {
        let dfg = DfgTable::default();
        let m = white(0.0, 0.4, true);
        for (no_v, no_l, phi) in [(0.9f32, 0.4f32, 0.7f32), (0.05, 0.8, 2.5)] {
            assert_eq!(
                brdf_cos(&m, &dfg, no_v, no_l, phi, 0).to_bits(),
                brdf_cos_energy(&m, &dfg, BrdfEnergy::Legacy, no_v, no_l, phi, 0).to_bits()
            );
        }
        let compensated = brdf_cos_energy(&m, &dfg, BrdfEnergy::Compensated, 0.05, 0.8, 2.5, 0);
        assert_ne!(compensated, brdf_cos(&m, &dfg, 0.05, 0.8, 2.5, 0));
    }

    /// `regia_render::brdf_energy::fresnel_bias_average` (the diffuse coupling's normalization)
    /// is a fit of this table's own average: it must track it, or the coupled diffuse gains or
    /// loses energy. Also checks the multiple-scattering normalization `1 − E_avg` (+ one 8-bit
    /// step) is never below the table's own `2 ∫ (1 − E) μ dμ`, so that lobe cannot add energy.
    #[test]
    fn the_bias_average_fit_tracks_the_table() {
        let dfg = DfgTable::default();
        let n = 1024;
        for j in 0..=64 {
            let r = j as f32 / 64.0;
            let (mut bias, mut kms) = (0.0f64, 0.0f64);
            for i in 0..n {
                let mu = (i as f32 + 0.5) / n as f32;
                let s = dfg.sample(mu, r);
                let w = f64::from(mu) * 2.0 / f64::from(n);
                kms += f64::from(1.0 - s[2].clamp(0.0, 1.0)) * w;
                let split = s[0] + s[1];
                if split > 1e-4 {
                    bias += f64::from(s[1] / split * s[2]) * w;
                }
            }
            let fit = f64::from(regia_render::brdf_energy::fresnel_bias_average(r));
            assert!((fit - bias).abs() < 5e-4, "r {r}: fit {fit} vs table {bias}");
            let used =
                f64::from(1.0 - dfg.sample(0.5, r)[3] + regia_render::brdf_energy::MS_NORMALIZATION_MARGIN);
            assert!(
                used >= kms - 1e-4,
                "r {r}: normalization {used} under the table's {kms}"
            );
        }
    }
}
