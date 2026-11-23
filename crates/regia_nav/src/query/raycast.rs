// SPDX-License-Identifier: MPL-2.0
//! A ray along the navmesh surface (NM-1): walks the polygons the XZ segment crosses, through
//! their portal edges, and stops at the first edge with no walkable neighbour (a wall) or whose
//! neighbour the filter leaves out. The standard mesh walk of the public navmesh literature
//! (a convex-polygon clip per polygon, then the portal on the exit edge); original code.

use regia_core::math::Vec3;

use super::{NavQuery, NavQueryError, find_nearest_allowed};
use crate::areas::area_bit;
use crate::links::NavQueryFilter;
use crate::mesh::{NavMesh, NavPoly};

/// How close (metres, XZ) the exit point must lie to a portal edge to cross it.
const PORTAL_TOLERANCE: f32 = 1.0e-3;
/// How far past the exit point (metres) the walk looks to tell which neighbour the ray enters.
const LOOK_AHEAD: f32 = 1.0e-3;

/// The result of [`NavQuery::raycast`].
#[derive(Debug, Clone, PartialEq)]
pub struct NavRaycastHit {
    /// Whether the ray stopped at a wall (or a left-out area) before `end`.
    pub hit: bool,
    /// The fraction of the segment from `start` (snapped) to `end` walked: the wall's position
    /// when `hit`, else 1.
    pub t: f32,
    /// The wall's XZ normal, unit length, pointing back into the walkable side; zero when no hit.
    pub normal: Vec3,
    /// The polygon the walk ended on (the one with the wall when `hit`).
    pub last_poly: u32,
    /// Every polygon the ray crossed, start polygon first.
    pub path_polys: Vec<u32>,
    /// The start point snapped onto the mesh (the ray's origin).
    pub start: Vec3,
    /// Where the walk stopped: `start + t * (end - start)` in XZ, at `last_poly`'s height.
    pub point: Vec3,
}

impl NavQuery<'_> {
    /// Casts a ray along the mesh surface from `start` toward `end` in XZ. `start` snaps onto the
    /// nearest polygon within `extents` that `filter` allows; the walk then crosses polygons
    /// through their shared edges and stops at the first edge with no neighbour, or whose
    /// neighbour is in an area `filter` leaves out. Heights follow the polygons (2.5D: the ray
    /// ignores `end.y`). Off-mesh links are never taken.
    ///
    /// Errors: [`NavQueryError::BadExtents`] for negative or non-finite extents,
    /// [`NavQueryError::InvalidInput`] for a non-finite point, [`NavQueryError::NoStartPoly`] when
    /// no allowed polygon lies within `extents` of `start`.
    ///
    /// ```
    /// # use regia_core::math::Vec3;
    /// # use regia_nav::{NavMeshBuilder, NavQuery, NavQueryFilter};
    /// # let h = 6.0;
    /// # let floor = [
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, -h), Vec3::new(h, 0.0, h)],
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, h), Vec3::new(-h, 0.0, h)],
    /// # ];
    /// # let mesh = NavMeshBuilder::default().build(&floor).unwrap();
    /// let query = NavQuery { mesh: &mesh, obstacles: None };
    /// let filter = NavQueryFilter::default();
    /// // Inside the room: clear.
    /// let clear = query.raycast(Vec3::ZERO, Vec3::new(3.0, 0.0, 0.0), Vec3::splat(1.0), &filter).unwrap();
    /// assert!(!clear.hit);
    /// // Through the east wall: stopped, the normal points back west.
    /// let wall = query.raycast(Vec3::ZERO, Vec3::new(20.0, 0.0, 0.0), Vec3::splat(1.0), &filter).unwrap();
    /// assert!(wall.hit && wall.normal.x < -0.99);
    /// ```
    pub fn raycast(
        &self,
        start: Vec3,
        end: Vec3,
        extents: Vec3,
        filter: &NavQueryFilter,
    ) -> Result<NavRaycastHit, NavQueryError> {
        check_extents(extents)?;
        if !(start.is_finite() && end.is_finite()) {
            return Err(NavQueryError::InvalidInput);
        }
        let _span = tracing::trace_span!("nav_raycast").entered();
        self.with_mesh(|mesh| raycast_on(mesh, start, end, extents, filter))
    }
}

/// Negative or non-finite extents are an error for the NM-1 queries.
pub(crate) fn check_extents(extents: Vec3) -> Result<(), NavQueryError> {
    if extents.is_finite() && extents.cmpge(Vec3::ZERO).all() {
        Ok(())
    } else {
        Err(NavQueryError::BadExtents)
    }
}

