// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/search.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-882 (tests 883-898 in search/tests.rs), and ir-sim/src/pathing/mod.rs lines 57-70 (`PATH_TICK_BUDGET`,
// `path_tick_budget_for`), mark: adapt (Stage A: unchanged arithmetic).
// Edits: module paths (S-01's `det::grid`, sqrt through `regia_detsim::math::sqrt_f64`), the profile index through
// `GridSettings::profile_index`, the tunnel layer passed in by the host (`plan_layered`'s `layer` argument; the
// source read it from the grid), the pilot-only crawlway arms dropped (S-01's grid has no crawlway graph; with an
// empty graph the source runs none of them, so node ids, scratch lengths and neighbor order are the source's),
// doc comments (game names dropped, missing docs added).

//! Two-level A*: regions per 40 m cluster for the long haul, then a cell search inside the corridor
//! of clusters the coarse route crosses.
//!
//! Every search finishes inside the call that starts it (no search state carries across ticks), and
//! its work is bounded: the corridor caps the cell search, and a cap on expansions falls back to the
//! coarse route. Ties break on `(f, h, index)` so two peers pop the same node.
//!
//! With a [`TunnelLayer`] the graph has a second layer: tunnel nodes joined to surface cells at the
//! portals, a tunnel meter costing a share (`tunnel_pct`) of an open one, and a fixed
//! [`PORTAL_COST`] per portal so a route does not dip in and out.
//!
//! ```
//! use std::sync::Arc;
//! use regia_nav_det::grid::{GridSettings, NavGrid, NavStatic, Profile};
//! use regia_nav_det::search::{self, PlanKind, Scratch};
//!
//! # fn main() -> Result<(), regia_nav_det::grid::GridError> {
//! let settings = Arc::new(GridSettings::hover_ground_jet());
//! let grid = NavGrid::new(Arc::new(NavStatic::build(None, settings)?))?;
//! let mut scratch = Scratch::default();
//! let plan = search::plan(&grid, &mut scratch, Profile::HOVER, [-100.0, 0.0], [100.0, 20.0])
//!     .expect("open ground has a route");
//! assert_eq!(plan.kind, PlanKind::Full);
//! assert_eq!(plan.waypoints.first(), Some(&[-100.0, 0.0]));
//! assert_eq!(plan.waypoints.last(), Some(&[100.0, 20.0]));
//! # Ok(())
//! # }
//! ```

use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};

use regia_detsim::math;

use super::grid::{COST_FLAT, NEIGHBOURS_8, NavGrid, Profile};
use super::layer::{PORTAL_COST, TUNNEL_COST_PCT, TunnelLayer, octile_m};
use super::region_graph::RegionGraph;

/// Cell expansions one request may spend before it falls back.
pub const MAX_FINE_EXPANSIONS: u32 = 12_000;

/// Fine-search cap for a grid of `width` by `height` cells. A grid no larger than the 256 x 256
/// diagonal (including every smaller grid) returns [`MAX_FINE_EXPANSIONS`] exactly, with no float.
/// A larger grid scales with its diagonal and never shrinks below that constant. A square uses
/// `side / 256` in integer ceil (410 -> 19219).
pub fn fine_expansions_for(width: usize, height: usize) -> u32 {
    let w = width as u64;
    let h = height as u64;
    let diag2 = w.saturating_mul(w).saturating_add(h.saturating_mul(h));
    let alpha2 = 256u64 * 256 + 256 * 256;
    if diag2 <= alpha2 {
        return MAX_FINE_EXPANSIONS;
    }
    if w == h {
        let cap = (MAX_FINE_EXPANSIONS as u64).saturating_mul(w).div_ceil(256);
        return (cap as u32).max(MAX_FINE_EXPANSIONS);
    }
    let scaled = (MAX_FINE_EXPANSIONS as f64) * math::sqrt_f64((diag2 as f64) / (alpha2 as f64));
    let scaled = scaled.ceil() as u32;
    scaled.max(MAX_FINE_EXPANSIONS)
}

/// Cell expansions all path requests may spend in one tick (a host's per-tick queue budget).
pub const PATH_TICK_BUDGET: i64 = 6_000;

/// Tick budget for a grid. A grid whose fine cap is still [`MAX_FINE_EXPANSIONS`] returns
/// [`PATH_TICK_BUDGET`] exactly. A larger grid scales by the same ratio, in integer arithmetic.
pub fn path_tick_budget_for(width: usize, height: usize) -> i64 {
    let cap = fine_expansions_for(width, height);
    if cap == MAX_FINE_EXPANSIONS {
        PATH_TICK_BUDGET
    } else {
        (PATH_TICK_BUDGET as u64 * cap as u64 / MAX_FINE_EXPANSIONS as u64) as i64
    }
}

/// Rings searched for a free cell around a blocked start (150 m on 5 m cells).
pub const ESCAPE_RINGS: i32 = 30;
/// Rings searched for a free cell around a blocked goal (200 m on 5 m cells).
pub const SNAP_RINGS: i32 = 40;
/// String pulling never skips more than this many cells in one leg.
pub const MAX_SKIP: usize = 48;

