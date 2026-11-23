// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-content/src/tunnels.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-425 and 605-1323, mark: as-is (Stage A: unchanged), with ADR-8166's det_*/libm calls (below).
// Re-synced by W-02 (ADR-8167) from W-01's regia-absorb-1 copy (d6ed37677, lines 1-397, 676-1428): the
// support floors (`welded`, `with_layer`, `support_floors`, `support_near`), `MeshLayers`, crate-visible
// `TriIndex`, `near`, `intersect_floor_xz` and `plane_y_and_normal`; `PORTAL_HYSTERESIS_M` and
// `DECK_SNAP_M` are gone.
// Stage A (spec 121 § 4, ADR-8050): arithmetic unchanged. Edits: module paths, doc comments (DO27
// file and engine names dropped, missing docs added), lint attributes, visibility of helpers the
// sibling compat modules call.
// ADR-8166 (review rev-b L1, L2): std trig and sqrt go through `regia_detsim::math` (libm 0.2.16) and
// std float min/max/clamp through `regia_detsim::float::det_*`; the source tag for those lines is
// DO27's DET_FLOAT change (pending regia-absorb-2).

//! Gallery tunnel collision: triangle mesh beside the heightfield.
//!
//! Outdoor ground stays a heightfield. Tunnels are a second collidable
//! surface — floor, wall, and ceiling triangles — queried by the same
//! hover step. No teleport, no force field, no reactionless motion: the
//! hull is a disc of radius 2.5 (the host's unit collision radius).

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;

use crate::compat::sampler::{FloorSample, FloorSurface, TerrainSampler};

/// Per-triangle surface role inside a gallery volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TunnelFaceKind {
    /// Walkable floor.
    Floor,
    /// Side wall.
    Wall,
    /// Ceiling.
    Ceiling,
}

/// One triangle of the tunnel collision mesh.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TunnelTriangle {
    /// First vertex (sim metres).
    pub a: [f32; 3],
    /// Second vertex.
    pub b: [f32; 3],
    /// Third vertex.
    pub c: [f32; 3],
    /// Floor, wall or ceiling.
    pub kind: TunnelFaceKind,
    /// Floor triangles that open onto the outdoor heightfield.
    /// Nav overlay edges connect these to outdoor pathing; without any
    /// portal the overlay cannot leave the gallery.
    pub portal: bool,
    /// Personnel decks (building interiors). Vehicle pathing skips these.
    /// Tunnel galleries leave this false so hulls and pilots both use them.
    pub pilot_only: bool,
}

/// Decks in one column: outdoor or the highest floor is 0, each deck
/// below it is one deeper, and the deepest pathable deck is 3.
/// Four decks is the stack a unit can be on. Baseline pathing was
/// one heightfield; this is the replacement.
pub const MAX_NAV_LEVELS: u8 = 4;

/// Triangle collision mesh for one or more gallery volumes.
/// Stored beside the heightfield in the map pack; the renderer never authors it.
#[derive(Debug, Clone, Default)]
pub struct TunnelCollisionMesh {
    /// The triangles, in mesh order (the order is part of the sim's bits).
    pub triangles: Vec<TunnelTriangle>,
    /// XZ bucket index built by [`TunnelCollisionMesh::new`] (a 2 km tube is
    /// thousands of triangles; every query walks one bucket, not the mesh).
    index: TriIndex,
    /// T3: the floors that carry bodies ([`crate::compat::surface`]): every floor
    /// triangle that is not trim, welded to the terrain by
    /// [`TunnelCollisionMesh::welded`]. Built once at load, in triangle order.
    support: Vec<TunnelTriangle>,
    support_index: TriIndex,
}

/// Every mesh layer of a map is triangles in this one indexed set (T3): the
/// tunnels today, overhangs and stacked levels later
/// ([`TunnelCollisionMesh::with_layer`]).
pub type MeshLayers = TunnelCollisionMesh;

/// Two meshes are the same mesh when their triangles are.
impl PartialEq for TunnelCollisionMesh {
    fn eq(&self, other: &Self) -> bool {
        self.triangles == other.triangles
    }
}

/// Bucket side (m) of the XZ index.
const INDEX_CELL_M: f32 = 16.0;
/// Wall triangles are filed this much wider, so a disc query of up to this
/// radius finds every wall it can touch from one bucket.
const INDEX_WALL_PAD_M: f32 = 8.0;

/// The XZ bucket index over a triangle list (16 m cells, walls padded 8 m).
#[derive(Debug, Clone, Default)]
pub(crate) struct TriIndex {
    bounds: Option<([f32; 3], [f32; 3])>,
    min_x: f32,
    min_z: f32,
    nx: usize,
    nz: usize,
    /// Triangle indices per bucket, ascending.
    buckets: Vec<Vec<u32>>,
}

