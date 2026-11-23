// SPDX-License-Identifier: MPL-2.0
//! The mouth cut on the ground lattice (spec 121 § 3 "Mouth continuity", spec 26 § 2a.4 item 2;
//! lane W-10, ADR-8214). Cook-only, integer end to end.
//!
//! - **Mouths are named holes.** A `Passages` layer's mouths are its layout's openings: one named
//!   hole per opening (id = the opening id), whose footprint is the union of one quadrilateral per
//!   open-cut ramp segment. A segment's quad joins the two cross-sections at its samples, each
//!   `width / 2 + LIP_W_M - LIP_OVERLAP_M` to each side along the sample's own left normal, so
//!   consecutive quads share their cross-section edge and a bend leaves no wedge. The top (mouth)
//!   segment runs `HOLE_END_M` past the ramp top, as the Stage A hole rect does. No hero hole, no
//!   one rectangle per ramp.
//! - **The cut** is the set of lattice triangles whose centroid is inside a mouth's footprint (an
//!   even-odd crossing test in `i64`, centroids scaled by 3 so they stay integer).
//! - **The rings** are the cut's boundary: closed loops of directed lattice edges (diagonals
//!   included), the cut on their left as the triangles wind. Where a mouth's own cut touches itself
//!   at a vertex (a pinch a footprint edge leaves at some slopes), the cut takes the six triangles
//!   round that vertex, to a fixed point; two mouths whose cuts touch at a vertex only are refused
//!   (`mouth_not_simple`).
//! - **Ring pins.** Every ring vertex gets `ring_mm = m_to_mm(offset + count * step)`.
//! - **The collar.** Every cut lattice triangle with a ring edge becomes a canonical floor triangle
//!   at `(x_mm(i), height, z_mm(j))`, ring vertices at their pin and any other vertex at
//!   `m_to_mm` of its count height. Each ring edge is then an edge of exactly one canonical triangle
//!   and of exactly one uncut lattice triangle, with equal integer endpoints. The zipper from the
//!   collar's inner side to the authored mesh is lane W-14's (spec 26 § 2a.4 item 3).

use std::collections::{BTreeMap, BTreeSet};

use crate::compat::layout::{HOLE_END_M, LIP_OVERLAP_M, LIP_W_M, PassageKind, SampledLayout, TunnelLayout};
use crate::cooked::{CutBitmap, LatticeConstants, TriKind, VertexHeightMm};
use crate::ground::{CellTriangle, cut_bit_index};
use crate::units::m_to_mm;

/// One mouth: a named hole and its footprint, world millimetres.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MouthFootprint {
    /// The named hole id (the opening id).
    pub id: String,
    /// One quadrilateral per open-cut ramp segment, `[x_mm, z_mm]` corners in order.
    pub quads: Vec<[[i64; 2]; 4]>,
}

/// A lattice triangle: cell `(i, j)` and which of its two triangles.
pub type LatticeTri = (u32, u32, CellTriangle);

/// A lattice vertex `(i, j)`.
pub type LatticeVertex = (u32, u32);

/// The cut of one mouth.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MouthCut {
    /// The named hole id.
    pub id: String,
    /// The cut lattice triangles, ascending `(j, i, t)` order of [`tri_key`].
    pub cut: BTreeSet<(u32, u32, CellTriangle)>,
    /// The ring loops, each a closed list of lattice vertices (the first is not repeated), starting
    /// at its smallest `(j, i)` vertex.
    pub rings: Vec<Vec<LatticeVertex>>,
}

/// Why the mouth cut failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CutError {
    /// (`mouth_not_simple`) a ring loop touches itself at a lattice vertex.
    #[error("mouth `{mouth}`: its ring touches itself at lattice vertex ({i}, {j})")]
    NotSimple {
        /// The mouth.
        mouth: String,
        /// Vertex column.
        i: u32,
        /// Vertex row.
        j: u32,
    },
    /// (`mouth_off_map`) a mouth's cut reaches the map edge, so its ring would not be closed by
    /// lattice edges inside the map.
    #[error("mouth `{0}` reaches the map edge")]
    OffMap(String),
}

