// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/navmesh.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-1943, mark: as-is (Stage A: unchanged).
// Edits: module path (`crate::math` -> `regia_detsim::math`, the canonical float bits from `regia_detsim::hash`); the
// module docs rewritten and doc comments added on public items (DO27 file, map, unit and process names dropped);
// float `min`/`max`/`clamp` through `regia_detsim::float::det_*` (ADR-8012 A-4: same bits except a zero's sign,
// which the wire and the bake hash fold anyway); `bake_hash` moved above the tests; clippy forms
// (`unwrap` -> index, range `contains`, a struct literal); `from_bytes` refuses a bucket index past
// `MAX_LOADED_BUCKETS` (fuzz hardening; every bake within the cap loads as before). ADR-8174.
// Also from the same commit: ir-content/src/math.rs lines 145-152 (`canonical_f32_bits`), mark: as-is, in the
// tests only, as the reference `do27_canonical_f32_bits` the lockstep kit's function must equal.

//! A deterministic navigation mesh: one baked polygon soup per map for on-foot bodies, arenas, big maps
//! and tunnel corridors, beside the grid ([`super::grid`]) that keeps hover hulls on open terrain.
//!
//! **Bake.** [`NavMesh::bake`] turns [`BakeParts`] into convex polygons: open ground merged from a
//! walkable [`GroundRaster`] into rectangles, one quad per [`Corridor`] segment (tubes, galleries,
//! ramps; [`PolyKind::Tunnel`]), one per [`Room`], [`StairRun`] and [`DoorQuad`]. Polygons that share an
//! edge segment link through that segment (the portal); polygons that overlap at a matching floor
//! (junctions, ramp openings, stair landings, door rims) link through a short portal across the overlap.
//! A link records its width, floor step and headroom, so one bake serves every [`MeshAgent`]: a door
//! passes a 0.35 m body and refuses a 2.5 m hull.
//!
//! **Query.** [`NavMesh::locate`] and [`NavMesh::locate_near`] find the polygon under a body's feet,
//! [`NavMesh::straight_walkable`] proves a straight leg polygon by polygon, and [`NavMesh::find_path`] runs
//! A* over polygons with integer costs (decimeters), an `(f, h, id)` tie-break and an expansion budget
//! ([`NAVMESH_MAX_EXPANSIONS`]), then pulls the portal midpoints into a short [`MeshPlan`].
//! [`NavMesh::find_path_scaled`] makes a meter inside a tunnel cost a share of a meter of ground.
//!
//! **Determinism.** Every iteration order is fixed (input order, sorted links, sorted buckets, the A*
//! tie-break). Floats come only from IEEE `+ - * /`, [`regia_detsim::math::sqrt`] and
//! [`regia_detsim::float`]'s order functions, so every peer and every architecture bakes the same mesh;
//! the wire and the bake hash write floats through [`regia_detsim::hash::canonical_f32_bits`] (`-0` is
//! `+0`, every NaN is `0x7fc0_0000`). No query reads a clock: a search is bounded by expansions.
//!
//! **Wire.** [`NavMesh::to_bytes`] writes the cooked mesh (`NVM2`, [`NAVMESH_VERSION`]); [`NavMesh::from_bytes`]
//! refuses bad magic, another version, truncation, trailing bytes and non-finite floats. The format is
//! registered as `nav-det-navmesh` ([`NAVMESH_LAYOUT`]). [`NavMesh::bake_id`] hashes the bake inputs, so a
//! host can compare a loaded mesh with its cook manifest.
//!
//! ```
//! use regia_nav_det::navmesh::{BakeParts, GroundRaster, MeshAgent, NavMesh};
//!
//! let parts = BakeParts {
//!     ground: Some(GroundRaster {
//!         cols: 8,
//!         rows: 6,
//!         step_m: 5.0,
//!         origin_x: 0.0,
//!         origin_z: 0.0,
//!         walk: vec![true; 48],
//!         height: vec![0.0; 48],
//!     }),
//!     extent_m: 40.0,
//!     ..BakeParts::default()
//! };
//! let mesh = NavMesh::bake(&parts).expect("a walkable raster bakes");
//! let plan = mesh
//!     .find_path([2.0, 0.0, 2.0], [33.0, 0.0, 22.0], &MeshAgent::pilot())
//!     .expect("open ground routes");
//! assert_eq!(plan.waypoints.first(), Some(&[2.0, 0.0, 2.0]));
//! let loaded = NavMesh::from_bytes(&mesh.to_bytes()).expect("the cooked bytes load");
//! assert_eq!(loaded.bake_id(), mesh.bake_id());
//! ```
//!
//! See the book page "Deterministic navigation mesh" (`book/agents/det-navmesh.md`).

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::hash::canonical_f32_bits;
use regia_detsim::math;

/// The book page's example (`book/agents/det-navmesh.md`, "How a host arms it") runs as a doctest.
/// The crate compiles its own copy (`fixtures/det-navmesh.md`: `cargo package` copies only the crate
/// folder, ADR-1485); `tests/det_navmesh_book_copy.rs` holds it byte-identical to the book page.
#[cfg(doctest)]
#[doc = include_str!("../fixtures/det-navmesh.md")]
struct BookPage;

/// Version tag of the serialized mesh. Bump when the bake rules change; the
/// authority refuses a mesh whose tag does not match.
pub const NAVMESH_VERSION: u32 = 2;
/// Maps wider than this route foot traffic on the mesh. The grid's memory
/// grows with the square of the map size; past about 2 km the mesh carries
/// the pilots instead.
pub const NAVMESH_MAP_EXTENT_M: f32 = 2000.0;
/// Cell expansions one mesh query may spend before it gives up (the caller
/// falls back to the grid). Bounds the tick cost without a clock.
pub const NAVMESH_MAX_EXPANSIONS: u32 = 4_000;
/// Spatial bucket size for point location (m).
const BUCKET_M: f32 = 8.0;
/// Endpoints this far off the mesh still snap onto it (m).
pub const MESH_SNAP_M: f32 = 6.0;
/// A portal narrower than this is dropped at bake: nobody walks it.
const MIN_PORTAL_M: f32 = 0.4;
/// Two floors this far apart or less may share a link (m). A lava tube 30 m
/// under the ground never links to the field above it; a ramp top does.
const LINK_FLOOR_M: f32 = 2.0;
/// Greedy string-pull skips at most this many portals in one leg.
const MAX_PULL_SKIP: usize = 32;
/// Hard cap on the polygon walk inside [`NavMesh::straight_walkable`].
const MAX_LINE_STEPS: usize = 128;

/// Entity origins may sit above the feet (body centre, hover offset): the
/// router retries the mesh locate at these height drops before it falls
/// back to the grid.
pub const MESH_LOCATE_DROPS: [f32; 3] = [0.0, -0.9, -1.8];

/// A pilot capsule: the mesh's first citizen.
pub const FOOT_RADIUS_M: f32 = 0.35;
/// Standing height of a pilot plus a little kit (m).
pub const FOOT_HEIGHT_M: f32 = 1.8;
/// Highest step a pilot takes in one stride (m; matches the capsule rule).
pub const FOOT_STEP_UP_M: f32 = 0.55;
/// Deepest drop a pilot sticks to without falling (m).
pub const FOOT_STEP_DOWN_M: f32 = 0.55;
/// Highest step a hover hull climbs in one stride (m). The grid's hover
/// cliff is 0.78 rise over run; a ramp that rises at most about 0.69 keeps
/// its 5 m sample step well inside this.
pub const HULL_STEP_UP_M: f32 = 4.0;
/// Deepest drop a hover hull sticks to without falling (m), same bound.
pub const HULL_STEP_DOWN_M: f32 = 4.0;

/// What a polygon is. Bake-time only; the query only reads floors, portals
/// and clearance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PolyKind {
    /// Open ground merged from the walkable raster.
    Ground = 0,
    /// A tunnel passage floor (tube, gallery or ramp).
    Tunnel = 1,
    /// An arena or facility room floor.
    Room = 2,
    /// A stair or ramp run between two levels.
    Stair = 3,
    /// A personnel doorway between rooms.
    Door = 4,
}

impl PolyKind {
    fn from_u8(v: u8) -> Option<PolyKind> {
        match v {
            0 => Some(PolyKind::Ground),
            1 => Some(PolyKind::Tunnel),
            2 => Some(PolyKind::Room),
            3 => Some(PolyKind::Stair),
            4 => Some(PolyKind::Door),
            _ => None,
        }
    }
}

/// One convex walkable polygon. Vertices run counter-clockwise in XZ with a
/// floor height each (ramps tilt); `ceil` is the headroom over the floor.
#[derive(Debug, Clone, PartialEq)]
pub struct NavPoly {
    /// Vertices in world XZ, counter-clockwise.
    pub verts: Vec<[f32; 2]>,
    /// Floor height per vertex, same order as `verts`.
    pub floor: Vec<f32>,
    /// Ceiling height (world Y). Open ground uses a very large value.
    pub ceil: f32,
    /// Storey index from the cook (informational; links are decided by
    /// floors, so a stacked room never joins the one below it).
    pub level: i32,
    /// Navigable width of the walkable surface (m): a corridor's full width,
    /// a room's narrow side, a door's opening. Open ground is [`f32::MAX`].
    /// Overlap portals (junctions and mouths) inherit the narrower side, so a
    /// hull that fits the passage passes the portal and a wider one is refused.
    pub width: f32,
    /// What the polygon was baked from.
    pub kind: PolyKind,
}

/// One shared edge between two polygons. The portal is the walked part of
/// the edge; `width` is its length, `step` the floor difference at its
/// middle (positive climbs from `a` to `b`), and `clearance` the headroom
/// over the portal.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NavLink {
    /// The neighbouring polygon.
    pub nbr: u32,
    /// First portal endpoint in world XZ.
    pub a: [f32; 2],
    /// Second portal endpoint in world XZ.
    pub b: [f32; 2],
    /// Navigable width of the portal (m).
    pub width: f32,
    /// Floor difference at the portal's middle (m); positive climbs toward `nbr`.
    pub step: f32,
    /// Headroom over the portal (m).
    pub clearance: f32,
}

/// The body asking for a route.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshAgent {
    /// Body radius (m). Portals narrower than twice this are refused.
    pub radius: f32,
    /// Body height (m). Links with less clearance are refused.
    pub height: f32,
    /// Highest step the body climbs (m).
    pub step_up: f32,
    /// Deepest step the body drops (m).
    pub step_down: f32,
}

impl MeshAgent {
    /// A pilot on foot.
    pub fn pilot() -> MeshAgent {
        MeshAgent {
            radius: FOOT_RADIUS_M,
            height: FOOT_HEIGHT_M,
            step_up: FOOT_STEP_UP_M,
            step_down: FOOT_STEP_DOWN_M,
        }
    }

    /// A pilot-scaled agent with this hull's radius (walkers, hulls that fit
    /// a door test). Height scales with the body; the step stays walkable.
    pub fn for_radius(radius: f32) -> MeshAgent {
        MeshAgent {
            radius,
            height: if radius <= 1.5 {
                FOOT_HEIGHT_M
            } else {
                radius + 2.0
            },
            step_up: FOOT_STEP_UP_M,
            step_down: FOOT_STEP_DOWN_M,
        }
    }

    /// A hover hull: a disc of `radius` that climbs the hover step limit.
    /// Used for tunnel corridors (a hull fits a 22 m tube, not a 1.10 m door).
    pub fn hover(radius: f32) -> MeshAgent {
        MeshAgent {
            radius,
            height: radius + 2.0,
            step_up: HULL_STEP_UP_M,
            step_down: HULL_STEP_DOWN_M,
        }
    }

    fn passes(&self, link: &NavLink) -> bool {
        link.width + 1e-3 >= self.radius * 2.0
            && link.clearance + 1e-3 >= self.height
            && link.step <= self.step_up + 1e-3
            && -link.step <= self.step_down + 1e-3
    }
}

/// A route across the mesh: feet waypoints with floor heights.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeshPlan {
    /// World waypoints (feet). First is the requester's position.
    pub waypoints: Vec<[f32; 3]>,
    /// Polygon per leg start, same length as `waypoints`.
    pub polys: Vec<u32>,
    /// Search expansions spent.
    pub expansions: u32,
    /// Route length in metres.
    pub length: f32,
}

/// Which surface a unit routes on. Hover hulls stay on the grid on open
/// terrain; foot traffic uses the mesh once the authority baked one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouteKind {
    /// Route on the navigation grid.
    Grid,
    /// Route on the navigation mesh.
    Mesh,
}

