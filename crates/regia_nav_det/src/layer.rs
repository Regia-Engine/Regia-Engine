// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/pathing/layer.rs, commit e6c84ba6d, tag regia-absorb-2), ADR-8012.
// Lines 1-379, mark: as-is (Stage A: unchanged).
// Edits: module paths (the tunnel description is `regia_world_geom::compat::state::MapTunnels`, the passage
// kind `compat::layout::PassageKind`, sqrt `regia_detsim::math::sqrt`), std float min/max/clamp through
// `regia_detsim::float::det_*` (the lockstep float policy, ADR-8085), the ramp-funnel mask read from S-01's
// renamed `NavStatic::lowered`, doc comments (game, unit and map names dropped, missing docs added), and
// `layer_for` (the source's `NavGrid::tunnel_for`, which read a size class the grid no longer names).

//! The tunnel nav layer: a graph on the sampled tunnel centerlines, joined to the surface grid at
//! the openings (portals).
//!
//! Node ids in a search: surface cells are `0..N` (N = grid width times height), tunnel nodes are
//! `N..N + M`. Edge costs use the same octile metric as the grid (tenths of a cell), so the grid's
//! octile heuristic stays admissible on the mixed graph.
//!
//! A host builds the layer once per map from its tunnel description ([`TunnelLayer::build`]), closes
//! the surface cells the layer's open cuts return (`NavGrid::with_closed_cells`), and passes the
//! layer to [`crate::search::plan_layered`] for the profiles that fit underground
//! ([`layer_for`]).

use std::sync::Arc;

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;
use regia_world_geom::compat::layout::PassageKind;
use regia_world_geom::compat::state::MapTunnels;

use super::grid::{NavGrid, NavStatic, Profile};

/// A body this close to a tunnel node (XZ, meters) can be on it.
pub const ATTACH_XZ_M: f32 = 14.0;
/// A body this close to a tunnel node's floor (Y, meters) can be on it.
pub const ATTACH_Y_M: f32 = 6.0;
/// Below the outdoor ground by this much is underground (not on the surface layer).
pub const UNDER_M: f32 = 3.0;
/// Extra cost (tenths of a cell) to pass a portal, so a route does not dip in and out.
pub const PORTAL_COST: u32 = 20;
/// Rock cover: a meter of tunnel costs this share (percent) of a meter of open, flat ground
/// ("shorter or safer"). The search heuristic is scaled by the same share where a tunnel layer
/// exists, so it stays admissible.
pub const TUNNEL_COST_PCT: u32 = 75;
/// A flanking route: tunnel meters cost this share (percent), so the route goes underground even
/// when the way through the tunnel is up to about half as long again as the open route.
pub const FLANK_COST_PCT: u32 = 20;
/// Surface cells this close to a ramp's open cut are closed on the surface layer (a drifting
/// hover hull needs the margin: one that strays into the trench drops in).
pub const CUT_MARGIN_M: f32 = 6.0;
/// Over a drawn mouth funnel (a baked ground patch, walkable) the cut keeps only this much beside
/// the ramp: the funnel slope is ground a body on foot or a hull can stand on and path across.
pub const FUNNEL_MARGIN_M: f32 = 1.0;
/// The ramp's footprint is closed on the surface up to this far short of the top edge; past
/// the top edge the ground is open. (A surface hull crossing the last meters of a ramp sideways
/// drops into the trench between its walls.)
pub const MOUTH_M: f32 = 1.0;
/// The closed shoulders beside the mouth reach this far past the top edge.
pub const SHOULDER_M: f32 = 7.0;
/// The closed shoulders beside the mouth are this wide.
pub const SHOULDER_WIDTH_M: f32 = 5.0;

/// The largest size class (index into `GridSettings::size_classes`) that fits a tunnel in the
/// `hover_ground_jet` preset: medium hulls and everything smaller go underground, the large class
/// goes round.
pub const PRESET_TUNNEL_SIZE_LIMIT: u8 = 1;

/// One tunnel node: a centerline sample.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TNode {
    /// World X (m).
    pub x: f32,
    /// Floor height (m).
    pub y: f32,
    /// World Z (m).
    pub z: f32,
    /// Index of the passage in the sampled layout.
    pub passage: u16,
    /// True on a ramp passage.
    pub ramp: bool,
    /// Half the passage width (m).
    pub half: f32,
    /// Mesh-layer level (0 surface, negative below). Single-system bakes leave every tunnel node at
    /// -1; a multi-level bake assigns per-node levels through [`TunnelLayer::set_levels`].
    pub level: i8,
}