/// How a plan ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlanKind {
    /// Cell path to the goal.
    #[default]
    Full,
    /// The goal is in another region set; the plan ends at the nearest reachable point.
    Partial,
    /// The cell search ran out; straight legs between region centers.
    Coarse,
}

/// A planned route.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// World XZ waypoints. The first is the requester's position.
    pub waypoints: Vec<[f32; 2]>,
    /// How the plan ends.
    pub kind: PlanKind,
    /// The goal sat in a blocked cell and was moved to the nearest free one.
    pub snapped: bool,
    /// Work spent (cell expansions plus a share for the coarse search).
    pub work: u32,
    /// Cell (and tunnel node) expansions of the fine search.
    pub fine_expansions: u32,
    /// Region (and tunnel node) expansions of the coarse search.
    pub coarse_expansions: u32,
    /// Node ids on the unsmoothed path: surface cells below width times height, tunnel nodes above.
    pub cells: Vec<u32>,
    /// Per waypoint: true where it is a tunnel node (underground).
    pub layers: Vec<bool>,
}

/// Search buffers. Not part of the game state: a clone starts empty.
#[derive(Debug, Default)]
pub struct Scratch {
    g: Vec<u32>,
    parent: Vec<u32>,
    seen: Vec<u32>,
    closed: Vec<u32>,
    stamp_id: u32,
    cg: Vec<u32>,
    cparent: Vec<u32>,
    cseen: Vec<u32>,
    cclosed: Vec<u32>,
    cgen: u32,
    corridor: Vec<u32>,
    corridor_gen: u32,
}

impl Clone for Scratch {
    fn clone(&self) -> Self {
        Scratch::default()
    }
}

impl Scratch {
    /// Buffer lengths, for a memory census (no behavior).
    #[doc(hidden)]
    pub fn leak_lens(&self) -> Vec<(&'static str, u64)> {
        vec![
            ("scratch_g", self.g.len() as u64),
            ("scratch_parent", self.parent.len() as u64),
            ("scratch_seen", self.seen.len() as u64),
            ("scratch_closed", self.closed.len() as u64),
            ("scratch_cg", self.cg.len() as u64),
            ("scratch_cparent", self.cparent.len() as u64),
            ("scratch_cseen", self.cseen.len() as u64),
            ("scratch_cclosed", self.cclosed.len() as u64),
            ("scratch_corridor", self.corridor.len() as u64),
        ]
    }
}

impl Scratch {
    fn ensure(&mut self, cells: usize, nodes: usize, clusters: usize) {
        if self.g.len() != cells {
            self.g = vec![0; cells];
            self.parent = vec![0; cells];
            self.seen = vec![0; cells];
            self.closed = vec![0; cells];
            self.stamp_id = 0;
        }
        if self.cg.len() != nodes {
            self.cg = vec![0; nodes];
            self.cparent = vec![0; nodes];
            self.cseen = vec![0; nodes];
            self.cclosed = vec![0; nodes];
            self.cgen = 0;
        }
        if self.corridor.len() != clusters {
            self.corridor = vec![0; clusters];
            self.corridor_gen = 0;
        }
    }

    fn next_gen(&mut self) -> u32 {
        self.stamp_id = self.stamp_id.wrapping_add(1);
        if self.stamp_id == 0 {
            self.seen.iter_mut().for_each(|v| *v = 0);
            self.closed.iter_mut().for_each(|v| *v = 0);
            self.stamp_id = 1;
        }
        self.stamp_id
    }

    fn next_cgen(&mut self) -> u32 {
        self.cgen = self.cgen.wrapping_add(1);
        if self.cgen == 0 {
            self.cseen.iter_mut().for_each(|v| *v = 0);
            self.cclosed.iter_mut().for_each(|v| *v = 0);
            self.cgen = 1;
        }
        self.cgen
    }

    fn next_corridor(&mut self) -> u32 {
        self.corridor_gen = self.corridor_gen.wrapping_add(1);
        if self.corridor_gen == 0 {
            self.corridor.iter_mut().for_each(|v| *v = 0);
            self.corridor_gen = 1;
        }
        self.corridor_gen
    }
}

/// Octile distance in tenths of a cell (10 straight, 14 diagonal).
#[inline]
pub fn octile(grid: &NavGrid, a: u32, b: u32) -> u32 {
    let (ax, az) = grid.xz(a);
    let (bx, b_z) = grid.xz(b);
    let dx = (ax as i64 - bx as i64).unsigned_abs() as u32;
    let dz = (az as i64 - b_z as i64).unsigned_abs() as u32;
    let (lo, hi) = if dx < dz { (dx, dz) } else { (dz, dx) };
    14 * lo + 10 * (hi - lo)
}