impl CutError {
    /// The agent error code.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::NotSimple { .. } => "mouth_not_simple",
            Self::OffMap(_) => "mouth_off_map",
        }
    }
}

/// `(j, i, t)`: the ascending order the cut is stored and walked in.
#[must_use]
pub fn tri_key(t: LatticeTri) -> (u32, u32, CellTriangle) {
    (t.1, t.0, t.2)
}

fn mm(v: f64) -> i64 {
    (v * 1000.0).round_ties_even() as i64
}

/// The mouths of a sampled `Passages` layout: one per opening whose ramp has an open-cut segment.
#[must_use]
pub fn passage_mouths(layout: &TunnelLayout, sampled: &SampledLayout) -> Vec<MouthFootprint> {
    let mut out = Vec::new();
    for o in &layout.openings {
        let Some(p) = sampled
            .passages
            .iter()
            .find(|p| p.id == o.ramp && p.kind == PassageKind::Ramp)
        else {
            continue;
        };
        let hw = f64::from(p.width_m) * 0.5 + f64::from(LIP_W_M) - f64::from(LIP_OVERLAP_M);
        let nseg = p.samples.len().saturating_sub(1);
        let mut quads = Vec::new();
        for k in 0..nseg {
            if p.covered.get(k).copied().unwrap_or(true) {
                continue;
            }
            let (a, b) = (&p.samples[k], &p.samples[k + 1]);
            let (ax, az) = (f64::from(a.p[0]), f64::from(a.p[2]));
            let (mut bx, mut bz) = (f64::from(b.p[0]), f64::from(b.p[2]));
            if k + 1 == nseg {
                // The mouth end runs past the ramp top, as the Stage A hole does.
                let (dx, dz) = (bx - ax, bz - az);
                let len =
                    regia_detsim::float::det_max_f64(regia_detsim::math::sqrt_f64(dx * dx + dz * dz), 1e-3);
                bx += dx / len * f64::from(HOLE_END_M);
                bz += dz / len * f64::from(HOLE_END_M);
            }
            let side = |x: f64, z: f64, left: [f32; 2], sign: f64| -> [i64; 2] {
                [
                    mm(x + f64::from(left[0]) * hw * sign),
                    mm(z + f64::from(left[1]) * hw * sign),
                ]
            };
            quads.push([
                side(ax, az, a.left, 1.0),
                side(ax, az, a.left, -1.0),
                side(bx, bz, b.left, -1.0),
                side(bx, bz, b.left, 1.0),
            ]);
        }
        if !quads.is_empty() {
            out.push(MouthFootprint {
                id: o.id.clone(),
                quads,
            });
        }
    }
    out
}

/// Even-odd crossing test of `p` (scaled by 3) against `poly` (scaled by 3 inside), all `i64`/`i128`.
fn inside3(poly: &[[i64; 2]], p: [i64; 2]) -> bool {
    let mut inside = false;
    let n = poly.len();
    for k in 0..n {
        let a = [poly[k][0] * 3, poly[k][1] * 3];
        let b = [poly[(k + 1) % n][0] * 3, poly[(k + 1) % n][1] * 3];
        if (a[1] > p[1]) != (b[1] > p[1]) {
            // x of the edge at p.z, compared without division: (p.x - a.x) * (b.z - a.z) vs
            // (b.x - a.x) * (p.z - a.z), with the sign of (b.z - a.z).
            let lhs = i128::from(p[0] - a[0]) * i128::from(b[1] - a[1]);
            let rhs = i128::from(b[0] - a[0]) * i128::from(p[1] - a[1]);
            let crosses = if b[1] > a[1] { lhs < rhs } else { lhs > rhs };
            if crosses {
                inside = !inside;
            }
        }
    }
    inside
}

/// The centroid of lattice triangle `t` of cell `(i, j)`, world millimetres times 3.
#[must_use]
pub fn centroid3(lattice: &LatticeConstants, i: u32, j: u32, t: CellTriangle) -> [i64; 2] {
    let (x0, x1) = (lattice.vertex_mm(i), lattice.vertex_mm(i + 1));
    let (z0, z1) = (lattice.vertex_mm(j), lattice.vertex_mm(j + 1));
    if t == 0 {
        [x0 + x0 + x1, z0 + z1 + z0]
    } else {
        [x1 + x0 + x1, z0 + z1 + z1]
    }
}

