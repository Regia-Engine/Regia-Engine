// SPDX-License-Identifier: MPL-2.0
//! Path queries (`docs/specs/22-tier2.md` § T2.2): locating a point on the mesh, and A* over the
//! polygon graph followed by string-pulling (the funnel algorithm) to a smooth path of waypoints.

use std::cell::RefCell;
use std::collections::{BinaryHeap, HashMap};

use regia_core::math::Vec3;

thread_local! {
    /// When `Some`, every polygon `astar` expands is pushed here (tests).
    static ASTAR_EXPANDED: RefCell<Option<Vec<u32>>> = const { RefCell::new(None) };
    /// Reused heap, scores and the neighbour list. `astar` does not call itself, and
    /// `find_path` calls it once, so the buffers are not re-entered.
    static ASTAR_SCRATCH: RefCell<AstarScratch> = RefCell::new(AstarScratch::new());
}

struct AstarScratch {
    open: BinaryHeap<OrderedPoly>,
    g_score: HashMap<u32, f32>,
    came_from: HashMap<u32, u32>,
    neighbors: Vec<(u32, f32)>,
}

impl AstarScratch {
    fn new() -> Self {
        Self {
            open: BinaryHeap::new(),
            g_score: HashMap::new(),
            came_from: HashMap::new(),
            neighbors: Vec::new(),
        }
    }

    fn clear(&mut self) {
        self.open.clear();
        self.g_score.clear();
        self.came_from.clear();
        self.neighbors.clear();
    }
}

/// Records expansions into a fresh list until [`take_astar_expanded`].
#[cfg(test)]
fn begin_astar_trace() {
    ASTAR_EXPANDED.with(|slot| *slot.borrow_mut() = Some(Vec::new()));
}

/// The polygons expanded since [`begin_astar_trace`], in pop order, and stops recording.
#[cfg(test)]
fn take_astar_expanded() -> Vec<u32> {
    ASTAR_EXPANDED.with(|slot| slot.borrow_mut().take().unwrap_or_default())
}

fn note_astar_expansion(poly: u32) {
    ASTAR_EXPANDED.with(|slot| {
        if let Some(out) = slot.borrow_mut().as_mut() {
            out.push(poly);
        }
    });
}

use crate::areas::{StepCosts, area_bit};
use crate::links::{LinkTable, NavQueryFilter};
use crate::mesh::{NavMesh, NavPoly, Portal};
use crate::obstacle::NavObstacles;

mod random;
mod raycast;

pub use raycast::NavRaycastHit;

/// Why a navmesh query (NM-1: [`NavQuery::raycast`], [`NavQuery::random_point_in_radius`]) could
/// not run. [`Self::code`] is the stable agent-protocol reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NavQueryError {
    /// A half-extent is negative or not finite.
    #[error("the search extents must be finite and non-negative")]
    BadExtents,
    /// A radius is negative or not finite.
    #[error("the radius must be finite and non-negative")]
    BadRadius,
    /// A point is not finite.
    #[error("the query point must be finite")]
    InvalidInput,
    /// No allowed polygon lies within the extents of the start (or center) point.
    #[error("no walkable polygon within the extents of the start point")]
    NoStartPoly,
}

impl NavQueryError {
    /// The stable lowercase reason code (agent protocol `data.reason`).
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadExtents => "nav_bad_extents",
            Self::BadRadius => "nav_bad_radius",
            Self::InvalidInput => "nav_invalid_input",
            Self::NoStartPoly => "nav_no_start_poly",
        }
    }
}

/// How [`NavQuery::find_path_partial`] ended.
#[derive(Debug, Clone, PartialEq)]
pub enum NavPathResult {
    /// A path to the goal.
    Complete(NavPath),
    /// The goal is unreachable (or off the mesh): a path to the point nearest the goal on the
    /// reachable polygon nearest it.
    Partial(NavPath),
    /// The start is not on the mesh (no allowed polygon within the extents).
    NoStart,
}

impl NavPathResult {
    /// The path, complete or partial.
    pub fn path(&self) -> Option<&NavPath> {
        match self {
            Self::Complete(p) | Self::Partial(p) => Some(p),
            Self::NoStart => None,
        }
    }

    /// Whether the path reaches the goal.
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }

    /// The lowercase name used by the agent protocol: `complete`, `partial` or `no_start`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Complete(_) => "complete",
            Self::Partial(_) => "partial",
            Self::NoStart => "no_start",
        }
    }
}

/// A path across the mesh: the polygon sequence and the smoothed waypoints.
#[derive(Debug, Clone, PartialEq)]
pub struct NavPath {
    /// The polygon indices traversed (start → end).
    pub polys: Vec<u32>,
    /// The smoothed waypoints (start, corners, end). Walkable waypoints sit at the surface height
    /// of their polygon; off-mesh link waypoints keep the authored link endpoint positions.
    pub waypoints: Vec<Vec3>,
    /// The path length along the waypoints (metres).
    pub length: f32,
}

/// A query over a mesh and its dynamic obstacle set. Mirrors the borrowed-resource query pattern of
/// the physics queries: build one from `&NavMesh` + `&NavObstacles`.
pub struct NavQuery<'a> {
    /// The mesh being queried.
    pub mesh: &'a NavMesh,
    /// The dynamic obstacles (absent or empty → nothing is carved).
    pub obstacles: Option<&'a NavObstacles>,
}

impl<'a> NavQuery<'a> {
    /// Each link's state on this query's mesh. Obstacle boxes block an endpoint the same way the
    /// carved mesh used for path queries does. The stored mesh is unchanged. With no obstacles
    /// this is [`NavMesh::link_statuses`].
    pub fn link_statuses(&self) -> Vec<crate::links::NavLinkStatus> {
        let Some(obstacles) = self.obstacles.filter(|o| !o.is_empty()) else {
            return self.mesh.link_statuses();
        };
        let boxes: Vec<_> = obstacles.boxes().collect();
        self.mesh.link_statuses_with_boxes(&boxes)
    }

    /// Runs `f` on the mesh to plan on. An empty obstacle set uses the base mesh. A non-empty set
    /// uses [`NavObstacles::with_carved`], which rebuilds the carved mesh only when the obstacles
    /// or the mesh content changed.
    fn with_mesh<R>(&self, f: impl FnOnce(&NavMesh) -> R) -> R {
        match self.obstacles {
            Some(obstacles) => obstacles.with_carved(self.mesh, f),
            None => f(self.mesh),
        }
    }

