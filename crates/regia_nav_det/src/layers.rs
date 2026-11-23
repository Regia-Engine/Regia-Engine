// SPDX-License-Identifier: MPL-2.0
// Portions adapted from Direct Order 27 (ir-sim/src/pathing/navmesh.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1176-1355, mark: adapt (Stage B, ADR-8486 / A-10: a new runtime with its own pin).
// Edits: the polygon A* of `find_path` / `find_path_scaled` (the `(f, h, index)` heap order, ascending neighbor
// order, the tunnel cost share applied to the step out of a tunnel polygon and to the heuristic while it is under
// 100) carried over to integer millimeters and integer costs on the layered bake; the greedy pull replaced by a
// funnel over the portals; the lane rule, the closed set, the dirty tiles, the flank share and the layer index are
// Regia's. The file is byte-identical between regia-absorb-2 (e6c84ba6d), regia-absorb-3 and regia-absorb-4.

//! The deterministic runtime path query over the layered bake (plan row W-20, ADR-8486).
//!
//! [`LayeredBake::bake`] turns a layered polygon mesh (one polygon set per walkable layer, from
//! `regia_nav`'s layered builder: tunnels under the ground, floors of a building) into integer
//! millimeters: vertices, heights, portals and links, a layer index per polygon (0 is the top
//! surface of its column), the owning world-geometry section of each polygon and a tile grid. The
//! polygons are sorted into a canonical order, so the bake's bytes ([`LayeredBake::to_bytes`]) and
//! its [`LayeredBake::content_hash`] depend only on the polygons, not on the builder's order. No
//! `f32` survives the bake.
//!
//! [`LayeredBake::plan`] is the runtime query: A* over the polygons with the `(f, h, index)` tie
//! rule, a cost share for tunnel and flank polygons, an expansion budget, then a funnel over the
//! portals. Every value is an integer (`i64` cross products, `u64::isqrt` distances), so a plan is
//! bit-identical on every peer and every architecture.
//!
//! Inside a passage (tunnel or flank polygons) the plan keeps to its lane: it follows the right-hand
//! wall, seen along the direction of travel with +x east and +z north, so two hulls that meet
//! head-on take opposite walls (see `LayeredBake::lanes` and [`LayerAgent::passing_gap_mm`]).
//!
//! [`LayerState`] is the per-match state the query reads: the closed polygons (the sections a
//! world-geometry blocker closes) and the dirty tiles (the tiles a ground edit touched, waiting for
//! a re-bake). [`LayerState::sync`] applies one generation's changes in a fixed order: the closed
//! set first (replaced whole), then the dirty rectangles, then the generation number.

use std::cmp::Reverse;
use std::collections::BinaryHeap;

/// The version of this module's answer bits. A lockstep handshake compares it beside the bake's
/// content hash; every change that moves a plan for the same bake bumps it.
///
/// - `1`: the first layered runtime (ADR-8486).
pub const LAYERED_NAV_BITS_VERSION: u32 = 1;

/// The bake's byte layout version ([`LayeredBake::to_bytes`]).
pub const LAYERED_BAKE_VERSION: u32 = 1;

/// The first bytes of [`LayeredBake::to_bytes`].
pub const LAYERED_BAKE_MAGIC: [u8; 4] = *b"RLNB";

/// The area id of a tunnel polygon (the layered builder's tunnel area).
pub const LAYER_AREA_TUNNEL: u8 = 60;

/// The area id of a flank polygon (the layered builder's flank area).
pub const LAYER_AREA_FLANK: u8 = 61;

/// The section of a polygon outside every world-geometry section (open ground).
pub const NO_SECTION: u16 = u16::MAX;

/// The default nav tile side, millimeters (32 m).
pub const DEFAULT_TILE_MM: i32 = 32_000;

/// The default expansion budget of one plan. A 0.5 m voxel bake of a 1.5 km tunnel system has
/// about 18 000 polygons, and a plan from one end to the other expands most of them, so the
/// budget is a few times that.
pub const LAYERED_MAX_EXPANSIONS: u32 = 65_536;

/// Two polygons whose heights differ by more than this are on different layers of one column.
pub const LAYER_GAP_MM: i32 = 1_500;

/// The side of the bake's lookup buckets, millimeters.
const BUCKET_MM: i64 = 8_000;

/// One polygon of a layered mesh, as the layered builder outputs it (meters).
#[derive(Clone, Debug, PartialEq)]
pub struct LayerPolyInput {
    /// The boundary loop, `[x, y, z]` meters. The polygon is horizontal: `y` of the first vertex is
    /// its height.
    pub vertices: Vec<[f32; 3]>,
    /// The area id ([`LAYER_AREA_TUNNEL`], [`LAYER_AREA_FLANK`], anything else is open ground).
    pub area: u8,
    /// The portals to the neighboring polygons.
    pub portals: Vec<LayerPortalInput>,
    /// The world-geometry section that owns the polygon, `None` outdoors.
    pub section: Option<u16>,
}

/// One portal of a [`LayerPolyInput`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerPortalInput {
    /// The neighbor's index in the input list.
    pub neighbor: u32,
    /// One end of the shared edge, meters.
    pub a: [f32; 3],
    /// The other end, meters.
    pub b: [f32; 3],
}

/// An off-mesh link (a ladder, deck stairs, a jump) of the layered mesh, meters.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LayerLinkInput {
    /// Where the link starts.
    pub start: [f32; 3],
    /// Where it lands.
    pub end: [f32; 3],
    /// Whether it can be taken from `end` to `start` too.
    pub bidirectional: bool,
    /// The link's flag bits (a [`LayerAgent::exclude_flags`] bit refuses it).
    pub flags: u32,
}

/// Bake settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayeredBakeSettings {
    /// The nav tile side, millimeters (a ground edit dirties whole tiles). Default
    /// [`DEFAULT_TILE_MM`].
    pub tile_mm: i32,
    /// The radius the mesh was eroded by (the hull class it serves), millimeters. Recorded in the
    /// bake; [`LayerAgent::for_bake`] reads it.
    pub agent_radius_mm: i32,
    /// How far a link end may be from a polygon and still attach to it, millimeters.
    pub link_snap_mm: i32,
}

impl Default for LayeredBakeSettings {
    fn default() -> Self {
        LayeredBakeSettings {
            tile_mm: DEFAULT_TILE_MM,
            agent_radius_mm: 0,
            link_snap_mm: 2_000,
        }
    }
}

/// What an edge between two polygons is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum LayerEdgeKind {
    /// A shared polygon edge.
    Portal,
    /// An off-mesh link (from `a` to `b`).
    Link,
}

/// One edge of the baked graph, integer millimeters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerEdge {
    /// The polygon on the other side.
    pub neighbor: u32,
    /// Portal: one end of the shared edge (`[x, z]`). Link: the start.
    pub a: [i32; 2],
    /// Portal: the other end. Link: the landing.
    pub b: [i32; 2],
    /// Portal or link.
    pub kind: LayerEdgeKind,
    /// The link's flag bits (0 for a portal).
    pub flags: u32,
}

/// One baked polygon, integer millimeters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerPoly {
    /// The boundary `[x, z]`, counter-clockwise (positive area with +x east, +z north), starting at
    /// the smallest vertex.
    pub verts: Vec<[i32; 2]>,
    /// The height, millimeters.
    pub y_mm: i32,
    /// The area id.
    pub area: u8,
    /// The layer: how many distinct walkable levels lie over the polygon's centroid (0 = top).
    pub layer: u8,
    /// The owning world-geometry section, [`NO_SECTION`] outdoors.
    pub section: u16,
    /// The bounding box `[x0, z0, x1, z1]`.
    pub bbox: [i32; 4],
    /// The centroid `[x, z]` (integer average of the vertices).
    pub centroid: [i32; 2],
    /// The first of this polygon's edges in [`LayeredBake::edges`].
    pub first_edge: u32,
    /// How many edges it has.
    pub edge_count: u32,
}

impl LayerPoly {
    /// Whether the polygon is inside a passage (tunnel or flank area).
    pub fn in_passage(&self) -> bool {
        self.area == LAYER_AREA_TUNNEL || self.area == LAYER_AREA_FLANK
    }

    /// Whether `p` (`[x, z]` mm) is inside or on the boundary.
    pub fn contains(&self, p: [i32; 2]) -> bool {
        let n = self.verts.len();
        if n < 3 {
            return false;
        }
        for i in 0..n {
            let a = self.verts[i];
            let b = self.verts[(i + 1) % n];
            if cross(sub(b, a), sub(p, a)) < 0 {
                return false;
            }
        }
        true
    }