/// Plan a surface route for `profile` from `from` to `goal` (world XZ), with no tunnel layer.
/// [`plan_layered`] with `None` for the layer and both tunnel nodes, and [`TUNNEL_COST_PCT`].
pub fn plan(
    grid: &NavGrid,
    scratch: &mut Scratch,
    profile: Profile,
    from: [f32; 2],
    goal: [f32; 2],
) -> Option<Plan> {
    plan_layered(
        grid,
        None,
        scratch,
        profile,
        from,
        None,
        goal,
        None,
        TUNNEL_COST_PCT,
    )
}

/// Plan a route for `profile` from `from` to `goal` (world XZ) over the surface grid and, when
/// `layer` is `Some`, the tunnel layer (pass [`crate::layer::layer_for`]'s answer for the
/// profile). `from_tunnel` / `goal_tunnel` name the tunnel node a body underground stands on
/// ([`TunnelLayer::locate`]). `tunnel_pct` is the share of a tunnel meter's cost against open
/// ground (clamped to 10-100; [`TUNNEL_COST_PCT`] by default,
/// [`crate::layer::FLANK_COST_PCT`] for a flank).
///
/// `None` when no free cell lies within [`ESCAPE_RINGS`] of the start or [`SNAP_RINGS`] of the goal.
#[expect(
    clippy::too_many_arguments,
    reason = "the source's eight arguments plus the host's tunnel layer (ADR-8195): a struct would change every call site of the switch"
)]
pub fn plan_layered(
    grid: &NavGrid,
    layer: Option<&TunnelLayer>,
    scratch: &mut Scratch,
    profile: Profile,
    from: [f32; 2],
    from_tunnel: Option<u32>,
    goal: [f32; 2],
    goal_tunnel: Option<u32>,
    tunnel_pct: u32,
) -> Option<Plan> {
    let args = PlanArgs {
        layer,
        profile,
        from,
        from_tunnel,
        goal,
        goal_tunnel,
        tunnel_pct,
    };
    plan_core(grid, None, scratch, &args, None).map(|(plan, _)| plan)
}

/// One plan request's inputs (see [`plan_layered`]).
pub(crate) struct PlanArgs<'a> {
    pub(crate) layer: Option<&'a TunnelLayer>,
    pub(crate) profile: Profile,
    pub(crate) from: [f32; 2],
    pub(crate) from_tunnel: Option<u32>,
    pub(crate) goal: [f32; 2],
    pub(crate) goal_tunnel: Option<u32>,
    pub(crate) tunnel_pct: u32,
}

/// A coarse route kept for reuse: the node path and whether it reached the goal region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CoarseRoute {
    pub(crate) nodes: Vec<u32>,
    pub(crate) reached: bool,
}

/// Coarse routes by `(start region, goal region, profile index, grid generation)`. A sorted map:
/// nothing iterates a hash container.
pub(crate) type RouteCache = BTreeMap<(u32, u32, usize, u64), CoarseRoute>;