impl TriIndex {
    pub(crate) fn build(triangles: &[TunnelTriangle]) -> TriIndex {
        let mut iter = triangles.iter().flat_map(|t| [t.a, t.b, t.c]);
        let Some(first) = iter.next() else {
            return TriIndex::default();
        };
        let mut min = first;
        let mut max = first;
        for p in iter {
            for i in 0..3 {
                min[i] = det_min(min[i], p[i]);
                max[i] = det_max(max[i], p[i]);
            }
        }
        let pad = INDEX_WALL_PAD_M;
        let min_x = min[0] - pad;
        let min_z = min[2] - pad;
        let nx = (((max[0] + pad - min_x) / INDEX_CELL_M) as usize + 1).max(1);
        let nz = (((max[2] + pad - min_z) / INDEX_CELL_M) as usize + 1).max(1);
        let mut buckets = vec![Vec::new(); nx * nz];
        for (i, tri) in triangles.iter().enumerate() {
            let pad = if tri.kind == TunnelFaceKind::Wall {
                INDEX_WALL_PAD_M
            } else {
                0.25
            };
            let lo_x = det_min(det_min(tri.a[0], tri.b[0]), tri.c[0]) - pad;
            let hi_x = det_max(det_max(tri.a[0], tri.b[0]), tri.c[0]) + pad;
            let lo_z = det_min(det_min(tri.a[2], tri.b[2]), tri.c[2]) - pad;
            let hi_z = det_max(det_max(tri.a[2], tri.b[2]), tri.c[2]) + pad;
            let bx0 = usize::min(
                det_max(((lo_x - min_x) / INDEX_CELL_M).floor(), 0.0) as usize,
                nx - 1,
            );
            let bx1 = usize::min(
                det_max(((hi_x - min_x) / INDEX_CELL_M).floor(), 0.0) as usize,
                nx - 1,
            );
            let bz0 = usize::min(
                det_max(((lo_z - min_z) / INDEX_CELL_M).floor(), 0.0) as usize,
                nz - 1,
            );
            let bz1 = usize::min(
                det_max(((hi_z - min_z) / INDEX_CELL_M).floor(), 0.0) as usize,
                nz - 1,
            );
            for b_z in bz0..=bz1 {
                for bx in bx0..=bx1 {
                    buckets[b_z * nx + bx].push(i as u32);
                }
            }
        }
        TriIndex {
            bounds: Some((min, max)),
            min_x,
            min_z,
            nx,
            nz,
            buckets,
        }
    }

    /// Triangles filed in the bucket under `(x, z)`; `None` when the index is
    /// empty (every triangle is a candidate) and an empty slice off the mesh.
    pub(crate) fn at(&self, x: f32, z: f32) -> Option<&[u32]> {
        if self.buckets.is_empty() {
            return None;
        }
        let fx = (x - self.min_x) / INDEX_CELL_M;
        let fz = (z - self.min_z) / INDEX_CELL_M;
        if !(fx >= 0.0 && fz >= 0.0) || fx >= self.nx as f32 || fz >= self.nz as f32 {
            return Some(&[]);
        }
        Some(&self.buckets[fz as usize * self.nx + fx as usize])
    }
}

impl TunnelCollisionMesh {
    /// A mesh over `triangles`, with its XZ index built.
    pub fn new(triangles: Vec<TunnelTriangle>) -> Self {
        let index = TriIndex::build(&triangles);
        let support = crate::compat::surface::support_floors(&triangles, &index);
        let support_index = TriIndex::build(&support);
        Self {
            triangles,
            index,
            support,
            support_index,
        }
    }

    /// T3: weld the support floors to `terrain` (a floor vertex up to
    /// [`crate::compat::surface::WELD_M`] proud of the ground is put on it), so a lip
    /// or cap modeled a hair over the ground joins it with no step. The drawn
    /// triangles are not changed. Call once at load, after [`Self::new`].
    pub fn welded(mut self, terrain: Option<&TerrainSampler>) -> Self {
        if let Some(terrain) = terrain {
            crate::compat::surface::weld_to_terrain(&mut self.support, terrain);
        }
        self
    }

    /// This set plus one more mesh layer (its floor, wall and ceiling
    /// triangles), re-indexed. Weld again afterwards.
    pub fn with_layer(self, layer: Vec<TunnelTriangle>) -> Self {
        let mut triangles = self.triangles;
        triangles.extend(layer);
        Self::new(triangles)
    }

    /// T3: the floor triangles that carry bodies (welded, trim left out).
    pub fn support_floors(&self) -> &[TunnelTriangle] {
        &self.support
    }

