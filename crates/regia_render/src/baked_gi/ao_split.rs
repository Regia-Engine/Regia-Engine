// SPDX-License-Identifier: MPL-2.0
//! Probe-relative screen-space AO over the baked GI volume (ADR-1104): the CPU mirror of the
//! split the SSAO kernel (`ssao.wgsl` `cs_main`) and the baked screen pass
//! (`baked_gi_screen.wgsl`, capsule occluders) apply.
//!
//! **The double count.** The baked irradiance already holds the occlusion of everything the bake
//! saw, at the scale its probes resolve: a probe beside a wall sees the wall. The forward and
//! terrain passes multiply that light by the screen-space AO (`SSAO_RADIUS_M` kernel), which sees
//! the same wall again, so the foot of a baked wall under a 1 m brick lost its sky twice (model
//! below: 0.36 at 0.1 m from a 3 m wall where the visibility is 0.51). The capsule occluders of
//! dynamic units darken the baked light, and the SSAO sees the same unit: a unit's contact shade
//! was counted twice too (0.51 where the visibility is 0.67 beside a 1 m sphere; 0.66 split).
//!
//! **Which occlusion the probes hold.** A probe field resolves an occluder point the receiver's
//! probes also see: one above the probes. Probes sit a fraction of a cell off the surfaces, so an
//! occluder point whose height above the receiver's tangent plane is below about half a cell lies
//! under their horizon and is not in the probe light; one higher is. Each SSAO sample's
//! obscurance is therefore weighted, for the probe share of the light, by
//! [`probe_ao_keep`]`(height, ρ)` = clamp(1.5 − height / ρ, 0, 1), with ρ = [`PROBE_AO_RESOLVE`] ×
//! the cell edge (a ramp centred on ρ: full below ρ / 2, none past 3ρ / 2). The height is at most
//! the sample's distance, so the same weight also keeps every point nearer than ρ / 2. A detail
//! brick resolves at its largest cell edge, the terrain layer at its largest column spacing (8 m
//! columns: ρ = 4 m, past the whole kernel, so the terrain layer keeps the SSAO as before).
//!
//! **What the SSAO writes.** Per AO texel, with the lookup's brick share `b` and terrain share
//! `t` of the coverage ([`crate::baked_gi::StaticGiSample::brick_share`]; each layer's coverage
//! share weighted by how much of its cell sees the receiver, as the lookup blends them, and
//! several volumes blended by their share of the light):
//! `ao = (1 − b − t)·ao(all samples) + b·ao(brick-weighted) + t·ao(terrain-weighted)`. The sky
//! share of the light (outside the volume's coverage) keeps the whole kernel. One AO value serves
//! the whole indirect term, so at a partial coverage the mix is approximate (exact at 0 and 1).
//!
//! **Capsules.** A capsule's occlusion of the baked light is reduced by the share the SSAO
//! already counts ([`ProbeAoSplit::capsule_ssao_share`]): the kernel's falloff at the capsule's
//! representative hit distance (half-way between the sphere's nearest point and its tangent
//! distance), times the probe-share weight of its height. With SSAO off the capsules keep their
//! whole occlusion.
//!
//! **Limits.** The probe model is an estimate (a probe field's visibility is a smoothed field, not
//! a ray cast from each receiver): the double count shrinks, it does not vanish (wall model: mean
//! error 0.089 → 0.072). Geometry neither baked nor a capsule (a dynamic prop without capsules)
//! keeps only its low part of the SSAO under a brick. The model scenarios below measure both.
//!
//! Public sources: the irradiance volume and its probe visibility (Greger et al. 1998; Majercik et
//! al., JCGT 2019), Alchemy AO's obscurance estimator (McGuire et al., HPG 2011), the
//! configuration factor of a sphere (Howell's catalog). The scale split is derived here.

use regia_asset::baked_gi::TerrainLayer;

use super::StaticGiSample;

