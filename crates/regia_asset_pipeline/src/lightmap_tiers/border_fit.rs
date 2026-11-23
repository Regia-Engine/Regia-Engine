// SPDX-License-Identifier: MPL-2.0
//! Chart-border fit of a derived tier (ADR-4995).
//!
//! The bake stitched its seams (ADR-2404): along a smooth cut, the two sides' bilinear lookups
//! read the same light. Area means at a coarser tier break that, because a tier texel next to a
//! border averages light from further inside its chart, and the gutter between two charts may be
//! a single texel that holds only one side's continuation. The fit restores the bake's border
//! light without the mesh: along every chart border (an unwrap edge only one triangle uses), at
//! `k + 0.5` of `n` points (`n` twice the edge's length in tier texels, 2 to [`MAX_EDGE_SAMPLES`]),
//! the tier's bilinear lookup (the runtime's, as `Lightmap::sample_page`) should read what the
//! bake's lookup reads at the same entry-local UV. Both sides of a stitched seam then read the
//! same light, because the bake's sides did; a crease keeps its step, because each side matches
//! its own side of the bake.
//!
//! **Ownership.** A gutter texel holds one chart's light (module docs of `lightmap_tiers`). A texel
//! that rows of two charts read is shared only where the bake already agrees there: the
//! row-weighted mean targets of every chart reading it must lie within [`SHARE_TOLERANCE`] of each
//! other. Otherwise (a crease or a cut the bake did not stitch) the texel is frozen at its value
//! and the rows of other charts that read it are dropped. Rows whose lookup would clamp at the
//! atlas edge in the tier or the bake are dropped too (the two clamp differently), and so are
//! rows where the bake's light is not close to linear over the first two tier texels inward (its
//! second difference over the border, one and two texels in exceeds [`SHARE_TOLERANCE`] of their
//! mean: a contact band at a joint, a shadow edge on the border): bilinear tier texels hold a
//! gradient exactly but not a band narrower than themselves, and matching the border value there
//! widens the band.
//! On such borders the texels on the chart (BAKED, centre on its surface) keep their area means
//! and are held, so no neighbouring row moves them either; the gutter texels the lookup reads
//! (centres off the chart) are still fitted to the bake's border light (ADR-5125). A gutter texel
//! only shapes the lookups between the border and the first texel centre inside the chart, so it
//! cannot widen the band inward, and without it a chart a few tier texels across (a pillar's strip at
//! ULTRA_LOW) reads its area mean at the border, not the bake's border light.
//!
//! **Solve.** As the baker's stitch (Liu, Ferguson, Jacobson and Gingold, *Seamless*, SIGGRAPH Asia
//! 2017, § 4, inhomogeneous rows): minimise `Σ_rows (g · x − t)² + λ Σ_texels (x − x₀)²` over the
//! texels the rows read, `x₀` the area-mean (or gutter) value, `λ` = [`FIT_REGULARIZATION`], per channel by
//! conjugate gradients with a Jacobi preconditioner. Irradiance only; directions keep their
//! luminance-weighted means. Each result is clamped to [`FIT_BOUND`] times its area-mean value,
//! the bound the baker's own gutter extrapolation uses: the fit corrects a texel, it never gives
//! it a new level (a lit texel cannot be fitted dark). A gutter texel (centre off its chart) is
//! bounded by [`FIT_BOUND`] times the range between its fill and the bake's border light its rows
//! ask for (ADR-5125): its fill extrapolates the chart's inside, which on a chart a few tier texels
//! across can be a third of the light at the border. Sequential, fixed order: deterministic.

use std::collections::BTreeMap;

use regia_asset::lightmap::{Coverage, Lightmap, LightmapEntry, MeshUnwrap};

/// Weight of a texel's area-mean value against the border rows. Smaller than the baker's stitch
/// weight (0.05): a gutter corner is read by few rows with small bilinear weights, and a larger
/// pull toward its fill value leaves a residual that leaks into the border lookups beside it.
const FIT_REGULARIZATION: f64 = 1e-3;
/// Range of a fitted texel, as multiples of its area-mean value (the baker's gutter bound).
const FIT_BOUND: [f64; 2] = [0.5, 1.5];
/// Most sample points along one border edge.
const MAX_EDGE_SAMPLES: usize = 64;
/// Most conjugate-gradient iterations per channel.
const MAX_CG_ITERATIONS: usize = 256;
/// Relative residual the solve stops at.
const CG_TOLERANCE: f64 = 1e-7;
/// Largest relative difference between two charts' targets at a texel they both read for it to
/// be shared (the shipping seam budget, `regia_asset::lightmap::DEFAULT_SEAM_BUDGET`).
const SHARE_TOLERANCE: f64 = 0.25;