    /// Support floors that can matter at `(x, z)`, in triangle order.
    pub(crate) fn support_near(&self, x: f32, z: f32) -> impl Iterator<Item = &TunnelTriangle> + '_ {
        let picked: Option<&[u32]> = self.support_index.at(x, z);
        let all = picked.is_none();
        let ids = picked.unwrap_or(&[]);
        let indexed = ids.iter().map(move |i| &self.support[*i as usize]);
        let everything = self.support.iter().filter(move |_| all);
        indexed.chain(everything)
    }

    /// True when the mesh has no triangles.
    pub fn is_empty(&self) -> bool {
        self.triangles.is_empty()
    }

    /// Triangles that can matter at `(x, z)`, in mesh order.
    pub(crate) fn near(&self, x: f32, z: f32) -> impl Iterator<Item = &TunnelTriangle> + '_ {
        let picked: Option<&[u32]> = self.index.at(x, z);
        let all = picked.is_none();
        let ids = picked.unwrap_or(&[]);
        let indexed = ids.iter().map(move |i| &self.triangles[*i as usize]);
        let everything = self.triangles.iter().filter(move |_| all);
        indexed.chain(everything)
    }

    /// Axis-aligned bounds of every triangle vertex.
    pub fn bounds(&self) -> Option<([f32; 3], [f32; 3])> {
        if self.index.bounds.is_some() {
            return self.index.bounds;
        }
        let mut iter = self.triangles.iter().flat_map(|t| [t.a, t.b, t.c]);
        let first = iter.next()?;
        let mut min = first;
        let mut max = first;
        for p in iter {
            for i in 0..3 {
                min[i] = det_min(min[i], p[i]);
                max[i] = det_max(max[i], p[i]);
            }
        }
        Some((min, max))
    }

    /// True when `(x, hint_y, z)` sits inside the mesh AABB (gallery volume).
    pub fn contains_hint(&self, x: f32, hint_y: f32, z: f32) -> bool {
        let Some((min, max)) = self.bounds() else {
            return false;
        };
        x >= min[0] && x <= max[0] && hint_y >= min[1] && hint_y <= max[1] && z >= min[2] && z <= max[2]
    }

    /// Highest floor triangle under `(x, z)`, if any.
    pub fn sample_floor_at(&self, x: f32, z: f32) -> Option<FloorSample> {
        self.floors_at(x, z).into_iter().max_by(|a, b| {
            a.point[1]
                .partial_cmp(&b.point[1])
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// Every floor under `(x, z)`. Stacked decks all remain; the highest
    /// one does not erase the ones below it.
    pub fn floors_at(&self, x: f32, z: f32) -> Vec<FloorSample> {
        let mut out = Vec::new();
        for tri in self.near(x, z) {
            if tri.kind != TunnelFaceKind::Floor {
                continue;
            }
            if let Some((y, normal)) = intersect_floor_xz(tri, x, z) {
                out.push(FloorSample {
                    point: [x, y, z],
                    normal,
                    surface: FloorSurface::TunnelFloor,
                });
            }
        }
        out
    }

    /// True when a floor triangle within the query bucket around `(x, z)` spans height `y` (± `tol`):
    /// the body is beside the tunnel system at its own height, even if just off a floor's edge.
    pub fn floor_near(&self, x: f32, z: f32, y: f32, tol: f32) -> bool {
        self.near(x, z).any(|tri| {
            tri.kind == TunnelFaceKind::Floor
                && y >= det_min(det_min(tri.a[1], tri.b[1]), tri.c[1]) - tol
                && y <= det_max(det_max(tri.a[1], tri.b[1]), tri.c[1]) + tol
        })
    }

    /// Floor whose height is closest to `hint_y`.
    pub fn sample_floor_near(&self, x: f32, z: f32, hint_y: f32) -> Option<FloorSample> {
        self.floors_at(x, z).into_iter().min_by(|a, b| {
            (a.point[1] - hint_y)
                .abs()
                .partial_cmp(&(b.point[1] - hint_y).abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        })
    }

    /// Lowest ceiling triangle above `(x, z)`, if any.
    pub fn ceiling_y_at(&self, x: f32, z: f32) -> Option<f32> {
        let mut best: Option<f32> = None;
        for tri in self.near(x, z) {
            if tri.kind != TunnelFaceKind::Ceiling {
                continue;
            }
            if let Some((y, _)) = intersect_floor_xz(tri, x, z) {
                best = Some(match best {
                    None => y,
                    Some(by) => det_min(by, y),
                });
            }
        }
        best
    }

    /// True when any floor triangle at `(x, z)` is tagged portal.
    pub fn is_portal_at(&self, x: f32, z: f32) -> bool {
        for tri in self.near(x, z) {
            if tri.kind != TunnelFaceKind::Floor || !tri.portal {
                continue;
            }
            if intersect_floor_xz(tri, x, z).is_some() {
                return true;
            }
        }
        false
    }

    /// Lowest ceiling strictly above `floor_y` at `(x, z)`.
    ///
    /// A deeper deck's ceiling must not count as the lid of a higher deck.
    #[expect(
        clippy::collapsible_if,
        reason = "Stage A: DO27's text is kept as copied (ADR-8050)"
    )]
    pub fn ceiling_above(&self, x: f32, z: f32, floor_y: f32) -> Option<f32> {
        let mut best: Option<f32> = None;
        for tri in self.near(x, z) {
            if tri.kind != TunnelFaceKind::Ceiling {
                continue;
            }
            if let Some((y, _)) = intersect_floor_xz(tri, x, z) {
                if y > floor_y + 0.05 {
                    best = Some(match best {
                        None => y,
                        Some(by) => det_min(by, y),
                    });
                }
            }
        }
        best
    }

    /// How many distinct ceiling heights cover `(x, z)`.
    /// Two triangles of one lid count as one.
    pub fn ceiling_count_at(&self, x: f32, z: f32) -> usize {
        let mut bands: Vec<f32> = Vec::new();
        for tri in self.near(x, z) {
            if tri.kind != TunnelFaceKind::Ceiling {
                continue;
            }
            let Some((y, _)) = intersect_floor_xz(tri, x, z) else {
                continue;
            };
            if bands.iter().any(|band| (band - y).abs() < 0.5) {
                continue;
            }
            bands.push(y);
        }
        bands.len()
    }

    /// Vertical clearance from a floor sample up to the ceiling directly above it.
    pub fn clearance_above(&self, floor: &FloorSample) -> Option<f32> {
        let ceiling = self.ceiling_above(floor.point[0], floor.point[2], floor.point[1])?;
        Some(ceiling - floor.point[1])
    }

    /// Fraction (0..1) along `from -> to` of the first wall or ceiling triangle the segment crosses,
    /// or `None` when the way is clear. Floors are not tested (a camera keeps off them with a floor
    /// query). Both faces count: a camera behind a wall is as blocked as one in front of it.
    #[expect(
        clippy::collapsible_if,
        reason = "Stage A: DO27's text is kept as copied (ADR-8050)"
    )]
    pub fn segment_blocker(&self, from: [f32; 3], to: [f32; 3]) -> Option<f32> {
        let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
        let len = math::sqrt(d[0] * d[0] + d[1] * d[1] + d[2] * d[2]);
        if len < 1e-6 {
            return None;
        }
        // Every bucket the segment passes through; buckets are 16 m, so a 6 m step visits each.
        let steps = usize::max((len / 6.0).ceil() as usize, 1);
        let mut ids: Vec<u32> = Vec::new();
        let mut everything = false;
        for i in 0..=steps {
            let t = i as f32 / steps as f32;
            match self.index.at(from[0] + d[0] * t, from[2] + d[2] * t) {
                Some(bucket) => ids.extend_from_slice(bucket),
                None => everything = true,
            }
        }
        ids.sort_unstable();
        ids.dedup();
        let mut best: Option<f32> = None;
        let mut test = |tri: &TunnelTriangle| {
            if tri.kind == TunnelFaceKind::Floor {
                return;
            }
            if let Some(t) = segment_triangle(from, d, tri) {
                if best.is_none_or(|b| t < b) {
                    best = Some(t);
                }
            }
        };
        if everything {
            self.triangles.iter().for_each(&mut test);
        } else {
            ids.iter().for_each(|i| test(&self.triangles[*i as usize]));
        }
        best
    }
}