/// Fraction of a probe cell's edge at which the probe field starts to resolve an occluder
/// (ρ = this × the cell edge; `baked_gi_screen.wgsl` `PROBE_AO_RESOLVE`).
pub const PROBE_AO_RESOLVE: f32 = 0.5;

/// How much of an occluder point `height` metres above the receiver's tangent plane the probe
/// field does not already hold, for resolve distance `rho` (m): 1 below `rho / 2`, 0 past
/// `1.5 · rho`, linear between (`probe_ao_keep` in `ssao.wgsl` and `baked_gi_screen.wgsl`).
#[must_use]
pub fn probe_ao_keep(height: f32, rho: f32) -> f32 {
    (1.5 - height / rho.max(1e-4)).clamp(0.0, 1.0)
}

/// A terrain layer's resolve distance (m): [`PROBE_AO_RESOLVE`] × its largest column spacing;
/// 0 without a terrain layer.
#[must_use]
pub fn terrain_resolve_distance(terrain: Option<&TerrainLayer>) -> f32 {
    terrain.map_or(0.0, |t| PROBE_AO_RESOLVE * t.spacing[0].max(t.spacing[1]))
}

/// One SSAO kernel sample, as `ssao.wgsl` measures it.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SsaoSample {
    /// Height of the sample point above the receiver's tangent plane (m).
    pub height: f32,
    /// Its obscurance: the rise cosine past the bias, times the kernel's falloff.
    pub obscurance: f32,
}

/// The probe-relative AO split at one screen texel: the shares of the baked light's coverage the
/// detail brick and the terrain layer hold and their resolve distances.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ProbeAoSplit {
    /// Share of the coverage the detail brick holds (0..=1).
    pub brick_share: f32,
    /// Share of the coverage the terrain layer holds (0..=1; with the brick share at most 1).
    pub terrain_share: f32,
    /// The brick's resolve distance (m): [`PROBE_AO_RESOLVE`] × its largest cell edge.
    pub brick_rho: f32,
    /// The terrain layer's resolve distance (m): [`PROBE_AO_RESOLVE`] × its largest column
    /// spacing ([`terrain_resolve_distance`]).
    pub terrain_rho: f32,
}

impl ProbeAoSplit {
    /// The split the baked screen pass writes for a lookup ([`crate::baked_gi::sample_static_gi`]
    /// or [`crate::baked_gi::sample_static_gi_blend`]): its shares, and ρ = [`PROBE_AO_RESOLVE`] ×
    /// each layer's cell size.
    #[must_use]
    pub fn of(sample: &StaticGiSample) -> Self {
        Self {
            brick_share: sample.brick_share,
            terrain_share: sample.terrain_share,
            brick_rho: PROBE_AO_RESOLVE * sample.brick_cell,
            terrain_rho: PROBE_AO_RESOLVE * sample.terrain_cell,
        }
    }

    /// The share of the light the split leaves to the whole kernel (outside the coverage).
    #[must_use]
    pub fn sky_share(&self) -> f32 {
        (1.0 - self.brick_share - self.terrain_share).max(0.0)
    }

    /// The AO value the SSAO kernel writes for `samples` out of `count` taken, at `intensity`
    /// (`ssao.wgsl` `cs_main`; `count` is the kernel's sample count, misses included).
    #[must_use]
    pub fn ssao_visibility(&self, samples: &[SsaoSample], count: usize, intensity: f32) -> f32 {
        let (mut all, mut brick, mut terrain) = (0.0f32, 0.0f32, 0.0f32);
        for s in samples {
            all += s.obscurance;
            brick += s.obscurance * probe_ao_keep(s.height, self.brick_rho);
            terrain += s.obscurance * probe_ao_keep(s.height, self.terrain_rho);
        }
        let k = intensity / count.max(1) as f32;
        let ao = |x: f32| (1.0 - x * k).clamp(0.0, 1.0);
        self.sky_share() * ao(all) + self.brick_share * ao(brick) + self.terrain_share * ao(terrain)
    }