/// What the fit did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct BorderFitStats {
    /// Border sample rows solved.
    pub rows: u64,
    /// Texels the solve changed.
    pub texels: u64,
    /// Texels kept as they were: those two charts read with disagreeing bake light, and the
    /// on-chart texels a steep border sample reads (held, ADR-5621). Each counted once.
    pub frozen: u64,
    /// Border samples where the bake's light is not close to linear over the two tier texels
    /// inward of them: their texels on the chart are held, only gutter texels are fitted.
    pub steep: u64,
}

/// The tier atlas the fit works on: page-major irradiance, coverage and the global chart of each
/// texel (`u32::MAX` for none).
pub(super) struct TierTexels<'a> {
    /// Irradiance per texel.
    pub irr: &'a mut [[f32; 3]],
    /// Coverage per texel.
    pub cov: &'a [u8],
    /// Global chart per texel.
    pub owner: &'a [u32],
    /// Atlas width.
    pub width: u32,
    /// Atlas page height.
    pub height: u32,
}

/// One border sample: its chart, target light and the tier taps it reads.
struct Row {
    chart: u32,
    target: [f64; 3],
    taps: Vec<(usize, f64)>,
}

/// Bilinear taps at atlas `uv` of a `w × h` page `page` (texel centres at `k + 0.5`): non-empty
/// texels, weights renormalised. `None` when a weighted tap would clamp at the page edge or
/// every tap is empty.
fn taps(
    uv: [f32; 2],
    page: u32,
    w: u32,
    h: u32,
    covered: impl Fn(usize) -> bool,
) -> Option<Vec<(usize, f64)>> {
    let fx = f64::from(uv[0]) * f64::from(w) - 0.5;
    let fy = f64::from(uv[1]) * f64::from(h) - 0.5;
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let mut out = Vec::with_capacity(4);
    let mut sum = 0.0;
    for (dx, dy, wt) in [
        (0.0, 0.0, (1.0 - tx) * (1.0 - ty)),
        (1.0, 0.0, tx * (1.0 - ty)),
        (0.0, 1.0, (1.0 - tx) * ty),
        (1.0, 1.0, tx * ty),
    ] {
        if wt <= 0.0 {
            continue;
        }
        let (x, y) = (x0 + dx, y0 + dy);
        if x < 0.0 || y < 0.0 || x >= f64::from(w) || y >= f64::from(h) {
            return None;
        }
        let k = (page as usize * h as usize + y as usize) * w as usize + x as usize;
        if covered(k) {
            out.push((k, wt));
            sum += wt;
        }
    }
    (sum > 0.0).then(|| out.into_iter().map(|(k, wt)| (k, wt / sum)).collect())
}

/// The border edges of one unwrap (edges only one triangle uses, by unwrapped vertex): triangle,
/// the edge's two vertices and the triangle's third, in triangle order.
fn border_edges(unwrap: &MeshUnwrap) -> Vec<(usize, u32, u32, u32)> {
    let tris = unwrap.indices.as_chunks::<3>().0;
    let mut uses: BTreeMap<(u32, u32), u32> = BTreeMap::new();
    for t in tris {
        for c in 0..3 {
            let (a, b) = (t[c], t[(c + 1) % 3]);
            *uses.entry((a.min(b), a.max(b))).or_insert(0) += 1;
        }
    }
    let mut out = Vec::new();
    for (k, t) in tris.iter().enumerate() {
        for c in 0..3 {
            let (a, b) = (t[c], t[(c + 1) % 3]);
            if uses.get(&(a.min(b), a.max(b))) == Some(&1) {
                out.push((k, a, b, t[(c + 2) % 3]));
            }
        }
    }
    out
}