/// Moller-Trumbore, two-sided. `t` in 0..=1 along `origin + dir * t`.
fn segment_triangle(origin: [f32; 3], dir: [f32; 3], tri: &TunnelTriangle) -> Option<f32> {
    let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let e1 = sub(tri.b, tri.a);
    let e2 = sub(tri.c, tri.a);
    let p = cross(dir, e2);
    let det = dot(e1, p);
    if det.abs() < 1e-9 {
        return None;
    }
    let inv = 1.0 / det;
    let s = sub(origin, tri.a);
    let u = dot(s, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(s, e1);
    let v = dot(dir, q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let t = dot(e2, q) * inv;
    (0.0..=1.0).contains(&t).then_some(t)
}

/// Clamp hull altitude so the disc stays under the ceiling of the volume.
pub fn clamp_altitude_under_ceiling(
    mesh: Option<&TunnelCollisionMesh>,
    x: f32,
    y: f32,
    z: f32,
    hull_radius: f32,
) -> f32 {
    let Some(mesh) = mesh else {
        return y;
    };
    // Inside a deck: the lid directly overhead. Above every lid: a single
    // gallery still clamps (one volume). A stack of decks leaves the roof
    // on the heightfield instead of pulling the hull into the basement.
    let ceiling = if let Some(ceiling) = mesh.ceiling_above(x, z, y) {
        ceiling
    } else if mesh.ceiling_count_at(x, z) == 1 {
        match mesh.ceiling_y_at(x, z) {
            Some(ceiling) => ceiling,
            None => return y,
        }
    } else {
        return y;
    };
    det_min(y, ceiling - hull_radius)
}

/// Push an XZ disc of `radius` out of wall triangles.
///
/// Walls are vertical, so the test is the disc against each wall's XZ
/// segment. The triangle winding's horizontal normal points into the
/// gallery; a center within `radius` of the segment is moved to that
/// distance on whichever side it already occupies. Floor and ceiling
/// clearance stay in [`sample_floor_with_tunnels`](crate::compat::ground::sample_floor_with_tunnels) and
/// [`clamp_altitude_under_ceiling`].
pub fn separate_disc_from_walls(mesh: &TunnelCollisionMesh, x: f32, z: f32, radius: f32) -> [f32; 2] {
    separate_disc_from_walls_band(mesh, x, z, radius, f32::NEG_INFINITY, f32::INFINITY)
}

/// [`separate_disc_from_walls`] for a body spanning `y_lo..=y_hi`: a wall whose vertical extent misses
/// that band is ignored. Passages overlap in plan (a ramp runs over the tube it joins), so a wall of the
/// tube must not shove a hull driving on the ramp above it, or the other way round.
pub fn separate_disc_from_walls_band(
    mesh: &TunnelCollisionMesh,
    x: f32,
    z: f32,
    radius: f32,
    y_lo: f32,
    y_hi: f32,
) -> [f32; 2] {
    if !(radius.is_finite() && radius > 0.0) || !x.is_finite() || !z.is_finite() {
        return [x, z];
    }
    let mut px = x;
    let mut pz = z;
    for tri in mesh.near(x, z) {
        if tri.kind != TunnelFaceKind::Wall {
            continue;
        }
        let top = det_max(det_max(tri.a[1], tri.b[1]), tri.c[1]);
        let bottom = det_min(det_min(tri.a[1], tri.b[1]), tri.c[1]);
        if top < y_lo || bottom > y_hi {
            continue;
        }
        let Some((nx, nz, ax, az, bx, b_z)) = wall_segment_xz(tri) else {
            continue;
        };
        if y_lo.is_finite() || y_hi.is_finite() {
            // The face's own height where the body meets it (a riser's top follows the floor it climbs to).
            if let Some((lo, hi)) = wall_span_at(tri, ax, az, bx, b_z, px, pz)
                && (hi < y_lo || lo > y_hi)
            {
                continue;
            }
        }
        let dist = nx * (px - ax) + nz * (pz - az);
        if dist >= radius || dist <= -radius {
            continue;
        }
        let abx = bx - ax;
        let abz = b_z - az;
        let len2 = abx * abx + abz * abz;
        if len2 < 1e-8 {
            continue;
        }
        let t = ((px - ax) * abx + (pz - az) * abz) / len2;
        if !(0.0..=1.0).contains(&t) {
            continue;
        }
        let target = if dist >= 0.0 { radius } else { -radius };
        let push = target - dist;
        px += nx * push;
        pz += nz * push;
    }
    [px, pz]
}

/// Vertical extent of a wall triangle on the vertical line through the point of its segment nearest
/// `(px, pz)`; `None` when the line misses it.
fn wall_span_at(
    tri: &TunnelTriangle,
    ax: f32,
    az: f32,
    bx: f32,
    b_z: f32,
    px: f32,
    pz: f32,
) -> Option<(f32, f32)> {
    let (abx, abz) = (bx - ax, b_z - az);
    let len2 = abx * abx + abz * abz;
    if len2 < 1e-8 {
        return None;
    }
    let param = |v: [f32; 3]| ((v[0] - ax) * abx + (v[2] - az) * abz) / len2;
    let t = det_clamp(((px - ax) * abx + (pz - az) * abz) / len2, 0.0, 1.0);
    let verts = [tri.a, tri.b, tri.c];
    let mut lo = f32::INFINITY;
    let mut hi = f32::NEG_INFINITY;
    for i in 0..3 {
        let (p, q) = (verts[i], verts[(i + 1) % 3]);
        let (tp, tq) = (param(p), param(q));
        let (t0, t1) = (det_min(tp, tq), det_max(tp, tq));
        if t < t0 - 1e-4 || t > t1 + 1e-4 {
            continue;
        }
        let y = if (tq - tp).abs() < 1e-6 {
            lo = det_min(lo, det_min(p[1], q[1]));
            hi = det_max(hi, det_max(p[1], q[1]));
            continue;
        } else {
            p[1] + (q[1] - p[1]) * ((t - tp) / (tq - tp))
        };
        lo = det_min(lo, y);
        hi = det_max(hi, y);
    }
    (lo <= hi).then_some((lo, hi))
}

/// Inward XZ normal plus the wall's horizontal segment endpoints.
fn wall_segment_xz(tri: &TunnelTriangle) -> Option<(f32, f32, f32, f32, f32, f32)> {
    let ux = tri.b[0] - tri.a[0];
    let uy = tri.b[1] - tri.a[1];
    let uz = tri.b[2] - tri.a[2];
    let vx = tri.c[0] - tri.a[0];
    let vy = tri.c[1] - tri.a[1];
    let vz = tri.c[2] - tri.a[2];
    let nx = uy * vz - uz * vy;
    let ny = uz * vx - ux * vz;
    let nz = ux * vy - uy * vx;
    let horiz = math::sqrt(nx * nx + nz * nz);
    if horiz < 1e-6 || ny.abs() > horiz {
        return None;
    }
    let inv = 1.0 / horiz;
    let (nx, nz) = (nx * inv, nz * inv);
    let pts = [tri.a, tri.b, tri.c];
    let mut best: Option<(f32, [f32; 3], [f32; 3])> = None;
    for i in 0..3 {
        for j in (i + 1)..3 {
            let dx = pts[i][0] - pts[j][0];
            let dz = pts[i][2] - pts[j][2];
            let d2 = dx * dx + dz * dz;
            let replace = match best {
                None => true,
                Some((bd, _, _)) => d2 > bd,
            };
            if replace {
                best = Some((d2, pts[i], pts[j]));
            }
        }
    }
    let (_, a, b) = best?;
    Some((nx, nz, a[0], a[2], b[0], b[2]))
}

/// Baked nav overlay: waypoints at drivable floor centroids, edges between
/// adjacent floors (including ramps between decks), portals linking out
/// to the heightfield.
///
/// `levels[i]` is how many decks sit above that waypoint, clamped to
/// [`MAX_NAV_LEVELS`] − 1. A path may cross all four decks. A goal on
/// another deck does not require an outdoor portal. A goal out on the
/// heightfield still does: with no portal the unit holds.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TunnelNavOverlay {
    /// Waypoints (floor triangle centroids).
    pub waypoints: Vec<[f32; 3]>,
    /// Undirected edges as index pairs into `waypoints`.
    pub edges: Vec<(usize, usize)>,
    /// Waypoint indices that are portal floors (connect outdoors).
    pub portal_indices: Vec<usize>,
    /// Deck index per waypoint. 0 is the highest floor in the column.
    pub levels: Vec<u8>,
    /// Personnel-only waypoints (building interiors).
    pub pilot_only: Vec<bool>,
}

/// Attach radius: a body this close to a floor centroid is on that deck.
const NAV_ATTACH_XZ_M: f32 = 12.0;
const NAV_ATTACH_Y_M: f32 = 6.0;

impl TunnelNavOverlay {
    /// Bake the overlay from a collision mesh: one waypoint per floor triangle, edges between triangles that share an edge.
    pub fn bake(mesh: &TunnelCollisionMesh) -> Self {
        let mut waypoints = Vec::new();
        let mut portal_indices = Vec::new();
        let mut pilot_only = Vec::new();
        let mut floor_tris: Vec<&TunnelTriangle> = Vec::new();
        for tri in &mesh.triangles {
            if tri.kind != TunnelFaceKind::Floor {
                continue;
            }
            let idx = waypoints.len();
            waypoints.push(centroid(tri));
            if tri.portal {
                portal_indices.push(idx);
            }
            pilot_only.push(tri.pilot_only);
            floor_tris.push(tri);
        }
        let mut edges = Vec::new();
        for i in 0..floor_tris.len() {
            for j in (i + 1)..floor_tris.len() {
                if triangles_share_edge(floor_tris[i], floor_tris[j]) {
                    edges.push((i, j));
                }
            }
        }
        let levels = deck_levels(&waypoints);
        Self {
            waypoints,
            edges,
            portal_indices,
            levels,
            pilot_only,
        }
    }

    /// Next waypoint for a pilot (building decks included).
    pub fn next_waypoint(&self, from: [f32; 3], goal: [f32; 3]) -> Option<[f32; 3]> {
        self.next_waypoint_for(from, goal, true)
    }

    /// Next waypoint. `pilot` false skips personnel-only decks, so a hull
    /// stays out of a building interior and still uses tunnel galleries.
    pub fn next_waypoint_for(&self, from: [f32; 3], goal: [f32; 3], pilot: bool) -> Option<[f32; 3]> {
        if self.waypoints.is_empty() {
            return None;
        }
        let allow: Vec<bool> = self.pilot_only.iter().map(|only| pilot || !only).collect();
        let start = self.attached(from, &allow)?;
        if let Some(goal_node) = self.attached(goal, &allow) {
            let hop = self.first_hop(start, goal_node, &allow)?;
            return Some(self.waypoints[hop]);
        }
        // Outdoor goal: leave through a portal. No portal → hold.
        let portal = self
            .portal_indices
            .iter()
            .copied()
            .filter(|&i| allow.get(i).copied().unwrap_or(false))
            .min_by(|&a, &b| {
                dist_sq(self.waypoints[a], goal)
                    .partial_cmp(&dist_sq(self.waypoints[b], goal))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })?;
        let hop = self.first_hop(start, portal, &allow)?;
        Some(self.waypoints[hop])
    }

    /// Deck index under `pos` when the body is standing on a baked floor.
    pub fn depth_at(&self, pos: [f32; 3]) -> Option<u8> {
        let allow = vec![true; self.waypoints.len()];
        let idx = self.attached(pos, &allow)?;
        self.levels.get(idx).copied()
    }

    fn attached(&self, pos: [f32; 3], allow: &[bool]) -> Option<usize> {
        let mut best: Option<(usize, f32)> = None;
        for (i, waypoint) in self.waypoints.iter().enumerate() {
            if !allow.get(i).copied().unwrap_or(false) {
                continue;
            }
            let dx = waypoint[0] - pos[0];
            let dy = (waypoint[1] - pos[1]).abs();
            let dz = waypoint[2] - pos[2];
            if dx * dx + dz * dz > NAV_ATTACH_XZ_M * NAV_ATTACH_XZ_M || dy > NAV_ATTACH_Y_M {
                continue;
            }
            let d = dx * dx + dy * dy + dz * dz;
            let replace = match best {
                None => true,
                Some((_, bd)) => d < bd,
            };
            if replace {
                best = Some((i, d));
            }
        }
        best.map(|(i, _)| i)
    }

    /// First step of a shortest path. Same-node returns that node.
    fn first_hop(&self, start: usize, target: usize, allow: &[bool]) -> Option<usize> {
        if start == target {
            return Some(start);
        }
        let n = self.waypoints.len();
        let mut dist = vec![f32::INFINITY; n];
        let mut parent = vec![None; n];
        let mut used = vec![false; n];
        dist[start] = 0.0;
        for _ in 0..n {
            let mut current: Option<usize> = None;
            let mut best = f32::INFINITY;
            for i in 0..n {
                if allow.get(i).copied().unwrap_or(false) && !used[i] && dist[i] < best {
                    best = dist[i];
                    current = Some(i);
                }
            }
            let Some(u) = current else {
                break;
            };
            if u == target {
                break;
            }
            used[u] = true;
            for &(a, b) in &self.edges {
                let v = if a == u {
                    b
                } else if b == u {
                    a
                } else {
                    continue;
                };
                if !allow.get(v).copied().unwrap_or(false) {
                    continue;
                }
                let step = math::sqrt(dist_sq(self.waypoints[u], self.waypoints[v]));
                let next = dist[u] + step;
                if next < dist[v] {
                    dist[v] = next;
                    parent[v] = Some(u);
                }
            }
        }
        if !dist[target].is_finite() {
            return None;
        }
        let mut hop = target;
        while let Some(prev) = parent[hop] {
            if prev == start {
                return Some(hop);
            }
            hop = prev;
        }
        None
    }
}

/// Distinct storeys above each waypoint, clamped to the last pathable deck.
/// Counting each triangle would mark every ramp as the bottom of the stack.
fn deck_levels(waypoints: &[[f32; 3]]) -> Vec<u8> {
    const COLUMN_M: f32 = 8.0;
    const STOREY_M: f32 = 2.5;
    waypoints
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let mut bands: Vec<f32> = Vec::new();
            for (j, q) in waypoints.iter().enumerate() {
                if i == j {
                    continue;
                }
                let dx = p[0] - q[0];
                let dz = p[2] - q[2];
                if dx * dx + dz * dz > COLUMN_M * COLUMN_M {
                    continue;
                }
                if q[1] <= p[1] + STOREY_M {
                    continue;
                }
                if bands.iter().any(|band| (band - q[1]).abs() < STOREY_M * 0.6) {
                    continue;
                }
                bands.push(q[1]);
            }
            (bands.len() as u8).min(MAX_NAV_LEVELS - 1)
        })
        .collect()
}

