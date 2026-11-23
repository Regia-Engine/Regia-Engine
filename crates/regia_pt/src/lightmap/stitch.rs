// SPDX-License-Identifier: MPL-2.0
//! Seam stitching (ADR-2404, `docs/specs/lightmaps.md` § 4.5): along every edge where a smooth
//! surface is cut in UV2, the two sides read different texels, so their bilinear lookups disagree
//! and a line shows that no lighting explains. After the bake (filter or denoiser, then the gutter
//! dilation) a least-squares solve changes the texels those lookups read so that they agree,
//! weighted toward the baked values (Iwanicki, *Lighting Technology of The Last of Us*, SIGGRAPH
//! 2013; the seam-constraint formulation of Liu, Ferguson, Jacobson and Gingold, *Seamless: Seam
//! erasure and seam-aware decoupling of shape from mesh resolution*, SIGGRAPH Asia 2017, § 4).
//!
//! **Seams.** The edges the chart-seam metric measures (`regia_asset::lightmap::seam_report`):
//! the source vertices are welded by world position, and an edge whose two sides read different
//! UV2 is a seam unless its faces bend by more than `DEFAULT_HARD_EDGE_DEG` (a real crease, where a
//! step in the light is right).
//!
//! **Constraints.** Each seam edge is sampled at `k + 0.5` of `n` points, `n` = twice its length
//! in texels (2 to [`MAX_EDGE_SAMPLES`]), so both sides are sampled at least at their own texel
//! spacing. At each point the two sides' bilinear lookups are the runtime's (`Lightmap::sample`:
//! texel centres at `k + 0.5`, clamp to edge, empty texels left out and the rest renormalised),
//! so a sample is a linear row `g · x = 0` over at most eight texels, `g` the first side's weights
//! minus the second's.
//!
//! **Solve.** Over the texels any row reads, minimise `Σ_rows (g · x)² + λ Σ_texels (x − x₀)²`,
//! `x₀` the baked value and `λ` = [`STITCH_REGULARIZATION`]: the normal equations
//! `(Σ g gᵀ + λ I) x = λ x₀` are symmetric positive definite, solved per channel by conjugate
//! gradients with a Jacobi preconditioner (at most [`MAX_CG_ITERATIONS`], to a relative residual of
//! [`CG_TOLERANCE`]), matrix-free over the rows. Every channel of a texel (irradiance and, in a
//! directional bake, its direction) uses the same rows. Irradiance is clamped at 0 afterwards and
//! the direction's agreement to `0..=1`.
//!
//! Deterministic: seams are visited in welded-edge order, unknowns numbered as first read, the
//! solver runs sequentially; no random numbers, no hash order.

use std::collections::HashMap;

use regia_asset::lightmap::{Coverage, DEFAULT_HARD_EDGE_DEG, LightmapLayout};

/// Weight of a texel's baked value against the seam rows.
pub(super) const STITCH_REGULARIZATION: f64 = 0.05;
/// Most sample points along one seam edge.
pub(super) const MAX_EDGE_SAMPLES: usize = 64;
/// Most conjugate-gradient iterations per channel.
pub(super) const MAX_CG_ITERATIONS: usize = 256;
/// Relative residual the solve stops at.
pub(super) const CG_TOLERANCE: f64 = 1e-7;
/// UVs closer than this are one point (as the seam metric).
const UV_EPSILON: f32 = 1e-6;

/// What the stitch did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct StitchStats {
    /// Seam edges stitched.
    pub edges: usize,
    /// Sample rows over all seams.
    pub samples: usize,
    /// Texels the solve changed.
    pub texels: usize,
}

/// One side of a welded edge: its triangle and its two unwrapped vertices, in welded order.
#[derive(Clone, Copy)]
struct Side {
    tri: u32,
    a: u32,
    b: u32,
}

/// One seam edge: the two sides, in the entry's local UV.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct SeamEdge {
    /// First side's ends.
    pub a: [[f32; 2]; 2],
    /// Second side's ends (the same welded points, in the same order).
    pub b: [[f32; 2]; 2],
}

fn same_uv(a: [f32; 2], b: [f32; 2]) -> bool {
    (a[0] - b[0]).abs() <= UV_EPSILON && (a[1] - b[1]).abs() <= UV_EPSILON
}