    /// The squared distance from `p` to the polygon (0 inside).
    pub fn dist2_to(&self, p: [i32; 2]) -> i64 {
        if self.contains(p) {
            return 0;
        }
        let n = self.verts.len();
        let mut best = i64::MAX;
        for i in 0..n {
            let d = seg_dist2(p, self.verts[i], self.verts[(i + 1) % n]);
            if d < best {
                best = d;
            }
        }
        best
    }
}

/// The layered bake: the polygons, their edges and the tile grid, integer millimeters.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayeredBake {
    /// The polygons in canonical order.
    pub polys: Vec<LayerPoly>,
    /// Every polygon's edges, grouped per polygon and sorted by `(neighbor, kind, a, b)`.
    pub edges: Vec<LayerEdge>,
    /// The settings the bake was made with.
    pub settings: LayeredBakeSettings,
    /// The tile grid origin `[x, z]`, millimeters (a multiple of the tile side).
    pub tile_origin: [i32; 2],
    /// Tiles along x.
    pub tiles_x: u32,
    /// Tiles along z.
    pub tiles_z: u32,
    buckets: Buckets,
    /// Which cost classes the polygons use: bit 0 open ground, bit 1 tunnel, bit 2 flank (the
    /// heuristic's share is the cheapest present class).
    classes: u8,
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
struct Buckets {
    origin: [i64; 2],
    nx: i64,
    nz: i64,
    start: Vec<u32>,
    items: Vec<u32>,
}

impl Buckets {
    fn build(polys: &[LayerPoly]) -> Buckets {
        if polys.is_empty() {
            return Buckets::default();
        }
        let mut lo = [i64::MAX; 2];
        let mut hi = [i64::MIN; 2];
        for p in polys {
            lo[0] = lo[0].min(i64::from(p.bbox[0]));
            lo[1] = lo[1].min(i64::from(p.bbox[1]));
            hi[0] = hi[0].max(i64::from(p.bbox[2]));
            hi[1] = hi[1].max(i64::from(p.bbox[3]));
        }
        let origin = [
            lo[0].div_euclid(BUCKET_MM) * BUCKET_MM,
            lo[1].div_euclid(BUCKET_MM) * BUCKET_MM,
        ];
        let nx = (hi[0] - origin[0]) / BUCKET_MM + 1;
        let nz = (hi[1] - origin[1]) / BUCKET_MM + 1;
        let cells = (nx * nz) as usize;
        let mut counts = vec![0u32; cells + 1];
        let range = |p: &LayerPoly| {
            let x0 = (i64::from(p.bbox[0]) - origin[0]) / BUCKET_MM;
            let z0 = (i64::from(p.bbox[1]) - origin[1]) / BUCKET_MM;
            let x1 = (i64::from(p.bbox[2]) - origin[0]) / BUCKET_MM;
            let z1 = (i64::from(p.bbox[3]) - origin[1]) / BUCKET_MM;
            (x0, z0, x1, z1)
        };
        for p in polys {
            let (x0, z0, x1, z1) = range(p);
            for z in z0..=z1 {
                for x in x0..=x1 {
                    counts[(z * nx + x) as usize + 1] += 1;
                }
            }
        }
        let mut acc = 0u32;
        for c in counts.iter_mut() {
            acc += *c;
            *c = acc;
        }
        let mut fill = counts.clone();
        let mut items = vec![0u32; counts[cells] as usize];
        for (i, p) in polys.iter().enumerate() {
            let (x0, z0, x1, z1) = range(p);
            for z in z0..=z1 {
                for x in x0..=x1 {
                    let c = (z * nx + x) as usize;
                    items[fill[c] as usize] = i as u32;
                    fill[c] += 1;
                }
            }
        }
        Buckets {
            origin,
            nx,
            nz,
            start: counts,
            items,
        }
    }

    /// The polygons whose box overlaps the bucket of `p`, ascending.
    fn at(&self, p: [i32; 2]) -> &[u32] {
        if self.nx == 0 {
            return &[];
        }
        let x = (i64::from(p[0]) - self.origin[0]).div_euclid(BUCKET_MM);
        let z = (i64::from(p[1]) - self.origin[1]).div_euclid(BUCKET_MM);
        if x < 0 || z < 0 || x >= self.nx || z >= self.nz {
            return &[];
        }
        let c = (z * self.nx + x) as usize;
        &self.items[self.start[c] as usize..self.start[c + 1] as usize]
    }
}

/// A rectangle in world millimeters, `[x0, x1) x [z0, z1)`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RectMm {
    /// West edge.
    pub x0: i32,
    /// South edge.
    pub z0: i32,
    /// East edge (exclusive).
    pub x1: i32,
    /// North edge (exclusive).
    pub z1: i32,
}

/// A body the plan is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LayerAgent {
    /// The body radius, millimeters (the bake is eroded by it already; used for the lane).
    pub radius_mm: i32,
    /// How far above a polygon a body may stand and still be on it, millimeters.
    pub climb_mm: i32,
    /// How far from every polygon an endpoint may be and still snap to the nearest, millimeters.
    pub snap_mm: i32,
    /// A link with any of these flag bits is refused.
    pub exclude_flags: u32,
    /// A millimeter inside a tunnel polygon costs this percent of one on open ground (10..=100).
    pub tunnel_pct: u32,
    /// The same for a flank polygon (10..=100).
    pub flank_pct: u32,
    /// The width a lane leaves free for a body coming the other way, millimeters; 0 turns the
    /// lane rule off.
    pub passing_gap_mm: i32,
    /// The expansion budget.
    pub max_expansions: u32,
}

impl LayerAgent {
    /// The agent of a bake: its radius, a climb of 1 m, a 3 m snap, the passage costs 75 and 20,
    /// a passing gap of one body diameter, the default budget.
    pub fn for_bake(bake: &LayeredBake) -> LayerAgent {
        let r = bake.settings.agent_radius_mm.max(0);
        LayerAgent {
            radius_mm: r,
            climb_mm: 1_000,
            snap_mm: 3_000,
            exclude_flags: 0,
            tunnel_pct: 75,
            flank_pct: 20,
            passing_gap_mm: r.saturating_mul(2),
            max_expansions: LAYERED_MAX_EXPANSIONS,
        }
    }
}

/// Why a plan failed. [`LayerPlanError::code`] is the agent-protocol code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayerPlanError {
    /// The start is off the mesh (beyond the snap distance).
    StartOffMesh,
    /// The goal is off the mesh.
    GoalOffMesh,
    /// The start polygon is closed (a blocker over it).
    StartClosed,
    /// The goal polygon is closed.
    GoalClosed,
    /// No open route joins them.
    Unreachable,
    /// The expansion budget ran out.
    Budget,
}

impl LayerPlanError {
    /// The lowercase protocol code.
    pub fn code(self) -> &'static str {
        match self {
            LayerPlanError::StartOffMesh => "plan_start_off_mesh",
            LayerPlanError::GoalOffMesh => "plan_goal_off_mesh",
            LayerPlanError::StartClosed => "plan_start_closed",
            LayerPlanError::GoalClosed => "plan_goal_closed",
            LayerPlanError::Unreachable => "plan_unreachable",
            LayerPlanError::Budget => "plan_budget",
        }
    }
}

impl std::fmt::Display for LayerPlanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for LayerPlanError {}

/// A plan on the layered bake.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerPlan {
    /// The corners `[x, y, z]` millimeters, start first, goal last.
    pub waypoints: Vec<[i32; 3]>,
    /// The polygon of each waypoint.
    pub waypoint_polys: Vec<u32>,
    /// The layer of each waypoint.
    pub layers: Vec<u8>,
    /// The polygon corridor, start polygon first.
    pub polys: Vec<u32>,
    /// The A* cost of the corridor (millimeters, passage shares applied).
    pub cost: u64,
    /// Polygons expanded.
    pub expansions: u32,
    /// The [`LayerState`] generation the plan was made at.
    pub generation: u64,
    /// Whether the corridor crosses a dirty tile (a ground edit not yet re-baked).
    pub crosses_dirty: bool,
}

impl LayerPlan {
    /// The layer of the goal waypoint.
    pub fn goal_layer(&self) -> u8 {
        self.layers.last().copied().unwrap_or(0)
    }

    /// The plan's length along its waypoints (xz), millimeters.
    pub fn length_mm(&self) -> u64 {
        self.waypoints
            .windows(2)
            .map(|w| dist_mm([w[0][0], w[0][2]], [w[1][0], w[1][2]]))
            .sum()
    }
}

/// The reusable search buffers of [`LayeredBake::plan`] (sized to a bake on first use).
#[derive(Clone, Debug, Default)]
pub struct LayerScratch {
    g: Vec<u64>,
    parent: Vec<u32>,
    parent_edge: Vec<u32>,
    via: Vec<[i32; 2]>,
    closed: Vec<bool>,
    touched: Vec<u32>,
    heap: BinaryHeap<Reverse<(u64, u64, u32)>>,
}

