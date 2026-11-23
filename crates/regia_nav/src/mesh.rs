// SPDX-License-Identifier: MPL-2.0
//! The polygon mesh itself (`docs/specs/22-tier2.md` § T2.2): a set of convex polygons (the
//! Recast "poly mesh" stage, built here from the compact walkable grid), off-mesh links, and the
//! spatial index used to locate a point on the mesh.
//!
//! Polygonisation merges adjacent same-height walkable cells into axis-aligned convex rectangles
//! (greedy rectangle decomposition — the same maximal-rectangle merge used in light-map atlas
//! packing), then links polygons that share a cell edge. Every polygon is convex and horizontal at
//! a single height, so point-in-polygon tests, the A* graph and the string-pulling funnel all work
//! on the XZ projection.

use std::borrow::Cow;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_app::prelude::*;
use regia_core::math::Vec3;
use serde::{Deserialize, Serialize};

use crate::areas::NavAreaCosts;
use crate::config::NavMeshBuildParams;
use crate::contours::Contour;
use crate::heightfield::{LayerLink, WalkableGrid, WalkableLayers};
use crate::links::{
    LinkBlockers, LinkTable, NavLinkError, NavLinkFlags, NavLinkKind, NavLinkStatus, links_fingerprint,
};
use crate::tile_cache::NavObstacleShape;

#[cfg(test)]
thread_local! {
    /// Times `query_box` examined a polygon stored in a bucket, on this thread. A polygon that
    /// covers several cells is examined once per query after the generation stamp skips the
    /// repeats. Tests read this through `query_box_visits`. Per thread so that tests running in
    /// parallel never count each other's queries (ADR-5517).
    static QUERY_BOX_VISITS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    /// Times `NavMesh::carved` built a mesh on this thread. Tests read this through
    /// `carve_builds`; per thread for the same reason.
    static CARVE_BUILDS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Zeroes `query_box_visits` on this thread.
#[cfg(test)]
pub(crate) fn reset_query_box_visits() {
    QUERY_BOX_VISITS.with(|c| c.set(0));
}

/// Polygon examinations in `query_box` on this thread since the last reset.
#[cfg(test)]
pub(crate) fn query_box_visits() -> u64 {
    QUERY_BOX_VISITS.with(std::cell::Cell::get)
}

/// Zeroes `carve_builds` on this thread.
#[cfg(test)]
pub(crate) fn reset_carve_builds() {
    CARVE_BUILDS.with(|c| c.set(0));
}

/// Meshes `NavMesh::carved` has built on this thread since the last reset.
#[cfg(test)]
pub(crate) fn carve_builds() -> u64 {
    CARVE_BUILDS.with(std::cell::Cell::get)
}

/// The next [`NavMesh::mesh_id`].
static NEXT_MESH_ID: AtomicU64 = AtomicU64::new(1);

fn mint_mesh_id() -> u64 {
    NEXT_MESH_ID.fetch_add(1, Ordering::Relaxed)
}

/// A connection between two polygons: the shared edge (portal) the string-pulling funnel walks
/// across.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Portal {
    /// The adjacent polygon index.
    pub neighbor: u32,
    /// One portal endpoint (world space).
    pub a: Vec3,
    /// The other portal endpoint (world space).
    pub b: Vec3,
}

/// A convex navigation polygon (always horizontal at a single height).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NavPoly {
    /// The polygon's boundary loop, ordered (counter-clockwise in XZ), world space.
    pub vertices: Vec<Vec3>,
    /// Area id (0 = plain ground; `docs/formats` and [`crate::NavAreaCosts`] say what ids cost).
    pub area: u8,
    /// Connections to neighbouring polygons, one portal each.
    pub portals: Vec<Portal>,
    /// Bounds (min corner). XZ is the polygon rectangle; Y is padded to the agent height below
    /// and 1 m above the surface (`[h - agent.height, h + 1]`), not the vertex box.
    pub min: Vec3,
    /// Bounds (max corner); see `min` for the Y padding.
    pub max: Vec3,
}

impl NavPoly {
    /// The polygon's walkable height (the shared `y` of its vertices).
    pub fn height(&self) -> f32 {
        self.vertices.first().map(|v| v.y).unwrap_or(0.0)
    }

    /// The XZ centroid of the polygon (used as a representative point for A*).
    pub fn center_xz(&self) -> Vec3 {
        let mut cx = 0.0;
        let mut cz = 0.0;
        for v in &self.vertices {
            cx += v.x;
            cz += v.z;
        }
        let n = self.vertices.len().max(1) as f32;
        Vec3::new(cx / n, 0.0, cz / n)
    }

    /// Whether the 2D point `(x, z)` lies inside the polygon's XZ projection (ray-casting test).
    pub fn contains_xz(&self, x: f32, z: f32) -> bool {
        let mut inside = false;
        let n = self.vertices.len();
        if n == 0 {
            return false;
        }
        let mut j = n - 1;
        for i in 0..n {
            let (xi, zi) = (self.vertices[i].x, self.vertices[i].z);
            let (xj, zj) = (self.vertices[j].x, self.vertices[j].z);
            let crosses = (zi > z) != (zj > z);
            if crosses {
                let at_x = (xj - xi) * (z - zi) / (zj - zi) + xi;
                if x < at_x {
                    inside = !inside;
                }
            }
            j = i;
        }
        inside
    }

    /// The point of the polygon's XZ footprint closest to `(x, z)` (the point itself when inside),
    /// at the polygon's surface height.
    pub fn closest_point_xz(&self, x: f32, z: f32) -> Vec3 {
        let y = self.height();
        if self.contains_xz(x, z) {
            return Vec3::new(x, y, z);
        }
        let mut best = (f32::INFINITY, Vec3::new(x, y, z));
        let n = self.vertices.len();
        for i in 0..n {
            let a = &self.vertices[i];
            let b = &self.vertices[(i + 1) % n];
            let (px, pz) = seg_closest(x, z, a.x, a.z, b.x, b.z);
            let d = (px - x) * (px - x) + (pz - z) * (pz - z);
            if d < best.0 {
                best = (d, Vec3::new(px, y, pz));
            }
        }
        best.1
    }

    /// The squared XZ distance from `(x, z)` to the polygon boundary (0 if inside).
    pub fn distance_sq_xz(&self, x: f32, z: f32) -> f32 {
        if self.contains_xz(x, z) {
            return 0.0;
        }
        let mut best = f32::INFINITY;
        let n = self.vertices.len();
        for i in 0..n {
            let a = &self.vertices[i];
            let b = &self.vertices[(i + 1) % n];
            best = best.min(seg_dist_sq(x, z, a.x, a.z, b.x, b.z));
        }
        best
    }
}