/// The router. `is_hover_hull` is true for the hover profiles (small and
/// large); every other profile is foot-scale traffic. The mesh carries foot
/// traffic wherever the authority armed one: arenas, big maps and tunnel
/// corridors baked onto the mesh. Hover hulls move onto the mesh only when
/// it has tunnel corridors (`mesh_tunnels`); otherwise they stay on the grid.
pub fn route_for(is_hover_hull: bool, mesh_armed: bool, mesh_tunnels: bool) -> RouteKind {
    if !mesh_armed || (is_hover_hull && !mesh_tunnels) {
        RouteKind::Grid
    } else {
        RouteKind::Mesh
    }
}

// ---------------------------------------------------------------- bake input

/// Walkable raster for open ground: `walk[i]` tells whether cell `i` (row
/// major, `cols` by `rows`) is walkable, `height[i]` its ground height.
/// The cooker fills this from the heightfield with the profile's slope
/// limit; adjacent walkable cells merge into rectangular polygons.
#[derive(Debug, Clone)]
pub struct GroundRaster {
    /// Cells along X.
    pub cols: usize,
    /// Cells along Z.
    pub rows: usize,
    /// Cell size (m).
    pub step_m: f32,
    /// World X of cell column 0's center.
    pub origin_x: f32,
    /// World Z of cell row 0's center.
    pub origin_z: f32,
    /// Walkable flag per cell, row major (`z * cols + x`).
    pub walk: Vec<bool>,
    /// Ground height per cell (world Y), row major.
    pub height: Vec<f32>,
}

/// One tunnel passage floor: centreline floor points with a half width and a
/// ceiling. Consecutive quads share their cross edge exactly; passages join
/// each other where a sample stands inside another passage's quad at a
/// matching floor (junctions), and meet the ground where a ramp top overlaps
/// a ground quad at a matching floor (openings).
#[derive(Debug, Clone)]
pub struct Corridor {
    /// Centerline floor points (world XYZ), at least two, no repeats.
    pub points: Vec<[f32; 3]>,
    /// Half the passage width (m).
    pub half_width_m: f32,
    /// Headroom over the floor (m).
    pub ceiling_m: f32,
}

/// One room floor: an axis-aligned rectangle at one level with headroom.
#[derive(Debug, Clone)]
pub struct Room {
    /// Smallest X (m).
    pub min_x: f32,
    /// Smallest Z (m).
    pub min_z: f32,
    /// Largest X (m).
    pub max_x: f32,
    /// Largest Z (m).
    pub max_z: f32,
    /// Floor height (world Y).
    pub floor_y: f32,
    /// Ceiling height (world Y), above the floor.
    pub ceil_y: f32,
    /// Storey index (informational).
    pub level: i32,
}

/// One stair or ramp run: a straight tilted quad from edge to edge.
#[derive(Debug, Clone)]
pub struct StairRun {
    /// Centre of the lower edge.
    pub from: [f32; 2],
    /// Centre of the upper edge.
    pub to: [f32; 2],
    /// Run width (m).
    pub width_m: f32,
    /// Floor height at the lower edge (world Y).
    pub y0: f32,
    /// Floor height at the upper edge (world Y).
    pub y1: f32,
    /// Headroom over the higher end (m).
    pub ceil_m: f32,
}

/// One personnel doorway: a narrow quad bridging two rooms. Its width is the
/// filter: a pilot capsule (0.35 m) fits a 1.10 m door, a hull disc does not.
#[derive(Debug, Clone)]
pub struct DoorQuad {
    /// Centre of the opening.
    pub center: [f32; 2],
    /// Direction across the wall (unit XZ).
    pub dir: [f32; 2],
    /// Opening width (m); at least the portal rule (0.4 m).
    pub width_m: f32,
    /// Depth through the wall (m); at least 0.3 m is baked.
    pub depth_m: f32,
    /// Floor height (world Y).
    pub floor_y: f32,
    /// Top of the opening (world Y), above the floor.
    pub ceil_y: f32,
    /// Storey index (informational).
    pub level: i32,
}

/// Everything one map's mesh is baked from, at cook time.
#[derive(Debug, Clone, Default)]
pub struct BakeParts {
    /// Open ground, if the map has any.
    pub ground: Option<GroundRaster>,
    /// Tunnel passages (tubes, galleries, ramps).
    pub corridors: Vec<Corridor>,
    /// Room floors.
    pub rooms: Vec<Room>,
    /// Stair and ramp runs.
    pub stairs: Vec<StairRun>,
    /// Personnel doorways.
    pub doors: Vec<DoorQuad>,
    /// Full map width in metres (decides the big-map rule).
    pub extent_m: f32,
    /// True for indoor arena maps (every room, stair and door is walked).
    pub arena: bool,
}

// ---------------------------------------------------------------- mesh

/// The baked mesh: convex polygons, sorted links, and a spatial index.
/// `Clone` is cheap enough to share; the authority holds one `Arc` of it.
#[derive(Debug, Clone, Default)]
pub struct NavMesh {
    polys: Vec<NavPoly>,
    /// Per-polygon links, ascending neighbour id.
    adj: Vec<Vec<NavLink>>,
    min_x: f32,
    min_z: f32,
    buckets_x: usize,
    buckets_z: usize,
    /// Polygon ids per bucket, ascending.
    buckets: Vec<Vec<u32>>,
    /// Content hash of the bake inputs (see [`NavMesh::bake_id`]).
    id: u64,
    extent_m: f32,
    arena: bool,
}

impl NavMesh {
    /// Content hash of the bake: map extent, arena flag and every input.
    /// The authority compares it against the cook manifest before loading.
    pub fn bake_id(&self) -> u64 {
        self.id
    }

    /// Full map width the bake was made for (m).
    pub fn extent_m(&self) -> f32 {
        self.extent_m
    }

    /// True for an indoor arena bake.
    pub fn is_arena(&self) -> bool {
        self.arena
    }

    /// Number of polygons.
    pub fn poly_count(&self) -> usize {
        self.polys.len()
    }

    /// True when the bake has tunnel corridors (tube, gallery or ramp). A
    /// tunnel-carrying mesh also routes hover hulls; a room-only mesh does not.
    pub fn has_tunnels(&self) -> bool {
        self.polys.iter().any(|p| p.kind == PolyKind::Tunnel)
    }

    /// The kind of one polygon, for callers that want to label a route leg.
    pub fn poly_kind(&self, id: u32) -> Option<PolyKind> {
        self.polys.get(id as usize).map(|p| p.kind)
    }

    /// Number of links, counting each direction.
    pub fn link_count(&self) -> usize {
        self.adj.iter().map(Vec::len).sum()
    }

    /// One line for the map-load log: polygons, links, extent, arena flag.
    pub fn describe(&self) -> String {
        format!(
            "navmesh bake {:016x}: {} polys, {} links, extent {} m{}",
            self.bake_id(),
            self.poly_count(),
            self.link_count(),
            self.extent_m,
            if self.arena { ", arena" } else { "" }
        )
    }

    fn push_poly(&mut self, poly: NavPoly) {
        self.adj.push(Vec::new());
        self.polys.push(poly);
    }

    /// Insert a link, both ways. Duplicates keep the widest portal; the
    /// reverse portal swaps endpoints and negates the step. `finish` sorts.
    /// `portal_width` is the navigable width of the opening: the shared-edge
    /// length for edge neighbours, and the narrower passage width for overlap
    /// portals (junctions, mouths) whose string-pull segment is deliberately
    /// short.
    fn try_link(&mut self, a: u32, b: u32, pa: [f32; 2], pb: [f32; 2], portal_width: f32) {
        if a == b {
            return;
        }
        let (na, nb) = (a as usize, b as usize);
        if na >= self.polys.len() || nb >= self.polys.len() {
            return;
        }
        let width = portal_width;
        if width < MIN_PORTAL_M {
            return;
        }
        let fa = self.floor_at(na, pa);
        let fb = self.floor_at(nb, pb);
        let Some(fa) = fa else { return };
        let Some(fb) = fb else { return };
        if (fa - fb).abs() > LINK_FLOOR_M {
            return;
        }
        let mid = [(pa[0] + pb[0]) * 0.5, (pa[1] + pb[1]) * 0.5];
        let ca = self.floor_at(na, mid).unwrap_or(fa);
        let cb = self.floor_at(nb, mid).unwrap_or(fb);
        let clearance = det_min(self.polys[na].ceil, self.polys[nb].ceil) - det_max(ca, cb);
        let fwd = NavLink {
            nbr: b,
            a: pa,
            b: pb,
            width,
            step: cb - ca,
            clearance,
        };
        let rev = NavLink {
            nbr: a,
            a: pb,
            b: pa,
            width,
            step: ca - cb,
            clearance,
        };
        insert_link(&mut self.adj[na], fwd);
        insert_link(&mut self.adj[nb], rev);
    }

    /// Floor height over polygon `i` at `(x, z)`: the plane through the
    /// first three vertices (bake keeps quads near-planar; degenerate
    /// triples fall back to the first vertex floor).
    fn floor_at(&self, i: usize, p: [f32; 2]) -> Option<f32> {
        let poly = self.polys.get(i)?;
        plane_floor(poly, p)
    }

    /// Finish a bake: sort every link list, build the spatial index, hash.
    fn finish(&mut self, id: u64, extent_m: f32, arena: bool) {
        for links in &mut self.adj {
            links.sort_by_key(|a| a.nbr);
            links.dedup_by(|a, b| a.nbr == b.nbr);
        }
        self.id = id;
        self.extent_m = extent_m;
        self.arena = arena;
        self.build_index();
    }

    fn build_index(&mut self) {
        let mut min_x = f32::MAX;
        let mut min_z = f32::MAX;
        let mut max_x = f32::MIN;
        let mut max_z = f32::MIN;
        for poly in &self.polys {
            for v in &poly.verts {
                min_x = det_min(min_x, v[0]);
                min_z = det_min(min_z, v[1]);
                max_x = det_max(max_x, v[0]);
                max_z = det_max(max_z, v[1]);
            }
        }
        if self.polys.is_empty() {
            min_x = 0.0;
            min_z = 0.0;
            max_x = 0.0;
            max_z = 0.0;
        }
        self.min_x = min_x;
        self.min_z = min_z;
        self.buckets_x = (((max_x - min_x) / BUCKET_M).floor() as usize + 1).max(1);
        self.buckets_z = (((max_z - min_z) / BUCKET_M).floor() as usize + 1).max(1);
        self.buckets = vec![Vec::new(); self.buckets_x * self.buckets_z];
        for (i, poly) in self.polys.iter().enumerate() {
            let mut x0 = usize::MAX;
            let mut x1 = 0usize;
            let mut z0 = usize::MAX;
            let mut z1 = 0usize;
            for v in &poly.verts {
                let bx = (((v[0] - min_x) / BUCKET_M).floor() as usize).min(self.buckets_x - 1);
                let b_z = (((v[1] - min_z) / BUCKET_M).floor() as usize).min(self.buckets_z - 1);
                x0 = x0.min(bx);
                x1 = x1.max(bx);
                z0 = z0.min(b_z);
                z1 = z1.max(b_z);
            }
            for b_z in z0..=z1 {
                for bx in x0..=x1 {
                    self.buckets[b_z * self.buckets_x + bx].push(i as u32);
                }
            }
        }
        for bucket in &mut self.buckets {
            bucket.sort_unstable();
            bucket.dedup();
        }
    }

    fn bucket_at(&self, x: f32, z: f32) -> Option<usize> {
        if self.buckets.is_empty() {
            return None;
        }
        let bx = ((x - self.min_x) / BUCKET_M).floor() as i32;
        let b_z = ((z - self.min_z) / BUCKET_M).floor() as i32;
        if bx < 0 || b_z < 0 || bx >= self.buckets_x as i32 || b_z >= self.buckets_z as i32 {
            return None;
        }
        Some(b_z as usize * self.buckets_x + bx as usize)
    }
}

fn insert_link(list: &mut Vec<NavLink>, link: NavLink) {
    if let Some(slot) = list.iter_mut().find(|l| l.nbr == link.nbr) {
        // Keep the widest portal between the pair.
        if link.width > slot.width {
            *slot = link;
        }
        return;
    }
    list.push(link);
}

/// Plane floor of a near-planar polygon at `p`. `None` for degenerate
/// polygons (fewer than 3 vertices).
fn plane_floor(poly: &NavPoly, p: [f32; 2]) -> Option<f32> {
    if poly.verts.len() < 3 || poly.floor.len() != poly.verts.len() {
        return None;
    }
    let a = poly.verts[0];
    let b = poly.verts[1];
    let c = poly.verts[2];
    let yab = poly.floor[1] - poly.floor[0];
    let yac = poly.floor[2] - poly.floor[0];
    let ex = [b[0] - a[0], b[1] - a[1]];
    let fx = [c[0] - a[0], c[1] - a[1]];
    let det = ex[0] * fx[1] - ex[1] * fx[0];
    if det.abs() < 1e-9 {
        return Some(poly.floor[0]);
    }
    // Solve p - a = s * ex + t * fx; floor = y0 + s * yab + t * yac.
    let rx = [p[0] - a[0], p[1] - a[1]];
    let s = (rx[0] * fx[1] - rx[1] * fx[0]) / det;
    let t = (ex[0] * rx[1] - ex[1] * rx[0]) / det;
    Some(poly.floor[0] + s * yab + t * yac)
}