/// [`plan_layered`] with an optional precomputed region graph (the same neighbors, read from a
/// table instead of the cluster borders) and an optional route cache. Returns the plan and whether
/// its coarse route came from the cache. With neither, the bits are [`plan_layered`]'s.
pub(crate) fn plan_core(
    grid: &NavGrid,
    graph: Option<&RegionGraph>,
    scratch: &mut Scratch,
    args: &PlanArgs,
    cache: Option<&mut RouteCache>,
) -> Option<(Plan, bool)> {
    let PlanArgs {
        layer,
        profile,
        from,
        from_tunnel,
        goal,
        goal_tunnel,
        tunnel_pct,
    } = *args;
    let n_cells = grid.width() * grid.height();
    let clusters = grid.clusters_x * grid.clusters_z;
    let mr = grid.max_regions();
    let m = layer.map_or(0, |l| l.len());
    let from_tunnel = from_tunnel.filter(|t| (*t as usize) < m);
    let goal_tunnel = goal_tunnel.filter(|t| (*t as usize) < m);
    scratch.ensure(n_cells + m, clusters * mr + m, clusters);
    let n = n_cells as u32;
    let raw_start = grid.cell_at(from[0], from[1]);
    let start = match from_tunnel {
        Some(t) => n + t,
        None => grid.nearest_passable(profile, raw_start, ESCAPE_RINGS)?,
    };
    let raw_goal = grid.cell_at(goal[0], goal[1]);
    let mut target = match goal_tunnel {
        Some(t) => n + t,
        None => grid.nearest_passable(profile, raw_goal, SNAP_RINGS)?,
    };
    let snapped = goal_tunnel.is_none() && (target != raw_goal || !grid.in_bounds_world(goal[0], goal[1]));
    let mut out = Plan {
        snapped,
        ..Plan::default()
    };
    let g = Graph {
        grid,
        profile,
        pi: grid.settings().profile_index(profile),
        layer,
        n,
        m: m as u32,
        r: (clusters * mr) as u32,
        pct: tunnel_pct.clamp(10, 100),
        area_diag: area_diag(grid),
        k: grid.cluster_cells(),
        mr,
        graph: graph.filter(|rg| rg.fits(grid)),
    };

    let start_region = g.coarse_of(start);
    let goal_region = g.coarse_of(target);
    let mut coarse_nodes: Vec<u32> = Vec::new();
    let mut shared = false;
    if start_region != goal_region {
        let goal_pos = g.pos(target);
        let key = (start_region, goal_region, g.pi, grid.generation());
        let cached = cache.as_ref().and_then(|c| c.get(&key)).cloned();
        let (nodes, reached, spent) = match cached {
            Some(route) => {
                shared = true;
                (route.nodes, route.reached, 0)
            }
            None => {
                let found = coarse(&g, scratch, start_region, goal_region, goal_pos);
                if let Some(c) = cache {
                    c.insert(
                        key,
                        CoarseRoute {
                            nodes: found.0.clone(),
                            reached: found.1,
                        },
                    );
                }
                found
            }
        };
        out.coarse_expansions = spent;
        if !reached {
            // Nearest reachable point to the goal inside the closest region found.
            let last = *nodes.last().unwrap_or(&start_region);
            target = if last >= g.r {
                n + (last - g.r)
            } else {
                nearest_cell_in_region(&g, last, raw_goal)
            };
            out.kind = PlanKind::Partial;
        }
        coarse_nodes = nodes;
    } else {
        coarse_nodes.push(start_region);
    }

    let mut cells: Option<Vec<u32>> = None;
    let mut fine_spent = 0u32;
    let fine_cap = fine_expansions_for(grid.width(), grid.height());
    for dilate in [1usize, 3] {
        let budget = fine_cap.saturating_sub(fine_spent);
        if budget == 0 {
            break;
        }
        let stamp_id = mark_corridor(&g, scratch, &coarse_nodes, start, target, dilate);
        let (found, spent) = fine(&g, scratch, start, target, stamp_id, budget);
        fine_spent += spent;
        if found.is_some() {
            cells = found;
            break;
        }
    }
    out.fine_expansions = fine_spent;
    out.work = fine_spent + out.coarse_expansions * 4 + 16;

    match cells {
        Some(cells) => {
            let exact = !snapped && out.kind == PlanKind::Full && goal_tunnel.is_none();
            let escaped = from_tunnel.is_none() && raw_start != start;
            let (wps, layers) = smooth(&g, &cells, from, goal, escaped, exact);
            out.waypoints = wps;
            out.layers = layers;
            out.cells = cells;
        }
        None => {
            // Region centers and tunnel nodes in order: always a route when the coarse search found one.
            out.kind = if out.kind == PlanKind::Partial {
                PlanKind::Partial
            } else {
                PlanKind::Coarse
            };
            let mut wps = vec![from];
            let mut layers = vec![from_tunnel.is_some()];
            if from_tunnel.is_none() && start != raw_start {
                wps.push(grid.center(start));
                layers.push(false);
            }
            for node in &coarse_nodes {
                if *node >= g.r {
                    wps.push(g.pos(n + (node - g.r)));
                    layers.push(true);
                } else {
                    let (c, r) = g.split(*node);
                    if let Some(rep) = grid.regions[g.pi][c].reps.get(r) {
                        wps.push(grid.center(*rep));
                        layers.push(false);
                    }
                }
            }
            wps.push(g.pos(target));
            layers.push(target >= n);
            out.waypoints = wps;
            out.layers = layers;
        }
    }
    Some((out, shared))
}

/// The mixed graph: surface cells `0..n`, tunnel nodes `n..n+m`. Coarse regions `0..r`, then the
/// same tunnel nodes.
struct Graph<'a> {
    grid: &'a NavGrid,
    profile: Profile,
    /// The profile's index into the grid's per-profile tables.
    pi: usize,
    layer: Option<&'a TunnelLayer>,
    n: u32,
    /// Tunnel node count.
    m: u32,
    r: u32,
    /// Share (percent) of a tunnel meter's cost against open ground.
    pct: u32,
    /// The cell heuristic's area scale (KI-WD-031): a diagonal step's least cost on the cheapest area
    /// (`14 * c / COST_FLAT`, `c` the flat cell cost there), or `None` when no area is cheaper than flat.
    area_diag: Option<u32>,
    /// Cells per cluster side.
    k: usize,
    /// Most regions per cluster: a region node is `cluster * mr + label`.
    mr: usize,
    /// Region neighbors from a table, when the caller keeps one current.
    graph: Option<&'a RegionGraph>,
}