/// The tunnel nav layer, with a dynamic closed set: a host closes nodes mid-match (for example
/// collapse rubble) and the search routes round them. The closed list is replaced whole, in sorted
/// order, so every peer in lockstep holds the same set.
///
/// The closed set sits behind a `Mutex` (interior mutability) so a host can close nodes on a layer
/// shared by grid clones (`Arc<TunnelLayer>`) without rebuilding it, exactly as the source does. A
/// later Stage B row (W-13, blockers) replaces it with an explicit input.
#[derive(Debug)]
pub struct TunnelLayer {
    /// The tunnel nodes, passage by passage in description order.
    pub nodes: Vec<TNode>,
    /// Neighbors with costs (tenths of a cell), ascending node id.
    pub adj: Vec<Vec<(u32, u32)>>,
    /// (tunnel node, surface cell) at each opening, ascending node id.
    pub portals: Vec<(u32, u32)>,
    /// Junction nodes (where passages join), for reports and tests.
    pub junctions: Vec<u32>,
    /// The grid's cell size (m) the costs were measured in.
    pub cell_m: f32,
    closed: std::sync::Mutex<Vec<u32>>,
}

impl Clone for TunnelLayer {
    fn clone(&self) -> Self {
        Self {
            nodes: self.nodes.clone(),
            adj: self.adj.clone(),
            portals: self.portals.clone(),
            junctions: self.junctions.clone(),
            cell_m: self.cell_m,
            closed: std::sync::Mutex::new(self.closed()),
        }
    }
}

impl PartialEq for TunnelLayer {
    fn eq(&self, other: &Self) -> bool {
        self.nodes == other.nodes
            && self.adj == other.adj
            && self.portals == other.portals
            && self.junctions == other.junctions
            && self.cell_m == other.cell_m
            && self.closed() == other.closed()
    }
}

/// Octile distance between two world points in tenths of a cell (floored).
#[inline]
pub fn octile_m(cell_m: f32, ax: f32, az: f32, bx: f32, b_z: f32) -> u32 {
    let dx = (ax - bx).abs() / cell_m;
    let dz = (az - b_z).abs() / cell_m;
    let (lo, hi) = if dx < dz { (dx, dz) } else { (dz, dx) };
    (14.0 * lo + 10.0 * (hi - lo)) as u32
}

/// The tunnel layer `profile` may use: `layer` when the profile's size class is at most
/// `size_limit` (the source's rule: hulls up to the medium class and bodies on foot fit, the large
/// class does not; [`PRESET_TUNNEL_SIZE_LIMIT`] for the preset), else `None`.
pub fn layer_for<'a>(
    grid: &NavGrid,
    layer: Option<&'a TunnelLayer>,
    profile: Profile,
    size_limit: u8,
) -> Option<&'a TunnelLayer> {
    if grid.settings().profile(profile).size_class > size_limit {
        return None;
    }
    layer
}