fn dist2(a: [f32; 2], b: [f32; 2]) -> f32 {
    let dx = a[0] - b[0];
    let dz = a[1] - b[1];
    math::sqrt(dx * dx + dz * dz)
}

/// True when `p` is inside the convex counter-clockwise polygon (edges
/// count as inside, so shared edges locate deterministically by id order).
fn point_in_convex(verts: &[[f32; 2]], p: [f32; 2]) -> bool {
    let n = verts.len();
    if n < 3 {
        return false;
    }
    let mut sign = 0.0f32;
    for i in 0..n {
        let a = verts[i];
        let b = verts[(i + 1) % n];
        let cross = (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]);
        if cross.abs() < 1e-6 {
            continue;
        }
        if sign == 0.0 {
            sign = cross;
        } else if (sign > 0.0) != (cross > 0.0) {
            return false;
        }
    }
    true
}

/// Signed area (positive is counter-clockwise).
fn signed_area(verts: &[[f32; 2]]) -> f32 {
    let mut sum = 0.0f32;
    for i in 0..verts.len() {
        let a = verts[i];
        let b = verts[(i + 1) % verts.len()];
        sum += a[0] * b[1] - b[0] * a[1];
    }
    sum * 0.5
}

/// Order `verts` counter-clockwise, dropping a repeated closing vertex.
fn make_ccw(verts: &mut Vec<[f32; 2]>, floor: &mut Vec<f32>) {
    if verts.len() > 1 && verts[0] == verts[verts.len() - 1] {
        verts.pop();
        floor.pop();
    }
    if signed_area(verts) < 0.0 {
        verts.reverse();
        floor.reverse();
    }
}

// ---------------------------------------------------------------- bake

/// What went wrong at cook time. The cooker reports the string and bakes
/// nothing; the authority never sees a half mesh.
pub type BakeError = String;

impl NavMesh {
    /// Bake one map's mesh from its parts, at cook time. Deterministic:
    /// polygons come out in input order (ground rows, corridors, rooms,
    /// stairs, doors) and links are sorted, so two cooks of the same parts
    /// produce identical bytes.
    pub fn bake(parts: &BakeParts) -> Result<NavMesh, BakeError> {
        let mut mesh = NavMesh::default();
        if let Some(ground) = &parts.ground {
            bake_ground(&mut mesh, ground)?;
        }
        for (ci, corridor) in parts.corridors.iter().enumerate() {
            if corridor.points.len() < 2 {
                return Err(format!("corridor {ci} has fewer than 2 points"));
            }
            if corridor.half_width_m <= 0.0 {
                return Err(format!("corridor {ci} has no width"));
            }
            bake_corridor(&mut mesh, corridor)?;
        }
        for (ri, room) in parts.rooms.iter().enumerate() {
            bake_room(&mut mesh, room, ri)?;
        }
        for (si, stair) in parts.stairs.iter().enumerate() {
            bake_stair(&mut mesh, stair, si)?;
        }
        for (di, door) in parts.doors.iter().enumerate() {
            bake_door(&mut mesh, door, di)?;
        }
        link_shared_edges(&mut mesh);
        // Junctions, openings and stair landings: a quad vertex standing
        // inside another quad at a matching floor joins them. Runs after
        // every part is baked so stairs meet rooms and ramps meet ground.
        link_overlaps(&mut mesh);
        let id = bake_hash(parts);
        mesh.finish(id, parts.extent_m, parts.arena);
        if mesh.poly_count() == 0 {
            return Err("bake produced no polygons".to_string());
        }
        Ok(mesh)
    }
}

/// Merge each row's walkable runs, then merge runs with the same XZ span
/// across consecutive rows, into rectangular ground polygons.
fn bake_ground(mesh: &mut NavMesh, ground: &GroundRaster) -> Result<(), BakeError> {
    let (cols, rows) = (ground.cols, ground.rows);
    if cols == 0 || rows == 0 {
        return Err("ground raster is empty".to_string());
    }
    if ground.walk.len() != cols * rows || ground.height.len() != cols * rows {
        return Err("ground raster lengths do not match cols * rows".to_string());
    }
    // Open runs per row: (x0, x1) inclusive.
    let mut runs: Vec<Vec<(usize, usize)>> = Vec::with_capacity(rows);
    for z in 0..rows {
        let mut row = Vec::new();
        let mut x = 0;
        while x < cols {
            if ground.walk[z * cols + x] {
                let x0 = x;
                while x < cols && ground.walk[z * cols + x] {
                    x += 1;
                }
                row.push((x0, x - 1));
            } else {
                x += 1;
            }
        }
        runs.push(row);
    }
    // Grow rectangles down: a run continues the rectangle above it when the
    // span matches exactly. Rectangles tile edge to edge, so every shared
    // edge links in full.
    struct Open {
        x0: usize,
        x1: usize,
        z0: usize,
    }
    let mut open: Vec<Open> = Vec::new();
    let mut rects: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (z, row) in runs.iter().enumerate() {
        let mut next: Vec<Open> = Vec::new();
        for &(x0, x1) in row {
            if let Some(k) = open.iter().position(|o| o.x0 == x0 && o.x1 == x1) {
                let o = open.remove(k);
                next.push(Open { x0, x1, z0: o.z0 });
            } else {
                next.push(Open { x0, x1, z0: z });
            }
        }
        for o in open.drain(..) {
            rects.push((o.x0, o.z0, o.x1, z - 1));
        }
        open = next;
    }
    for o in open.drain(..) {
        rects.push((o.x0, o.z0, o.x1, rows - 1));
    }
    for (x0, z0, x1, z1) in rects {
        let hx = |cx: usize| ground.origin_x + cx as f32 * ground.step_m;
        let hz = |cz: usize| ground.origin_z + cz as f32 * ground.step_m;
        let h = ground.step_m * 0.5;
        let (ax, az) = (hx(x0) - h, hz(z0) - h);
        let (bx, b_z) = (hx(x1) + h, hz(z1) + h);
        // Corner floors from the nearest raster heights.
        let at = |x: f32, z: f32| {
            let cx =
                (((x - ground.origin_x) / ground.step_m).round() as i64).clamp(0, cols as i64 - 1) as usize;
            let cz =
                (((z - ground.origin_z) / ground.step_m).round() as i64).clamp(0, rows as i64 - 1) as usize;
            ground.height[cz * cols + cx]
        };
        let mut verts = vec![[ax, az], [bx, az], [bx, b_z], [ax, b_z]];
        let mut floor = vec![at(ax, az), at(bx, az), at(bx, b_z), at(ax, b_z)];
        make_ccw(&mut verts, &mut floor);
        mesh.push_poly(NavPoly {
            verts,
            floor,
            ceil: f32::MAX,
            level: 0,
            width: f32::MAX,
            kind: PolyKind::Ground,
        });
    }
    Ok(())
}

/// One quad per centreline segment. Quad `i` spans sample `i` to `i + 1`,
/// so consecutive quads share their cross edge exactly.
fn bake_corridor(mesh: &mut NavMesh, corridor: &Corridor) -> Result<(), BakeError> {
    let pts = &corridor.points;
    let half = corridor.half_width_m;
    let mut left: Vec<[f32; 2]> = Vec::with_capacity(pts.len());
    for i in 0..pts.len() {
        // Travel direction at this sample (forward difference, last one back).
        let (ax, az) = if i + 1 < pts.len() {
            (pts[i + 1][0] - pts[i][0], pts[i + 1][2] - pts[i][2])
        } else {
            (pts[i][0] - pts[i - 1][0], pts[i][2] - pts[i - 1][2])
        };
        let len = math::sqrt(ax * ax + az * az);
        if len < 1e-6 {
            return Err("corridor has a repeated point".to_string());
        }
        // Left of travel.
        left.push([-az / len, ax / len]);
    }
    for i in 0..pts.len() - 1 {
        let a = pts[i];
        let b = pts[i + 1];
        let (la, lb) = (left[i], left[i + 1]);
        let mut verts = vec![
            [a[0] + la[0] * half, a[2] + la[1] * half],
            [a[0] - la[0] * half, a[2] - la[1] * half],
            [b[0] - lb[0] * half, b[2] - lb[1] * half],
            [b[0] + lb[0] * half, b[2] + lb[1] * half],
        ];
        let mut floor = vec![a[1], a[1], b[1], b[1]];
        if signed_area(&verts).abs() < 1e-6 {
            return Err("corridor folds back on itself".to_string());
        }
        make_ccw(&mut verts, &mut floor);
        let ceil = det_max(a[1], b[1]) + corridor.ceiling_m;
        mesh.push_poly(NavPoly {
            verts,
            floor,
            ceil,
            level: -1,
            width: corridor.half_width_m * 2.0,
            kind: PolyKind::Tunnel,
        });
    }
    Ok(())
}

fn bake_room(mesh: &mut NavMesh, room: &Room, ri: usize) -> Result<(), BakeError> {
    if room.max_x <= room.min_x || room.max_z <= room.min_z {
        return Err(format!("room {ri} is empty"));
    }
    if room.ceil_y <= room.floor_y {
        return Err(format!("room {ri} has no headroom"));
    }
    let mut verts = vec![
        [room.min_x, room.min_z],
        [room.max_x, room.min_z],
        [room.max_x, room.max_z],
        [room.min_x, room.max_z],
    ];
    let mut floor = vec![room.floor_y; 4];
    make_ccw(&mut verts, &mut floor);
    mesh.push_poly(NavPoly {
        verts,
        floor,
        ceil: room.ceil_y,
        level: room.level,
        width: det_min(room.max_x - room.min_x, room.max_z - room.min_z),
        kind: PolyKind::Room,
    });
    Ok(())
}

fn bake_stair(mesh: &mut NavMesh, stair: &StairRun, si: usize) -> Result<(), BakeError> {
    let dx = stair.to[0] - stair.from[0];
    let dz = stair.to[1] - stair.from[1];
    let len = math::sqrt(dx * dx + dz * dz);
    if len < 1e-6 {
        return Err(format!("stair {si} has no run"));
    }
    if stair.width_m <= 0.0 {
        return Err(format!("stair {si} has no width"));
    }
    // Left of the climb.
    let lx = -dz / len;
    let lz = dx / len;
    let h = stair.width_m * 0.5;
    let mut verts = vec![
        [stair.from[0] + lx * h, stair.from[1] + lz * h],
        [stair.from[0] - lx * h, stair.from[1] - lz * h],
        [stair.to[0] - lx * h, stair.to[1] - lz * h],
        [stair.to[0] + lx * h, stair.to[1] + lz * h],
    ];
    let mut floor = vec![stair.y0, stair.y0, stair.y1, stair.y1];
    make_ccw(&mut verts, &mut floor);
    mesh.push_poly(NavPoly {
        verts,
        floor,
        ceil: det_max(stair.y1, stair.y0) + stair.ceil_m,
        level: 0,
        width: stair.width_m,
        kind: PolyKind::Stair,
    });
    Ok(())
}

fn bake_door(mesh: &mut NavMesh, door: &DoorQuad, di: usize) -> Result<(), BakeError> {
    if door.width_m < MIN_PORTAL_M {
        return Err(format!("door {di} is narrower than the portal rule"));
    }
    if door.ceil_y <= door.floor_y {
        return Err(format!("door {di} has no headroom"));
    }
    let len = math::sqrt(door.dir[0] * door.dir[0] + door.dir[1] * door.dir[1]);
    if len < 1e-6 {
        return Err(format!("door {di} has no direction"));
    }
    let (dx, dz) = (door.dir[0] / len, door.dir[1] / len);
    // Across the wall: perpendicular of the crossing direction.
    let (px, pz) = (-dz, dx);
    let hw = door.width_m * 0.5;
    let hd = det_max(door.depth_m, 0.3) * 0.5;
    let (cx, cz) = (door.center[0], door.center[1]);
    let mut verts = vec![
        [cx + px * hw - dx * hd, cz + pz * hw - dz * hd],
        [cx - px * hw - dx * hd, cz - pz * hw - dz * hd],
        [cx - px * hw + dx * hd, cz - pz * hw + dz * hd],
        [cx + px * hw + dx * hd, cz + pz * hw + dz * hd],
    ];
    let mut floor = vec![door.floor_y; 4];
    make_ccw(&mut verts, &mut floor);
    mesh.push_poly(NavPoly {
        verts,
        floor,
        ceil: door.ceil_y,
        level: door.level,
        width: door.width_m,
        kind: PolyKind::Door,
    });
    Ok(())
}

