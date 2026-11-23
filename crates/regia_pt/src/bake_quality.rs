// SPDX-License-Identifier: MPL-2.0
//! Multi-bounce bake quality controls (ADR-1096): what the probe bake does with its sample groups
//! (firefly suppression and the noise estimate), and how much light a bounce count can miss.
//!
//! - **Sample groups.** [`crate::probe::ProbeTraceSettings::sample_groups`] splits a probe's
//!   samples into groups that are summed and read back separately. [`combine_groups`] folds them
//!   back into one estimate.
//! - **Firefly suppression** (median of means; Lugosi & Mendelson 2019, applied to rendering by
//!   Buisine et al. 2021): with three groups or more and a threshold `k > 1`, a group whose mean
//!   luminance exceeds `k ×` the median group's is scaled down to it. A firefly is one rare,
//!   very bright path; it lands in one group and lifts that group far above the others. As the
//!   sample count grows the group means concentrate, no group crosses `k ×` the median, and the
//!   estimate becomes the plain mean again: the suppression is consistent. A probe that most
//!   groups see as black is left alone, because one group cannot tell a rare light from a firefly.
//! - **Noise estimate**: with two groups or more, the spread of the group means gives the relative
//!   standard error of the probe's mean luminance. Noise falls as `1/√samples`, so
//!   `samples × (noise / target)²` samples reach `target`.
//! - **Bounce budget** ([`bounce_energy_bounds`], [`bounce_budget`]): in a closed room of uniform
//!   albedo `a`, light that reflected `j` times carries `a^j` of the emitted light, so a bake that
//!   stops after `B` path vertices keeps between `1 − a^B` and `1 − a^(B+1)` of the converged
//!   result (the last bounce is partly kept: next-event estimation at the last vertex adds it,
//!   the BSDF-sampled half of its MIS pair is cut). Open scenes miss less (sky escapes end paths).
//!
//! The per-contribution luminance clamps (`clamp_direct`, `clamp_indirect`) run in the shader
//! (`pt.wgsl`, `add_probe`); [`crate::probe::ProbeTrace::clamp_loss`] reports what they removed.

use regia_asset::baked_gi::GI_COMPONENTS;
use serde::Serialize;

use crate::scene::PtScene;

/// SH sum rows per probe: one per light component and color channel, component-major.
pub const SH_ROWS: usize = GI_COMPONENTS * 3;

/// One probe's SH sums in one sample group: [`SH_ROWS`] rows (component, then R, G, B) of the
/// four L1 coefficients (L00, L1x, L1y, L1z), summed over the group's rays before the `4π/N`
/// Monte Carlo scale.
pub type ShSums = [[f32; 4]; SH_ROWS];

/// Fewest sample groups firefly suppression runs with (with two, the median is their mean and an
/// outlier can never exceed twice it).
pub const MIN_FIREFLY_GROUPS: usize = 3;

/// The most sample groups a trace splits into.
pub const MAX_SAMPLE_GROUPS: u32 = 64;

/// One probe's sums in one sample group.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GroupSums {
    /// SH sums.
    pub sh: ShSums,
    /// Finite rays the sums hold.
    pub rays: f32,
}

/// One probe's sample groups folded into one estimate ([`combine_groups`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Combined {
    /// SH sums after suppression.
    pub sh: ShSums,
    /// Finite rays in all the groups.
    pub rays: f32,
    /// Groups scaled down by firefly suppression.
    pub suppressed: u32,
    /// Fraction of the probe's L00 luminance that suppression removed (0..1).
    pub removed: f32,
    /// Relative standard error of the probe's mean luminance, from the spread of the group means
    /// (after suppression). `None` with fewer than two non-empty groups or a black probe.
    pub noise: Option<f32>,
}

/// Rec. 709 luminance of the L00 sums of every component (proportional to the summed ray
/// luminance; the shader's `luminance`).
#[must_use]
pub fn l00_luminance(sh: &ShSums) -> f32 {
    sh.as_chunks::<3>()
        .0
        .iter()
        .map(|c| 0.2126 * c[0][0] + 0.7152 * c[1][0] + 0.0722 * c[2][0])
        .sum()
}