fn raycast_on(
    mesh: &NavMesh,
    start: Vec3,
    end: Vec3,
    extents: Vec3,
    filter: &NavQueryFilter,
) -> Result<NavRaycastHit, NavQueryError> {
    let excluded = filter.excluded_areas;
    let (mut cur, origin) =
        find_nearest_allowed(mesh, start, extents, excluded).ok_or(NavQueryError::NoStartPoly)?;
    let dx = end.x - origin.x;
    let dz = end.z - origin.z;
    let len = (dx * dx + dz * dz).sqrt();
    let mut path = vec![cur];
    let finish = |hit: bool, t: f32, normal: Vec3, last: u32, path: Vec<u32>| {
        let y = mesh.polys.get(last as usize).map_or(origin.y, NavPoly::height);
        NavRaycastHit {
            hit,
            t,
            normal,
            last_poly: last,
            path_polys: path,
            start: origin,
            point: Vec3::new(origin.x + dx * t, y, origin.z + dz * t),
        }
    };
    if len <= f32::EPSILON {
        return Ok(finish(false, 1.0, Vec3::ZERO, cur, path));
    }
    let (ux, uz) = (dx / len, dz / len);
    let mut t_in = 0.0f32;
    // Each step moves to a new polygon; a convex mesh walk crosses each at most once.
    for _ in 0..=mesh.polys.len() {
        let Some(poly) = mesh.polys.get(cur as usize) else {
            break;
        };
        let Some((t_exit, edge)) = exit_of(poly, origin, dx, dz) else {
            // A degenerate polygon (no edge to leave by): stop here.
            return Ok(finish(true, t_in, Vec3::ZERO, cur, path));
        };
        let t_exit = t_exit.max(t_in);
        if t_exit >= 1.0 {
            return Ok(finish(false, 1.0, Vec3::ZERO, cur, path));
        }
        let (x, z) = (origin.x + dx * t_exit, origin.z + dz * t_exit);
        let (ax, az) = (x + ux * LOOK_AHEAD, z + uz * LOOK_AHEAD);
        // The neighbour the ray enters: across a portal through the exit point, the one whose
        // footprint holds the point just ahead (the first in portal order). Through a vertex
        // shared only diagonally, the first portal at the exit point; the walk goes on from
        // there at the same `t`.
        let mut ahead_hit = None;
        let mut fallback = None;
        for portal in &poly.portals {
            if seg_dist_sq(x, z, portal.a.x, portal.a.z, portal.b.x, portal.b.z) > PORTAL_TOLERANCE.powi(2) {
                continue;
            }
            let Some(np) = mesh.polys.get(portal.neighbor as usize) else {
                continue;
            };
            if path.contains(&portal.neighbor) {
                continue;
            }
            if np.distance_sq_xz(ax, az) <= 1.0e-8 {
                ahead_hit = Some(portal.neighbor);
                break;
            }
            fallback.get_or_insert(portal.neighbor);
        }
        match ahead_hit.or(fallback) {
            Some(n) if excluded & area_bit(mesh.polys[n as usize].area) == 0 => {
                cur = n;
                path.push(n);
                t_in = t_exit;
            }
            _ => return Ok(finish(true, t_exit, inward_normal(poly, edge), cur, path)),
        }
    }
    Ok(finish(true, t_in, Vec3::ZERO, cur, path))
}

/// The segment parameter where `origin + t * (dx, dz)` leaves the convex `poly` (XZ), and the
/// index of the edge it leaves by (the lowest index on a tie). `None` for a polygon with no edge
/// the direction moves out through (degenerate).
fn exit_of(poly: &NavPoly, origin: Vec3, dx: f32, dz: f32) -> Option<(f32, usize)> {
    let n = poly.vertices.len();
    if n < 3 {
        return None;
    }
    let center = poly.center_xz();
    let mut best: Option<(f32, usize)> = None;
    for i in 0..n {
        let a = poly.vertices[i];
        let b = poly.vertices[(i + 1) % n];
        let (ox, oz) = outward(a, b, center);
        let den = ox * dx + oz * dz;
        if den <= 0.0 {
            continue;
        }
        let t = (ox * (a.x - origin.x) + oz * (a.z - origin.z)) / den;
        if best.is_none_or(|(bt, _)| t < bt) {
            best = Some((t, i));
        }
    }
    best
}

/// The (unnormalised) XZ normal of edge `a-b` pointing away from `center`.
fn outward(a: Vec3, b: Vec3, center: Vec3) -> (f32, f32) {
    let (ex, ez) = (b.x - a.x, b.z - a.z);
    let (nx, nz) = (ez, -ex);
    if nx * (center.x - a.x) + nz * (center.z - a.z) > 0.0 {
        (-nx, -nz)
    } else {
        (nx, nz)
    }
}

/// The unit XZ normal of `poly`'s edge `edge`, pointing into the polygon.
fn inward_normal(poly: &NavPoly, edge: usize) -> Vec3 {
    let n = poly.vertices.len();
    let a = poly.vertices[edge];
    let b = poly.vertices[(edge + 1) % n];
    let (ox, oz) = outward(a, b, poly.center_xz());
    Vec3::new(-ox, 0.0, -oz).normalize_or_zero()
}

/// Squared XZ distance from `(px, pz)` to the segment `(ax, az)-(bx, bz)`.
pub(crate) fn seg_dist_sq(px: f32, pz: f32, ax: f32, az: f32, bx: f32, bz: f32) -> f32 {
    let (ex, ez) = (bx - ax, bz - az);
    let len_sq = ex * ex + ez * ez;
    let t = if len_sq > 0.0 {
        (((px - ax) * ex + (pz - az) * ez) / len_sq).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (cx, cz) = (ax + ex * t - px, az + ez * t - pz);
    cx * cx + cz * cz
}