impl TunnelLayer {
    /// The layer and the surface cells its open cuts close (1 = closed, one byte per grid cell).
    pub fn build(map: &MapTunnels, stat: &NavStatic) -> (TunnelLayer, Vec<u8>) {
        let cell_m = stat.cell_m;
        let stride = stat.width;
        let (w, h) = (stat.width as i64, stat.height as i64);
        let mut nodes: Vec<TNode> = Vec::new();
        let mut ranges: Vec<(usize, usize)> = Vec::new();
        for (pi, p) in map.sampled.passages.iter().enumerate() {
            let start = nodes.len();
            for s in &p.samples {
                nodes.push(TNode {
                    x: s.p[0],
                    y: s.p[1],
                    z: s.p[2],
                    passage: pi as u16,
                    ramp: p.kind == PassageKind::Ramp,
                    half: p.width_m * 0.5,
                    level: -1,
                });
            }
            ranges.push((start, nodes.len()));
        }
        let mut adj: Vec<Vec<(u32, u32)>> = vec![Vec::new(); nodes.len()];
        // Raw length cost; the search applies the tunnel share (TUNNEL_COST_PCT, or a flank's).
        let cost = |a: &TNode, b: &TNode| octile_m(cell_m, a.x, a.z, b.x, b.z).max(1);
        let link = |adj: &mut Vec<Vec<(u32, u32)>>, a: usize, b: usize, c: u32| {
            if a == b || adj[a].iter().any(|(n, _)| *n == b as u32) {
                return;
            }
            adj[a].push((b as u32, c));
            adj[b].push((a as u32, c));
        };
        for &(start, end) in &ranges {
            for i in start..end.saturating_sub(1) {
                let c = cost(&nodes[i], &nodes[i + 1]);
                link(&mut adj, i, i + 1, c);
            }
        }
        // Junctions: a passage end that opens into another passage joins its nearest node.
        let mut junctions = Vec::new();
        for (pi, p) in map.sampled.passages.iter().enumerate() {
            let (start, end) = ranges[pi];
            if end <= start {
                continue;
            }
            let is_top = map.layout.openings.iter().any(|o| o.ramp == p.id);
            let ends = [(start, p.cap_start), (end - 1, p.cap_end || is_top)];
            for (i, closed) in ends {
                if closed {
                    continue;
                }
                let me = nodes[i];
                let mut best: Option<(f32, usize)> = None;
                for (j, other) in nodes.iter().enumerate() {
                    if other.passage as usize == pi || (other.y - me.y).abs() > 4.0 {
                        continue;
                    }
                    let dx = other.x - me.x;
                    let dz = other.z - me.z;
                    let d = dx * dx + dz * dz;
                    if d <= 15.0 * 15.0 && best.is_none_or(|(bd, bj)| d < bd || (d == bd && j < bj)) {
                        best = Some((d, j));
                    }
                }
                if let Some((_, j)) = best {
                    let c = cost(&nodes[i], &nodes[j]);
                    link(&mut adj, i, j, c);
                    junctions.push(j as u32);
                }
            }
        }
        for row in &mut adj {
            row.sort_unstable();
        }
        junctions.sort_unstable();
        junctions.dedup();
        let cell_of = |x: f32, z: f32| -> u32 {
            let fx = ((x - stat.origin_x) / cell_m + 0.5).floor() as i64;
            let fz = ((z - stat.origin_z) / cell_m + 0.5).floor() as i64;
            let cx = fx.clamp(0, w - 1) as usize;
            let cz = fz.clamp(0, h - 1) as usize;
            (cz * stride + cx) as u32
        };
        let mut portals = Vec::new();
        for o in &map.layout.openings {
            let Some(pi) = map.sampled.passages.iter().position(|p| p.id == o.ramp) else {
                continue;
            };
            let (start, end) = ranges[pi];
            if end <= start {
                continue;
            }
            // The surface cell just past the top, along the ramp.
            let top = nodes[end - 1];
            let below = nodes[end.saturating_sub(2).max(start)];
            let (dx, dz) = (top.x - below.x, top.z - below.z);
            let len = det_max(math::sqrt(dx * dx + dz * dz), 1e-3);
            let cell = cell_of(top.x + dx / len * cell_m, top.z + dz / len * cell_m);
            portals.push(((end - 1) as u32, cell));
        }
        portals.sort_unstable();
        // Open cuts: surface cells over an uncovered ramp section, except at the mouth.
        let mut cut = vec![0u8; stat.width * stat.height];
        for (pi, p) in map.sampled.passages.iter().enumerate() {
            if p.kind != PassageKind::Ramp {
                continue;
            }
            let top = p.samples.last().map(|s| s.p).unwrap_or([0.0; 3]);
            let below = p
                .samples
                .get(p.samples.len().saturating_sub(2))
                .map(|s| s.p)
                .unwrap_or(top);
            let (ux, uz) = {
                let (dx, dz) = (top[0] - below[0], top[2] - below[2]);
                let l = det_max(math::sqrt(dx * dx + dz * dz), 1e-3);
                (dx / l, dz / l)
            };
            let half = p.width_m * 0.5 + CUT_MARGIN_M;
            for k in 0..p.samples.len().saturating_sub(1) {
                if p.covered.get(k).copied().unwrap_or(true) {
                    continue;
                }
                let a = p.samples[k].p;
                let b = p.samples[k + 1].p;
                let (x0, x1) = (det_min(a[0], b[0]) - half, det_max(a[0], b[0]) + half);
                let (z0, z1) = (det_min(a[2], b[2]) - half, det_max(a[2], b[2]) + half);
                let c0 = cell_of(x0, z0) as usize;
                let c1 = cell_of(x1, z1) as usize;
                let (cx0, cz0, cx1, cz1) = (c0 % stride, c0 / stride, c1 % stride, c1 / stride);
                for cz in cz0..=cz1 {
                    for cx in cx0..=cx1 {
                        let wx = stat.origin_x + cx as f32 * cell_m;
                        let wz = stat.origin_z + cz as f32 * cell_m;
                        let abx = b[0] - a[0];
                        let abz = b[2] - a[2];
                        let l2 = abx * abx + abz * abz;
                        let t = if l2 < 1e-6 {
                            0.0
                        } else {
                            det_clamp(((wx - a[0]) * abx + (wz - a[2]) * abz) / l2, 0.0, 1.0)
                        };
                        let qx = a[0] + abx * t - wx;
                        let qz = a[2] + abz * t - wz;
                        if qx * qx + qz * qz > half * half {
                            continue;
                        }
                        let near = p.width_m * 0.5 + FUNNEL_MARGIN_M;
                        if stat.lowered.get(cz * stride + cx).copied().unwrap_or(0) != 0
                            && qx * qx + qz * qz > near * near
                        {
                            continue;
                        }
                        // Past the top edge (less MOUTH_M) the ground stays open.
                        if (wx - top[0]) * ux + (wz - top[2]) * uz > -MOUTH_M {
                            continue;
                        }
                        cut[cz * stride + cx] = 1;
                    }
                }
            }
            // Shoulders: beside the mouth lane, out to SHOULDER_M past the top edge, so a route
            // that crosses the mouth sideways passes well clear of the trench's last meters.
            let lane = p.width_m * 0.5;
            let reach = lane + SHOULDER_WIDTH_M;
            let r = reach + SHOULDER_M + cell_m;
            let c0 = cell_of(top[0] - r, top[2] - r) as usize;
            let c1 = cell_of(top[0] + r, top[2] + r) as usize;
            let (cx0, cz0, cx1, cz1) = (c0 % stride, c0 / stride, c1 % stride, c1 / stride);
            for cz in cz0..=cz1 {
                for cx in cx0..=cx1 {
                    let wx = stat.origin_x + cx as f32 * cell_m - top[0];
                    let wz = stat.origin_z + cz as f32 * cell_m - top[2];
                    let along = wx * ux + wz * uz;
                    let lateral = (wx * -uz + wz * ux).abs();
                    if (-MOUTH_M..=SHOULDER_M).contains(&along)
                        && lateral > lane - 1.0
                        && lateral <= reach
                        && stat.lowered.get(cz * stride + cx).copied().unwrap_or(0) == 0
                    {
                        cut[cz * stride + cx] = 1;
                    }
                }
            }
            let _ = pi;
        }
        // The portal cells themselves stay open.
        for (_, cell) in &portals {
            cut[*cell as usize] = 0;
        }
        (
            TunnelLayer {
                nodes,
                adj,
                portals,
                junctions,
                cell_m,
                closed: std::sync::Mutex::new(Vec::new()),
            },
            cut,
        )
    }