/// A jump / drop / climb / teleport between two points on the mesh (Recast "off-mesh connection";
/// `docs/specs/92-navmesh-offmesh.md` § 3.1). Build one with [`OffMeshLink::new`] and the `with_*`
/// methods; every field added after the first four has a serde default, so older serialized lists
/// still read.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct OffMeshLink {
    /// Start point (world space).
    pub from: Vec3,
    /// End point (world space).
    pub to: Vec3,
    /// Whether the link can be traversed in both directions.
    pub bidirectional: bool,
    /// Area / cost category.
    pub area: u8,
    /// The game's id for the link (0 = unnamed). [`NavMesh::set_link_enabled`] acts on every link
    /// with the id.
    #[serde(default)]
    pub user_id: u64,
    /// What kind of move it is (picks the gameplay animation).
    #[serde(default)]
    pub kind: NavLinkKind,
    /// Endpoint snap radius in metres: a polygon within `radius` in X and Z (and
    /// `max(radius, agent.max_climb)` in Y) of an endpoint carries it.
    #[serde(default = "default_link_radius")]
    pub radius: f32,
    /// Multiplier on the link's 3D length (`> 0`; a non-positive value is treated as 1 when the
    /// link is resolved). The searched A* cost is that product times the mesh cost of [`Self::area`]
    /// times the query filter's scale for the same area.
    #[serde(default = "default_cost_scale")]
    pub cost_scale: f32,
    /// Filter bits checked against a [`NavQueryFilter`](crate::NavQueryFilter).
    #[serde(default = "default_link_flags")]
    pub flags: u16,
    /// The runtime switch: a disabled link is not part of the graph.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Crowd traversal time in seconds; 0 = the 3D length divided by the agent's `max_speed`.
    #[serde(default)]
    pub duration: f32,
}

/// The default endpoint snap radius (the old fixed 2 m snap box).
pub const DEFAULT_LINK_RADIUS: f32 = 2.0;

fn default_link_radius() -> f32 {
    DEFAULT_LINK_RADIUS
}

fn default_cost_scale() -> f32 {
    1.0
}

fn default_link_flags() -> u16 {
    NavLinkFlags::WALK
}

fn default_enabled() -> bool {
    true
}

impl OffMeshLink {
    /// A bidirectional, enabled [`NavLinkKind::Jump`] from `from` to `to` with the default radius,
    /// cost scale, flags and an automatic duration.
    pub fn new(from: Vec3, to: Vec3) -> Self {
        Self {
            from,
            to,
            bidirectional: true,
            area: 0,
            user_id: 0,
            kind: NavLinkKind::Jump,
            radius: DEFAULT_LINK_RADIUS,
            cost_scale: 1.0,
            flags: NavLinkFlags::WALK,
            enabled: true,
            duration: 0.0,
        }
    }

    /// The same link, usable from `from` to `to` only.
    pub fn one_way(mut self) -> Self {
        self.bidirectional = false;
        self
    }

    /// The same link with another kind.
    pub fn with_kind(mut self, kind: NavLinkKind) -> Self {
        self.kind = kind;
        self
    }

    /// The same link with a user id.
    pub fn with_user_id(mut self, user_id: u64) -> Self {
        self.user_id = user_id;
        self
    }

    /// The same link with other filter bits.
    pub fn with_flags(mut self, flags: u16) -> Self {
        self.flags = flags;
        self
    }

    /// The same link with another endpoint snap radius.
    pub fn with_radius(mut self, radius: f32) -> Self {
        self.radius = radius;
        self
    }

    /// Checks the values: finite endpoints, a positive finite radius and cost scale, and a finite
    /// duration `>= 0`.
    pub fn validate(&self) -> Result<(), NavLinkError> {
        self.check()
            .map_err(|reason| NavLinkError::InvalidLink { index: 0, reason })
    }

    /// [`Self::validate`] without the index wrapper.
    pub(crate) fn check(&self) -> Result<(), &'static str> {
        if !(self.from.is_finite() && self.to.is_finite()) {
            return Err("endpoints must be finite");
        }
        if !(self.radius.is_finite() && self.radius > 0.0) {
            return Err("radius must be finite and positive");
        }
        if !(self.cost_scale.is_finite() && self.cost_scale > 0.0) {
            return Err("cost_scale must be finite and positive");
        }
        if !(self.duration.is_finite() && self.duration >= 0.0) {
            return Err("duration must be finite and not negative");
        }
        Ok(())
    }
}

/// Per-query generation stamps so a polygon that sits in several buckets is reported once.
#[derive(Debug)]
struct PolyVisit {
    /// The generation of the query in progress. Zero means "no query yet".
    generation: u32,
    /// `stamps[poly] == generation` when this query has already emitted `poly`.
    stamps: Vec<u32>,
}

/// A uniform spatial grid over polygon bounds, for cheap point → candidate-polygon lookups.
#[derive(Debug)]
struct SpatialGrid {
    origin: Vec3,
    cell_size: f32,
    width: usize,
    height: usize,
    cells: Vec<Vec<u32>>,
    /// Generation stamp for [`Self::query_box`]. A mutex because the mesh is `Sync` and the
    /// query takes `&self`.
    visit: Mutex<PolyVisit>,
}

impl Clone for SpatialGrid {
    fn clone(&self) -> Self {
        let n = self
            .visit
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .stamps
            .len();
        Self {
            origin: self.origin,
            cell_size: self.cell_size,
            width: self.width,
            height: self.height,
            cells: self.cells.clone(),
            // A clone starts a fresh generation. Sharing the stamps would race two queries.
            visit: Mutex::new(PolyVisit {
                generation: 0,
                stamps: vec![0; n],
            }),
        }
    }
}

impl SpatialGrid {
    fn build(polys: &[NavPoly], bounds_min: Vec3, bounds_max: Vec3, cell_size: f32) -> Self {
        let width = ((bounds_max.x - bounds_min.x) / cell_size).ceil().max(1.0) as usize;
        let height = ((bounds_max.z - bounds_min.z) / cell_size).ceil().max(1.0) as usize;
        let mut cells = vec![Vec::new(); width * height];
        for (i, poly) in polys.iter().enumerate() {
            let gx0 = ((poly.min.x - bounds_min.x) / cell_size).floor().max(0.0) as usize;
            let gz0 = ((poly.min.z - bounds_min.z) / cell_size).floor().max(0.0) as usize;
            let gx1 =
                (((poly.max.x - bounds_min.x) / cell_size).ceil() as usize).min(width.saturating_sub(1));
            let gz1 =
                (((poly.max.z - bounds_min.z) / cell_size).ceil() as usize).min(height.saturating_sub(1));
            for gz in gz0..=gz1 {
                for gx in gx0..=gx1 {
                    cells[gz * width + gx].push(i as u32);
                }
            }
        }
        Self {
            origin: bounds_min,
            cell_size,
            width,
            height,
            cells,
            visit: Mutex::new(PolyVisit {
                generation: 0,
                stamps: vec![0; polys.len()],
            }),
        }
    }

