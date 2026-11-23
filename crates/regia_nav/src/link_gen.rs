// SPDX-License-Identifier: MPL-2.0
//! Automatic off-mesh links (`docs/specs/92-navmesh-offmesh.md` § 3.2, ADR-1821): drops off
//! ledges and jumps across gaps, found from the mesh's wall edges.
//!
//! # Method
//!
//! Every polygon edge part without a portal is a wall. Sample points are spread evenly along each
//! wall part (`max(1, min(4096, floor(length / spacing)))` of them, centred). From each sample the generator
//! probes outward (perpendicular to the edge, away from the polygon centre) every `cell_size`, up
//! to `2 * agent.radius + max_jump_gap` (the walkable surface is eroded by the agent radius on both
//! sides of a gap). At each probe it looks at the polygons whose footprint holds the probe point:
//!
//! - a polygon whose rise above the sample exceeds `max(max_jump_up, max_climb)` stops the
//!   probe (a wall top; a rise the agent can still step is not a wall);
//! - one lower than `max_drop` below is ignored (the floor of a pit too deep to drop into);
//! - otherwise the highest one is the landing: a bidirectional **Jump** when it is within
//!   `max_climb` of the sample height (only after at least one probe found nothing: an adjacent
//!   level polygon without a portal is not a gap), a one-way **Jump** up when higher, and a one-way
//!   **Drop** when lower.
//!
//! The take-off point sits `agent.radius` inside the source polygon and the landing
//! `agent.radius` past the probe, both clamped onto their polygons. A bidirectional jump is skipped
//! when the reverse of one already generated joins the same polygons within `spacing`.
//!
//! The mesh cannot tell a thin wall from a gap, so every candidate goes through a clearance
//! callback. [`TriangleClearance`] is the built-in one: it tests the move against the level
//! triangles at knee and waist height (segment against triangle, Möller and Trumbore 1997).

use std::collections::BTreeMap;

use regia_core::math::Vec3;

use crate::links::{MAX_NAV_LINKS, NavLinkError, NavLinkFlags, NavLinkKind};
use crate::mesh::{NavMesh, OffMeshLink};

/// Settings of [`generate_links`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLinkGenParams {
    /// The deepest drop, metres (`>= 0`; default 3).
    pub max_drop: f32,
    /// The widest gap a jump crosses, metres, measured between the walkable edges once the agent
    /// radius is added back on both sides (`>= 0`; default 2).
    pub max_jump_gap: f32,
    /// The highest jump up, metres (`>= 0`; default 1).
    pub max_jump_up: f32,
    /// Distance between samples along a wall, metres (`> 0`; default 2).
    pub spacing: f32,
    /// The endpoint snap radius of the generated links, metres (`> 0`; default 0.5).
    pub radius: f32,
    /// The user id of the first generated link; the rest count up (default `1 << 48`).
    pub first_user_id: u64,
    /// The filter bits of the generated links (default [`NavLinkFlags::WALK`]).
    pub flags: u16,
}

impl Default for NavLinkGenParams {
    fn default() -> Self {
        Self {
            max_drop: 3.0,
            max_jump_gap: 2.0,
            max_jump_up: 1.0,
            spacing: 2.0,
            radius: 0.5,
            first_user_id: 1 << 48,
            flags: NavLinkFlags::WALK,
        }
    }
}

impl NavLinkGenParams {
    /// Checks the values: finite, distances non-negative, spacing and radius positive.
    pub fn validate(&self) -> Result<(), NavLinkError> {
        let all = [
            self.max_drop,
            self.max_jump_gap,
            self.max_jump_up,
            self.spacing,
            self.radius,
        ];
        if !all.iter().all(|v| v.is_finite()) {
            return Err(NavLinkError::InvalidParams("every parameter must be finite"));
        }
        if self.max_drop < 0.0 || self.max_jump_gap < 0.0 || self.max_jump_up < 0.0 {
            return Err(NavLinkError::InvalidParams(
                "max_drop, max_jump_gap and max_jump_up must not be negative",
            ));
        }
        if self.spacing <= 0.0 || self.radius <= 0.0 {
            return Err(NavLinkError::InvalidParams("spacing and radius must be positive"));
        }
        Ok(())
    }
}

/// A link the generator found, before the clearance callback accepts it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLinkCandidate {
    /// Take-off point (on `from_poly`).
    pub from: Vec3,
    /// Landing point (on `to_poly`).
    pub to: Vec3,
    /// Drop or jump.
    pub kind: NavLinkKind,
    /// Whether the move works both ways (the floors are within `max_climb` of each other).
    pub bidirectional: bool,
    /// The take-off polygon.
    pub from_poly: u32,
    /// The landing polygon.
    pub to_poly: u32,
}

