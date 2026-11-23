// SPDX-License-Identifier: MPL-2.0
//! 2D polygons with robust booleans and offsetting (`docs/specs/64-splines-polygons-roads.md` § 3).
//!
//! A [`PolygonData`] is a set of rings in a horizontal plane: its `origin` gives the plane height
//! (`origin[1]`) and the XZ origin the ring coordinates are relative to. Rings are normalized on
//! construction: outer rings counter-clockwise in `(x, z)` (positive shoelace area), holes
//! clockwise, duplicate vertices and zero-area rings removed, self-intersections resolved by the
//! even-odd rule.
//!
//! **Robustness:** every boolean and offset snaps coordinates to a 1 mm integer grid relative to
//! the polygon's origin (`i32` range, ±2 147 km; beyond that the operation fails with
//! `polygon_range`) and runs the overlay on integers (the `i_overlay` crate, i64 engine), so
//! results are identical on every platform. Arcs of round joins use `detmath`, never libm.
//! Provenance: `docs/provenance.md` ("Polygons").

use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay::{Overlay, ShapeType};
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::i_float::int::point::IntPoint;

use crate::detmath;

/// Grid step of the integer snapping, meters.
pub const GRID_M: f64 = 0.001;

/// Largest grid coordinate (`i32` range of the spec).
const GRID_LIMIT: f64 = i32::MAX as f64;

/// Largest sagitta of a round-join arc, meters.
const ARC_TOLERANCE_M: f64 = 0.002;

/// A polygon operation that cannot run.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PolygonError {
    /// A coordinate beyond the `i32` millimeter grid around the polygon's origin.
    #[error("polygon coordinate beyond ±2 147 km of the polygon origin")]
    Range,
}

impl PolygonError {
    /// The node error code.
    pub fn code(&self) -> &'static str {
        "polygon_range"
    }
}

/// How offset corners are joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Join {
    /// Sharp corners, beveled beyond the miter limit.
    Miter,
    /// Circular arcs.
    Round,
    /// Corners cut square at the offset distance.
    Square,
}

impl Join {
    /// Parses `miter`, `round` or `square` (anything else is `Miter`).
    pub fn from_name(name: &str) -> Self {
        match name {
            "round" => Self::Round,
            "square" => Self::Square,
            _ => Self::Miter,
        }
    }
}

/// A boolean operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoolOp {
    /// Either.
    Union,
    /// Both.
    Intersection,
    /// The first without the second.
    Difference,
    /// Exactly one.
    Xor,
}

impl BoolOp {
    /// Parses `union`, `intersection`, `difference` or `xor`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "union" => Some(Self::Union),
            "intersection" => Some(Self::Intersection),
            "difference" => Some(Self::Difference),
            "xor" => Some(Self::Xor),
            _ => None,
        }
    }

    fn rule(self) -> OverlayRule {
        match self {
            Self::Union => OverlayRule::Union,
            Self::Intersection => OverlayRule::Intersect,
            Self::Difference => OverlayRule::Difference,
            Self::Xor => OverlayRule::Xor,
        }
    }
}

type IRing = Vec<IntPoint<i64>>;

/// Rings in a horizontal plane (spec 64 § 3).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PolygonData {
    /// World position of the local origin; `origin[1]` is the plane height.
    pub origin: [f64; 3],
    /// Rings of `[x, z]` relative to the origin: outer counter-clockwise, holes clockwise.
    pub rings: Vec<Vec<[f64; 2]>>,
}

/// Signed shoelace area of a ring (positive = counter-clockwise in `(x, z)`).
pub fn ring_area(ring: &[[f64; 2]]) -> f64 {
    let n = ring.len();
    if n < 3 {
        return 0.0;
    }
    let mut a = 0.0;
    for i in 0..n {
        let (p, q) = (ring[i], ring[(i + 1) % n]);
        a += p[0] * q[1] - q[0] * p[1];
    }
    a * 0.5
}

fn snap(v: f64) -> Result<i64, PolygonError> {
    let g = (v / GRID_M).round();
    if !g.is_finite() || g.abs() > GRID_LIMIT {
        return Err(PolygonError::Range);
    }
    Ok(g as i64)
}