/// The smooth seams of one entry (module docs), from its unwrap and the world positions of its
/// source vertices. Deterministic order.
pub(super) fn seam_edges(
    positions: &[[f32; 3]],
    xref: &[u32],
    uv: &[[f32; 2]],
    indices: &[u32],
    hard_edge_deg: f32,
) -> Vec<SeamEdge> {
    if xref.iter().any(|&s| s as usize >= positions.len()) || indices.iter().any(|&i| i as usize >= uv.len())
    {
        return Vec::new();
    }
    let position = |v: u32| positions[xref[v as usize] as usize];
    let mut welds: HashMap<[u32; 3], u32> = HashMap::new();
    let weld: Vec<u32> = (0..xref.len() as u32)
        .map(|v| {
            let key = position(v).map(|c| (c + 0.0).to_bits());
            let next = welds.len() as u32;
            *welds.entry(key).or_insert(next)
        })
        .collect();
    let tris = indices.as_chunks::<3>().0;
    let mut edges: Vec<((u32, u32), Side)> = Vec::with_capacity(tris.len() * 3);
    for (k, t) in tris.iter().enumerate() {
        for c in 0..3 {
            let (a, b) = (t[c], t[(c + 1) % 3]);
            let (wa, wb) = (weld[a as usize], weld[b as usize]);
            if wa == wb {
                continue;
            }
            let side = Side {
                tri: k as u32,
                a: if wa < wb { a } else { b },
                b: if wa < wb { b } else { a },
            };
            edges.push(((wa.min(wb), wa.max(wb)), side));
        }
    }
    edges.sort_by_key(|(key, side)| (*key, side.tri));
    let face = |tri: u32| -> Option<[f64; 3]> {
        let t = tris[tri as usize];
        let p = |v: u32| position(v).map(f64::from);
        let (p0, p1, p2) = (p(t[0]), p(t[1]), p(t[2]));
        let u = [p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2]];
        let v = [p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2]];
        let n = [
            u[1] * v[2] - u[2] * v[1],
            u[2] * v[0] - u[0] * v[2],
            u[0] * v[1] - u[1] * v[0],
        ];
        let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
        (len > 1e-20).then(|| n.map(|c| c / len))
    };
    let crease_cos = f64::from(hard_edge_deg.to_radians().cos());
    let mut out = Vec::new();
    let mut at = 0;
    while at < edges.len() {
        let key = edges[at].0;
        let end = at + edges[at..].iter().take_while(|(k, _)| *k == key).count();
        let group = &edges[at..end];
        at = end;
        // Every pair of sides (as `regia_asset::lightmap::seam_report`): where closed blocks
        // meet, four faces share an edge and the coplanar pair is the seam (ADR-4108).
        for (i, (_, first)) in group.iter().enumerate() {
            for (_, other) in &group[i + 1..] {
                if same_uv(uv[first.a as usize], uv[other.a as usize])
                    && same_uv(uv[first.b as usize], uv[other.b as usize])
                {
                    continue;
                }
                let (Some(n0), Some(n1)) = (face(first.tri), face(other.tri)) else {
                    continue;
                };
                if n0[0] * n1[0] + n0[1] * n1[1] + n0[2] * n1[2] < crease_cos {
                    continue;
                }
                out.push(SeamEdge {
                    a: [uv[first.a as usize], uv[first.b as usize]],
                    b: [uv[other.a as usize], uv[other.b as usize]],
                });
            }
        }
    }
    out
}

