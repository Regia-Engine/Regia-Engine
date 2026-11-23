// SPDX-License-Identifier: MPL-2.0
//! The lightmap denoiser (ADR-2314): an edge-avoiding à-trous wavelet filter over the baked
//! texels of each chart (Dammertz, Sewtz, Hanika and Lensch, *Edge-Avoiding À-Trous Wavelet
//! Transform for fast Global Illumination Filtering*, HPG 2010), with the luminance weight scaled
//! by each texel's own measured noise and that noise carried through the passes (Schied et al.,
//! *Spatiotemporal Variance-Guided Filtering*, HPG 2017, § 4.4 and § 4.5; the spatial part only).
//!
//! Pass `i` (of `iterations`) visits the 5×5 taps `p + 2^i · (dx, dy)` with the B3-spline weights
//! `h = [1, 4, 6, 4, 1] / 16` per axis. A tap counts when it is a baked texel of the **same
//! chart**; a tap that falls off the chart (when the texel has world axes) is replaced by the baked
//! texel of another chart or entry that continues the surface at the tap's world point: within
//! 0.75 texel of it and with a normal within 30° (`ACROSS_REACH`, `ACROSS_MIN_COS`), and with no
//! geometry on the straight path between the two texels lifted off their surfaces (`across_clear`),
//! so light crosses a smooth chart border but not a hard edge, and not a wall: a sealed room's
//! floor does not read the sunlit ground outside it, which lies parallel to it a texel away beyond
//! the wall. Each tap weighs
//!
//! - `max(0, n_p · n_q)^normal_power` (the normals),
//! - `exp(−(excess / (sigma_position · t))²)`, where `t` is the texel's world size and `excess`
//!   the world distance between the two texels beyond `1.5 ×` what their UV offset explains (a
//!   fold or a gap inside a chart),
//! - `exp(−|L_p − L_q| / (sigma_luminance · √(σ²_p + σ²_q) + ε))`, with `σ²` the variance of each
//!   texel's luminance estimate (from the bake's per-texel standard error; 10 % of the luminance
//!   when a texel took fewer than two samples).
//!
//! A pass's result is the weighted mean of its taps, moved toward the value at the texel of a plane
//! `a + b · (dx, dy)` fitted to the same taps by weighted least squares (a first-order, locally
//! linear estimate; Cleveland 1979's local regression, as Moon, Carr and Yoon 2014 and Bitterli et
//! al. 2016 use it for rendering; ADR-4108). With the taps balanced around the texel the two agree;
//! where a chart border, an opening or a light edge cuts the kernel off on one side, the mean is
//! pulled toward the light inside the chart by the gradient times the kernel's offset (a step
//! between two charts of one flat wall), and the fit is not. The fit pays for that with variance
//! where it extrapolates (a texel on the border reads taps on one side only), so each texel takes
//! `mean + alpha · (fit − mean)`, `alpha = b² / (b² + Δσ²)`: `Δσ²` the variance the fit adds over
//! the mean (from the carried per-texel variances and the two sets of weights), `b²` the squared
//! difference of the two results less nine times its own variance (the part that stands three
//! standard errors out of the noise). A gradient that stands out of the noise is followed and noise
//! is not extrapolated (a shrinkage estimator in the James-Stein sense). A result that extrapolates
//! an irradiance below zero falls back to the mean.
//!
//! Every channel of a texel (irradiance and, for a directional bake, its direction) is filtered
//! with the same weights. Texels that are not baked are neither read nor written; the gutters are
//! filled afterwards, from the denoised texels. Deterministic: a fixed visiting order, no random
//! numbers, and each pass reads only the previous one.

use std::collections::HashMap;

use regia_asset::lightmap::{Coverage, LightmapDenoiseInfo};
use regia_core::glam::Vec3;

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
thread_local! {
    static COUNT_RESULT_VECS: Cell<bool> = const { Cell::new(false) };
    static RESULT_VECS: Cell<u32> = const { Cell::new(0) };
    static COUNT_ACROSS_VISITS: Cell<bool> = const { Cell::new(false) };
    static ACROSS_VISITS: Cell<u32> = const { Cell::new(0) };
}

#[cfg(test)]
fn note_result_vec() {
    if COUNT_RESULT_VECS.get() {
        RESULT_VECS.set(RESULT_VECS.get() + 1);
    }
}

#[cfg(test)]
fn note_across_visit() {
    if COUNT_ACROSS_VISITS.get() {
        ACROSS_VISITS.set(ACROSS_VISITS.get() + 1);
    }
}

/// B3-spline taps of one axis.
const H: [f32; 5] = [1.0 / 16.0, 4.0 / 16.0, 6.0 / 16.0, 4.0 / 16.0, 1.0 / 16.0];
/// Relative noise assumed for a texel without a measured error.
const FALLBACK_REL_NOISE: f32 = 0.1;

/// Per-texel guides of the denoiser (all atlas-sized).
pub(crate) struct DenoiseGuides<'a> {
    /// Coverage of each texel (only [`Coverage::BAKED`] texels are filtered).
    pub coverage: &'a [u8],
    /// Shading normal at the texel.
    pub normals: &'a [Vec3],
    /// World position the texel was baked at.
    pub positions: &'a [Vec3],
    /// The chart (unique across entries) the texel belongs to; `u32::MAX` for none.
    pub chart_of: &'a [u32],
    /// World size of one texel, metres.
    pub texel_size: &'a [f32],
    /// Relative standard error of the texel's luminance (NaN when unknown).
    pub rel_err: &'a [f32],
    /// World offset of one atlas texel step in x and in y on the texel's triangle (its UV2
    /// Jacobian; zero when unknown, which keeps the texel inside its chart).
    pub axes: &'a [[Vec3; 2]],
    /// Whether the straight segment between two world points crosses no geometry of the bake's
    /// scene. A tap across a chart border reads the other texel only when it is clear
    /// (`across_clear`); `None` (a fixture without a scene) skips the check.
    pub clear: Option<&'a (dyn Fn(Vec3, Vec3) -> bool + Sync)>,
    /// Worker threads (at least 1; the result does not depend on it).
    pub threads: usize,
    /// Atlas width.
    pub w: usize,
    /// Atlas height.
    pub h: usize,
}

/// Luminance (Rec. 709) of a texel's first three channels.
fn lum<const N: usize>(c: &[f32; N]) -> f32 {
    0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2]
}