fn to_iring(ring: &[[f64; 2]], shift: [f64; 2]) -> Result<IRing, PolygonError> {
    ring.iter()
        .map(|p| Ok(IntPoint::new(snap(p[0] + shift[0])?, snap(p[1] + shift[1])?)))
        .collect()
}

fn from_iring(ring: &[IntPoint<i64>]) -> Vec<[f64; 2]> {
    ring.iter()
        .map(|p| [p.x as f64 * GRID_M, p.y as f64 * GRID_M])
        .collect()
}

/// A ring made counter-clockwise.
fn ccw(mut ring: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    if ring_area(&ring) < 0.0 {
        ring.reverse();
    }
    ring
}

/// Runs one overlay: `subject` and `clip` rings under one fill rule.
fn run(subject: &[IRing], clip: &[IRing], rule: OverlayRule, fill: FillRule) -> Vec<Vec<IRing>> {
    let edges = subject.iter().chain(clip).map(Vec::len).sum();
    let mut ov = Overlay::<i64>::new(edges);
    for r in subject {
        ov.add_contour(r, ShapeType::Subject);
    }
    for r in clip {
        ov.add_contour(r, ShapeType::Clip);
    }
    ov.overlay(rule, fill)
}

impl PolygonData {
    /// A normalized polygon from rings relative to `origin` (even-odd for self-intersections).
    ///
    /// # Errors
    ///
    /// [`PolygonError::Range`] for a coordinate beyond the grid.
    pub fn new(origin: [f64; 3], rings: &[Vec<[f64; 2]>]) -> Result<Self, PolygonError> {
        let irings = rings
            .iter()
            .map(|r| to_iring(r, [0.0, 0.0]))
            .collect::<Result<Vec<_>, _>>()?;
        let shapes = run(&irings, &[], OverlayRule::Subject, FillRule::EvenOdd);
        Ok(Self::from_shapes(origin, &shapes))
    }

    /// An axis-aligned rectangle `[min_x, min_z, max_x, max_z]` in world XZ at height `y`.
    pub fn rect(rect: [f64; 4], y: f64) -> Self {
        let origin = [(rect[0] + rect[2]) * 0.5, y, (rect[1] + rect[3]) * 0.5];
        let (hx, hz) = ((rect[2] - rect[0]) * 0.5, (rect[3] - rect[1]) * 0.5);
        Self {
            origin,
            rings: vec![vec![[-hx, -hz], [hx, -hz], [hx, hz], [-hx, hz]]],
        }
    }

    fn from_shapes(origin: [f64; 3], shapes: &[Vec<IRing>]) -> Self {
        Self {
            origin,
            rings: shapes.iter().flatten().map(|r| from_iring(r)).collect(),
        }
    }

    /// Whether there are no rings.
    pub fn is_empty(&self) -> bool {
        self.rings.is_empty()
    }

    /// Area, square meters (outer rings minus holes).
    pub fn area(&self) -> f64 {
        self.rings.iter().map(|r| ring_area(r)).sum()
    }

    /// Rings in world XZ.
    pub fn world_rings(&self) -> Vec<Vec<[f64; 2]>> {
        let o = [self.origin[0], self.origin[2]];
        self.rings
            .iter()
            .map(|r| r.iter().map(|p| [p[0] + o[0], p[1] + o[1]]).collect())
            .collect()
    }

    /// Even-odd containment of a world XZ point.
    pub fn contains(&self, x: f64, z: f64) -> bool {
        let (x, z) = (x - self.origin[0], z - self.origin[2]);
        let mut inside = false;
        for ring in &self.rings {
            let n = ring.len();
            for i in 0..n {
                let (a, b) = (ring[i], ring[(i + 1) % n]);
                if (a[1] > z) != (b[1] > z) {
                    let t = (z - a[1]) / (b[1] - a[1]);
                    if x < a[0] + t * (b[0] - a[0]) {
                        inside = !inside;
                    }
                }
            }
        }
        inside
    }