/// Link every pair of polygons that shares an edge segment. The portal is
/// the shared part; a shared corner alone is not a link.
fn link_shared_edges(mesh: &mut NavMesh) {
    let n = mesh.polys.len();
    // Edge list per polygon: (a, b) undirected.
    let mut edges: Vec<Vec<([f32; 2], [f32; 2])>> = Vec::with_capacity(n);
    for poly in &mesh.polys {
        let mut list = Vec::with_capacity(poly.verts.len());
        for i in 0..poly.verts.len() {
            list.push((poly.verts[i], poly.verts[(i + 1) % poly.verts.len()]));
        }
        edges.push(list);
    }
    for a in 0..n {
        for b in (a + 1)..n {
            // No level rule here: try_link already refuses floors that do
            // not match, so a room stacked over another never links while a
            // ramp top meets the ground it surfaces through.
            for &(a0, a1) in &edges[a] {
                for &(b0, b1) in &edges[b] {
                    if let Some((pa, pb)) = shared_segment(a0, a1, b0, b1) {
                        mesh.try_link(a as u32, b as u32, pa, pb, dist2(pa, pb));
                    }
                }
            }
        }
    }
}

/// Shared part of two collinear overlapping segments, or `None`. Endpoints
/// snap within 5 cm so quad edges that meet at a mitre still join.
fn shared_segment(a0: [f32; 2], a1: [f32; 2], b0: [f32; 2], b1: [f32; 2]) -> Option<([f32; 2], [f32; 2])> {
    const EPS: f32 = 0.05;
    let dx = a1[0] - a0[0];
    let dz = a1[1] - a0[1];
    let len2 = dx * dx + dz * dz;
    if len2 < 1e-9 {
        return None;
    }
    // Both of b's endpoints must sit on a's line.
    let on_line = |p: [f32; 2]| ((p[0] - a0[0]) * dz - (p[1] - a0[1]) * dx).abs() / math::sqrt(len2) <= EPS;
    if !on_line(b0) || !on_line(b1) {
        return None;
    }
    // Project onto a's axis.
    let len = math::sqrt(len2);
    let ux = dx / len;
    let uz = dz / len;
    let proj = |p: [f32; 2]| (p[0] - a0[0]) * ux + (p[1] - a0[1]) * uz;
    let (mut t0, mut t1) = (proj(b0), proj(b1));
    if t0 > t1 {
        core::mem::swap(&mut t0, &mut t1);
    }
    let (lo, hi) = (det_max(t0, 0.0), det_min(t1, len));
    if hi - lo < MIN_PORTAL_M {
        return None;
    }
    Some((
        [a0[0] + ux * lo, a0[1] + uz * lo],
        [a0[0] + ux * hi, a0[1] + uz * hi],
    ))
}

/// Junctions, openings and landings for every pair at once: a quad vertex
/// standing inside another quad at a matching floor joins them (tunnel
/// junctions, ramp openings, stair landings, door rims). Pairs run in
/// ascending id order, so the result is deterministic. Cook rule: polygons
/// that must not connect (stacked rooms, walled neighbours) share neither
/// edges nor vertices; the floor check is the second guard.
fn link_overlaps(mesh: &mut NavMesh) {
    let n = mesh.poly_count();
    // Bounding boxes first: the vertex test only runs on pairs that touch.
    let mut boxes: Vec<[f32; 4]> = Vec::with_capacity(n);
    for poly in &mesh.polys {
        let mut bb = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
        for v in &poly.verts {
            bb[0] = det_min(bb[0], v[0]);
            bb[1] = det_min(bb[1], v[1]);
            bb[2] = det_max(bb[2], v[0]);
            bb[3] = det_max(bb[3], v[1]);
        }
        boxes.push(bb);
    }
    for a in 0..n {
        for b in (a + 1)..n {
            let (ba, bb) = (boxes[a], boxes[b]);
            if ba[2] < bb[0] || bb[2] < ba[0] || ba[3] < bb[1] || bb[3] < ba[1] {
                continue;
            }
            // Any vertex of `a` inside `b`, or vice versa (rims count). A
            // stair's quad runs bottom-to-top: its first contained vertex
            // can be the bottom one, whose floor matches the room below,
            // not the room above. Try every contained vertex on both sides
            // and link at the first whose floors agree, so a stair (or a
            // ramp) joins the room its TOP lands in too.
            let mut at: Option<[f32; 2]> = None;
            'found: for (src, dst) in [(a, b), (b, a)] {
                for v in mesh.polys[src].verts.clone() {
                    if point_in_convex(&mesh.polys[dst].verts, v) {
                        let fa = mesh.floor_at(src, v).unwrap_or(f32::MAX);
                        let fb = mesh.floor_at(dst, v).unwrap_or(f32::MAX);
                        if (fa - fb).abs() <= LINK_FLOOR_M {
                            at = Some(v);
                            break 'found;
                        }
                    }
                }
            }
            let Some(at) = at else { continue };
            let fa = mesh.floor_at(a, at).unwrap_or(f32::MAX);
            let fb = mesh.floor_at(b, at).unwrap_or(f32::MAX);
            if (fa - fb).abs() > LINK_FLOOR_M {
                continue;
            }
            // Portal: a short segment across the overlap, so the pull has
            // something to walk through. The navigable width is the narrower
            // passage, so a hull that fits the corridor passes the portal and
            // a wider one is refused at the mouth or junction.
            let width = det_min(mesh.polys[a].width, mesh.polys[b].width);
            if width < MIN_PORTAL_M {
                continue;
            }
            let h = det_min(width, 2.0) * 0.5;
            // Axis: perpendicular of the host quad's longest edge.
            let [mut ex, mut ez] = longest_edge(&mesh.polys[b]);
            let el = det_max(math::sqrt(ex * ex + ez * ez), 1e-6);
            ex /= el;
            ez /= el;
            let pa = [at[0] - ez * h, at[1] + ex * h];
            let pb = [at[0] + ez * h, at[1] - ex * h];
            mesh.try_link(a as u32, b as u32, pa, pb, width);
        }
    }
}

/// Longest edge as a direction vector.
fn longest_edge(poly: &NavPoly) -> [f32; 2] {
    let mut best = [1.0, 0.0];
    let mut len = 0.0f32;
    for i in 0..poly.verts.len() {
        let a = poly.verts[i];
        let b = poly.verts[(i + 1) % poly.verts.len()];
        let d = dist2(a, b);
        if d > len {
            len = d;
            best = [b[0] - a[0], b[1] - a[1]];
        }
    }
    best
}

// ---------------------------------------------------------------- query

impl NavMesh {
    /// Polygons whose bucket contains `(x, z)`, ascending id.
    fn candidates(&self, x: f32, z: f32) -> &[u32] {
        match self.bucket_at(x, z) {
            Some(b) => &self.buckets[b],
            None => &[],
        }
    }