    /// Locates the polygon nearest to `point` within `extents` (a half-extent box), returning the
    /// polygon index and the closest point on the polygon (XZ clamped to the polygon, at its
    /// surface height). A negative or non-finite extent, or a non-finite point, finds nothing.
    ///
    /// ```
    /// # use regia_core::math::Vec3;
    /// # use regia_nav::{NavMeshBuilder, NavObstacles, NavQuery};
    /// # let h = 6.0;
    /// # let floor = [
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, -h), Vec3::new(h, 0.0, h)],
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, h), Vec3::new(-h, 0.0, h)],
    /// # ];
    /// # let mesh = NavMeshBuilder::default().build(&floor).unwrap();
    /// let query = NavQuery { mesh: &mesh, obstacles: None };
    /// // A point 1 m above the floor snaps down onto it.
    /// let (_poly, on_mesh) = query.find_nearest(Vec3::new(1.0, 1.0, 1.0), Vec3::splat(2.0)).unwrap();
    /// assert!(on_mesh.y.abs() < 0.5);
    /// // A point far outside the search box finds nothing.
    /// assert!(query.find_nearest(Vec3::new(50.0, 0.0, 0.0), Vec3::splat(2.0)).is_none());
    /// ```
    pub fn find_nearest(&self, point: Vec3, extents: Vec3) -> Option<(u32, Vec3)> {
        self.with_mesh(|mesh| find_nearest_on(mesh, point, extents))
    }

    /// [`Self::find_nearest`] over the polygons `filter` allows.
    pub fn find_nearest_filtered(
        &self,
        point: Vec3,
        extents: Vec3,
        filter: &NavQueryFilter,
    ) -> Option<(u32, Vec3)> {
        self.with_mesh(|mesh| find_nearest_allowed(mesh, point, extents, filter.excluded_areas))
    }

    /// [`Self::find_nearest`] at the body origin, then 0.9 m lower, then 1.8 m lower
    /// ([`crate::MESH_LOCATE_DROPS`]). The first hit wins. `find_nearest` itself is unchanged.
    pub fn locate_body(&self, point: Vec3, extents: Vec3) -> Option<(u32, Vec3)> {
        for drop in crate::config::MESH_LOCATE_DROPS {
            let at = Vec3::new(point.x, point.y + drop, point.z);
            if let Some(hit) = self.find_nearest(at, extents) {
                return Some(hit);
            }
        }
        None
    }

    /// Computes a path from `start` to `end`, or `None` when either end is not on the mesh or no
    /// route exists. The returned waypoints are the smoothed corridor (string pulling). Every
    /// enabled, unblocked off-mesh link may be used.
    ///
    /// ```
    /// # use regia_core::math::Vec3;
    /// # use regia_nav::{NavMeshBuilder, NavObstacles, NavQuery};
    /// # let h = 6.0;
    /// # let floor = [
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, -h), Vec3::new(h, 0.0, h)],
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, h), Vec3::new(-h, 0.0, h)],
    /// # ];
    /// # let mesh = NavMeshBuilder::default().build(&floor).unwrap();
    /// let query = NavQuery { mesh: &mesh, obstacles: None };
    /// let extents = Vec3::splat(2.0);
    /// let path = query.find_path(Vec3::new(-4.0, 0.0, -4.0), Vec3::new(4.0, 0.0, 4.0), extents).unwrap();
    /// // The diagonal of an 8 m square is about 11.3 m.
    /// assert!((path.length - 8.0 * 2f32.sqrt()).abs() < 0.75);
    /// // An end far off the mesh has no path.
    /// assert!(query.find_path(Vec3::new(-4.0, 0.0, 0.0), Vec3::new(90.0, 0.0, 0.0), extents).is_none());
    /// ```
    pub fn find_path(&self, start: Vec3, end: Vec3, extents: Vec3) -> Option<NavPath> {
        self.find_path_filtered(start, end, extents, &NavQueryFilter::default())
    }

    /// [`Self::find_path`] at `filter`'s area costs, over the areas it allows and only the off-mesh
    /// links that pass it (ADR-1821, ADR-1934). An end in a left-out area snaps to the nearest
    /// allowed polygon within `extents`.
    pub fn find_path_filtered(
        &self,
        start: Vec3,
        end: Vec3,
        extents: Vec3,
        filter: &NavQueryFilter,
    ) -> Option<NavPath> {
        self.with_mesh(|mesh| self.find_path_on(mesh, start, end, extents, filter))
    }

    /// [`Self::find_path_filtered`] that still answers when the goal is unreachable: then the
    /// path runs to the reachable polygon nearest the goal (3D distance from the goal to the
    /// polygon's closest point; ties by the lower polygon index), ending at that polygon's point
    /// nearest the goal. A goal off the mesh (nothing within `extents`) is unreachable too. Only a
    /// start off the mesh gives [`NavPathResult::NoStart`]. A reachable goal gives exactly the
    /// path [`Self::find_path_filtered`] returns.
    ///
    /// ```
    /// # use regia_core::math::Vec3;
    /// # use regia_nav::{NavMeshBuilder, NavPathResult, NavQuery, NavQueryFilter};
    /// # let h = 6.0;
    /// # let floor = [
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, -h), Vec3::new(h, 0.0, h)],
    /// #     [Vec3::new(-h, 0.0, -h), Vec3::new(h, 0.0, h), Vec3::new(-h, 0.0, h)],
    /// # ];
    /// # let mesh = NavMeshBuilder::default().build(&floor).unwrap();
    /// let query = NavQuery { mesh: &mesh, obstacles: None };
    /// let filter = NavQueryFilter::default();
    /// // A goal far off the floor: the path stops at the floor's east edge.
    /// let result = query.find_path_partial(Vec3::ZERO, Vec3::new(40.0, 0.0, 0.0), Vec3::splat(2.0), &filter);
    /// let NavPathResult::Partial(path) = result else { panic!("partial") };
    /// assert!(path.waypoints.last().unwrap().x > 5.0);
    /// ```
    pub fn find_path_partial(
        &self,
        start: Vec3,
        end: Vec3,
        extents: Vec3,
        filter: &NavQueryFilter,
    ) -> NavPathResult {
        self.with_mesh(|mesh| self.find_path_partial_on(mesh, start, end, extents, filter))
    }

    fn find_path_partial_on(
        &self,
        mesh: &NavMesh,
        start: Vec3,
        end: Vec3,
        extents: Vec3,
        filter: &NavQueryFilter,
    ) -> NavPathResult {
        let Some((start_poly, start_pt)) = find_nearest_allowed(mesh, start, extents, filter.excluded_areas)
        else {
            return NavPathResult::NoStart;
        };
        let end_hit = find_nearest_allowed(mesh, end, extents, filter.excluded_areas);
        if let Some((end_poly, end_pt)) = end_hit
            && end_poly == start_poly
        {
            return NavPathResult::Complete(NavPath {
                polys: vec![start_poly],
                waypoints: vec![start_pt, end_pt],
                length: (end_pt - start_pt).length(),
            });
        }
        let links = mesh.link_table();
        // The goal the nearest polygon is measured to: the snapped goal when it is on the mesh.
        let goal = end_hit.map_or(end, |(_, p)| p);
        let searched = astar_or_nearest(mesh, start_poly, end_hit.map(|(p, _)| p), &links, filter, goal);
        match searched {
            Ok(poly_path) => {
                // Reached: `goal` is the snapped end point, as `find_path_on` uses.
                match build_path(mesh, &links, filter, poly_path, start_pt, goal) {
                    Some(path) => NavPathResult::Complete(path),
                    None => NavPathResult::NoStart,
                }
            }
            Err(poly_path) => {
                let last = poly_path.last().copied().unwrap_or(start_poly);
                let end_pt = mesh.polys[last as usize].closest_point_xz(goal.x, goal.z);
                if poly_path.len() <= 1 {
                    return NavPathResult::Partial(NavPath {
                        polys: vec![start_poly],
                        waypoints: vec![start_pt, end_pt],
                        length: (end_pt - start_pt).length(),
                    });
                }
                match build_path(mesh, &links, filter, poly_path, start_pt, end_pt) {
                    Some(path) => NavPathResult::Partial(path),
                    None => NavPathResult::NoStart,
                }
            }
        }
    }

    fn find_path_on(
        &self,
        mesh: &NavMesh,
        start: Vec3,
        end: Vec3,
        extents: Vec3,
        filter: &NavQueryFilter,
    ) -> Option<NavPath> {
        let (start_poly, start_pt) = find_nearest_allowed(mesh, start, extents, filter.excluded_areas)?;
        let (end_poly, end_pt) = find_nearest_allowed(mesh, end, extents, filter.excluded_areas)?;

        if start_poly == end_poly {
            return Some(NavPath {
                polys: vec![start_poly],
                waypoints: vec![start_pt, end_pt],
                length: (end_pt - start_pt).length(),
            });
        }

        // The off-mesh links as polygon-to-polygon edges (resolved once per mesh, ADR-1821).
        let links = mesh.link_table();

        let poly_path = astar(mesh, start_poly, end_poly, &links, filter)?;
        build_path(mesh, &links, filter, poly_path, start_pt, end_pt)
    }
}

/// String-pulls `poly_path` from `start_pt` to `end_pt` into a [`NavPath`].
fn build_path(
    mesh: &NavMesh,
    links: &LinkTable,
    filter: &NavQueryFilter,
    poly_path: Vec<u32>,
    start_pt: Vec3,
    end_pt: Vec3,
) -> Option<NavPath> {
    // Walk the polygon path, string-pulling each corridor of shared-portal edges separately and
    // inserting off-mesh link endpoints as explicit waypoints (Detour's approach: the funnel
    // runs only within a corridor; a jump breaks it). This keeps the path exactly on the link's
    // take-off and landing points instead of cutting the corner.
    let mut waypoints: Vec<Vec3> = vec![start_pt];
    let mut seg_start = start_pt;
    let mut seg_portals: Vec<(Vec3, Vec3)> = Vec::new();

    for w in poly_path.windows(2) {
        let a = w[0];
        let b = w[1];
        if let Some(portal) = portal_between(mesh, a, b) {
            seg_portals.push(portal);
            continue;
        }
        let Some(edge) = links.between(a, b, filter, mesh.area_costs()) else {
            // A valid A* step always has a geometric connection.
            return None;
        };
        let (from, to) = (edge.from, edge.to);
        // Close this corridor at the jump's take-off point, then cross the link.
        let seg = funnel(seg_start, from, &seg_portals);
        waypoints.extend(seg.into_iter().skip(1));
        waypoints.push(to);
        seg_start = to;
        seg_portals.clear();
    }

    let seg = funnel(seg_start, end_pt, &seg_portals);
    waypoints.extend(seg.into_iter().skip(1));

    let mut length = 0.0;
    for pair in waypoints.windows(2) {
        length += (pair[1] - pair[0]).length();
    }

    Some(NavPath {
        polys: poly_path,
        waypoints,
        length,
    })
}