    /// Distance from a world XZ point to the nearest ring edge, meters.
    pub fn edge_distance(&self, x: f64, z: f64) -> f64 {
        let (x, z) = (x - self.origin[0], z - self.origin[2]);
        let mut best = f64::INFINITY;
        for ring in &self.rings {
            let n = ring.len();
            for i in 0..n {
                let (a, b) = (ring[i], ring[(i + 1) % n]);
                let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
                let l2 = dx * dx + dz * dz;
                let t = if l2 > 0.0 {
                    (((x - a[0]) * dx + (z - a[1]) * dz) / l2).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let (px, pz) = (a[0] + dx * t - x, a[1] + dz * t - z);
                best = best.min((px * px + pz * pz).sqrt());
            }
        }
        best
    }

    /// World XZ bounds `[min_x, min_z, max_x, max_z]` (`None` when empty).
    pub fn bounds(&self) -> Option<[f64; 4]> {
        let mut it = self.rings.iter().flatten();
        let first = it.next()?;
        let mut b = [first[0], first[1], first[0], first[1]];
        for p in it {
            b = [b[0].min(p[0]), b[1].min(p[1]), b[2].max(p[0]), b[3].max(p[1])];
        }
        Some([
            b[0] + self.origin[0],
            b[1] + self.origin[2],
            b[2] + self.origin[0],
            b[3] + self.origin[2],
        ])
    }

    /// Rings of another polygon on this one's integer grid.
    fn other_irings(&self, other: &Self) -> Result<Vec<IRing>, PolygonError> {
        let shift = [other.origin[0] - self.origin[0], other.origin[2] - self.origin[2]];
        other.rings.iter().map(|r| to_iring(r, shift)).collect()
    }

    fn irings(&self) -> Result<Vec<IRing>, PolygonError> {
        self.rings.iter().map(|r| to_iring(r, [0.0, 0.0])).collect()
    }

    /// A boolean against `other` (result in this polygon's frame).
    ///
    /// # Errors
    ///
    /// [`PolygonError::Range`] when `other` lies beyond the grid around this origin.
    pub fn boolean(&self, other: &Self, op: BoolOp) -> Result<Self, PolygonError> {
        let shapes = run(
            &self.irings()?,
            &self.other_irings(other)?,
            op.rule(),
            FillRule::NonZero,
        );
        Ok(Self::from_shapes(self.origin, &shapes))
    }

    /// The union of several polygons in the first one's frame (`None` for no input).
    ///
    /// # Errors
    ///
    /// [`PolygonError::Range`] as for [`PolygonData::boolean`].
    pub fn union_all(items: &[&Self]) -> Result<Option<Self>, PolygonError> {
        let Some(first) = items.first() else {
            return Ok(None);
        };
        let mut rings = Vec::new();
        for p in items {
            rings.extend(first.other_irings(p)?);
        }
        let shapes = run(&rings, &[], OverlayRule::Subject, FillRule::NonZero);
        Ok(Some(Self::from_shapes(first.origin, &shapes)))
    }

    /// The connected pieces: one polygon per outer ring with its holes.
    pub fn pieces(&self) -> Vec<Self> {
        let Ok(irings) = self.irings() else {
            return vec![self.clone()];
        };
        run(&irings, &[], OverlayRule::Subject, FillRule::NonZero)
            .iter()
            .map(|s| Self::from_shapes(self.origin, std::slice::from_ref(s)))
            .collect()
    }

    /// Area-weighted centroid of the rings in world XZ (the first vertex for zero area).
    pub fn centroid(&self) -> [f64; 2] {
        let (mut cx, mut cz, mut a) = (0.0, 0.0, 0.0);
        for ring in &self.rings {
            let n = ring.len();
            for i in 0..n {
                let (p, q) = (ring[i], ring[(i + 1) % n]);
                let c = p[0] * q[1] - q[0] * p[1];
                cx += (p[0] + q[0]) * c;
                cz += (p[1] + q[1]) * c;
                a += c;
            }
        }
        if a.abs() > 1e-12 {
            [cx / (3.0 * a) + self.origin[0], cz / (3.0 * a) + self.origin[2]]
        } else {
            let p = self
                .rings
                .first()
                .and_then(|r| r.first())
                .copied()
                .unwrap_or([0.0; 2]);
            [p[0] + self.origin[0], p[1] + self.origin[2]]
        }
    }

    /// Grows (positive) or shrinks (negative) the polygon by `distance_m`; collapsed parts
    /// disappear. Built as the union (or difference) of one quad per edge and one join wedge per
    /// corner on the moving side, on the integer grid.
    ///
    /// # Errors
    ///
    /// [`PolygonError::Range`] when the grown polygon leaves the grid.
    pub fn offset(&self, distance_m: f64, join: Join, miter_limit: f64) -> Result<Self, PolygonError> {
        if distance_m == 0.0 || !distance_m.is_finite() || self.is_empty() {
            return Ok(self.clone());
        }
        let s = distance_m.signum();
        let d = distance_m.abs();
        let mut pieces: Vec<Vec<[f64; 2]>> = Vec::new();
        for ring in &self.rings {
            let n = ring.len();
            if n < 3 {
                continue;
            }
            // Edge directions and right-hand normals (away from the filled side).
            let dirs: Vec<[f64; 2]> = (0..n)
                .map(|i| {
                    let (a, b) = (ring[i], ring[(i + 1) % n]);
                    unit([b[0] - a[0], b[1] - a[1]])
                })
                .collect();
            for i in 0..n {
                let (a, b) = (ring[i], ring[(i + 1) % n]);
                let e = dirs[i];
                let m = [e[1] * s * d, -e[0] * s * d];
                pieces.push(ccw(vec![
                    a,
                    b,
                    [b[0] + m[0], b[1] + m[1]],
                    [a[0] + m[0], a[1] + m[1]],
                ]));
            }
            for i in 0..n {
                let (e1, e2) = (dirs[(i + n - 1) % n], dirs[i]);
                let cross = e1[0] * e2[1] - e1[1] * e2[0];
                // Growing opens gaps at material-convex corners, shrinking at reflex ones.
                if cross * s <= 0.0 {
                    continue;
                }
                let n1 = [e1[1] * s, -e1[0] * s];
                let n2 = [e2[1] * s, -e2[0] * s];
                let corner = Corner {
                    v: ring[i],
                    n1,
                    n2,
                    e1,
                    e2,
                };
                if let Some(w) = join_wedge(&corner, d, join, miter_limit) {
                    pieces.push(ccw(w));
                }
            }
        }
        let own = self.irings()?;
        let extra = pieces
            .iter()
            .map(|r| to_iring(r, [0.0, 0.0]))
            .collect::<Result<Vec<_>, _>>()?;
        let shapes = if s > 0.0 {
            let mut all = own;
            all.extend(extra);
            run(&all, &[], OverlayRule::Subject, FillRule::NonZero)
        } else {
            run(&own, &extra, OverlayRule::Difference, FillRule::NonZero)
        };
        Ok(Self::from_shapes(self.origin, &shapes))
    }

    /// Cuts the polygon along world-XZ polylines, removing a band `gap_m` wide around each
    /// (at least one grid step, so pieces always separate). Pieces are ordered by centroid
    /// (x, then z).
    ///
    /// # Errors
    ///
    /// [`PolygonError::Range`] when a cutter lies beyond the grid around this origin.
    pub fn cut(&self, polylines: &[Vec<[f64; 2]>], gap_m: f64) -> Result<Vec<Self>, PolygonError> {
        let band = band_rings(
            polylines,
            [self.origin[0], self.origin[2]],
            (gap_m * 0.5).max(GRID_M * 0.5),
        )?;
        let band_union = run(&band, &[], OverlayRule::Subject, FillRule::NonZero);
        let cutters: Vec<IRing> = band_union.into_iter().flatten().collect();
        let shapes = run(
            &self.irings()?,
            &cutters,
            OverlayRule::Difference,
            FillRule::NonZero,
        );
        let mut out: Vec<Self> = shapes
            .iter()
            .map(|s| Self::from_shapes(self.origin, std::slice::from_ref(s)))
            .collect();
        out.sort_by(|a, b| {
            let (ca, cb) = (a.centroid(), b.centroid());
            ca[0].total_cmp(&cb[0]).then(ca[1].total_cmp(&cb[1]))
        });
        Ok(out)
    }

    /// The region within `half_width_m` of world-XZ polylines (segment quads plus round joints),
    /// at height `y`, with its origin at `origin_xz`.
    ///
    /// # Errors
    ///
    /// [`PolygonError::Range`] for a polyline beyond the grid around the origin.
    pub fn band(
        polylines: &[Vec<[f64; 2]>],
        half_width_m: f64,
        origin_xz: [f64; 2],
        y: f64,
    ) -> Result<Self, PolygonError> {
        let rings = band_rings(polylines, origin_xz, half_width_m.max(GRID_M * 0.5))?;
        let shapes = run(&rings, &[], OverlayRule::Subject, FillRule::NonZero);
        Ok(Self::from_shapes([origin_xz[0], y, origin_xz[1]], &shapes))
    }

    /// Triangles covering the polygon: ear clipping per piece after bridging each hole to the outer
    /// ring at the closest visible vertex (holes in order of their largest x). Vertices are the ring
    /// vertices in world XZ, in ring order; the result depends only on the rings.
    pub fn triangulate(&self) -> Triangulation {
        let mut out = Triangulation::default();
        for piece in self.pieces() {
            let o = [piece.origin[0], piece.origin[2]];
            let mut outer: Option<Vec<u32>> = None;
            let mut holes: Vec<Vec<u32>> = Vec::new();
            for ring in &piece.rings {
                let start = out.vertices.len() as u32;
                out.vertices
                    .extend(ring.iter().map(|p| [p[0] + o[0], p[1] + o[1]]));
                let idx: Vec<u32> = (start..start + ring.len() as u32).collect();
                if ring_area(ring) > 0.0 && outer.is_none() {
                    outer = Some(idx);
                } else {
                    holes.push(idx);
                }
            }
            let Some(mut poly) = outer else {
                continue;
            };
            let v = out.vertices.clone();
            let x_of = |i: u32| v[i as usize][0];
            holes.sort_by(|a, b| {
                let ma = a.iter().map(|i| x_of(*i)).fold(f64::NEG_INFINITY, f64::max);
                let mb = b.iter().map(|i| x_of(*i)).fold(f64::NEG_INFINITY, f64::max);
                mb.total_cmp(&ma)
            });
            for hole in &holes {
                bridge(&v, &mut poly, hole, &holes);
            }
            ear_clip(&v, &poly, &mut out.indices);
        }
        out
    }

    /// Feeds the canonical encoding into a hasher.
    pub(crate) fn hash_into(&self, h: &mut blake3::Hasher) {
        h.update(b"polygon");
        for v in self.origin {
            h.update(&v.to_bits().to_le_bytes());
        }
        for ring in &self.rings {
            h.update(&(ring.len() as u64).to_le_bytes());
            for p in ring {
                h.update(&p[0].to_bits().to_le_bytes());
                h.update(&p[1].to_bits().to_le_bytes());
            }
        }
    }
}

/// Quads along each polyline segment and discs at interior vertices, on the grid around `o`.
fn band_rings(polylines: &[Vec<[f64; 2]>], o: [f64; 2], h: f64) -> Result<Vec<IRing>, PolygonError> {
    let mut band: Vec<IRing> = Vec::new();
    for line in polylines {
        let pts: Vec<[f64; 2]> = line.iter().map(|p| [p[0] - o[0], p[1] - o[1]]).collect();
        for w in pts.windows(2) {
            let e = unit([w[1][0] - w[0][0], w[1][1] - w[0][1]]);
            if e == [0.0, 0.0] {
                continue;
            }
            let m = [e[1] * h, -e[0] * h];
            let quad = vec![
                [w[0][0] + m[0], w[0][1] + m[1]],
                [w[1][0] + m[0], w[1][1] + m[1]],
                [w[1][0] - m[0], w[1][1] - m[1]],
                [w[0][0] - m[0], w[0][1] - m[1]],
            ];
            band.push(to_iring(&ccw(quad), [0.0, 0.0])?);
        }
        if pts.len() > 2 {
            for p in &pts[1..pts.len() - 1] {
                band.push(to_iring(&circle(*p, h), [0.0, 0.0])?);
            }
        }
    }
    Ok(band)
}

/// The result of [`PolygonData::triangulate`].
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Triangulation {
    /// Vertices in world XZ.
    pub vertices: Vec<[f64; 2]>,
    /// Counter-clockwise triangles (in `(x, z)`), three indices each.
    pub indices: Vec<u32>,
}

fn cross3(a: [f64; 2], b: [f64; 2], c: [f64; 2]) -> f64 {
    (b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])
}