/// The three vertices of lattice triangle `t` of cell `(i, j)` in winding order:
/// `[v00, v01, v10]` or `[v10, v01, v11]`.
#[must_use]
pub fn tri_vertices(i: u32, j: u32, t: CellTriangle) -> [LatticeVertex; 3] {
    if t == 0 {
        [(i, j), (i, j + 1), (i + 1, j)]
    } else {
        [(i + 1, j), (i, j + 1), (i + 1, j + 1)]
    }
}

/// The lattice triangle across edge `e` (0, 1, 2 in winding order) of triangle `t` of cell
/// `(i, j)`, or `None` off the map.
#[must_use]
pub fn neighbor(cells: u32, i: u32, j: u32, t: CellTriangle, e: u8) -> Option<LatticeTri> {
    match (t, e) {
        // [v00, v01, v10]: west edge, diagonal, south edge.
        (0, 0) => (i > 0).then(|| (i - 1, j, 1)),
        (0, 1) => Some((i, j, 1)),
        (0, _) => (j > 0).then(|| (i, j - 1, 1)),
        // [v10, v01, v11]: diagonal, north edge, east edge.
        (_, 0) => Some((i, j, 0)),
        (_, 1) => (j + 1 < cells).then_some((i, j + 1, 0)),
        (_, _) => (i + 1 < cells).then_some((i + 1, j, 0)),
    }
}

/// Cuts the lattice under one mouth.
///
/// # Errors
///
/// [`CutError::OffMap`], [`CutError::NotSimple`].
pub fn cut_mouth(lattice: &LatticeConstants, mouth: &MouthFootprint) -> Result<MouthCut, CutError> {
    let cells = lattice.cells();
    let mut cut = BTreeSet::new();
    for quad in &mouth.quads {
        let (mut lo, mut hi) = ([i64::MAX; 2], [i64::MIN; 2]);
        for c in quad {
            lo = [lo[0].min(c[0]), lo[1].min(c[1])];
            hi = [hi[0].max(c[0]), hi[1].max(c[1])];
        }
        let range = |lo: i64, hi: i64| -> (u32, u32) {
            // The cells whose span overlaps [lo, hi]: a cheap superset, refined by the centroid test.
            let mut a = 0u32;
            while a < cells && lattice.vertex_mm(a + 1) < lo {
                a += 1;
            }
            let mut b = a;
            while b < cells && lattice.vertex_mm(b) <= hi {
                b += 1;
            }
            (a, b)
        };
        let (i0, i1) = range(lo[0], hi[0]);
        let (j0, j1) = range(lo[1], hi[1]);
        for j in j0..j1 {
            for i in i0..i1 {
                for t in 0..2u8 {
                    if inside3(quad, centroid3(lattice, i, j, t)) {
                        cut.insert((j, i, t));
                    }
                }
            }
        }
    }
    // A footprint edge at some slopes leaves two cut triangles touching at one vertex with ground
    // between them (a pinch). The cut takes every lattice triangle round such a vertex, to a fixed
    // point, so each mouth's ring is simple; only two mouths touching stay `mouth_not_simple`.
    while let Some(pinches) = pinch_vertices(cells, &cut).filter(|p| !p.is_empty()) {
        for (vi, vj) in pinches {
            for t in triangles_round(cells, vi, vj) {
                cut.insert(tri_key(t));
            }
        }
    }
    let rings = rings_of(cells, &mouth.id, &cut)?;
    Ok(MouthCut {
        id: mouth.id.clone(),
        cut,
        rings,
    })
}

/// Directed boundary edges keyed by their start vertex `(j, i)`, each with its end vertices `(j, i)`.
type BoundaryEdges = BTreeMap<(u32, u32), Vec<(u32, u32)>>;