impl LayerScratch {
    fn reset(&mut self, n: usize) {
        if self.g.len() != n {
            self.g = vec![u64::MAX; n];
            self.parent = vec![u32::MAX; n];
            self.parent_edge = vec![u32::MAX; n];
            self.via = vec![[0, 0]; n];
            self.closed = vec![false; n];
            self.touched.clear();
        } else {
            for &i in &self.touched {
                let i = i as usize;
                self.g[i] = u64::MAX;
                self.parent[i] = u32::MAX;
                self.parent_edge[i] = u32::MAX;
                self.closed[i] = false;
            }
            self.touched.clear();
        }
        self.heap.clear();
    }
}

/// The per-match state the query reads: closed polygons and dirty tiles, with a generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerState {
    generation: u64,
    closed_sections: Vec<u16>,
    closed_polys: Vec<u64>,
    closed_count: u32,
    dirty_tiles: Vec<u32>,
}

/// One generation's changes for [`LayerState::sync`].
#[derive(Clone, Copy, Debug, Default)]
pub struct LayerSync<'a> {
    /// The generation these changes belong to (the world-geometry state's generation).
    pub generation: u64,
    /// Every section closed at this generation (the whole set; any order).
    pub closed_sections: &'a [u16],
    /// The ground rectangles edited since the last sync.
    pub dirty_rects: &'a [RectMm],
}

impl LayerState {
    /// A fresh state: nothing closed, nothing dirty, generation 0.
    pub fn new(bake: &LayeredBake) -> LayerState {
        LayerState {
            generation: 0,
            closed_sections: Vec::new(),
            closed_polys: vec![0; bake.polys.len().div_ceil(64)],
            closed_count: 0,
            dirty_tiles: Vec::new(),
        }
    }

    /// Applies one generation, in this fixed order: the closed set is replaced whole (and each
    /// polygon's bit recomputed from its section), the dirty rectangles mark their tiles, then the
    /// generation is set. Returns the polygons whose closed bit changed, ascending.
    pub fn sync(&mut self, bake: &LayeredBake, sync: &LayerSync<'_>) -> Vec<u32> {
        let _span = tracing::trace_span!("det_layers_sync").entered();
        let mut sections = sync.closed_sections.to_vec();
        sections.sort_unstable();
        sections.dedup();
        let mut flipped = Vec::new();
        if sections != self.closed_sections || self.closed_polys.len() != bake.polys.len().div_ceil(64) {
            if self.closed_polys.len() != bake.polys.len().div_ceil(64) {
                self.closed_polys = vec![0; bake.polys.len().div_ceil(64)];
                self.closed_count = 0;
            }
            for (i, p) in bake.polys.iter().enumerate() {
                let now = p.section != NO_SECTION && sections.binary_search(&p.section).is_ok();
                let word = &mut self.closed_polys[i / 64];
                let bit = 1u64 << (i % 64);
                let was = *word & bit != 0;
                if now != was {
                    *word ^= bit;
                    flipped.push(i as u32);
                    if now {
                        self.closed_count += 1;
                    } else {
                        self.closed_count -= 1;
                    }
                }
            }
            self.closed_sections = sections;
        }
        for rect in sync.dirty_rects {
            bake.tiles_in_rect(*rect, &mut self.dirty_tiles);
        }
        self.dirty_tiles.sort_unstable();
        self.dirty_tiles.dedup();
        self.generation = sync.generation;
        flipped
    }

    /// The generation of the last [`Self::sync`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The closed sections, ascending.
    pub fn closed_sections(&self) -> &[u16] {
        &self.closed_sections
    }

    /// Whether polygon `poly` is closed.
    pub fn poly_closed(&self, poly: u32) -> bool {
        let i = poly as usize;
        self.closed_polys
            .get(i / 64)
            .is_some_and(|w| w & (1u64 << (i % 64)) != 0)
    }

    /// How many polygons are closed.
    pub fn closed_poly_count(&self) -> u32 {
        self.closed_count
    }

    /// The dirty tiles (ascending tile ids, `z * tiles_x + x`).
    pub fn dirty_tiles(&self) -> &[u32] {
        &self.dirty_tiles
    }

    /// Clears the listed tiles (after the host re-baked them).
    pub fn clear_dirty(&mut self, tiles: &[u32]) {
        self.dirty_tiles.retain(|t| !tiles.contains(t));
    }

    /// A hash of the state (FNV-1a over the generation, the closed sections and the dirty tiles),
    /// for desync checks.
    pub fn state_hash(&self) -> u64 {
        let mut h = Fnv::new();
        h.u64(self.generation);
        h.u32(self.closed_sections.len() as u32);
        for s in &self.closed_sections {
            h.bytes(&s.to_le_bytes());
        }
        h.u32(self.dirty_tiles.len() as u32);
        for t in &self.dirty_tiles {
            h.u32(*t);
        }
        h.finish()
    }
}

/// Meters to millimeters, rounded half away from zero, IEEE `* -` only.
pub fn mm(v: f32) -> i32 {
    let s = v * 1000.0;
    let t = s as i32;
    let r = s - t as f32;
    if r >= 0.5 {
        t + 1
    } else if r <= -0.5 {
        t - 1
    } else {
        t
    }
}

fn sub(a: [i32; 2], b: [i32; 2]) -> [i64; 2] {
    [
        i64::from(a[0]) - i64::from(b[0]),
        i64::from(a[1]) - i64::from(b[1]),
    ]
}

fn cross(u: [i64; 2], v: [i64; 2]) -> i64 {
    u[0] * v[1] - u[1] * v[0]
}

fn dist_mm(a: [i32; 2], b: [i32; 2]) -> u64 {
    let d = sub(b, a);
    ((d[0] * d[0] + d[1] * d[1]) as u64).isqrt()
}

fn seg_dist2(p: [i32; 2], a: [i32; 2], b: [i32; 2]) -> i64 {
    let ab = sub(b, a);
    let ap = sub(p, a);
    let len2 = ab[0] * ab[0] + ab[1] * ab[1];
    if len2 == 0 {
        return ap[0] * ap[0] + ap[1] * ap[1];
    }
    let t = ap[0] * ab[0] + ap[1] * ab[1];
    if t <= 0 {
        return ap[0] * ap[0] + ap[1] * ap[1];
    }
    if t >= len2 {
        let bp = sub(p, b);
        return bp[0] * bp[0] + bp[1] * bp[1];
    }
    // Perpendicular distance squared: cross^2 / len2 (integer division rounds down).
    let c = i128::from(cross(ab, ap));
    (c * c / i128::from(len2)) as i64
}

struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, b: &[u8]) {
        for x in b {
            self.0 ^= u64::from(*x);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.bytes(&v.to_le_bytes());
    }
    fn finish(&self) -> u64 {
        self.0
    }
}

/// FNV-1a 64 of `bytes` (the bake's content hash and the plan pins use it).
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = Fnv::new();
    h.bytes(bytes);
    h.finish()
}

/// A quantized input polygon before sorting.
struct Quant {
    verts: Vec<[i32; 2]>,
    y_mm: i32,
    area: u8,
    section: u16,
}

fn quantize(p: &LayerPolyInput) -> Option<Quant> {
    let first = p.vertices.first()?;
    let y_mm = mm(first[1]);
    let mut verts: Vec<[i32; 2]> = Vec::with_capacity(p.vertices.len());
    for v in &p.vertices {
        let q = [mm(v[0]), mm(v[2])];
        if verts.last() != Some(&q) {
            verts.push(q);
        }
    }
    while verts.len() > 1 && verts.first() == verts.last() {
        verts.pop();
    }
    if verts.len() < 3 {
        return None;
    }
    let n = verts.len();
    let mut area2: i64 = 0;
    for i in 0..n {
        let a = verts[i];
        let b = verts[(i + 1) % n];
        area2 += i64::from(a[0]) * i64::from(b[1]) - i64::from(b[0]) * i64::from(a[1]);
    }
    if area2 == 0 {
        return None;
    }
    if area2 < 0 {
        verts.reverse();
    }
    let start = (0..verts.len()).min_by_key(|&i| verts[i]).unwrap_or(0);
    verts.rotate_left(start);
    Some(Quant {
        verts,
        y_mm,
        area: p.area,
        section: p.section.unwrap_or(NO_SECTION),
    })
}