impl Graph<'_> {
    /// Cluster and local region of a region node.
    #[inline]
    fn split(&self, node: u32) -> (usize, usize) {
        (node as usize / self.mr, node as usize % self.mr)
    }

    #[inline]
    fn tunnel_cost(&self, raw: u32) -> u32 {
        (raw * self.pct / 100).max(1)
    }

    /// World XZ of a fine node.
    fn pos(&self, id: u32) -> [f32; 2] {
        if id < self.n {
            self.grid.center(id)
        } else {
            let t = &self.layer.expect("tunnel id without a layer").nodes[(id - self.n) as usize];
            [t.x, t.z]
        }
    }

    fn coarse_of(&self, id: u32) -> u32 {
        if id < self.n {
            node_of(self.grid, self.pi, id)
        } else {
            self.r + (id - self.n)
        }
    }

    fn coarse_pos(&self, node: u32) -> [f32; 2] {
        if node >= self.r {
            self.pos(self.n + (node - self.r))
        } else {
            self.grid.center(rep_of(self, node))
        }
    }

    fn h(&self, id: u32, goal: u32, goal_pos: [f32; 2]) -> u32 {
        let raw = if id < self.n && goal < self.n {
            octile(self.grid, id, goal)
        } else {
            let p = self.pos(id);
            octile_m(self.grid.cell_m(), p[0], p[1], goal_pos[0], goal_pos[1])
        };
        let h = self.scale_h(raw);
        // KI-WD-031: an area cheaper than flat ground lowers the least step cost; scale so the cell
        // heuristic never overestimates. Every straight step costs at least `c` and every diagonal at
        // least `area_diag` (`14 * c / 10`, floored), and `raw * area_diag / 14` is below both shares.
        match self.area_diag {
            Some(d) => h * d / 14,
            None => h,
        }
    }

    /// With a tunnel layer a meter can cost less than open ground: scale the heuristic down.
    #[inline]
    fn scale_h(&self, raw: u32) -> u32 {
        if self.layer.is_some() {
            raw * self.pct / 100
        } else {
            raw
        }
    }

    /// Portal cost between a surface cell and a tunnel node.
    fn portal_cost(&self, cell: u32, t: u32) -> u32 {
        let a = self.grid.center(cell);
        let b = self.pos(self.n + t);
        octile_m(self.grid.cell_m(), a[0], a[1], b[0], b[1]) + PORTAL_COST
    }

    /// Coarse neighbors with costs, fixed order.
    fn coarse_neighbours(&self, node: u32, nbrs: &mut Vec<u32>, out: &mut Vec<(u32, u32)>) {
        out.clear();
        let grid = self.grid;
        let profile = self.profile;
        if node < self.r {
            let rep = rep_of(self, node);
            let cost_here = region_cost(self, node);
            match self.graph {
                Some(rg) => {
                    nbrs.clear();
                    nbrs.extend_from_slice(rg.neighbours(self.pi, node));
                }
                None => region_neighbours(grid, profile, self.pi, node, nbrs),
            }
            for &next in nbrs.iter() {
                let next_rep = rep_of(self, next);
                let step = octile(grid, rep, next_rep).max(10) * (cost_here + region_cost(self, next))
                    / (2 * COST_FLAT);
                out.push((next, step));
            }
            if let Some(layer) = self.layer {
                for (t, cell) in &layer.portals {
                    if layer.is_closed(*t) {
                        continue;
                    }
                    if grid.passable(profile, *cell) && node_of(grid, self.pi, *cell) == node {
                        let p = self.pos(self.n + t);
                        let a = grid.center(rep);
                        let c = octile_m(grid.cell_m(), a[0], a[1], p[0], p[1]) + PORTAL_COST;
                        out.push((self.r + t, c));
                    }
                }
            }
        } else if node < self.r + self.m
            && let Some(layer) = self.layer
        {
            let t = node - self.r;
            if layer.is_closed(t) {
                return;
            }
            for (u, c) in &layer.adj[t as usize] {
                if layer.is_closed(*u) {
                    continue;
                }
                out.push((self.r + u, self.tunnel_cost(*c)));
            }
            if let Some(cell) = layer.portal_cell(t)
                && grid.passable(profile, cell)
            {
                out.push((node_of(grid, self.pi, cell), self.portal_cost(cell, t)));
            }
        }
    }
}

/// Region node of a cell: `cluster * max_regions + raw label` (a blocked cell's raw label is
/// past the cluster's regions, as in the source).
#[inline]
fn node_of(grid: &NavGrid, pi: usize, cell: u32) -> u32 {
    let label = grid.labels_ref(pi).raw(cell as usize) as usize;
    (grid.cluster_of(cell) * grid.max_regions() + label) as u32
}

fn rep_of(g: &Graph, node: u32) -> u32 {
    let (c, r) = g.split(node);
    g.grid.regions[g.pi][c].reps[r]
}

fn region_cost(g: &Graph, node: u32) -> u32 {
    let (c, r) = g.split(node);
    u32::from(g.grid.regions[g.pi][c].costs[r]).max(COST_FLAT)
}

