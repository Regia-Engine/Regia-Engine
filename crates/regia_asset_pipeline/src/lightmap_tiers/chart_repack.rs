// SPDX-License-Identifier: MPL-2.0
//! Target-resolution chart repack of a derived tier (ADR-5125).
//!
//! The bake leaves `padding` gutter texels on every side of each chart. At a tier `factor` times
//! coarser that gutter is `padding / factor` tier texels; under one whole texel the charts of an
//! entry abut, no gutter texel is left to hold either side's continuation, and a lookup at a chart
//! border reads its neighbour's light (ULTRA_LOW from a 2-texel bake measured a seam p95 of 1.065).
//! A chart here is one of the baker's: the connected pieces of the unwrap (`triangle_charts`) whose
//! boxes overlap or touch are one baked chart (a chart whose vertices are split along an edge is
//! several pieces, but the bake packed them touching, so that edge is interior and must stay so)
//! and move together. Pieces with any gap between their boxes, however small, are separate charts
//! (ADR-5621): the baker keeps two charts at least twice its padding apart, but an imported UV2
//! may pack its charts closer, and grouping those would keep their sub-texel gap at the tier, the
//! seam the repack exists to fix. Charts whose boxes overlap (an L-shaped chart around another)
//! still move as one. Each chart of the entry keeps its size at the tier (the bake's UV2 scaled by the factor,
//! "native" tier texels) and is moved by a whole number of tier texels to a new place in the
//! entry, packed as the baker packs charts (`lightmap_uv::pack_chart_rects`) with `padding` gutter
//! texels on every side at the tier's own resolution. Only UV2 changes, and only by a per-chart
//! translation: the unwrap's vertices, `xref` and indices are the bake's, so meshlets built over the
//! unwrap and the receiver mesh stay valid, and a tier texel still covers exactly the bake texels
//! it covered before the move (the resample reads them through the inverse translation).

use regia_asset::lightmap::MeshUnwrap;

/// One entry's charts repacked at the tier.
pub(super) struct EntryRepack {
    /// The entry's rectangle at the tier, texels.
    pub size: [u32; 2],
    /// Per connected piece: its tier texel is its native tier texel plus this.
    pub shift: Vec<[i64; 2]>,
    /// Baked charts moved (pieces grouped as the bake packed them).
    pub charts: u64,
    /// The repacked unwrap: the bake's vertices and indices, each chart's UV2 translated.
    pub unwrap: MeshUnwrap,
}