    /// The polygon the feet at `pos` stand on: inside the polygon, headroom
    /// for `agent.height`, and the floor within the step window of the feet.
    /// Shared edges locate to the lowest id (deterministic).
    pub fn locate(&self, pos: [f32; 3], agent: &MeshAgent) -> Option<u32> {
        let mut best: Option<(f32, u32)> = None;
        for &id in self.candidates(pos[0], pos[2]) {
            let poly = &self.polys[id as usize];
            if !point_in_convex(&poly.verts, [pos[0], pos[2]]) {
                continue;
            }
            let Some(floor) = self.floor_at(id as usize, [pos[0], pos[2]]) else {
                continue;
            };
            let rise = floor - pos[1];
            if rise > agent.step_up || -rise > agent.step_down {
                continue;
            }
            if poly.ceil - pos[1] < agent.height {
                continue;
            }
            let gap = rise.abs();
            if best.is_none_or(|(bd, bi)| gap < bd || (gap == bd && id < bi)) {
                best = Some((gap, id));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Nearest polygon to `(x, z)` within [`MESH_SNAP_M`] that fits the
    /// agent's headroom, or `None`. Snap is XZ only; the caller keeps its
    /// own feet height.
    pub fn locate_near(&self, x: f32, z: f32, agent: &MeshAgent) -> Option<u32> {
        let mut best: Option<(f32, u32)> = None;
        // Buckets the snap disc touches.
        let r = (MESH_SNAP_M / BUCKET_M).ceil() as i32 + 1;
        let cx = ((x - self.min_x) / BUCKET_M).floor() as i32;
        let cz = ((z - self.min_z) / BUCKET_M).floor() as i32;
        let mut seen: Vec<u32> = Vec::new();
        for dz in -r..=r {
            for dx in -r..=r {
                let (bx, b_z) = (cx + dx, cz + dz);
                if bx < 0 || b_z < 0 || bx >= self.buckets_x as i32 || b_z >= self.buckets_z as i32 {
                    continue;
                }
                for &id in &self.buckets[b_z as usize * self.buckets_x + bx as usize] {
                    if seen.contains(&id) {
                        continue;
                    }
                    seen.push(id);
                    let poly = &self.polys[id as usize];
                    if poly.ceil < agent.height && poly.ceil < 1.0e8 {
                        // No floor reference here; skip only hopelessly low lids.
                        continue;
                    }
                    let d = dist_to_convex(&poly.verts, [x, z]);
                    if d > MESH_SNAP_M {
                        continue;
                    }
                    if best.is_none_or(|(bd, bi)| d < bd || (d == bd && id < bi)) {
                        best = Some((d, id));
                    }
                }
            }
        }
        // `seen.contains` is linear, but the snap disc covers few buckets;
        // keep std-only and allocation-light instead of a hash set.
        best.map(|(_, id)| id)
    }

    /// The walk has reached `goal`: inside the current polygon, and standing
    /// on it or on a linked neighbour (a goal exactly on a shared rim
    /// locates to the lowest id, which may be next door).
    fn at_goal(&self, cur: u32, goal: [f32; 3], agent: &MeshAgent) -> bool {
        if !point_in_convex(&self.polys[cur as usize].verts, [goal[0], goal[2]]) {
            return false;
        }
        match self.locate(goal, agent) {
            Some(g) => g == cur || self.adj[cur as usize].iter().any(|l| l.nbr == g),
            None => false,
        }
    }

    /// True when the straight segment from `from` to `goal` (feet) stays on
    /// linked polygons the agent fits. Walks the mesh polygon by polygon:
    /// each leg is proven, never sampled.
    pub fn straight_walkable(&self, from: [f32; 3], goal: [f32; 3], agent: &MeshAgent) -> bool {
        let Some(mut cur) = self.locate(from, agent) else {
            return false;
        };
        if self.at_goal(cur, goal, agent) {
            return true;
        }
        let dir = [goal[0] - from[0], goal[2] - from[2]];
        let total = math::sqrt(dir[0] * dir[0] + dir[1] * dir[1]);
        if total < 1e-6 {
            return true;
        }
        let dir = [dir[0] / total, dir[1] / total];
        let mut at = [from[0], from[2]];
        for _ in 0..MAX_LINE_STEPS {
            if self.at_goal(cur, goal, agent) {
                return true;
            }
            // First portal the ray crosses.
            let mut cross: Option<(f32, u32)> = None;
            for link in &self.adj[cur as usize] {
                if !agent.passes(link) {
                    continue;
                }
                if let Some(t) = ray_segment_t(at, dir, link.a, link.b)
                    && t > 1e-4
                    && cross.is_none_or(|(bt, bi)| t < bt || (t == bt && link.nbr < bi))
                {
                    cross = Some((t, link.nbr));
                }
            }
            let Some((t, next)) = cross else { return false };
            // The goal must lie past the crossing (else it was inside).
            // `at` and `dir` are XZ. `goal` is XYZ, so the horizontal pair is x and z.
            let along = (goal[0] - at[0]) * dir[0] + (goal[2] - at[1]) * dir[1];
            if t >= along - 1e-3 {
                return point_in_convex(&self.polys[next as usize].verts, [goal[0], goal[2]]);
            }
            cur = next;
            at = [at[0] + dir[0] * (t + 1e-3), at[1] + dir[1] * (t + 1e-3)];
        }
        false
    }

    /// Route feet from `from` to `goal` across the mesh. Endpoints snap onto
    /// the mesh within [`MESH_SNAP_M`]; links the agent does not fit are
    /// refused (doors stop hulls, ceilings stop tall bodies). `None` when an
    /// endpoint is off-mesh, the goal is unreachable, or the expansion
    /// budget runs out (the caller falls back to the grid).
    pub fn find_path(&self, from: [f32; 3], goal: [f32; 3], agent: &MeshAgent) -> Option<MeshPlan> {
        self.find_path_scaled(from, goal, agent, 100)
    }

    /// [`Self::find_path`] with a tunnel cost share: a metre inside a tunnel
    /// polygon costs `tunnel_pct` percent of a metre of open ground (75 for
    /// "shorter or safer", 20 for a flank). The heuristic is scaled by the
    /// same share while it is under 100, so it stays admissible.
    pub fn find_path_scaled(
        &self,
        from: [f32; 3],
        goal: [f32; 3],
        agent: &MeshAgent,
        tunnel_pct: u32,
    ) -> Option<MeshPlan> {
        let pct = tunnel_pct.clamp(10, 100);
        let start = self
            .locate(from, agent)
            .or_else(|| self.locate_near(from[0], from[2], agent))?;
        let end = self
            .locate(goal, agent)
            .or_else(|| self.locate_near(goal[0], goal[2], agent))?;
        if start == end {
            let poly = &self.polys[start as usize];
            if point_in_convex(&poly.verts, [goal[0], goal[2]]) {
                let gy = self.floor_at(end as usize, [goal[0], goal[2]]).unwrap_or(goal[1]);
                return Some(MeshPlan {
                    waypoints: vec![from, [goal[0], gy, goal[2]]],
                    polys: vec![start, end],
                    length: dist2([from[0], from[2]], [goal[0], goal[2]]),
                    ..MeshPlan::default()
                });
            }
        }
        let n = self.polys.len();
        let goal_xz = [goal[0], goal[2]];
        let heuristic = |id: u32| -> u32 {
            let c = centroid(&self.polys[id as usize]);
            let d = (dist2(c, goal_xz) * 10.0) as u32;
            if pct < 100 { d * pct / 100 } else { d }
        };
        // A* over polygons. (f, h, index) tie-break, ascending neighbour
        // order: two authorities find the same route.
        let mut g = vec![u32::MAX; n];
        let mut parent: Vec<Option<u32>> = vec![None; n];
        let mut via: Vec<[f32; 2]> = vec![[0.0, 0.0]; n];
        let mut closed = vec![false; n];
        let mut heap: std::collections::BinaryHeap<std::cmp::Reverse<(u32, u32, u32)>> =
            std::collections::BinaryHeap::new();
        g[start as usize] = 0;
        via[start as usize] = [from[0], from[2]];
        heap.push(std::cmp::Reverse((heuristic(start), heuristic(start), start)));
        let mut expansions = 0u32;
        let mut reached = false;
        while let Some(std::cmp::Reverse((_, _, id))) = heap.pop() {
            if closed[id as usize] {
                continue;
            }
            closed[id as usize] = true;
            expansions += 1;
            if expansions > NAVMESH_MAX_EXPANSIONS {
                return None;
            }
            if id == end {
                reached = true;
                break;
            }
            let ga = g[id as usize];
            for link in self.adj[id as usize].iter() {
                if !agent.passes(link) || closed[link.nbr as usize] {
                    continue;
                }
                let mid = [(link.a[0] + link.b[0]) * 0.5, (link.a[1] + link.b[1]) * 0.5];
                let raw = (dist2(via[id as usize], mid) * 10.0) as u32;
                let step = if self.polys[id as usize].kind == PolyKind::Tunnel && pct < 100 {
                    raw * pct / 100
                } else {
                    raw
                };
                let ng = ga.saturating_add(step.max(1));
                if ng < g[link.nbr as usize] {
                    g[link.nbr as usize] = ng;
                    parent[link.nbr as usize] = Some(id);
                    via[link.nbr as usize] = mid;
                    let h = heuristic(link.nbr);
                    heap.push(std::cmp::Reverse((ng.saturating_add(h), h, link.nbr)));
                }
            }
        }
        if !reached {
            return None;
        }
        // Polygon path, start first.
        let mut path: Vec<u32> = vec![end];
        while path[path.len() - 1] != start {
            let p = parent[path[path.len() - 1] as usize].expect("reached implies a chain");
            path.push(p);
        }
        path.reverse();
        // Portal midpoints, then a greedy pull: legs are (point, polygon)
        // pairs, and every skipped leg is re-proven with straight_walkable,
        // so each emitted leg is still walked. The unskipped midpoints are
        // valid by convexity: both touch one polygon.
        let mut portals: Vec<[f32; 2]> = Vec::with_capacity(path.len());
        for w in path.windows(2) {
            let (a, b) = (w[0], w[1]);
            let link = self.adj[a as usize]
                .iter()
                .find(|l| l.nbr == b)
                .expect("path follows links");
            portals.push([(link.a[0] + link.b[0]) * 0.5, (link.a[1] + link.b[1]) * 0.5]);
        }
        let floor_of = |mesh: &NavMesh, poly: u32, p: [f32; 2], or: f32| -> f32 {
            mesh.floor_at(poly as usize, p).unwrap_or(or)
        };
        let mut legs: Vec<([f32; 2], u32)> = vec![([from[0], from[2]], path[0])];
        let mut k = 0usize;
        while k < portals.len() {
            // Furthest portal (or the goal) reachable from the anchor.
            let mut far: Option<usize> = None;
            let mut j = (k + MAX_PULL_SKIP).min(portals.len());
            loop {
                let (tp, tpoly) = if j >= portals.len() {
                    (goal_xz, path[path.len() - 1])
                } else {
                    (portals[j], path[j + 1])
                };
                let (ax, apoly) = legs[legs.len() - 1];
                let ay = floor_of(self, apoly, ax, from[1]);
                let by = floor_of(self, tpoly, tp, goal[1]);
                if self.straight_walkable([ax[0], ay, ax[1]], [tp[0], by, tp[1]], agent) {
                    far = Some(j);
                    break;
                }
                if j == k {
                    break;
                }
                j -= 1;
            }
            match far {
                Some(j) if j >= portals.len() => {
                    legs.push((goal_xz, path[path.len() - 1]));
                    break;
                }
                Some(j) => {
                    legs.push((portals[j], path[j + 1]));
                    k = j + 1;
                }
                None => {
                    // Unreachable even one step on (corner grazing): keep
                    // the next midpoint, which is valid by convexity.
                    legs.push((portals[k], path[k + 1]));
                    k += 1;
                }
            }
            if k >= portals.len() {
                legs.push((goal_xz, path[path.len() - 1]));
                break;
            }
        }
        if legs.last().is_none_or(|(p, _)| *p != goal_xz) {
            legs.push((goal_xz, path[path.len() - 1]));
        }
        let mut waypoints = Vec::with_capacity(legs.len());
        let mut polys = Vec::with_capacity(legs.len());
        let mut length = 0.0f32;
        for (i, (leg, poly)) in legs.iter().enumerate() {
            let y = if i == 0 {
                from[1]
            } else {
                floor_of(self, *poly, *leg, goal[1])
            };
            if i > 0 {
                length += dist2(legs[i - 1].0, *leg);
            }
            waypoints.push([leg[0], y, leg[1]]);
            polys.push(*poly);
        }
        Some(MeshPlan {
            waypoints,
            polys,
            expansions,
            length,
        })
    }
}

/// Distance from a point to a convex polygon (0 inside).
fn dist_to_convex(verts: &[[f32; 2]], p: [f32; 2]) -> f32 {
    if point_in_convex(verts, p) {
        return 0.0;
    }
    let mut best = f32::MAX;
    for i in 0..verts.len() {
        let a = verts[i];
        let b = verts[(i + 1) % verts.len()];
        best = det_min(best, dist_to_segment(p, a, b));
    }
    best
}

fn dist_to_segment(p: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    let abx = b[0] - a[0];
    let abz = b[1] - a[1];
    let l2 = abx * abx + abz * abz;
    let t = if l2 < 1e-9 {
        0.0
    } else {
        det_clamp(((p[0] - a[0]) * abx + (p[1] - a[1]) * abz) / l2, 0.0, 1.0)
    };
    dist2(p, [a[0] + abx * t, a[1] + abz * t])
}

/// Ray/segment crossing distance `t` along the ray, or `None` when parallel
/// or past the segment. Deterministic: pure arithmetic, no tables.
fn ray_segment_t(at: [f32; 2], dir: [f32; 2], a: [f32; 2], b: [f32; 2]) -> Option<f32> {
    let ex = b[0] - a[0];
    let ez = b[1] - a[1];
    let denom = dir[0] * ez - dir[1] * ex;
    if denom.abs() < 1e-9 {
        return None;
    }
    let dx = a[0] - at[0];
    let dz = a[1] - at[1];
    let t = (dx * ez - dz * ex) / denom;
    let s = (dx * dir[1] - dz * dir[0]) / denom;
    if t >= 0.0 && (0.0..=1.0).contains(&s) {
        Some(t)
    } else {
        None
    }
}

fn centroid(poly: &NavPoly) -> [f32; 2] {
    // det-scan: allow(std_float_fn): `usize::max` on a vertex count, not a float order method.
    let n = poly.verts.len().max(1) as f32;
    let (mut x, mut z) = (0.0f32, 0.0f32);
    for v in &poly.verts {
        x += v[0];
        z += v[1];
    }
    [x / n, z / n]
}

// ---------------------------------------------------------------- wire

/// Magic of the serialized mesh (cook-time file, authority load).
pub const NAVMESH_MAGIC: [u8; 4] = *b"NVM2";

/// Registry name of the cooked mesh in `cargo xtask check-formats` and `regia upgrade-project`
/// (`docs/formats/nav-det-navmesh.md`).
pub const NAVMESH_FORMAT: &str = "nav-det-navmesh";

/// File suffix of a cooked mesh (`maps/arena.detnav`).
pub const NAVMESH_SUFFIX: &str = ".detnav";

/// The most spatial buckets, and the most bucket entries, a loaded mesh may index: 2^22 buckets of
/// 8 m cover a square about 16 km wide. [`NavMesh::from_bytes`] refuses a file past it instead of
/// allocating an index the size of its coordinates (a Regia addition, ADR-8174; [`NavMesh::bake`]
/// is unchanged).
pub const MAX_LOADED_BUCKETS: usize = 1 << 22;

/// The field layout of the cooked mesh, in write order, hashed by `cargo xtask check-formats`
/// (`nav-det-navmesh`). Little-endian; every `f32` is written as its canonical bits (`-0` is `+0`,
/// every NaN is `0x7fc0_0000`). A change here is a format change: bump [`NAVMESH_VERSION`].
pub const NAVMESH_LAYOUT: &[&str] = &[
    "magic:[u8;4]=NVM2",
    "version:u32",
    "bake_id:u64",
    "extent_m:f32",
    "arena:u8",
    "poly_count:u32",
    "poly.kind:u8=ground,tunnel,room,stair,door",
    "poly.level:i32",
    "poly.ceil:f32",
    "poly.width:f32",
    "poly.vert_count:u8",
    "poly.vert.x:f32",
    "poly.vert.z:f32",
    "poly.vert.floor:f32",
    "poly.link_count:u32",
    "poly.link.nbr:u32",
    "poly.link.a:[f32;2]",
    "poly.link.b:[f32;2]",
    "poly.link.width:f32",
    "poly.link.step:f32",
    "poly.link.clearance:f32",
];

/// The fingerprint `cargo xtask check-formats` hashes for the cooked mesh: [`NAVMESH_LAYOUT`].
#[must_use]
pub fn navmesh_schema_fingerprint() -> String {
    NAVMESH_LAYOUT.join(",")
}

/// The older versions the loader upgrades: none. A mesh of another version is refused; re-cook it.
#[must_use]
pub fn navmesh_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// True when the bucket index of `polys` stays within [`MAX_LOADED_BUCKETS`] buckets and entries.
/// The same bounds and bucket arithmetic as `NavMesh::build_index`.
fn index_fits(polys: &[NavPoly]) -> bool {
    if polys.is_empty() {
        return true;
    }
    let (mut min_x, mut min_z, mut max_x, mut max_z) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for poly in polys {
        for v in &poly.verts {
            min_x = det_min(min_x, v[0]);
            min_z = det_min(min_z, v[1]);
            max_x = det_max(max_x, v[0]);
            max_z = det_max(max_z, v[1]);
        }
    }
    // Finite vertices can still span past `f32::MAX` (the difference overflows to infinity).
    let buckets = |span: f32| -> Option<usize> {
        let b = (span / BUCKET_M).floor();
        // Checked before the cast: an infinite span must not reach `as usize + 1`.
        if b.is_finite() && b < MAX_LOADED_BUCKETS as f32 {
            Some(b as usize + 1)
        } else {
            None
        }
    };
    let (Some(bx), Some(bz)) = (buckets(max_x - min_x), buckets(max_z - min_z)) else {
        return false;
    };
    if bx.checked_mul(bz).is_none_or(|n| n > MAX_LOADED_BUCKETS) {
        return false;
    }
    let mut entries = 0usize;
    for poly in polys {
        let (mut x0, mut x1, mut z0, mut z1) = (usize::MAX, 0usize, usize::MAX, 0usize);
        for v in &poly.verts {
            let cx = (((v[0] - min_x) / BUCKET_M).floor() as usize).min(bx - 1);
            let cz = (((v[1] - min_z) / BUCKET_M).floor() as usize).min(bz - 1);
            x0 = x0.min(cx);
            x1 = x1.max(cx);
            z0 = z0.min(cz);
            z1 = z1.max(cz);
        }
        if x0 > x1 || z0 > z1 {
            continue;
        }
        entries = entries.saturating_add((x1 - x0 + 1) * (z1 - z0 + 1));
        if entries > MAX_LOADED_BUCKETS {
            return false;
        }
    }
    true
}

fn put_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&canonical_f32_bits(v).to_le_bytes());
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let bytes = self.bytes;
        if self.pos + n > bytes.len() {
            return None;
        }
        let s = &bytes[self.pos..self.pos + n];
        self.pos += n;
        Some(s)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn i32(&mut self) -> Option<i32> {
        Some(i32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }

    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }

    fn f32(&mut self) -> Option<f32> {
        Some(f32::from_bits(u32::from_le_bytes(self.take(4)?.try_into().ok()?)))
    }
}

impl NavMesh {
    /// Serialize for the cook-time file the authority loads. Little-endian,
    /// versioned; bit-exact across cooks of the same parts.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&NAVMESH_MAGIC);
        put_u32(&mut out, NAVMESH_VERSION);
        put_u64(&mut out, self.id);
        put_f32(&mut out, self.extent_m);
        put_u8(&mut out, u8::from(self.arena));
        put_u32(&mut out, self.polys.len() as u32);
        for (pi, poly) in self.polys.iter().enumerate() {
            put_u8(&mut out, poly.kind as u8);
            put_i32(&mut out, poly.level);
            put_f32(&mut out, poly.ceil);
            put_f32(&mut out, poly.width);
            put_u8(&mut out, poly.verts.len() as u8);
            for (v, y) in poly.verts.iter().zip(poly.floor.iter()) {
                put_f32(&mut out, v[0]);
                put_f32(&mut out, v[1]);
                put_f32(&mut out, *y);
            }
            put_u32(&mut out, self.adj[pi].len() as u32);
            for link in &self.adj[pi] {
                put_u32(&mut out, link.nbr);
                put_f32(&mut out, link.a[0]);
                put_f32(&mut out, link.a[1]);
                put_f32(&mut out, link.b[0]);
                put_f32(&mut out, link.b[1]);
                put_f32(&mut out, link.width);
                put_f32(&mut out, link.step);
                put_f32(&mut out, link.clearance);
            }
        }
        out
    }