    /// Collects every polygon in a bucket the XZ box `[min_x, max_x] x [min_z, max_z]` touches.
    fn query_box(&self, min_x: f32, max_x: f32, min_z: f32, max_z: f32, out: &mut Vec<u32>) {
        out.clear();
        let last_x = self.width as isize - 1;
        let last_z = self.height as isize - 1;
        // Float-to-int casts saturate, so huge extents clamp to the grid instead of overflowing.
        let bucket = |v: f32, origin: f32, last: isize| -> isize {
            (((v - origin) / self.cell_size).floor() as isize).clamp(0, last)
        };
        let gx0 = bucket(min_x, self.origin.x, last_x);
        let gx1 = bucket(max_x, self.origin.x, last_x);
        let gz0 = bucket(min_z, self.origin.z, last_z);
        let gz1 = bucket(max_z, self.origin.z, last_z);
        {
            let mut visit = self.visit.lock().unwrap_or_else(|poison| poison.into_inner());
            visit.generation = visit.generation.wrapping_add(1);
            if visit.generation == 0 {
                visit.stamps.fill(0);
                visit.generation = 1;
            }
            let generation = visit.generation;
            for cz in gz0..=gz1 {
                for cx in gx0..=gx1 {
                    for &poly in &self.cells[cz as usize * self.width + cx as usize] {
                        if let Some(slot) = visit.stamps.get_mut(poly as usize) {
                            if *slot == generation {
                                continue;
                            }
                            *slot = generation;
                        }
                        #[cfg(test)]
                        QUERY_BOX_VISITS.with(|c| c.set(c.get() + 1));
                        out.push(poly);
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
    }
}

/// A generated navigation mesh.
#[derive(Resource, Debug, Clone)]
pub struct NavMesh {
    /// The convex polygons.
    pub polys: Vec<NavPoly>,
    /// Off-mesh connections.
    pub off_mesh_links: Vec<OffMeshLink>,
    /// World bounds (min corner).
    pub min: Vec3,
    /// World bounds (max corner).
    pub max: Vec3,
    /// The build parameters used.
    pub config: NavMeshBuildParams,
    /// How many connected walkable regions were found during generation.
    pub region_count: u32,
    /// The traced boundary contours of each region (for editor visualisation and testing).
    pub contours: Vec<Contour>,
    /// The pristine walkable cell grid this mesh was polygonised from, kept so dynamic obstacles
    /// can re-polygonise the mesh ([`Self::carved`]) without re-rasterising the level geometry.
    pub(crate) base_grid: WalkableGrid,
    /// The pristine walkable layers under `base_grid` (ADR-8076; empty for a single-surface mesh).
    pub(crate) lower_grids: Vec<WalkableGrid>,
    /// The steps between cells of different layers (layer 0 is `base_grid`).
    pub(crate) layer_links: Vec<LayerLink>,
    grid: SpatialGrid,
    /// The resolved links (ADR-1821), rebuilt whenever the polygons or the links change.
    link_table: LinkTable,
    /// The obstacles that block link endpoints on this mesh.
    link_blockers: LinkBlockers,
    /// What each area costs per metre, for every query on this mesh.
    area_costs: NavAreaCosts,
    /// Identity of this mesh for the obstacle carve cache. A clone copies it; the two stay
    /// interchangeable until one of them changes content.
    mesh_id: u64,
    /// Bumped when content the carve depends on changes (costs, links, blockers, origin).
    content_stamp: u64,
}

impl NavMesh {
    /// Builds a navmesh from the compact walkable grid.
    pub(crate) fn from_grid(grid: &WalkableGrid, config: NavMeshBuildParams) -> Self {
        let layered = Layered {
            top: grid,
            lower: &[],
            links: &[],
        };
        let Assembled {
            polys,
            region_count,
            contours,
            min,
            max,
        } = layered.assemble(&config);
        let grid_index = SpatialGrid::build(&polys, min, max, (config.cell_size * 4.0).max(1.0));
        Self {
            polys,
            off_mesh_links: Vec::new(),
            min,
            max,
            config,
            region_count,
            contours,
            base_grid: grid.clone(),
            lower_grids: Vec::new(),
            layer_links: Vec::new(),
            grid: grid_index,
            link_table: LinkTable::default(),
            link_blockers: LinkBlockers::default(),
            area_costs: NavAreaCosts::default(),
            mesh_id: mint_mesh_id(),
            content_stamp: 0,
        }
        .with_resolved_links()
    }

    /// Builds a navmesh from every walkable layer (ADR-8076). With no lower layer this is
    /// [`Self::from_grid`] of the top grid.
    pub(crate) fn from_layers(layers: &WalkableLayers, config: NavMeshBuildParams) -> Self {
        if layers.lower.is_empty() {
            return Self::from_grid(&layers.top, config);
        }
        let layered = Layered {
            top: &layers.top,
            lower: &layers.lower,
            links: &layers.links,
        };
        let Assembled {
            polys,
            region_count,
            contours,
            min,
            max,
        } = layered.assemble(&config);
        let grid_index = SpatialGrid::build(&polys, min, max, (config.cell_size * 4.0).max(1.0));
        Self {
            polys,
            off_mesh_links: Vec::new(),
            min,
            max,
            config,
            region_count,
            contours,
            base_grid: layers.top.clone(),
            lower_grids: layers.lower.clone(),
            layer_links: layers.links.clone(),
            grid: grid_index,
            link_table: LinkTable::default(),
            link_blockers: LinkBlockers::default(),
            area_costs: NavAreaCosts::default(),
            mesh_id: mint_mesh_id(),
            content_stamp: 0,
        }
        .with_resolved_links()
    }

    /// How many walkable layers the mesh was built from: 1 for a single surface per column, more
    /// where walkable surfaces stack (a tunnel under the ground, the ground under a bridge, the
    /// floors of a building; ADR-8076). Meshes from a [`crate::NavTileCache`] have one layer.
    pub fn layer_count(&self) -> usize {
        1 + self.lower_grids.len()
    }

    /// Identity and content generation for [`crate::obstacle::NavObstacles`]'s carve cache.
    pub(crate) fn carve_identity(&self) -> (u64, u64) {
        (self.mesh_id, self.content_stamp)
    }

    fn bump_content(&mut self) {
        self.content_stamp = self.content_stamp.wrapping_add(1);
    }

    /// Re-polygonises the mesh from its pristine cell grid with the given axis-aligned obstacle
    /// boxes carved out (their XZ footprint removes every cell whose surface height falls inside
    /// the box's vertical range). Returns a fresh mesh; the original is untouched.
    ///
    /// This is the "rebuild" side of dynamic obstacles (`docs/specs/22-tier2.md` § T2.2): blocked
    /// cells are removed from the walkable surface, so A* and point location route around a box
    /// that does not span the floor. A full tile rebuild against the level geometry is always
    /// available via [`crate::NavMeshBuilder`]; this is the per-obstacle-set version.
    pub(crate) fn carved(&self, boxes: &[(Vec3, Vec3)]) -> Self {
        #[cfg(test)]
        CARVE_BUILDS.with(|c| c.set(c.get() + 1));
        let _span = tracing::info_span!(
            "nav_mesh_carve",
            boxes = boxes.len(),
            cells = self.base_grid.cells.len()
        )
        .entered();
        let mut grid = self.base_grid.clone();
        carve_grid(&mut grid, boxes);
        let mut lower = self.lower_grids.clone();
        for layer in &mut lower {
            carve_grid(layer, boxes);
        }
        let mut blockers = self.link_blockers.clone();
        blockers
            .shapes
            .extend(boxes.iter().map(|&(min, max)| NavObstacleShape::Box { min, max }));
        self.rebuild(grid, lower, blockers)
    }

    /// Rebuilds the polygon mesh, regions, contours and spatial index from `grid`, and resolves
    /// the links against `blockers`.
    fn rebuild(&self, grid: WalkableGrid, lower: Vec<WalkableGrid>, blockers: LinkBlockers) -> Self {
        let layered = Layered {
            top: &grid,
            lower: &lower,
            links: &self.layer_links,
        };
        let Assembled {
            polys,
            region_count,
            contours,
            min,
            max,
        } = layered.assemble(&self.config);
        let grid_index = SpatialGrid::build(&polys, min, max, (self.config.cell_size * 4.0).max(1.0));
        Self {
            polys,
            off_mesh_links: self.off_mesh_links.clone(),
            min,
            max,
            config: self.config.clone(),
            region_count,
            contours,
            base_grid: grid,
            lower_grids: lower,
            layer_links: self.layer_links.clone(),
            grid: grid_index,
            link_table: LinkTable::default(),
            link_blockers: blockers,
            area_costs: self.area_costs,
            mesh_id: mint_mesh_id(),
            content_stamp: 0,
        }
        .with_resolved_links()
    }

    /// Assembles a mesh from already-linked polygons (the tile cache's stitched tiles). `grid` is
    /// the current walkable grid (obstacles applied), kept as the base for [`Self::carved`].
    pub(crate) fn from_parts(
        polys: Vec<NavPoly>,
        grid: WalkableGrid,
        config: NavMeshBuildParams,
        region_count: u32,
        contours: Vec<Contour>,
        off_mesh_links: Vec<OffMeshLink>,
    ) -> Self {
        let (min, max) = grid_bounds(&grid);
        let grid_index = SpatialGrid::build(&polys, min, max, (config.cell_size * 4.0).max(1.0));
        Self {
            polys,
            off_mesh_links,
            min,
            max,
            config,
            region_count,
            contours,
            base_grid: grid,
            lower_grids: Vec::new(),
            layer_links: Vec::new(),
            grid: grid_index,
            link_table: LinkTable::default(),
            link_blockers: LinkBlockers::default(),
            area_costs: NavAreaCosts::default(),
            mesh_id: mint_mesh_id(),
            content_stamp: 0,
        }
        .with_resolved_links()
    }

    /// Resolves the link table against the current polygons and blockers.
    /// Moves the whole mesh by `delta` (the floating-origin step of spec 78 § 12.2): polygons,
    /// portals, contours, bounds, the walkable grid, the spatial index, links and their blockers.
    /// Polygon numbers stay, so a path planned before stays valid once shifted; the link table is
    /// resolved again over the moved links.
    pub fn shift_origin(&mut self, delta: Vec3) {
        if !delta.is_finite() || delta == Vec3::ZERO {
            return;
        }
        for poly in &mut self.polys {
            for v in &mut poly.vertices {
                *v += delta;
            }
            for portal in &mut poly.portals {
                portal.a += delta;
                portal.b += delta;
            }
            poly.min += delta;
            poly.max += delta;
        }
        // Contours are XZ outlines at y = 0.
        let flat = Vec3::new(delta.x, 0.0, delta.z);
        for contour in &mut self.contours {
            for point in contour.loops.iter_mut().flatten() {
                *point += flat;
            }
        }
        self.min += delta;
        self.max += delta;
        self.base_grid.origin += delta;
        for y in self.base_grid.cells.iter_mut().flatten() {
            *y += delta.y;
        }
        for layer in &mut self.lower_grids {
            layer.origin += delta;
            for y in layer.cells.iter_mut().flatten() {
                *y += delta.y;
            }
        }
        self.grid.origin += delta;
        for link in &mut self.off_mesh_links {
            link.from += delta;
            link.to += delta;
        }
        for shape in &mut self.link_blockers.shapes {
            *shape = shape.translated(delta);
        }
        if let Some(gone) = &mut self.link_blockers.unloaded {
            gone.origin_x += delta.x;
            gone.origin_z += delta.z;
        }
        self.link_table = LinkTable::resolve(self, &self.link_blockers);
        self.bump_content();
    }

    fn with_resolved_links(mut self) -> Self {
        self.link_table = LinkTable::resolve(&self, &self.link_blockers);
        self
    }

    /// What each area costs per metre, for every query on this mesh.
    pub fn area_costs(&self) -> &NavAreaCosts {
        &self.area_costs
    }

    /// Replaces the area cost table. Corridors planned earlier keep their route: a
    /// [`crate::NavCrowd`] on this mesh needs [`crate::NavCrowd::request_replan_all`] afterwards.
    /// [`crate::NavTileCache::set_area_costs`] does that through the revision.
    pub fn set_area_costs(&mut self, costs: NavAreaCosts) {
        self.area_costs = costs;
        self.bump_content();
    }

    /// Sets the off-mesh links.
    pub fn with_off_mesh_links(mut self, links: Vec<OffMeshLink>) -> Self {
        self.set_off_mesh_links(links);
        self
    }

    /// Replaces the off-mesh links and resolves them once (ADR-1821). Editing `off_mesh_links`
    /// in place also works, but every query then resolves a temporary table until this is called.
    pub fn set_off_mesh_links(&mut self, links: Vec<OffMeshLink>) {
        self.off_mesh_links = links;
        self.link_table = LinkTable::resolve(self, &self.link_blockers);
        self.bump_content();
    }

    /// Switches every link with `user_id` on or off; returns how many changed. A
    /// [`crate::NavCrowd`] on this mesh needs [`crate::NavCrowd::request_replan_all`] afterwards:
    /// agents with no path find out about a link that came back only when they plan.
    pub fn set_link_enabled(&mut self, user_id: u64, enabled: bool) -> usize {
        let mut changed = 0;
        for link in &mut self.off_mesh_links {
            if link.user_id == user_id && link.enabled != enabled {
                link.enabled = enabled;
                changed += 1;
            }
        }
        self.link_table = LinkTable::resolve(self, &self.link_blockers);
        if changed > 0 {
            self.bump_content();
        }
        changed
    }

    /// Each link's state in the current graph (active, disabled, blocked or unresolved), in link
    /// order.
    pub fn link_statuses(&self) -> Vec<NavLinkStatus> {
        let table = self.link_table();
        self.off_mesh_links
            .iter()
            .zip(&table.states)
            .enumerate()
            .map(|(index, (link, &state))| NavLinkStatus {
                index,
                user_id: link.user_id,
                state,
            })
            .collect()
    }

    /// Link states with `boxes` added to the stored blockers. The stored polygons and blockers
    /// stay as they are. [`Self::carved`] adds the same box shapes when it rebuilds a query mesh.
    pub(crate) fn link_statuses_with_boxes(&self, boxes: &[(Vec3, Vec3)]) -> Vec<NavLinkStatus> {
        if boxes.is_empty() {
            return self.link_statuses();
        }
        let mut blockers = self.link_blockers.clone();
        blockers
            .shapes
            .extend(boxes.iter().map(|&(min, max)| NavObstacleShape::Box { min, max }));
        let table = LinkTable::resolve(self, &blockers);
        self.off_mesh_links
            .iter()
            .zip(&table.states)
            .enumerate()
            .map(|(index, (link, &state))| NavLinkStatus {
                index,
                user_id: link.user_id,
                state,
            })
            .collect()
    }

    /// The resolved link table: the cached one, or a temporary one when `off_mesh_links` was
    /// edited in place since it was built.
    pub(crate) fn link_table(&self) -> Cow<'_, LinkTable> {
        if self.link_table.fingerprint == links_fingerprint(&self.off_mesh_links) {
            Cow::Borrowed(&self.link_table)
        } else {
            Cow::Owned(LinkTable::resolve(self, &self.link_blockers))
        }
    }

    /// Sets the obstacles that block link endpoints and re-resolves the links.
    pub(crate) fn set_link_blockers(&mut self, blockers: LinkBlockers) {
        self.link_blockers = blockers;
        self.link_table = LinkTable::resolve(self, &self.link_blockers);
        self.bump_content();
    }

    /// The polygons in every spatial-grid bucket the XZ box `(x, z) +- (reach_x, reach_z)` touches
    /// (a candidate set; callers still test each polygon).
    pub(crate) fn candidate_polys(&self, x: f32, z: f32, reach_x: f32, reach_z: f32, out: &mut Vec<u32>) {
        self.grid
            .query_box(x - reach_x, x + reach_x, z - reach_z, z + reach_z, out);
    }

    /// The total polygon count.
    pub fn poly_count(&self) -> usize {
        self.polys.len()
    }

    /// The total number of polygon vertices.
    pub fn vertex_count(&self) -> usize {
        self.polys.iter().map(|p| p.vertices.len()).sum()
    }

    /// The indices of polygons whose bounds overlap the box `[min, max]`.
    pub fn polys_in_box(&self, min: Vec3, max: Vec3) -> Vec<u32> {
        self.polys
            .iter()
            .enumerate()
            .filter(|(_, p)| aabb_overlap(p.min, p.max, min, max))
            .map(|(i, _)| i as u32)
            .collect()
    }
}

/// Whether two axis-aligned boxes `[amin, amax]` and `[bmin, bmax]` overlap.
fn aabb_overlap(amin: Vec3, amax: Vec3, bmin: Vec3, bmax: Vec3) -> bool {
    amin.x <= bmax.x
        && amax.x >= bmin.x
        && amin.y <= bmax.y
        && amax.y >= bmin.y
        && amin.z <= bmax.z
        && amax.z >= bmin.z
}

/// The world bounds of the grid (min/max XZ plus a vertical padding for query tolerance).
fn grid_bounds(grid: &WalkableGrid) -> (Vec3, Vec3) {
    let (mut lo, mut hi) = (f32::INFINITY, f32::NEG_INFINITY);
    for h in grid.cells.iter().flatten() {
        lo = lo.min(*h);
        hi = hi.max(*h);
    }
    if lo > hi {
        (lo, hi) = (0.0, 0.0);
    }
    let min = Vec3::new(grid.origin.x, lo - 1.0, grid.origin.z);
    let max = Vec3::new(
        grid.origin.x + grid.width as f32 * grid.cell_size,
        hi + 1.0,
        grid.origin.z + grid.height as f32 * grid.cell_size,
    );
    (min, max)
}

/// Builds convex polygons by greedily merging same-height walkable cells into maximal rectangles.
fn build_polys(grid: &WalkableGrid, config: &NavMeshBuildParams) -> Vec<NavPoly> {
    build_polys_rect(grid, config, CellRect::full(grid)).0
}

/// Removes every cell of `grid` whose XZ footprint overlaps one of `boxes` and whose surface
/// height falls inside the box's vertical range.
fn carve_grid(grid: &mut WalkableGrid, boxes: &[(Vec3, Vec3)]) {
    for (i, cell) in grid.cells.iter_mut().enumerate() {
        let Some(y) = *cell else {
            continue;
        };
        let x = i % grid.width;
        let z = i / grid.width;
        let x0 = grid.origin.x + x as f32 * grid.cell_size;
        let z0 = grid.origin.z + z as f32 * grid.cell_size;
        let x1 = x0 + grid.cell_size;
        let z1 = z0 + grid.cell_size;
        for &(bmin, bmax) in boxes {
            let overlaps_xz = bmax.x > x0 && bmin.x < x1 && bmax.z > z0 && bmin.z < z1;
            if overlaps_xz && bmin.y <= y && bmax.y >= y {
                *cell = None;
                break;
            }
        }
    }
}

/// The walkable layers a mesh is polygonised from (ADR-8076).
struct Layered<'a> {
    /// Layer 0.
    top: &'a WalkableGrid,
    /// Layers 1.. (same grid layout as `top`).
    lower: &'a [WalkableGrid],
    /// Steps between cells of different layers.
    links: &'a [LayerLink],
}

impl Layered<'_> {
    /// The polygons, region count, contours and bounds. With no lower layer this is the
    /// single-surface build: regions, contours and polygons of `top` alone.
    fn assemble(&self, config: &NavMeshBuildParams) -> Assembled {
        let regions = crate::regions::regionize(self.top, &config.agent);
        let mut contours = crate::contours::trace_contours(&regions, self.top);
        if self.lower.is_empty() {
            let polys = build_polys(self.top, config);
            let (min, max) = grid_bounds(self.top);
            return Assembled {
                polys,
                region_count: regions.count,
                contours,
                min,
                max,
            };
        }
        let _span = tracing::info_span!("nav_mesh_layers", layers = self.lower.len() + 1).entered();
        let (mut polys, top_owner) = build_polys_rect(self.top, config, CellRect::full(self.top));
        let mut owners = vec![top_owner];
        let mut region_base = regions.count;
        for layer in self.lower {
            let layer_regions = crate::regions::regionize(layer, &config.agent);
            for mut contour in crate::contours::trace_contours(&layer_regions, layer) {
                contour.region += region_base;
                contours.push(contour);
            }
            region_base += layer_regions.count;
            let (layer_polys, mut owner) = build_polys_rect(layer, config, CellRect::full(layer));
            let offset = polys.len() as u32;
            for o in owner.iter_mut().filter(|o| **o != u32::MAX) {
                *o += offset;
            }
            polys.extend(layer_polys.into_iter().map(|mut p| {
                for portal in &mut p.portals {
                    portal.neighbor += offset;
                }
                p
            }));
            owners.push(owner);
        }
        let grid_of = |layer: u16| -> Option<&WalkableGrid> {
            match layer {
                0 => Some(self.top),
                k => self.lower.get(usize::from(k) - 1),
            }
        };
        for link in self.links {
            let (Some(ga), Some(gb)) = (grid_of(link.a_layer), grid_of(link.b_layer)) else {
                continue;
            };
            let (a_cell, b_cell) = (link.a_cell as usize, link.b_cell as usize);
            let (Some(&pa), Some(&pb)) = (
                owners[usize::from(link.a_layer)].get(a_cell),
                owners[usize::from(link.b_layer)].get(b_cell),
            ) else {
                continue;
            };
            if pa == u32::MAX || pb == u32::MAX || pa == pb {
                continue;
            }
            let (Some(ha), Some(hb)) = (ga.cells[a_cell], gb.cells[b_cell]) else {
                continue;
            };
            // `b_cell` is the +X or the +Z neighbour; with one column there is no +X.
            let along_x = ga.width > 1 && b_cell == a_cell + 1;
            let (x, z) = (a_cell % ga.width, a_cell / ga.width);
            let (a, b) = shared_cell_edge(ga, x, z, along_x, ha.min(hb));
            add_portal(&mut polys, pa, pb, a, b);
        }
        let region_count = crate::tile_cache::connected_components(&polys);
        let (mut min, mut max) = grid_bounds(self.top);
        let mut any_top = self.top.cells.iter().any(Option::is_some);
        for layer in self.lower {
            if !layer.cells.iter().any(Option::is_some) {
                continue;
            }
            let (lo, hi) = grid_bounds(layer);
            if any_top {
                min = min.min(lo);
                max = max.max(hi);
            } else {
                (min, max) = (lo, hi);
                any_top = true;
            }
        }
        Assembled {
            polys,
            region_count,
            contours,
            min,
            max,
        }
    }
}

/// What [`Layered::assemble`] builds.
struct Assembled {
    polys: Vec<NavPoly>,
    region_count: u32,
    contours: Vec<Contour>,
    min: Vec3,
    max: Vec3,
}

/// The two corners of the shared side of cell `(x, z)` and its `+X` (`along_x`) or `+Z`
/// neighbour, at height `h`.
fn shared_cell_edge(grid: &WalkableGrid, x: usize, z: usize, along_x: bool, h: f32) -> (Vec3, Vec3) {
    let c = grid.cell_size;
    if along_x {
        let px = grid.origin.x + (x as f32 + 1.0) * c;
        let z0 = grid.origin.z + z as f32 * c;
        let z1 = grid.origin.z + (z as f32 + 1.0) * c;
        (Vec3::new(px, h, z0), Vec3::new(px, h, z1))
    } else {
        let pz = grid.origin.z + (z as f32 + 1.0) * c;
        let x0 = grid.origin.x + x as f32 * c;
        let x1 = grid.origin.x + (x as f32 + 1.0) * c;
        (Vec3::new(x0, h, pz), Vec3::new(x1, h, pz))
    }
}

/// A rectangle of grid cells `[x0, x1) x [z0, z1)` (a navmesh tile, or the whole grid).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellRect {
    /// First cell column.
    pub x0: usize,
    /// First cell row.
    pub z0: usize,
    /// One past the last cell column.
    pub x1: usize,
    /// One past the last cell row.
    pub z1: usize,
}