impl LayeredBake {
    /// Bakes a layered polygon mesh into integer millimeters (see the module docs). Polygons with
    /// fewer than three distinct vertices after rounding are dropped with their portals. Link ends
    /// attach to the polygon that contains them (nearest height), or the nearest polygon within
    /// [`LayeredBakeSettings::link_snap_mm`]; a link with an end nowhere near the mesh is dropped.
    pub fn bake(
        polys: &[LayerPolyInput],
        links: &[LayerLinkInput],
        settings: LayeredBakeSettings,
    ) -> LayeredBake {
        let _span =
            tracing::info_span!("det_layers_bake", polys = polys.len(), links = links.len()).entered();
        let tile_mm = settings.tile_mm.max(1_000);
        let settings = LayeredBakeSettings { tile_mm, ..settings };
        // 1. Quantize, then a canonical order: (height, vertices, area, section).
        let quant: Vec<Option<Quant>> = polys.iter().map(quantize).collect();
        let mut order: Vec<u32> = (0..quant.len() as u32)
            .filter(|&i| quant[i as usize].is_some())
            .collect();
        order.sort_by(|&a, &b| {
            let (qa, qb) = (quant[a as usize].as_ref(), quant[b as usize].as_ref());
            match (qa, qb) {
                (Some(qa), Some(qb)) => (qa.y_mm, &qa.verts, qa.area, qa.section, a)
                    .cmp(&(qb.y_mm, &qb.verts, qb.area, qb.section, b)),
                _ => a.cmp(&b),
            }
        });
        let mut remap = vec![u32::MAX; quant.len()];
        for (new, &old) in order.iter().enumerate() {
            remap[old as usize] = new as u32;
        }
        let mut out: Vec<LayerPoly> = Vec::with_capacity(order.len());
        for &old in &order {
            let Some(q) = quant[old as usize].as_ref() else {
                continue;
            };
            let mut bbox = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
            let (mut sx, mut sz) = (0i64, 0i64);
            for v in &q.verts {
                bbox[0] = bbox[0].min(v[0]);
                bbox[1] = bbox[1].min(v[1]);
                bbox[2] = bbox[2].max(v[0]);
                bbox[3] = bbox[3].max(v[1]);
                sx += i64::from(v[0]);
                sz += i64::from(v[1]);
            }
            let n = q.verts.len() as i64;
            out.push(LayerPoly {
                verts: q.verts.clone(),
                y_mm: q.y_mm,
                area: q.area,
                layer: 0,
                section: q.section,
                bbox,
                centroid: [sx.div_euclid(n) as i32, sz.div_euclid(n) as i32],
                first_edge: 0,
                edge_count: 0,
            });
        }
        let buckets = Buckets::build(&out);
        // 2. Layers: distinct levels above the centroid.
        let mut above: Vec<i32> = Vec::new();
        let mut layers: Vec<u8> = Vec::with_capacity(out.len());
        for poly in &out {
            above.clear();
            let c = poly.centroid;
            let y = poly.y_mm;
            for &j in buckets.at(c) {
                let other = &out[j as usize];
                if other.y_mm > y.saturating_add(LAYER_GAP_MM) && other.contains(c) {
                    above.push(other.y_mm);
                }
            }
            above.sort_unstable();
            let mut levels = 0u32;
            let mut last: Option<i32> = None;
            for &h in &above {
                if last.is_none_or(|l| h > l.saturating_add(LAYER_GAP_MM)) {
                    levels += 1;
                    last = Some(h);
                }
            }
            layers.push(levels.min(u32::from(u8::MAX)) as u8);
        }
        for (poly, layer) in out.iter_mut().zip(layers) {
            poly.layer = layer;
        }
        // 3. Edges: portals, then links.
        let mut per: Vec<Vec<LayerEdge>> = vec![Vec::new(); out.len()];
        for (old, p) in polys.iter().enumerate() {
            let from = remap[old];
            if from == u32::MAX {
                continue;
            }
            for portal in &p.portals {
                let to = remap.get(portal.neighbor as usize).copied().unwrap_or(u32::MAX);
                if to == u32::MAX || to == from {
                    continue;
                }
                let (a, b) = (
                    [mm(portal.a[0]), mm(portal.a[2])],
                    [mm(portal.b[0]), mm(portal.b[2])],
                );
                let (a, b) = if a <= b { (a, b) } else { (b, a) };
                per[from as usize].push(LayerEdge {
                    neighbor: to,
                    a,
                    b,
                    kind: LayerEdgeKind::Portal,
                    flags: 0,
                });
            }
        }
        let snap2 = i64::from(settings.link_snap_mm.max(0)).pow(2);
        let attach = |p: [f32; 3]| -> Option<u32> {
            let q = [mm(p[0]), mm(p[2])];
            locate_in(&out, &buckets, q, mm(p[1]), 1_000, snap2, |_| true)
        };
        for link in links {
            let (Some(s), Some(e)) = (attach(link.start), attach(link.end)) else {
                continue;
            };
            if s == e {
                continue;
            }
            let (a, b) = (
                [mm(link.start[0]), mm(link.start[2])],
                [mm(link.end[0]), mm(link.end[2])],
            );
            per[s as usize].push(LayerEdge {
                neighbor: e,
                a,
                b,
                kind: LayerEdgeKind::Link,
                flags: link.flags,
            });
            if link.bidirectional {
                per[e as usize].push(LayerEdge {
                    neighbor: s,
                    a: b,
                    b: a,
                    kind: LayerEdgeKind::Link,
                    flags: link.flags,
                });
            }
        }
        let mut edges = Vec::new();
        for (i, mut list) in per.into_iter().enumerate() {
            list.sort_by_key(|e| (e.neighbor, e.kind, e.a, e.b, e.flags));
            list.dedup();
            out[i].first_edge = edges.len() as u32;
            out[i].edge_count = list.len() as u32;
            edges.extend(list);
        }
        // 4. Tiles over the polygons' bounds.
        let (mut lo, mut hi) = ([0i64; 2], [0i64; 2]);
        if !out.is_empty() {
            lo = [i64::MAX; 2];
            hi = [i64::MIN; 2];
            for p in &out {
                lo[0] = lo[0].min(i64::from(p.bbox[0]));
                lo[1] = lo[1].min(i64::from(p.bbox[1]));
                hi[0] = hi[0].max(i64::from(p.bbox[2]));
                hi[1] = hi[1].max(i64::from(p.bbox[3]));
            }
        }
        let t = i64::from(tile_mm);
        let origin = [lo[0].div_euclid(t) * t, lo[1].div_euclid(t) * t];
        let tiles_x = ((hi[0] - origin[0]) / t + 1) as u32;
        let tiles_z = ((hi[1] - origin[1]) / t + 1) as u32;
        let classes = out.iter().fold(0u8, |c, p| {
            c | match p.area {
                LAYER_AREA_TUNNEL => 2,
                LAYER_AREA_FLANK => 4,
                _ => 1,
            }
        });
        LayeredBake {
            polys: out,
            edges,
            settings,
            tile_origin: [origin[0] as i32, origin[1] as i32],
            tiles_x,
            tiles_z,
            buckets,
            classes,
        }
    }

    /// The edges of polygon `poly`.
    pub fn edges_of(&self, poly: u32) -> &[LayerEdge] {
        let p = &self.polys[poly as usize];
        &self.edges[p.first_edge as usize..(p.first_edge + p.edge_count) as usize]
    }

    /// How many distinct layers the bake has (the largest layer index plus one; 0 when empty).
    pub fn layer_count(&self) -> u32 {
        self.polys
            .iter()
            .map(|p| u32::from(p.layer) + 1)
            .max()
            .unwrap_or(0)
    }

    /// The tile ids (`z * tiles_x + x`) a rectangle overlaps, appended to `out` in ascending order
    /// of z then x. A rectangle outside the grid adds nothing.
    pub fn tiles_in_rect(&self, rect: RectMm, out: &mut Vec<u32>) {
        if self.tiles_x == 0 || rect.x1 <= rect.x0 || rect.z1 <= rect.z0 {
            return;
        }
        let t = i64::from(self.settings.tile_mm);
        let tx = |x: i64| (x - i64::from(self.tile_origin[0])).div_euclid(t);
        let tz = |z: i64| (z - i64::from(self.tile_origin[1])).div_euclid(t);
        let x0 = tx(i64::from(rect.x0)).max(0);
        let z0 = tz(i64::from(rect.z0)).max(0);
        let x1 = tx(i64::from(rect.x1) - 1).min(i64::from(self.tiles_x) - 1);
        let z1 = tz(i64::from(rect.z1) - 1).min(i64::from(self.tiles_z) - 1);
        for z in z0..=z1 {
            for x in x0..=x1 {
                out.push((z * i64::from(self.tiles_x) + x) as u32);
            }
        }
    }

    /// The tiles polygon `poly`'s box overlaps.
    pub fn poly_tiles(&self, poly: u32, out: &mut Vec<u32>) {
        let b = self.polys[poly as usize].bbox;
        self.tiles_in_rect(
            RectMm {
                x0: b[0],
                z0: b[1],
                x1: b[2].saturating_add(1),
                z1: b[3].saturating_add(1),
            },
            out,
        );
    }