    /// Load a cooked mesh. `None` on any truncation, bad magic or version,
    /// a non-finite float or an index past [`MAX_LOADED_BUCKETS`]: the
    /// authority bakes nothing itself, it refuses the file instead.
    pub fn from_bytes(bytes: &[u8]) -> Option<NavMesh> {
        let mut cur = Cursor { bytes, pos: 0 };
        if cur.take(4)? != NAVMESH_MAGIC.as_slice() {
            return None;
        }
        if cur.u32()? != NAVMESH_VERSION {
            return None;
        }
        let id = cur.u64()?;
        let extent_m = cur.f32()?;
        let arena = cur.u8()? != 0;
        let npoly = cur.u32()? as usize;
        if npoly == 0 || npoly > 1_000_000 {
            return None;
        }
        let mut mesh = NavMesh::default();
        for _ in 0..npoly {
            let kind = PolyKind::from_u8(cur.u8()?)?;
            let level = cur.i32()?;
            let ceil = cur.f32()?;
            let width = cur.f32()?;
            let nvert = cur.u8()? as usize;
            if !(3..=16).contains(&nvert) {
                return None;
            }
            let mut verts = Vec::with_capacity(nvert);
            let mut floor = Vec::with_capacity(nvert);
            for _ in 0..nvert {
                let x = cur.f32()?;
                let z = cur.f32()?;
                let y = cur.f32()?;
                if !x.is_finite() || !z.is_finite() || !y.is_finite() || !ceil.is_finite() {
                    return None;
                }
                verts.push([x, z]);
                floor.push(y);
            }
            if !width.is_finite() {
                return None;
            }
            mesh.push_poly(NavPoly {
                verts,
                floor,
                ceil,
                level,
                width,
                kind,
            });
            let nlinks = cur.u32()? as usize;
            if nlinks > npoly {
                return None;
            }
            let pi = mesh.polys.len() - 1;
            for _ in 0..nlinks {
                let nbr = cur.u32()?;
                if nbr as usize >= npoly {
                    return None;
                }
                let link = NavLink {
                    nbr,
                    a: [cur.f32()?, cur.f32()?],
                    b: [cur.f32()?, cur.f32()?],
                    width: cur.f32()?,
                    step: cur.f32()?,
                    clearance: cur.f32()?,
                };
                if !link.width.is_finite() || !link.step.is_finite() || !link.clearance.is_finite() {
                    return None;
                }
                mesh.adj[pi].push(link);
            }
        }
        if cur.pos != bytes.len() {
            return None;
        }
        if !index_fits(&mesh.polys) {
            return None;
        }
        mesh.finish(id, extent_m, arena);
        Some(mesh)
    }
}

// ---------------------------------------------------------------- tests