    /// The share (0..=1) of a capsule's occlusion of the baked light that the SSAO kernel of
    /// radius `ssao_radius` (m, 0 = SSAO off) already counts: the capsule's nearest sphere is `d`
    /// metres from the receiver, of radius `r`, at cosine `cos_n` from its normal
    /// (`baked_gi_screen.wgsl` `bgi_capsule_ssao_share`).
    #[must_use]
    pub fn capsule_ssao_share(&self, d: f32, r: f32, cos_n: f32, ssao_radius: f32) -> f32 {
        if ssao_radius <= 0.0 {
            return 0.0;
        }
        // Half-way between the sphere's nearest point and its tangent distance: where the rays
        // that hit it land on average.
        let t = (0.5 * ((d - r) + (d * d - r * r).max(0.0).sqrt())).max(0.0);
        let falloff = (1.0 - t * t / (ssao_radius * ssao_radius)).clamp(0.0, 1.0);
        let covered = self.brick_share + self.terrain_share;
        if covered <= 1e-6 {
            return falloff;
        }
        let height = cos_n.max(0.0) * t;
        falloff
            * (self.brick_share * probe_ao_keep(height, self.brick_rho)
                + self.terrain_share * probe_ao_keep(height, self.terrain_rho))
            / covered
    }

    /// The three words the baked screen pass writes per texel after the lookup's texels:
    /// `pack2x16unorm(brick share, terrain share)`, then the brick's and the terrain layer's
    /// resolve distances as f32 bits (per texel: blended volumes can differ). The shader packs
    /// them itself; this documents and tests the layout.
    #[cfg(test)]
    fn encode(&self) -> [u32; 3] {
        let unorm = |v: f32| (v.clamp(0.0, 1.0) * 65535.0).round() as u32;
        [
            unorm(self.brick_share) | (unorm(self.terrain_share) << 16),
            self.brick_rho.to_bits(),
            self.terrain_rho.to_bits(),
        ]
    }

    /// The split from a texel's three words (`encode`'s layout).
    pub(crate) fn decode(words: [u32; 3]) -> Self {
        let unorm = |v: u32| (v & 0xffff) as f32 / 65535.0;
        Self {
            brick_share: unorm(words[0]),
            terrain_share: unorm(words[0] >> 16),
            brick_rho: f32::from_bits(words[1]),
            terrain_rho: f32::from_bits(words[2]),
        }
    }
}

#[cfg(test)]
mod tests {
    //! The split's arithmetic, and model scenarios: a receiver on a floor beside static geometry
    //! (lit through a 1 m brick) or a dynamic sphere (a capsule occluder), where the cosine-
    //! weighted visibility is ray cast exactly and the two estimates the renderer combines are
    //! modelled: the probe field's (bilinear between probes, those inside geometry dropped) and
    //! the SSAO kernel's (each ray that hits within the radius is a sample at its hit point,
    //! with the kernel's falloff). The old combination (product of the two) is compared with the
    //! split against the exact visibility.
    use super::*;
    use crate::baked_gi::{GiCapsule, capsule_visibility, capsule_visibility_split};
    use crate::ssao::SSAO_RADIUS_M;

    type V3 = [f32; 3];

    #[test]
    fn the_keep_ramp_is_centred_on_the_resolve_distance() {
        assert_eq!(probe_ao_keep(0.0, 0.5), 1.0);
        assert_eq!(probe_ao_keep(0.25, 0.5), 1.0);
        assert!((probe_ao_keep(0.5, 0.5) - 0.5).abs() < 1e-6);
        assert_eq!(probe_ao_keep(0.75, 0.5), 0.0);
        assert_eq!(probe_ao_keep(3.0, 0.5), 0.0);
        // A degenerate resolve distance keeps only the contact itself.
        assert_eq!(probe_ao_keep(0.0, 0.0), 1.0);
        assert_eq!(probe_ao_keep(0.01, 0.0), 0.0);
    }