    /// The polygons whose box overlaps any of `tiles`, ascending (what a re-bake of those tiles
    /// replaces).
    pub fn polys_in_tiles(&self, tiles: &[u32]) -> Vec<u32> {
        let mut mine = Vec::new();
        let mut out = Vec::new();
        for i in 0..self.polys.len() as u32 {
            mine.clear();
            self.poly_tiles(i, &mut mine);
            if mine.iter().any(|t| tiles.binary_search(t).is_ok()) {
                out.push(i);
            }
        }
        out
    }

    /// The polygon under `p` (`[x, y, z]` mm): the one containing it whose height is the highest
    /// at most `climb_mm` above `p`, else the lowest above that, else the nearest within `snap_mm`
    /// (ties: distance, height difference, index).
    pub fn locate(&self, p: [i32; 3], climb_mm: i32, snap_mm: i32) -> Option<u32> {
        let snap2 = i64::from(snap_mm.max(0)).pow(2);
        locate_in(
            &self.polys,
            &self.buckets,
            [p[0], p[2]],
            p[1],
            climb_mm,
            snap2,
            |_| true,
        )
    }

    /// The canonical bytes: magic, version, settings, tile grid, then every polygon (height,
    /// area, layer, section, vertex count, vertices) and every edge (neighbor, kind, flags, ends),
    /// all little-endian.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(64 + self.polys.len() * 64 + self.edges.len() * 24);
        b.extend_from_slice(&LAYERED_BAKE_MAGIC);
        b.extend_from_slice(&LAYERED_BAKE_VERSION.to_le_bytes());
        for v in [
            self.settings.tile_mm,
            self.settings.agent_radius_mm,
            self.settings.link_snap_mm,
        ] {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.tile_origin[0].to_le_bytes());
        b.extend_from_slice(&self.tile_origin[1].to_le_bytes());
        b.extend_from_slice(&self.tiles_x.to_le_bytes());
        b.extend_from_slice(&self.tiles_z.to_le_bytes());
        b.extend_from_slice(&(self.polys.len() as u32).to_le_bytes());
        for p in &self.polys {
            b.extend_from_slice(&p.y_mm.to_le_bytes());
            b.push(p.area);
            b.push(p.layer);
            b.extend_from_slice(&p.section.to_le_bytes());
            b.extend_from_slice(&p.edge_count.to_le_bytes());
            b.extend_from_slice(&(p.verts.len() as u32).to_le_bytes());
            for v in &p.verts {
                b.extend_from_slice(&v[0].to_le_bytes());
                b.extend_from_slice(&v[1].to_le_bytes());
            }
        }
        b.extend_from_slice(&(self.edges.len() as u32).to_le_bytes());
        for e in &self.edges {
            b.extend_from_slice(&e.neighbor.to_le_bytes());
            b.push(match e.kind {
                LayerEdgeKind::Portal => 0,
                LayerEdgeKind::Link => 1,
            });
            b.extend_from_slice(&e.flags.to_le_bytes());
            for v in [e.a[0], e.a[1], e.b[0], e.b[1]] {
                b.extend_from_slice(&v.to_le_bytes());
            }
        }
        b
    }

    /// FNV-1a 64 of [`Self::to_bytes`]: the value peers compare before a match.
    pub fn content_hash(&self) -> u64 {
        fnv1a64(&self.to_bytes())
    }

    /// Plans from `from` to `goal` (`[x, y, z]` mm) for `agent` on the open polygons of `state`.
    ///
    /// # Errors
    ///
    /// [`LayerPlanError`]: an endpoint off the mesh or closed, no open route, or the budget.
    pub fn plan(
        &self,
        state: &LayerState,
        from: [i32; 3],
        goal: [i32; 3],
        agent: &LayerAgent,
        scratch: &mut LayerScratch,
    ) -> Result<LayerPlan, LayerPlanError> {
        let _span = tracing::trace_span!("det_layers_plan").entered();
        let start = self
            .locate(from, agent.climb_mm, agent.snap_mm)
            .ok_or(LayerPlanError::StartOffMesh)?;
        let end = self
            .locate(goal, agent.climb_mm, agent.snap_mm)
            .ok_or(LayerPlanError::GoalOffMesh)?;
        if state.poly_closed(start) {
            return Err(LayerPlanError::StartClosed);
        }
        if state.poly_closed(end) {
            return Err(LayerPlanError::GoalClosed);
        }
        let tunnel_pct = u64::from(agent.tunnel_pct.clamp(10, 100));
        let flank_pct = u64::from(agent.flank_pct.clamp(10, 100));
        // The heuristic's share: the cheapest cost class the bake has (admissible, and as tight as
        // the bake allows: a map without flank polygons is not searched at the flank share).
        let h_pct = [(1u8, 100u64), (2, tunnel_pct), (4, flank_pct)]
            .iter()
            .filter(|(bit, _)| self.classes & bit != 0)
            .map(|&(_, pct)| pct)
            .min()
            .unwrap_or(100);
        let pct_of = |poly: &LayerPoly| -> u64 {
            match poly.area {
                LAYER_AREA_TUNNEL => tunnel_pct,
                LAYER_AREA_FLANK => flank_pct,
                _ => 100,
            }
        };
        let goal_xz = [goal[0], goal[2]];
        let heuristic = |id: u32| -> u64 {
            let d = dist_mm(self.polys[id as usize].centroid, goal_xz);
            if h_pct < 100 { d * h_pct / 100 } else { d }
        };
        let n = self.polys.len();
        scratch.reset(n);
        let s = start as usize;
        scratch.g[s] = 0;
        scratch.via[s] = [from[0], from[2]];
        scratch.touched.push(start);
        let h0 = heuristic(start);
        scratch.heap.push(Reverse((h0, h0, start)));
        let mut expansions = 0u32;
        let mut reached = false;
        // A* over polygons, (f, h, index) ties and ascending edge order: every peer expands the
        // same polygons in the same order.
        while let Some(Reverse((_, _, id))) = scratch.heap.pop() {
            let i = id as usize;
            if scratch.closed[i] {
                continue;
            }
            scratch.closed[i] = true;
            expansions += 1;
            if expansions > agent.max_expansions {
                return Err(LayerPlanError::Budget);
            }
            if id == end {
                reached = true;
                break;
            }
            let ga = scratch.g[i];
            let here = &self.polys[i];
            let pct = pct_of(here);
            for k in 0..here.edge_count {
                let e_idx = here.first_edge + k;
                let e = &self.edges[e_idx as usize];
                let nb = e.neighbor as usize;
                if scratch.closed[nb] || state.poly_closed(e.neighbor) {
                    continue;
                }
                if e.kind == LayerEdgeKind::Link && e.flags & agent.exclude_flags != 0 {
                    continue;
                }
                let mid = match e.kind {
                    LayerEdgeKind::Portal => [
                        ((i64::from(e.a[0]) + i64::from(e.b[0])).div_euclid(2)) as i32,
                        ((i64::from(e.a[1]) + i64::from(e.b[1])).div_euclid(2)) as i32,
                    ],
                    LayerEdgeKind::Link => e.b,
                };
                let raw = match e.kind {
                    LayerEdgeKind::Portal => dist_mm(scratch.via[i], mid),
                    LayerEdgeKind::Link => dist_mm(scratch.via[i], e.a) + dist_mm(e.a, e.b),
                };
                let step = if pct < 100 { raw * pct / 100 } else { raw };
                let ng = ga.saturating_add(step.max(1));
                if ng < scratch.g[nb] {
                    if scratch.g[nb] == u64::MAX {
                        scratch.touched.push(e.neighbor);
                    }
                    scratch.g[nb] = ng;
                    scratch.parent[nb] = id;
                    scratch.parent_edge[nb] = e_idx;
                    scratch.via[nb] = mid;
                    let h = heuristic(e.neighbor);
                    scratch.heap.push(Reverse((ng.saturating_add(h), h, e.neighbor)));
                }
            }
        }
        if !reached {
            return Err(LayerPlanError::Unreachable);
        }
        let mut corridor = vec![end];
        let mut used = Vec::new();
        let mut at = end;
        while at != start {
            used.push(scratch.parent_edge[at as usize]);
            at = scratch.parent[at as usize];
            corridor.push(at);
        }
        corridor.reverse();
        used.reverse();
        let mut plan = self.funnel(state, &corridor, &used, from, goal, agent);
        plan.cost = scratch.g[end as usize];
        plan.expansions = expansions;
        plan.generation = state.generation;
        if !state.dirty_tiles.is_empty() {
            let mut tiles = Vec::new();
            plan.crosses_dirty = corridor.iter().any(|&p| {
                tiles.clear();
                self.poly_tiles(p, &mut tiles);
                tiles.iter().any(|t| state.dirty_tiles.binary_search(t).is_ok())
            });
        }
        plan.polys = corridor;
        Ok(plan)
    }

    /// The portal sequence of a corridor, oriented (left, right) along the travel direction, the
    /// funnel over it, then the lane rule ([`Self::lanes`]).
    fn funnel(
        &self,
        state: &LayerState,
        corridor: &[u32],
        used: &[u32],
        from: [i32; 3],
        goal: [i32; 3],
        agent: &LayerAgent,
    ) -> LayerPlan {
        let start = [from[0], from[2]];
        let end = [goal[0], goal[2]];
        // (left, right, polygon index in the corridor after the portal).
        let mut portals: Vec<([i32; 2], [i32; 2], usize)> = Vec::with_capacity(used.len() * 2 + 2);
        portals.push((start, start, 0));
        for (k, &e_idx) in used.iter().enumerate() {
            let e = &self.edges[e_idx as usize];
            let (p, q) = (
                &self.polys[corridor[k] as usize],
                &self.polys[corridor[k + 1] as usize],
            );
            match e.kind {
                LayerEdgeKind::Link => {
                    portals.push((e.a, e.a, k));
                    portals.push((e.b, e.b, k + 1));
                }
                LayerEdgeKind::Portal => {
                    let d = sub(q.centroid, p.centroid);
                    let mid = [
                        ((i64::from(e.a[0]) + i64::from(e.b[0])).div_euclid(2)) as i32,
                        ((i64::from(e.a[1]) + i64::from(e.b[1])).div_euclid(2)) as i32,
                    ];
                    let (left, right) = if cross(d, sub(e.a, mid)) >= 0 {
                        (e.a, e.b)
                    } else {
                        (e.b, e.a)
                    };
                    portals.push((left, right, k + 1));
                }
            }
        }
        portals.push((end, end, corridor.len() - 1));
        let points = string_pull(&portals);
        let mut corners: Vec<([i32; 3], usize)> = Vec::with_capacity(points.len());
        for (pt, ci) in points {
            let poly = corridor[ci];
            let y = if corners.is_empty() {
                from[1]
            } else {
                self.polys[poly as usize].y_mm
            };
            corners.push(([pt[0], y, pt[1]], ci));
        }
        let corners = self.lanes(state, corridor, &corners, agent);
        let mut plan = LayerPlan::default();
        for (pt, poly) in corners {
            plan.waypoints.push(pt);
            plan.waypoint_polys.push(poly);
            plan.layers.push(self.polys[poly as usize].layer);
        }
        if plan.waypoints.len() < 2 {
            let p = &self.polys[corridor[corridor.len() - 1] as usize];
            plan.waypoints.push([end[0], p.y_mm, end[1]]);
            plan.waypoint_polys.push(corridor[corridor.len() - 1]);
            plan.layers.push(p.layer);
        }
        plan
    }
}