/// Locates the nearest polygon on `mesh` to `point` within the half-extent box `extents`.
pub(crate) fn find_nearest_on(mesh: &NavMesh, point: Vec3, extents: Vec3) -> Option<(u32, Vec3)> {
    find_nearest_allowed(mesh, point, extents, 0)
}

/// [`find_nearest_on`] skipping polygons whose area bit is in `excluded`.
pub(crate) fn find_nearest_allowed(
    mesh: &NavMesh,
    point: Vec3,
    extents: Vec3,
    excluded: u64,
) -> Option<(u32, Vec3)> {
    // A negative half-extent describes an empty box (min above max, as in Detour's
    // `findNearestPoly` overlap test), so it matches nothing (ADR-1384).
    if !(point.is_finite() && extents.is_finite()) || extents.cmplt(Vec3::ZERO).any() {
        return None;
    }
    let (ex, ey, ez) = (extents.x, extents.y, extents.z);
    let mut candidates = Vec::new();
    mesh.candidate_polys(point.x, point.z, ex, ez, &mut candidates);
    // On a layered mesh (ADR-8076) polygons stack in XZ: among equally near ones the polygon
    // whose surface is nearest the point in height wins (a point in a tunnel finds the tunnel
    // floor, not the ground over it). A single-surface mesh keeps the XZ-only order.
    let layered = mesh.layer_count() > 1;
    let mut best: Option<(f32, f32, u32, Vec3)> = None;
    for &poly in &candidates {
        let Some(p) = mesh.polys.get(poly as usize) else {
            continue;
        };
        let dy = (point.y - p.height()).abs();
        if dy > ey || excluded & area_bit(p.area) != 0 {
            continue;
        }
        // The per-axis gap from the point to its closest point on the polygon (0 inside).
        let c = p.closest_point_xz(point.x, point.z);
        if (c.x - point.x).abs() > ex || (c.z - point.z).abs() > ez {
            continue;
        }
        let d = (c.x - point.x) * (c.x - point.x) + (c.z - point.z) * (c.z - point.z);
        let better = match best {
            None => true,
            Some((bd, _, _, _)) if !layered => d < bd,
            Some((bd, bdy, _, _)) => d < bd || (d == bd && dy < bdy),
        };
        if better {
            best = Some((d, dy, poly, c));
        }
    }
    let (_, _, poly, c) = best?;
    Some((poly, c))
}

/// The shared portal between polygons `a` and `b` (the edge they share), or `None`.
pub(crate) fn portal_between(mesh: &NavMesh, a: u32, b: u32) -> Option<(Vec3, Vec3)> {
    let poly = &mesh.polys[a as usize];
    poly.portals.iter().find(|p| p.neighbor == b).map(|p| (p.a, p.b))
}

/// The cost of the step between the centres of two adjacent polygons: the distance, each part
/// priced by the polygon it lies in (split at the portal midpoint). Equal costs skip the split, so
/// a mesh with no area costs plans exactly as distance alone.
fn portal_step_cost(poly: &NavPoly, next: &NavPoly, portal: &Portal, costs: &StepCosts) -> f32 {
    let (a, b) = (poly.center_xz(), next.center_xz());
    let d = ((a.x - b.x).powi(2) + (a.z - b.z).powi(2)).sqrt();
    let (cost_a, cost_b) = (costs.of(poly.area), costs.of(next.area));
    if cost_a == cost_b {
        return d * cost_a;
    }
    let (mx, mz) = ((portal.a.x + portal.b.x) * 0.5, (portal.a.z + portal.b.z) * 0.5);
    let in_a = ((a.x - mx).powi(2) + (a.z - mz).powi(2)).sqrt();
    let in_b = ((b.x - mx).powi(2) + (b.z - mz).powi(2)).sqrt();
    let total = in_a + in_b;
    if total <= 0.0 {
        return d * cost_a.max(cost_b);
    }
    d * (in_a * cost_a + in_b * cost_b) / total
}

/// A* over the polygon graph, returning the polygon index sequence (start → end) or `None`. A
/// polygon step costs its priced distance, and an off-mesh link its own cost times its area's.
fn astar(
    mesh: &NavMesh,
    start: u32,
    end: u32,
    links: &LinkTable,
    filter: &NavQueryFilter,
) -> Option<Vec<u32>> {
    ASTAR_SCRATCH.with(|slot| {
        let mut scratch = slot.borrow_mut();
        scratch.clear();
        astar_in(&mut scratch, mesh, start, Some(end), links, filter)
    })
}

/// A* toward `end` (Dijkstra over the reachable set when `None`): `Ok` with the path when it is
/// reached, else `Err` with the path to the reached polygon nearest `goal` (3D distance to the
/// polygon's closest point; ties by the lower index). Used by [`NavQuery::find_path_partial`].
fn astar_or_nearest(
    mesh: &NavMesh,
    start: u32,
    end: Option<u32>,
    links: &LinkTable,
    filter: &NavQueryFilter,
    goal: Vec3,
) -> Result<Vec<u32>, Vec<u32>> {
    ASTAR_SCRATCH.with(|slot| {
        let mut scratch = slot.borrow_mut();
        scratch.clear();
        if let Some(path) = astar_in(&mut scratch, mesh, start, end, links, filter) {
            return Ok(path);
        }
        // Every polygon with a score was reached; the search ran to exhaustion.
        let mut best: Option<(f32, u32)> = None;
        for &poly in scratch.g_score.keys() {
            let Some(p) = mesh.polys.get(poly as usize) else {
                continue;
            };
            let c = p.closest_point_xz(goal.x, goal.z);
            let d = (c - goal).length_squared();
            let better = best.is_none_or(|(bd, bp)| d < bd || (d == bd && poly < bp));
            if better {
                best = Some((d, poly));
            }
        }
        let nearest = best.map_or(start, |(_, p)| p);
        Err(reconstruct(&scratch.came_from, nearest))
    })
}

fn astar_in(
    scratch: &mut AstarScratch,
    mesh: &NavMesh,
    start: u32,
    end: Option<u32>,
    links: &LinkTable,
    filter: &NavQueryFilter,
) -> Option<Vec<u32>> {
    let costs = StepCosts::new(mesh.area_costs(), &filter.area_costs, filter.excluded_areas);
    // The straight-line distance at the lowest cost per metre of any step: an area's, or a link's.
    let rate = costs.lowest * links.lowest_rate;
    let end_center = end.map(|end| mesh.polys[end as usize].center_xz());
    let heuristic = |poly: u32| -> f32 {
        // No end: a flood (Dijkstra) over everything reachable.
        let Some(end_center) = end_center else {
            return 0.0;
        };
        let c = mesh.polys[poly as usize].center_xz();
        let dx = c.x - end_center.x;
        let dz = c.z - end_center.z;
        (dx * dx + dz * dz).sqrt() * rate
    };

    scratch.g_score.insert(start, 0.0f32);
    let start_f = heuristic(start);
    scratch.open.push(OrderedPoly {
        f: start_f,
        g: 0.0,
        poly: start,
    });

    while let Some(OrderedPoly { g, poly, .. }) = scratch.open.pop() {
        let best = scratch.g_score.get(&poly).copied().unwrap_or(f32::INFINITY);
        // A worse copy of this polygon was left on the heap when a cheaper path arrived later.
        // Expanding it would rebuild its neighbours and push still more stale entries.
        if g > best {
            continue;
        }
        if Some(poly) == end {
            return Some(reconstruct(&scratch.came_from, poly));
        }
        // The heuristic is consistent (triangle inequality on straight-line distances, scaled by
        // the lowest cost per metre of any step), so the first non-stale pop of a node is settled.
        note_astar_expansion(poly);
        let here = &mesh.polys[poly as usize];
        scratch.neighbors.clear();
        for portal in &here.portals {
            // A neighbour index outside the mesh (a hand-built or corrupt portal) is a wall.
            let Some(next) = mesh.polys.get(portal.neighbor as usize) else {
                continue;
            };
            if costs.allows(next.area) {
                scratch
                    .neighbors
                    .push((portal.neighbor, portal_step_cost(here, next, portal, &costs)));
            }
        }
        for edge in links.edges_from(poly) {
            let Some(landing) = mesh.polys.get(edge.to_poly as usize).map(|p| p.area) else {
                continue;
            };
            if filter.passes_flags(edge.flags) && costs.allows(edge.area) && costs.allows(landing) {
                scratch
                    .neighbors
                    .push((edge.to_poly, edge.cost * costs.of(edge.area)));
            }
        }

        // Copy the neighbour list out of the scratch borrow before pushing, so the heap and the
        // list can be used together. The list is at most the polygon's degree.
        let degree = scratch.neighbors.len();
        for index in 0..degree {
            let (neighbor, step_cost) = scratch.neighbors[index];
            let tentative = g + step_cost;
            let better = scratch
                .g_score
                .get(&neighbor)
                .is_none_or(|&existing| tentative < existing);
            if better {
                scratch.g_score.insert(neighbor, tentative);
                scratch.came_from.insert(neighbor, poly);
                let f = tentative + heuristic(neighbor);
                scratch.open.push(OrderedPoly {
                    f,
                    g: tentative,
                    poly: neighbor,
                });
            }
        }
    }
    None
}