    #[test]
    fn without_coverage_the_kernel_is_unchanged() {
        let samples = [
            SsaoSample {
                height: 0.1,
                obscurance: 0.6,
            },
            SsaoSample {
                height: 0.9,
                obscurance: 0.4,
            },
        ];
        let none = ProbeAoSplit::default();
        let plain = (1.0f32 - (0.6 + 0.4) * 1.6 / 12.0).clamp(0.0, 1.0);
        assert!((none.ssao_visibility(&samples, 12, 1.6) - plain).abs() < 1e-6);
        // The terrain layer's 8 m columns resolve nothing the kernel sees.
        let terrain = ProbeAoSplit {
            terrain_share: 1.0,
            terrain_rho: 4.0,
            ..ProbeAoSplit::default()
        };
        assert!((terrain.ssao_visibility(&samples, 12, 1.6) - plain).abs() < 1e-6);
        // A 1 m brick takes the high sample out of the probe share.
        let brick = ProbeAoSplit {
            brick_share: 1.0,
            brick_rho: 0.5,
            ..ProbeAoSplit::default()
        };
        let low_only = 1.0 - 0.6 * 1.6 / 12.0;
        assert!((brick.ssao_visibility(&samples, 12, 1.6) - low_only).abs() < 1e-6);
        // Half covered: half of each.
        let half = ProbeAoSplit {
            brick_share: 0.5,
            ..brick
        };
        assert!((half.ssao_visibility(&samples, 12, 1.6) - 0.5 * (plain + low_only)).abs() < 1e-6);
    }

    #[test]
    fn the_split_never_darkens() {
        let split = ProbeAoSplit {
            brick_share: 0.6,
            terrain_share: 0.3,
            brick_rho: 0.5,
            terrain_rho: 4.0,
        };
        let samples: Vec<SsaoSample> = (0..16)
            .map(|i| SsaoSample {
                height: i as f32 * 0.08,
                obscurance: 0.05 * (i % 5) as f32,
            })
            .collect();
        let none = ProbeAoSplit::default().ssao_visibility(&samples, 16, 1.6);
        assert!(split.ssao_visibility(&samples, 16, 1.6) >= none);
    }

    #[test]
    fn the_capsule_share_follows_the_kernel() {
        let terrain = ProbeAoSplit {
            terrain_share: 1.0,
            terrain_rho: 4.0,
            ..ProbeAoSplit::default()
        };
        // SSAO off: the capsule keeps everything.
        assert_eq!(terrain.capsule_ssao_share(1.1, 1.0, 0.9, 0.0), 0.0);
        // Touching: the kernel counts it all; past the kernel, none.
        assert!((terrain.capsule_ssao_share(1.0, 1.0, 1.0, 1.2) - 1.0).abs() < 1e-6);
        assert_eq!(terrain.capsule_ssao_share(4.0, 1.0, 1.0, 1.2), 0.0);
        // Under a brick, a sphere high above the probes' level is the capsule's alone.
        let brick = ProbeAoSplit {
            brick_share: 1.0,
            brick_rho: 0.5,
            ..ProbeAoSplit::default()
        };
        let d = 1.6;
        assert!(brick.capsule_ssao_share(d, 1.0, 1.0, 1.2) < terrain.capsule_ssao_share(d, 1.0, 1.0, 1.2));
        // Without coverage (no probe light) the kernel's falloff alone.
        let none = ProbeAoSplit::default();
        assert!(
            (none.capsule_ssao_share(d, 1.0, 1.0, 1.2) - terrain.capsule_ssao_share(d, 1.0, 1.0, 1.2)).abs()
                < 1e-6
        );
    }