fn centroid(tri: &TunnelTriangle) -> [f32; 3] {
    [
        (tri.a[0] + tri.b[0] + tri.c[0]) / 3.0,
        (tri.a[1] + tri.b[1] + tri.c[1]) / 3.0,
        (tri.a[2] + tri.b[2] + tri.c[2]) / 3.0,
    ]
}

fn triangles_share_edge(a: &TunnelTriangle, b: &TunnelTriangle) -> bool {
    let ae = [a.a, a.b, a.c];
    let be = [b.a, b.b, b.c];
    let mut shared = 0;
    for pa in &ae {
        for pb in &be {
            if points_close(pa, pb) {
                shared += 1;
            }
        }
    }
    shared >= 2
}

fn points_close(a: &[f32; 3], b: &[f32; 3]) -> bool {
    (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3 && (a[2] - b[2]).abs() < 1e-3
}

pub(crate) fn dist_sq(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    let dz = a[2] - b[2];
    dx * dx + dy * dy + dz * dz
}

/// Vertical probe: does the triangle cover `(x, z)` in XZ, and at what y?
pub(crate) fn intersect_floor_xz(tri: &TunnelTriangle, x: f32, z: f32) -> Option<(f32, [f32; 3])> {
    let (y, normal) = plane_y_and_normal(tri, x, z)?;
    if point_in_triangle_xz(tri, x, z) {
        Some((y, normal))
    } else {
        None
    }
}

pub(crate) fn plane_y_and_normal(tri: &TunnelTriangle, x: f32, z: f32) -> Option<(f32, [f32; 3])> {
    let ux = tri.b[0] - tri.a[0];
    let uy = tri.b[1] - tri.a[1];
    let uz = tri.b[2] - tri.a[2];
    let vx = tri.c[0] - tri.a[0];
    let vy = tri.c[1] - tri.a[1];
    let vz = tri.c[2] - tri.a[2];
    let nx = uy * vz - uz * vy;
    let ny = uz * vx - ux * vz;
    let nz = ux * vy - uy * vx;
    let len = math::sqrt(nx * nx + ny * ny + nz * nz);
    if len < 1e-8 || ny.abs() < 1e-6 {
        return None;
    }
    let inv = 1.0 / len;
    let normal = [nx * inv, ny * inv, nz * inv];
    // Plane: n·(p - a) = 0 → y = a.y - (nx*(x-a.x) + nz*(z-a.z)) / ny
    let y = tri.a[1] - (nx * (x - tri.a[0]) + nz * (z - tri.a[2])) / ny;
    // Prefer upward-facing normals for floors/ceilings.
    let normal = if normal[1] < 0.0 {
        [-normal[0], -normal[1], -normal[2]]
    } else {
        normal
    };
    Some((y, normal))
}

fn point_in_triangle_xz(tri: &TunnelTriangle, x: f32, z: f32) -> bool {
    let (x0, z0) = (tri.a[0], tri.a[2]);
    let (x1, z1) = (tri.b[0], tri.b[2]);
    let (x2, z2) = (tri.c[0], tri.c[2]);
    let d = (z1 - z2) * (x0 - x2) + (x2 - x1) * (z0 - z2);
    if d.abs() < 1e-12 {
        return false;
    }
    let a = ((z1 - z2) * (x - x2) + (x2 - x1) * (z - z2)) / d;
    let b = ((z2 - z0) * (x - x2) + (x0 - x2) * (z - z2)) / d;
    let c = 1.0 - a - b;
    a >= -1e-5 && b >= -1e-5 && c >= -1e-5
}

#[cfg(any(test, feature = "fixtures"))]
/// Synthetic gallery for tests: floor at `floor_y`, ceiling at
/// `ceiling_y`, walls on ±half extents, optional portal strip on -Z.
pub fn synthetic_gallery(
    half_x: f32,
    half_z: f32,
    floor_y: f32,
    ceiling_y: f32,
    with_portal: bool,
) -> TunnelCollisionMesh {
    let y0 = floor_y;
    let mut tris = Vec::new();

    // Floor: two triangles covering [-half_x, half_x] x [-half_z, half_z].
    tris.push(TunnelTriangle {
        a: [-half_x, y0, -half_z],
        b: [half_x, y0, -half_z],
        c: [half_x, y0, half_z],
        kind: TunnelFaceKind::Floor,
        portal: false,
        pilot_only: false,
    });
    tris.push(TunnelTriangle {
        a: [-half_x, y0, -half_z],
        b: [half_x, y0, half_z],
        c: [-half_x, y0, half_z],
        kind: TunnelFaceKind::Floor,
        portal: false,
        pilot_only: false,
    });

    if with_portal {
        // Portal apron just outside -Z edge (connects outdoor).
        let pz0 = -half_z;
        let pz1 = -half_z - 2.0;
        tris.push(TunnelTriangle {
            a: [-half_x, y0, pz1],
            b: [half_x, y0, pz1],
            c: [half_x, y0, pz0],
            kind: TunnelFaceKind::Floor,
            portal: true,
            pilot_only: false,
        });
        tris.push(TunnelTriangle {
            a: [-half_x, y0, pz1],
            b: [half_x, y0, pz0],
            c: [-half_x, y0, pz0],
            kind: TunnelFaceKind::Floor,
            portal: true,
            pilot_only: false,
        });
    }

    // Ceiling.
    tris.push(TunnelTriangle {
        a: [-half_x, ceiling_y, -half_z],
        b: [half_x, ceiling_y, half_z],
        c: [half_x, ceiling_y, -half_z],
        kind: TunnelFaceKind::Ceiling,
        portal: false,
        pilot_only: false,
    });
    tris.push(TunnelTriangle {
        a: [-half_x, ceiling_y, -half_z],
        b: [-half_x, ceiling_y, half_z],
        c: [half_x, ceiling_y, half_z],
        kind: TunnelFaceKind::Ceiling,
        portal: false,
        pilot_only: false,
    });

    // Four walls (each two tris).
    let walls = [
        // -X
        (
            [-half_x, y0, -half_z],
            [-half_x, ceiling_y, -half_z],
            [-half_x, ceiling_y, half_z],
            [-half_x, y0, half_z],
        ),
        // +X
        (
            [half_x, y0, -half_z],
            [half_x, y0, half_z],
            [half_x, ceiling_y, half_z],
            [half_x, ceiling_y, -half_z],
        ),
        // -Z
        (
            [-half_x, y0, -half_z],
            [half_x, y0, -half_z],
            [half_x, ceiling_y, -half_z],
            [-half_x, ceiling_y, -half_z],
        ),
        // +Z
        (
            [-half_x, y0, half_z],
            [-half_x, ceiling_y, half_z],
            [half_x, ceiling_y, half_z],
            [half_x, y0, half_z],
        ),
    ];
    for (a, b, c, d) in walls {
        tris.push(TunnelTriangle {
            a,
            b,
            c,
            kind: TunnelFaceKind::Wall,
            portal: false,
            pilot_only: false,
        });
        tris.push(TunnelTriangle {
            a,
            b: c,
            c: d,
            kind: TunnelFaceKind::Wall,
            portal: false,
            pilot_only: false,
        });
    }

    TunnelCollisionMesh::new(tris)
}

#[cfg(any(test, feature = "fixtures"))]
#[expect(
    clippy::too_many_arguments,
    reason = "Stage A: DO27's text is kept as copied (ADR-8050)"
)]
fn push_floor_quad(
    tris: &mut Vec<TunnelTriangle>,
    a: [f32; 3],
    b: [f32; 3],
    c: [f32; 3],
    d: [f32; 3],
    kind: TunnelFaceKind,
    portal: bool,
    pilot_only: bool,
) {
    tris.push(TunnelTriangle {
        a,
        b,
        c,
        kind,
        portal,
        pilot_only,
    });
    tris.push(TunnelTriangle {
        a,
        b: c,
        c: d,
        kind,
        portal,
        pilot_only,
    });
}