/// The directed boundary edges of a cut set, keyed by their start vertex `(j, i)`; `None` when the
/// cut reaches the map edge.
fn boundary(cells: u32, cut: &BTreeSet<(u32, u32, CellTriangle)>) -> Option<BoundaryEdges> {
    let mut next = BoundaryEdges::new();
    for &(j, i, t) in cut {
        let v = tri_vertices(i, j, t);
        for e in 0..3u8 {
            let (ni, nj, nt) = neighbor(cells, i, j, t, e)?;
            if !cut.contains(&(nj, ni, nt)) {
                let (a, b) = (v[e as usize], v[(e as usize + 1) % 3]);
                next.entry((a.1, a.0)).or_default().push((b.1, b.0));
            }
        }
    }
    Some(next)
}

/// The vertices `(i, j)` where a cut's boundary touches itself; `None` when the cut reaches the map
/// edge.
fn pinch_vertices(cells: u32, cut: &BTreeSet<(u32, u32, CellTriangle)>) -> Option<Vec<LatticeVertex>> {
    Some(
        boundary(cells, cut)?
            .into_iter()
            .filter(|(_, outs)| outs.len() > 1)
            .map(|((j, i), _)| (i, j))
            .collect(),
    )
}

/// The six lattice triangles round vertex `(vi, vj)` that lie inside the map.
fn triangles_round(cells: u32, vi: u32, vj: u32) -> Vec<LatticeTri> {
    let mut out = Vec::with_capacity(6);
    let cell = |di: i64, dj: i64| -> Option<(u32, u32)> {
        let (i, j) = (i64::from(vi) + di, i64::from(vj) + dj);
        (i >= 0 && j >= 0 && i < i64::from(cells) && j < i64::from(cells)).then_some((i as u32, j as u32))
    };
    for (di, dj, ts) in [
        (0, 0, &[0u8][..]),
        (-1, 0, &[0, 1][..]),
        (0, -1, &[0, 1][..]),
        (-1, -1, &[1][..]),
    ] {
        if let Some((i, j)) = cell(di, dj) {
            out.extend(ts.iter().map(|&t| (i, j, t)));
        }
    }
    out
}

/// The ring loops of a cut set (keys `(j, i, t)`).
fn rings_of(
    cells: u32,
    mouth: &str,
    cut: &BTreeSet<(u32, u32, CellTriangle)>,
) -> Result<Vec<Vec<LatticeVertex>>, CutError> {
    // Keyed (j, i) so loops start at the smallest (j, i) vertex.
    let Some(next) = boundary(cells, cut) else {
        return Err(CutError::OffMap(mouth.into()));
    };
    for (&(j, i), outs) in &next {
        if outs.len() > 1 {
            return Err(CutError::NotSimple {
                mouth: mouth.into(),
                i,
                j,
            });
        }
    }
    let mut rings = Vec::new();
    let mut unused: BTreeMap<(u32, u32), (u32, u32)> = next.into_iter().map(|(k, v)| (k, v[0])).collect();
    while let Some((&start, _)) = unused.iter().next() {
        let mut ring = Vec::new();
        let mut at = start;
        while let Some(to) = unused.remove(&at) {
            ring.push((at.1, at.0));
            at = to;
        }
        rings.push(ring);
    }
    Ok(rings)
}

/// The whole cut of a map: every mouth's cut, the union, the ring pins and the collar.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LatticeCut {
    /// One entry per mouth, in the order given.
    pub mouths: Vec<MouthCut>,
    /// The union of the mouths' cuts, keys `(j, i, t)`.
    pub cut: BTreeSet<(u32, u32, CellTriangle)>,
    /// The union's ring loops (where mouths overlap, one ring goes round both).
    pub rings: Vec<Vec<LatticeVertex>>,
    /// Every ring vertex with its pinned height, ascending vertex index.
    pub ring_pins: Vec<VertexHeightMm>,
}

/// The count height of lattice vertex `(i, j)`, metres, by the spec 121 § 3 formula.
#[must_use]
pub fn count_height_m(lattice: &LatticeConstants, counts: &[u16], i: u32, j: u32) -> f32 {
    let n = lattice.lattice_n as usize;
    let c = counts[j as usize * n + i as usize];
    lattice.offset_m() + (c as f32) * lattice.step_m()
}