/// Repacks the `pieces` connected pieces of `unwrap` (`tri_chart` per triangle), grouped into the
/// bake's charts (`baked`: the entry's bake size, texels), whose native size
/// at the tier is `native` texels, with `padding` gutter texels on every side (module docs).
pub(super) fn repack_entry(
    unwrap: &MeshUnwrap,
    tri_chart: &[u32],
    pieces: u32,
    native: [u32; 2],
    baked: [u32; 2],
    padding: u32,
) -> EntryRepack {
    let pieces = pieces as usize;
    let (nw, nh) = (f64::from(native[0].max(1)), f64::from(native[1].max(1)));
    let mut vertex_chart = vec![u32::MAX; unwrap.uv.len()];
    for (t, &c) in unwrap.indices.as_chunks::<3>().0.iter().zip(tri_chart) {
        for &v in t {
            if let Some(slot) = vertex_chart.get_mut(v as usize) {
                *slot = c;
            }
        }
    }
    let mut lo = vec![[f64::INFINITY; 2]; pieces];
    let mut hi = vec![[f64::NEG_INFINITY; 2]; pieces];
    for (uv, &c) in unwrap.uv.iter().zip(&vertex_chart) {
        let (Some(l), Some(h)) = (lo.get_mut(c as usize), hi.get_mut(c as usize)) else {
            continue;
        };
        let p = [f64::from(uv[0]) * nw, f64::from(uv[1]) * nh];
        *l = [l[0].min(p[0]), l[1].min(p[1])];
        *h = [h[0].max(p[0]), h[1].max(p[1])];
    }
    let group = bake_charts(&lo, &hi, [nw, nh], baked);
    let (mut glo, mut ghi) = (lo.clone(), hi.clone());
    for (c, &g) in group.iter().enumerate() {
        glo[g] = [glo[g][0].min(lo[c][0]), glo[g][1].min(lo[c][1])];
        ghi[g] = [ghi[g][0].max(hi[c][0]), ghi[g][1].max(hi[c][1])];
    }
    // Each baked chart's box in whole native texels: the texels its triangles can cover. Only the
    // groups' roots are packed; every piece moves with its root.
    let roots: Vec<usize> = (0..pieces).filter(|&c| group[c] == c).collect();
    let boxes: Vec<([i64; 2], [u32; 2])> = roots
        .iter()
        .map(|&g| {
            let (l, h) = (glo[g], ghi[g]);
            if !(l[0].is_finite() && l[1].is_finite()) {
                return ([0, 0], [1, 1]);
            }
            let x0 = l[0].floor() as i64;
            let y0 = l[1].floor() as i64;
            let x1 = (h[0].ceil() as i64).max(x0 + 1);
            let y1 = (h[1].ceil() as i64).max(y0 + 1);
            ([x0, y0], [(x1 - x0) as u32, (y1 - y0) as u32])
        })
        .collect();
    let sizes: Vec<[u32; 2]> = boxes
        .iter()
        .map(|(_, s)| [s[0] + 2 * padding, s[1] + 2 * padding])
        .collect();
    let (size, at) = crate::lightmap_uv::pack_chart_rects(&sizes);
    let mut root_shift = vec![[0i64; 2]; pieces];
    for ((&g, (o, _)), a) in roots.iter().zip(&boxes).zip(&at) {
        root_shift[g] = [i64::from(a[0] + padding) - o[0], i64::from(a[1] + padding) - o[1]];
    }
    let shift: Vec<[i64; 2]> = group.iter().map(|&g| root_shift[g]).collect();
    let (w, h) = (f64::from(size[0]), f64::from(size[1]));
    let uv = unwrap
        .uv
        .iter()
        .zip(&vertex_chart)
        .map(|(uv, &c)| {
            let s = shift.get(c as usize).copied().unwrap_or([0, 0]);
            [
                ((f64::from(uv[0]) * nw + s[0] as f64) / w) as f32,
                ((f64::from(uv[1]) * nh + s[1] as f64) / h) as f32,
            ]
        })
        .collect();
    EntryRepack {
        size,
        shift,
        charts: roots.len() as u64,
        unwrap: MeshUnwrap {
            xref: unwrap.xref.clone(),
            uv,
            indices: unwrap.indices.clone(),
        },
    }
}

/// How far apart, in bake texels, two pieces' boxes may be on an axis and still touch: the float
/// error of scaling a shared UV2 coordinate, far under any gap a packer leaves.
const TOUCH_TOLERANCE: f64 = 1e-3;

/// The baked chart of every piece (its group's smallest piece): pieces whose native boxes `lo..hi`
/// overlap or touch (within [`TOUCH_TOLERANCE`] bake texels on both axes, module docs). Pieces
/// without vertices stay alone.
fn bake_charts(lo: &[[f64; 2]], hi: &[[f64; 2]], native: [f64; 2], [bw, bh]: [u32; 2]) -> Vec<usize> {
    fn root(group: &mut [usize], mut g: usize) -> usize {
        while group[g] != g {
            group[g] = group[group[g]];
            g = group[g];
        }
        g
    }
    let to_bake = [f64::from(bw) / native[0], f64::from(bh) / native[1]];
    let mut group: Vec<usize> = (0..lo.len()).collect();
    for a in 0..lo.len() {
        if !lo[a][0].is_finite() {
            continue;
        }
        for b in a + 1..lo.len() {
            if !lo[b][0].is_finite() {
                continue;
            }
            let gap = |k: usize| ((lo[b][k] - hi[a][k]).max(lo[a][k] - hi[b][k]) * to_bake[k]).max(0.0);
            if gap(0) <= TOUCH_TOLERANCE && gap(1) <= TOUCH_TOLERANCE {
                let (ra, rb) = (root(&mut group, a), root(&mut group, b));
                group[ra.max(rb)] = ra.min(rb);
            }
        }
    }
    (0..lo.len()).map(|c| root(&mut group, c)).collect()
}