/// Smallest cosine between two texels' normals for a tap across a chart border: the same surface
/// continuing, not a crease (the seam metric's 30° limit).
const ACROSS_MIN_COS: f32 = 0.866;
/// How far from the tap's world point a texel across a border may lie, in texels of the texel's
/// own chart.
const ACROSS_REACH: f32 = 0.75;
/// How far both ends of an across tap's visibility segment are lifted off their surfaces along
/// their normals, in texels of the texel's own chart, before the curvature term: clear of the two
/// surfaces' own triangles and of the shared edge of coplanar neighbours.
const ACROSS_LIFT: f32 = 0.1;

/// The texel `j` a tap of texel `i` found across a chart border is visible from `i`: nothing lies
/// on the straight segment between them, each end lifted off its surface along its normal. The lift
/// is `ACROSS_LIFT` texels plus `0.75 · length · tan(θ / 2)`, `θ` the angle between the normals:
/// two facets that turn by `θ` put their ridge up to `(length / 2) · tan(θ / 2)` above the chord, so
/// a convex continuation stays visible. A wall between the two (a sealed room's floor and the
/// ground outside it) blocks the tap.
fn across_clear(g: &DenoiseGuides<'_>, i: usize, j: usize, ts: f32) -> bool {
    let Some(clear) = g.clear else {
        return true;
    };
    let (xp, np) = (g.positions[i], g.normals[i]);
    let (xq, nq) = (g.positions[j], g.normals[j]);
    let cos = np.dot(nq).clamp(-1.0, 1.0);
    let half_tan = ((1.0 - cos) / (1.0 + cos).max(1e-6)).max(0.0).sqrt();
    let lift = ACROSS_LIFT * ts + 0.75 * xp.distance(xq) * half_tan;
    clear(xp + np * lift, xq + nq * lift)
}

#[cfg(test)]
thread_local! {
    /// Texels `WorldGrid::across` has examined on this thread.
    static ACROSS_CANDIDATES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Baked texels by world cell, for taps that leave their chart.
///
/// One uniform grid per texel-size class (sizes within a factor of 2), each with cells as large
/// as its largest texel. One grid sized by the largest texel of the bake put every texel of a
/// fine interior into a few cells next to one coarse entry, so each off-chart tap scanned them all.
struct WorldGrid {
    classes: Vec<GridClass>,
}

/// The texels of one size class, by world cell.
struct GridClass {
    cell: f32,
    cells: HashMap<[i32; 3], Vec<u32>>,
}

impl WorldGrid {
    fn new(g: &DenoiseGuides<'_>, baked: &[usize]) -> Self {
        let size = |i: usize| {
            let s = g.texel_size[i];
            if s.is_finite() { s.max(1e-4) } else { 1e-4 }
        };
        // Classes by octave of the texel size, in ascending order (deterministic).
        let mut by_class: std::collections::BTreeMap<i32, Vec<usize>> = std::collections::BTreeMap::new();
        for &i in baked {
            by_class.entry(size(i).log2().floor() as i32).or_default().push(i);
        }
        let classes = by_class
            .into_values()
            .map(|members| {
                let cell = members.iter().map(|&i| size(i)).fold(1e-4f32, f32::max);
                let mut cells: HashMap<[i32; 3], Vec<u32>> = HashMap::new();
                for &i in &members {
                    cells
                        .entry(Self::key(g.positions[i], cell))
                        .or_default()
                        .push(i as u32);
                }
                GridClass { cell, cells }
            })
            .collect();
        Self { classes }
    }

    fn key(p: Vec3, cell: f32) -> [i32; 3] {
        (p / cell).floor().as_ivec3().to_array()
    }

    /// The baked texel of another chart nearest `target` within `reach`, facing like `n` (ties:
    /// the lowest index, so the answer does not depend on the scan order).
    fn across(&self, g: &DenoiseGuides<'_>, target: Vec3, reach: f32, n: Vec3, chart: u32) -> Option<usize> {
        let mut best: Option<(f32, usize)> = None;
        let mut consider = |list: &[u32]| {
            for &j in list {
                #[cfg(test)]
                ACROSS_CANDIDATES.with(|c| c.set(c.get() + 1));
                #[cfg(test)]
                note_across_visit();
                let j = j as usize;
                if g.chart_of[j] == chart || g.normals[j].dot(n) < ACROSS_MIN_COS {
                    continue;
                }
                let d = g.positions[j].distance(target);
                if d <= reach && best.is_none_or(|(bd, bj)| d < bd || (d == bd && j < bj)) {
                    best = Some((d, j));
                }
            }
        };
        for class in &self.classes {
            // A texel within `reach` lies at most `ceil(reach / cell)` cells away per axis.
            let k = i64::from((reach / class.cell).ceil().clamp(1.0, 1024.0) as i32);
            let side = 2 * k + 1;
            if side * side * side > class.cells.len() as i64 {
                // Fewer occupied cells than the cube: every texel of the class is cheaper.
                for list in class.cells.values() {
                    consider(list);
                }
                continue;
            }
            let c = Self::key(target, class.cell).map(i64::from);
            for dz in -k..=k {
                for dy in -k..=k {
                    for dx in -k..=k {
                        let key = [c[0] + dx, c[1] + dy, c[2] + dz]
                            .map(|v| i32::try_from(v).unwrap_or(if v < 0 { i32::MIN } else { i32::MAX }));
                        if let Some(list) = class.cells.get(&key) {
                            consider(list);
                        }
                    }
                }
            }
        }
        best.map(|b| b.1)
    }
}

/// Denoises `texels` in place (see the module docs) and returns the variance of each texel's
/// luminance after the last pass. `settings` must be valid (`LightmapDenoiseInfo::validate`).
pub(crate) fn denoise<const N: usize>(
    texels: &mut [[f32; N]],
    g: &DenoiseGuides<'_>,
    settings: &LightmapDenoiseInfo,
) -> Vec<f32> {
    let _span = tracing::info_span!(
        "lightmap_denoise",
        w = g.w,
        h = g.h,
        iterations = settings.iterations
    )
    .entered();
    let n = g.w * g.h;
    let baked: Vec<usize> = (0..n).filter(|&i| g.coverage[i] == Coverage::BAKED).collect();
    let grid = WorldGrid::new(g, &baked);
    // Variance of each texel's luminance estimate.
    let mut var: Vec<f32> = (0..n)
        .map(|i| {
            let l = lum(&texels[i]).max(0.0);
            let rel = g.rel_err[i];
            let rel = if rel.is_finite() && rel >= 0.0 {
                rel
            } else {
                FALLBACK_REL_NOISE
            };
            (rel * l) * (rel * l)
        })
        .collect();
    let mut src = texels.to_vec();
    let threads = g.threads.max(1);
    let chunk = baked.len().div_ceil(threads).max(1);
    // One buffer for every pass. Workers write disjoint slices; the parent copies them back
    // in baked order, which is the order the per-worker vectors used to be flattened in.
    let mut pass_out = vec![([0.0f32; N], 0.0f32); baked.len()];
    #[cfg(test)]
    note_result_vec();
    for pass in 0..settings.iterations {
        let env = PassEnv {
            step: 1i64 << pass,
            src: &src,
            var: &var,
            g,
            grid: &grid,
            settings,
        };
        std::thread::scope(|scope| {
            spawn_pass_chunks(scope, &baked, &mut pass_out, chunk, &env);
        });
        for (&i, (out, v)) in baked.iter().zip(pass_out.iter()) {
            texels[i] = *out;
            var[i] = *v;
        }
        src.copy_from_slice(texels);
    }
    var
}

/// Shared borrows of one denoise pass.
struct PassEnv<'a, const N: usize> {
    step: i64,
    src: &'a [[f32; N]],
    var: &'a [f32],
    g: &'a DenoiseGuides<'a>,
    grid: &'a WorldGrid,
    settings: &'a LightmapDenoiseInfo,
}