/// A heap entry ordering by ascending `f`, then polygon, then the `g` it was pushed with.
///
/// `g` is part of the order so two entries for one polygon (a stale copy and the improved one)
/// are not `Equal`. The pop skips an entry whose `g` is worse than the best known.
struct OrderedPoly {
    f: f32,
    g: f32,
    poly: u32,
}

impl PartialEq for OrderedPoly {
    fn eq(&self, other: &Self) -> bool {
        self.f.to_bits() == other.f.to_bits()
            && self.poly == other.poly
            && self.g.to_bits() == other.g.to_bits()
    }
}
impl Eq for OrderedPoly {}
impl PartialOrd for OrderedPoly {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrderedPoly {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse so the smallest f pops first.
        other
            .f
            .total_cmp(&self.f)
            .then_with(|| other.poly.cmp(&self.poly))
            .then_with(|| other.g.total_cmp(&self.g))
    }
}

fn reconstruct(came_from: &HashMap<u32, u32>, end: u32) -> Vec<u32> {
    let mut path = vec![end];
    let mut cur = end;
    while let Some(&prev) = came_from.get(&cur) {
        path.push(prev);
        cur = prev;
    }
    path.reverse();
    path
}

/// The standard XZ cross product: positive when `(bx, bz)` is to the left of `(ax, az)`.
///
/// This is the opposite sign of [`tri_area2`]. Portal orientation uses this one so `left` is the
/// walker's left; the funnel's tighten tests use Mononen's sign. Unifying the two flips every
/// portal and the funnel locks the far end of each edge.
fn cross2(ax: f32, az: f32, bx: f32, bz: f32) -> f32 {
    ax * bz - az * bx
}

/// Signed double-area of triangle `a-b-c` in XZ, Mononen's convention.
///
/// `triarea2(a, b, c) = (c.x-a.x)*(b.z-a.z) - (b.x-a.x)*(c.z-a.z)`. Positive when `c` lies to
/// the **right** of the directed segment `a → b` (a clockwise turn). With this sign, the right
/// funnel edge tightens when `tri_area2(apex, right, new_right) <= 0`.
fn tri_area2(a: Vec3, b: Vec3, c: Vec3) -> f32 {
    (c.x - a.x) * (b.z - a.z) - (b.x - a.x) * (c.z - a.z)
}

/// String-pulling over a corridor of portals (the Simple Stupid Funnel).
///
/// `portals` are unordered edge endpoints. Each is oriented once so `left` is on the walker's
/// left in XZ, against the direction from the previous portal's midpoint. The start and the goal
/// are degenerate portals, so the last edge is tightened instead of having the goal appended
/// past an untested portal (that is what lets a path cut a hole).
///
/// On a restart the scan continues at `apex_index + 1`. Re-processing the portal that produced
/// the new apex re-opens the opposite edge, the next portal immediately restarts on the same
/// apex, and the corner list grows without bound. `apex_index` strictly increases on every
/// restart, so the scan terminates; a hard cap of `4·n²` iterations returns the corridor
/// midpoints if a malformed portal list would still spin.
///
/// Cite: Mikko Mononen, "Simple Stupid Funnel Algorithm" (2010); Demyen & Buro, "Efficient
/// Triangulation-Based Pathfinding", AAAI 2006.
pub(crate) fn funnel(start: Vec3, end: Vec3, portals: &[(Vec3, Vec3)]) -> Vec<Vec3> {
    let mut oriented = Vec::new();
    let mut pts = Vec::new();
    let mut result = Vec::new();
    funnel_into(start, end, portals, &mut oriented, &mut pts, &mut result);
    result
}

/// [`funnel`] writing into caller-owned buffers. Each buffer is cleared first.
pub(crate) fn funnel_into(
    start: Vec3,
    end: Vec3,
    portals: &[(Vec3, Vec3)],
    oriented: &mut Vec<(Vec3, Vec3)>,
    pts: &mut Vec<Vec3>,
    result: &mut Vec<Vec3>,
) {
    oriented.clear();
    pts.clear();
    result.clear();
    // A portal that holds the start (or the goal) is already crossed: that point lies in both
    // polygons it joins. Keeping it puts the apex on the portal's own line, where the incoming
    // direction names no side and every later area test against its ends is zero, so the funnel
    // turned around a portal end it never had to touch (nav-funnel-visibility-oracle,
    // `portal_interior_diagonal_a_forward`). Skip leading portals holding the start and trailing
    // portals holding the goal.
    let first = portals
        .iter()
        .position(|&(a, b)| !on_segment_xz(start, a, b))
        .unwrap_or(portals.len());
    let last = portals[first..]
        .iter()
        .rposition(|&(a, b)| !on_segment_xz(end, a, b))
        .map_or(first, |k| first + k + 1);
    let portals = &portals[first..last];
    oriented.push((start, start));
    let mut prev_mid = start;
    for &(a, b) in portals {
        oriented.push(orient_portal(prev_mid, a, b));
        prev_mid = Vec3::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5, (a.z + b.z) * 0.5);
    }
    oriented.push((end, end));

    let n = oriented.len();
    let mut apex = start;
    let mut left = start;
    let mut right = start;
    // `left_index` / `right_index` are the portals the current funnel edges came from.
    // A restart's new apex is one of those, so the index strictly increases (it starts at 0
    // and the first real portal is 1). The scan then continues at apex+1.
    let mut left_index = 0usize;
    let mut right_index = 0usize;

    // 4·n² covers the O(n) restarts each re-scanning O(n) portals, with slack. Past this the
    // portal list is not a corridor; return the midpoints rather than allocating without bound.
    let limit = n.saturating_mul(n).saturating_mul(4);
    let mut iterations = 0usize;
    let mut i = 1usize;
    while i < n {
        iterations += 1;
        if iterations > limit {
            tracing::warn!(
                iterations,
                portals = n,
                "funnel exceeded 4·n² iterations; returning corridor midpoints"
            );
            corridor_midpoints_into(start, end, portals, result);
            return;
        }

        let (portal_left, portal_right) = oriented[i];

        // Right edge. A new right vertex tightens when it is left of (or on) the current right
        // edge, and still strictly right of the left edge. Otherwise the left vertex is a corner.
        if tri_area2(apex, right, portal_right) <= 0.0 {
            if apex == right || tri_area2(apex, left, portal_right) > 0.0 {
                right = portal_right;
                right_index = i;
            } else {
                // New apex is the left edge. `left_index` is already that portal, and it is
                // strictly past the previous apex, so continuing at `left_index + 1` makes progress.
                let corner = left;
                pts.push(corner);
                apex = corner;
                left = corner;
                right = corner;
                right_index = left_index;
                i = left_index + 1;
                continue;
            }
        }

        // Left edge, signs flipped: `>= 0` tightens, `< 0` means the right vertex is the corner.
        if tri_area2(apex, left, portal_left) >= 0.0 {
            if apex == left || tri_area2(apex, right, portal_left) < 0.0 {
                left = portal_left;
                left_index = i;
            } else {
                let corner = right;
                pts.push(corner);
                apex = corner;
                left = corner;
                right = corner;
                left_index = right_index;
                i = right_index + 1;
                continue;
            }
        }

        i += 1;
    }

    result.push(start);
    result.extend_from_slice(pts);
    if result.last() != Some(&end) {
        result.push(end);
    }
}