/// The bilinear taps of `Lightmap::sample` at atlas UV `uv` over the texels `coverage` does not
/// mark empty, renormalised; empty when none is covered. `page_rows` is the height of one page
/// of a stacked atlas (`h` when the atlas has one page) and `page` the page of the entry the
/// sample belongs to: the sample clamps inside that page, the way `Lightmap::sample_page` does
/// with the entry's page. Clamping to `h` mixes the last row of a page with the first row of the
/// next one, and inferring the page from the sample puts a sample on the top half-texel of a
/// page on the page before it.
pub(super) fn bilinear_taps(
    uv: [f32; 2],
    coverage: &[u8],
    w: usize,
    h: usize,
    page_rows: usize,
    page: usize,
) -> Vec<(usize, f64)> {
    let fx = uv[0] * w as f32 - 0.5;
    let fy = uv[1] * h as f32 - 0.5;
    let (x0, y0) = (fx.floor(), fy.floor());
    let (tx, ty) = (fx - x0, fy - y0);
    let page_rows = page_rows.clamp(1, h.max(1));
    let page = page.min(h.max(1).div_ceil(page_rows).saturating_sub(1));
    let y_lo = page * page_rows;
    let y_hi = ((page + 1) * page_rows).min(h).saturating_sub(1);
    let clamp_x = |v: f32| v.clamp(0.0, w.saturating_sub(1) as f32) as usize;
    let clamp_y = |v: f32| v.clamp(y_lo as f32, y_hi as f32) as usize;
    let mut taps: Vec<(usize, f64)> = Vec::with_capacity(4);
    for (dx, dy, wgt) in [
        (0.0, 0.0, (1.0 - tx) * (1.0 - ty)),
        (1.0, 0.0, tx * (1.0 - ty)),
        (0.0, 1.0, (1.0 - tx) * ty),
        (1.0, 1.0, tx * ty),
    ] {
        let i = clamp_y(y0 + dy) * w + clamp_x(x0 + dx);
        if coverage[i] == Coverage::EMPTY || wgt <= 0.0 {
            continue;
        }
        match taps.iter_mut().find(|t| t.0 == i) {
            Some(t) => t.1 += f64::from(wgt),
            None => taps.push((i, f64::from(wgt))),
        }
    }
    let sum: f64 = taps.iter().map(|t| t.1).sum();
    if sum <= 0.0 {
        return Vec::new();
    }
    for t in &mut taps {
        t.1 /= sum;
    }
    taps
}

/// One sample row over local unknowns.
struct Row {
    taps: Vec<(u32, f64)>,
}

/// Stitches the smooth seams of every entry in `values` (module docs). `positions[e]` holds the
/// world positions of entry `e`'s source vertices (empty: the entry is left alone); `coverage` is
/// after the gutter dilation. `page_rows` is one page of the stacked atlas (`h` for one page);
/// seam samples do not read the next page.
pub(super) fn stitch<const N: usize>(
    values: &mut [[f32; N]],
    coverage: &[u8],
    layout: &LightmapLayout,
    positions: &[&[[f32; 3]]],
    w: usize,
    h: usize,
    page_rows: usize,
) -> StitchStats {
    let _span = tracing::info_span!("lightmap_stitch", entries = layout.entries.len()).entered();
    let mut stats = StitchStats::default();
    let mut index_of: HashMap<usize, u32> = HashMap::new();
    let mut texels: Vec<usize> = Vec::new();
    let mut rows: Vec<Row> = Vec::new();
    for (e, (entry, unwrap)) in layout.entries.iter().zip(&layout.unwraps).enumerate() {
        let Some(source) = positions.get(e).copied() else {
            continue;
        };
        let seams = seam_edges(
            source,
            &unwrap.xref,
            &unwrap.uv,
            &unwrap.indices,
            DEFAULT_HARD_EDGE_DEG,
        );
        let s = entry.scale_offset;
        let atlas = |p: [f32; 2]| [p[0] * s[0] + s[2], p[1] * s[1] + s[3]];
        // The stacked layout keeps each entry's rows on its own page (`LightmapLayout::stacked`).
        let page = entry.rect[1] as usize / page_rows.max(1);
        let (rw, rh) = (entry.rect[2] as f32, entry.rect[3] as f32);
        for seam in &seams {
            let len = |q: [[f32; 2]; 2]| {
                let (dx, dy) = ((q[1][0] - q[0][0]) * rw, (q[1][1] - q[0][1]) * rh);
                (dx * dx + dy * dy).sqrt()
            };
            let length = len(seam.a).max(len(seam.b));
            let n = ((2.0 * length).ceil() as usize).clamp(2, MAX_EDGE_SAMPLES);
            let mut used = false;
            for k in 0..n {
                let t = (k as f32 + 0.5) / n as f32;
                let lerp = |q: [[f32; 2]; 2]| {
                    [
                        q[0][0] + (q[1][0] - q[0][0]) * t,
                        q[0][1] + (q[1][1] - q[0][1]) * t,
                    ]
                };
                let ta = bilinear_taps(atlas(lerp(seam.a)), coverage, w, h, page_rows, page);
                let tb = bilinear_taps(atlas(lerp(seam.b)), coverage, w, h, page_rows, page);
                if ta.is_empty() || tb.is_empty() {
                    continue;
                }
                let mut g: Vec<(usize, f64)> = ta;
                for (i, wb) in tb {
                    match g.iter_mut().find(|t| t.0 == i) {
                        Some(t) => t.1 -= wb,
                        None => g.push((i, -wb)),
                    }
                }
                g.retain(|t| t.1.abs() > 1e-6);
                if g.is_empty() {
                    continue;
                }
                let taps = g
                    .into_iter()
                    .map(|(i, wgt)| {
                        let next = texels.len() as u32;
                        let local = *index_of.entry(i).or_insert(next);
                        if local == next {
                            texels.push(i);
                        }
                        (local, wgt)
                    })
                    .collect();
                rows.push(Row { taps });
                used = true;
            }
            stats.edges += usize::from(used);
        }
    }
    stats.samples = rows.len();
    if rows.is_empty() {
        return stats;
    }
    let m = texels.len();
    // Jacobi preconditioner: the diagonal of Σ g gᵀ + λ I.
    let mut diag = vec![STITCH_REGULARIZATION; m];
    for r in &rows {
        for &(i, wgt) in &r.taps {
            diag[i as usize] += wgt * wgt;
        }
    }
    let apply = |x: &[f64], out: &mut [f64]| {
        for (o, v) in out.iter_mut().zip(x) {
            *o = STITCH_REGULARIZATION * v;
        }
        for r in &rows {
            let dot: f64 = r.taps.iter().map(|&(i, wgt)| wgt * x[i as usize]).sum();
            for &(i, wgt) in &r.taps {
                out[i as usize] += wgt * dot;
            }
        }
    };
    // One solve per channel, then every changed texel written back.
    let solved: Vec<Vec<f64>> = (0..N)
        .map(|c| {
            let x0: Vec<f64> = texels.iter().map(|&t| f64::from(values[t][c])).collect();
            let b: Vec<f64> = x0.iter().map(|v| STITCH_REGULARIZATION * v).collect();
            conjugate_gradients(&apply, &b, &x0, &diag)
        })
        .collect();
    let mut changed = vec![false; m];
    for (k, &t) in texels.iter().enumerate() {
        for (c, (slot, x)) in values[t].iter_mut().zip(&solved).enumerate() {
            let mut v = x[k] as f32;
            if c < 3 {
                v = v.max(0.0);
            } else if c == 6 {
                v = v.clamp(0.0, 1.0);
            }
            if v.is_finite() && v != *slot {
                *slot = v;
                changed[k] = true;
            }
        }
    }
    stats.texels = changed.iter().filter(|&&c| c).count();
    stats
}