    #[test]
    fn the_texel_words_round_trip() {
        let s = ProbeAoSplit {
            brick_share: 0.25,
            terrain_share: 0.75,
            brick_rho: 0.5,
            terrain_rho: 4.0,
        };
        let [shares, brick_rho, terrain_rho] = s.encode();
        assert_eq!(shares & 0xffff, (0.25f32 * 65535.0).round() as u32);
        assert_eq!(shares >> 16, (0.75f32 * 65535.0).round() as u32);
        assert_eq!(f32::from_bits(brick_rho), 0.5);
        assert_eq!(f32::from_bits(terrain_rho), 4.0);
        assert_eq!(ProbeAoSplit::default().encode(), [0, 0, 0]);
        let back = ProbeAoSplit::decode(s.encode());
        assert!((back.brick_share - 0.25).abs() < 1e-4 && (back.terrain_share - 0.75).abs() < 1e-4);
        assert_eq!((back.brick_rho, back.terrain_rho), (0.5, 4.0));
    }

    // ---- Model scenarios ----

    /// Cosine-weighted directions around +y on a fixed grid (no randomness).
    fn hemisphere(n: usize) -> Vec<V3> {
        let mut out = Vec::with_capacity(n * n);
        for i in 0..n {
            let u1 = (i as f32 + 0.5) / n as f32;
            for j in 0..n {
                let u2 = (j as f32 + 0.5) / n as f32;
                let r = u1.sqrt();
                let phi = std::f32::consts::TAU * u2;
                out.push([r * phi.cos(), (1.0 - u1).max(0.0).sqrt(), r * phi.sin()]);
            }
        }
        out
    }

    #[derive(Clone, Copy)]
    enum Shape {
        /// Axis-aligned box: min, max.
        Box(V3, V3),
        /// Sphere: centre, radius.
        Sphere(V3, f32),
    }

    fn hit(shape: Shape, o: V3, d: V3) -> Option<f32> {
        match shape {
            Shape::Box(lo, hi) => {
                let (mut t0, mut t1) = (f32::NEG_INFINITY, f32::INFINITY);
                for (((&oa, &da), &la), &ha) in o.iter().zip(&d).zip(&lo).zip(&hi) {
                    if da.abs() < 1e-9 {
                        if oa < la || oa > ha {
                            return None;
                        }
                    } else {
                        let (ta, tb) = ((la - oa) / da, (ha - oa) / da);
                        t0 = t0.max(ta.min(tb));
                        t1 = t1.min(ta.max(tb));
                    }
                }
                (t1 >= t0.max(0.0)).then_some(t0.max(0.0))
            }
            Shape::Sphere(c, r) => {
                let oc = [o[0] - c[0], o[1] - c[1], o[2] - c[2]];
                let b = oc[0] * d[0] + oc[1] * d[1] + oc[2] * d[2];
                let cc = oc[0] * oc[0] + oc[1] * oc[1] + oc[2] * oc[2] - r * r;
                let disc = b * b - cc;
                if disc < 0.0 {
                    return None;
                }
                let t = -b - disc.sqrt();
                let t = if t < 0.0 { -b + disc.sqrt() } else { t };
                (t >= 0.0).then_some(t)
            }
        }
    }

    fn inside(shape: Shape, p: V3) -> bool {
        match shape {
            Shape::Box(lo, hi) => (0..3).all(|a| p[a] >= lo[a] && p[a] <= hi[a]),
            Shape::Sphere(c, r) => {
                (p[0] - c[0]).powi(2) + (p[1] - c[1]).powi(2) + (p[2] - c[2]).powi(2) < r * r
            }
        }
    }

    /// Nearest hit distance per direction (`None`: open sky).
    fn cast(scene: &[Shape], dirs: &[V3], o: V3) -> Vec<Option<f32>> {
        dirs.iter()
            .map(|&d| scene.iter().filter_map(|&s| hit(s, o, d)).min_by(f32::total_cmp))
            .collect()
    }

    fn visibility(hits: &[Option<f32>]) -> f32 {
        hits.iter().filter(|h| h.is_none()).count() as f32 / hits.len() as f32
    }