impl CellRect {
    /// The whole grid.
    pub(crate) fn full(grid: &WalkableGrid) -> Self {
        Self {
            x0: 0,
            z0: 0,
            x1: grid.width,
            z1: grid.height,
        }
    }

    /// Cells along X.
    pub(crate) fn width(&self) -> usize {
        self.x1 - self.x0
    }

    /// The rect-local index of the grid cell `(x, z)` (which must lie inside the rect).
    pub(crate) fn local(&self, x: usize, z: usize) -> usize {
        (z - self.z0) * self.width() + (x - self.x0)
    }
}

/// Polygonises the cells inside `rect` only (greedy same-height rectangle merge), linking the
/// polygons that share a cell edge inside the rect. Returns the polygons (portal neighbours are
/// indices into the returned list) and the rect-local owner map (`u32::MAX` = not walkable).
///
/// Vertex positions are computed from global cell indices, so a tile's polygons line up exactly
/// with its neighbours' and a rect covering the whole grid reproduces the untiled mesh.
pub(crate) fn build_polys_rect(
    grid: &WalkableGrid,
    config: &NavMeshBuildParams,
    rect: CellRect,
) -> (Vec<NavPoly>, Vec<u32>) {
    let width = grid.width;
    let rect_w = rect.width();
    let mut owner = vec![u32::MAX; rect_w * (rect.z1 - rect.z0)];
    let mut polys: Vec<NavPoly> = Vec::new();

    for z in rect.z0..rect.z1 {
        for x in rect.x0..rect.x1 {
            let idx = z * width + x;
            if owner[rect.local(x, z)] != u32::MAX {
                continue;
            }
            let Some(h) = grid.cells[idx] else {
                continue;
            };
            let area = grid.area_at(idx);
            let same = |i: usize| grid.cells[i] == Some(h) && grid.area_at(i) == area;
            // Grow right while same height and area.
            let mut x1 = x;
            while x1 + 1 < rect.x1 {
                let nidx = z * width + (x1 + 1);
                if owner[rect.local(x1 + 1, z)] == u32::MAX && same(nidx) {
                    x1 += 1;
                } else {
                    break;
                }
            }
            // Grow down while the entire row [x..=x1] matches.
            let mut z1 = z;
            'down: while z1 + 1 < rect.z1 {
                for cx in x..=x1 {
                    let nidx = (z1 + 1) * width + cx;
                    if owner[rect.local(cx, z1 + 1)] != u32::MAX || !same(nidx) {
                        break 'down;
                    }
                }
                z1 += 1;
            }
            // Mark owned and emit a rectangle polygon.
            let poly_index = polys.len() as u32;
            for cz in z..=z1 {
                for cx in x..=x1 {
                    owner[rect.local(cx, cz)] = poly_index;
                }
            }
            let c = grid.cell_size;
            let x0 = grid.origin.x + x as f32 * c;
            let z0 = grid.origin.z + z as f32 * c;
            let x1w = grid.origin.x + (x1 as f32 + 1.0) * c;
            let z1w = grid.origin.z + (z1 as f32 + 1.0) * c;
            let vertices = vec![
                Vec3::new(x0, h, z0),
                Vec3::new(x1w, h, z0),
                Vec3::new(x1w, h, z1w),
                Vec3::new(x0, h, z1w),
            ];
            polys.push(NavPoly {
                min: Vec3::new(x0, h - config.agent.height, z0),
                max: Vec3::new(x1w, h + 1.0, z1w),
                area,
                vertices,
                portals: Vec::new(),
            });
        }
    }

    // Link polygons sharing a cell edge. Walk each cell's right and down neighbours.
    for z in rect.z0..rect.z1 {
        for x in rect.x0..rect.x1 {
            let this = owner[rect.local(x, z)];
            if this == u32::MAX {
                continue;
            }
            for (dx, dz) in [(1usize, 0usize), (0, 1)] {
                let (nx, nz) = (x + dx, z + dz);
                if nx >= rect.x1 || nz >= rect.z1 {
                    continue;
                }
                let other = owner[rect.local(nx, nz)];
                if other == u32::MAX || other == this {
                    continue;
                }
                if let Some((a, b)) = cell_edge_portal(grid, config, x, z, dx == 1) {
                    add_portal(&mut polys, this, other, a, b);
                }
            }
        }
    }

    (polys, owner)
}