/// Clamps one solved channel the way `stitch` writes a packed texel back: irradiance channels
/// `0..3` at 0, the direction agreement (packed channel 6) to `0..=1`. Other channels, including
/// the direction's `xyz`, are left for the caller (the direction encoder clamps `xyz` to `-1..=1`).
#[must_use]
pub(super) fn clamp_stitched_channel(channel: usize, value: f32) -> f32 {
    if channel < 3 {
        value.max(0.0)
    } else if channel == 6 {
        value.clamp(0.0, 1.0)
    } else {
        value
    }
}

/// Clamps one solved direction component. `component` 0..3 is `xyz` (not clamped here). `component`
/// 3 is the agreement, clamped to `0..=1` (`clamp_stitched_channel` on packed channel 6).
#[must_use]
pub(super) fn clamp_direction_component(component: usize, value: f32) -> f32 {
    clamp_stitched_channel(component.saturating_add(3), value)
}

/// Solves the chart stitch's normal equations `(Σ g gᵀ + λ I) x = λ x₀` once per channel.
///
/// `rows` are the sample rows over local unknowns (the first side's bilinear weights minus the
/// second's), the same rows `stitch` builds. `x0[unknown][channel]` is the baked value. The
/// result is unclamped: callers apply `clamp_stitched_channel` or `clamp_direction_component`.
/// An empty `x0` solves nothing. A row index past `x0` is a caller bug.
#[must_use]
pub(super) fn solve_seam_channels<const N: usize>(
    rows: &[Vec<(u32, f64)>],
    x0: &[[f64; N]],
) -> Vec<[f64; N]> {
    let m = x0.len();
    if m == 0 {
        return Vec::new();
    }
    debug_assert!(rows.iter().flatten().all(|&(i, _)| (i as usize) < m));
    let mut diag = vec![STITCH_REGULARIZATION; m];
    for r in rows {
        for &(i, wgt) in r {
            diag[i as usize] += wgt * wgt;
        }
    }
    let apply = |x: &[f64], out: &mut [f64]| {
        for (o, v) in out.iter_mut().zip(x) {
            *o = STITCH_REGULARIZATION * v;
        }
        for r in rows {
            let dot: f64 = r.iter().map(|&(i, wgt)| wgt * x[i as usize]).sum();
            for &(i, wgt) in r {
                out[i as usize] += wgt * dot;
            }
        }
    };
    let solved: Vec<Vec<f64>> = (0..N)
        .map(|c| {
            let start: Vec<f64> = x0.iter().map(|v| v[c]).collect();
            let rhs: Vec<f64> = start.iter().map(|v| STITCH_REGULARIZATION * v).collect();
            conjugate_gradients(&apply, &rhs, &start, &diag)
        })
        .collect();
    (0..m)
        .map(|k| {
            let mut channel = [0.0; N];
            for (c, slot) in channel.iter_mut().enumerate() {
                *slot = solved[c][k];
            }
            channel
        })
        .collect()
}