    /// Sorted closed node ids, cloned out.
    pub fn closed(&self) -> Vec<u32> {
        self.closed.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// True when `node` is closed: the search never enters, leaves or passes through it.
    pub fn is_closed(&self, node: u32) -> bool {
        self.closed
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .binary_search(&node)
            .is_ok()
    }

    /// Replace the closed set (sorted, deduped). A host passes its whole current list here after
    /// every change (a collapse, a clearing).
    pub fn set_closed(&self, mut ids: Vec<u32>) {
        ids.sort_unstable();
        ids.dedup();
        *self.closed.lock().unwrap_or_else(|p| p.into_inner()) = ids;
    }

    /// Assign per-node mesh-layer levels (a multi-level bake). Unknown ids are ignored; the vec
    /// stays ascending for determinism.
    pub fn set_levels(&mut self, levels: &[(u32, i8)]) {
        for &(id, level) in levels {
            if let Some(n) = self.nodes.get_mut(id as usize) {
                n.level = level;
            }
        }
    }

    /// Tunnel node count.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// True without tunnel nodes.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The tunnel node a body at `pos` stands on, if it is underground (or on a ramp below the
    /// surface). `surface_y` is the outdoor ground under it.
    pub fn locate(&self, pos: [f32; 3], surface_y: f32) -> Option<u32> {
        let deep = pos[1] <= surface_y - UNDER_M;
        let mut best: Option<(f32, u32)> = None;
        for (i, n) in self.nodes.iter().enumerate() {
            let dx = n.x - pos[0];
            let dz = n.z - pos[2];
            let d = dx * dx + dz * dz;
            // On a ramp (open cut included) a body is on the tunnel layer when it is on the ramp's
            // floor; elsewhere only when it is below the outdoor ground.
            let fits = if n.ramp {
                d <= (n.half + 1.0) * (n.half + 1.0) && (pos[1] - n.y).abs() <= 3.0
            } else {
                deep && d <= ATTACH_XZ_M * ATTACH_XZ_M && (pos[1] - n.y).abs() <= ATTACH_Y_M
            };
            if !fits {
                continue;
            }
            if best.is_none_or(|(bd, bi)| d < bd || (d == bd && (i as u32) < bi)) {
                best = Some((d, i as u32));
            }
        }
        best.map(|(_, i)| i)
    }

    /// Portal nodes whose surface cell is `cell`.
    pub fn portals_at(&self, cell: u32) -> impl Iterator<Item = u32> + '_ {
        self.portals
            .iter()
            .filter(move |(_, c)| *c == cell)
            .map(|(n, _)| *n)
    }

    /// The surface cell of a portal node.
    pub fn portal_cell(&self, node: u32) -> Option<u32> {
        self.portals.iter().find(|(n, _)| *n == node).map(|(_, c)| *c)
    }
}

/// Shared handle so grid clones stay cheap.
pub type SharedLayer = Arc<TunnelLayer>;