/// Whether open segments `a-b` and `c-d` properly cross.
fn segments_cross(a: [f64; 2], b: [f64; 2], c: [f64; 2], d: [f64; 2]) -> bool {
    let (d1, d2) = (cross3(a, b, c), cross3(a, b, d));
    let (d3, d4) = (cross3(c, d, a), cross3(c, d, b));
    ((d1 > 0.0 && d2 < 0.0) || (d1 < 0.0 && d2 > 0.0)) && ((d3 > 0.0 && d4 < 0.0) || (d3 < 0.0 && d4 > 0.0))
}

/// Splices `hole` into `poly` through a bridge from the hole's largest-x vertex to the closest
/// polygon vertex whose bridge crosses no edge.
fn bridge(v: &[[f64; 2]], poly: &mut Vec<u32>, hole: &[u32], holes: &[Vec<u32>]) {
    let Some(hk) = (0..hole.len()).max_by(|a, b| {
        v[hole[*a] as usize][0]
            .total_cmp(&v[hole[*b] as usize][0])
            .then(b.cmp(a))
    }) else {
        return;
    };
    let m = v[hole[hk] as usize];
    let mut order: Vec<usize> = (0..poly.len()).collect();
    let d2 = |k: usize| {
        let p = v[poly[k] as usize];
        let (dx, dz) = (p[0] - m[0], p[1] - m[1]);
        dx * dx + dz * dz
    };
    order.sort_by(|a, b| d2(*a).total_cmp(&d2(*b)).then(a.cmp(b)));
    let edges = |ring: &[u32]| -> Vec<([f64; 2], [f64; 2])> {
        (0..ring.len())
            .map(|i| (v[ring[i] as usize], v[ring[(i + 1) % ring.len()] as usize]))
            .collect()
    };
    let mut all = edges(poly);
    for h in holes {
        all.extend(edges(h));
    }
    let pick = order
        .iter()
        .copied()
        .find(|k| {
            let p = v[poly[*k] as usize];
            !all.iter().any(|(a, b)| segments_cross(m, p, *a, *b))
        })
        .unwrap_or(order[0]);
    let mut spliced = Vec::with_capacity(poly.len() + hole.len() + 2);
    spliced.extend_from_slice(&poly[..=pick]);
    for i in 0..=hole.len() {
        spliced.push(hole[(hk + i) % hole.len()]);
    }
    spliced.push(poly[pick]);
    spliced.extend_from_slice(&poly[pick + 1..]);
    *poly = spliced;
}