/// Spawns one worker per `chunk` of `baked`, each writing its slice of `out`.
fn spawn_pass_chunks<'scope, const N: usize>(
    scope: &'scope std::thread::Scope<'scope, '_>,
    baked: &'scope [usize],
    out: &'scope mut [([f32; N], f32)],
    chunk: usize,
    env: &'scope PassEnv<'scope, N>,
) {
    if baked.is_empty() {
        return;
    }
    let n = chunk.min(baked.len());
    let (head, tail) = baked.split_at(n);
    let (out_head, out_tail) = out.split_at_mut(n);
    scope.spawn(move || {
        for (slot, i) in out_head.iter_mut().zip(head.iter().copied()) {
            *slot = filter_texel(i, env.step, env.src, env.var, env.g, env.grid, env.settings);
        }
    });
    spawn_pass_chunks(scope, tail, out_tail, chunk, env);
}

/// One texel of one pass: its taps (in its chart, or across a border onto the surface that
/// continues it), then the blend of their weighted mean and first-order fit (module docs).
fn filter_texel<const N: usize>(
    i: usize,
    step: i64,
    src: &[[f32; N]],
    var: &[f32],
    g: &DenoiseGuides<'_>,
    grid: &WorldGrid,
    settings: &LightmapDenoiseInfo,
) -> ([f32; N], f32) {
    let (x, y) = ((i % g.w) as i64, (i / g.w) as i64);
    let (np, xp, cp) = (g.normals[i], g.positions[i], g.chart_of[i]);
    let ts = g.texel_size[i].max(1e-6);
    let [ax, ay] = g.axes[i];
    let can_cross = ax != Vec3::ZERO && ay != Vec3::ZERO;
    let lp = lum(&src[i]);
    let mut taps = [(0usize, 0.0f32, 0.0f32, 0.0f32); 25];
    let mut count = 0usize;
    for (dy, hy) in (-2i64..=2).zip(H) {
        for (dx, hx) in (-2i64..=2).zip(H) {
            let (qx, qy) = (x + dx * step, y + dy * step);
            let inside = qx >= 0 && qy >= 0 && qx < g.w as i64 && qy < g.h as i64;
            let here = inside.then(|| qy as usize * g.w + qx as usize);
            let j = match here {
                Some(j) if g.coverage[j] == Coverage::BAKED && g.chart_of[j] == cp => j,
                _ if can_cross => {
                    // Off the chart: the texel that continues the surface there, if any.
                    let target = xp + ax * (dx * step) as f32 + ay * (dy * step) as f32;
                    match grid.across(g, target, ACROSS_REACH * ts, np, cp) {
                        Some(j) if across_clear(g, i, j, ts) => j,
                        _ => continue,
                    }
                }
                _ => continue,
            };
            let wgt = if j == i {
                hx * hy
            } else {
                let wn = np.dot(g.normals[j]).max(0.0).powf(settings.normal_power);
                let uv_dist = ((dx * dx + dy * dy) as f32).sqrt() * step as f32 * ts;
                let excess =
                    (xp.distance(g.positions[j]) - 1.5 * uv_dist).max(0.0) / (settings.sigma_position * ts);
                let wx = (-excess * excess).exp();
                let lq = lum(&src[j]);
                let sigma = settings.sigma_luminance * (var[i] + var[j]).sqrt();
                let eps = 1e-4 * lp.abs().max(lq.abs()) + 1e-12;
                let wl = (-(lp - lq).abs() / (sigma + eps)).exp();
                hx * hy * wn * wx * wl
            };
            if wgt <= 0.0 {
                continue;
            }
            // Two offsets across a border can resolve to one texel. Keep the offset whose
            // world target is closer to that texel; a tie keeps the heavier kernel weight.
            let target = |ox: f32, oy: f32| xp + ax * (ox * step as f32) + ay * (oy * step as f32);
            if let Some(slot) = taps[..count].iter().position(|t| t.0 == j) {
                let old = taps[slot];
                let dist = g.positions[j].distance_squared(target(dx as f32, dy as f32));
                let old_dist = g.positions[j].distance_squared(target(old.2, old.3));
                if dist < old_dist || (dist == old_dist && wgt > old.1) {
                    taps[slot] = (j, wgt, dx as f32, dy as f32);
                }
                continue;
            }
            taps[count] = (j, wgt, dx as f32, dy as f32);
            count += 1;
        }
    }
    // The texel's own tap is always there, so the taps are not empty.
    let taps = &taps[..count];
    // The weighted mean and the first-order fit of the same taps, blended by their estimated
    // error (module docs).
    let [c0, cx, cy] = first_order(taps);
    let wsum: f32 = taps.iter().map(|t| t.1).sum();
    let (mut lm, mut lf) = (0.0f32, 0.0f32);
    let (mut var_m, mut var_f, mut var_d) = (0.0f32, 0.0f32, 0.0f32);
    for &(j, w, dx, dy) in taps {
        let (wm, wf) = (w / wsum, w * (c0 + cx * dx + cy * dy));
        let l = lum(&src[j]);
        lm += wm * l;
        lf += wf * l;
        var_m += wm * wm * var[j];
        var_f += wf * wf * var[j];
        var_d += (wm - wf) * (wm - wf) * var[j];
    }
    let bias2 = ((lf - lm) * (lf - lm) - FIRST_ORDER_SIGNIFICANCE * var_d).max(0.0);
    let cost = (var_f - var_m).max(0.0);
    let alpha = if bias2 + cost > 0.0 {
        bias2 / (bias2 + cost)
    } else {
        0.0
    };
    let blend = |alpha: f32| -> ([f32; N], f32) {
        let mut out = [0.0f32; N];
        let mut vsum = 0.0f32;
        for &(j, w, dx, dy) in taps {
            let wb = (1.0 - alpha) * w / wsum + alpha * w * (c0 + cx * dx + cy * dy);
            for (s, v) in out.iter_mut().zip(src[j]) {
                *s += wb * v;
            }
            vsum += wb * wb * var[j];
        }
        (out, vsum)
    };
    let alpha = if alpha.is_finite() { alpha } else { 0.0 };
    let (out, vsum) = blend(alpha);
    if out[..N.min(3)].iter().any(|c| *c < 0.0 || !c.is_finite()) {
        // Extrapolating a steep fit went below zero: the plain weighted mean instead.
        return blend(0.0);
    }
    (out, vsum)
}