    /// The idealised kernel: each ray that hits within the radius is a sample at its hit point
    /// (height = distance × the ray's cosine), obscurance = the kernel's falloff.
    fn kernel(hits: &[Option<f32>], dirs: &[V3], split: &ProbeAoSplit) -> f32 {
        let r2 = SSAO_RADIUS_M * SSAO_RADIUS_M;
        let samples: Vec<SsaoSample> = hits
            .iter()
            .zip(dirs)
            .filter_map(|(h, d)| {
                let t = (*h)?;
                (t < SSAO_RADIUS_M).then(|| SsaoSample {
                    height: t * d[1],
                    obscurance: (1.0 - t * t / r2).max(0.0),
                })
            })
            .collect();
        split.ssao_visibility(&samples, hits.len(), 1.0)
    }

    /// The probe field's visibility toward +y at a floor point `x` (scene invariant along z):
    /// probes every `cell` metres at (cell/2 + i·cell, cell/2 + j·cell), bilinear, a probe
    /// inside geometry dropped.
    fn probe_field(scene: &[Shape], dirs: &[V3], x: f32, cell: f32) -> f32 {
        let gx = (x - 0.5 * cell) / cell;
        let i0 = gx.floor();
        let fx = gx - i0;
        let (mut sum, mut wsum) = (0.0, 0.0);
        for di in 0..2 {
            let px = (i0 + di as f32) * cell + 0.5 * cell;
            // The floor point sits under the first probe row (y = cell/2): no lower row.
            let p = [px, 0.5 * cell, 0.0];
            if scene.iter().any(|&s| inside(s, p)) {
                continue;
            }
            let w = (if di == 1 { fx } else { 1.0 - fx }) + 1e-3;
            sum += w * visibility(&cast(scene, dirs, p));
            wsum += w;
        }
        sum / wsum
    }

    const FAR: f32 = 1000.0;
    const RECEIVERS: [f32; 7] = [0.05, 0.1, 0.2, 0.3, 0.5, 0.8, 1.2];

    /// Mean |error| of the old product and of the split over the receivers, beside `scene`
    /// lit through a 1 m brick; asserts the split never darkens and never exceeds the probe
    /// light's own visibility.
    fn static_errors(scene: &[Shape]) -> (f32, f32) {
        let dirs = hemisphere(48);
        let split = ProbeAoSplit {
            brick_share: 1.0,
            brick_rho: PROBE_AO_RESOLVE * 1.0,
            ..ProbeAoSplit::default()
        };
        let (mut old, mut new) = (0.0, 0.0);
        for x in RECEIVERS {
            let hits = cast(scene, &dirs, [x, 0.0, 0.0]);
            let truth = visibility(&hits);
            let probe = probe_field(scene, &dirs, x, 1.0);
            let product = probe * kernel(&hits, &dirs, &ProbeAoSplit::default());
            let relative = probe * kernel(&hits, &dirs, &split);
            assert!(
                relative >= product - 1e-6 && relative <= probe + 1e-6,
                "{x}: {relative} {product} {probe}"
            );
            old += (product - truth).abs();
            new += (relative - truth).abs();
        }
        (old / RECEIVERS.len() as f32, new / RECEIVERS.len() as f32)
    }

    #[test]
    fn a_baked_wall_is_not_occluded_twice() {
        // A 3 m wall: the probes beside it see it, the kernel sees it again.
        let (old, new) = static_errors(&[Shape::Box([-FAR, 0.0, -FAR], [0.0, 3.0, FAR])]);
        assert!(new < 0.9 * old, "wall: old {old} new {new}");
        // An overhang 0.6 m up (a ledge, a vehicle bay): the probes hold most of it.
        let (old, new) = static_errors(&[Shape::Box([-FAR, 0.6, -FAR], [0.0, 3.0, FAR])]);
        assert!(new < 0.9 * old, "ledge: old {old} new {new}");
        // A 1 m block, 1 m wide.
        let (old, new) = static_errors(&[Shape::Box([-FAR, 0.0, -0.5], [0.0, 1.0, 0.5])]);
        assert!(new < 0.75 * old, "block: old {old} new {new}");
    }