/// Preconditioned conjugate gradients for `A x = b` from `x0` (`A` symmetric positive definite,
/// applied by `apply`; `diag` its diagonal).
pub(super) fn conjugate_gradients(
    apply: &impl Fn(&[f64], &mut [f64]),
    b: &[f64],
    x0: &[f64],
    diag: &[f64],
) -> Vec<f64> {
    let m = b.len();
    let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f64>();
    let mut x = x0.to_vec();
    let mut ax = vec![0.0; m];
    apply(&x, &mut ax);
    let mut r: Vec<f64> = b.iter().zip(&ax).map(|(b, a)| b - a).collect();
    let b_norm = dot(b, b).sqrt().max(1e-30);
    let mut z: Vec<f64> = r.iter().zip(diag).map(|(r, d)| r / d).collect();
    let mut p = z.clone();
    let mut rz = dot(&r, &z);
    let mut ap = vec![0.0; m];
    for _ in 0..MAX_CG_ITERATIONS {
        if dot(&r, &r).sqrt() <= CG_TOLERANCE * b_norm {
            break;
        }
        apply(&p, &mut ap);
        let pap = dot(&p, &ap);
        if pap <= 0.0 || !pap.is_finite() {
            break;
        }
        let alpha = rz / pap;
        for (xk, pk) in x.iter_mut().zip(&p) {
            *xk += alpha * pk;
        }
        for (rk, apk) in r.iter_mut().zip(&ap) {
            *rk -= alpha * apk;
        }
        for ((zk, rk), dk) in z.iter_mut().zip(&r).zip(diag) {
            *zk = rk / dk;
        }
        let rz_next = dot(&r, &z);
        let beta = rz_next / rz.max(1e-300);
        rz = rz_next;
        for (pk, zk) in p.iter_mut().zip(&z) {
            *pk = zk + beta * *pk;
        }
    }
    x
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::lightmap::{LightmapEntry, MeshUnwrap};

    const W: usize = 14;
    const H: usize = 4;

    /// Two unit quads side by side along x, sharing the edge x = 1 (`fold_deg` turns the second
    /// one up about it), each its own chart in a 14 × 4 atlas: the shared edge runs through texel
    /// column 5 on the first chart and column 8 on the second.
    fn two_quads(fold_deg: f32) -> (Vec<[f32; 3]>, LightmapLayout) {
        let (s, c) = fold_deg.to_radians().sin_cos();
        let positions = vec![
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, 0.0, 1.0],
            [0.0, 0.0, 1.0],
            [1.0, 0.0, 0.0],
            [1.0 + c, s, 0.0],
            [1.0 + c, s, 1.0],
            [1.0, 0.0, 1.0],
        ];
        let t = |x: f32, y: f32| [x / W as f32, y / H as f32];
        let uv = vec![
            t(1.5, 0.5),
            t(5.5, 0.5),
            t(5.5, 3.5),
            t(1.5, 3.5),
            t(8.5, 0.5),
            t(12.5, 0.5),
            t(12.5, 3.5),
            t(8.5, 3.5),
        ];
        let layout = LightmapLayout {
            width: W as u32,
            height: H as u32,
            density: 4.0,
            padding: 1,
            entries: vec![LightmapEntry {
                name: "quads".into(),
                source_vertices: 8,
                rect: [0, 0, W as u32, H as u32],
                scale_offset: [1.0, 1.0, 0.0, 0.0],
                vertex_count: 8,
                index_count: 12,
                first_vertex: 0,
                first_index: 0,
                charts: 2,
                area_m2: 2.0,
                texels_per_metre: 4.0,
                key: None,
                seams: None,
                page: 0,
            }],
            unwraps: vec![MeshUnwrap {
                xref: (0..8).collect(),
                uv,
                indices: vec![0, 2, 1, 0, 3, 2, 4, 6, 5, 4, 7, 6],
            }],
        };
        (positions, layout)
    }

    /// Chart A (columns 1-5) holds 1, chart B (columns 8-12) holds 2; the rest is empty.
    fn texels() -> (Vec<[f32; 3]>, Vec<u8>) {
        let mut v = vec![[0.0f32; 3]; W * H];
        let mut cov = vec![Coverage::EMPTY; W * H];
        for y in 0..H {
            for x in 1..=5 {
                v[y * W + x] = [1.0; 3];
                cov[y * W + x] = Coverage::BAKED;
            }
            for x in 8..=12 {
                v[y * W + x] = [2.0; 3];
                cov[y * W + x] = Coverage::BAKED;
            }
        }
        (v, cov)
    }

    fn lookup(v: &[[f32; 3]], cov: &[u8], uv: [f32; 2]) -> f64 {
        bilinear_taps(uv, cov, W, H, H, 0)
            .iter()
            .map(|&(i, w)| w * f64::from(v[i][0]))
            .sum()
    }

    #[test]
    fn a_flat_seam_is_found_once_and_a_fold_is_a_crease() {
        let (p, layout) = two_quads(0.0);
        let u = &layout.unwraps[0];
        let seams = seam_edges(&p, &u.xref, &u.uv, &u.indices, DEFAULT_HARD_EDGE_DEG);
        assert_eq!(seams.len(), 1, "{seams:?}");
        // Columns 5 and 8 meet there.
        assert!((seams[0].a[0][0] * W as f32 - 5.5).abs() < 1e-4, "{seams:?}");
        assert!((seams[0].b[0][0] * W as f32 - 8.5).abs() < 1e-4, "{seams:?}");
        let (p, layout) = two_quads(90.0);
        let u = &layout.unwraps[0];
        assert!(seam_edges(&p, &u.xref, &u.uv, &u.indices, DEFAULT_HARD_EDGE_DEG).is_empty());
        // A mild bend is still a seam.
        let (p, layout) = two_quads(20.0);
        let u = &layout.unwraps[0];
        assert_eq!(
            seam_edges(&p, &u.xref, &u.uv, &u.indices, DEFAULT_HARD_EDGE_DEG).len(),
            1
        );
    }

    /// Where two closed blocks meet in one plane (a wall built as a grid of blocks around an
    /// opening), four faces share the edge: the two coplanar faces and the two blocks' inner
    /// faces, which come first in triangle order. The coplanar pair is still the seam (ADR-4108;
    /// comparing only the first side with the rest missed it).
    #[test]
    fn a_seam_shared_with_inner_faces_is_found() {
        let (mut p, layout) = two_quads(0.0);
        let u = &layout.unwraps[0];
        // The seam's two source corners (the quads lie in y = 0), and two points under them.
        let (sa, sb) = (u.xref[u.indices[1] as usize], u.xref[u.indices[2] as usize]);
        let (pa, pb) = (p[sa as usize], p[sb as usize]);
        let back = |q: [f32; 3]| [q[0], q[1] - 1.0, q[2]];
        p.push(back(pa));
        p.push(back(pb));
        let (ba, bb) = (p.len() as u32 - 2, p.len() as u32 - 1);
        let mut xref = u.xref.clone();
        let mut uv = u.uv.clone();
        let first_new = xref.len() as u32;
        // Two back-to-back inner triangles on the seam edge, with UV2 of their own.
        xref.extend_from_slice(&[sa, sb, ba, sb, sa, bb]);
        uv.extend_from_slice(&[
            [0.1, 0.1],
            [0.1, 0.2],
            [0.2, 0.1],
            [0.3, 0.2],
            [0.3, 0.1],
            [0.4, 0.2],
        ]);
        let mut indices: Vec<u32> = (first_new..first_new + 6).collect();
        indices.extend_from_slice(&u.indices);
        let seams = seam_edges(&p, &xref, &uv, &indices, DEFAULT_HARD_EDGE_DEG);
        assert_eq!(seams.len(), 1, "{seams:?}");
        assert!((seams[0].a[0][0] * W as f32 - 5.5).abs() < 1e-4, "{seams:?}");
        assert!((seams[0].b[0][0] * W as f32 - 8.5).abs() < 1e-4, "{seams:?}");
    }

    /// Pins the single-atlas stitch: a later cell-seam change must not move these texels.
    #[test]
    fn the_stitch_makes_both_sides_agree_and_leaves_the_rest() {
        let (p, layout) = two_quads(0.0);
        let (mut v, cov) = texels();
        let before = lookup(&v, &cov, [5.5 / W as f32, 2.0 / H as f32])
            - lookup(&v, &cov, [8.5 / W as f32, 2.0 / H as f32]);
        assert!((before.abs() - 1.0).abs() < 1e-9);
        let stats = stitch(&mut v, &cov, &layout, &[&p], W, H, H);
        assert_eq!(stats.edges, 1, "{stats:?}");
        assert!(stats.samples >= 6, "{stats:?}");
        assert_eq!(stats.texels, 8, "columns 5 and 8, four rows each: {stats:?}");
        for k in 0..8 {
            let y = (k as f32 + 0.5) / 8.0 * 3.0 + 0.5;
            let a = lookup(&v, &cov, [5.5 / W as f32, y / H as f32]);
            let b = lookup(&v, &cov, [8.5 / W as f32, y / H as f32]);
            assert!((a - b).abs() < 0.05, "row {y}: {a} vs {b}");
            // Toward the mean of the two sides, not to one of them.
            assert!((a - 1.5).abs() < 0.1, "{a}");
        }
        for y in 0..H {
            assert_eq!(v[y * W + 1], [1.0; 3]);
            assert_eq!(v[y * W + 12], [2.0; 3]);
        }
    }

    #[test]
    fn a_fold_and_an_entry_without_geometry_are_left_alone() {
        let (p, layout) = two_quads(90.0);
        let (mut v, cov) = texels();
        let want = v.clone();
        assert_eq!(
            stitch(&mut v, &cov, &layout, &[&p], W, H, H),
            StitchStats::default()
        );
        assert_eq!(v, want);
        assert_eq!(
            stitch(&mut v, &cov, &layout, &[], W, H, H),
            StitchStats::default()
        );
        let short: [[f32; 3]; 2] = [[0.0; 3]; 2];
        assert_eq!(
            stitch(&mut v, &cov, &layout, &[&short], W, H, H),
            StitchStats::default()
        );
        assert_eq!(v, want);
    }

    #[test]
    fn the_solver_reaches_the_least_squares_answer() {
        // One row x0 - x1 = 0 with λ: x = x0 + (mean - x0)·(2/(2 + λ)) on each side.
        let rows = [Row {
            taps: vec![(0, 1.0), (1, -1.0)],
        }];
        let lambda = STITCH_REGULARIZATION;
        let apply = |x: &[f64], out: &mut [f64]| {
            for (o, v) in out.iter_mut().zip(x) {
                *o = lambda * v;
            }
            for r in &rows {
                let dot: f64 = r.taps.iter().map(|&(i, w)| w * x[i as usize]).sum();
                for &(i, w) in &r.taps {
                    out[i as usize] += w * dot;
                }
            }
        };
        let x0 = [1.0, 3.0];
        let b = x0.map(|v| lambda * v);
        let x = conjugate_gradients(&apply, &b, &x0, &[lambda + 1.0, lambda + 1.0]);
        let pull = 2.0 / (2.0 + lambda);
        assert!((x[0] - (1.0 + pull)).abs() < 1e-6, "{x:?}");
        assert!((x[1] - (3.0 - pull)).abs() < 1e-6, "{x:?}");
    }

    /// A sample on the last row of a stacked page mixes halfway into the next row. That
    /// row is the next page. The runtime sampler (`Lightmap::sample_page`) stays inside
    /// the page; padding 0 puts a seam on this row.
    #[test]
    fn a_page_edge_sample_does_not_read_the_next_page() {
        let page = 4usize;
        let w = 2usize;
        let h = page * 2;
        let mut cov = vec![Coverage::EMPTY; w * h];
        let last = (page - 1) * w;
        let next = page * w;
        cov[last] = Coverage::BAKED;
        cov[next] = Coverage::BAKED;
        let uv = [0.5 / w as f32, page as f32 / h as f32];
        let taps = bilinear_taps(uv, &cov, w, h, page, 0);
        assert!(
            taps.iter().all(|&(i, _)| i != next),
            "read the next page's row: {taps:?}"
        );
        assert!(
            taps.iter().any(|&(i, _)| i == last),
            "missed this page's last row: {taps:?}"
        );
    }

    /// The cell seam's direction helper is the same solve, and the agreement clamp is `0..=1`.
    #[test]
    fn the_channel_helper_matches_the_chart_solve_and_clamps_agreement() {
        let rows = vec![vec![(0u32, 1.0), (1, -1.0)]];
        let x0 = [[1.0, -0.4], [3.0, 1.5]];
        let solved = solve_seam_channels(&rows, &x0);
        let pull = 2.0 / (2.0 + STITCH_REGULARIZATION);
        assert!((solved[0][0] - (1.0 + pull)).abs() < 1e-6, "{solved:?}");
        assert!((solved[1][0] - (3.0 - pull)).abs() < 1e-6, "{solved:?}");
        assert_eq!(clamp_stitched_channel(0, -1.0), 0.0);
        assert_eq!(clamp_stitched_channel(2, 0.4), 0.4);
        assert_eq!(clamp_direction_component(0, -1.5), -1.5);
        assert_eq!(clamp_direction_component(3, -0.2), 0.0);
        assert_eq!(clamp_direction_component(3, 1.4), 1.0);
        assert_eq!(clamp_direction_component(3, 0.4), 0.4);
        assert!(solve_seam_channels::<4>(&[], &[]).is_empty());
    }

    /// A sample on the top half-texel of a page belongs to that page's entry: it clamps to the
    /// page's first row, as `Lightmap::sample_page` does with the entry's page, and never reads
    /// the last row of the page before (another entry's texels).
    #[test]
    fn a_page_top_edge_sample_reads_its_own_page() {
        let page = 4usize;
        let w = 2usize;
        let h = page * 2;
        let mut cov = vec![Coverage::EMPTY; w * h];
        let prev_last = (page - 1) * w;
        let first = page * w;
        cov[prev_last] = Coverage::BAKED;
        cov[first] = Coverage::BAKED;
        let uv = [0.5 / w as f32, 4.25 / h as f32];
        let taps = bilinear_taps(uv, &cov, w, h, page, 1);
        assert!(
            taps.iter().all(|&(i, _)| i != prev_last),
            "read the previous page's last row: {taps:?}"
        );
        assert!(
            taps.iter().any(|&(i, _)| i == first),
            "missed this page's first row: {taps:?}"
        );
    }

    /// End to end on a two-page stacked atlas: the quads entry sits at the top of page 1 with its
    /// seam starting on the page's first row, and another entry's texels fill page 0's last row.
    /// The stitch must constrain page 1's columns only and leave page 0 alone (the old page
    /// inference sent the top samples to page 0 and pulled its two texels together).
    #[test]
    fn a_seam_on_a_page_top_never_rewrites_the_page_before() {
        let (p, mut layout) = two_quads(0.0);
        for (k, uv) in layout.unwraps[0].uv.iter_mut().enumerate() {
            // The quads' top vertices (0, 1, 4, 5) move to the entry's first texel edge.
            if matches!(k, 0 | 1 | 4 | 5) {
                uv[1] = 0.0;
            }
        }
        let h = 2 * H;
        layout.height = h as u32;
        layout.entries[0].rect = [0, H as u32, W as u32, H as u32];
        layout.entries[0].scale_offset = [1.0, 0.5, 0.0, 0.5];
        let mut v = vec![[0.0f32; 3]; W * h];
        let mut cov = vec![Coverage::EMPTY; W * h];
        for y in H..h {
            for x in 1..=5 {
                v[y * W + x] = [1.0; 3];
                cov[y * W + x] = Coverage::BAKED;
            }
            for x in 8..=12 {
                v[y * W + x] = [2.0; 3];
                cov[y * W + x] = Coverage::BAKED;
            }
        }
        let row = (H - 1) * W;
        for x in 1..=12 {
            v[row + x] = [if x <= 6 { 3.0 } else { 9.0 }; 3];
            cov[row + x] = Coverage::BAKED;
        }
        let page0 = v[..W * H].to_vec();
        let stats = stitch(&mut v, &cov, &layout, &[&p], W, h, H);
        assert_eq!(stats.edges, 1, "{stats:?}");
        assert_eq!(&v[..W * H], page0.as_slice(), "page 0 was rewritten: {stats:?}");
        assert_eq!(
            stats.texels, 8,
            "columns 5 and 8 of page 1, four rows each: {stats:?}"
        );
    }
}