/// Fits the tier's border texels to the bake's border light (module docs). `entries` are the
/// tier's entries (rectangles and scale-offsets at the tier) and `unwraps` its unwraps (the bake's,
/// or charts repacked at the tier, ADR-5125: the same vertices, so a border point is the same
/// edge parameter in both), `native[e]` entry `e`'s size at the tier with the bake's layout, and
/// `tri_charts[e]` the global chart of each of its triangles.
pub(super) fn fit_borders(
    src: &Lightmap,
    entries: &[LightmapEntry],
    unwraps: &[MeshUnwrap],
    native: &[[u32; 2]],
    tri_charts: &[Vec<u32>],
    tier: &mut TierTexels<'_>,
) -> BorderFitStats {
    let _span = tracing::debug_span!("lightmap_tier_border_fit", entries = entries.len()).entered();
    let mut stats = BorderFitStats::default();
    let mut rows: Vec<Row> = Vec::new();
    // Texels a steep border sample reads: held at their values so no other row moves them.
    let mut held: Vec<usize> = Vec::new();
    let tier_cov = tier.cov;
    let lum = |c: [f64; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    for (e, (entry, unwrap)) in entries.iter().zip(unwraps).enumerate() {
        let (Some(src_entry), Some(src_unwrap), Some(&[nw, nh])) =
            (src.entries.get(e), src.unwraps.get(e), native.get(e))
        else {
            continue;
        };
        let [_, _, tw, th] = entry.rect;
        let so = entry.scale_offset;
        let ss = src_entry.scale_offset;
        for (k, a, b, c) in border_edges(unwrap) {
            let (Some(pa), Some(pb), Some(qa), Some(qb), Some(qc)) = (
                unwrap.uv.get(a as usize),
                unwrap.uv.get(b as usize),
                src_unwrap.uv.get(a as usize),
                src_unwrap.uv.get(b as usize),
                src_unwrap.uv.get(c as usize),
            ) else {
                continue;
            };
            let Some(&chart) = tri_charts.get(e).and_then(|c| c.get(k)) else {
                continue;
            };
            let len = ((pb[0] - pa[0]) * tw as f32).hypot((pb[1] - pa[1]) * th as f32);
            let n = ((2.0 * len).ceil() as usize).clamp(2, MAX_EDGE_SAMPLES);
            for s in 0..n {
                let f = (s as f32 + 0.5) / n as f32;
                // The same point of the edge in the tier's unwrap and in the bake's.
                let p = [pa[0] + (pb[0] - pa[0]) * f, pa[1] + (pb[1] - pa[1]) * f];
                let q = [qa[0] + (qb[0] - qa[0]) * f, qa[1] + (qb[1] - qa[1]) * f];
                let src_uv = [q[0] * ss[0] + ss[2], q[1] * ss[1] + ss[3]];
                // The bake's lookup must not clamp either (its value at the page edge is not the
                // light a clamped tier lookup could match), and must read a texel: an all-empty
                // lookup's 0 is no light the bake measured.
                if taps(src_uv, src_entry.page, src.width, src.height, |i| {
                    src.coverage.get(i).is_some_and(|&c| c != Coverage::EMPTY)
                })
                .is_none()
                {
                    continue;
                }
                let uv = [p[0] * so[0] + so[2], p[1] * so[1] + so[3]];
                let Some(t) = taps(uv, entry.page, tier.width, tier.height, |i| {
                    tier_cov.get(i).is_some_and(|&c| c != Coverage::EMPTY)
                }) else {
                    continue;
                };
                let target = src.sample_entry(e, q).map(f64::from);
                // One and two tier texels inward, toward the triangle's third vertex: where the
                // bake's light is not close to linear there (a contact band at a joint, a shadow
                // edge on the border), the tier's bilinear texels cannot hold it, and matching it at
                // the border only widens the band. A linear gradient they hold exactly. Measured in
                // the bake's unwrap at the tier's scale (a repack only moves a chart by whole texels).
                let inward = [(qc[0] - q[0]) * nw as f32, (qc[1] - q[1]) * nh as f32];
                let reach = inward[0].hypot(inward[1]);
                if reach <= 1e-6 {
                    continue;
                }
                let step = |d: f32| {
                    [
                        q[0] + d * inward[0] / reach / nw as f32,
                        q[1] + d * inward[1] / reach / nh as f32,
                    ]
                };
                let at = |q: [f32; 2]| lum(src.sample_entry(e, q).map(f64::from));
                let (l0, l1, l2) = (lum(target), at(step(1.0)), at(step(2.0)));
                let mean = ((l0 + l1 + l2) / 3.0).max(1e-9);
                if (l0 - 2.0 * l1 + l2).abs() > SHARE_TOLERANCE * mean {
                    stats.steep += 1;
                    // The texels on the chart (BAKED: centre on its surface) keep their area means,
                    // so the band is not widened inward. A gutter texel's centre is off the chart:
                    // it only shapes lookups between the border and the first texel centre inside,
                    // so it is fitted to the border light (ADR-5125). A row with no gutter tap
                    // has nothing left to fit.
                    let on_chart = |k: usize| tier_cov.get(k) == Some(&Coverage::BAKED);
                    held.extend(t.iter().filter(|&&(k, _)| on_chart(k)).map(|&(k, _)| k));
                    if t.iter().all(|&(k, _)| on_chart(k)) {
                        continue;
                    }
                }
                rows.push(Row {
                    chart,
                    target,
                    taps: t,
                });
            }
        }
    }
    // Which charts read each texel, with their row-weighted mean target luminance.
    let mut readers: BTreeMap<usize, BTreeMap<u32, (f64, f64)>> = BTreeMap::new();
    for row in &rows {
        let l = lum(row.target);
        for &(k, w) in &row.taps {
            let slot = readers
                .entry(k)
                .or_default()
                .entry(row.chart)
                .or_insert((0.0, 0.0));
            slot.0 += w * l;
            slot.1 += w;
        }
    }
    // A held texel is frozen as a disagreeing one is (other charts' rows that read it are dropped
    // below), so it is counted with them: `frozen` is every texel the fit left as it was (ADR-5621).
    let mut frozen = vec![false; tier.irr.len()];
    for &k in &held {
        if let Some(slot) = frozen.get_mut(k)
            && !*slot
        {
            *slot = true;
            stats.frozen += 1;
        }
    }
    for (&k, charts) in &readers {
        if charts.len() < 2 || frozen[k] {
            continue;
        }
        let means: Vec<f64> = charts.values().map(|(s, w)| s / w.max(1e-12)).collect();
        let (lo, hi) = means
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), &m| {
                (lo.min(m), hi.max(m))
            });
        if hi - lo > SHARE_TOLERANCE * (0.5 * (hi + lo)).max(1e-9) {
            frozen[k] = true;
            stats.frozen += 1;
        }
    }
    // A row that reads another chart's frozen texel cannot be met without blending: drop it.
    rows.retain(|row| {
        row.taps
            .iter()
            .all(|&(k, _)| !frozen[k] || tier.owner.get(k) == Some(&row.chart))
    });
    stats.rows = rows.len() as u64;
    if rows.is_empty() {
        return stats;
    }
    // Unknowns: every non-frozen texel a row reads, numbered as first read.
    let mut index: BTreeMap<usize, usize> = BTreeMap::new();
    let mut texels: Vec<usize> = Vec::new();
    let mut row_taps: Vec<Vec<(usize, f64)>> = Vec::with_capacity(rows.len());
    let mut row_rhs: Vec<[f64; 3]> = Vec::with_capacity(rows.len());
    // Per unknown: the weight-summed bake target of the rows that read it, and their weight.
    let mut served: Vec<([f64; 3], f64)> = Vec::new();
    for row in &rows {
        let mut unknown = Vec::with_capacity(row.taps.len());
        let mut rhs = row.target;
        for &(k, w) in &row.taps {
            if frozen[k] {
                for (r, v) in rhs.iter_mut().zip(tier.irr[k]) {
                    *r -= w * f64::from(v);
                }
            } else {
                let next = texels.len();
                let u = *index.entry(k).or_insert_with(|| {
                    texels.push(k);
                    served.push(([0.0; 3], 0.0));
                    next
                });
                let (sum, weight) = &mut served[u];
                for (s, t) in sum.iter_mut().zip(row.target) {
                    *s += w * t;
                }
                *weight += w;
                unknown.push((u, w));
            }
        }
        row_taps.push(unknown);
        row_rhs.push(rhs);
    }
    let m = texels.len();
    let mut diag = vec![FIT_REGULARIZATION; m];
    for t in &row_taps {
        for &(u, w) in t {
            diag[u] += w * w;
        }
    }
    let apply = |x: &[f64], out: &mut [f64]| {
        for (o, xi) in out.iter_mut().zip(x) {
            *o = FIT_REGULARIZATION * xi;
        }
        for t in &row_taps {
            let g: f64 = t.iter().map(|&(u, w)| w * x[u]).sum();
            for &(u, w) in t {
                out[u] += w * g;
            }
        }
    };
    let mut changed = vec![false; m];
    for c in 0..3 {
        let x0: Vec<f64> = texels.iter().map(|&k| f64::from(tier.irr[k][c])).collect();
        let mut b: Vec<f64> = x0.iter().map(|v| FIT_REGULARIZATION * v).collect();
        for (t, rhs) in row_taps.iter().zip(&row_rhs) {
            for &(u, w) in t {
                b[u] += w * rhs[c];
            }
        }
        let x = conjugate_gradients(&apply, &b, &x0, &diag);
        for ((((&k, v), x0), flag), &(sum, weight)) in
            texels.iter().zip(x).zip(&x0).zip(changed.iter_mut()).zip(&served)
        {
            // Bounded as the baker's gutter extrapolation (ADR-4930): a correction, never a new level.
            // A texel on the chart is bounded by its own area mean. A gutter texel exists only to
            // carry the bake's border light to the lookups beside it, so its range also takes in
            // that light (the rows' weighted target, ADR-5125): a level the bake has there, never a
            // new one. A non-finite value is left as it was (`clamp` needs ordered bounds).
            let (lo, hi) = if tier.cov.get(k) == Some(&Coverage::BAKED) || weight <= 0.0 {
                (*x0, *x0)
            } else {
                let t = sum[c] / weight;
                (x0.min(t), x0.max(t))
            };
            let v = if lo.is_finite() && hi.is_finite() && v.is_finite() {
                v.clamp(FIT_BOUND[0] * lo.max(0.0), FIT_BOUND[1] * hi.max(0.0)) as f32
            } else {
                *x0 as f32
            };
            if v != tier.irr[k][c] {
                *flag = true;
            }
            tier.irr[k][c] = v;
        }
    }
    stats.texels = changed.iter().filter(|&&c| c).count() as u64;
    stats
}