/// Generates drop and jump links for `mesh` (see the module docs). `clearance` accepts or
/// rejects each candidate; pass `|_| true` to keep them all, or a [`TriangleClearance`]. The
/// output order is deterministic. Errors: invalid parameters, or more than `MAX_NAV_LINKS` links.
pub fn generate_links(
    mesh: &NavMesh,
    params: &NavLinkGenParams,
    mut clearance: impl FnMut(&NavLinkCandidate) -> bool,
) -> Result<Vec<OffMeshLink>, NavLinkError> {
    params.validate()?;
    let _span = tracing::info_span!("nav_generate_links", polys = mesh.polys.len()).entered();
    let cell = mesh.config.cell_size.max(1e-3);
    let r = mesh.config.agent.radius.max(0.0);
    let climb = mesh.config.agent.max_climb.max(0.0);
    let reach = 2.0 * r + params.max_jump_gap + cell;
    let probes = (reach / cell).ceil().max(1.0) as usize;

    let mut out: Vec<OffMeshLink> = Vec::new();
    // Bidirectional jumps by unordered polygon pair, to skip the reverse duplicates.
    let mut level_jumps: BTreeMap<(u32, u32), Vec<usize>> = BTreeMap::new();
    let mut prober = Prober {
        mesh,
        cell,
        probes,
        max_jump_up: params.max_jump_up,
        max_drop: params.max_drop,
        climb,
        hits: Vec::new(),
    };

    for (pi, poly) in mesh.polys.iter().enumerate() {
        let pi = pi as u32;
        let h = poly.height();
        let centre = poly.center_xz();
        let n = poly.vertices.len();
        for e in 0..n {
            let (a, b) = (poly.vertices[e], poly.vertices[(e + 1) % n]);
            let (ex, ez) = (b.x - a.x, b.z - a.z);
            let len = (ex * ex + ez * ez).sqrt();
            if len < 1e-4 {
                continue;
            }
            let (dx, dz) = (ex / len, ez / len);
            // The perpendicular pointing away from the centre.
            let (mut nx, mut nz) = (dz, -dx);
            let (mx, mz) = ((a.x + b.x) * 0.5 - centre.x, (a.z + b.z) * 0.5 - centre.z);
            if nx * mx + nz * mz < 0.0 {
                (nx, nz) = (-nx, -nz);
            }
            for (t0, t1) in wall_parts(poly, a, dx, dz, len) {
                let part = t1 - t0;
                let count = ((part / params.spacing)
                    .floor()
                    .min(MAX_SAMPLES_PER_WALL_PART as f32) as usize)
                    .max(1);
                for k in 0..count {
                    let t = t0 + part * (k as f32 + 0.5) / count as f32;
                    let s = Vec3::new(a.x + dx * t, h, a.z + dz * t);
                    let Some((ti, hit)) = prober.run(pi, s, (nx, nz)) else {
                        continue;
                    };
                    let target = &mesh.polys[ti as usize];
                    let th = target.height();
                    let dh = th - h;
                    let (kind, bidirectional) = if dh.abs() <= climb {
                        (NavLinkKind::Jump, true)
                    } else if dh > 0.0 {
                        (NavLinkKind::Jump, false)
                    } else {
                        (NavLinkKind::Drop, false)
                    };
                    let from = poly.closest_point_xz(s.x - nx * r, s.z - nz * r);
                    let to = target.closest_point_xz(hit.x + nx * r, hit.z + nz * r);
                    if bidirectional {
                        let key = (pi.min(ti), pi.max(ti));
                        let duplicate = level_jumps.get(&key).is_some_and(|list| {
                            list.iter().any(|&j| {
                                let l = &out[j];
                                xz_dist(l.to, from) < params.spacing && xz_dist(l.from, to) < params.spacing
                            })
                        });
                        if duplicate {
                            continue;
                        }
                    }
                    let candidate = NavLinkCandidate {
                        from,
                        to,
                        kind,
                        bidirectional,
                        from_poly: pi,
                        to_poly: ti,
                    };
                    if !clearance(&candidate) {
                        continue;
                    }
                    if out.len() >= MAX_NAV_LINKS {
                        return Err(NavLinkError::TooManyLinks(out.len() + 1));
                    }
                    if bidirectional {
                        level_jumps
                            .entry((pi.min(ti), pi.max(ti)))
                            .or_default()
                            .push(out.len());
                    }
                    let mut link = OffMeshLink::new(from, to)
                        .with_kind(kind)
                        .with_user_id(params.first_user_id.wrapping_add(out.len() as u64))
                        .with_flags(params.flags)
                        .with_radius(params.radius);
                    link.bidirectional = bidirectional;
                    out.push(link);
                }
            }
        }
    }
    Ok(out)
}