/// The median of `values` (the mean of the middle two for an even count; 0 when empty).
#[must_use]
pub fn median(values: &[f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v = values.to_vec();
    v.sort_by(f32::total_cmp);
    let mid = v.len() / 2;
    if v.len().is_multiple_of(2) {
        0.5 * (v[mid - 1] + v[mid])
    } else {
        v[mid]
    }
}

fn scaled(sh: &ShSums, k: f32) -> ShSums {
    let mut out = *sh;
    for v in out.iter_mut().flatten() {
        *v *= k;
    }
    out
}

/// Folds one probe's sample groups into one estimate: firefly suppression when
/// `firefly_threshold > 1` and there are at least [`MIN_FIREFLY_GROUPS`] groups, then the sum
/// and the noise estimate. One group (or a threshold of 0) returns its sums bit for bit.
#[must_use]
pub fn combine_groups(groups: &[GroupSums], firefly_threshold: f32) -> Combined {
    let Some(first) = groups.first() else {
        return Combined {
            sh: [[0.0; 4]; SH_ROWS],
            rays: 0.0,
            suppressed: 0,
            removed: 0.0,
            noise: None,
        };
    };
    let means: Vec<f32> = groups
        .iter()
        .map(|g| {
            if g.rays > 0.0 {
                l00_luminance(&g.sh) / g.rays
            } else {
                0.0
            }
        })
        .collect();
    let mut scale = vec![1.0f32; groups.len()];
    let mut suppressed = 0u32;
    let suppress =
        groups.len() >= MIN_FIREFLY_GROUPS && firefly_threshold > 1.0 && firefly_threshold.is_finite();
    if suppress {
        let filled: Vec<f32> = groups
            .iter()
            .zip(&means)
            .filter(|(g, _)| g.rays > 0.0)
            .map(|(_, m)| *m)
            .collect();
        let m = median(&filled);
        if m > 0.0 {
            let cap = firefly_threshold * m;
            for (s, &mean) in scale.iter_mut().zip(&means) {
                if mean > cap {
                    *s = cap / mean;
                    suppressed += 1;
                }
            }
        }
    }
    let mut sh = scaled(&first.sh, scale[0]);
    let mut rays = first.rays;
    for (g, &s) in groups.iter().zip(&scale).skip(1) {
        for (a, b) in sh.iter_mut().flatten().zip(g.sh.iter().flatten()) {
            *a += s * b;
        }
        rays += g.rays;
    }
    let (mut before, mut after) = (0.0f64, 0.0f64);
    for ((g, &m), &s) in groups.iter().zip(&means).zip(&scale) {
        before += f64::from(m) * f64::from(g.rays);
        after += f64::from(m * s) * f64::from(g.rays);
    }
    let removed = if before > 0.0 {
        ((before - after) / before).clamp(0.0, 1.0) as f32
    } else {
        0.0
    };
    // Spread of the (suppressed) group means: the standard error of their mean, relative to it.
    let kept: Vec<f64> = groups
        .iter()
        .zip(&means)
        .zip(&scale)
        .filter(|((g, _), _)| g.rays > 0.0)
        .map(|((_, &m), &s)| f64::from(m * s))
        .collect();
    let noise = if kept.len() >= 2 {
        let n = kept.len() as f64;
        let mean = kept.iter().sum::<f64>() / n;
        let var = kept.iter().map(|m| (m - mean) * (m - mean)).sum::<f64>() / (n - 1.0);
        (mean > 0.0).then(|| ((var / n).sqrt() / mean) as f32)
    } else {
        None
    };
    Combined {
        sh,
        rays,
        suppressed,
        removed,
        noise,
    }
}

/// The sample range `[start, end)` of group `group` of `groups` over `samples` samples: sizes
/// differ by one at most, the larger groups first. Independent of how probes are batched.
#[must_use]
pub fn group_range(samples: u32, groups: u32, group: u32) -> (u32, u32) {
    let groups = groups.max(1);
    let base = samples / groups;
    let extra = samples % groups;
    let start = group * base + group.min(extra);
    let len = base + u32::from(group < extra);
    (start, start + len)
}

/// Fraction of the converged light a bake with `max_bounces` path vertices keeps in a closed room
/// of uniform albedo `albedo`: `(1 − a^B, 1 − a^(B+1))`. The lower bound counts the reflections
/// the bake always keeps (up to `B − 1`); the upper bound adds the `B`-th, which next-event
/// estimation at the last vertex keeps in part (all of it for a delta sun).
#[must_use]
pub fn bounce_energy_bounds(albedo: f32, max_bounces: u32) -> (f32, f32) {
    let a = albedo.clamp(0.0, 1.0);
    let b = max_bounces.max(1);
    let pow = |n: u32| a.powi(n.min(i32::MAX as u32) as i32);
    (1.0 - pow(b), 1.0 - pow(b + 1))
}

/// The smallest bounce count (1..=64) whose lower bound misses at most `tolerance` of the light
/// in a closed room of albedo `albedo` (`a^B ≤ tolerance`); 64 when even that misses more.
#[must_use]
pub fn bounces_for(albedo: f32, tolerance: f32) -> u32 {
    let a = albedo.clamp(0.0, 1.0);
    if a <= 0.0 || tolerance >= 1.0 {
        return 1;
    }
    if a >= 1.0 || tolerance <= 0.0 {
        return 64;
    }
    (1..=64u32).find(|&b| a.powi(b as i32) <= tolerance).unwrap_or(64)
}

/// The bounce count's worst case for a scene ([`bounce_budget`]): printed by `regia bake-gi`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct BounceBudget {
    /// Path vertices after the probe.
    pub max_bounces: u32,
    /// Highest luminance of a base color the scene traces (terrain and instances), 0..1.
    pub max_albedo: f32,
    /// Upper bound of the fraction of the light the bounce count drops: `max_albedo^max_bounces`
    /// (a closed room of the brightest material; open scenes drop less).
    pub missing_bound: f32,
    /// Bounces that keep the bound under 1 % ([`bounces_for`]).
    pub bounces_for_1_percent: u32,
}