#[cfg(any(test, feature = "fixtures"))]
/// Four stacked galleries joined by ramps. Deck 0 is y = 0. Each deck
/// below drops 4 m. Hulls and pilots both path them.
pub fn synthetic_four_decks() -> TunnelCollisionMesh {
    let mut tris = Vec::new();
    for i in 0..4 {
        let y = -4.0 * i as f32;
        push_floor_quad(
            &mut tris,
            [-4.0, y, -4.0],
            [4.0, y, -4.0],
            [4.0, y, 4.0],
            [-4.0, y, 4.0],
            TunnelFaceKind::Floor,
            false,
            false,
        );
        push_floor_quad(
            &mut tris,
            [4.0, y, -4.0],
            [8.0, y, -4.0],
            [8.0, y, 4.0],
            [4.0, y, 4.0],
            TunnelFaceKind::Floor,
            false,
            false,
        );
        let yc = y + 3.5;
        push_floor_quad(
            &mut tris,
            [-4.0, yc, -4.0],
            [8.0, yc, -4.0],
            [8.0, yc, 4.0],
            [-4.0, yc, 4.0],
            TunnelFaceKind::Ceiling,
            false,
            false,
        );
        if i + 1 < 4 {
            let y_l = y - 4.0;
            tris.push(TunnelTriangle {
                a: [4.0, y, -4.0],
                b: [4.0, y, 4.0],
                c: [8.0, y_l, 4.0],
                kind: TunnelFaceKind::Floor,
                portal: false,
                pilot_only: false,
            });
            tris.push(TunnelTriangle {
                a: [4.0, y, -4.0],
                b: [8.0, y_l, 4.0],
                c: [8.0, y_l, -4.0],
                kind: TunnelFaceKind::Floor,
                portal: false,
                pilot_only: false,
            });
        }
    }
    TunnelCollisionMesh::new(tris)
}