/// The parts `[t0, t1]` of the edge `a + (dx, dz) * t`, `t ∈ [0, len]`, that no portal of `poly`
/// covers.
fn wall_parts(poly: &crate::NavPoly, a: Vec3, dx: f32, dz: f32, len: f32) -> Vec<(f32, f32)> {
    let mut covered: Vec<(f32, f32)> = Vec::new();
    for portal in &poly.portals {
        // A portal lies on this edge when both its ends are on the edge's line.
        let off = |p: Vec3| ((p.x - a.x) * dz - (p.z - a.z) * dx).abs();
        if off(portal.a) > 1e-3 || off(portal.b) > 1e-3 {
            continue;
        }
        let ta = (portal.a.x - a.x) * dx + (portal.a.z - a.z) * dz;
        let tb = (portal.b.x - a.x) * dx + (portal.b.z - a.z) * dz;
        let (lo, hi) = (ta.min(tb).max(0.0), ta.max(tb).min(len));
        if hi > lo {
            covered.push((lo, hi));
        }
    }
    covered.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut parts = Vec::new();
    let mut at = 0.0f32;
    for (lo, hi) in covered {
        if lo > at + 1e-3 {
            parts.push((at, lo));
        }
        at = at.max(hi);
    }
    if len > at + 1e-3 {
        parts.push((at, len));
    }
    parts
}

/// The outward probe of one generator run (see the module docs).
struct Prober<'a> {
    mesh: &'a NavMesh,
    cell: f32,
    probes: usize,
    max_jump_up: f32,
    max_drop: f32,
    climb: f32,
    hits: Vec<u32>,
}

impl Prober<'_> {
    /// Probes outward from the wall sample `s` (on polygon `source`, at height `s.y`) along
    /// `(nx, nz)` and returns the landing polygon and the probe point that found it.
    fn run(&mut self, source: u32, s: Vec3, (nx, nz): (f32, f32)) -> Option<(u32, Vec3)> {
        let h = s.y;
        for k in 1..=self.probes {
            let d = self.cell * (k as f32 - 0.5);
            let p = Vec3::new(s.x + nx * d, h, s.z + nz * d);
            self.mesh.candidate_polys(p.x, p.z, 0.0, 0.0, &mut self.hits);
            let mut best: Option<(f32, u32)> = None;
            for &q in &self.hits {
                if q == source {
                    continue;
                }
                let Some(poly) = self.mesh.polys.get(q as usize) else {
                    continue;
                };
                if poly.distance_sq_xz(p.x, p.z) > 1e-8 {
                    continue;
                }
                let ph = poly.height();
                if ph - h > self.max_jump_up.max(self.climb) {
                    // A wall top in the way: nothing beyond is reachable. `max_climb` is
                    // included so a rise the agent can step is still a landing (a
                    // bidirectional jump), not a wall.
                    return None;
                }
                if ph - h < -self.max_drop {
                    continue;
                }
                if best.is_none_or(|(bh, bq)| ph > bh || (ph == bh && q < bq)) {
                    best = Some((ph, q));
                }
            }
            if let Some((th, q)) = best {
                if k == 1 && (th - h).abs() <= self.climb {
                    // A level neighbour right at the edge without a portal is not a gap.
                    return None;
                }
                return Some((q, p));
            }
        }
        None
    }
}

/// [`generate_links`] with a [`TriangleClearance`] over `colliders`, the level's collision
/// triangles (ADR-2420): a thin wall, a railing or a ceiling the navmesh cannot see stops a jump or
/// a drop. Returns the links and how many candidates the triangles rejected. Errors: those of
/// [`generate_links`]. `regia_framework::nav_world::generate_level_links` passes the level's
/// static colliders.
pub fn generate_clear_links(
    mesh: &NavMesh,
    params: &NavLinkGenParams,
    colliders: &[[Vec3; 3]],
) -> Result<(Vec<OffMeshLink>, usize), NavLinkError> {
    let mut clearance = TriangleClearance::for_mesh(colliders, mesh);
    let mut rejected = 0;
    let links = generate_links(mesh, params, |c| {
        let clear = clearance.is_clear(c);
        rejected += usize::from(!clear);
        clear
    })?;
    Ok((links, rejected))
}

fn xz_dist(a: Vec3, b: Vec3) -> f32 {
    ((a.x - b.x).powi(2) + (a.z - b.z).powi(2)).sqrt()
}

/// A clearance test against level triangles for [`generate_links`]: a candidate passes when the
/// move, at knee height (`max_climb + 0.05` m) and at waist height (half the agent height), hits
/// no triangle. A drop moves level to above its landing, then straight down; a jump up rises
/// straight up first, then moves level. A triangle too large to bucket is still tested
/// (ADR-5007).
#[derive(Debug, Clone)]
pub struct TriangleClearance {
    triangles: Vec<[Vec3; 3]>,
    /// Triangles too large to bucket. Every clearance segment is tested against this list.
    oversized: Vec<[Vec3; 3]>,
    /// Triangle indices per XZ bucket.
    buckets: BTreeMap<(i32, i32), Vec<u32>>,
    bucket: f32,
    offsets: [f32; 2],
    /// Segment tests run (for tests and the perf check).
    tests: u64,
}