/// The bounce budget of `scene` at `max_bounces` path vertices.
#[must_use]
pub fn bounce_budget(scene: &PtScene, max_bounces: u32) -> BounceBudget {
    let lum = |c: [f32; 4]| (0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]).clamp(0.0, 1.0);
    let used = scene
        .instances
        .iter()
        .map(|i| i.material)
        .chain(scene.terrain.as_ref().map(|t| t.material));
    let max_albedo = used
        .filter_map(|m| scene.materials.get(m))
        .map(|m| lum(m.base_color))
        .fold(0.0f32, f32::max);
    let b = max_bounces.clamp(1, 64);
    BounceBudget {
        max_bounces: b,
        max_albedo,
        missing_bound: 1.0 - bounce_energy_bounds(max_albedo, b).0,
        bounces_for_1_percent: bounces_for(max_albedo, 0.01),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SplitMix64 (Steele, Lea & Flood 2014): a fixed stream for the convergence tests.
    struct Rng(u64);

    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// A group of `rays` grey rays of luminance `value`: L00 sums of `value · Y00` per ray in
    /// every channel of the sky component.
    fn grey(value: f64, rays: u32) -> GroupSums {
        let mut sh = [[0.0f32; 4]; SH_ROWS];
        for row in &mut sh[3..6] {
            row[0] = (value * 0.282_094_8) as f32;
        }
        GroupSums {
            sh,
            rays: rays as f32,
        }
    }

    /// Mean ray luminance of combined sums.
    fn mean(c: &Combined) -> f64 {
        f64::from(l00_luminance(&c.sh)) / f64::from(c.rays) / 0.282_094_8
    }

    /// `groups` groups of `rays` draws of `draw`: the group sums of the per-ray values.
    fn simulate(rng: &mut Rng, groups: u32, rays: u32, draw: &dyn Fn(&mut Rng) -> f64) -> Vec<GroupSums> {
        (0..groups)
            .map(|_| {
                let sum: f64 = (0..rays).map(|_| draw(rng)).sum();
                grey(sum, rays)
            })
            .collect()
    }

    #[test]
    fn median_of_odd_and_even_counts() {
        assert_eq!(median(&[]), 0.0);
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 3.0, 2.0]), 2.5);
        assert_eq!(median(&[5.0]), 5.0);
    }

    #[test]
    fn group_ranges_tile_the_samples() {
        for (samples, groups) in [(16u32, 1u32), (16, 4), (17, 4), (5, 5), (64, 7)] {
            let mut next = 0;
            for g in 0..groups {
                let (a, b) = group_range(samples, groups, g);
                assert_eq!(a, next, "{samples}/{groups} group {g}");
                assert!(b > a, "empty group {g} of {samples}/{groups}");
                next = b;
            }
            assert_eq!(next, samples);
        }
        assert_eq!(group_range(17, 4, 0), (0, 5));
        assert_eq!(group_range(17, 4, 3), (13, 17));
    }

    /// One group, or suppression off, is the plain sum: bit for bit for one group, so a bake
    /// without groups is unchanged.
    #[test]
    fn one_group_is_the_plain_sum() {
        let mut g = grey(10.0, 64);
        g.sh[0] = [-0.0, 1.5, -2.25, 3.0e-7];
        let c = combine_groups(&[g], 4.0);
        assert_eq!(c.sh, g.sh);
        assert_eq!(c.sh[0][0].to_bits(), (-0.0f32).to_bits());
        assert_eq!((c.rays, c.suppressed, c.removed, c.noise), (64.0, 0, 0.0, None));
        let groups = [grey(10.0, 64), grey(1000.0, 64), grey(11.0, 64)];
        let off = combine_groups(&groups, 0.0);
        assert_eq!(off.suppressed, 0);
        assert!((mean(&off) - 1021.0 / 192.0).abs() < 1e-3, "{}", mean(&off));
    }

    /// A firefly group is scaled to `k ×` the median group; the others are untouched, the loss is
    /// reported, colour ratios and L1 directions of the scaled group are kept.
    #[test]
    fn a_firefly_group_is_scaled_to_the_threshold() {
        let mut groups: Vec<GroupSums> = [1.0, 1.1, 0.9, 1.0, 1.05]
            .iter()
            .map(|v| grey(v * 64.0, 64))
            .collect();
        groups[2] = grey(1000.0 * 64.0, 64);
        groups[2].sh[4][2] = 50.0;
        let c = combine_groups(&groups, 4.0);
        assert_eq!(c.suppressed, 1);
        // Median of (1, 1.1, 1000, 1, 1.05) is 1.05: the firefly group is capped at 4.2.
        let want = (1.0 + 1.1 + 4.2 + 1.0 + 1.05) / 5.0;
        assert!((mean(&c) - want).abs() < 1e-4, "{} want {want}", mean(&c));
        let before = (1.0 + 1.1 + 1000.0 + 1.0 + 1.05) / 5.0;
        assert!(
            (f64::from(c.removed) - (before - want) / before).abs() < 1e-4,
            "{}",
            c.removed
        );
        let k = 4.2 / 1000.0;
        assert!((c.sh[4][2] - (50.0 * k) as f32).abs() < 1e-4, "{:?}", c.sh[4]);
        // Uniform groups: nothing suppressed, zero noise.
        let even = vec![grey(64.0, 64); 6];
        let c = combine_groups(&even, 4.0);
        assert_eq!((c.suppressed, c.removed), (0, 0.0));
        assert_eq!(c.noise, Some(0.0));
    }

    /// Most groups black: the median is 0 and the probe is left alone (a rare light and a firefly
    /// look the same); empty groups do not count; two groups never suppress.
    #[test]
    fn sparse_probes_and_too_few_groups_are_left_alone() {
        let groups = [grey(0.0, 64), grey(0.0, 64), grey(500.0, 64), grey(0.0, 64)];
        let c = combine_groups(&groups, 4.0);
        assert_eq!(c.suppressed, 0);
        assert!((mean(&c) - 500.0 / 256.0).abs() < 1e-4);
        let two = [grey(64.0, 64), grey(64_000.0, 64)];
        assert_eq!(combine_groups(&two, 1.5).suppressed, 0);
        // A group whose rays were all rejected is neither a sample of the median nor of the noise.
        let holes = [
            grey(64.0, 64),
            GroupSums {
                sh: [[0.0; 4]; SH_ROWS],
                rays: 0.0,
            },
            grey(64.0, 64),
            grey(64.0, 64),
        ];
        let c = combine_groups(&holes, 4.0);
        assert_eq!((c.suppressed, c.rays), (0, 192.0));
        assert_eq!(c.noise, Some(0.0));
        assert_eq!(combine_groups(&[], 4.0).rays, 0.0);
    }

    /// Convergence of the suppressed estimator on a heavy-tailed integrand: a rare bright path
    /// (probability 1/1000, value 500, a third of the answer) over a unit background. At low
    /// sample counts suppression cuts the error against the plain mean (and is biased low); as
    /// the count grows it suppresses less and less, its bias vanishes, and at a million rays it
    /// suppresses nothing and lands on the answer.
    #[test]
    fn firefly_suppression_converges_to_the_mean() {
        let draw = |r: &mut Rng| if r.next() < 1e-3 { 500.0 } else { 1.0 };
        let truth = 0.999 + 0.5;
        let mut rng = Rng(7);
        let (mut bias, mut suppressed) = (Vec::new(), Vec::new());
        for rays in [64u32, 1024, 16_384] {
            let trials = 100;
            let (mut se_plain, mut se_robust, mut sum_robust, mut groups_cut) =
                (0.0f64, 0.0f64, 0.0f64, 0u32);
            for _ in 0..trials {
                let groups = simulate(&mut rng, 8, rays, &draw);
                let plain = mean(&combine_groups(&groups, 0.0));
                let c = combine_groups(&groups, 4.0);
                let robust = mean(&c);
                se_plain += (plain - truth).powi(2);
                se_robust += (robust - truth).powi(2);
                sum_robust += robust;
                groups_cut += c.suppressed;
            }
            let rmse_plain = (se_plain / f64::from(trials)).sqrt();
            let rmse_robust = (se_robust / f64::from(trials)).sqrt();
            let b = (sum_robust / f64::from(trials) - truth).abs() / truth;
            eprintln!(
                "rays/group {rays}: RMSE plain {rmse_plain:.4} robust {rmse_robust:.4}, robust bias {b:.4}, \
                 groups suppressed {groups_cut}"
            );
            if rays == 64 {
                assert!(rmse_robust < 0.8 * rmse_plain, "{rmse_robust} vs {rmse_plain}");
                assert!(b > 0.05, "a low sample count is biased low: {b}");
            }
            bias.push(b);
            suppressed.push(groups_cut);
        }
        assert!(
            bias[1] < 0.5 * bias[0] && bias[2] < 0.03,
            "bias must fall with samples: {bias:?}"
        );
        assert!(
            suppressed[1] < suppressed[0] && suppressed[2] <= suppressed[1],
            "{suppressed:?}"
        );
        let groups = simulate(&mut rng, 8, 1 << 17, &draw);
        let c = combine_groups(&groups, 4.0);
        assert_eq!(c.suppressed, 0);
        // Four standard errors of a million draws of this integrand (σ ≈ 15.8 per draw).
        assert!((mean(&c) - truth).abs() / truth < 0.045, "{}", mean(&c));
    }

    /// The noise estimate tracks the real standard error of the mean and falls as `1/√N`.
    #[test]
    fn the_noise_estimate_falls_as_one_over_root_n() {
        // Uniform on [0, 2]: mean 1, standard deviation 1/√3 per ray.
        let draw = |r: &mut Rng| 2.0 * r.next();
        let mut rng = Rng(11);
        let mut estimates = Vec::new();
        for rays in [256u32, 1024, 4096] {
            let trials = 64;
            let (mut est, mut err2) = (0.0f64, 0.0f64);
            for _ in 0..trials {
                let c = combine_groups(&simulate(&mut rng, 8, rays, &draw), 0.0);
                est += f64::from(c.noise.expect("noise"));
                err2 += (mean(&c) - 1.0).powi(2);
            }
            let est = est / trials as f64;
            let real = (err2 / trials as f64).sqrt();
            let model = (1.0 / 3.0f64).sqrt() / f64::from(8 * rays).sqrt();
            eprintln!("rays/group {rays}: estimated {est:.5}, measured {real:.5}, model {model:.5}");
            assert!((est / model - 1.0).abs() < 0.15, "{est} vs {model}");
            assert!((real / model - 1.0).abs() < 0.3, "{real} vs {model}");
            estimates.push(est);
        }
        for w in estimates.windows(2) {
            assert!((w[0] / w[1] - 2.0).abs() < 0.3, "{estimates:?}");
        }
    }

    #[test]
    fn bounce_bounds_follow_the_geometric_series() {
        let (lo, hi) = bounce_energy_bounds(0.5, 3);
        assert!(
            (lo - 0.875).abs() < 1e-6 && (hi - 0.9375).abs() < 1e-6,
            "{lo} {hi}"
        );
        assert_eq!(bounce_energy_bounds(0.0, 1), (1.0, 1.0));
        assert_eq!(bounce_energy_bounds(1.0, 8), (0.0, 0.0));
        // More bounces never keep less.
        for a in [0.2f32, 0.5, 0.8, 0.95] {
            let mut last = 0.0;
            for b in 1..=64 {
                let (lo, hi) = bounce_energy_bounds(a, b);
                assert!(lo >= last && hi >= lo, "{a} {b}");
                last = lo;
            }
        }
        assert_eq!(bounces_for(0.5, 0.01), 7);
        assert_eq!(bounces_for(0.8, 0.01), 21);
        assert_eq!(bounces_for(0.3, 0.01), 4);
        assert_eq!(bounces_for(0.0, 0.01), 1);
        assert_eq!(bounces_for(0.999, 0.01), 64);
    }

    #[test]
    fn the_bounce_budget_reads_the_traced_materials() {
        let (scene, _) = crate::scenes::closed_sphere(0.6, 1.0, 0.5, 4);
        let b = bounce_budget(&scene, 8);
        assert!((b.max_albedo - 0.6).abs() < 1e-5, "{b:?}");
        assert!((b.missing_bound - 0.6f32.powi(8)).abs() < 1e-6, "{b:?}");
        assert_eq!(b.bounces_for_1_percent, 10);
        // Unused materials do not count; an empty scene has nothing to bounce.
        let mut empty = PtScene::new(scene.camera);
        empty.add_material(crate::PtMaterial::lambert([0.9; 3]));
        assert_eq!(bounce_budget(&empty, 8).max_albedo, 0.0);
    }
}