/// The spacing of the lane samples along a plan, millimeters.
pub const LANE_STEP_MM: i64 = 2_000;

/// The step of the walk toward a wall, millimeters.
const LANE_MARCH_MM: i64 = 250;

/// How far a lane may move away from the plan, sideways, per [`LANE_STEP_MM`] forward: half of it
/// (about 27 degrees), so a lane that meets a side opening on its right does not swing into it.
const LANE_DRIFT_MM: i64 = LANE_STEP_MM / 2;

/// A walk of [`LayeredBake::lanes`]: from `p` along `dir / len`.
#[derive(Clone, Copy)]
struct Ray {
    p: [i32; 2],
    dir: [i64; 2],
    len: i64,
}

impl LayeredBake {
    /// The open passage polygon that holds `q`: `poly` itself, one of its portal neighbors, or a
    /// neighbor of those (lowest edge order first). Heights are never compared: the walk stays on
    /// the surface it started on, so a passage over or under another one never captures it.
    fn step_into(&self, state: &LayerState, poly: u32, q: [i32; 2]) -> Option<u32> {
        let open = |j: u32| {
            let p = &self.polys[j as usize];
            p.in_passage() && !state.poly_closed(j)
        };
        if self.polys[poly as usize].contains(q) {
            return Some(poly);
        }
        let ring = |j: u32| {
            self.edges_of(j)
                .iter()
                .filter(|e| e.kind == LayerEdgeKind::Portal)
                .map(|e| e.neighbor)
        };
        if let Some(j) = ring(poly).find(|&j| open(j) && self.polys[j as usize].contains(q)) {
            return Some(j);
        }
        ring(poly)
            .filter(|&j| open(j))
            .flat_map(ring)
            .find(|&k| open(k) && self.polys[k as usize].contains(q))
    }

    /// Walks from `p` (in `poly`) along `dir` (scaled by `1 / len`) in [`LANE_MARCH_MM`] steps
    /// while the point stays in open passage polygons connected to `poly`, at most `limit`; the
    /// distance reached, the last point inside and its polygon.
    fn walk(&self, state: &LayerState, ray: Ray, limit: i64, poly: u32) -> (i64, [i32; 2], u32) {
        let Ray { p, dir, len } = ray;
        let (mut best, mut best_poly, mut reached) = (p, poly, 0);
        let mut s = LANE_MARCH_MM;
        while s <= limit {
            let q = [
                (i64::from(p[0]) + dir[0] * s / len) as i32,
                (i64::from(p[1]) + dir[1] * s / len) as i32,
            ];
            match self.step_into(state, best_poly, q) {
                Some(j) => {
                    best = q;
                    best_poly = j;
                    reached = s;
                    s += LANE_MARCH_MM;
                }
                None => break,
            }
        }
        (reached, best, best_poly)
    }

    /// The lane rule. The corners of the shortest path (with their corridor indices) are
    /// resampled every [`LANE_STEP_MM`]. A sample in an open passage polygon of the corridor moves
    /// right (seen along travel, +x east, +z north) by the lane offset, toward the right-hand
    /// wall: on the first sample of a passage the offset is the distance to that wall (at most one
    /// passing gap), after that at most [`LANE_DRIFT_MM`] more than the previous offset per sample,
    /// and never past the wall. A body therefore runs along the right wall of a passage and
    /// carries its line across a side opening or a junction instead of turning into it; two bodies
    /// that meet head-on keep the passage's width between their centers. Where the path turns
    /// left inside a passage (the lane on the outside of the turn), the lane sweeps around the
    /// corner at its offset (`corner_arc`), so it never cuts across the inside of the turn. A
    /// sample that would not move forward of the previous lane point is dropped; samples outside
    /// passages keep their place; the start and the goal stay. Walls are found by walking polygon
    /// adjacency (`walk`), so stacked passages never mix. With a gap of 0 the corners are returned
    /// unchanged.
    fn lanes(
        &self,
        state: &LayerState,
        corridor: &[u32],
        corners: &[([i32; 3], usize)],
        agent: &LayerAgent,
    ) -> Vec<([i32; 3], u32)> {
        let plain = || {
            corners
                .iter()
                .map(|&(p, ci)| (p, corridor[ci]))
                .collect::<Vec<_>>()
        };
        if agent.passing_gap_mm <= 0 || corners.len() < 2 {
            return plain();
        }
        let gap = i64::from(agent.passing_gap_mm);
        // A wall farther than two gaps is an opening, not a wall.
        let limit = 2 * gap;
        let open = |j: u32| self.polys[j as usize].in_passage() && !state.poly_closed(j);
        let mut out: Vec<([i32; 3], u32)> = Vec::with_capacity(corners.len() * 4);
        out.push((corners[0].0, corridor[corners[0].1]));
        // The lane offset and the last lane point, or `None` outside passages.
        let mut lane: Option<(i64, [i32; 2])> = None;
        let mut cursor = corners[0].1;
        let last = corners.len() - 1;
        for k in 0..last {
            let ((a, ca), (b, cb)) = (corners[k], corners[k + 1]);
            let d = [
                i64::from(b[0]) - i64::from(a[0]),
                i64::from(b[2]) - i64::from(a[2]),
            ];
            let len = ((d[0] * d[0] + d[1] * d[1]) as u64).isqrt() as i64;
            cursor = cursor.max(ca).min(cb);
            if len > 0 {
                let right = [d[1], -d[0]];
                let mut t = LANE_STEP_MM;
                while t < len {
                    let p = [
                        (i64::from(a[0]) + d[0] * t / len) as i32,
                        (i64::from(a[2]) + d[1] * t / len) as i32,
                    ];
                    t += LANE_STEP_MM;
                    // The corridor polygon under the sample: the path between two corners runs
                    // through the corridor's polygons `ca..=cb` in order.
                    let found = (cursor..=cb)
                        .chain(ca..cursor)
                        .find(|&c| self.polys[corridor[c] as usize].contains(p));
                    let Some(c) = found else {
                        continue;
                    };
                    cursor = cursor.max(c);
                    let poly = corridor[c];
                    if !open(poly) {
                        lane = None;
                        out.push(([p[0], self.polys[poly as usize].y_mm, p[1]], poly));
                        continue;
                    }
                    let (r_ext, _, _) = self.walk(state, Ray { p, dir: right, len }, limit, poly);
                    let off = match lane {
                        None => r_ext.min(gap),
                        Some((off, _)) => (off + LANE_DRIFT_MM).min(r_ext),
                    };
                    let (_, target, target_poly) = self.walk(state, Ray { p, dir: right, len }, off, poly);
                    if let Some((_, q)) = lane {
                        let forward = (i64::from(target[0]) - i64::from(q[0])) * d[0]
                            + (i64::from(target[1]) - i64::from(q[1])) * d[1];
                        if forward <= 0 {
                            continue;
                        }
                    }
                    lane = Some((off, target));
                    out.push((
                        [target[0], self.polys[target_poly as usize].y_mm, target[1]],
                        target_poly,
                    ));
                }
            }
            let corner_poly = corridor[cb];
            if k + 1 == last {
                out.push((b, corner_poly));
            } else if !open(corner_poly) {
                // An inner corner outside every passage stays (the path turns on open ground).
                lane = None;
                out.push((b, corner_poly));
            } else if let Some((off, _)) = lane {
                // Inside a passage the samples replace the corner; a left turn sweeps the lane
                // around it.
                let c = corners[k + 2].0;
                let e = [
                    i64::from(c[0]) - i64::from(b[0]),
                    i64::from(c[2]) - i64::from(b[2]),
                ];
                for (q, j) in self.corner_arc(state, [b[0], b[2]], corner_poly, d, e, off) {
                    out.push(([q[0], self.polys[j as usize].y_mm, q[1]], j));
                    lane = Some((off, q));
                }
            }
        }
        out.dedup_by_key(|c| c.0);
        out
    }