/// The safe fallback when the funnel will not terminate: start, each portal midpoint, goal.
/// For convex corridor polygons the segment between consecutive portal midpoints stays inside.
fn corridor_midpoints_into(start: Vec3, end: Vec3, portals: &[(Vec3, Vec3)], pts: &mut Vec<Vec3>) {
    pts.clear();
    pts.push(start);
    for &(a, b) in portals {
        pts.push(Vec3::new((a.x + b.x) * 0.5, (a.y + b.y) * 0.5, (a.z + b.z) * 0.5));
    }
    if pts.last() != Some(&end) {
        pts.push(end);
    }
}

/// Whether `p` lies on the segment `a-b` in XZ, exactly (endpoints included). Exact on purpose:
/// it only drops portals a path endpoint sits on, which the builder and the snapped query
/// endpoints produce bit for bit.
fn on_segment_xz(p: Vec3, a: Vec3, b: Vec3) -> bool {
    let (ax, az) = (p.x - a.x, p.z - a.z);
    let (bx, bz) = (p.x - b.x, p.z - b.z);
    cross2(b.x - a.x, b.z - a.z, ax, az) == 0.0 && ax * bx + az * bz <= 0.0
}

/// Orients an unordered portal edge `(a, b)` into `(left, right)` so `left` is on the walker's
/// left of the direction from `from` toward the portal midpoint.
fn orient_portal(from: Vec3, a: Vec3, b: Vec3) -> (Vec3, Vec3) {
    let dir_x = (a.x + b.x) * 0.5 - from.x;
    let dir_z = (a.z + b.z) * 0.5 - from.z;
    let edge_x = b.x - a.x;
    let edge_z = b.z - a.z;
    // `cross2 > 0` means `b - a` points to the walker's left, so `b` is the left endpoint.
    let side = cross2(dir_x, dir_z, edge_x, edge_z);
    if side >= 0.0 { (b, a) } else { (a, b) }
}