    #[test]
    fn a_curb_below_the_probes_keeps_its_contact_shade() {
        // 0.3 m high: under the probes' horizon, so the probe light holds none of it and the
        // product was right. The split keeps it (the low points stay in the probe share).
        let (old, new) = static_errors(&[Shape::Box([-FAR, 0.0, -FAR], [0.0, 0.3, FAR])]);
        assert!(new <= old + 0.005, "curb: old {old} new {new}");
    }

    /// Mean |error| of the old product (full kernel × whole capsule) and of the split, for floor
    /// receivers beside a sphere of `radius` resting on the floor (a dynamic unit: the probe
    /// light holds none of it, the capsule stands for it).
    fn capsule_errors(radius: f32, gaps: &[f32], split: ProbeAoSplit) -> (f32, f32) {
        let dirs = hemisphere(48);
        let (mut old, mut new) = (0.0, 0.0);
        for &x in gaps {
            let centre = [x, radius, 0.0];
            let hits = cast(&[Shape::Sphere(centre, radius)], &dirs, [0.0; 3]);
            let truth = visibility(&hits);
            let caps = [GiCapsule::sphere(centre, radius)];
            let whole = capsule_visibility(&caps, [0.0; 3], [0.0, 1.0, 0.0]);
            let product = kernel(&hits, &dirs, &ProbeAoSplit::default()) * whole;
            let rest = capsule_visibility_split(&caps, [0.0; 3], [0.0, 1.0, 0.0], &split, SSAO_RADIUS_M);
            let relative = kernel(&hits, &dirs, &split) * rest;
            old += (product - truth).abs();
            new += (relative - truth).abs();
        }
        (old / gaps.len() as f32, new / gaps.len() as f32)
    }

    #[test]
    fn a_unit_is_not_occluded_twice() {
        let terrain = ProbeAoSplit {
            terrain_share: 1.0,
            terrain_rho: 4.0,
            ..ProbeAoSplit::default()
        };
        let brick = ProbeAoSplit {
            brick_share: 1.0,
            brick_rho: 0.5,
            ..ProbeAoSplit::default()
        };
        let gaps = [1.05, 1.2, 1.5, 2.0, 2.5, 3.0];
        for (name, split) in [("terrain", terrain), ("brick", brick)] {
            let (old, new) = capsule_errors(1.0, &gaps, split);
            assert!(new < 0.5 * old, "1 m unit over the {name}: old {old} new {new}");
        }
        let (old, new) = capsule_errors(0.3, &[0.35, 0.5, 0.8, 1.2], terrain);
        assert!(new < 0.5 * old, "0.3 m unit: old {old} new {new}");
    }

    #[test]
    fn a_volume_reports_its_resolve_distances() {
        let sample = StaticGiSample {
            coverage: 1.0,
            brick_share: 0.7,
            terrain_share: 0.3,
            brick_cell: 2.0,
            ..StaticGiSample::default()
        };
        let split = ProbeAoSplit::of(&sample);
        assert_eq!(split.brick_rho, 1.0);
        assert_eq!(split.terrain_rho, 0.0, "no terrain layer");
        assert!(split.sky_share().abs() < 1e-6);
        let columns = ProbeAoSplit::of(&StaticGiSample {
            terrain_cell: 8.0,
            ..sample
        });
        assert_eq!(columns.terrain_rho, 4.0);
        let layer = TerrainLayer {
            origin: [0.0; 2],
            spacing: [8.0, 6.0],
            counts: [2, 2],
            levels: vec![1.0],
            ground: vec![0.0; 4],
        };
        assert_eq!(terrain_resolve_distance(Some(&layer)), 4.0);
    }
}