impl TriangleClearance {
    /// A clearance test over `triangles` for an agent of `height` that climbs `max_climb`.
    pub fn new(triangles: &[[Vec3; 3]], height: f32, max_climb: f32) -> Self {
        let bucket = 2.0f32;
        let mut buckets: BTreeMap<(i32, i32), Vec<u32>> = BTreeMap::new();
        let mut kept = Vec::with_capacity(triangles.len());
        let mut oversized = Vec::new();
        for tri in triangles {
            if !tri.iter().all(|v| v.is_finite()) {
                continue;
            }
            let i = kept.len() as u32;
            let lo = tri[0].min(tri[1]).min(tri[2]);
            let hi = tri[0].max(tri[1]).max(tri[2]);
            let (z0, z1) = (key(lo.z, bucket), key(hi.z, bucket));
            let (x0, x1) = (key(lo.x, bucket), key(hi.x, bucket));
            // One inflated triangle must not allocate billions of map entries. A span past the
            // cap, or whose bucket product does not fit in i64, stays on `oversized` and is
            // tested against every segment (ADR-5007). It is not dropped.
            let count = (i64::from(z1) - i64::from(z0) + 1).checked_mul(i64::from(x1) - i64::from(x0) + 1);
            if count.is_none_or(|n| n > MAX_BUCKETS_PER_TRIANGLE) {
                oversized.push(*tri);
                continue;
            }
            for bz in z0..=z1 {
                for bx in x0..=x1 {
                    buckets.entry((bx, bz)).or_default().push(i);
                }
            }
            kept.push(*tri);
        }
        Self {
            triangles: kept,
            oversized,
            buckets,
            bucket,
            offsets: [max_climb.max(0.0) + 0.05, (height * 0.5).max(max_climb + 0.1)],
            tests: 0,
        }
    }

    /// The clearance test for `mesh`'s agent.
    pub fn for_mesh(triangles: &[[Vec3; 3]], mesh: &NavMesh) -> Self {
        Self::new(triangles, mesh.config.agent.height, mesh.config.agent.max_climb)
    }

    /// How many segment-triangle tests ran.
    pub fn tests(&self) -> u64 {
        self.tests
    }

    /// Whether `candidate`'s move is clear.
    pub fn is_clear(&mut self, candidate: &NavLinkCandidate) -> bool {
        let (from, to) = (candidate.from, candidate.to);
        let corner = if to.y < from.y {
            Vec3::new(to.x, from.y, to.z)
        } else {
            Vec3::new(from.x, to.y, from.z)
        };
        for o in self.offsets {
            let up = Vec3::Y * o;
            if self.segment_hits(from + up, corner + up) || self.segment_hits(corner + up, to + up) {
                return false;
            }
        }
        true
    }

    /// Whether the segment `p → q` hits a triangle.
    fn segment_hits(&mut self, p: Vec3, q: Vec3) -> bool {
        let lo = p.min(q);
        let hi = p.max(q);
        let mut seen: Vec<u32> = Vec::new();
        for bz in key(lo.z, self.bucket)..=key(hi.z, self.bucket) {
            for bx in key(lo.x, self.bucket)..=key(hi.x, self.bucket) {
                if let Some(list) = self.buckets.get(&(bx, bz)) {
                    seen.extend_from_slice(list);
                }
            }
        }
        seen.sort_unstable();
        seen.dedup();
        for i in seen {
            self.tests += 1;
            if segment_triangle(p, q, &self.triangles[i as usize]) {
                return true;
            }
        }
        // Copied so the test counter can move while the triangles are read. The list is short:
        // only triangles past the bucket cap.
        let oversized = self.oversized.clone();
        for tri in &oversized {
            self.tests += 1;
            if segment_triangle(p, q, tri) {
                return true;
            }
        }
        false
    }
}

/// The most 2 m buckets one triangle may cover (a 2 km square). A larger triangle, or one whose
/// bucket product does not fit in `i64`, is tested one by one instead of indexed.
const MAX_BUCKETS_PER_TRIANGLE: i64 = 1 << 20;

/// The most probe samples one wall part gets, so a tiny `spacing` cannot make the loop endless.
const MAX_SAMPLES_PER_WALL_PART: usize = 4096;

fn key(v: f32, bucket: f32) -> i32 {
    (v / bucket).floor().clamp(i32::MIN as f32, i32::MAX as f32) as i32
}