/// Regions next to `node` across the four cluster borders, in a fixed order: direction E, W, S, N,
/// then border cell order, each region once. [`RegionGraph`] stores the same lists.
pub(crate) fn region_neighbours(grid: &NavGrid, profile: Profile, pi: usize, node: u32, out: &mut Vec<u32>) {
    out.clear();
    let (k_side, mr) = (grid.cluster_cells(), grid.max_regions());
    let (c, r) = (node as usize / mr, node as usize % mr);
    let cs = grid.clusters_x;
    let (cz_n, stride, w, h) = (grid.clusters_z, grid.width(), grid.width(), grid.height());
    let (ccx, ccz) = (c % cs, c / cs);
    let labels = grid.labels_ref(pi);
    let blocked = labels.blocked();
    // (neighbor cluster dx, dz)
    for (dx, dz) in [(1i32, 0i32), (-1, 0), (0, 1), (0, -1)] {
        let ncx = ccx as i32 + dx;
        let ncz = ccz as i32 + dz;
        if ncx < 0 || ncz < 0 || ncx >= cs as i32 || ncz >= cz_n as i32 {
            continue;
        }
        let nc = ncz as usize * cs + ncx as usize;
        for k in 0..k_side {
            let (ax, az, bx, b_z) = border_pair(ccx, ccz, k_side, k, dx, dz);
            if ax >= w || az >= h || bx >= w || b_z >= h {
                continue;
            }
            if labels.raw(az * stride + ax) != r as u32 {
                continue;
            }
            let lb = labels.raw(b_z * stride + bx);
            if lb == blocked || !grid.can_step(profile, (az * stride + ax) as u32, dx, dz) {
                continue;
            }
            let id = (nc * mr + lb as usize) as u32;
            if !out.contains(&id) {
                out.push(id);
            }
        }
    }
}

/// Border cell `k` of cluster `(ccx, ccz)` on side `(dx, dz)` and the cell across the border
/// (wrapping below zero, which the caller's bounds check drops).
#[inline]
pub(crate) fn border_pair(
    ccx: usize,
    ccz: usize,
    side: usize,
    k: usize,
    dx: i32,
    dz: i32,
) -> (usize, usize, usize, usize) {
    match (dx, dz) {
        (1, 0) => {
            let x = ccx * side + side - 1;
            let z = ccz * side + k;
            (x, z, x + 1, z)
        }
        (-1, 0) => {
            let x = ccx * side;
            let z = ccz * side + k;
            (x, z, x.wrapping_sub(1), z)
        }
        (0, 1) => {
            let x = ccx * side + k;
            let z = ccz * side + side - 1;
            (x, z, x, z + 1)
        }
        _ => {
            let x = ccx * side + k;
            let z = ccz * side;
            (x, z, x, z.wrapping_sub(1))
        }
    }
}

/// The cell heuristic's area scale (KI-WD-031, ADR-10070): with an area cheaper than flat ground
/// (`GridSettings::min_area_pct` under 100), a cell there costs at least
/// `c = max(1, (COST_FLAT * pct + 50) / 100)` (terrain is never below `COST_FLAT`), so a diagonal
/// step costs at least `14 * c / COST_FLAT`. `None` when that equals the flat 14, so grids without
/// cheap areas keep every plan bit.
fn area_diag(grid: &NavGrid) -> Option<u32> {
    let pct = u32::from(grid.settings().min_area_pct());
    let c = ((COST_FLAT * pct + 50) / 100).max(1);
    let diag = 14 * c / COST_FLAT;
    (diag < 14).then_some(diag)
}

/// A* over regions and tunnel nodes. Returns the node path (start first), whether it reached the
/// goal, and the expansions spent. Without a route the path ends at the reached node whose center is
/// nearest the goal.
fn coarse(
    g: &Graph,
    scratch: &mut Scratch,
    start: u32,
    goal: u32,
    goal_pos: [f32; 2],
) -> (Vec<u32>, bool, u32) {
    let stamp_id = scratch.next_cgen();
    let cell_m = g.grid.cell_m();
    let h_of = |node: u32| -> u32 {
        let p = g.coarse_pos(node);
        g.scale_h(octile_m(cell_m, p[0], p[1], goal_pos[0], goal_pos[1]))
    };
    let mut heap: BinaryHeap<Reverse<(u32, u32, u32)>> = BinaryHeap::new();
    let h0 = h_of(start);
    scratch.cg[start as usize] = 0;
    scratch.cseen[start as usize] = stamp_id;
    scratch.cparent[start as usize] = start;
    heap.push(Reverse((h0, h0, start)));
    let mut best = (h0, start);
    let mut spent = 0u32;
    let mut nbrs: Vec<u32> = Vec::with_capacity(16);
    let mut next_nodes: Vec<(u32, u32)> = Vec::with_capacity(16);
    let mut reached = false;
    while let Some(Reverse((_, h, node))) = heap.pop() {
        if scratch.cclosed[node as usize] == stamp_id {
            continue;
        }
        scratch.cclosed[node as usize] = stamp_id;
        spent += 1;
        if h < best.0 || (h == best.0 && node < best.1) {
            best = (h, node);
        }
        if node == goal {
            reached = true;
            best = (0, node);
            break;
        }
        let gc = scratch.cg[node as usize];
        g.coarse_neighbours(node, &mut nbrs, &mut next_nodes);
        for &(next, step) in &next_nodes {
            if scratch.cclosed[next as usize] == stamp_id {
                continue;
            }
            let ng = gc + step;
            if scratch.cseen[next as usize] == stamp_id && scratch.cg[next as usize] <= ng {
                continue;
            }
            scratch.cseen[next as usize] = stamp_id;
            scratch.cg[next as usize] = ng;
            scratch.cparent[next as usize] = node;
            let nh = if next == goal { 0 } else { h_of(next) };
            heap.push(Reverse((ng + nh, nh, next)));
        }
    }
    let mut path = vec![best.1];
    let mut at = best.1;
    while at != start {
        at = scratch.cparent[at as usize];
        path.push(at);
        if path.len() > 8192 {
            break;
        }
    }
    path.reverse();
    (path, reached, spent)
}