/// Ear clipping of a counter-clockwise (possibly bridged) ring of vertex indices.
fn ear_clip(v: &[[f64; 2]], ring: &[u32], out: &mut Vec<u32>) {
    let mut r: Vec<u32> = ring.to_vec();
    let mut guard = 0usize;
    while r.len() > 3 && guard < 4 * ring.len() * ring.len() + 16 {
        guard += 1;
        let n = r.len();
        let mut clipped = false;
        for i in 0..n {
            let (ia, ib, ic) = (r[(i + n - 1) % n], r[i], r[(i + 1) % n]);
            let (a, b, c) = (v[ia as usize], v[ib as usize], v[ic as usize]);
            if cross3(a, b, c) <= 0.0 {
                continue;
            }
            let inside = r.iter().any(|&j| {
                let p = v[j as usize];
                if p == a || p == b || p == c {
                    return false;
                }
                cross3(a, b, p) >= 0.0 && cross3(b, c, p) >= 0.0 && cross3(c, a, p) >= 0.0
            });
            if inside {
                continue;
            }
            out.extend([ia, ib, ic]);
            r.remove(i);
            clipped = true;
            break;
        }
        if !clipped {
            // Degenerate rest (collinear or touching): drop the flattest corner.
            let k = (0..r.len())
                .min_by(|x, y| {
                    let f = |i: usize| {
                        let n = r.len();
                        cross3(
                            v[r[(i + n - 1) % n] as usize],
                            v[r[i] as usize],
                            v[r[(i + 1) % n] as usize],
                        )
                        .abs()
                    };
                    f(*x).total_cmp(&f(*y))
                })
                .unwrap_or(0);
            r.remove(k);
        }
    }
    if r.len() == 3 && cross3(v[r[0] as usize], v[r[1] as usize], v[r[2] as usize]) > 0.0 {
        out.extend([r[0], r[1], r[2]]);
    }
}