    /// The lane points around a passage corner `b` (in `poly`) where the path turns left from
    /// direction `d` to `e`: `b` plus `off` along the right normal of `d`, the directions between
    /// it and the right normal of `e` (through `d` itself on a turn of 90 degrees or more), and the
    /// right normal of `e`, each cut short where the passage ends. Empty for a right turn or a
    /// non-positive offset.
    fn corner_arc(
        &self,
        state: &LayerState,
        b: [i32; 2],
        poly: u32,
        d: [i64; 2],
        e: [i64; 2],
        off: i64,
    ) -> Vec<([i32; 2], u32)> {
        const UNIT: i64 = 1 << 20;
        let unit = |v: [i64; 2]| -> Option<[i64; 2]> {
            let l = ((v[0] * v[0] + v[1] * v[1]) as u64).isqrt() as i64;
            (l > 0).then(|| [v[0] * UNIT / l, v[1] * UNIT / l])
        };
        let (Some(du), Some(eu)) = (unit(d), unit(e)) else {
            return Vec::new();
        };
        if off <= 0 || cross(du, eu) <= 0 {
            return Vec::new();
        }
        let n_in = [du[1], -du[0]];
        let n_out = [eu[1], -eu[0]];
        let mid = |u: [i64; 2], v: [i64; 2]| unit([u[0] + v[0], u[1] + v[1]]);
        let mut dirs = vec![Some(n_in)];
        if n_in[0] * n_out[0] + n_in[1] * n_out[1] >= 0 {
            dirs.push(mid(n_in, n_out));
        } else {
            dirs.extend([mid(n_in, du), Some(du), mid(du, n_out)]);
        }
        dirs.push(Some(n_out));
        dirs.into_iter()
            .flatten()
            .map(|dir| {
                let (_, q, j) = self.walk(state, Ray { p: b, dir, len: UNIT }, off, poly);
                (q, j)
            })
            .collect()
    }
}

/// The funnel over `(left, right, corridor index)` portals; the first and last are the start and
/// the goal. Returns the corners with the corridor index of the polygon each lies in.
fn string_pull(portals: &[([i32; 2], [i32; 2], usize)]) -> Vec<([i32; 2], usize)> {
    let mut out: Vec<([i32; 2], usize)> = Vec::new();
    let Some(&(first, _, ci0)) = portals.first() else {
        return out;
    };
    out.push((first, ci0));
    let mut apex = first;
    let (mut left, mut right) = (first, first);
    let (mut left_i, mut right_i) = (0usize, 0usize);
    let mut i = 1usize;
    let mut guard = 0usize;
    while i < portals.len() {
        guard += 1;
        if guard > portals.len() * portals.len() + 16 {
            break;
        }
        let (pl, pr, _) = portals[i];
        // Right side: tighten when the new right is left of (or on) the ray apex -> right.
        if cross(sub(right, apex), sub(pr, apex)) >= 0 {
            if apex == right || cross(sub(left, apex), sub(pr, apex)) < 0 {
                right = pr;
                right_i = i;
            } else {
                // The right crossed over the left: the left is a corner.
                apex = left;
                let apex_i = left_i;
                push_corner(&mut out, apex, portals[apex_i].2);
                left = apex;
                right = apex;
                left_i = apex_i;
                right_i = apex_i;
                i = apex_i + 1;
                continue;
            }
        }
        // Left side: tighten when the new left is right of (or on) the ray apex -> left.
        if cross(sub(left, apex), sub(pl, apex)) <= 0 {
            if apex == left || cross(sub(right, apex), sub(pl, apex)) > 0 {
                left = pl;
                left_i = i;
            } else {
                apex = right;
                let apex_i = right_i;
                push_corner(&mut out, apex, portals[apex_i].2);
                left = apex;
                right = apex;
                left_i = apex_i;
                right_i = apex_i;
                i = apex_i + 1;
                continue;
            }
        }
        i += 1;
    }
    if let Some(&(last, _, ci)) = portals.last() {
        push_corner(&mut out, last, ci);
    }
    out
}

fn push_corner(out: &mut Vec<([i32; 2], usize)>, p: [i32; 2], ci: usize) {
    if out.last().map(|o| o.0) != Some(p) {
        out.push((p, ci));
    }
}