/// FNV-1a over the bake inputs: version, extent, arena flag, every part.
fn bake_hash(parts: &BakeParts) -> u64 {
    fn mix_u64(h: &mut u64, v: u64) {
        *h ^= v;
        *h = h.wrapping_mul(0x100_0000_01b3);
    }
    fn mix_f32(h: &mut u64, v: f32) {
        mix_u64(h, u64::from(canonical_f32_bits(v)));
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    mix_u64(&mut h, u64::from(NAVMESH_VERSION));
    mix_f32(&mut h, parts.extent_m);
    mix_u64(&mut h, u64::from(parts.arena));
    if let Some(g) = &parts.ground {
        mix_u64(&mut h, g.cols as u64);
        mix_u64(&mut h, g.rows as u64);
        mix_f32(&mut h, g.step_m);
        mix_f32(&mut h, g.origin_x);
        mix_f32(&mut h, g.origin_z);
        for (w, y) in g.walk.iter().zip(g.height.iter()) {
            mix_u64(&mut h, u64::from(*w) ^ (u64::from(canonical_f32_bits(*y)) << 1));
        }
    }
    for c in &parts.corridors {
        mix_f32(&mut h, c.half_width_m);
        mix_f32(&mut h, c.ceiling_m);
        for p in &c.points {
            mix_f32(&mut h, p[0]);
            mix_f32(&mut h, p[1]);
            mix_f32(&mut h, p[2]);
        }
    }
    for r in &parts.rooms {
        mix_f32(&mut h, r.min_x);
        mix_f32(&mut h, r.min_z);
        mix_f32(&mut h, r.max_x);
        mix_f32(&mut h, r.max_z);
        mix_f32(&mut h, r.floor_y);
        mix_f32(&mut h, r.ceil_y);
        mix_u64(&mut h, r.level as u64);
    }
    for s in &parts.stairs {
        mix_f32(&mut h, s.from[0]);
        mix_f32(&mut h, s.from[1]);
        mix_f32(&mut h, s.to[0]);
        mix_f32(&mut h, s.to[1]);
        mix_f32(&mut h, s.width_m);
        mix_f32(&mut h, s.y0);
        mix_f32(&mut h, s.y1);
        mix_f32(&mut h, s.ceil_m);
    }
    for d in &parts.doors {
        mix_f32(&mut h, d.center[0]);
        mix_f32(&mut h, d.center[1]);
        mix_f32(&mut h, d.dir[0]);
        mix_f32(&mut h, d.dir[1]);
        mix_f32(&mut h, d.width_m);
        mix_f32(&mut h, d.depth_m);
        mix_f32(&mut h, d.floor_y);
        mix_f32(&mut h, d.ceil_y);
        mix_u64(&mut h, d.level as u64);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pilot() -> MeshAgent {
        MeshAgent::pilot()
    }

    /// Every waypoint stands on the mesh: the route is walked, not drawn.
    fn assert_plan_valid(mesh: &NavMesh, plan: &MeshPlan, agent: &MeshAgent) {
        assert!(plan.waypoints.len() >= 2, "a plan has legs");
        assert_eq!(plan.waypoints.len(), plan.polys.len());
        for w in &plan.waypoints {
            assert!(
                mesh.locate(*w, agent).is_some(),
                "waypoint {w:?} stands on the mesh"
            );
        }
        assert!(plan.expansions <= NAVMESH_MAX_EXPANSIONS);
    }

    /// Flat raster with an optional blocked column and a gap of open rows.
    fn wall_raster(cols: usize, rows: usize, wall_x: usize, gap: (usize, usize)) -> GroundRaster {
        let mut walk = vec![true; cols * rows];
        for z in 0..rows {
            if z < gap.0 || z > gap.1 {
                walk[z * cols + wall_x] = false;
            }
        }
        GroundRaster {
            cols,
            rows,
            step_m: 5.0,
            origin_x: 0.0,
            origin_z: 0.0,
            walk,
            height: vec![0.0; cols * rows],
        }
    }

    #[test]
    fn ground_bake_merges_runs_and_routes_through_the_gap() {
        let parts = BakeParts {
            ground: Some(wall_raster(8, 6, 4, (2, 3))),
            extent_m: 40.0,
            ..BakeParts::default()
        };
        let mesh = NavMesh::bake(&parts).expect("bake");
        // Two spans left and right of the wall, plus the full rows of the gap.
        assert_eq!(mesh.poly_count(), 5, "runs merge into 5 rectangles");
        assert!(mesh.link_count() > 0, "rectangles link edge to edge");
        let agent = pilot();
        let plan = mesh
            .find_path([5.0, 0.0, 27.0], [35.0, 0.0, 27.0], &agent)
            .expect("a route through the gap");
        assert_plan_valid(&mesh, &plan, &agent);
        assert!(
            plan.length > 31.0,
            "straight is 30 m; the gap detour is longer ({})",
            plan.length
        );
    }

    #[test]
    fn a_full_wall_is_no_route_not_a_hang() {
        let mut raster = wall_raster(8, 6, 4, (2, 3));
        for z in 0..6 {
            raster.walk[z * 8 + 4] = false;
        }
        let parts = BakeParts {
            ground: Some(raster),
            extent_m: 40.0,
            ..BakeParts::default()
        };
        let mesh = NavMesh::bake(&parts).expect("bake");
        assert!(
            mesh.find_path([5.0, 0.0, 27.0], [35.0, 0.0, 27.0], &pilot())
                .is_none(),
            "no gap, no route"
        );
    }

    /// A two-room cafe with a personnel door and a stair to an upper room.
    fn cafe_parts() -> BakeParts {
        BakeParts {
            ground: None,
            corridors: Vec::new(),
            rooms: vec![
                Room {
                    min_x: -20.0,
                    min_z: -10.0,
                    max_x: -1.0,
                    max_z: 10.0,
                    floor_y: 0.0,
                    ceil_y: 3.2,
                    level: 0,
                },
                Room {
                    min_x: 1.0,
                    min_z: -10.0,
                    max_x: 20.0,
                    max_z: 10.0,
                    floor_y: 0.0,
                    ceil_y: 3.2,
                    level: 0,
                },
                Room {
                    min_x: 1.0,
                    min_z: -10.0,
                    max_x: 20.0,
                    max_z: 10.0,
                    floor_y: 3.0,
                    ceil_y: 6.2,
                    level: 1,
                },
            ],
            doors: vec![DoorQuad {
                center: [0.0, 0.0],
                dir: [1.0, 0.0],
                width_m: 1.10,
                depth_m: 2.0,
                floor_y: 0.0,
                ceil_y: 2.2,
                level: 0,
            }],
            stairs: vec![StairRun {
                from: [4.0, -8.0],
                to: [4.0, 8.0],
                width_m: 2.0,
                y0: 0.0,
                y1: 3.0,
                ceil_m: 3.0,
            }],
            extent_m: 40.0,
            arena: true,
        }
    }

    #[test]
    fn a_pilot_walks_every_room_stair_and_door() {
        let mesh = NavMesh::bake(&cafe_parts()).expect("bake");
        assert!(mesh.is_arena());
        let agent = pilot();
        // Every room locates at its own floor, including the stacked one.
        assert!(mesh.locate([-10.0, 0.0, 0.0], &agent).is_some());
        assert!(mesh.locate([10.0, 0.0, 0.0], &agent).is_some());
        assert!(mesh.locate([10.0, 3.0, 0.0], &agent).is_some());
        eprintln!(
            "DIAG mesh polys {} links {} bake {:?}",
            mesh.poly_count(),
            mesh.link_count(),
            mesh.bake_id()
        );
        for (pi, p) in mesh.polys.iter().enumerate() {
            eprintln!(
                "DIAG poly {pi} kind {:?} level {} floor {:?} verts {:?}",
                p.kind, p.level, p.floor, p.verts
            );
            for l in &mesh.adj[pi] {
                eprintln!(
                    "DIAG   -> {} portal {:?}-{:?} step {:.2} width {:.2}",
                    l.nbr, l.a, l.b, l.step, l.width
                );
            }
        }
        eprintln!(
            "DIAG door leg A->B: {:?}",
            mesh.find_path([-10.0, 0.0, 0.0], [10.0, 0.0, 0.0], &agent)
                .map(|p| p.waypoints.len())
        );
        eprintln!(
            "DIAG stair leg B->C: {:?}",
            mesh.find_path([10.0, 0.0, 0.0], [10.0, 3.0, 0.0], &agent)
                .map(|p| p.waypoints.len())
        );
        // Lower room to upper room: through the door, up the stair.
        let plan = mesh
            .find_path([-10.0, 0.0, 0.0], [10.0, 3.0, 0.0], &agent)
            .expect("a pilot walks door and stair");
        assert_plan_valid(&mesh, &plan, &agent);
        let top = plan.waypoints.iter().map(|w| w[1]).fold(0.0f32, det_max);
        assert!(top >= 2.5, "the route climbs the stair (top {top})");
        assert!(
            !mesh.straight_walkable([-10.0, 0.0, 0.0], [10.0, 3.0, 0.0], &agent),
            "the direct line is not walked: walls are walls"
        );
    }

    #[test]
    fn a_hull_disc_does_not_fit_the_personnel_door() {
        let mesh = NavMesh::bake(&cafe_parts()).expect("bake");
        let hull = MeshAgent {
            radius: 2.5,
            height: 1.8,
            step_up: 0.55,
            step_down: 0.55,
        };
        assert!(
            mesh.find_path([-10.0, 0.0, 0.0], [10.0, 0.0, 0.0], &hull)
                .is_none(),
            "a 2.5 m disc cannot use a 1.10 m door"
        );
        // The pilot still can, on the same bake.
        assert!(
            mesh.find_path([-10.0, 0.0, 0.0], [10.0, 0.0, 0.0], &pilot())
                .is_some(),
            "the same door passes a 0.35 m capsule"
        );
    }

    /// A lava tube crossed by a gallery, with a ramp up to the surface.
    fn tunnel_parts() -> BakeParts {
        let mut tube = Vec::new();
        let mut x = -80.0;
        while x <= 80.0 {
            tube.push([x, -40.0, 0.0]);
            x += 8.0;
        }
        let mut gallery = Vec::new();
        let mut z = -64.0;
        while z <= 64.0 {
            gallery.push([0.0, -40.0, z]);
            z += 8.0;
        }
        let walk = vec![true; 48 * 8];
        BakeParts {
            ground: Some(GroundRaster {
                cols: 48,
                rows: 8,
                step_m: 5.0,
                origin_x: -120.0,
                origin_z: -20.0,
                walk,
                height: vec![0.0; 48 * 8],
            }),
            corridors: vec![
                Corridor {
                    points: tube,
                    half_width_m: 11.0,
                    ceiling_m: 12.0,
                },
                Corridor {
                    points: gallery,
                    half_width_m: 9.0,
                    ceiling_m: 10.0,
                },
                Corridor {
                    points: vec![[-80.0, -40.0, 0.0], [-95.0, -20.0, 0.0], [-110.0, 0.0, 0.0]],
                    half_width_m: 10.0,
                    ceiling_m: 12.0,
                },
            ],
            extent_m: 240.0,
            ..BakeParts::default()
        }
    }

    #[test]
    fn tunnel_corridors_move_onto_the_mesh() {
        let mesh = NavMesh::bake(&tunnel_parts()).expect("bake");
        let agent = pilot();
        // Down the tube, end to end.
        let along = mesh
            .find_path([75.0, -40.0, 0.0], [-75.0, -40.0, 0.0], &agent)
            .expect("tube end to end");
        assert_plan_valid(&mesh, &along, &agent);
        // Gallery south end to tube west end: the only connection is the
        // junction, so a valid route proves the crossing link works.
        // Straight is 96 m; the junction route is longer.
        let across = mesh
            .find_path([0.0, -40.0, -60.0], [-75.0, -40.0, 0.0], &agent)
            .expect("gallery joins the tube");
        assert_plan_valid(&mesh, &across, &agent);
        assert!(
            across.length > 100.0,
            "the junction route is no straight line ({})",
            across.length
        );
        // Up the ramp onto the surface.
        let out = mesh
            .find_path([75.0, -40.0, 0.0], [-110.0, 0.0, 0.0], &agent)
            .expect("ramp surfaces");
        assert_plan_valid(&mesh, &out, &agent);
        let top = out.waypoints.iter().map(|w| w[1]).fold(f32::MIN, det_max);
        assert!(top >= -0.5, "the route climbs out (top {top})");
    }

    #[test]
    fn wire_round_trips_and_rejects_junk() {
        let mesh = NavMesh::bake(&cafe_parts()).expect("bake");
        let bytes = mesh.to_bytes();
        let back = NavMesh::from_bytes(&bytes).expect("load");
        assert_eq!(back.bake_id(), mesh.bake_id());
        assert_eq!(back.poly_count(), mesh.poly_count());
        assert_eq!(back.link_count(), mesh.link_count());
        assert_eq!(back.to_bytes(), bytes, "bytes are stable across the round trip");
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(NavMesh::from_bytes(&bad).is_none(), "bad magic is refused");
        assert!(
            NavMesh::from_bytes(&bytes[..bytes.len() / 2]).is_none(),
            "truncation is refused"
        );
        let mut versioned = bytes.clone();
        versioned[4] = versioned[4].wrapping_add(1);
        assert!(
            NavMesh::from_bytes(&versioned).is_none(),
            "a new version is refused"
        );
        let mut trailing = bytes.clone();
        trailing.push(0);
        assert!(
            NavMesh::from_bytes(&trailing).is_none(),
            "trailing bytes are refused"
        );
    }

    #[test]
    fn bake_is_deterministic() {
        let a = NavMesh::bake(&tunnel_parts()).expect("bake").to_bytes();
        let b = NavMesh::bake(&tunnel_parts()).expect("bake").to_bytes();
        assert_eq!(a, b, "two cooks agree byte for byte");
    }

    #[test]
    fn router_keeps_hover_hulls_on_the_grid_without_tunnels() {
        assert_eq!(route_for(true, true, false), RouteKind::Grid);
        assert_eq!(route_for(true, false, false), RouteKind::Grid);
        assert_eq!(route_for(false, false, false), RouteKind::Grid);
        assert_eq!(route_for(false, true, false), RouteKind::Mesh);
        // A tunnel-carrying mesh routes hover hulls too; jet flight never does.
        assert_eq!(route_for(true, true, true), RouteKind::Mesh);
        assert_eq!(route_for(false, true, true), RouteKind::Mesh);
    }

    #[test]
    fn bake_rejects_empty_and_broken_parts() {
        assert!(
            NavMesh::bake(&BakeParts::default()).is_err(),
            "no polygons is an error"
        );
        let mut bad = cafe_parts();
        bad.doors[0].width_m = 0.1;
        assert!(NavMesh::bake(&bad).is_err(), "a slit door is an error");
        let mut bad = BakeParts::default();
        bad.corridors.push(Corridor {
            points: vec![[0.0, 0.0, 0.0]],
            half_width_m: 5.0,
            ceiling_m: 8.0,
        });
        assert!(NavMesh::bake(&bad).is_err(), "a one-point corridor is an error");
    }

    // ------------------------------------------------------------ Regia tests (ADR-8174)

    use regia_detsim::golden::{self, DetRun, DetScenario, DetStatus, GoldenMode, StateHasher, TickRate};

    /// A 256 x 256 ground raster: a grid of 4 x 4 blocked squares, a diagonal wall with a gap, and
    /// heights that vary per cell (every corner floor differs).
    fn ground_256() -> GroundRaster {
        let n = 256usize;
        let mut walk = vec![true; n * n];
        let mut height = vec![0.0f32; n * n];
        for z in 0..n {
            for x in 0..n {
                let i = z * n + x;
                let in_block = x % 32 >= 14 && x % 32 < 18 && z % 32 >= 14 && z % 32 < 18;
                // Two cells thick, so diagonal neighbors never touch at a corner.
                let on_wall = (x + 40 == z || x + 39 == z) && !(120..136).contains(&x);
                walk[i] = !(in_block || on_wall);
                height[i] = ((x * 7 + z * 13) % 17) as f32 * 0.03125;
            }
        }
        GroundRaster {
            cols: n,
            rows: n,
            step_m: 2.0,
            origin_x: -255.0,
            origin_z: -255.0,
            walk,
            height,
        }
    }

    fn ground_256_parts() -> BakeParts {
        BakeParts {
            ground: Some(ground_256()),
            extent_m: 512.0,
            ..BakeParts::default()
        }
    }

    /// The three bake fixtures of the acceptance list, by name.
    fn fixtures() -> [(&'static str, BakeParts); 3] {
        [
            ("tunnel_parts", tunnel_parts()),
            ("cafe_parts", cafe_parts()),
            ("ground_256", ground_256_parts()),
        ]
    }

    /// FNV-1a 64 over bytes.
    fn fnv64(bytes: &[u8]) -> u64 {
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for b in bytes {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100_0000_01b3);
        }
        h
    }

    #[test]
    fn bake_bytes_identical_twice() {
        for (name, parts) in fixtures() {
            let a = NavMesh::bake(&parts).expect("bake");
            let b = NavMesh::bake(&parts).expect("bake");
            assert_eq!(a.bake_id(), b.bake_id(), "{name}: bake id");
            let bytes = a.to_bytes();
            assert_eq!(bytes, b.to_bytes(), "{name}: two bakes agree byte for byte");
            let back = NavMesh::from_bytes(&bytes).expect("the cooked bytes load");
            assert_eq!(
                back.to_bytes(),
                bytes,
                "{name}: the loaded mesh writes the same bytes"
            );
            assert_eq!(
                back.bake_id(),
                a.bake_id(),
                "{name}: the loaded mesh keeps the bake id"
            );
        }
        let ground = NavMesh::bake(&ground_256_parts()).expect("bake");
        assert!(
            ground.poly_count() > 100,
            "the 256 raster merges into many rectangles"
        );
        assert!(ground.link_count() > ground.poly_count(), "and links them");
    }

    #[test]
    fn the_256_raster_routes_around_blocks_and_through_the_wall_gap() {
        let mesh = NavMesh::bake(&ground_256_parts()).expect("bake");
        let agent = pilot();
        // From below the diagonal wall to above it: the only way through is the gap.
        let plan = mesh
            .find_path([150.0, 0.25, -200.0], [-200.0, 0.25, 150.0], &agent)
            .expect("a route through the gap");
        assert!(plan.waypoints.len() >= 3, "the route bends at the gap");
        assert!(plan.expansions <= NAVMESH_MAX_EXPANSIONS);
    }

    /// DO27's canonical float rule (ir-content/src/math.rs lines 145-152 at regia-absorb-2), kept
    /// here as the reference the lockstep kit's function must equal bit for bit.
    fn do27_canonical_f32_bits(value: f32) -> u32 {
        let bits = value.to_bits();
        match bits & 0x7fff_ffff {
            0 => 0,
            magnitude if magnitude > 0x7f80_0000 => 0x7fc0_0000,
            _ => bits,
        }
    }

    #[test]
    fn canonical_float_bytes_match_do27_rule() {
        // The function: every edge pattern, then a sweep over the whole 32-bit space.
        let edges = [
            0u32,
            0x8000_0000,
            0x0000_0001,
            0x8000_0001,
            0x007f_ffff,
            0x0080_0000,
            0x3f80_0000,
            0x7f7f_ffff,
            0x7f80_0000,
            0xff80_0000,
            0x7f80_0001,
            0x7fc0_0000,
            0x7fff_ffff,
            0xffc0_0000,
            0xffa0_0042,
            0xffff_ffff,
        ];
        let sweep = (0..=u32::MAX / 65_521).map(|k| k * 65_521);
        for bits in edges.into_iter().chain(sweep) {
            let v = f32::from_bits(bits);
            assert_eq!(
                canonical_f32_bits(v),
                do27_canonical_f32_bits(v),
                "bits {bits:#010x}"
            );
        }
        // The bake: a raster with a -0.0 and a NaN height (a payload, negative sign) bakes to the
        // same bytes and bake id as the same raster with +0.0 and the canonical NaN.
        let raster = |zero: f32, nan: f32| {
            let mut g = wall_raster(8, 6, 4, (2, 3));
            g.height[0] = zero; // the first rectangle's corner
            g.height[7] = nan; // the right rectangle's corner
            g.height[3 * 8 + 5] = zero;
            BakeParts {
                ground: Some(g),
                extent_m: 40.0,
                ..BakeParts::default()
            }
        };
        let odd = NavMesh::bake(&raster(-0.0, f32::from_bits(0xffa0_0042))).expect("bake");
        let canon = NavMesh::bake(&raster(0.0, f32::from_bits(0x7fc0_0000))).expect("bake");
        assert_eq!(
            odd.bake_id(),
            canon.bake_id(),
            "the bake hash folds the zero sign and the NaN"
        );
        let bytes = odd.to_bytes();
        assert_eq!(
            bytes,
            canon.to_bytes(),
            "the wire folds the zero sign and the NaN"
        );
        // The NaN corner reached the wire, as the canonical NaN; no -0 is written anywhere.
        let words: Vec<u32> = bytes[4..]
            .windows(4)
            .map(|w| u32::from_le_bytes([w[0], w[1], w[2], w[3]]))
            .collect();
        assert!(
            words.contains(&0x7fc0_0000),
            "the NaN floor is written canonically"
        );
        assert!(
            !bytes.windows(4).any(|w| w == 0xffa0_0042u32.to_le_bytes()),
            "no payload NaN"
        );
    }

    #[test]
    fn from_bytes_refuses_a_newer_version() {
        let bytes = NavMesh::bake(&cafe_parts()).expect("bake").to_bytes();
        assert_eq!(&bytes[..4], &NAVMESH_MAGIC);
        assert_eq!(
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            NAVMESH_VERSION
        );
        for version in [NAVMESH_VERSION + 1, NAVMESH_VERSION + 100, u32::MAX, 1, 0] {
            let mut other = bytes.clone();
            other[4..8].copy_from_slice(&version.to_le_bytes());
            assert!(
                NavMesh::from_bytes(&other).is_none(),
                "version {version} is refused"
            );
        }
        assert!(NavMesh::from_bytes(&bytes).is_some(), "the current version loads");
    }

    /// The spec 12 § 4a name of the same check (the format row's test).
    #[test]
    fn newer_version_is_refused() {
        let mut bytes = NavMesh::bake(&tunnel_parts()).expect("bake").to_bytes();
        bytes[4..8].copy_from_slice(&(NAVMESH_VERSION + 1).to_le_bytes());
        assert!(NavMesh::from_bytes(&bytes).is_none());
        assert!(
            navmesh_migration_sources().is_empty(),
            "no older version is read either"
        );
    }

    #[test]
    fn navmesh_layout_matches_the_writer() {
        // Byte sizes of the layout's field types, then the size of every fixture's bytes.
        let size = |field: &str| -> usize {
            let ty = field
                .split(':')
                .nth(1)
                .expect("typed")
                .split('=')
                .next()
                .expect("type");
            match ty {
                "[u8;4]" => 4,
                "u8" => 1,
                "u32" | "i32" | "f32" => 4,
                "u64" => 8,
                "[f32;2]" => 8,
                other => panic!("unknown field type {other}"),
            }
        };
        let group = |prefix: &str| -> usize {
            NAVMESH_LAYOUT
                .iter()
                .filter(|f| {
                    let name = f.split(':').next().expect("named");
                    name.strip_prefix(prefix).is_some_and(|rest| !rest.contains('.'))
                })
                .copied()
                .map(size)
                .sum()
        };
        let header: usize = NAVMESH_LAYOUT
            .iter()
            .take_while(|f| !f.starts_with("poly."))
            .copied()
            .map(size)
            .sum();
        assert_eq!(header, 4 + 4 + 8 + 4 + 1 + 4);
        assert!(NAVMESH_LAYOUT[0].ends_with(std::str::from_utf8(&NAVMESH_MAGIC).expect("ascii")));
        for (name, parts) in fixtures() {
            let mesh = NavMesh::bake(&parts).expect("bake");
            let verts: usize = mesh.polys.iter().map(|p| p.verts.len()).sum();
            let expected = header
                + mesh.poly_count() * group("poly.")
                + verts * group("poly.vert.")
                + mesh.link_count() * group("poly.link.");
            assert_eq!(mesh.to_bytes().len(), expected, "{name}");
        }
        assert!(navmesh_schema_fingerprint().contains("poly.link.clearance"));
    }

    #[test]
    fn from_bytes_refuses_an_index_past_the_bucket_cap() {
        let mesh = NavMesh::bake(&cafe_parts()).expect("bake");
        let bytes = mesh.to_bytes();
        // The first vertex's x sits after the header and the first polygon's fixed fields.
        let at = 25 + 1 + 4 + 4 + 4 + 1;
        assert_eq!(
            f32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]),
            mesh.polys[0].verts[0][0]
        );
        for x in [3.0e38f32, -3.0e38, 1.0e9, -2.0e7] {
            let mut far = bytes.clone();
            far[at..at + 4].copy_from_slice(&x.to_le_bytes());
            assert!(
                NavMesh::from_bytes(&far).is_none(),
                "a vertex at x = {x} is refused"
            );
        }
        // A vertex moved a few kilometers still loads (the cap is a ~16 km square).
        let mut near = bytes.clone();
        near[at..at + 4].copy_from_slice(&(-3000.0f32).to_le_bytes());
        assert!(NavMesh::from_bytes(&near).is_some(), "a 3 km index loads");
    }

    #[test]
    fn from_bytes_never_panics_on_mutated_bytes() {
        // Seeded byte flips, truncations and splices of every fixture (the fuzz target in
        // `regia_agent` runs the same loader through the shared harness).
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        for (_, parts) in fixtures() {
            let bytes = NavMesh::bake(&parts).expect("bake").to_bytes();
            for _ in 0..300 {
                let mut m = bytes.clone();
                match next() % 4 {
                    0 => {
                        let i = (next() as usize) % m.len();
                        m[i] ^= 1 << (next() % 8);
                    }
                    1 => m.truncate((next() as usize) % m.len()),
                    2 => {
                        let i = (next() as usize) % m.len();
                        let word = (next() as u32).to_le_bytes();
                        for (k, b) in word.iter().enumerate() {
                            if let Some(slot) = m.get_mut(i + k) {
                                *slot = *b;
                            }
                        }
                    }
                    _ => {
                        let i = (next() as usize) % m.len();
                        m.insert(i, next() as u8);
                    }
                }
                if let Some(mesh) = NavMesh::from_bytes(&m) {
                    // A mesh that loads answers queries without panicking.
                    let _ = mesh.find_path([0.0, 0.0, 0.0], [10.0, 0.0, 10.0], &pilot());
                    let _ = mesh.to_bytes();
                }
            }
        }
    }

    /// The acceptance scenario: tick 0-2 bake the three fixtures (hash: the bake id and the cooked
    /// bytes), ticks 3-66 run 64 seeded `find_path` queries over them (hash: every waypoint, polygon,
    /// expansion count and length). One golden for every architecture (ADR-8012 A-4).
    struct NavmeshBakeScenario {
        meshes: Vec<NavMesh>,
        bounds: Vec<([f32; 3], [f32; 3])>,
        rng: u64,
        bake: Option<(u64, u64, u32)>,
        plan: Option<Option<MeshPlan>>,
    }

    impl NavmeshBakeScenario {
        const BAKES: u32 = 3;
        const PLANS: u32 = 64;

        fn new() -> Self {
            NavmeshBakeScenario {
                meshes: Vec::new(),
                bounds: vec![
                    ([-80.0, -40.0, -64.0], [80.0, -40.0, 64.0]),
                    ([-20.0, 0.0, -10.0], [20.0, 3.0, 10.0]),
                    ([-250.0, 0.25, -250.0], [250.0, 0.25, 250.0]),
                ],
                rng: 0,
                bake: None,
                plan: None,
            }
        }

        fn next(&mut self) -> u64 {
            // splitmix64
            self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = self.rng;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        }

        /// A point in `[lo, hi]` on a 0.25 m lattice (exact in f32).
        fn point(&mut self, lo: [f32; 3], hi: [f32; 3]) -> [f32; 3] {
            let mut p = [0.0f32; 3];
            for (k, slot) in p.iter_mut().enumerate() {
                let steps = ((hi[k] - lo[k]) * 4.0) as u64;
                *slot = lo[k] + (self.next() % (steps + 1)) as f32 * 0.25;
            }
            p
        }
    }

    impl DetScenario for NavmeshBakeScenario {
        fn name(&self) -> &str {
            "navmesh_bake"
        }

        fn default_run(&self) -> DetRun {
            DetRun::new(0x5eed_0a11, Self::BAKES + Self::PLANS, TickRate::default())
        }

        fn reset(&mut self, run: &DetRun) {
            self.meshes.clear();
            self.rng = run.seed;
            self.bake = None;
            self.plan = None;
        }

        fn step(&mut self, tick: u32) {
            if tick < Self::BAKES {
                let (_, parts) = fixtures().into_iter().nth(tick as usize).expect("three fixtures");
                let mesh = NavMesh::bake(&parts).expect("bake");
                let bytes = mesh.to_bytes();
                self.bake = Some((mesh.bake_id(), fnv64(&bytes), bytes.len() as u32));
                self.meshes.push(mesh);
                self.plan = None;
                return;
            }
            let which = (self.next() % u64::from(Self::BAKES)) as usize;
            let (lo, hi) = self.bounds[which];
            let from = self.point(lo, hi);
            let goal = self.point(lo, hi);
            let scaled = self.next().is_multiple_of(2);
            let agent = if self.next().is_multiple_of(4) {
                MeshAgent::hover(3.0)
            } else {
                MeshAgent::pilot()
            };
            let mesh = &self.meshes[which];
            self.plan = Some(if scaled {
                mesh.find_path_scaled(from, goal, &agent, 40)
            } else {
                mesh.find_path(from, goal, &agent)
            });
            self.bake = None;
        }

        fn hash_state(&self, h: &mut StateHasher) {
            if let Some((id, bytes_hash, len)) = self.bake {
                h.u64("bake_id", id);
                h.u64("bytes_fnv", bytes_hash);
                h.u32("bytes_len", len);
            }
            if let Some(plan) = &self.plan {
                h.bool("found", plan.is_some());
                if let Some(plan) = plan {
                    h.u32("expansions", plan.expansions);
                    h.f32("length", plan.length);
                    for (i, (w, poly)) in plan.waypoints.iter().zip(&plan.polys).enumerate() {
                        h.item("waypoint", i as u32);
                        h.f32("x", w[0]);
                        h.f32("y", w[1]);
                        h.f32("z", w[2]);
                        h.u32("poly", *poly);
                    }
                }
            }
        }
    }

    #[test]
    fn navmesh_scenario_finds_routes_and_misses_some() {
        let mut scenario = NavmeshBakeScenario::new();
        let run = scenario.default_run();
        scenario.reset(&run);
        let (mut found, mut missed) = (0, 0);
        for tick in 0..run.ticks {
            scenario.step(tick);
            match &scenario.plan {
                Some(Some(_)) => found += 1,
                Some(None) => missed += 1,
                None => {}
            }
        }
        assert_eq!(found + missed, NavmeshBakeScenario::PLANS);
        assert!(found >= 32, "most seeded queries route ({found} of 64)");
        assert!(
            missed >= 1,
            "some do not (hull through a door, off-mesh endpoints): {missed}"
        );
    }

    /// Prints one `regia-det-golden {json}` line, and reads the golden from
    /// `$REGIA_DET_GOLDEN_DIR` when set, so the det-golden legs (a device, the Deck, Windows)
    /// can run this binary away from the source tree (ADR-8838; before, the line had no prefix
    /// and the directory was the build tree's only).
    #[test]
    #[expect(
        clippy::disallowed_methods,
        reason = "test harness: where the pinned golden lives, as regia_detsim::golden::engine_golden_dir; no sim tick reads the environment (ADR-8367, ADR-8877)"
    )]
    fn navmesh_bake_id_pinned_on_every_arch() {
        // det-scan: allow(env_read): test harness: where the pinned golden lives, not sim input
        let dir = match std::env::var_os(golden::GOLDEN_DIR_ENV) {
            Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
            _ => std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens"),
        };
        let mut scenario = NavmeshBakeScenario::new();
        let report = golden::check_golden(&mut scenario, &dir, GoldenMode::from_env());
        println!("{}{}", golden::REPORT_PREFIX, report.to_json());
        assert!(
            matches!(
                report.status,
                DetStatus::Ok | DetStatus::UnrecordedTarget | DetStatus::Blessed
            ),
            "{report}"
        );
    }
}