/// Jacobi-preconditioned conjugate gradients on `apply(x) = b` from `x0`.
fn conjugate_gradients(apply: &impl Fn(&[f64], &mut [f64]), b: &[f64], x0: &[f64], diag: &[f64]) -> Vec<f64> {
    let n = b.len();
    let mut x = x0.to_vec();
    let mut ax = vec![0.0; n];
    apply(&x, &mut ax);
    let mut r: Vec<f64> = b.iter().zip(&ax).map(|(b, a)| b - a).collect();
    let norm_b = b.iter().map(|v| v * v).sum::<f64>().sqrt().max(1e-30);
    let mut z: Vec<f64> = r.iter().zip(diag).map(|(r, d)| r / d).collect();
    let mut p = z.clone();
    let mut rz: f64 = r.iter().zip(&z).map(|(a, b)| a * b).sum();
    let mut ap = vec![0.0; n];
    for _ in 0..MAX_CG_ITERATIONS {
        if r.iter().map(|v| v * v).sum::<f64>().sqrt() <= CG_TOLERANCE * norm_b {
            break;
        }
        apply(&p, &mut ap);
        let pap: f64 = p.iter().zip(&ap).map(|(a, b)| a * b).sum();
        if pap <= 0.0 {
            break;
        }
        let alpha = rz / pap;
        for (xi, pi) in x.iter_mut().zip(&p) {
            *xi += alpha * pi;
        }
        for (ri, api) in r.iter_mut().zip(&ap) {
            *ri -= alpha * api;
        }
        for ((zi, ri), di) in z.iter_mut().zip(&r).zip(diag) {
            *zi = ri / di;
        }
        let rz_next: f64 = r.iter().zip(&z).map(|(a, b)| a * b).sum();
        let beta = rz_next / rz.max(1e-300);
        rz = rz_next;
        for (pi, zi) in p.iter_mut().zip(&z) {
            *pi = zi + beta * *pi;
        }
    }
    x
}