/// How many times its own variance the squared difference of the fit and the mean must exceed
/// before the excess counts as the mean's bias (3 standard errors). The carried variances
/// understate the noise after the first pass (neighbouring texels share taps), so a lower bar
/// extrapolated noise at chart borders.
const FIRST_ORDER_SIGNIFICANCE: f32 = 9.0;

/// Smallest determinant of the first-order fit's normal matrix, over the cube of its weight sum,
/// that is solved as it is: below it the taps do not span both axes (a chart a texel or two wide).
const FIRST_ORDER_MIN_DET: f64 = 1e-3;
/// Ridge on the gradient terms of a near-singular fit, in squared tap offsets per unit weight: the
/// unsupported gradient falls back toward zero, the fit toward the weighted mean.
const FIRST_ORDER_RIDGE: f64 = 0.25;

/// The first-order (locally linear) weighting of one pass's taps `(texel, weight, dx, dy)`, `dx`
/// and `dy` in tap offsets (ADR-4108): coefficients `c` such that the tap weights
/// `w · (c0 + cx · dx + cy · dy)` fit `a + b · (dx, dy)` to the taps by weighted least squares and
/// return its value at the texel, `a`. They sum to one and reproduce a linear ramp exactly, so a
/// kernel cut off on one side (a chart border, an opening, a shadow edge the luminance weight
/// stops at) no longer pulls the texel toward the light inside the chart; with taps balanced
/// around the texel it is the weighted mean.
fn first_order(taps: &[(usize, f32, f32, f32)]) -> [f32; 3] {
    // M = Σ w · [1, dx, dy]ᵀ[1, dx, dy], and c = M⁻¹ · e0 (M is symmetric).
    let mut m = [[0.0f64; 3]; 3];
    for &(_, w, dx, dy) in taps {
        let r = [1.0, f64::from(dx), f64::from(dy)];
        for (a, ra) in r.iter().enumerate() {
            for (b, rb) in r.iter().enumerate() {
                m[a][b] += f64::from(w) * ra * rb;
            }
        }
    }
    let wsum = m[0][0];
    if wsum <= 0.0 {
        return [0.0; 3];
    }
    // The first column of M⁻¹ by cofactors; `None` when M is near singular.
    let solve = |m: &[[f64; 3]; 3]| -> Option<[f32; 3]> {
        let cof = [
            m[1][1] * m[2][2] - m[1][2] * m[2][1],
            m[1][2] * m[2][0] - m[1][0] * m[2][2],
            m[1][0] * m[2][1] - m[1][1] * m[2][0],
        ];
        let det = m[0][0] * cof[0] + m[0][1] * cof[1] + m[0][2] * cof[2];
        (det.abs() > FIRST_ORDER_MIN_DET * wsum * wsum * wsum).then(|| cof.map(|c| (c / det) as f32))
    };
    if let Some(c) = solve(&m) {
        return c;
    }
    m[1][1] += FIRST_ORDER_RIDGE * wsum;
    m[2][2] += FIRST_ORDER_RIDGE * wsum;
    solve(&m).unwrap_or([(1.0 / wsum) as f32, 0.0, 0.0])
}