#[cfg(any(test, feature = "fixtures"))]
/// Four personnel decks inside a building. Ground floor (y = 0) is the
/// door portal. Upper decks are reached by stairs, not by a drop.
/// Vehicle pathing ignores the whole stack.
pub fn synthetic_building_decks() -> TunnelCollisionMesh {
    let mut tris = Vec::new();
    let storey = 3.2;
    for i in 0..4 {
        let y = storey * i as f32;
        let portal = i == 0;
        push_floor_quad(
            &mut tris,
            [-4.0, y, -4.0],
            [4.0, y, -4.0],
            [4.0, y, 4.0],
            [-4.0, y, 4.0],
            TunnelFaceKind::Floor,
            portal,
            true,
        );
        push_floor_quad(
            &mut tris,
            [4.0, y, -4.0],
            [8.0, y, -4.0],
            [8.0, y, 4.0],
            [4.0, y, 4.0],
            TunnelFaceKind::Floor,
            portal,
            true,
        );
        let yc = y + 3.0;
        push_floor_quad(
            &mut tris,
            [-4.0, yc, -4.0],
            [8.0, yc, -4.0],
            [8.0, yc, 4.0],
            [-4.0, yc, 4.0],
            TunnelFaceKind::Ceiling,
            false,
            true,
        );
        if i + 1 < 4 {
            let y_u = y + storey;
            tris.push(TunnelTriangle {
                a: [4.0, y, -4.0],
                b: [4.0, y, 4.0],
                c: [8.0, y_u, 4.0],
                kind: TunnelFaceKind::Floor,
                portal: false,
                pilot_only: true,
            });
            tris.push(TunnelTriangle {
                a: [4.0, y, -4.0],
                b: [8.0, y_u, 4.0],
                c: [8.0, y_u, -4.0],
                kind: TunnelFaceKind::Floor,
                portal: false,
                pilot_only: true,
            });
        }
    }
    TunnelCollisionMesh::new(tris)
}