fn unit(v: [f64; 2]) -> [f64; 2] {
    let l = (v[0] * v[0] + v[1] * v[1]).sqrt();
    if l > 1e-12 {
        [v[0] / l, v[1] / l]
    } else {
        [0.0, 0.0]
    }
}

/// Angular step of an arc of radius `r` with sagitta at most [`ARC_TOLERANCE_M`].
fn arc_step(r: f64) -> f64 {
    let max = std::f64::consts::PI / 8.0;
    let min = std::f64::consts::TAU / 256.0;
    if r <= 0.0 {
        return max;
    }
    (8.0 * ARC_TOLERANCE_M / r).sqrt().clamp(min, max)
}

/// A regular polygon approximating a circle (counter-clockwise).
fn circle(c: [f64; 2], r: f64) -> Vec<[f64; 2]> {
    let n = ((std::f64::consts::TAU / arc_step(r)).ceil() as usize).max(8);
    (0..n)
        .map(|k| {
            let (s, co) = detmath::sin_cos(std::f64::consts::TAU * k as f64 / n as f64);
            [c[0] + r * co, c[1] + r * s]
        })
        .collect()
}

/// One corner of an offset ring: the vertex, the offset-side normals and the directions of the
/// incoming and outgoing edges.
#[derive(Clone, Copy)]
struct Corner {
    v: [f64; 2],
    n1: [f64; 2],
    n2: [f64; 2],
    e1: [f64; 2],
    e2: [f64; 2],
}