/// Whether every 0.05 m sample of `waypoints` lies in the union of `poly_ids` (the boundary
/// counts). A string-pulled corridor stays inside; a segment that cuts a hole does not.
#[cfg(test)]
pub(crate) fn assert_waypoints_in_corridor(mesh: &NavMesh, poly_ids: &[u32], waypoints: &[Vec3]) {
    for pair in waypoints.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        let dx = b.x - a.x;
        let dz = b.z - a.z;
        let len = (dx * dx + dz * dz).sqrt();
        let steps = ((len / 0.05).ceil() as usize).max(1);
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let x = a.x + dx * t;
            let z = a.z + dz * t;
            let inside = poly_ids.iter().any(|&id| {
                mesh.polys
                    .get(id as usize)
                    .is_some_and(|p| p.distance_sq_xz(x, z) <= 1.0e-6)
            });
            assert!(
                inside,
                "({x:.3}, {z:.3}) on {a:?} → {b:?} leaves the corridor; waypoints = {waypoints:?}"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NavMeshBuildParams;
    use crate::heightfield::WalkableGrid;
    use crate::mesh::OffMeshLink;

    fn flat_mesh(size: usize) -> NavMesh {
        let mut cells = vec![None; size * size];
        for z in 0..size {
            for x in 0..size {
                cells[z * size + x] = Some(0.0);
            }
        }
        let grid = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: size,
            height: size,
            cells,
            areas: Vec::new(),
        };
        NavMesh::from_grid(&grid, NavMeshBuildParams::default())
    }

    fn mesh_with_wall() -> NavMesh {
        // A 20x20 floor with a wall across the middle (column x=10 not walkable, except a gap at z=10).
        let size = 20;
        let mut cells = vec![None; size * size];
        for z in 0..size {
            for x in 0..size {
                let wall = x == 10 && z != 10;
                cells[z * size + x] = (!wall).then_some(0.0);
            }
        }
        let grid = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: size,
            height: size,
            cells,
            areas: Vec::new(),
        };
        NavMesh::from_grid(&grid, NavMeshBuildParams::default())
    }

    #[test]
    fn a_point_beyond_the_hash_neighborhood_but_inside_the_extents_is_located() {
        let mesh = flat_mesh(4);
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        // 1.5 m outside the mesh edge at x = 0: more than one hash cell away, inside the 2 m extents.
        let (poly, point) = q
            .find_nearest(Vec3::new(-1.5, 0.0, 2.0), Vec3::splat(2.0))
            .expect("the mesh is within the extents");
        assert_eq!(poly, 0);
        assert!(
            (point.x - 0.0).abs() < 1e-5,
            "snapped onto the edge, got {point:?}"
        );
        assert!((point.z - 2.0).abs() < 1e-5);
        // Beyond the extents it is still rejected.
        assert!(
            q.find_nearest(Vec3::new(-2.5, 0.0, 2.0), Vec3::splat(2.0))
                .is_none()
        );
    }

    #[test]
    fn find_nearest_returns_a_point_on_the_mesh_for_an_outside_query() {
        let mesh = flat_mesh(4);
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let (poly, point) = q
            .find_nearest(Vec3::new(5.0, 0.0, 6.0), Vec3::splat(3.0))
            .expect("within extents of the corner");
        assert!(mesh.polys[poly as usize].contains_xz(point.x - 1e-3, point.z - 1e-3));
        assert!((point.x - 4.0).abs() < 1e-5 && (point.z - 4.0).abs() < 1e-5);
        // An inside query is returned unchanged.
        let (_, inside) = q
            .find_nearest(Vec3::new(1.0, 0.0, 2.0), Vec3::splat(1.0))
            .expect("on the mesh");
        assert_eq!((inside.x, inside.z), (1.0, 2.0));
    }

    #[test]
    fn a_path_from_outside_starts_on_the_mesh() {
        let mesh = flat_mesh(4);
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let path = q
            .find_path(
                Vec3::new(-1.0, 0.0, 2.0),
                Vec3::new(3.0, 0.0, 2.0),
                Vec3::splat(2.0),
            )
            .expect("path");
        assert!((path.waypoints[0].x - 0.0).abs() < 1e-5);
    }

    fn query<'a>(mesh: &'a NavMesh, obstacles: &'a NavObstacles) -> NavQuery<'a> {
        NavQuery {
            mesh,
            obstacles: Some(obstacles),
        }
    }

    #[test]
    fn find_nearest_honors_each_extent_axis_far_outside_the_hash_reach() {
        // A 4x4 floor on 1 m hash buckets; the sample sits many buckets past the +z edge.
        let mesh = flat_mesh(4);
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let far = Vec3::new(0.2, 0.0, 9.0);
        assert!(
            q.find_nearest(far, Vec3::new(0.5, 2.0, 5.0)).is_some(),
            "5 m z reach spans the gap"
        );
        assert!(
            q.find_nearest(far, Vec3::new(0.5, 2.0, 4.0)).is_none(),
            "4 m z reach falls short"
        );
        // A wide x extent must not widen the z test.
        assert!(q.find_nearest(far, Vec3::new(50.0, 2.0, 1.0)).is_none());
        // Same on x.
        let side = Vec3::new(-9.0, 0.0, 1.0);
        assert!(q.find_nearest(side, Vec3::new(9.5, 2.0, 0.5)).is_some());
        assert!(q.find_nearest(side, Vec3::new(8.5, 2.0, 50.0)).is_none());
    }

    #[test]
    fn find_nearest_treats_negative_and_non_finite_extents_sanely() {
        let mesh = flat_mesh(4);
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let on = Vec3::new(1.5, 0.0, 1.5);
        assert!(q.find_nearest(on, Vec3::new(-2.0, 2.0, -2.0)).is_none());
        assert!(q.find_nearest(on, Vec3::new(2.0, 2.0, -2.0)).is_none());
        assert!(q.find_nearest(on, Vec3::new(-0.0, 2.0, 2.0)).is_some());
        assert!(q.find_nearest(on, Vec3::new(f32::NAN, 2.0, 2.0)).is_none());
        assert!(
            q.find_nearest(Vec3::new(f32::INFINITY, 0.0, 0.0), Vec3::splat(2.0))
                .is_none()
        );
    }

    #[test]
    fn straight_path_is_two_waypoints() {
        let mesh = flat_mesh(10);
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let path = q
            .find_path(
                Vec3::new(1.0, 0.0, 1.0),
                Vec3::new(8.0, 0.0, 1.0),
                Vec3::splat(2.0),
            )
            .expect("a path exists");
        assert_eq!(path.waypoints.len(), 2, "no corners on a straight run");
        let near =
            |p: Vec3, x: f32, z: f32| (p.x - x).abs() < 1e-3 && p.y.abs() < 1e-3 && (p.z - z).abs() < 1e-3;
        assert!(
            near(path.waypoints[0], 1.0, 1.0),
            "start: {:?}",
            path.waypoints[0]
        );
        assert!(near(path.waypoints[1], 8.0, 1.0), "goal: {:?}", path.waypoints[1]);
        assert!((path.length - 7.0).abs() < 1e-3, "length {}", path.length);
    }

    #[test]
    fn path_goes_around_a_wall() {
        let mesh = mesh_with_wall();
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        // Both ends are off the gap row (z=10), so the straight line would cross the wall.
        let path = q
            .find_path(
                Vec3::new(2.0, 0.0, 2.5),
                Vec3::new(18.0, 0.0, 2.5),
                Vec3::splat(2.0),
            )
            .expect("the gap provides a route");
        assert!(path.waypoints.len() >= 3, "the path bends: {:?}", path.waypoints);
        // The wall column is x in [10, 11] outside the gap z in [10, 11]. No 0.05 m sample of any
        // segment may lie inside it.
        for pair in path.waypoints.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let len = ((b.x - a.x).powi(2) + (b.z - a.z).powi(2)).sqrt();
            let steps = ((len / 0.05).ceil() as usize).max(1);
            for s in 0..=steps {
                let t = s as f32 / steps as f32;
                let x = a.x + (b.x - a.x) * t;
                let z = a.z + (b.z - a.z) * t;
                let in_wall = x > 10.001 && x < 10.999 && (z < 9.999 || z > 11.001);
                assert!(!in_wall, "({x}, {z}) is inside the wall: {:?}", path.waypoints);
            }
        }
        assert_waypoints_in_corridor(&mesh, &path.polys, &path.waypoints);
    }

    #[test]
    fn an_obstacle_blocks_the_only_gap() {
        let mesh = mesh_with_wall();
        let mut obstacles = NavObstacles::default();
        // Block the gap itself → no route from left to right.
        obstacles.add_box(Vec3::new(9.5, -1.0, 9.5), Vec3::new(11.0, 2.0, 11.0));
        let q = query(&mesh, &obstacles);
        let path = q.find_path(
            Vec3::new(2.0, 0.0, 10.0),
            Vec3::new(18.0, 0.0, 10.0),
            Vec3::splat(2.0),
        );
        assert!(path.is_none(), "no route once the gap is blocked");
    }

    #[test]
    fn nearest_points_to_a_polygon() {
        let mesh = flat_mesh(4);
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let (poly, pt) = q
            .find_nearest(Vec3::new(1.5, 1.0, 1.5), Vec3::splat(2.0))
            .expect("on the mesh");
        assert!((pt.y - 0.0).abs() < 1e-3, "snapped to the floor");
        assert_eq!(poly, 0, "the single rectangle polygon");
        assert!(
            (pt.x - 1.5).abs() < 1e-3 && (pt.z - 1.5).abs() < 1e-3,
            "XZ is kept for an on-mesh point: {pt:?}"
        );
        // Far away → nothing.
        assert!(
            q.find_nearest(Vec3::new(100.0, 0.0, 100.0), Vec3::splat(2.0))
                .is_none()
        );
    }

    #[test]
    fn funnel_keeps_a_straight_line_that_crosses_the_portal() {
        let start = Vec3::new(0.0, 0.0, 0.0);
        let end = Vec3::new(10.0, 0.0, 10.0);
        let portals = vec![(Vec3::new(5.0, 0.0, 0.0), Vec3::new(5.0, 0.0, 10.0))];
        let pts = funnel(start, end, &portals);
        assert_eq!(pts.len(), 2, "no corner needed: {pts:?}");
        assert_eq!(pts[0], start);
        assert_eq!(pts[1], end);
    }

    #[test]
    fn funnel_constricts_to_the_endpoint() {
        // Portal (6,3)–(10,3). The straight line (1,1)→(8,8) crosses z=3 at x=3, outside the
        // portal. The near apex is (6, 3). Locking the far end (10, 3) is the sign-convention
        // bug: any waypoint with z≈3 used to pass, including that far corner.
        let start = Vec3::new(1.0, 0.0, 1.0);
        let end = Vec3::new(8.0, 0.0, 8.0);
        let portals = vec![(Vec3::new(6.0, 0.0, 3.0), Vec3::new(10.0, 0.0, 3.0))];
        let pts = funnel(start, end, &portals);
        assert!(pts.len() >= 3, "the path bends at the portal corner: {pts:?}");
        assert_eq!(pts[0], start);
        assert_eq!(*pts.last().expect("non-empty"), end);
        assert!(
            pts.iter()
                .any(|w| (w.x - 6.0).abs() < 1e-3 && (w.z - 3.0).abs() < 1e-3),
            "the funnel must bend at the near end (6, 3), got {pts:?}"
        );
        assert!(
            pts.iter()
                .all(|w| (w.x - 10.0).abs() > 1e-3 || (w.z - 3.0).abs() > 1e-3),
            "the funnel must not lock the far portal end (10, 3), got {pts:?}"
        );
    }

    #[test]
    fn tri_area2_uses_mononen_sign() {
        // Positive when `c` is a right turn from `a → b`. The standard XZ cross is the opposite.
        let a = Vec3::new(0.0, 0.0, 0.0);
        let b = Vec3::new(1.0, 0.0, 0.0);
        let left = Vec3::new(1.0, 0.0, 1.0);
        let right = Vec3::new(1.0, 0.0, -1.0);
        assert!(tri_area2(a, b, left) < 0.0, "a left turn is negative");
        assert!(tri_area2(a, b, right) > 0.0, "a right turn is positive");
        let expect = (right.x - a.x) * (b.z - a.z) - (b.x - a.x) * (right.z - a.z);
        assert_eq!(tri_area2(a, b, right), expect);
    }

    #[test]
    fn a_start_on_the_first_portal_does_not_bend_around_its_ends() {
        // The start (4, 1) lies on the first portal x = 4 (a triangle edge it shares with the
        // next polygon). The corridor then rises: a diagonal portal and the x = 8 portal whose
        // lower end (8, 4) is the only corner the shortest path touches. Before the fix the
        // apex sat on the first portal's line and the path bent around (4, 0) or (4, 2).
        let start = Vec3::new(4.0, 0.0, 1.0);
        let end = Vec3::new(12.0, 0.0, 5.0);
        let rest = [
            (Vec3::new(4.0, 0.0, 0.0), Vec3::new(8.0, 0.0, 6.0)),
            (Vec3::new(8.0, 0.0, 4.0), Vec3::new(8.0, 0.0, 6.0)),
        ];
        for first in [
            (Vec3::new(4.0, 0.0, 0.0), Vec3::new(4.0, 0.0, 2.0)),
            (Vec3::new(4.0, 0.0, 2.0), Vec3::new(4.0, 0.0, 0.0)),
        ] {
            let path = funnel(start, end, &[first, rest[0], rest[1]]);
            assert_eq!(
                path,
                vec![start, Vec3::new(8.0, 0.0, 4.0), end],
                "first portal {first:?}"
            );
            // The mirror: the goal on the last portal.
            let reversed = [rest[1], rest[0], first];
            let back = funnel(end, start, &reversed);
            assert_eq!(
                back,
                vec![end, Vec3::new(8.0, 0.0, 4.0), start],
                "last portal {first:?}"
            );
        }
        // Both ends on one portal: nothing to cross.
        let on = (Vec3::new(4.0, 0.0, 0.0), Vec3::new(4.0, 0.0, 2.0));
        let a = Vec3::new(4.0, 0.0, 0.5);
        let b = Vec3::new(4.0, 0.0, 1.5);
        assert_eq!(funnel(a, b, &[on]), vec![a, b]);
    }

    #[test]
    fn every_portal_keeps_left_on_the_walkers_left() {
        // Swapped endpoint order must not swap left/right. That swap is the classic funnel bug:
        // the algorithm then tightens the far side of every edge.
        let edges = [
            (Vec3::new(1.0, 0.0, 0.0), Vec3::new(1.0, 0.0, 4.0)),
            (Vec3::new(3.0, 0.0, 1.0), Vec3::new(3.0, 0.0, 5.0)),
            (Vec3::new(5.0, 0.0, 0.0), Vec3::new(5.0, 0.0, 3.0)),
        ];
        let mut prev = Vec3::new(0.0, 0.0, 2.0);
        for (a, b) in edges {
            let mut seen: Option<(Vec3, Vec3)> = None;
            for (p, q) in [(a, b), (b, a)] {
                let (left, right) = orient_portal(prev, p, q);
                let mid = Vec3::new((p.x + q.x) * 0.5, 0.0, (p.z + q.z) * 0.5);
                let dir_x = mid.x - prev.x;
                let dir_z = mid.z - prev.z;
                let side_l = dir_x * (left.z - mid.z) - dir_z * (left.x - mid.x);
                let side_r = dir_x * (right.z - mid.z) - dir_z * (right.x - mid.x);
                assert!(
                    side_l > 0.0,
                    "left is not on the walker's left: left={left:?} right={right:?}"
                );
                assert!(
                    side_r < 0.0,
                    "right is not on the walker's right: left={left:?} right={right:?}"
                );
                if let Some((pl, pr)) = seen {
                    assert_eq!(left, pl, "swapped endpoints flipped left");
                    assert_eq!(right, pr, "swapped endpoints flipped right");
                }
                seen = Some((left, right));
            }
            prev = Vec3::new((a.x + b.x) * 0.5, 0.0, (a.z + b.z) * 0.5);
        }
    }

    #[test]
    fn a_200_portal_corridor_finishes_quickly_and_stays_small() {
        // A straight channel the diagonal crosses, plus a zigzag that restarts at every gate.
        // Flipping only `tri_area2` and re-scanning the apex portal grew this to ~6 GB.
        let n = 200;
        let mut wide = Vec::with_capacity(n);
        for i in 0..n {
            let x = (i + 1) as f32;
            wide.push((Vec3::new(x, 0.0, -10.0), Vec3::new(x, 0.0, 10.0)));
        }
        let start = Vec3::ZERO;
        let end = Vec3::new((n + 1) as f32, 0.0, 0.0);

        let mut zigzag = Vec::with_capacity(n);
        for i in 0..n {
            let x = (i + 1) as f32;
            let (z0, z1) = if i % 2 == 0 { (0.0, 1.0) } else { (2.0, 3.0) };
            zigzag.push((Vec3::new(x, 0.0, z0), Vec3::new(x, 0.0, z1)));
        }
        let zag_start = Vec3::new(0.0, 0.0, 0.5);
        let zag_end = Vec3::new((n + 1) as f32, 0.0, if n % 2 == 0 { 0.5 } else { 2.5 });

        let straight = funnel(start, end, &wide);
        let zag = funnel(zag_start, zag_end, &zigzag);
        assert_eq!(
            straight.len(),
            2,
            "a straight corridor needs no corner: {straight:?}"
        );
        assert_eq!(straight[0], start);
        assert_eq!(straight[1], end);
        // The first zigzag gate is x=1, z∈[0,1]. Facing +X, the near (left) end is (1, 1).
        // The old sign pushed the far end (1, 0); the midpoint fallback would emit (1, 0.5).
        assert!(
            (zag[1].x - 1.0).abs() < 1e-3 && (zag[1].z - 1.0).abs() < 1e-3,
            "first corner must be the near gate end (1, 1), got {zag:?}"
        );
        assert!(zag.len() <= n + 2, "path exploded to {} waypoints", zag.len());
        // Every interior waypoint is a gate end: an integer x and z in {0, 1, 2, 3}. The
        // one-corner-per-portal midpoint fallback would put corners at z = 0.5 / 2.5.
        for w in &zag[1..zag.len() - 1] {
            assert!((w.x - w.x.round()).abs() < 1e-3, "corner x is a gate: {w:?}");
            assert!(
                (w.z - w.z.round()).abs() < 1e-3 && (0.0..=3.0).contains(&w.z),
                "corner is a gate end, not a midpoint: {w:?}"
            );
        }
        assert_eq!(zag[0], zag_start);
        assert_eq!(*zag.last().expect("non-empty"), zag_end);
    }

    #[test]
    fn find_path_snakes_through_two_staggered_walls() {
        // 8×8 floor. A wall on z==3 for x<6 and a wall on z==5 for x>=2. The gaps are at the
        // opposite ends. The broken funnel returns [(7,1),(8,3),(1,7)]: the second segment
        // crosses the upper wall (it meets z=5 at x=4.5).
        let size = 8;
        let mut cells = vec![None; size * size];
        for z in 0..size {
            for x in 0..size {
                let wall = (z == 3 && x < 6) || (z == 5 && x >= 2);
                cells[z * size + x] = (!wall).then_some(0.0);
            }
        }
        let grid = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: size,
            height: size,
            cells,
            areas: Vec::new(),
        };
        let mesh = NavMesh::from_grid(&grid, NavMeshBuildParams::default());
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let path = q
            .find_path(
                Vec3::new(7.0, 0.0, 1.0),
                Vec3::new(1.0, 0.0, 7.0),
                Vec3::splat(2.0),
            )
            .expect("the two gaps connect");
        let bad = path.waypoints.len() == 3
            && (path.waypoints[1].x - 8.0).abs() < 1e-2
            && (path.waypoints[1].z - 3.0).abs() < 1e-2;
        assert!(
            !bad,
            "funnel locked the far corner of the first gap: {:?}",
            path.waypoints
        );
        for pair in path.waypoints.windows(2) {
            assert!(
                !segment_hits_staggered_wall(pair[0], pair[1]),
                "segment crosses a wall: {pair:?} path={:?}",
                path.waypoints
            );
        }
        assert_waypoints_in_corridor(&mesh, &path.polys, &path.waypoints);
    }

    /// Whether the open segment passes through the interior of either staggered wall
    /// (`z==3 && x<6`, `z==5 && x>=2` on the 8×8 floor). Boundary corners shared with a gap count
    /// as missed.
    fn segment_hits_staggered_wall(a: Vec3, b: Vec3) -> bool {
        let dx = b.x - a.x;
        let dz = b.z - a.z;
        let len = (dx * dx + dz * dz).sqrt();
        let steps = ((len / 0.05).ceil() as usize).max(1);
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let x = a.x + dx * t;
            let z = a.z + dz * t;
            const EPS: f32 = 1.0e-3;
            let in_lower = x > EPS && x < 6.0 - EPS && z > 3.0 + EPS && z < 4.0 - EPS;
            let in_upper = x > 2.0 + EPS && x < 8.0 - EPS && z > 5.0 + EPS && z < 6.0 - EPS;
            if in_lower || in_upper {
                return true;
            }
        }
        false
    }

    #[test]
    fn path_bends_around_an_l_shaped_wall() {
        // A wall across x=10 for z in [2, 20]; the only gap is at the bottom (z < 2).
        let size = 20;
        let mut cells = vec![None; size * size];
        for z in 0..size {
            for x in 0..size {
                let wall = x == 10 && z >= 2;
                cells[z * size + x] = (!wall).then_some(0.0);
            }
        }
        let grid = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: size,
            height: size,
            cells,
            areas: Vec::new(),
        };
        let mesh = NavMesh::from_grid(&grid, NavMeshBuildParams::default());
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        // Straight across would hit the wall at (10, 10); the path must go around the bottom.
        let path = q
            .find_path(
                Vec3::new(2.0, 0.0, 10.0),
                Vec3::new(18.0, 0.0, 10.0),
                Vec3::splat(2.0),
            )
            .expect("the bottom gap provides a route");
        assert!(
            path.waypoints.len() >= 3,
            "the path bends around the wall: {:?}",
            path.waypoints
        );
        for w in &path.waypoints {
            let in_wall = (w.x - 10.5).abs() < 0.4 && w.z >= 2.0;
            assert!(!in_wall, "waypoint inside the wall: {w:?}");
        }
        // The path is longer than the straight 16 m (it detours through the gap).
        assert!(path.length > 16.0, "path length {} > straight 16 m", path.length);
    }

    #[test]
    fn find_path_refuses_a_step_above_max_climb() {
        // Two adjacent cells at heights 0 and 2.0 (step 2.0 > max_climb 0.5): no route.
        let g = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: 2,
            height: 1,
            cells: vec![Some(0.0), Some(2.0)],
            areas: Vec::new(),
        };
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let path = q.find_path(
            Vec3::new(0.5, 0.0, 0.5),
            Vec3::new(1.5, 2.0, 0.5),
            Vec3::splat(2.0),
        );
        assert!(path.is_none(), "a step above max_climb has no route");
    }

    #[test]
    fn find_path_accepts_a_step_within_max_climb() {
        // A 0.4 m step (within max_climb 0.5) is traversable.
        let g = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: 2,
            height: 1,
            cells: vec![Some(0.0), Some(0.4)],
            areas: Vec::new(),
        };
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let path = q.find_path(
            Vec3::new(0.5, 0.0, 0.5),
            Vec3::new(1.5, 0.4, 0.5),
            Vec3::splat(2.0),
        );
        assert!(path.is_some(), "a climbable step has a route");
    }

    #[test]
    fn an_off_mesh_link_connects_two_islands() {
        // Two isolated islands (regions) joined only by a jump link, placed off-axis so the path
        // must actually pass through the link's take-off and landing points.
        let width = 6;
        let height = 2;
        let mut cells = vec![None; width * height];
        cells[0] = Some(0.0); // (0,0)
        cells[1] = Some(0.0); // (1,0)
        cells[10] = Some(0.0); // (4,1)
        cells[11] = Some(0.0); // (5,1)
        let g = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width,
            height,
            cells,
            areas: Vec::new(),
        };
        let mesh =
            NavMesh::from_grid(&g, NavMeshBuildParams::default()).with_off_mesh_links(vec![OffMeshLink {
                from: Vec3::new(1.5, 0.0, 0.5),
                to: Vec3::new(5.5, 0.0, 1.5),
                bidirectional: true,
                area: 0,
                ..OffMeshLink::new(Vec3::ZERO, Vec3::ZERO)
            }]);
        assert_eq!(mesh.region_count, 2, "two islands");
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let path = q
            .find_path(
                Vec3::new(0.5, 0.0, 0.5),
                Vec3::new(4.5, 0.0, 1.5),
                Vec3::splat(2.0),
            )
            .expect("the off-mesh link provides a route");
        // The path passes through both link endpoints (the jump is not cut across).
        let has_from = path
            .waypoints
            .iter()
            .any(|w| (w.x - 1.5).abs() < 1e-3 && (w.z - 0.5).abs() < 1e-3);
        let has_to = path
            .waypoints
            .iter()
            .any(|w| (w.x - 5.5).abs() < 1e-3 && (w.z - 1.5).abs() < 1e-3);
        assert!(
            has_from && has_to,
            "waypoints pass through the link: {:?}",
            path.waypoints
        );
    }

    #[test]
    fn find_nearest_honours_z_extent_and_snaps_xz_to_the_polygon() {
        let g = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: 4,
            height: 4,
            cells: vec![Some(0.0); 16],
            areas: Vec::new(),
        };
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        let q = NavQuery {
            mesh: &mesh,
            obstacles: None,
        };
        // 5 m outside on Z: found with a wide Z extent although X extent is tiny.
        let far = Vec3::new(2.0, 0.0, 9.0);
        assert!(q.find_nearest(far, Vec3::new(0.2, 2.0, 8.0)).is_some());
        assert!(q.find_nearest(far, Vec3::new(8.0, 2.0, 0.2)).is_none());
        // The result lies on the polygon, not at the raw query XZ.
        let (_, p) = q
            .find_nearest(Vec3::new(2.0, 0.0, 5.5), Vec3::splat(2.0))
            .expect("within extents");
        assert!((p.z - 4.0).abs() < 1e-4 && (p.x - 2.0).abs() < 1e-4, "got {p:?}");
    }

    #[test]
    fn a_one_way_off_mesh_link_cannot_be_climbed_backward() {
        let width = 6;
        let height = 2;
        let mut cells = vec![None; width * height];
        cells[0] = Some(0.0);
        cells[1] = Some(0.0);
        cells[10] = Some(0.0);
        cells[11] = Some(0.0);
        let g = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width,
            height,
            cells,
            areas: Vec::new(),
        };
        let mesh =
            NavMesh::from_grid(&g, NavMeshBuildParams::default()).with_off_mesh_links(vec![OffMeshLink {
                from: Vec3::new(1.5, 0.0, 0.5),
                to: Vec3::new(5.5, 0.0, 1.5),
                bidirectional: false,
                area: 0,
                ..OffMeshLink::new(Vec3::ZERO, Vec3::ZERO)
            }]);
        assert_eq!(mesh.region_count, 2, "two islands");
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        assert!(
            q.find_path(
                Vec3::new(0.5, 0.0, 0.5),
                Vec3::new(4.5, 0.0, 1.5),
                Vec3::splat(2.0),
            )
            .is_some(),
            "the drop works forward"
        );
        assert!(
            q.find_path(
                Vec3::new(4.5, 0.0, 1.5),
                Vec3::new(0.5, 0.0, 0.5),
                Vec3::splat(2.0),
            )
            .is_none(),
            "a one-way link has no way back"
        );
    }

    #[test]
    fn a_portal_to_a_missing_polygon_is_a_wall() {
        let mut mesh = flat_mesh(4);
        mesh.polys[0].portals.push(crate::mesh::Portal {
            neighbor: 9999,
            a: Vec3::ZERO,
            b: Vec3::ZERO,
        });
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        let path = q.find_path(
            Vec3::new(0.3, 0.0, 0.3),
            Vec3::new(3.5, 0.0, 3.5),
            Vec3::new(1.0, 2.0, 1.0),
        );
        assert!(
            path.is_some(),
            "the bad portal is skipped, the real ones still route"
        );
    }

    /// audit sprint-audit-g-nav-perf M6: a node pushed twice is expanded once.
    ///
    /// The bottom row is one polygon of area 1, which costs 10. Its centre-priced portals put
    /// the far middle cells on the heap first. The cost-1 middle row then reaches those cells
    /// more cheaply, leaving the worse copy on the heap. The goal (area 40, cost 80) is expensive
    /// enough that the worse copy is popped before it. On the pre-fix search that copy was
    /// expanded too: 13 expansions of 8 polygons, path `[0, 5, 6, 7, 8]`.
    #[test]
    fn astar_expands_each_polygon_once() {
        let width = 8usize;
        let height = 2usize;
        let mut areas = vec![1u8; width];
        for x in 0..width {
            let area = if x + 1 == width { 40 } else { 2 + x as u8 };
            areas.push(area);
        }
        let grid = WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width,
            height,
            cells: vec![Some(0.0f32); width * height],
            areas,
        };
        let mut mesh = NavMesh::from_grid(&grid, NavMeshBuildParams::default());
        let mut costs = crate::areas::NavAreaCosts::default();
        costs.set(1, 10.0).expect("area 1");
        costs.set(40, 80.0).expect("area 40");
        mesh.set_area_costs(costs);
        let obstacles = NavObstacles::default();
        let q = query(&mesh, &obstacles);
        begin_astar_trace();
        let path = q
            .find_path(
                Vec3::new(4.0, 0.0, 0.5),
                Vec3::new(7.5, 0.0, 1.5),
                Vec3::splat(1.0),
            )
            .expect("a route to the expensive cell");
        let expanded = take_astar_expanded();
        assert!(
            path.polys.len() > 2,
            "the route crosses several polygons: {:?}",
            path.polys
        );
        let unique = expanded
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .len();
        assert!(unique > 1, "the search expanded past the start, got {expanded:?}");
        assert_eq!(
            expanded.len(),
            unique,
            "each polygon expands once ({unique} polygons, {} expansions): {expanded:?}",
            expanded.len()
        );
    }

    /// Spec 92 § 5: open-list entries with equal `f` pop in ascending polygon index, whatever
    /// order they were pushed in, and a lower `f` still pops first.
    #[test]
    fn open_list_ties_on_f_break_by_polygon_index() {
        let mut heap = BinaryHeap::new();
        for (f, poly) in [(3.0, 9), (2.0, 7), (2.0, 4), (2.0, 11), (1.0, 20), (2.0, 0)] {
            heap.push(OrderedPoly { f, g: 1.0, poly });
        }
        let popped: Vec<u32> = std::iter::from_fn(|| heap.pop().map(|e| e.poly)).collect();
        assert_eq!(popped, vec![20, 0, 4, 7, 11, 9]);

        // Two copies of one polygon with equal f differ by g and stay distinct entries.
        let a = OrderedPoly {
            f: 2.0,
            g: 1.0,
            poly: 5,
        };
        let b = OrderedPoly {
            f: 2.0,
            g: 1.5,
            poly: 5,
        };
        assert!(a != b, "entries with different g are not equal");
        assert_ne!(a.cmp(&b), std::cmp::Ordering::Equal);
    }
}
