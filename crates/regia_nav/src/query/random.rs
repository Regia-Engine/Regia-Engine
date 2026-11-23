// SPDX-License-Identifier: MPL-2.0
//! Random points on the navmesh (NM-1): an area-weighted polygon pick, then a uniform point in
//! that polygon (a fan triangle picked by area, then the square-root warp of Turk, "Generating
//! Random Points in Triangles", Graphics Gems, 1990). The caller owns the [`Rng`], so a seed in
//! gives the same point out on every platform. Original code.

use std::collections::VecDeque;

use regia_core::math::{Rng, Vec3};

use super::{NavQuery, NavQueryError, find_nearest_allowed};
use crate::areas::area_bit;
use crate::links::NavQueryFilter;
use crate::mesh::NavMesh;

/// Draws per call before [`NavQuery::random_point_in_radius`] falls back to the nearest point.
const MAX_CIRCLE_TRIES: u32 = 32;

impl NavQuery<'_> {
    /// A uniformly random point on the polygons `filter` allows: each polygon is picked with
    /// probability proportional to its XZ area, then a point is drawn uniformly inside it. Returns
    /// the polygon and the point (at the polygon's height), or `None` when no allowed polygon has
    /// any area. Draws four numbers from `rng`; the same `rng` state gives the same point.
    ///
    /// ```
    /// # use regia_core::math::{Rng, Vec3};
    /// # use regia_nav::{NavMeshBuilder, NavQuery, NavQueryFilter};
    /// # let h = 6.0;
    /// # let floor = [
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, -h), Vec3::new(h, 0.0, h)],
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, h), Vec3::new(-h, 0.0, h)],
    /// # ];
    /// # let mesh = NavMeshBuilder::default().build(&floor).unwrap();
    /// let query = NavQuery { mesh: &mesh, obstacles: None };
    /// let (poly, p) = query.random_point(&NavQueryFilter::default(), &mut Rng::new(7)).unwrap();
    /// assert!(mesh.polys[poly as usize].distance_sq_xz(p.x, p.z) < 1e-6);
    /// // Same seed, same point.
    /// assert_eq!(query.random_point(&NavQueryFilter::default(), &mut Rng::new(7)), Some((poly, p)));
    /// ```
    pub fn random_point(&self, filter: &NavQueryFilter, rng: &mut Rng) -> Option<(u32, Vec3)> {
        self.with_mesh(|mesh| {
            let polys: Vec<(u32, &[Vec3])> = mesh
                .polys
                .iter()
                .enumerate()
                .filter(|(_, p)| filter.excluded_areas & area_bit(p.area) == 0)
                .map(|(i, p)| (i as u32, p.vertices.as_slice()))
                .collect();
            sample_polygons(&polys, rng).map(|(i, p)| (polys[i].0, p))
        })
    }

    /// A uniformly random point of the allowed mesh within `radius` (XZ) of `center`. With
    /// `reachable_only`, only polygons connected to the center's polygon (through shared edges
    /// and off-mesh links `filter` passes, without leaving the circle) count, so islands are
    /// skipped; the center then must snap onto the mesh within `extents`. Returns `Ok(None)` when
    /// no allowed polygon touches the circle.
    ///
    /// Each try clips the candidate polygons to the circle's bounding square, draws an
    /// area-weighted point there and keeps it when it lies inside the circle (uniform over
    /// mesh ∩ circle); after 32 misses it returns the allowed point nearest `center`. The same
    /// `rng` state gives the same point.
    ///
    /// Errors: [`NavQueryError::BadRadius`] for a negative or non-finite radius,
    /// [`NavQueryError::BadExtents`], [`NavQueryError::InvalidInput`] for a non-finite center,
    /// and [`NavQueryError::NoStartPoly`] when `reachable_only` and the center is off the mesh.
    pub fn random_point_in_radius(
        &self,
        center: Vec3,
        radius: f32,
        extents: Vec3,
        reachable_only: bool,
        filter: &NavQueryFilter,
        rng: &mut Rng,
    ) -> Result<Option<(u32, Vec3)>, NavQueryError> {
        if !(radius.is_finite() && radius >= 0.0) {
            return Err(NavQueryError::BadRadius);
        }
        super::raycast::check_extents(extents)?;
        if !center.is_finite() {
            return Err(NavQueryError::InvalidInput);
        }
        self.with_mesh(|mesh| in_radius_on(mesh, center, radius, extents, reachable_only, filter, rng))
    }
}