/// The join wedge at a corner between offset normals `n1` (incoming edge) and `n2` (outgoing).
fn join_wedge(c: &Corner, d: f64, join: Join, miter_limit: f64) -> Option<Vec<[f64; 2]>> {
    let Corner { v, n1, n2, e1, e2 } = *c;
    let a = [v[0] + n1[0] * d, v[1] + n1[1] * d];
    let b = [v[0] + n2[0] * d, v[1] + n2[1] * d];
    let dot = n1[0] * n2[0] + n1[1] * n2[1];
    let bevel = vec![v, a, b];
    match join {
        Join::Round => {
            let a1 = detmath::atan2(n1[1], n1[0]);
            let sweep = detmath::atan2(n1[0] * n2[1] - n1[1] * n2[0], dot);
            let steps = ((sweep.abs() / arc_step(d)).ceil() as usize).max(1);
            let mut w = vec![v];
            for k in 0..=steps {
                let (s, c) = detmath::sin_cos(a1 + sweep * k as f64 / steps as f64);
                w.push([v[0] + d * c, v[1] + d * s]);
            }
            Some(w)
        }
        Join::Miter => {
            if 1.0 + dot <= 1e-12 {
                return Some(bevel);
            }
            let ratio = (2.0 / (1.0 + dot)).sqrt();
            if ratio > miter_limit.max(1.0) {
                return Some(bevel);
            }
            let k = d / (1.0 + dot);
            Some(vec![
                v,
                a,
                [v[0] + (n1[0] + n2[0]) * k, v[1] + (n1[1] + n2[1]) * k],
                b,
            ])
        }
        Join::Square => {
            let bis = unit([n1[0] + n2[0], n1[1] + n2[1]]);
            let along = e1[0] * bis[0] + e1[1] * bis[1];
            if along <= 1e-9 || bis == [0.0, 0.0] {
                return Some(bevel);
            }
            let t = d * (1.0 - (n1[0] * bis[0] + n1[1] * bis[1])) / along;
            Some(vec![
                v,
                a,
                [a[0] + e1[0] * t, a[1] + e1[1] * t],
                [b[0] - e2[0] * t, b[1] - e2[1] * t],
                b,
            ])
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x0: f64, z0: f64, s: f64) -> Vec<[f64; 2]> {
        vec![[x0, z0], [x0 + s, z0], [x0 + s, z0 + s], [x0, z0 + s]]
    }

    #[test]
    fn normalizes_orientation_and_drops_degenerate_rings() {
        let mut cw = square(0.0, 0.0, 2.0);
        cw.reverse();
        let p = PolygonData::new([0.0; 3], &[cw, vec![[0.0, 0.0], [1.0, 0.0]]]).expect("poly");
        assert_eq!(p.rings.len(), 1);
        assert!((p.area() - 4.0).abs() < 1e-9);
        assert!(ring_area(&p.rings[0]) > 0.0);
    }

    #[test]
    fn range_errors() {
        let far = vec![[0.0, 0.0], [3.0e6, 0.0], [3.0e6, 1.0]];
        assert_eq!(PolygonData::new([0.0; 3], &[far]), Err(PolygonError::Range));
    }

    #[test]
    fn miter_and_square_offsets_of_a_square() {
        let p = PolygonData::new([0.0; 3], &[square(0.0, 0.0, 1.0)]).expect("poly");
        let m = p.offset(1.0, Join::Miter, 2.0).expect("miter");
        assert!((m.area() - 9.0).abs() < 1e-6, "{}", m.area());
        let b = p.offset(1.0, Join::Miter, 1.0).expect("bevel");
        assert!((b.area() - 7.0).abs() < 1e-6, "{}", b.area());
        let s = p.offset(1.0, Join::Square, 2.0).expect("square");
        // Corners cut at distance 1 along the diagonal: each corner loses (√2 − 1)² of the 1 × 1.
        let corner = 1.0 - (2.0f64.sqrt() - 1.0).powi(2);
        assert!((s.area() - (5.0 + 4.0 * corner)).abs() < 1e-3, "{}", s.area());
        let shrunk = PolygonData::new([0.0; 3], &[square(0.0, 0.0, 4.0)])
            .expect("poly")
            .offset(-1.0, Join::Miter, 2.0)
            .expect("shrink");
        assert!((shrunk.area() - 4.0).abs() < 1e-6, "{}", shrunk.area());
        assert!(p.offset(-0.6, Join::Round, 2.0).expect("gone").is_empty());
    }

    #[test]
    fn triangulation_covers_the_area_with_holes() {
        let tri_area = |t: &Triangulation| {
            t.indices
                .as_chunks::<3>()
                .0
                .iter()
                .map(|k| {
                    let [a, b, c] = k.map(|i| t.vertices[i as usize]);
                    cross3(a, b, c) * 0.5
                })
                .sum::<f64>()
        };
        let mut hole = square(4.0, 4.0, 2.0);
        hole.reverse();
        let holed = PolygonData::new([0.0; 3], &[square(0.0, 0.0, 10.0), hole]).expect("holed");
        let t = holed.triangulate();
        assert_eq!(t.indices.len() / 3, 8, "n + 2h - 2 triangles");
        assert!((tri_area(&t) - 96.0).abs() < 1e-9, "{}", tri_area(&t));
        let ell = PolygonData::new(
            [5.0, 0.0, 5.0],
            &[vec![
                [0.0, 0.0],
                [4.0, 0.0],
                [4.0, 1.0],
                [1.0, 1.0],
                [1.0, 3.0],
                [0.0, 3.0],
            ]],
        )
        .expect("ell");
        let t = ell.triangulate();
        assert!((tri_area(&t) - ell.area()).abs() < 1e-9);
        assert!(
            t.vertices.iter().all(|p| p[0] >= 5.0 && p[1] >= 5.0),
            "world coordinates"
        );
        assert_eq!(ell.triangulate(), t, "deterministic");
    }

    #[test]
    fn contains_and_centroid() {
        let p = PolygonData::new([10.0, 0.0, 0.0], &[square(-1.0, -1.0, 2.0)]).expect("poly");
        assert!(p.contains(10.5, 0.5) && !p.contains(8.0, 0.0));
        let c = p.centroid();
        assert!((c[0] - 10.0).abs() < 1e-9 && c[1].abs() < 1e-9);
        let b = p.bounds().unwrap_or_default();
        assert!(
            b.iter()
                .zip([9.0, -1.0, 11.0, 1.0])
                .all(|(a, e)| (a - e).abs() < 1e-9),
            "{b:?}"
        );
    }
}