/// Whether segment `p → q` crosses triangle `t` (Möller and Trumbore, "Fast, Minimum Storage
/// Ray/Triangle Intersection", 1997, with the hit distance limited to the segment). Hits at the
/// very ends of the segment do not count, so a move that starts on a floor is clear.
fn segment_triangle(p: Vec3, q: Vec3, t: &[Vec3; 3]) -> bool {
    let dir = q - p;
    let e1 = t[1] - t[0];
    let e2 = t[2] - t[0];
    let pv = dir.cross(e2);
    let det = e1.dot(pv);
    if det.abs() < 1e-9 {
        return false;
    }
    let inv = 1.0 / det;
    let tv = p - t[0];
    let u = tv.dot(pv) * inv;
    if !(0.0..=1.0).contains(&u) {
        return false;
    }
    let qv = tv.cross(e1);
    let v = dir.dot(qv) * inv;
    if v < 0.0 || u + v > 1.0 {
        return false;
    }
    let s = e2.dot(qv) * inv;
    s > 1e-4 && s < 1.0 - 1e-4
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NavMeshBuilder;
    use crate::config::NavMeshBuildParams;
    use crate::query::NavQuery;

    fn rect(x0: f32, z0: f32, x1: f32, z1: f32, y: f32) -> [[Vec3; 3]; 2] {
        [
            [Vec3::new(x0, y, z0), Vec3::new(x1, y, z1), Vec3::new(x1, y, z0)],
            [Vec3::new(x0, y, z0), Vec3::new(x0, y, z1), Vec3::new(x1, y, z1)],
        ]
    }

    fn params() -> NavMeshBuildParams {
        NavMeshBuildParams {
            cell_size: 0.25,
            ..Default::default()
        }
    }

    fn build(tris: &[[Vec3; 3]]) -> NavMesh {
        NavMeshBuilder::new(params())
            .build(tris)
            .expect("the test level builds")
    }

    /// A 2 m high ledge `x ∈ [0, 6)` above a floor `x ∈ [6, 12)`, both 6 m deep, with the
    /// ledge's cliff face.
    fn ledge() -> Vec<[Vec3; 3]> {
        let mut t: Vec<[Vec3; 3]> = rect(0.0, 0.0, 6.0, 6.0, 2.0).into();
        t.extend(rect(6.0, 0.0, 12.0, 6.0, 0.0));
        t.push([
            Vec3::new(6.0, 0.0, 0.0),
            Vec3::new(6.0, 2.0, 0.0),
            Vec3::new(6.0, 2.0, 6.0),
        ]);
        t.push([
            Vec3::new(6.0, 0.0, 0.0),
            Vec3::new(6.0, 2.0, 6.0),
            Vec3::new(6.0, 0.0, 6.0),
        ]);
        t
    }

    /// Two level floors `x ∈ [0, 6)` and `x ∈ [6 + gap, 12 + gap)` with an open gap between.
    fn gap(gap: f32) -> Vec<[Vec3; 3]> {
        let mut t: Vec<[Vec3; 3]> = rect(0.0, 0.0, 6.0, 6.0, 0.0).into();
        t.extend(rect(6.0 + gap, 0.0, 12.0 + gap, 6.0, 0.0));
        t
    }

    #[test]
    fn a_ledge_gets_one_way_drops_and_a_path_down() {
        let mesh = build(&ledge());
        let links = generate_links(&mesh, &NavLinkGenParams::default(), |_| true).expect("valid");
        let drops: Vec<&OffMeshLink> = links.iter().filter(|l| l.kind == NavLinkKind::Drop).collect();
        assert!(!drops.is_empty(), "drops off the ledge: {links:?}");
        for d in &drops {
            assert!(!d.bidirectional);
            assert!(d.from.y > 1.9 && d.to.y < 0.1, "{d:?}");
            assert!(d.from.x < 6.0 && d.to.x > 6.0, "{d:?}");
        }
        // The 2 m step up is above max_jump_up (1 m): no jump back.
        assert!(links.iter().all(|l| l.kind == NavLinkKind::Drop), "{links:?}");
        let mesh = mesh.with_off_mesh_links(links);
        let q = NavQuery {
            mesh: &mesh,
            obstacles: None,
        };
        assert!(
            q.find_path(
                Vec3::new(1.0, 2.0, 3.0),
                Vec3::new(11.0, 0.0, 3.0),
                Vec3::splat(1.0)
            )
            .is_some()
        );
        assert!(
            q.find_path(
                Vec3::new(11.0, 0.0, 3.0),
                Vec3::new(1.0, 2.0, 3.0),
                Vec3::splat(1.0)
            )
            .is_none(),
            "one way only"
        );
    }

    #[test]
    fn a_low_ledge_also_gets_jumps_up() {
        let mut t: Vec<[Vec3; 3]> = rect(0.0, 0.0, 6.0, 6.0, 0.8).into();
        t.extend(rect(6.0, 0.0, 12.0, 6.0, 0.0));
        let mesh = build(&t);
        let links = generate_links(&mesh, &NavLinkGenParams::default(), |_| true).expect("valid");
        assert!(links.iter().any(|l| l.kind == NavLinkKind::Drop));
        assert!(
            links
                .iter()
                .any(|l| l.kind == NavLinkKind::Jump && !l.bidirectional && l.to.y > l.from.y),
            "a 0.8 m step is within max_jump_up: {links:?}"
        );
    }

    /// 0.4 m is inside the default `max_climb` (0.5 m), so a gap is one bidirectional jump,
    /// not a one-way drop plus a one-way jump.
    #[test]
    fn a_step_within_climb_across_a_gap_is_one_bidirectional_jump() {
        let mut t: Vec<[Vec3; 3]> = rect(0.0, 0.0, 6.0, 6.0, 0.4).into();
        t.extend(rect(7.5, 0.0, 13.5, 6.0, 0.0));
        let mesh = build(&t);
        let links = generate_links(&mesh, &NavLinkGenParams::default(), |_| true).expect("valid");
        assert!(
            !links.is_empty()
                && links
                    .iter()
                    .all(|l| l.kind == NavLinkKind::Jump && l.bidirectional),
            "within max_climb is one bidirectional jump: {links:?}"
        );
    }

    /// A rise above `max_jump_up` but inside `max_climb` is still a step the agent can take, so
    /// the probe does not treat it as a wall and the link stays bidirectional.
    #[test]
    fn a_rise_inside_climb_stays_a_jump_when_max_jump_up_is_lower() {
        let mut t: Vec<[Vec3; 3]> = rect(0.0, 0.0, 6.0, 6.0, 0.4).into();
        t.extend(rect(7.5, 0.0, 13.5, 6.0, 0.0));
        let mesh = build(&t);
        let params = NavLinkGenParams {
            max_jump_up: 0.2,
            ..NavLinkGenParams::default()
        };
        let links = generate_links(&mesh, &params, |_| true).expect("valid");
        assert!(
            links
                .iter()
                .any(|l| l.kind == NavLinkKind::Jump && l.bidirectional),
            "max(max_jump_up, max_climb) still reaches 0.4 m: {links:?}"
        );
        assert!(
            links.iter().all(|l| l.kind != NavLinkKind::Drop),
            "0.4 m is not deeper than max_climb: {links:?}"
        );
    }

    #[test]
    fn a_gap_gets_bidirectional_jumps_without_duplicates() {
        let mesh = build(&gap(1.5));
        let p = NavLinkGenParams::default();
        let links = generate_links(&mesh, &p, |_| true).expect("valid");
        assert!(!links.is_empty());
        assert!(
            links
                .iter()
                .all(|l| l.kind == NavLinkKind::Jump && l.bidirectional),
            "{links:?}"
        );
        // One side's samples cover the reverse ones: no two links mirror each other.
        for (i, a) in links.iter().enumerate() {
            for b in &links[i + 1..] {
                let mirror = xz_dist(a.from, b.to) < p.spacing && xz_dist(a.to, b.from) < p.spacing;
                assert!(!mirror, "{a:?} mirrors {b:?}");
            }
        }
        let mesh = mesh.with_off_mesh_links(links);
        let q = NavQuery {
            mesh: &mesh,
            obstacles: None,
        };
        assert!(
            q.find_path(
                Vec3::new(1.0, 0.0, 3.0),
                Vec3::new(12.0, 0.0, 3.0),
                Vec3::splat(1.0)
            )
            .is_some()
        );
        assert!(
            q.find_path(
                Vec3::new(12.0, 0.0, 3.0),
                Vec3::new(1.0, 0.0, 3.0),
                Vec3::splat(1.0)
            )
            .is_some()
        );
    }

    #[test]
    fn gaps_too_wide_and_drops_too_deep_get_nothing() {
        let mesh = build(&gap(4.0));
        assert!(
            generate_links(&mesh, &NavLinkGenParams::default(), |_| true)
                .expect("valid")
                .is_empty()
        );
        let mut t: Vec<[Vec3; 3]> = rect(0.0, 0.0, 6.0, 6.0, 5.0).into();
        t.extend(rect(6.0, 0.0, 12.0, 6.0, 0.0));
        let mesh = build(&t);
        let links = generate_links(&mesh, &NavLinkGenParams::default(), |_| true).expect("valid");
        assert!(links.is_empty(), "5 m is deeper than max_drop: {links:?}");
        let deep = NavLinkGenParams {
            max_drop: 6.0,
            ..Default::default()
        };
        assert!(!generate_links(&mesh, &deep, |_| true).expect("valid").is_empty());
    }

    #[test]
    fn clear_links_count_what_the_colliders_reject() {
        let mesh = build(&gap(1.5));
        let params = NavLinkGenParams::default();
        let all = generate_links(&mesh, &params, |_| true).expect("valid");
        let (open, rejected) = generate_clear_links(&mesh, &params, &gap(1.5)).expect("valid");
        assert_eq!(
            (open.len(), rejected),
            (all.len(), 0),
            "the floors alone do not block"
        );
        let mut walled = gap(1.5);
        walled.push([
            Vec3::new(6.75, 0.0, -1.0),
            Vec3::new(6.75, 2.0, -1.0),
            Vec3::new(6.75, 2.0, 7.0),
        ]);
        walled.push([
            Vec3::new(6.75, 0.0, -1.0),
            Vec3::new(6.75, 2.0, 7.0),
            Vec3::new(6.75, 0.0, 7.0),
        ]);
        let (blocked, rejected) = generate_clear_links(&mesh, &params, &walled).expect("valid");
        assert!(blocked.is_empty(), "{blocked:?}");
        assert!(rejected >= all.len(), "every candidate was refused: {rejected}");
    }

    #[test]
    fn a_tiny_spacing_cannot_make_endless_wall_samples() {
        let mesh = build(&gap(1.5));
        let mut seen = 0usize;
        let links = generate_links(
            &mesh,
            &NavLinkGenParams {
                spacing: 1.0e-9,
                ..NavLinkGenParams::default()
            },
            |_| {
                seen += 1;
                true
            },
        )
        .expect("valid");
        // Each wall part contributes at most MAX_SAMPLES_PER_WALL_PART candidates; the default
        // mesh has far fewer than 64 wall parts, so an uncapped loop would count in the millions.
        assert!(seen <= 4096 * 64, "wall samples are not capped: {seen}");
        assert!(!links.is_empty() || seen == 0);
    }

    #[test]
    fn the_callback_rejects_and_triangle_clearance_sees_a_thin_wall() {
        let mesh = build(&gap(1.5));
        let mut seen = 0;
        let none = generate_links(&mesh, &NavLinkGenParams::default(), |_| {
            seen += 1;
            false
        })
        .expect("valid");
        assert!(none.is_empty());
        assert!(seen > 0);

        // The same gap with a 2 m high wall standing in it: the mesh looks the same, the
        // triangles do not.
        let mut walled = gap(1.5);
        walled.push([
            Vec3::new(6.75, 0.0, -1.0),
            Vec3::new(6.75, 2.0, -1.0),
            Vec3::new(6.75, 2.0, 7.0),
        ]);
        walled.push([
            Vec3::new(6.75, 0.0, -1.0),
            Vec3::new(6.75, 2.0, 7.0),
            Vec3::new(6.75, 0.0, 7.0),
        ]);
        let mut clear = TriangleClearance::for_mesh(&walled, &mesh);
        let blocked =
            generate_links(&mesh, &NavLinkGenParams::default(), |c| clear.is_clear(c)).expect("valid");
        assert!(blocked.is_empty(), "the wall blocks every jump: {blocked:?}");
        assert!(clear.tests() > 0);
        let mut open = TriangleClearance::for_mesh(&gap(1.5), &mesh);
        let kept = generate_links(&mesh, &NavLinkGenParams::default(), |c| open.is_clear(c)).expect("valid");
        assert!(!kept.is_empty(), "the floors alone do not block");
    }

    #[test]
    fn drops_clear_their_own_cliff_but_not_a_railing() {
        let level = ledge();
        let mesh = build(&level);
        let mut clear = TriangleClearance::for_mesh(&level, &mesh);
        let links =
            generate_links(&mesh, &NavLinkGenParams::default(), |c| clear.is_clear(c)).expect("valid");
        assert!(
            links.iter().any(|l| l.kind == NavLinkKind::Drop),
            "the cliff face alone does not block"
        );
        let mut railed = level.clone();
        railed.push([
            Vec3::new(5.9, 2.0, -1.0),
            Vec3::new(5.9, 3.2, -1.0),
            Vec3::new(5.9, 3.2, 7.0),
        ]);
        railed.push([
            Vec3::new(5.9, 2.0, -1.0),
            Vec3::new(5.9, 3.2, 7.0),
            Vec3::new(5.9, 2.0, 7.0),
        ]);
        let mut clear = TriangleClearance::for_mesh(&railed, &mesh);
        let links =
            generate_links(&mesh, &NavLinkGenParams::default(), |c| clear.is_clear(c)).expect("valid");
        assert!(
            links.is_empty(),
            "a railing on the edge blocks the drops: {links:?}"
        );
    }

    #[test]
    fn generation_is_deterministic_and_ids_count_up() {
        let mesh = build(&ledge());
        let p = NavLinkGenParams {
            first_user_id: 100,
            ..Default::default()
        };
        let a = generate_links(&mesh, &p, |_| true).expect("valid");
        let b = generate_links(&mesh, &p, |_| true).expect("valid");
        assert_eq!(a, b);
        for (i, l) in a.iter().enumerate() {
            assert_eq!(l.user_id, 100 + i as u64);
            assert_eq!(l.radius, p.radius);
            assert!(l.validate().is_ok());
        }
    }

    #[test]
    fn invalid_parameters_are_typed_errors() {
        let mesh = build(&gap(1.5));
        for bad in [
            NavLinkGenParams {
                spacing: 0.0,
                ..Default::default()
            },
            NavLinkGenParams {
                radius: -1.0,
                ..Default::default()
            },
            NavLinkGenParams {
                max_drop: -1.0,
                ..Default::default()
            },
            NavLinkGenParams {
                max_jump_gap: f32::NAN,
                ..Default::default()
            },
        ] {
            let e = generate_links(&mesh, &bad, |_| true).expect_err("invalid");
            assert_eq!(e.code(), "nav_invalid_link_params");
        }
    }

    #[test]
    fn segment_triangle_hits_inside_only() {
        let t = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
        ];
        assert!(segment_triangle(
            Vec3::new(0.2, 1.0, 0.2),
            Vec3::new(0.2, -1.0, 0.2),
            &t
        ));
        assert!(
            !segment_triangle(Vec3::new(0.9, 1.0, 0.9), Vec3::new(0.9, -1.0, 0.9), &t),
            "outside"
        );
        assert!(
            !segment_triangle(Vec3::new(0.2, 1.0, 0.2), Vec3::new(0.2, 0.5, 0.2), &t),
            "short"
        );
        assert!(
            !segment_triangle(Vec3::new(0.2, 0.0, 0.2), Vec3::new(0.2, 1.0, 0.2), &t),
            "starts on it"
        );
    }

    fn candidate(from: Vec3, to: Vec3) -> NavLinkCandidate {
        NavLinkCandidate {
            from,
            to,
            kind: NavLinkKind::Jump,
            bidirectional: true,
            from_poly: 0,
            to_poly: 1,
        }
    }

    #[test]
    fn a_huge_triangle_is_skipped_not_indexed() {
        let huge = [Vec3::ZERO, Vec3::new(1e5, 0.0, 0.0), Vec3::new(0.0, 0.0, 1e5)];
        let ok = rect(0.0, 0.0, 4.0, 4.0, 0.0);
        let tris = [huge, ok[0], ok[1]];
        let clearance = TriangleClearance::new(&tris, 2.0, 0.5);
        assert_eq!(clearance.triangles.len(), 2, "the huge triangle is not bucketed");
        assert_eq!(clearance.oversized.len(), 1, "it is still tested");
    }

    /// Audit finding 2: a triangle past the bucket cap is still geometry. The knee-height
    /// segment of this jump crosses it, so the jump is not open air.
    #[test]
    fn an_oversized_triangle_still_blocks_clearance() {
        let tri = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(4000.0, 0.0, 0.0),
            Vec3::new(0.0, 4.0, 4000.0),
        ];
        let knee_a = Vec3::new(10.0, 1.0, 0.0);
        let knee_b = Vec3::new(10.0, 1.0, 2000.0);
        assert!(
            segment_triangle(knee_a, knee_b, &tri),
            "the knee segment crosses the triangle"
        );
        let mut clearance = TriangleClearance::new(std::slice::from_ref(&tri), 2.0, 0.5);
        assert!(clearance.triangles.is_empty(), "too many buckets to index");
        assert!(
            !clearance.is_clear(&candidate(
                Vec3::new(10.0, 0.45, 0.0),
                Vec3::new(10.0, 0.45, 2000.0)
            )),
            "a triangle past the bucket cap still blocks the jump"
        );
    }

    /// Audit finding 4: keys at both ends of `i32` make a bucket count of 2^64. `new` must
    /// return and keep the triangle, and a segment that crosses it is not clear.
    ///
    /// A 20 m probe loses the hit: above 2^24 the segment parameter rounds to 0 in f32, so
    /// Möller–Trumbore reports a miss. The knee segment here is long enough that the same
    /// triangle still reports a hit (`u = 0.25`, `v = 0.5`).
    #[test]
    fn a_triangle_across_the_whole_key_range_still_blocks_clearance() {
        let tri = [
            Vec3::new(-5.0e9, 0.0, -5.0e9),
            Vec3::new(5.0e9, 0.0, -5.0e9),
            Vec3::new(0.0, 2.0, 5.0e9),
        ];
        let knee_a = Vec3::new(0.0, 1.0, -10_000.0);
        let knee_b = Vec3::new(0.0, 1.0, 10_000.0);
        assert!(
            segment_triangle(knee_a, knee_b, &tri),
            "the knee segment crosses the triangle"
        );
        let mut clearance = TriangleClearance::new(std::slice::from_ref(&tri), 2.0, 0.5);
        assert!(
            clearance.triangles.is_empty(),
            "the bucket product does not fit in i64"
        );
        assert_eq!(clearance.oversized.len(), 1, "it is still tested");
        assert!(
            !clearance.is_clear(&candidate(
                Vec3::new(0.0, 0.45, -10_000.0),
                Vec3::new(0.0, 0.45, 10_000.0)
            )),
            "an overflowing bucket count is not open air"
        );
    }
}