fn in_radius_on(
    mesh: &NavMesh,
    center: Vec3,
    radius: f32,
    extents: Vec3,
    reachable_only: bool,
    filter: &NavQueryFilter,
    rng: &mut Rng,
) -> Result<Option<(u32, Vec3)>, NavQueryError> {
    let excluded = filter.excluded_areas;
    let r_sq = radius * radius;
    let touches = |i: u32| {
        mesh.polys
            .get(i as usize)
            .is_some_and(|p| excluded & area_bit(p.area) == 0 && p.distance_sq_xz(center.x, center.z) <= r_sq)
    };
    let candidates: Vec<u32> = if reachable_only {
        let (start, _) =
            find_nearest_allowed(mesh, center, extents, excluded).ok_or(NavQueryError::NoStartPoly)?;
        let links = mesh.link_table();
        let mut seen = vec![false; mesh.polys.len()];
        let mut out = Vec::new();
        let mut queue = VecDeque::new();
        if let Some(s) = seen.get_mut(start as usize) {
            *s = true;
            queue.push_back(start);
        }
        while let Some(p) = queue.pop_front() {
            if touches(p) {
                out.push(p);
            } else if p != start {
                continue;
            }
            let poly = &mesh.polys[p as usize];
            let portal_next = poly.portals.iter().map(|q| q.neighbor);
            let link_next = links
                .edges_from(p)
                .iter()
                .filter(|e| filter.passes_flags(e.flags) && excluded & area_bit(e.area) == 0)
                .map(|e| e.to_poly);
            for n in portal_next.chain(link_next) {
                if let Some(s) = seen.get_mut(n as usize)
                    && !*s
                {
                    *s = true;
                    queue.push_back(n);
                }
            }
        }
        out.sort_unstable();
        out
    } else {
        (0..mesh.polys.len() as u32).filter(|&i| touches(i)).collect()
    };
    if candidates.is_empty() {
        return Ok(None);
    }
    // Each candidate clipped to the circle's bounding square: uniform there, then kept in the
    // circle, is uniform over the circle.
    let (x0, x1, z0, z1) = (
        center.x - radius,
        center.x + radius,
        center.z - radius,
        center.z + radius,
    );
    let clipped_storage: Vec<(u32, Vec<Vec3>)> = candidates
        .iter()
        .map(|&i| (i, clip_to_rect(&mesh.polys[i as usize].vertices, x0, x1, z0, z1)))
        .collect();
    let clipped: Vec<(u32, &[Vec3])> = clipped_storage.iter().map(|(i, v)| (*i, v.as_slice())).collect();
    for _ in 0..MAX_CIRCLE_TRIES {
        let Some((k, p)) = sample_polygons(&clipped, rng) else {
            break;
        };
        let (dx, dz) = (p.x - center.x, p.z - center.z);
        if dx * dx + dz * dz <= r_sq {
            return Ok(Some((clipped[k].0, p)));
        }
    }
    // The circle barely touches the mesh: the allowed point nearest the center (within radius).
    let mut best: Option<(f32, u32, Vec3)> = None;
    for &i in &candidates {
        let c = mesh.polys[i as usize].closest_point_xz(center.x, center.z);
        let d = (c.x - center.x).powi(2) + (c.z - center.z).powi(2);
        if best.is_none_or(|(bd, ..)| d < bd) {
            best = Some((d, i, c));
        }
    }
    Ok(best.map(|(_, i, c)| (i, c)))
}