/// For each triangle of an unwrap, its chart: triangles joined through shared unwrapped vertices
/// (a chart seam splits a vertex, so charts share none), numbered from 0 in order of first
/// triangle. Returns the per-triangle chart and the chart count.
pub(crate) fn triangle_charts(indices: &[u32], vertices: usize) -> (Vec<u32>, u32) {
    let mut parent: Vec<u32> = (0..vertices as u32).collect();
    fn find(parent: &mut [u32], mut v: u32) -> u32 {
        while parent[v as usize] != v {
            let up = parent[parent[v as usize] as usize];
            parent[v as usize] = up;
            v = up;
        }
        v
    }
    let tris = indices.as_chunks::<3>().0;
    for t in tris {
        if t.iter().any(|&v| v as usize >= vertices) {
            continue;
        }
        for &v in &t[1..] {
            let (a, b) = (find(&mut parent, t[0]), find(&mut parent, v));
            if a != b {
                parent[a.max(b) as usize] = a.min(b);
            }
        }
    }
    let mut number = HashMap::new();
    let mut out = Vec::with_capacity(tris.len());
    for t in tris {
        if t.iter().any(|&v| v as usize >= vertices) {
            out.push(u32::MAX);
            continue;
        }
        let root = find(&mut parent, t[0]);
        let next = number.len() as u32;
        out.push(*number.entry(root).or_insert(next));
    }
    let count = number.len() as u32;
    (out, count)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A noisy flat chart: a constant value with deterministic multiplicative noise.
    fn noisy(w: usize, h: usize, value: f32, amp: f32, seed: u32) -> Vec<[f32; 3]> {
        let mut s = seed | 1;
        (0..w * h)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                let u = (s >> 8) as f32 / (1u32 << 24) as f32 - 0.5;
                let v = value * (1.0 + 2.0 * amp * u);
                [v, v, v]
            })
            .collect()
    }

    fn variance(t: &[[f32; 3]], keep: impl Fn(usize) -> bool) -> f64 {
        let vals: Vec<f64> = t
            .iter()
            .enumerate()
            .filter(|(i, _)| keep(*i))
            .map(|(_, c)| f64::from(c[0]))
            .collect();
        let mean = vals.iter().sum::<f64>() / vals.len() as f64;
        vals.iter().map(|v| (v - mean) * (v - mean)).sum::<f64>() / vals.len() as f64
    }

    struct Fixture {
        w: usize,
        h: usize,
        cov: Vec<u8>,
        normals: Vec<Vec3>,
        positions: Vec<Vec3>,
        chart: Vec<u32>,
        size: Vec<f32>,
        rel: Vec<f32>,
        axes: Vec<[Vec3; 2]>,
    }

    impl Fixture {
        /// A `w × h` atlas: the left half chart 0, the right half chart 1, all baked, on a plane.
        fn two_charts(w: usize, h: usize, rel_err: f32) -> Self {
            let n = w * h;
            Self {
                w,
                h,
                cov: vec![Coverage::BAKED; n],
                normals: vec![Vec3::Y; n],
                positions: (0..n)
                    .map(|i| Vec3::new((i % w) as f32 * 0.125, 0.0, (i / w) as f32 * 0.125))
                    .collect(),
                chart: (0..n).map(|i| u32::from(i % w >= w / 2)).collect(),
                size: vec![0.125; n],
                rel: vec![rel_err; n],
                axes: vec![[Vec3::X * 0.125, Vec3::Z * 0.125]; n],
            }
        }

        fn guides(&self) -> DenoiseGuides<'_> {
            DenoiseGuides {
                coverage: &self.cov,
                normals: &self.normals,
                positions: &self.positions,
                chart_of: &self.chart,
                texel_size: &self.size,
                rel_err: &self.rel,
                axes: &self.axes,
                clear: None,
                threads: 3,
                w: self.w,
                h: self.h,
            }
        }
    }

    /// One denoise of a chart allocates one result vector per worker per pass. The pass should
    /// write into one buffer kept for every pass.
    #[test]
    fn perfaud_denoise_pass_reuses_one_result_buffer() {
        let (w, h) = (16, 8);
        let f = Fixture::two_charts(w, h, 0.1);
        let mut t = vec![[1.0f32; 3]; w * h];
        let settings = LightmapDenoiseInfo {
            iterations: 3,
            ..LightmapDenoiseInfo::default()
        };
        COUNT_RESULT_VECS.set(true);
        RESULT_VECS.set(0);
        denoise(&mut t, &f.guides(), &settings);
        let vecs = RESULT_VECS.get();
        COUNT_RESULT_VECS.set(false);
        assert!(
            vecs <= 1,
            "denoise allocated {vecs} result vectors (3 passes, 3 workers); want at most 1 reused buffer"
        );
    }

    /// A 1 cm query next to a 50 m texel must not walk every small texel. The cell size used to be
    /// the largest texel, so the small texels shared one bucket.
    #[test]
    fn perfaud_small_across_query_does_not_scan_the_coarse_bucket() {
        let n = 64 * 64;
        let total = n + 1;
        let mut positions = Vec::with_capacity(total);
        let mut size = Vec::with_capacity(total);
        let mut chart = Vec::with_capacity(total);
        for i in 0..n {
            let x = (i % 64) as f32 * 0.01;
            let z = (i / 64) as f32 * 0.01;
            positions.push(Vec3::new(x, 0.0, z));
            size.push(0.01);
            chart.push(0);
        }
        positions.push(Vec3::new(0.0, 0.0, 0.0));
        size.push(50.0);
        chart.push(2);
        let normals = vec![Vec3::Y; total];
        let cov = vec![Coverage::BAKED; total];
        let rel = vec![0.1f32; total];
        let axes = vec![[Vec3::X * 0.01, Vec3::Z * 0.01]; total];
        let g = DenoiseGuides {
            coverage: &cov,
            normals: &normals,
            positions: &positions,
            chart_of: &chart,
            texel_size: &size,
            rel_err: &rel,
            axes: &axes,
            clear: None,
            threads: 1,
            w: total,
            h: 1,
        };
        let baked: Vec<usize> = (0..total).collect();
        let grid = WorldGrid::new(&g, &baked);
        let target = Vec3::new(0.02, 0.0, 0.02);
        let reach = 0.75 * 0.01;
        COUNT_ACROSS_VISITS.set(true);
        ACROSS_VISITS.set(0);
        let found = grid.across(&g, target, reach, Vec3::Y, 1);
        let visits = ACROSS_VISITS.get();
        COUNT_ACROSS_VISITS.set(false);
        let mut brute: Option<(f32, usize)> = None;
        for j in 0..total {
            if chart[j] == 1 || normals[j].dot(Vec3::Y) < ACROSS_MIN_COS {
                continue;
            }
            let d = positions[j].distance(target);
            if d <= reach && brute.is_none_or(|(bd, bj)| d < bd || (d == bd && j < bj)) {
                brute = Some((d, j));
            }
        }
        assert_eq!(found, brute.map(|b| b.1), "across changed which texel it returns");
        assert!(
            visits < 64,
            "a 1 cm query examined {visits} texels; the 50 m texel pulled them into one bucket"
        );
        // A reach that covers the large texel stays on the coarse grid and returns the same texel.
        let wide_reach = 0.75 * 50.0;
        let wide = grid.across(&g, target, wide_reach, Vec3::Y, 1);
        let mut wide_brute: Option<(f32, usize)> = None;
        for j in 0..total {
            if chart[j] == 1 || normals[j].dot(Vec3::Y) < ACROSS_MIN_COS {
                continue;
            }
            let d = positions[j].distance(target);
            if d <= wide_reach && wide_brute.is_none_or(|(bd, bj)| d < bd || (d == bd && j < bj)) {
                wide_brute = Some((d, j));
            }
        }
        assert_eq!(
            wide,
            wide_brute.map(|b| b.1),
            "the coarse grid changed which texel a wide query returns"
        );
    }

    /// The acceptance figure: on a noisy flat chart (±20 % uniform noise, a relative standard
    /// error of 11.5 %) the default denoiser cuts the variance by at least 8×.
    #[test]
    fn denoise_reduces_variance_on_a_noisy_chart_by_a_fixed_factor() {
        let (w, h) = (32, 32);
        let f = Fixture::two_charts(w, h, 0.115);
        let mut t = noisy(w, h, 100.0, 0.2, 7);
        let before = variance(&t, |_| true);
        denoise(&mut t, &f.guides(), &LightmapDenoiseInfo::default());
        let left = variance(&t, |i| i % w < w / 2);
        let right = variance(&t, |i| i % w >= w / 2);
        assert!(
            left * 8.0 <= before && right * 8.0 <= before,
            "{before} -> {left} / {right}"
        );
        // The mean is kept (no darkening).
        let mean = t.iter().map(|c| f64::from(c[0])).sum::<f64>() / t.len() as f64;
        assert!((mean - 100.0).abs() < 1.0, "{mean}");
    }

    /// A linear ramp across two charts (one flat wall cut in two, as a grid of blocks around an
    /// opening is): the weighted mean of a kernel cut off at the border pulls each side toward its
    /// own chart's light, a step of about a kernel's reach times the gradient; where the gradient
    /// stands out of the noise the first-order fit takes it back (ADR-4108).
    #[test]
    fn a_ramp_across_a_chart_border_has_no_step() {
        let (w, h) = (64, 16);
        // Texels measured to 2 %: the luminance weights take the ramp's neighbours, and the
        // gradient stands out of the noise, so the border texels follow the fit.
        let f = Fixture::two_charts(w, h, 0.02);
        // 1 % of the mean per texel, rising to the right.
        let ramp = |x: usize| 100.0 + x as f32;
        let mut t: Vec<[f32; 3]> = (0..w * h).map(|i| [ramp(i % w); 3]).collect();
        denoise(&mut t, &f.guides(), &LightmapDenoiseInfo::default());
        // The step between the two border texels, less the ramp's own one unit, over the middle
        // rows (away from the atlas's top and bottom edges): the plain weighted mean leaves 2.95
        // (measured, ADR-4108); the blend with the fit must cut it by a third at least (it
        // leaves 1.71).
        let rows = h / 4..3 * h / 4;
        let excess: f32 = rows
            .clone()
            .map(|y| t[y * w + w / 2][0] - t[y * w + w / 2 - 1][0] - 1.0)
            .sum::<f32>()
            / rows.len() as f32;
        assert!(excess.abs() <= 2.0, "excess step {excess}");
        // The plain mean of the same one-sided kernel: what the old denoiser did.
        let mean_at_border: f32 = {
            let taps: Vec<(usize, f32, f32, f32)> = (-2i32..=0)
                .flat_map(|dx| (-2i32..=2).map(move |dy| (dx, dy)))
                .map(|(dx, dy)| {
                    let hx = H[(dx + 2) as usize];
                    let hy = H[(dy + 2) as usize];
                    (0, hx * hy, dx as f32, dy as f32)
                })
                .collect();
            let wsum: f32 = taps.iter().map(|t| t.1).sum();
            taps.iter()
                .map(|t| t.1 / wsum * ramp((31.0 + t.2) as usize))
                .sum()
        };
        // Half a unit for one pass at spacing 1 (the kernel's centroid sits 0.55 taps inside).
        assert!(
            ramp(31) - mean_at_border > 0.5,
            "the one-sided mean is biased: {mean_at_border}"
        );
        // The fit of the same taps is exact.
        let taps: Vec<(usize, f32, f32, f32)> = (-2i32..=0)
            .flat_map(|dx| (-2i32..=2).map(move |dy| (dx, dy)))
            .map(|(dx, dy)| {
                (
                    0,
                    H[(dx + 2) as usize] * H[(dy + 2) as usize],
                    dx as f32,
                    dy as f32,
                )
            })
            .collect();
        let [c0, cx, cy] = first_order(&taps);
        let fit: f32 = taps
            .iter()
            .map(|t| t.1 * (c0 + cx * t.2 + cy * t.3) * ramp((31.0 + t.2) as usize))
            .sum();
        assert!((fit - ramp(31)).abs() < 1e-3, "{fit}");
    }

    /// Where the texels' noise is far larger than what a one-sided kernel's gradient explains, the
    /// texel keeps (nearly) the weighted mean: the fit would extrapolate noise.
    #[test]
    fn noise_at_a_border_is_not_extrapolated() {
        let (w, h) = (32, 32);
        let f = Fixture::two_charts(w, h, 0.115);
        let mut fit = noisy(w, h, 100.0, 0.2, 5);
        let start = fit.clone();
        denoise(&mut fit, &f.guides(), &LightmapDenoiseInfo::default());
        // A one-sided kernel has fewer taps, so the border columns keep more noise than the
        // interior even with the plain mean (2.9 times here, measured); always taking the fit made
        // it 4.0 times. The blend stays within 3.3 times.
        let column = |t: &[[f32; 3]], x: usize| variance(t, |i| i % w == x);
        let border = 0.5 * (column(&fit, w / 2 - 1) + column(&fit, w / 2));
        let inner = 0.5 * (column(&fit, w / 4) + column(&fit, 3 * w / 4));
        assert!(
            border <= 3.3 * inner.max(1e-6),
            "border {border} interior {inner}"
        );
        assert!(border * 8.0 <= variance(&start, |_| true), "{border}");
    }

    /// The first-order weights sum to one, and a strip one texel wide (no spread across it) falls
    /// back toward the mean instead of dividing by zero.
    #[test]
    fn first_order_weights_sum_to_one_and_survive_a_strip() {
        let full: Vec<(usize, f32, f32, f32)> = (-2i32..=2)
            .flat_map(|dx| (-2i32..=2).map(move |dy| (dx, dy)))
            .map(|(dx, dy)| {
                (
                    0,
                    H[(dx + 2) as usize] * H[(dy + 2) as usize],
                    dx as f32,
                    dy as f32,
                )
            })
            .collect();
        let [c0, cx, cy] = first_order(&full);
        let sum: f32 = full.iter().map(|t| t.1 * (c0 + cx * t.2 + cy * t.3)).sum();
        assert!((sum - 1.0).abs() < 1e-5, "{sum}");
        // Balanced taps: the plain weighted mean (no gradient terms).
        assert!(cx.abs() < 1e-6 && cy.abs() < 1e-6, "{cx} {cy}");
        let strip: Vec<(usize, f32, f32, f32)> = (-2i32..=2)
            .map(|dx| (0, H[(dx + 2) as usize], dx as f32, 0.0))
            .collect();
        let c = first_order(&strip);
        assert!(c.iter().all(|v| v.is_finite()), "{c:?}");
        let sum: f32 = strip.iter().map(|t| t.1 * (c[0] + c[1] * t.2 + c[2] * t.3)).sum();
        assert!((sum - 1.0).abs() < 1e-4, "{sum}");
    }

    /// A chart is denoised as if charts elsewhere in the world (another wall, another entry) were
    /// not there: bit-exact. Only the surface that continues a chart across its border is read
    /// (next test); here the right chart lies 100 m away.
    #[test]
    fn chart_borders_stay_exact() {
        let (w, h) = (32, 16);
        let mut f = Fixture::two_charts(w, h, 0.1);
        for (i, p) in f.positions.iter_mut().enumerate() {
            if i % w >= w / 2 {
                p.x += 100.0;
            }
        }
        let mut both = noisy(w, h, 50.0, 0.2, 3);
        // The right chart is ten times brighter.
        for (i, c) in both.iter_mut().enumerate() {
            if i % w >= w / 2 {
                *c = c.map(|v| v * 10.0);
            }
        }
        let mut alone = both.clone();
        // The right chart's texels unbaked: the left chart cannot see them at all.
        let mut f_alone = Fixture::two_charts(w, h, 0.1);
        for (i, c) in f_alone.cov.iter_mut().enumerate() {
            if i % w >= w / 2 {
                *c = Coverage::EMPTY;
            }
        }
        let settings = LightmapDenoiseInfo::default();
        denoise(&mut both, &f.guides(), &settings);
        denoise(&mut alone, &f_alone.guides(), &settings);
        for i in (0..w * h).filter(|i| i % w < w / 2) {
            assert_eq!(both[i], alone[i], "texel {i}");
        }
        // Unbaked texels are never written.
        for i in (0..w * h).filter(|i| i % w >= w / 2) {
            assert_eq!(f_alone.cov[i], Coverage::EMPTY);
        }
    }

    /// Two charts that continue one flat surface (a wall built as a grid of blocks around an
    /// opening) are denoised as one: their residual noise is continuous across the border, so the
    /// border shows no step. Apart in the world, the same two charts keep a step of their own
    /// residuals (ADR-4108).
    #[test]
    fn a_surface_continued_across_a_chart_border_is_denoised_as_one() {
        let (w, h) = (64, 32);
        // The mean step between the two border columns, over the rows, relative to the light.
        let step = |t: &[[f32; 3]]| -> f32 {
            (0..h)
                .map(|y| (t[y * w + w / 2][0] - t[y * w + w / 2 - 1][0]).abs())
                .sum::<f32>()
                / h as f32
                / 100.0
        };
        let (mut joined, mut apart) = (0.0f32, 0.0f32);
        for seed in [3, 7, 11, 19] {
            let start = noisy(w, h, 100.0, 0.4, seed);
            let f = Fixture::two_charts(w, h, 0.23);
            let mut t = start.clone();
            denoise(&mut t, &f.guides(), &LightmapDenoiseInfo::default());
            joined += step(&t);
            let mut g = Fixture::two_charts(w, h, 0.23);
            for (i, p) in g.positions.iter_mut().enumerate() {
                if i % w >= w / 2 {
                    p.x += 100.0;
                }
            }
            let mut t = start;
            denoise(&mut t, &g.guides(), &LightmapDenoiseInfo::default());
            apart += step(&t);
        }
        assert!(joined < 0.6 * apart, "joined {joined} apart {apart}");
        // A crease (the other side's normal 90° away) is not a continuation.
        let mut f = Fixture::two_charts(w, h, 0.23);
        for (i, n) in f.normals.iter_mut().enumerate() {
            if i % w >= w / 2 {
                *n = Vec3::X;
            }
        }
        let mut lit: Vec<[f32; 3]> = (0..w * h)
            .map(|i| if i % w < w / 2 { [10.0; 3] } else { [1000.0; 3] })
            .collect();
        denoise(&mut lit, &f.guides(), &LightmapDenoiseInfo::default());
        assert!((lit[w / 2 - 1][0] - 10.0).abs() < 1e-3, "{:?}", lit[w / 2 - 1]);
    }

    /// A wall between two charts of one plane blocks the taps across their border: the dark chart
    /// (a sealed room's floor) keeps its light exactly while the bright one (the sunlit ground
    /// outside) is a hundred times brighter and noisy enough for the luminance weight to let it in.
    /// With nothing between them the same border texel reads the bright chart.
    #[test]
    fn a_wall_between_two_charts_blocks_the_taps_across() {
        let (w, h) = (32, 16);
        let f = Fixture::two_charts(w, h, 0.25);
        // Texel columns sit at x = col / 8; the wall stands between columns 15 and 16.
        let wall_x = (w / 2) as f32 * 0.125 - 0.0625;
        let wall = |a: Vec3, b: Vec3| (a.x < wall_x) == (b.x < wall_x);
        let open = |_: Vec3, _: Vec3| true;
        let lit = || -> Vec<[f32; 3]> {
            (0..w * h)
                .map(|i| if i % w < w / 2 { [10.0; 3] } else { [1000.0; 3] })
                .collect()
        };
        let border = w / 2 - 1 + (h / 2) * w;
        let settings = LightmapDenoiseInfo::default();
        let mut walled = lit();
        let mut g = f.guides();
        g.clear = Some(&wall);
        denoise(&mut walled, &g, &settings);
        for i in (0..w * h).filter(|i| i % w < w / 2) {
            assert!((walled[i][0] - 10.0).abs() < 1e-3, "texel {i}: {:?}", walled[i]);
        }
        let mut seen = lit();
        g.clear = Some(&open);
        denoise(&mut seen, &g, &settings);
        assert!(seen[border][0] > 20.0, "{:?}", seen[border]);
    }

    /// A crease inside one chart (a 90° fold) and a luminance step both survive.
    #[test]
    fn creases_and_light_edges_survive() {
        let (w, h) = (16, 16);
        let mut f = Fixture::two_charts(w, h, 0.02);
        f.chart = vec![0; w * h];
        for (i, n) in f.normals.iter_mut().enumerate() {
            if i % w >= w / 2 {
                *n = Vec3::X;
            }
        }
        let mut t: Vec<[f32; 3]> = (0..w * h)
            .map(|i| if i % w < w / 2 { [10.0; 3] } else { [1000.0; 3] })
            .collect();
        denoise(&mut t, &f.guides(), &LightmapDenoiseInfo::default());
        for (i, c) in t.iter().enumerate() {
            let want = if i % w < w / 2 { 10.0 } else { 1000.0 };
            assert!((c[0] - want).abs() <= want * 1e-3, "texel {i}: {c:?}");
        }
        // The same step on one plane with no normal change: the noise-scaled luminance weight
        // alone keeps it (a step of 100× is far outside 4 standard errors of 2 % noise).
        let f = Fixture {
            chart: vec![0; w * h],
            ..Fixture::two_charts(w, h, 0.02)
        };
        let mut t: Vec<[f32; 3]> = (0..w * h)
            .map(|i| if i % w < w / 2 { [10.0; 3] } else { [1000.0; 3] })
            .collect();
        denoise(&mut t, &f.guides(), &LightmapDenoiseInfo::default());
        assert!((t[w / 2 - 1][0] - 10.0).abs() < 0.1, "{:?}", t[w / 2 - 1]);
        assert!((t[w / 2][0] - 1000.0).abs() < 10.0, "{:?}", t[w / 2]);
    }

    #[test]
    fn denoise_is_deterministic_and_leaves_flat_light_alone() {
        let (w, h) = (12, 12);
        let f = Fixture::two_charts(w, h, f32::NAN);
        let flat = vec![[3.5f32, 2.0, 1.0]; w * h];
        let mut t = flat.clone();
        denoise(&mut t, &f.guides(), &LightmapDenoiseInfo::default());
        for (a, b) in t.iter().zip(&flat) {
            for c in 0..3 {
                assert!((a[c] - b[c]).abs() <= b[c] * 1e-6);
            }
        }
        let mut a = noisy(w, h, 9.0, 0.3, 11);
        let mut b = a.clone();
        denoise(&mut a, &f.guides(), &LightmapDenoiseInfo::default());
        denoise(&mut b, &f.guides(), &LightmapDenoiseInfo::default());
        assert_eq!(a, b);
    }

    /// Kernel offsets dx = 1 and dx = 2 both reach one texel of the next chart (it sits
    /// 0.5 texel from each target, inside the 0.75 reach). Counting it twice pulls the
    /// border texel toward that chart. With the luminance weight held at 1 the single
    /// count (the heavier tap, dx = 1) is 0.6; both taps give about 0.545.
    #[test]
    fn an_across_chart_texel_is_one_tap() {
        let ts = 1.0f32;
        let (w, h) = (2usize, 1usize);
        let n = w * h;
        let cov = vec![Coverage::BAKED; n];
        let normals = vec![Vec3::Y; n];
        let positions = vec![Vec3::ZERO, Vec3::new(1.5 * ts, 0.0, 0.0)];
        let chart = vec![0u32, 1];
        let size = vec![ts; n];
        let rel = vec![0.0; n];
        let axes = vec![[Vec3::X * ts, Vec3::Z * ts]; n];
        let g = DenoiseGuides {
            coverage: &cov,
            normals: &normals,
            positions: &positions,
            chart_of: &chart,
            texel_size: &size,
            rel_err: &rel,
            axes: &axes,
            clear: None,
            threads: 1,
            w,
            h,
        };
        let src = vec![[1.0f32; 3], [0.0; 3]];
        let var = vec![1.0e20; n];
        let grid = WorldGrid::new(&g, &[0, 1]);
        let (out, _) = filter_texel(0, 1, &src, &var, &g, &grid, &LightmapDenoiseInfo::default());
        assert!(
            (out[0] - 0.6).abs() < 1.0e-3,
            "across-chart texel counted twice: {}",
            out[0]
        );
    }

    #[test]
    fn charts_are_the_vertex_connected_triangles() {
        // Two quads sharing no vertex, then a third triangle joined to the second quad.
        let indices = [0, 1, 2, 0, 2, 3, 4, 5, 6, 4, 6, 7, 6, 7, 8];
        let (charts, count) = triangle_charts(&indices, 9);
        assert_eq!(charts, [0, 0, 1, 1, 1]);
        assert_eq!(count, 2);
        // Out-of-range indices do not panic.
        let (charts, count) = triangle_charts(&[0, 1, 9], 3);
        assert_eq!((charts, count), (vec![u32::MAX], 0));
    }

    /// One coarse texel (a 20 m texel, as a small `density_scale` gives a far mesh) next to a
    /// fine plane must not make every off-chart lookup scan the whole plane: the grid is kept per
    /// texel-size class. The answers equal a brute-force nearest search over every texel.
    #[test]
    fn a_coarse_texel_does_not_make_across_lookups_scan_the_whole_bake() {
        let (w, h) = (64, 64);
        let mut f = Fixture::two_charts(w, h, 0.05);
        // 4 × 4-texel charts, so most taps leave their chart.
        f.chart = (0..w * h)
            .map(|i| ((i % w) / 4 + (i / w) / 4 * 16) as u32)
            .collect();
        f.size[0] = 20.0;
        let g = f.guides();
        let baked: Vec<usize> = (0..w * h).collect();
        let grid = WorldGrid::new(&g, &baked);
        let brute = |target: Vec3, reach: f32, n: Vec3, chart: u32| {
            let mut best: Option<(f32, usize)> = None;
            for j in 0..w * h {
                if g.chart_of[j] == chart || g.normals[j].dot(n) < ACROSS_MIN_COS {
                    continue;
                }
                let d = g.positions[j].distance(target);
                if d <= reach && best.is_none_or(|(bd, bj)| d < bd || (d == bd && j < bj)) {
                    best = Some((d, j));
                }
            }
            best.map(|b| b.1)
        };
        let before = ACROSS_CANDIDATES.with(std::cell::Cell::get);
        let mut lookups = 0u64;
        for i in (1..w * h).step_by(7) {
            let target = g.positions[i] + Vec3::X * 0.25;
            let reach = ACROSS_REACH * g.texel_size[i];
            let got = grid.across(&g, target, reach, Vec3::Y, g.chart_of[i]);
            assert_eq!(got, brute(target, reach, Vec3::Y, g.chart_of[i]), "texel {i}");
            lookups += 1;
        }
        let per = (ACROSS_CANDIDATES.with(std::cell::Cell::get) - before) / lookups;
        assert!(per < 200, "{per} candidates per fine lookup");
        // The coarse texel's own lookup still finds the nearest texel within its 15 m reach.
        let target = g.positions[0] + Vec3::X * 3.0;
        let got = grid.across(&g, target, ACROSS_REACH * 20.0, Vec3::Y, g.chart_of[0]);
        assert_eq!(got, brute(target, ACROSS_REACH * 20.0, Vec3::Y, g.chart_of[0]));
    }
}