fn locate_in(
    polys: &[LayerPoly],
    buckets: &Buckets,
    q: [i32; 2],
    y: i32,
    climb_mm: i32,
    snap2: i64,
    allow: impl Fn(u32) -> bool,
) -> Option<u32> {
    let cand = buckets.at(q);
    let top = y.saturating_add(climb_mm.max(0));
    // Containing polygons: the highest at or under `top`, else the lowest above it.
    let mut under: Option<(i32, u32)> = None;
    let mut over: Option<(i32, u32)> = None;
    for &j in cand {
        let p = &polys[j as usize];
        if !allow(j) || !p.contains(q) {
            continue;
        }
        if p.y_mm <= top {
            if under.is_none_or(|(h, _)| p.y_mm > h) {
                under = Some((p.y_mm, j));
            }
        } else if over.is_none_or(|(h, _)| p.y_mm < h) {
            over = Some((p.y_mm, j));
        }
    }
    if let Some((_, j)) = under.or(over) {
        return Some(j);
    }
    // Nearest within the snap: (distance, height difference, index). The neighbor buckets are
    // searched too, so the snap reaches across one bucket side.
    let mut best: Option<(i64, i64, u32)> = None;
    let reach = (snap2.max(0) as u64).isqrt() as i64;
    let mut seen: Vec<u32> = Vec::new();
    for dz in [-reach, 0, reach] {
        for dx in [-reach, 0, reach] {
            let p2 = [
                (i64::from(q[0]) + dx).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
                (i64::from(q[1]) + dz).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32,
            ];
            seen.extend_from_slice(buckets.at(p2));
        }
    }
    seen.sort_unstable();
    seen.dedup();
    for j in seen {
        if !allow(j) {
            continue;
        }
        let p = &polys[j as usize];
        let d = p.dist2_to(q);
        if d > snap2 {
            continue;
        }
        let key = (d, (i64::from(p.y_mm) - i64::from(y)).abs(), j);
        if best.is_none_or(|b| key < b) {
            best = Some(key);
        }
    }
    best.map(|b| b.2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn quad(x0: f32, z0: f32, x1: f32, z1: f32, y: f32, area: u8) -> LayerPolyInput {
        LayerPolyInput {
            vertices: vec![[x0, y, z0], [x1, y, z0], [x1, y, z1], [x0, y, z1]],
            area,
            portals: Vec::new(),
            section: None,
        }
    }

    fn strip(n: usize, width: f32, area: u8) -> Vec<LayerPolyInput> {
        // n squares of 10 m along x, `width` wide in z, linked in a row.
        let mut v: Vec<LayerPolyInput> = (0..n)
            .map(|i| quad(i as f32 * 10.0, 0.0, i as f32 * 10.0 + 10.0, width, 0.0, area))
            .collect();
        for i in 0..n {
            let x = (i as f32 + 1.0) * 10.0;
            if i + 1 < n {
                v[i].portals.push(LayerPortalInput {
                    neighbor: i as u32 + 1,
                    a: [x, 0.0, 0.0],
                    b: [x, 0.0, width],
                });
                v[i + 1].portals.push(LayerPortalInput {
                    neighbor: i as u32,
                    a: [x, 0.0, width],
                    b: [x, 0.0, 0.0],
                });
            }
        }
        v
    }

    #[test]
    fn mm_rounds_half_away_from_zero() {
        assert_eq!(mm(1.0), 1000);
        assert_eq!(mm(-1.0), -1000);
        assert_eq!(mm(0.0015), 2);
        assert_eq!(mm(-0.0015), -2);
        assert_eq!(mm(0.0004), 0);
        assert_eq!(mm(1280.25), 1_280_250);
    }

    #[test]
    fn bake_order_does_not_depend_on_the_input_order() {
        let a = strip(5, 20.0, LAYER_AREA_TUNNEL);
        let mut b = a.clone();
        b.reverse();
        let n = b.len() as u32;
        for p in &mut b {
            for portal in &mut p.portals {
                portal.neighbor = n - 1 - portal.neighbor;
            }
        }
        let s = LayeredBakeSettings::default();
        let (ba, bb) = (LayeredBake::bake(&a, &[], s), LayeredBake::bake(&b, &[], s));
        assert_eq!(ba.to_bytes(), bb.to_bytes());
        assert_eq!(ba.content_hash(), bb.content_hash());
        assert_eq!(ba.polys.len(), 5);
    }

    #[test]
    fn a_straight_strip_plans_straight_and_lanes_keep_right() {
        let bake = LayeredBake::bake(
            &strip(8, 20.0, LAYER_AREA_TUNNEL),
            &[],
            LayeredBakeSettings::default(),
        );
        let state = LayerState::new(&bake);
        let mut agent = LayerAgent::for_bake(&bake);
        agent.passing_gap_mm = 0;
        let mut scratch = LayerScratch::default();
        let plan = bake
            .plan(
                &state,
                [2_000, 0, 10_000],
                [78_000, 0, 10_000],
                &agent,
                &mut scratch,
            )
            .expect("plans");
        assert_eq!(plan.waypoints, vec![[2_000, 0, 10_000], [78_000, 0, 10_000]]);
        assert_eq!(plan.polys.len(), 8);
        // Lanes: eastbound keeps to z < 10 m - gap / 2 (right of east is south), westbound to the north.
        agent.passing_gap_mm = 9_000;
        let east = bake
            .plan(
                &state,
                [2_000, 0, 10_000],
                [78_000, 0, 10_000],
                &agent,
                &mut scratch,
            )
            .expect("plans");
        let west = bake
            .plan(
                &state,
                [78_000, 0, 10_000],
                [2_000, 0, 10_000],
                &agent,
                &mut scratch,
            )
            .expect("plans");
        for w in &east.waypoints[1..east.waypoints.len() - 1] {
            assert!(w[2] <= 5_500, "east {:?}", east.waypoints);
        }
        for w in &west.waypoints[1..west.waypoints.len() - 1] {
            assert!(w[2] >= 14_500, "west {:?}", west.waypoints);
        }
    }

    #[test]
    fn a_closed_section_reroutes_and_reopens() {
        // Two parallel strips joined at both ends: the north one is section 7.
        let mut polys = vec![
            quad(0.0, 0.0, 10.0, 30.0, 0.0, 0),
            quad(10.0, 0.0, 50.0, 10.0, 0.0, 0),
            quad(10.0, 20.0, 50.0, 30.0, 0.0, 0),
            quad(50.0, 0.0, 60.0, 30.0, 0.0, 0),
        ];
        polys[2].section = Some(7);
        let link = |p: &mut LayerPolyInput, n: u32, a: [f32; 3], b: [f32; 3]| {
            p.portals.push(LayerPortalInput { neighbor: n, a, b })
        };
        link(&mut polys[0], 1, [10.0, 0.0, 0.0], [10.0, 0.0, 10.0]);
        link(&mut polys[0], 2, [10.0, 0.0, 20.0], [10.0, 0.0, 30.0]);
        link(&mut polys[1], 0, [10.0, 0.0, 0.0], [10.0, 0.0, 10.0]);
        link(&mut polys[2], 0, [10.0, 0.0, 20.0], [10.0, 0.0, 30.0]);
        link(&mut polys[1], 3, [50.0, 0.0, 0.0], [50.0, 0.0, 10.0]);
        link(&mut polys[2], 3, [50.0, 0.0, 20.0], [50.0, 0.0, 30.0]);
        link(&mut polys[3], 1, [50.0, 0.0, 0.0], [50.0, 0.0, 10.0]);
        link(&mut polys[3], 2, [50.0, 0.0, 20.0], [50.0, 0.0, 30.0]);
        let bake = LayeredBake::bake(&polys, &[], LayeredBakeSettings::default());
        let mut state = LayerState::new(&bake);
        let agent = LayerAgent::for_bake(&bake);
        let mut scratch = LayerScratch::default();
        let (a, b) = ([5_000, 0, 28_000], [55_000, 0, 28_000]);
        let open = bake.plan(&state, a, b, &agent, &mut scratch).expect("plans");
        let north = bake
            .polys
            .iter()
            .position(|p| p.section == 7)
            .expect("section 7 polygon") as u32;
        assert!(open.polys.contains(&north));
        let flipped = state.sync(
            &bake,
            &LayerSync {
                generation: 1,
                closed_sections: &[7],
                dirty_rects: &[],
            },
        );
        assert_eq!(flipped, vec![north]);
        let shut = bake.plan(&state, a, b, &agent, &mut scratch).expect("plans");
        assert!(!shut.polys.contains(&north));
        assert_eq!(shut.generation, 1);
        state.sync(
            &bake,
            &LayerSync {
                generation: 2,
                closed_sections: &[],
                dirty_rects: &[],
            },
        );
        assert_eq!(
            bake.plan(&state, a, b, &agent, &mut scratch)
                .expect("plans")
                .polys,
            open.polys
        );
    }

    #[test]
    fn dirty_rects_mark_their_tiles_in_order() {
        let bake = LayeredBake::bake(&strip(10, 20.0, 0), &[], LayeredBakeSettings::default());
        assert_eq!((bake.tiles_x, bake.tiles_z), (4, 1));
        let mut state = LayerState::new(&bake);
        let rects = [
            RectMm {
                x0: 70_000,
                z0: 1_000,
                x1: 71_000,
                z1: 2_000,
            },
            RectMm {
                x0: 1,
                z0: 1,
                x1: 2,
                z1: 2,
            },
        ];
        state.sync(
            &bake,
            &LayerSync {
                generation: 3,
                closed_sections: &[],
                dirty_rects: &rects,
            },
        );
        assert_eq!(state.dirty_tiles(), &[0, 2]);
        let polys = bake.polys_in_tiles(state.dirty_tiles());
        assert!(polys.contains(&0) && polys.contains(&7));
        let agent = LayerAgent::for_bake(&bake);
        let plan = bake
            .plan(
                &state,
                [5_000, 0, 5_000],
                [95_000, 0, 5_000],
                &agent,
                &mut LayerScratch::default(),
            )
            .expect("plans");
        assert!(plan.crosses_dirty);
        state.clear_dirty(&[0, 2]);
        assert!(state.dirty_tiles().is_empty());
    }

    #[test]
    fn links_respect_the_exclude_flags() {
        let polys = vec![
            quad(0.0, 0.0, 10.0, 10.0, 0.0, 0),
            quad(0.0, 0.0, 10.0, 10.0, 15.0, 0),
        ];
        let links = [LayerLinkInput {
            start: [5.0, 0.0, 5.0],
            end: [5.0, 15.0, 5.0],
            bidirectional: true,
            flags: 256,
        }];
        let bake = LayeredBake::bake(&polys, &links, LayeredBakeSettings::default());
        assert_eq!(bake.layer_count(), 2);
        let state = LayerState::new(&bake);
        let mut agent = LayerAgent::for_bake(&bake);
        let mut scratch = LayerScratch::default();
        let plan = bake
            .plan(
                &state,
                [2_000, 0, 2_000],
                [8_000, 15_000, 8_000],
                &agent,
                &mut scratch,
            )
            .expect("plans");
        assert_eq!(plan.layers.first(), Some(&1));
        assert_eq!(plan.goal_layer(), 0);
        agent.exclude_flags = 256;
        assert_eq!(
            bake.plan(
                &state,
                [2_000, 0, 2_000],
                [8_000, 15_000, 8_000],
                &agent,
                &mut scratch
            ),
            Err(LayerPlanError::Unreachable)
        );
        assert_eq!(LayerPlanError::Unreachable.code(), "plan_unreachable");
    }
}