fn nearest_cell_in_region(g: &Graph, node: u32, toward: u32) -> u32 {
    let grid = g.grid;
    let (c, r) = g.split(node);
    let cs = grid.clusters_x;
    let (w, h) = (grid.width(), grid.height());
    let k = g.k;
    let (x0, z0) = ((c % cs) * k, (c / cs) * k);
    let labels = grid.labels_ref(g.pi);
    let mut best: Option<(u32, u32)> = None;
    for z in z0..(z0 + k).min(h) {
        for x in x0..(x0 + k).min(w) {
            let cell = grid.index(x, z);
            if labels.raw(cell as usize) != r as u32 {
                continue;
            }
            let d = octile(grid, cell, toward);
            if best.is_none_or(|(bd, bc)| d < bd || (d == bd && cell < bc)) {
                best = Some((d, cell));
            }
        }
    }
    best.map(|(_, cell)| cell).unwrap_or_else(|| rep_of(g, node))
}

/// Clusters the cell search may enter: the coarse route (plus the start and goal clusters, and the
/// surface side of every portal on the route), dilated by `dilate` clusters. Tunnel nodes are always
/// open.
fn mark_corridor(
    g: &Graph,
    scratch: &mut Scratch,
    nodes: &[u32],
    start: u32,
    goal: u32,
    dilate: usize,
) -> u32 {
    let stamp_id = scratch.next_corridor();
    let (cs, cz_n) = (g.grid.clusters_x as i32, g.grid.clusters_z as i32);
    let d = dilate as i32;
    let mut mark = |c: usize| {
        let (cx, cz) = ((c % cs as usize) as i32, (c / cs as usize) as i32);
        for dz in -d..=d {
            for dx in -d..=d {
                let (x, z) = (cx + dx, cz + dz);
                if x >= 0 && z >= 0 && x < cs && z < cz_n {
                    scratch.corridor[(z * cs + x) as usize] = stamp_id;
                }
            }
        }
    };
    for id in [start, goal] {
        if id < g.n {
            mark(g.grid.cluster_of(id));
        }
    }
    for node in nodes {
        if *node < g.r {
            mark(g.split(*node).0);
        } else {
            let idx = *node - g.r;
            if let Some(cell) = g.layer.and_then(|l| l.portal_cell(idx)) {
                mark(g.grid.cluster_of(cell));
            }
        }
    }
    stamp_id
}

/// A* over cells (inside the corridor) and tunnel nodes. Returns the node path and expansions.
fn fine(
    g: &Graph,
    scratch: &mut Scratch,
    start: u32,
    goal: u32,
    corridor_gen: u32,
    budget: u32,
) -> (Option<Vec<u32>>, u32) {
    let grid = g.grid;
    let profile = g.profile;
    let stamp_id = scratch.next_gen();
    let (w, h) = (grid.width() as i32, grid.height() as i32);
    let stride = w;
    let goal_pos = g.pos(goal);
    let mut heap: BinaryHeap<Reverse<(u32, u32, u32)>> = BinaryHeap::new();
    let h0 = g.h(start, goal, goal_pos);
    scratch.g[start as usize] = 0;
    scratch.seen[start as usize] = stamp_id;
    scratch.parent[start as usize] = start;
    heap.push(Reverse((h0, h0, start)));
    let mut spent = 0u32;
    let mut steps: Vec<(u32, u32)> = Vec::with_capacity(12);
    while let Some(Reverse((_, _, cell))) = heap.pop() {
        if scratch.closed[cell as usize] == stamp_id {
            continue;
        }
        scratch.closed[cell as usize] = stamp_id;
        spent += 1;
        if cell == goal {
            let mut path = vec![goal];
            let mut at = goal;
            while at != start {
                at = scratch.parent[at as usize];
                path.push(at);
            }
            path.reverse();
            return (Some(path), spent);
        }
        if spent >= budget {
            return (None, spent);
        }
        let gc = scratch.g[cell as usize];
        steps.clear();
        {
            let allowed = |c: u32| -> bool {
                scratch.corridor[grid.cluster_of(c)] == corridor_gen && grid.passable(profile, c)
            };
            if cell < g.n {
                let (x, z) = grid.xz(cell);
                let (x, z) = (x as i32, z as i32);
                for (dx, dz) in NEIGHBOURS_8 {
                    let (nx, nz) = (x + dx, z + dz);
                    if nx < 0 || nz < 0 || nx >= w || nz >= h {
                        continue;
                    }
                    let next = (nz * stride + nx) as u32;
                    if !allowed(next) || !grid.can_step(profile, cell, dx, dz) {
                        continue;
                    }
                    let diagonal = dx != 0 && dz != 0;
                    if diagonal {
                        // No corner cutting: both side cells and both side steps must be open.
                        let a = (z * stride + nx) as u32;
                        let b = (nz * stride + x) as u32;
                        if !allowed(a)
                            || !allowed(b)
                            || !grid.can_step(profile, cell, dx, 0)
                            || !grid.can_step(profile, cell, 0, dz)
                        {
                            continue;
                        }
                    }
                    let base = if diagonal { 14 } else { 10 };
                    steps.push((next, base * grid.cost(profile, next) / COST_FLAT));
                }
                if let Some(layer) = g.layer {
                    for t in layer.portals_at(cell) {
                        if !layer.is_closed(t) {
                            steps.push((g.n + t, g.portal_cost(cell, t)));
                        }
                    }
                }
            } else if cell < g.n + g.m
                && let Some(layer) = g.layer
            {
                let t = cell - g.n;
                if layer.is_closed(t) {
                    continue;
                }
                for (u, c) in &layer.adj[t as usize] {
                    if layer.is_closed(*u) {
                        continue;
                    }
                    steps.push((g.n + u, g.tunnel_cost(*c)));
                }
                if let Some(pc) = layer.portal_cell(t)
                    && allowed(pc)
                {
                    steps.push((pc, g.portal_cost(pc, t)));
                }
            }
        }
        for &(next, step) in &steps {
            if scratch.closed[next as usize] == stamp_id {
                continue;
            }
            let ng = gc + step;
            if scratch.seen[next as usize] == stamp_id && scratch.g[next as usize] <= ng {
                continue;
            }
            scratch.seen[next as usize] = stamp_id;
            scratch.g[next as usize] = ng;
            scratch.parent[next as usize] = cell;
            let nh = g.h(next, goal, goal_pos);
            heap.push(Reverse((ng + nh, nh, next)));
        }
    }
    (None, spent)
}