/// The portal across the shared edge of cell `(x, z)` and its `+X` neighbour (`along_x`) or `+Z`
/// neighbour, or `None` when either cell is not walkable or the step between them is taller than
/// the agent can climb. Endpoints are at the shared side's corners, at the lower surface height.
pub(crate) fn cell_edge_portal(
    grid: &WalkableGrid,
    config: &NavMeshBuildParams,
    x: usize,
    z: usize,
    along_x: bool,
) -> Option<(Vec3, Vec3)> {
    let (nx, nz) = if along_x { (x + 1, z) } else { (x, z + 1) };
    if nx >= grid.width || nz >= grid.height {
        return None;
    }
    let h_this = grid.cells[z * grid.width + x]?;
    let h_other = grid.cells[nz * grid.width + nx]?;
    // A step taller than the agent can climb is not traversable: no portal.
    if (h_this - h_other).abs() > config.agent.max_climb + 1e-4 {
        return None;
    }
    Some(shared_cell_edge(grid, x, z, along_x, h_this.min(h_other)))
}

/// Adds a bidirectional portal between `a` and `b` (deduplicating the reverse link). The portal
/// endpoints `pa`/`pb` are the two corners of one cell-length of the shared edge; when the two
/// rectangles share a longer edge, each consecutive cell extends the existing portal so it spans
/// the full shared edge rather than collapsing to the first 1-cell sliver.
pub(crate) fn add_portal(polys: &mut [NavPoly], a: u32, b: u32, pa: Vec3, pb: Vec3) {
    // Extend an existing portal to include the 1-cell edge `(pa, pb)`. The edge is axis-aligned:
    // a constant-z edge extends along X, a constant-x edge along Z.
    let merge = |portal: &mut Portal| {
        if (pa.x - pb.x).abs() > (pa.z - pb.z).abs() {
            // Horizontal edge (constant z): span the min..max x of the shared side.
            let x_min = pa.x.min(pb.x);
            let x_max = pa.x.max(pb.x);
            portal.a.x = portal.a.x.min(x_min);
            portal.b.x = portal.b.x.max(x_max);
        } else {
            // Vertical edge (constant x): span the min..max z of the shared side.
            let z_min = pa.z.min(pb.z);
            let z_max = pa.z.max(pb.z);
            portal.a.z = portal.a.z.min(z_min);
            portal.b.z = portal.b.z.max(z_max);
        }
    };
    match polys[a as usize].portals.iter_mut().find(|p| p.neighbor == b) {
        Some(p) => merge(p),
        None => polys[a as usize].portals.push(Portal {
            neighbor: b,
            a: pa,
            b: pb,
        }),
    }
    match polys[b as usize].portals.iter_mut().find(|p| p.neighbor == a) {
        Some(p) => merge(p),
        None => polys[b as usize].portals.push(Portal {
            neighbor: a,
            a: pa,
            b: pb,
        }),
    }
}