/// Twice the XZ area of a convex polygon (shoelace, absolute).
fn area2(v: &[Vec3]) -> f32 {
    let n = v.len();
    if n < 3 {
        return 0.0;
    }
    let mut s = 0.0f32;
    for i in 0..n {
        let (a, b) = (v[i], v[(i + 1) % n]);
        s += a.x * b.z - b.x * a.z;
    }
    s.abs()
}

/// Twice the XZ area of triangle `a-b-c` (absolute).
fn tri_area2(a: Vec3, b: Vec3, c: Vec3) -> f32 {
    ((b.x - a.x) * (c.z - a.z) - (c.x - a.x) * (b.z - a.z)).abs()
}

/// An area-weighted pick among `polys`, then a uniform point inside the picked one. Returns the
/// index into `polys` and the point (at the polygon's height). Draws four numbers.
fn sample_polygons(polys: &[(u32, &[Vec3])], rng: &mut Rng) -> Option<(usize, Vec3)> {
    let areas: Vec<f32> = polys.iter().map(|(_, v)| area2(v)).collect();
    let total: f32 = areas.iter().sum();
    if total.is_nan() || total <= 0.0 {
        return None;
    }
    let pick = rng.f32() * total;
    let mut acc = 0.0f32;
    // The last polygon with area takes any rounding left at the top of the range.
    let mut chosen = areas.iter().rposition(|&a| a > 0.0)?;
    for (k, &a) in areas.iter().enumerate() {
        acc += a;
        if a > 0.0 && pick < acc {
            chosen = k;
            break;
        }
    }
    let point = sample_convex(polys[chosen].1, rng)?;
    Some((chosen, point))
}

/// A uniform point in a convex polygon: a fan triangle by area, then Turk's warp
/// `p = (1 - √s) a + √s (1 - t) b + √s t c`. Draws three numbers.
fn sample_convex(v: &[Vec3], rng: &mut Rng) -> Option<Vec3> {
    let n = v.len();
    if n < 3 {
        return None;
    }
    let total = area2(v);
    let pick = rng.f32() * total;
    let mut acc = 0.0f32;
    let mut tri = n - 2;
    for k in 1..n - 1 {
        acc += tri_area2(v[0], v[k], v[k + 1]);
        if pick < acc {
            tri = k;
            break;
        }
    }
    let (a, b, c) = (v[0], v[tri], v[tri + 1]);
    let s = rng.f32().sqrt();
    let t = rng.f32();
    let (wa, wb, wc) = (1.0 - s, s * (1.0 - t), s * t);
    Some(Vec3::new(
        wa * a.x + wb * b.x + wc * c.x,
        a.y,
        wa * a.z + wb * b.z + wc * c.z,
    ))
}

/// A convex polygon clipped to the XZ rectangle `[x0, x1] x [z0, z1]` (Sutherland-Hodgman, one
/// pass per side). The result keeps the polygon's height.
fn clip_to_rect(v: &[Vec3], x0: f32, x1: f32, z0: f32, z1: f32) -> Vec<Vec3> {
    let mut out = v.to_vec();
    // (axis is x?, bound, keep the side >= bound?)
    for (on_x, bound, keep_ge) in [
        (true, x0, true),
        (true, x1, false),
        (false, z0, true),
        (false, z1, false),
    ] {
        let input = std::mem::take(&mut out);
        let n = input.len();
        if n == 0 {
            break;
        }
        let coord = |p: Vec3| if on_x { p.x } else { p.z };
        let inside = |p: Vec3| {
            if keep_ge {
                coord(p) >= bound
            } else {
                coord(p) <= bound
            }
        };
        for i in 0..n {
            let (a, b) = (input[i], input[(i + 1) % n]);
            let (ia, ib) = (inside(a), inside(b));
            if ia {
                out.push(a);
            }
            if ia != ib {
                let t = (bound - coord(a)) / (coord(b) - coord(a));
                out.push(a + (b - a) * t);
            }
        }
    }
    out
}