/// Cuts every mouth and pins the rings of the union.
///
/// # Errors
///
/// As [`cut_mouth`]; and [`CutError::NotSimple`] when two mouths' cuts touch at a vertex only.
pub fn cut_lattice(
    lattice: &LatticeConstants,
    counts: &[u16],
    mouths: &[MouthFootprint],
) -> Result<LatticeCut, CutError> {
    let mut out = LatticeCut::default();
    for m in mouths {
        let c = cut_mouth(lattice, m)?;
        out.cut.extend(c.cut.iter().copied());
        out.mouths.push(c);
    }
    out.rings = rings_of(lattice.cells(), "(all mouths)", &out.cut)?;
    let n = lattice.lattice_n;
    let mut pins: BTreeMap<u32, i32> = BTreeMap::new();
    for ring in &out.rings {
        for &(i, j) in ring {
            pins.insert(j * n + i, m_to_mm(count_height_m(lattice, counts, i, j)));
        }
    }
    out.ring_pins = pins
        .into_iter()
        .map(|(vertex, height_mm)| VertexHeightMm { vertex, height_mm })
        .collect();
    Ok(out)
}

/// The per-ground-tile cut bitmaps of a cut, ascending tile index.
#[must_use]
pub fn cut_bitmaps(lattice: &LatticeConstants, cut: &BTreeSet<(u32, u32, CellTriangle)>) -> Vec<CutBitmap> {
    let g = u32::from(lattice.ground_tile_cells);
    let per_side = lattice.tiles_per_side();
    let mut by_tile: BTreeMap<u32, Vec<u64>> = BTreeMap::new();
    for &(j, i, t) in cut {
        let (a, b) = (i / g, j / g);
        let tile = b * per_side + a;
        let (x0, x1) = lattice.tile_cells(a);
        let (z0, z1) = lattice.tile_cells(b);
        let words = (2 * u64::from(x1 - x0) * u64::from(z1 - z0)).div_ceil(64) as usize;
        let bits = by_tile.entry(tile).or_insert_with(|| vec![0; words]);
        let bit = cut_bit_index(x1 - x0, i - x0, j - z0, t);
        bits[(bit / 64) as usize] |= 1 << (bit % 64);
    }
    by_tile
        .into_iter()
        .map(|(tile, bits)| CutBitmap { tile, bits })
        .collect()
}

/// A collar triangle: world millimetre vertices.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CollarTriangle {
    /// The mouth (index into [`LatticeCut::mouths`]).
    pub mouth: usize,
    /// The vertices `[x_mm, y_mm, z_mm]` in the lattice triangle's winding.
    pub v: [[i32; 3]; 3],
    /// Its kind (always [`TriKind::Floor`]).
    pub kind: TriKind,
}

/// The collar of a cut: every cut lattice triangle with a ring edge, as a canonical floor triangle
/// (ring vertices at their pins, other vertices at `m_to_mm` of their count height), in ascending
/// `(j, i, t)` order, each assigned to the first mouth whose cut holds it.
#[must_use]
pub fn collar(lattice: &LatticeConstants, counts: &[u16], cut: &LatticeCut) -> Vec<CollarTriangle> {
    let cells = lattice.cells();
    let n = lattice.lattice_n;
    let pin = |i: u32, j: u32| -> i32 {
        let vertex = j * n + i;
        match cut.ring_pins.binary_search_by_key(&vertex, |p| p.vertex) {
            Ok(k) => cut.ring_pins[k].height_mm,
            Err(_) => m_to_mm(count_height_m(lattice, counts, i, j)),
        }
    };
    let mut out = Vec::new();
    for &(j, i, t) in &cut.cut {
        let ring_edge = (0..3u8)
            .any(|e| neighbor(cells, i, j, t, e).is_none_or(|(ni, nj, nt)| !cut.cut.contains(&(nj, ni, nt))));
        if !ring_edge {
            continue;
        }
        let mouth = cut
            .mouths
            .iter()
            .position(|m| m.cut.contains(&(j, i, t)))
            .unwrap_or(0);
        let v = tri_vertices(i, j, t).map(|(vi, vj)| {
            [
                lattice.vertex_mm(vi) as i32,
                pin(vi, vj),
                lattice.vertex_mm(vj) as i32,
            ]
        });
        out.push(CollarTriangle {
            mouth,
            v,
            kind: TriKind::Floor,
        });
    }
    out
}