/// The point on segment `(ax, az) - (bx, bz)` closest to `(x, z)`.
fn seg_closest(x: f32, z: f32, ax: f32, az: f32, bx: f32, bz: f32) -> (f32, f32) {
    let dx = bx - ax;
    let dz = bz - az;
    let len_sq = dx * dx + dz * dz;
    if len_sq <= f32::EPSILON {
        return (ax, az);
    }
    let t = (((x - ax) * dx + (z - az) * dz) / len_sq).clamp(0.0, 1.0);
    (ax + t * dx, az + t * dz)
}

/// Squared distance from `(x, z)` to the segment `(ax, az) - (bx, bz)`.
fn seg_dist_sq(x: f32, z: f32, ax: f32, az: f32, bx: f32, bz: f32) -> f32 {
    let dx = bx - ax;
    let dz = bz - az;
    let len_sq = dx * dx + dz * dz;
    if len_sq <= f32::EPSILON {
        let ddx = x - ax;
        let ddz = z - az;
        return ddx * ddx + ddz * ddz;
    }
    let t = (((x - ax) * dx + (z - az) * dz) / len_sq).clamp(0.0, 1.0);
    let px = ax + t * dx;
    let pz = az + t * dz;
    let ddx = x - px;
    let ddz = z - pz;
    ddx * ddx + ddz * ddz
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::NavMeshBuildParams;

    fn grid(width: usize, height: usize, walkable: &[(usize, usize, f32)]) -> WalkableGrid {
        let mut cells = vec![None; width * height];
        for &(x, z, y) in walkable {
            cells[z * width + x] = Some(y);
        }
        WalkableGrid {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width,
            height,
            cells,
            areas: Vec::new(),
        }
    }

    #[test]
    fn a_full_grid_merges_into_one_rectangle() {
        let walkable: Vec<(usize, usize, f32)> =
            (0..4).flat_map(|z| (0..4).map(move |x| (x, z, 0.0))).collect();
        let g = grid(4, 4, &walkable);
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        assert_eq!(mesh.poly_count(), 1, "one merged rectangle");
        assert_eq!(mesh.vertex_count(), 4);
        let poly = &mesh.polys[0];
        assert!(poly.contains_xz(1.5, 1.5));
        assert!(!poly.contains_xz(5.0, 5.0));
    }

    #[test]
    fn an_obstacle_splits_into_four_rectangles_with_portals() {
        // A 3x3 floor with the centre cell missing → 4 L/bar rectangles, all connected.
        let walkable: Vec<(usize, usize, f32)> = (0..3)
            .flat_map(|z| (0..3).map(move |x| (x, z, 0.0)))
            .filter(|&(x, z, _)| !(x == 1 && z == 1))
            .collect();
        let g = grid(3, 3, &walkable);
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        assert!(
            mesh.poly_count() >= 4,
            "obstacle forces a split: {}",
            mesh.poly_count()
        );
        // Every polygon reaches every other through portals (single connected region).
        assert!(mesh.polys.iter().all(|p| !p.portals.is_empty()));
    }

    #[test]
    fn different_heights_do_not_merge() {
        let g = grid(2, 1, &[(0, 0, 0.0), (1, 0, 1.0)]);
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        assert_eq!(mesh.poly_count(), 2, "different heights stay separate");
    }

    #[test]
    fn a_step_above_max_climb_gets_no_portal() {
        // Two adjacent cells whose height difference exceeds max_climb (0.5) must not connect.
        let g = grid(2, 1, &[(0, 0, 0.0), (1, 0, 2.0)]);
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        assert_eq!(mesh.poly_count(), 2);
        assert!(
            mesh.polys.iter().all(|p| p.portals.is_empty()),
            "an unclimbable step yields no portal"
        );
    }

    #[test]
    fn a_step_within_max_climb_still_connects() {
        // A 0.4 m step is within max_climb (0.5): the two polygons share a portal.
        let g = grid(2, 1, &[(0, 0, 0.0), (1, 0, 0.4)]);
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        assert_eq!(mesh.poly_count(), 2);
        assert!(
            mesh.polys.iter().any(|p| !p.portals.is_empty()),
            "a climbable step keeps its portal"
        );
    }

    #[test]
    fn a_portal_spans_the_full_shared_edge() {
        // Two rectangles share a 3-cell-tall vertical edge at x = 2 (the right column is a
        // climbable 0.4 m step up). The single portal must span all of z ∈ [0, 3), not collapse
        // to the first 1-cell sliver.
        let walkable: Vec<(usize, usize, f32)> = (0..3)
            .flat_map(|z| (0..3).map(move |x| (x, z, if x == 2 { 0.4 } else { 0.0 })))
            .collect();
        let g = grid(3, 3, &walkable);
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        assert_eq!(mesh.poly_count(), 2, "a left and a right rectangle");
        let portal = mesh.polys[0]
            .portals
            .iter()
            .find(|p| p.neighbor == 1)
            .expect("the rectangles share a portal");
        let z_span = (portal.a.z - portal.b.z).abs();
        assert!(
            (z_span - 3.0).abs() < 1e-3,
            "the portal spans the full shared edge, got {portal:?}"
        );
    }

    #[test]
    fn carving_removes_cells_under_a_box() {
        // A 4x4 floor; a box over the centre leaves a ring of walkable cells.
        let walkable: Vec<(usize, usize, f32)> =
            (0..4).flat_map(|z| (0..4).map(move |x| (x, z, 0.0))).collect();
        let g = grid(4, 4, &walkable);
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        // The box covers cells x∈[1,3) z∈[1,3) at y=0 → carves a 2x2 hole.
        let carved = mesh.carved(&[(Vec3::new(1.0, -1.0, 1.0), Vec3::new(3.0, 1.0, 3.0))]);
        assert!(carved.poly_count() >= 1);
        // The centre of the box is not on the carved mesh.
        let q = crate::query::NavQuery {
            mesh: &carved,
            obstacles: None,
        };
        assert!(
            q.find_nearest(Vec3::new(2.0, 0.0, 2.0), Vec3::splat(0.6))
                .is_none(),
            "the carved center is gone"
        );
        // A corner still has a polygon.
        assert!(
            q.find_nearest(Vec3::new(0.5, 0.0, 0.5), Vec3::splat(0.6))
                .is_some(),
            "the ring around the box is walkable"
        );
    }

    #[test]
    fn off_mesh_links_are_stored() {
        let g = grid(1, 1, &[(0, 0, 0.0)]);
        let mesh =
            NavMesh::from_grid(&g, NavMeshBuildParams::default()).with_off_mesh_links(vec![OffMeshLink {
                from: Vec3::new(0.0, 0.0, 0.0),
                to: Vec3::new(1.0, 0.0, 1.0),
                bidirectional: true,
                area: 0,
                ..OffMeshLink::new(Vec3::ZERO, Vec3::ZERO)
            }]);
        assert_eq!(mesh.off_mesh_links.len(), 1);
        assert!(mesh.off_mesh_links[0].bidirectional);
    }

    #[test]
    fn bounds_follow_the_floor_height() {
        let g = grid(2, 2, &[(0, 0, 30.0), (1, 0, 30.0)]);
        let mesh = NavMesh::from_grid(&g, NavMeshBuildParams::default());
        assert!(
            mesh.max.y >= 30.0 && mesh.min.y <= 30.0,
            "{:?} {:?}",
            mesh.min,
            mesh.max
        );
        assert!(mesh.max.y < 40.0);
    }

    /// audit sprint-audit-g-nav-perf M5: a polygon that covers several spatial cells is visited
    /// once per query, not once per overlapped cell.
    #[test]
    fn query_box_visits_each_polygon_once() {
        let side = 8;
        let walkable: Vec<(usize, usize, f32)> = (0..side)
            .flat_map(|z| (0..side).map(move |x| (x, z, 0.0)))
            .collect();
        let mesh = NavMesh::from_grid(&grid(side, side, &walkable), NavMeshBuildParams::default());
        assert_eq!(mesh.poly_count(), 1, "one height merges into one polygon");
        reset_query_box_visits();
        let mut out = Vec::new();
        mesh.candidate_polys(4.0, 4.0, 20.0, 20.0, &mut out);
        let visits = query_box_visits();
        assert_eq!(out, vec![0], "the wide box still returns the one polygon");
        assert_eq!(
            visits,
            mesh.poly_count() as u64,
            "visits {visits} should equal the polygon count {}, not one per overlapped cell",
            mesh.poly_count()
        );
    }
}