/// Waypoints from a node path. Surface runs are string-pulled (the farthest later cell in clear
/// sight, never across ground costlier than the path it replaces); tunnel runs keep every second
/// node and both ends. Returns the waypoints and a tunnel flag per waypoint.
fn smooth(
    g: &Graph,
    ids: &[u32],
    from: [f32; 2],
    goal: [f32; 2],
    escaped: bool,
    exact_goal: bool,
) -> (Vec<[f32; 2]>, Vec<bool>) {
    let grid = g.grid;
    let profile = g.profile;
    let mut out = vec![from];
    let mut layers = vec![ids.first().is_some_and(|id| *id >= g.n)];
    if ids.is_empty() {
        return (out, layers);
    }
    if escaped {
        // Leave the blocked start cell first, straight to the nearest open one.
        out.push(grid.center(ids[0]));
        layers.push(false);
    }
    let n = ids.len();
    let mut i = 0usize;
    while i < n {
        let tunnel = ids[i] >= g.n;
        let mut j = i;
        while j + 1 < n && (ids[j + 1] >= g.n) == tunnel {
            j += 1;
        }
        let run = &ids[i..=j];
        if tunnel {
            for (k, id) in run.iter().enumerate() {
                if k == 0 || k + 1 == run.len() || k % 2 == 0 {
                    out.push(g.pos(*id));
                    layers.push(true);
                }
            }
        } else {
            // The run's first cell joins the previous point (start or a portal).
            if i > 0 {
                out.push(grid.center(run[0]));
                layers.push(false);
            }
            let m = run.len();
            let mut anchor = 0usize;
            while anchor + 1 < m {
                let mut best = anchor + 1;
                let mut max_cost = grid
                    .cost(profile, run[anchor])
                    .max(grid.cost(profile, run[anchor + 1]));
                let mut k = anchor + 2;
                while k < m && k <= anchor + MAX_SKIP {
                    max_cost = max_cost.max(grid.cost(profile, run[k]));
                    if grid.los(profile, run[anchor], run[k], max_cost + 2, 0) {
                        best = k;
                    } else {
                        break;
                    }
                    k += 1;
                }
                out.push(grid.center(run[best]));
                layers.push(false);
                anchor = best;
            }
            if m == 1 && i == 0 {
                out.push(grid.center(run[0]));
                layers.push(false);
            }
        }
        i = j + 1;
    }
    if exact_goal && let Some(last) = out.last_mut() {
        *last = goal;
    }
    // Drop a first leg shorter than half a cell (the start sits on its cell).
    if !escaped && out.len() > 2 && layers[1] == layers[0] {
        let dx = out[1][0] - out[0][0];
        let dz = out[1][1] - out[0][1];
        if dx * dx + dz * dz < (grid.cell_m() * 0.5) * (grid.cell_m() * 0.5) {
            out.remove(1);
            layers.remove(1);
        }
    }
    (out, layers)
}

#[cfg(test)]
mod tests;
