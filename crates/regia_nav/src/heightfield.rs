// SPDX-License-Identifier: MPL-2.0
//! Voxelisation (rasterisation) of level geometry into a span heightfield, and the walkable
//! surface it yields (`docs/specs/22-tier2.md` § T2.2).
//!
//! This is the first stage of the Recast-style pipeline: triangles are rasterised conservatively
//! (nav cell v3, ADR-8899: a cell takes every triangle whose XZ projection overlaps its square, so
//! a wall top or a plank thinner than `cell_size` is seen wherever it falls on the grid) into a 2D
//! grid of vertical spans, spans are merged per column, and each column's top span that is both
//! flat enough and has enough headroom becomes the walkable surface at that cell.

use regia_core::math::Vec3;

use crate::config::{NavAgentParams, NavMeshBuildParams};
use crate::mesh::CellRect;

/// One vertical span of solid geometry in a column: `[min_y, max_y]` plus the surface normal of
/// its top (an upward-facing vector used for the slope test).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Span {
    /// Bottom of the span (world y).
    pub min_y: f32,
    /// Top of the span (world y).
    pub max_y: f32,
    /// Upward-facing surface normal of the span top.
    pub normal: Vec3,
    /// The area id of the triangle that made the span top (ADR-2420; 0 when unpainted).
    pub area: u8,
    /// The top came only from triangles that overlap the cell without covering its centre
    /// (conservative coverage, ADR-8899). Erosion treats such a cell as a boundary for an agent
    /// with a radius, so the partly covered cells at a floor edge never widen the walkable area;
    /// a point-sized agent (radius 0) may stand on them.
    pub partial: bool,
}

/// A rasterised span heightfield over the XZ plane (Recast `rcHeightfield`).
#[derive(Debug, Clone)]
pub(crate) struct HeightField {
    /// World position of the grid's minimum (X) and minimum (Z) corner.
    pub origin: Vec3,
    /// Horizontal voxel size (metres).
    pub cell_size: f32,
    /// Number of columns along X.
    pub width: usize,
    /// Number of columns along Z.
    pub height: usize,
    /// Spans per column, row-major (`z * width + x`), sorted bottom-to-top and merged.
    pub columns: Vec<Vec<Span>>,
}

/// A compact, walkable-surface grid: for each cell, the world height of the top walkable span, or
/// `None` where the cell is not walkable (Recast `rcCompactHeightfield`).
#[derive(Debug, Clone)]
pub(crate) struct WalkableGrid {
    /// World position of the grid's minimum (X) / minimum (Z) corner.
    pub origin: Vec3,
    /// Horizontal voxel size (metres).
    pub cell_size: f32,
    /// Number of cells along X.
    pub width: usize,
    /// Number of cells along Z.
    pub height: usize,
    /// Top height of the walkable surface per cell (`z * width + x`).
    pub cells: Vec<Option<f32>>,
    /// Area id per cell (same order); empty means every cell is area 0.
    pub areas: Vec<u8>,
}

impl WalkableGrid {
    /// The area id of cell `index`.
    pub(crate) fn area_at(&self, index: usize) -> u8 {
        self.areas.get(index).copied().unwrap_or(0)
    }
}

impl HeightField {
    /// Rasterises `triangles` into a span heightfield.
    ///
    /// Conservative rasterisation (nav cell v3, ADR-8899; it replaced the centre sampling of
    /// ADR-7558): a triangle reaches every cell whose square its XZ projection overlaps with a
    /// positive area (the separating-axis test). A cell whose centre the triangle covers records
    /// the plane height at that centre, exactly as before; a cell it only overlaps records the
    /// height range of the triangle clipped to the cell square. Adjacent spans merge, and inside
    /// a merged span an overlap top replaces the centre-sampled top only when it is more than
    /// `cell_height` above it, so a flat floor and a walkable slope keep the heights centre
    /// sampling gave them while a thin wall top, plank or steep face is no longer missed. A top
    /// from overlap only is `Span::partial`, a boundary for the radius erosion.
    #[cfg(test)]
    pub(crate) fn rasterize(
        triangles: &[[Vec3; 3]],
        params: &NavMeshBuildParams,
        bounds_min: Vec3,
        bounds_max: Vec3,
    ) -> Self {
        Self::rasterize_with_areas(triangles, &[], params, bounds_min, bounds_max)
    }

    /// [`Self::rasterize`] with an area id per triangle (`triangle_areas[i]` for `triangles[i]`;
    /// an empty slice, or a missing entry, is area 0). A span top keeps its triangle's area.
    pub(crate) fn rasterize_with_areas(
        triangles: &[[Vec3; 3]],
        triangle_areas: &[u8],
        params: &NavMeshBuildParams,
        bounds_min: Vec3,
        bounds_max: Vec3,
    ) -> Self {
        let cell_size = params.cell_size;
        let cell_height = params.cell_height;
        let border = params.border_size;

        let min = Vec3::new(bounds_min.x - border, bounds_min.y, bounds_min.z - border);
        let max = Vec3::new(bounds_max.x + border, bounds_max.y, bounds_max.z + border);

        // An f32 span can overflow to infinity for finite vertices; measure it in f64 then.
        let span = |hi: f32, lo: f32| {
            let q = (hi - lo) / cell_size;
            let q = if q.is_finite() {
                f64::from(q)
            } else {
                (f64::from(hi) - f64::from(lo)) / f64::from(cell_size)
            };
            (q.ceil().max(1.0) as usize).saturating_add(1)
        };
        let width = span(max.x, min.x);
        let height = span(max.z, min.z);

        let window = CellRect {
            x0: 0,
            z0: 0,
            x1: width,
            z1: height,
        };
        Self::rasterize_window_with_areas(triangles, triangle_areas, min, cell_size, cell_height, window)
    }

    /// Rasterises `triangles` into the cells of `window` only, on the grid anchored at `origin`.
    /// Cell squares and centres come from global cell indices, so a window sees exactly the spans a full
    /// rasterisation puts in those cells (the region rebuild relies on it). The result's `width`
    /// and `height` are the window's and `origin` is the grid's: it is not a standalone grid.
    pub(crate) fn rasterize_window(
        triangles: &[[Vec3; 3]],
        origin: Vec3,
        cell_size: f32,
        cell_height: f32,
        window: CellRect,
    ) -> Self {
        Self::rasterize_window_with_areas(triangles, &[], origin, cell_size, cell_height, window)
    }

    /// [`Self::rasterize_window`] with an area id per triangle (see
    /// [`Self::rasterize_with_areas`]).
    pub(crate) fn rasterize_window_with_areas(
        triangles: &[[Vec3; 3]],
        triangle_areas: &[u8],
        origin: Vec3,
        cell_size: f32,
        cell_height: f32,
        window: CellRect,
    ) -> Self {
        let width = window.width();
        let height = window.z1 - window.z0;
        let mut raw: Vec<Vec<RawSpan>> = vec![Vec::new(); width * height];

        for (i, tri) in triangles.iter().enumerate() {
            let area = triangle_areas.get(i).copied().unwrap_or(0);
            rasterize_triangle(tri, area, &origin, cell_size, window, &mut raw);
        }

        let columns = raw
            .iter_mut()
            .map(|spans| merge_column(spans, cell_height))
            .collect();

        Self {
            origin,
            cell_size,
            width,
            height,
            columns,
        }
    }

    /// Computes the walkable surface: the top span of each column that is flat enough (slope
    /// within `agent.max_slope`), has `agent.height` of headroom above it, and is eroded away from
    /// walls and the grid border by [`NavAgentParams::erosion_radius`] (the radius, plus one metre
    /// each side for a hover hull) so the body fits without crossing a wall.
    pub(crate) fn walkable_grid(&self, agent: &NavAgentParams) -> WalkableGrid {
        let (mut cells, mut areas) = self.walkable_tops_with_areas(agent);
        self.drop_partial_tops(agent, agent.erosion_radius(), &mut cells, &mut areas);
        erode_walkable(
            &mut cells,
            self.width,
            self.height,
            self.cell_size,
            agent.erosion_radius(),
        );
        WalkableGrid {
            origin: self.origin,
            cell_size: self.cell_size,
            width: self.width,
            height: self.height,
            cells,
            areas,
        }
    }

    /// [`Self::walkable_tops`] plus the area of each walkable top. The areas are empty (every cell
    /// area 0) when no walkable top came from a painted triangle.
    pub(crate) fn walkable_tops_with_areas(&self, agent: &NavAgentParams) -> (Vec<Option<f32>>, Vec<u8>) {
        let min_normal_y = agent.min_normal_y();
        let mut cells = vec![None; self.width * self.height];
        let mut areas = Vec::new();
        for (i, spans) in self.columns.iter().enumerate() {
            if let Some(span) = walkable_top(spans, min_normal_y, agent.height) {
                cells[i] = Some(span.max_y);
                if span.area != 0 {
                    if areas.is_empty() {
                        areas = vec![0; self.width * self.height];
                    }
                    areas[i] = span.area;
                }
            }
        }
        (cells, areas)
    }

    /// Clears the walkable tops that are `Span::partial` (and their areas) when `radius` is
    /// positive: erosion then counts them as boundary cells, so conservative coverage never
    /// widens the surface an agent with a body stands on (ADR-8899). `cells` and `areas` (empty
    /// means all 0) are indexed like the columns.
    pub(crate) fn drop_partial_tops(
        &self,
        agent: &NavAgentParams,
        radius: f32,
        cells: &mut [Option<f32>],
        areas: &mut [u8],
    ) {
        if radius <= 0.0 {
            return;
        }
        let min_normal_y = agent.min_normal_y();
        for (i, spans) in self.columns.iter().enumerate() {
            if walkable_top(spans, min_normal_y, agent.height).is_some_and(|s| s.partial) {
                cells[i] = None;
                if let Some(a) = areas.get_mut(i) {
                    *a = 0;
                }
            }
        }
    }

    /// How many walkable spans the fullest column holds, after the slope test, the headroom test
    /// and wall closure. Radius erosion is not applied: a span the agent could stand on still
    /// counts when a later inset would remove the cell. Used by the strict level cap.
    pub(crate) fn max_walkable_levels(&self, agent: &NavAgentParams, walls: &WallExtents) -> u16 {
        SpanGraph::build(self, agent, walls).max_column_spans()
    }

    /// The walkable surface before erosion: the top span of each column passing the slope and
    /// headroom tests.
    pub(crate) fn walkable_tops(&self, agent: &NavAgentParams) -> Vec<Option<f32>> {
        let min_normal_y = agent.min_normal_y();
        self.columns
            .iter()
            .map(|spans| walkable_top(spans, min_normal_y, agent.height).map(|s| s.max_y))
            .collect()
    }

    /// Every walkable span, as layers (ADR-8076). Layer 0 (`top`) is exactly
    /// [`Self::walkable_grid`]: the top walkable span of each column. The walkable spans under a
    /// column's top one (a tunnel floor under the ground, the ground under a bridge, the lower
    /// floor of a building) are eroded on the span graph and packed into `lower` layers, each a
    /// 2.5D grid with at most one surface per column, and `links` joins cells of different layers
    /// that an agent can step between. With no column holding two walkable spans, `lower` and
    /// `links` are empty and the result is the single-surface grid of before.
    ///
    /// Two spans in neighbouring columns connect when their heights differ by at most
    /// `agent.max_climb` and the open space above both overlaps by at least `agent.height`
    /// (the neighbour connection of Recast's compact heightfield). Lower spans are eroded by
    /// `agent.radius` over those connections: a missing connection is a wall. The packing floods
    /// connected spans into the first layer where they fit; a span fits a layer when its column
    /// is free there and no unconnected neighbour in that layer sits within `max_climb` of it
    /// (which the per-layer polygon builder would otherwise join). This is the layer partition of
    /// Recast's heightfield layers, described in Mononen's public Recast documentation; original
    /// code (`docs/provenance.md`).
    ///
    /// At most `max_layers` layers are kept (spans that fit none are dropped; 1 keeps `top` only).
    /// A lower span whose headroom a wall in `walls` crosses is not walkable, so a closed box's
    /// inside never joins the floor around it.
    pub(crate) fn walkable_layers(
        &self,
        agent: &NavAgentParams,
        max_layers: u16,
        walls: &WallExtents,
    ) -> WalkableLayers {
        let top = self.walkable_grid(agent);
        let mut out = WalkableLayers {
            top,
            lower: Vec::new(),
            links: Vec::new(),
        };
        if max_layers <= 1 {
            return out;
        }
        // A cheap scan first: a single-surface level allocates nothing more.
        let min_normal_y = agent.min_normal_y();
        let stacked = self.columns.iter().any(|spans| {
            (0..spans.len())
                .filter(|&i| span_is_walkable(spans, i, min_normal_y, agent.height))
                .nth(1)
                .is_some()
        });
        if !stacked {
            return out;
        }
        let graph = SpanGraph::build(self, agent, walls);
        let alive = graph.eroded_lower(self.cell_size, agent.erosion_radius());
        let (layer_of, occupancy) = graph.pack_layers(&alive, agent.max_climb, usize::from(max_layers - 1));
        if occupancy.is_empty() {
            return out;
        }
        let n = self.width * self.height;
        for occ in &occupancy {
            let mut cells = vec![None; n];
            let mut areas = Vec::new();
            for (col, &span) in occ.iter().enumerate() {
                if span == NO_SPAN {
                    continue;
                }
                let s = &graph.spans[span as usize];
                cells[col] = Some(s.y);
                if s.area != 0 {
                    if areas.is_empty() {
                        areas = vec![0; n];
                    }
                    areas[col] = s.area;
                }
            }
            out.lower.push(WalkableGrid {
                origin: self.origin,
                cell_size: self.cell_size,
                width: self.width,
                height: self.height,
                cells,
                areas,
            });
        }
        // Cross-layer links, in column order, towards +X then +Z.
        let layer_at = |span: u32| -> Option<u16> {
            let s = &graph.spans[span as usize];
            if s.is_top {
                out.top.cells[s.col as usize].is_some().then_some(0)
            } else {
                // Not `then_some(k + 1)`: that adds before the test and overflows on `NO_LAYER`.
                match layer_of[span as usize] {
                    NO_LAYER => None,
                    k => Some(k + 1),
                }
            }
        };
        let mut links = Vec::new();
        for (id, s) in graph.spans.iter().enumerate() {
            let Some(a_layer) = layer_at(id as u32) else {
                continue;
            };
            for dir in [DIR_PX, DIR_PZ] {
                let t = s.con[dir];
                if t == NO_SPAN {
                    continue;
                }
                let Some(b_layer) = layer_at(t) else {
                    continue;
                };
                if a_layer != b_layer {
                    links.push(LayerLink {
                        a_layer,
                        a_cell: s.col,
                        b_layer,
                        b_cell: graph.spans[t as usize].col,
                    });
                }
            }
        }
        out.links = links;
        out
    }
}

/// The walkable surface as layers (ADR-8076): see [`HeightField::walkable_layers`].
#[derive(Debug, Clone)]
pub(crate) struct WalkableLayers {
    /// Layer 0: the top walkable span of each column (the single-surface grid).
    pub top: WalkableGrid,
    /// Layers 1..: the walkable spans under the top ones, at most one per column per layer.
    pub lower: Vec<WalkableGrid>,
    /// Steps between 4-adjacent cells of different layers.
    pub links: Vec<LayerLink>,
}

impl WalkableLayers {
    /// Paints `volumes` over every layer (see `crate::areas::paint_grid`).
    pub(crate) fn paint(&mut self, volumes: &[crate::areas::NavAreaVolume], climb: f32) {
        crate::areas::paint_grid(&mut self.top, volumes, climb);
        for layer in &mut self.lower {
            crate::areas::paint_grid(layer, volumes, climb);
        }
    }
}

/// The vertical extents of the walls in each column (ADR-8076). The span rasterisation records
/// nothing for a vertical triangle, so its XZ segment is walked separately and every
/// cell it touches gets the triangle's whole height range (conservative: a gable's slanted top
/// blocks up to its peak). Only the layered build uses it; layer 0 ignores walls as before.
#[derive(Debug, Clone, Default)]
pub(crate) struct WallExtents {
    /// `(column, bottom, top)`, sorted.
    walls: Vec<(u32, f32, f32)>,
}

impl WallExtents {
    /// The walls of `triangles` (exactly those `rasterize_triangle` skips as vertical) over the
    /// grid of `field`.
    pub(crate) fn rasterize(triangles: &[[Vec3; 3]], field: &HeightField) -> Self {
        let mut walls = Vec::new();
        let cs = field.cell_size;
        for tri in triangles {
            let [a, b, c] = *tri;
            let normal = (b - a).cross(c - a);
            if normal.length() < 1e-12 || normal.y.abs() >= 1e-6 {
                continue;
            }
            let lo = a.y.min(b.y).min(c.y);
            let hi = a.y.max(b.y).max(c.y);
            // The XZ projection is (nearly) a segment: its two farthest-apart vertices.
            let d2 = |p: Vec3, q: Vec3| (p.x - q.x) * (p.x - q.x) + (p.z - q.z) * (p.z - q.z);
            let (p, q) = [(a, b), (b, c), (c, a)].into_iter().fold((a, a), |best, (p, q)| {
                if d2(p, q) > d2(best.0, best.1) {
                    (p, q)
                } else {
                    best
                }
            });
            // Sample every quarter cell; a sample outside the grid touches nothing.
            let steps = (d2(p, q).sqrt() / (cs * 0.25)).ceil();
            let steps = if steps.is_finite() {
                (steps as usize).min(1 << 20)
            } else {
                0
            };
            // A point on (or within a hair of) a cell edge marks the cells on both sides, so a
            // wall along a grid line closes the cells on either side of it.
            let eps = cs * 1e-3;
            for i in 0..=steps {
                let t = if steps == 0 { 0.0 } else { i as f32 / steps as f32 };
                let px = p.x + (q.x - p.x) * t - field.origin.x;
                let pz = p.z + (q.z - p.z) * t - field.origin.z;
                for (ox, oz) in [(-eps, -eps), (eps, -eps), (-eps, eps), (eps, eps)] {
                    let x = ((px + ox) / cs).floor();
                    let z = ((pz + oz) / cs).floor();
                    if !(x >= 0.0 && z >= 0.0 && (x as usize) < field.width && (z as usize) < field.height) {
                        continue;
                    }
                    walls.push(((z as usize * field.width + x as usize) as u32, lo, hi));
                }
            }
        }
        walls.sort_by(|l, r| l.0.cmp(&r.0).then(l.1.total_cmp(&r.1)).then(l.2.total_cmp(&r.2)));
        walls.dedup();
        Self { walls }
    }

    /// Whether a wall in column `col` overlaps the open height range `(lo, hi)`.
    fn crosses(&self, col: u32, lo: f32, hi: f32) -> bool {
        let start = self.walls.partition_point(|w| w.0 < col);
        self.walls[start..]
            .iter()
            .take_while(|w| w.0 == col)
            .any(|w| w.1 < hi && w.2 > lo)
    }
}

/// A step between two 4-adjacent cells in different layers (layer 0 is the top grid, layer `k`
/// is `lower[k - 1]`). `b_cell` is the `+X` or `+Z` neighbour of `a_cell`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LayerLink {
    /// The layer of `a_cell`.
    pub a_layer: u16,
    /// The grid index of the first cell.
    pub a_cell: u32,
    /// The layer of `b_cell`.
    pub b_layer: u16,
    /// The grid index of the `+X` or `+Z` neighbour.
    pub b_cell: u32,
}

/// No span.
const NO_SPAN: u32 = u32::MAX;
/// Not in a lower layer.
const NO_LAYER: u16 = u16::MAX;
/// Neighbour directions: -X, +X, -Z, +Z.
const DIR_NX: usize = 0;
const DIR_PX: usize = 1;
const DIR_NZ: usize = 2;
const DIR_PZ: usize = 3;

/// One walkable span of the span graph.
#[derive(Debug, Clone, Copy)]
struct WalkSpan {
    /// The column's grid index.
    col: u32,
    /// The surface height.
    y: f32,
    /// The bottom of the next span up (the ceiling), or infinity.
    ceil: f32,
    /// The area id of the surface.
    area: u8,
    /// Whether this is the column's top walkable span (layer 0's surface).
    is_top: bool,
    /// The span is `Span::partial`: a boundary for the radius erosion.
    partial: bool,
    /// The connected span in each neighbour column (`DIR_*` order), or `NO_SPAN`.
    con: [u32; 4],
}

/// Every walkable span and its neighbour connections.
struct SpanGraph {
    /// Columns along X.
    width: usize,
    /// Columns along Z.
    height: usize,
    spans: Vec<WalkSpan>,
}

impl SpanGraph {
    fn build(field: &HeightField, agent: &NavAgentParams, walls: &WallExtents) -> Self {
        let min_normal_y = agent.min_normal_y();
        let n = field.width * field.height;
        let mut first = vec![0u32; n + 1];
        let mut spans: Vec<WalkSpan> = Vec::new();
        for (col, column) in field.columns.iter().enumerate() {
            first[col] = spans.len() as u32;
            let start = spans.len();
            let top = (0..column.len())
                .rev()
                .find(|&i| span_is_walkable(column, i, min_normal_y, agent.height));
            for (idx, span) in column.iter().enumerate() {
                if !span_is_walkable(column, idx, min_normal_y, agent.height) {
                    continue;
                }
                // A wall through the headroom of a lower span closes it (the top span is layer
                // 0's, which keeps the single-surface rules).
                if Some(idx) != top
                    && walls.crosses(col as u32, span.max_y + 1e-4, span.max_y + agent.height - 1e-4)
                {
                    continue;
                }
                let ceil = column.get(idx + 1).map_or(f32::INFINITY, |above| above.min_y);
                spans.push(WalkSpan {
                    col: col as u32,
                    y: span.max_y,
                    ceil,
                    area: span.area,
                    is_top: false,
                    partial: span.partial,
                    con: [NO_SPAN; 4],
                });
            }
            if top.is_some()
                && let Some(last) = spans.get_mut(start..).and_then(|s| s.last_mut())
            {
                last.is_top = true;
            }
        }
        first[n] = spans.len() as u32;
        let (w, h) = (field.width, field.height);
        let mut cons = vec![[NO_SPAN; 4]; spans.len()];
        for (s, con) in spans.iter().zip(cons.iter_mut()) {
            let (x, z) = (s.col as usize % w, s.col as usize / w);
            for dir in [DIR_NX, DIR_PX, DIR_NZ, DIR_PZ] {
                let Some(nb) = neighbour_col(x, z, w, h, dir) else {
                    continue;
                };
                let mut best: Option<(f32, u32)> = None;
                for t in first[nb]..first[nb + 1] {
                    let o = &spans[t as usize];
                    let dy = (o.y - s.y).abs();
                    if dy > agent.max_climb + 1e-4 {
                        continue;
                    }
                    let gap = s.ceil.min(o.ceil) - s.y.max(o.y);
                    if gap + 1e-4 < agent.height {
                        continue;
                    }
                    if best.is_none_or(|(bd, _)| dy < bd) {
                        best = Some((dy, t));
                    }
                }
                if let Some((_, t)) = best {
                    con[dir] = t;
                }
            }
        }
        for (s, con) in spans.iter_mut().zip(cons) {
            s.con = con;
        }
        Self {
            width: w,
            height: h,
            spans,
        }
    }

    /// The most walkable spans any one column contributed. Spans are stored in column order.
    fn max_column_spans(&self) -> u16 {
        let mut max = 0u16;
        let mut i = 0;
        while i < self.spans.len() {
            let col = self.spans[i].col;
            let start = i;
            i += 1;
            while i < self.spans.len() && self.spans[i].col == col {
                i += 1;
            }
            let n = u16::try_from(i - start).unwrap_or(u16::MAX);
            if n > max {
                max = n;
            }
        }
        max
    }

    /// Which spans under a top span survive the agent-radius erosion. The boundary distance is
    /// counted over the span connections, as `erode_walkable` counts it over the grid: a span
    /// with a missing connection is 1 from a wall, and each connection step adds 1.
    fn eroded_lower(&self, cell_size: f32, radius: f32) -> Vec<bool> {
        let mut alive: Vec<bool> = self.spans.iter().map(|s| !s.is_top).collect();
        if radius <= 0.0 {
            return alive;
        }
        let mut dist = vec![u32::MAX; self.spans.len()];
        let mut queue = std::collections::VecDeque::new();
        // A partial span (conservative coverage only, ADR-8899) is a boundary, as a missing
        // cell is on the top layer: distance 0, seeded before the distance-1 spans.
        for (id, s) in self.spans.iter().enumerate() {
            if s.partial {
                dist[id] = 0;
                queue.push_back(id);
            }
        }
        for (id, s) in self.spans.iter().enumerate() {
            if dist[id] == u32::MAX && s.con.contains(&NO_SPAN) {
                dist[id] = 1;
                queue.push_back(id);
            }
        }
        while let Some(cur) = queue.pop_front() {
            let next = dist[cur] + 1;
            for &t in &self.spans[cur].con {
                if t != NO_SPAN && dist[t as usize] == u32::MAX {
                    dist[t as usize] = next;
                    queue.push_back(t as usize);
                }
            }
        }
        let threshold = radius / cell_size;
        for (id, keep) in alive.iter_mut().enumerate() {
            if *keep && (dist[id] as f32) < threshold {
                *keep = false;
            }
        }
        alive
    }

    /// Packs the `alive` spans into layers. Returns each span's lower-layer index (`NO_LAYER` when
    /// not packed) and each layer's occupancy (the span id per column, or `NO_SPAN`).
    fn pack_layers(&self, alive: &[bool], climb: f32, max_layers: usize) -> (Vec<u16>, Vec<Vec<u32>>) {
        let n_cols = self.width * self.height;
        let mut layer_of = vec![NO_LAYER; self.spans.len()];
        let mut occupancy: Vec<Vec<u32>> = Vec::new();
        let mut stack = Vec::new();
        for seed in 0..self.spans.len() {
            if !alive[seed] || layer_of[seed] != NO_LAYER {
                continue;
            }
            let k = match (0..occupancy.len()).find(|&k| self.fits(seed as u32, &occupancy[k], climb)) {
                Some(k) => k,
                None => {
                    if occupancy.len() >= max_layers.min(usize::from(NO_LAYER)) {
                        // Past the last layer: this surface is dropped.
                        continue;
                    }
                    occupancy.push(vec![NO_SPAN; n_cols]);
                    occupancy.len() - 1
                }
            };
            let occ = &mut occupancy[k];
            occ[self.spans[seed].col as usize] = seed as u32;
            layer_of[seed] = k as u16;
            stack.push(seed as u32);
            while let Some(cur) = stack.pop() {
                for t in self.spans[cur as usize].con {
                    if t == NO_SPAN || !alive[t as usize] || layer_of[t as usize] != NO_LAYER {
                        continue;
                    }
                    if self.fits(t, occ, climb) {
                        occ[self.spans[t as usize].col as usize] = t;
                        layer_of[t as usize] = k as u16;
                        stack.push(t);
                    }
                }
            }
        }
        (layer_of, occupancy)
    }

    /// Whether span `id` can join a layer with occupancy `occ`: its column is free, and every
    /// neighbour already in the layer is either connected to it or more than `climb` away in
    /// height (so the layer's own step test does not join unconnected surfaces).
    fn fits(&self, id: u32, occ: &[u32], climb: f32) -> bool {
        let s = &self.spans[id as usize];
        let col = s.col as usize;
        if occ.get(col).copied() != Some(NO_SPAN) {
            return false;
        }
        let (x, z) = (col % self.width, col / self.width);
        for (dir, &con) in s.con.iter().enumerate() {
            let Some(nb) = neighbour_col(x, z, self.width, self.height, dir) else {
                continue;
            };
            let other = occ[nb];
            if other == NO_SPAN || other == con {
                continue;
            }
            if (self.spans[other as usize].y - s.y).abs() <= climb + 1e-4 {
                return false;
            }
        }
        true
    }
}

/// The grid index of the neighbour of column `(x, z)` in direction `dir` (`DIR_*`), if inside.
fn neighbour_col(x: usize, z: usize, width: usize, height: usize, dir: usize) -> Option<usize> {
    let (nx, nz) = match dir {
        DIR_NX => (x.checked_sub(1)?, z),
        DIR_PX => (x + 1, z),
        DIR_NZ => (x, z.checked_sub(1)?),
        _ => (x, z + 1),
    };
    (nx < width && nz < height).then_some(nz * width + nx)
}

/// Whether span `idx` of a column passes the slope and headroom tests (as in `walkable_top`).
fn span_is_walkable(spans: &[Span], idx: usize, min_normal_y: f32, height: f32) -> bool {
    let span = &spans[idx];
    if span.normal.y < min_normal_y {
        return false;
    }
    let clearance = spans
        .get(idx + 1)
        .map_or(f32::INFINITY, |above| above.min_y - span.max_y);
    clearance + 1e-4 >= height
}

/// The highest span of a column passing the slope and headroom tests.
fn walkable_top(spans: &[Span], min_normal_y: f32, height: f32) -> Option<&Span> {
    // Walk top-down: the first (highest) walkable span is the surface.
    for (idx, span) in spans.iter().enumerate().rev() {
        let slope_ok = span.normal.y >= min_normal_y;
        if !slope_ok {
            continue;
        }
        let clearance = match spans.get(idx + 1) {
            // The span above's bottom is the headroom ceiling.
            Some(above) => above.min_y - span.max_y,
            None => f32::INFINITY,
        };
        if clearance + 1e-4 >= height {
            return Some(span);
        }
    }
    None
}

/// Erodes the walkable surface by `radius` (Recast's `rcErodeWalkableArea`): a walkable cell is
/// dropped when its boundary distance — the number of 4-connected cells to the nearest non-walkable
/// cell or the grid border — is less than `radius / cell_size`.
///
/// Cite: Mikko Mononen, Recast "build distance field" + "erode walkable area" (zlib), as described
/// in `docs/provenance.md`; the distance field is a simple breadth-first flood from non-walkable
/// cells and the border, no code copied.
pub(crate) fn erode_walkable(
    cells: &mut [Option<f32>],
    width: usize,
    height: usize,
    cell_size: f32,
    radius: f32,
) {
    if radius <= 0.0 {
        return;
    }
    let n = width * height;
    let mut dist = vec![u32::MAX; n];
    let mut queue = std::collections::VecDeque::new();

    // Seed non-walkable cells at distance 0.
    for z in 0..height {
        for x in 0..width {
            let i = z * width + x;
            if cells[i].is_none() {
                dist[i] = 0;
                queue.push_back(i);
            }
        }
    }
    // Seed walkable cells on the grid edge at distance 1 (one cell from the border).
    for z in 0..height {
        for x in 0..width {
            if x == 0 || x + 1 == width || z == 0 || z + 1 == height {
                let i = z * width + x;
                if dist[i] == u32::MAX {
                    dist[i] = 1;
                    queue.push_back(i);
                }
            }
        }
    }

    // Multi-source breadth-first flood: 4-connected distance to the nearest obstacle/border.
    while let Some(cur) = queue.pop_front() {
        let cx = cur % width;
        let cz = cur / width;
        let next = dist[cur] + 1;
        for (nx, nz) in [
            (cx.wrapping_sub(1), cz),
            (cx + 1, cz),
            (cx, cz.wrapping_sub(1)),
            (cx, cz + 1),
        ] {
            if nx >= width || nz >= height {
                continue;
            }
            let ni = nz * width + nx;
            if dist[ni] == u32::MAX {
                dist[ni] = next;
                queue.push_back(ni);
            }
        }
    }

    let threshold = radius / cell_size;
    for (i, cell) in cells.iter_mut().enumerate() {
        if cell.is_some() && (dist[i] as f32) < threshold {
            *cell = None;
        }
    }
}

/// One span as a triangle writes it, before the per-column merge: `centre` says whether the
/// triangle covered the cell centre (a centre sample) or only overlapped the cell square.
#[derive(Debug, Clone, Copy)]
struct RawSpan {
    min_y: f32,
    max_y: f32,
    normal: Vec3,
    area: u8,
    centre: bool,
}

/// The top a merged span takes from one kind of raw span: the highest top, ties keeping the
/// flatter normal (the greater y, so a steep face never displaces a flat one by arriving later)
/// and the higher area id, so the result does not depend on the triangle order (audit
/// sprint-audit-g-nav-determinism H3).
#[derive(Debug, Clone, Copy)]
struct Top {
    y: f32,
    normal: Vec3,
    area: u8,
}

impl Top {
    fn of(span: &RawSpan) -> Self {
        Self {
            y: span.max_y,
            normal: span.normal,
            area: span.area,
        }
    }

    fn fold(top: &mut Option<Self>, span: &RawSpan) {
        match top {
            None => *top = Some(Self::of(span)),
            Some(t) if span.max_y > t.y => *t = Self::of(span),
            Some(t) if span.max_y == t.y => {
                if span.normal.y > t.normal.y {
                    t.normal = span.normal;
                }
                t.area = t.area.max(span.area);
            }
            Some(_) => {}
        }
    }
}

/// Merges one column's raw spans into sorted, disjoint spans. Spans whose gap is at most
/// `cell_height / 2` join one span (bottom = the lowest bottom). Its top is the highest
/// centre-sampled top, unless an overlap top lies more than `cell_height` above it (or there is
/// no centre sample): then the highest overlap top. A coplanar neighbour triangle that overlaps a
/// cell another triangle's centre sample already measured therefore cannot raise the cell by its
/// slope across the cell (up to about 0.7 `cell_size` x slope), while geometry that stands a
/// voxel above the sampled surface (a thin wall top, a steep face) does.
fn merge_column(spans: &mut [RawSpan], cell_height: f32) -> Vec<Span> {
    /// One merged span being built: its bottom, the highest top of any member (the gap test's
    /// reference), and the best centre and overlap tops.
    struct Group {
        min_y: f32,
        reach: f32,
        centre: Option<Top>,
        overlap: Option<Top>,
    }
    impl Group {
        fn add(&mut self, span: &RawSpan) {
            self.reach = self.reach.max(span.max_y);
            Top::fold(
                if span.centre {
                    &mut self.centre
                } else {
                    &mut self.overlap
                },
                span,
            );
        }
        fn close(self, cell_height: f32) -> Option<Span> {
            let (top, partial) = match (self.centre, self.overlap) {
                (Some(c), Some(o)) if o.y > c.y + cell_height => (o, true),
                (Some(c), _) => (c, false),
                (None, Some(o)) => (o, true),
                (None, None) => return None,
            };
            Some(Span {
                min_y: self.min_y,
                max_y: top.y,
                normal: top.normal,
                area: top.area,
                partial,
            })
        }
    }

    if spans.is_empty() {
        return Vec::new();
    }
    spans.sort_by(|a, b| a.min_y.total_cmp(&b.min_y));
    let mut out: Vec<Span> = Vec::new();
    let mut group: Option<Group> = None;
    for span in spans.iter() {
        match &mut group {
            Some(g) if span.min_y - g.reach <= cell_height * 0.5 => g.add(span),
            _ => {
                if let Some(done) = group.take().and_then(|g| g.close(cell_height)) {
                    out.push(done);
                }
                let mut g = Group {
                    min_y: span.min_y,
                    reach: span.max_y,
                    centre: None,
                    overlap: None,
                };
                g.add(span);
                group = Some(g);
            }
        }
    }
    if let Some(done) = group.and_then(|g| g.close(cell_height)) {
        out.push(done);
    }
    out
}

/// The fraction of `cell_size` by which the separating-axis test shrinks a cell square, so a
/// triangle that only touches a cell's edge or corner (a floor edge on a grid line, give or take
/// rounding) does not reach the neighbouring cell.
const OVERLAP_SHRINK: f32 = 1e-4;

/// Rasterises one triangle into `columns` (the cells of `window`, row-major) conservatively: every
/// cell whose square the triangle's XZ projection overlaps gets a span (see
/// [`HeightField::rasterize_with_areas`]).
///
/// The overlap test is the separating-axis theorem for two convex polygons in the plane (the
/// cell's two axes and the triangle's three edge normals; Eberly, "Intersection of Convex Objects:
/// The Method of Separating Axes", Geometric Tools, and Akenine-Möller, "Fast 3D Triangle-Box
/// Overlap Testing", JGT 2001, reduced to 2D); the height range of an overlapped cell comes from
/// clipping the triangle to the cell square (Sutherland and Hodgman, "Reentrant Polygon
/// Clipping", CACM 1974). Original code (`docs/provenance.md`).
fn rasterize_triangle(
    tri: &[Vec3; 3],
    area: u8,
    origin: &Vec3,
    cell_size: f32,
    window: CellRect,
    columns: &mut [Vec<RawSpan>],
) {
    let [a, b, c] = *tri;

    // Upward-facing geometric normal (slope test needs it), skipping degenerate triangles.
    let mut normal = (b - a).cross(c - a);
    let area2 = normal.length();
    if area2 < 1e-12 || normal.y.abs() < 1e-6 {
        return;
    }
    normal /= area2;
    if normal.y < 0.0 {
        normal = -normal;
    }

    // The XZ bounding box in cell coordinates (expanded by one cell).
    let min_x = a.x.min(b.x).min(c.x);
    let max_x = a.x.max(b.x).max(c.x);
    let min_z = a.z.min(b.z).min(c.z);
    let max_z = a.z.max(b.z).max(c.z);
    // `as isize` saturates for a huge (finite) vertex, so the one-cell widening saturates too
    // instead of overflowing.
    let gx0 = (((min_x - origin.x) / cell_size).floor() as isize)
        .saturating_sub(1)
        .max(window.x0 as isize);
    let gx1 = (((max_x - origin.x) / cell_size).ceil() as isize)
        .saturating_add(1)
        .min(window.x1 as isize - 1);
    let gz0 = (((min_z - origin.z) / cell_size).floor() as isize)
        .saturating_sub(1)
        .max(window.z0 as isize);
    let gz1 = (((max_z - origin.z) / cell_size).ceil() as isize)
        .saturating_add(1)
        .min(window.z1 as isize - 1);
    // A triangle wholly outside the window: a negative end must not wrap to a huge cell range.
    if gx1 < gx0 || gz1 < gz0 {
        return;
    }
    let (gx0, gx1, gz0, gz1) = (gx0 as usize, gx1 as usize, gz0 as usize, gz1 as usize);

    // The triangle's three edge normals (XZ) and its extent along each, for the separating axes.
    let axes = [(a, b), (b, c), (c, a)].map(|(p, q)| {
        let (nx, nz) = (q.z - p.z, p.x - q.x);
        let d = [a, b, c].map(|v| nx * v.x + nz * v.z);
        (nx, nz, d[0].min(d[1]).min(d[2]), d[0].max(d[1]).max(d[2]))
    });
    let shrink = cell_size * OVERLAP_SHRINK;

    for z in gz0..=gz1 {
        for x in gx0..=gx1 {
            let px = origin.x + (x as f32 + 0.5) * cell_size;
            let pz = origin.z + (z as f32 + 0.5) * cell_size;
            let point = Vec3::new(px, 0.0, pz);
            let span = if point_in_triangle_xz(point, a, b, c) {
                let y = plane_height(px, pz, a, normal);
                // A (near-)vertical triangle has no well-defined height at a cell centre.
                if !y.is_finite() {
                    continue;
                }
                RawSpan {
                    min_y: y,
                    max_y: y,
                    normal,
                    area,
                    centre: true,
                }
            } else {
                let x0 = origin.x + x as f32 * cell_size;
                let x1 = origin.x + (x + 1) as f32 * cell_size;
                let z0 = origin.z + z as f32 * cell_size;
                let z1 = origin.z + (z + 1) as f32 * cell_size;
                let square = [x0 + shrink, x1 - shrink, z0 + shrink, z1 - shrink];
                if !overlaps_xz(&axes, [min_x, max_x, min_z, max_z], square) {
                    continue;
                }
                let Some((lo, hi)) = clipped_height_range(tri, [x0, x1, z0, z1]) else {
                    continue;
                };
                RawSpan {
                    min_y: lo,
                    max_y: hi,
                    normal,
                    area,
                    centre: false,
                }
            };
            columns[window.local(x, z)].push(span);
        }
    }
}

/// Whether a triangle (its edge axes `(nx, nz, min, max)` and its XZ box `[min_x, max_x, min_z,
/// max_z]`) and the square `[x0, x1, z0, z1]` overlap with a positive area: no separating axis
/// among the square's two and the triangle's three. Touching edges count as separated.
fn overlaps_xz(axes: &[(f32, f32, f32, f32); 3], tri_box: [f32; 4], square: [f32; 4]) -> bool {
    let [x0, x1, z0, z1] = square;
    if x1 <= x0 || z1 <= z0 {
        return false;
    }
    if tri_box[1] <= x0 || tri_box[0] >= x1 || tri_box[3] <= z0 || tri_box[2] >= z1 {
        return false;
    }
    axes.iter().all(|&(nx, nz, lo, hi)| {
        let d = [
            nx * x0 + nz * z0,
            nx * x1 + nz * z0,
            nx * x0 + nz * z1,
            nx * x1 + nz * z1,
        ];
        let s_lo = d[0].min(d[1]).min(d[2]).min(d[3]);
        let s_hi = d[0].max(d[1]).max(d[2]).max(d[3]);
        s_hi > lo && s_lo < hi
    })
}

/// The lowest and highest y of triangle `tri` clipped to the column over the square `[x0, x1, z0,
/// z1]` (Sutherland-Hodgman against the four sides; y is interpolated along each cut edge), or
/// `None` when nothing finite is left.
fn clipped_height_range(tri: &[Vec3; 3], square: [f32; 4]) -> Option<(f32, f32)> {
    // A triangle clipped by four half-planes has at most 7 vertices.
    let mut poly = [Vec3::ZERO; 8];
    let mut next = [Vec3::ZERO; 8];
    poly[..3].copy_from_slice(tri);
    let mut n = 3;
    let [x0, x1, z0, z1] = square;
    // Each side as (signed distance of a point inside it, >= 0 means kept).
    let sides: [&dyn Fn(Vec3) -> f32; 4] = [&|p| p.x - x0, &|p| x1 - p.x, &|p| p.z - z0, &|p| z1 - p.z];
    for side in sides {
        let mut m = 0;
        for (i, &p) in poly[..n].iter().enumerate() {
            let q = poly[(i + 1) % n];
            let (dp, dq) = (side(p), side(q));
            if dp >= 0.0 {
                next[m] = p;
                m += 1;
            }
            if (dp >= 0.0) != (dq >= 0.0) && m < next.len() {
                let t = dp / (dp - dq);
                next[m] = p + (q - p) * t;
                m += 1;
            }
            if m >= next.len() {
                break;
            }
        }
        poly[..m].copy_from_slice(&next[..m]);
        n = m;
        if n == 0 {
            return None;
        }
    }
    let ys = poly[..n].iter().map(|p| p.y);
    let lo = ys.clone().fold(f32::INFINITY, f32::min);
    let hi = ys.fold(f32::NEG_INFINITY, f32::max);
    (lo.is_finite() && hi.is_finite()).then_some((lo, hi))
}

/// Whether `p` (in XZ) lies inside the triangle `a..c`'s XZ projection (barycentric sign test).
fn point_in_triangle_xz(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> bool {
    let sign = |p0: Vec3, p1: Vec3, p2: Vec3| -> f32 {
        (p0.x - p2.x) * (p1.z - p2.z) - (p1.x - p2.x) * (p0.z - p2.z)
    };
    let d1 = sign(p, a, b);
    let d2 = sign(p, b, c);
    let d3 = sign(p, c, a);
    let has_neg = d1 < 0.0 || d2 < 0.0 || d3 < 0.0;
    let has_pos = d1 > 0.0 || d2 > 0.0 || d3 > 0.0;
    !(has_neg && has_pos)
}

/// The world y where the `normal`-plane passes over `(x, z)`, solving `normal · (p - a) = 0`
/// for `p.y` (the plane never being vertical, since `normal.y` is positive for walkable surfaces).
fn plane_height(x: f32, z: f32, a: Vec3, normal: Vec3) -> f32 {
    a.y - (normal.x * (x - a.x) + normal.z * (z - a.z)) / normal.y
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_floor() -> Vec<[Vec3; 3]> {
        // A 10x10 floor at y = 0, top face facing up.
        let c = 5.0;
        vec![
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, -c),
                Vec3::new(c, 0.0, c),
            ],
            [
                Vec3::new(-c, 0.0, -c),
                Vec3::new(c, 0.0, c),
                Vec3::new(-c, 0.0, c),
            ],
        ]
    }

    #[test]
    fn triangle_areas_reach_the_walkable_tops_whatever_the_order() {
        let params = NavMeshBuildParams {
            cell_size: 0.5,
            cell_height: 0.2,
            border_size: 0.5,
            ..Default::default()
        };
        let floor = flat_floor();
        let doubled: Vec<[Vec3; 3]> = floor.iter().chain(floor.iter()).copied().collect();
        let lo = Vec3::splat(-5.0);
        let hi = Vec3::splat(5.0);
        // Two coplanar copies of the floor, painted 2 and 5: equal tops keep the higher id in
        // either order.
        for areas in [[2u8, 2, 5, 5], [5, 5, 2, 2]] {
            let hf = HeightField::rasterize_with_areas(&doubled, &areas, &params, lo, hi);
            let grid = hf.walkable_grid(&params.agent);
            assert!(!grid.areas.is_empty());
            for (i, cell) in grid.cells.iter().enumerate() {
                if cell.is_some() {
                    assert_eq!(grid.area_at(i), 5);
                }
            }
        }
        // Unpainted: no area list at all, as before triangle areas existed.
        let hf = HeightField::rasterize_with_areas(&floor, &[0, 0], &params, lo, hi);
        assert!(hf.walkable_grid(&params.agent).areas.is_empty());
    }

    /// ADR-8076: a single walkable surface per column gives no lower layer and exactly the grid
    /// `walkable_grid` gives, so single-surface meshes do not change.
    #[test]
    fn a_single_surface_has_no_lower_layer_and_the_same_top_grid() {
        let params = NavMeshBuildParams {
            cell_size: 0.5,
            cell_height: 0.2,
            border_size: 0.5,
            ..Default::default()
        };
        let mut tris = flat_floor();
        // A low slab: the floor under it lacks headroom, so it is still one surface per column.
        tris.push([
            Vec3::new(-1.0, 1.0, -1.0),
            Vec3::new(1.0, 1.0, -1.0),
            Vec3::new(1.0, 1.0, 1.0),
        ]);
        let hf = HeightField::rasterize(&tris, &params, Vec3::splat(-5.0), Vec3::splat(5.0));
        let layers = hf.walkable_layers(&params.agent, 4, &WallExtents::default());
        let grid = hf.walkable_grid(&params.agent);
        assert!(layers.lower.is_empty());
        assert!(layers.links.is_empty());
        assert_eq!(layers.top.cells, grid.cells);
        assert_eq!(layers.top.areas, grid.areas);
    }

    fn flat_span(y: f32) -> Span {
        Span {
            partial: false,
            min_y: y,
            max_y: y,
            normal: Vec3::Y,
            area: 0,
        }
    }

    /// Two columns, each with a lower floor under a higher one.
    fn two_columns(lower: [f32; 2], upper: [f32; 2]) -> HeightField {
        HeightField {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: 2,
            height: 1,
            columns: vec![
                vec![flat_span(lower[0]), flat_span(upper[0])],
                vec![flat_span(lower[1]), flat_span(upper[1])],
            ],
        }
    }

    /// ADR-8076: lower floors within a step of each other whose headroom does not overlap by the
    /// agent's height are not connected, and never share a layer (the per-layer step test would
    /// join them).
    #[test]
    fn unconnected_floors_within_a_step_never_share_a_layer() {
        let agent = NavAgentParams {
            radius: 0.0,
            ..NavAgentParams::default()
        };
        // 0.0 under 1.7 and 0.4 under 2.0: a 0.4 m step, but only 1.3 m of shared headroom.
        let layers = two_columns([0.0, 0.4], [1.7, 2.0]).walkable_layers(&agent, 4, &WallExtents::default());
        assert_eq!(layers.top.cells, vec![Some(1.7), Some(2.0)]);
        assert_eq!(layers.lower.len(), 2, "{:?}", layers.lower);
        assert_eq!(layers.lower[0].cells, vec![Some(0.0), None]);
        assert_eq!(layers.lower[1].cells, vec![None, Some(0.4)]);
        assert!(layers.links.is_empty());

        // 0.0 under 2.2 and 0.4 under 2.0: 1.6 m shared, so they connect and share a layer.
        let layers = two_columns([0.0, 0.4], [2.2, 2.0]).walkable_layers(&agent, 4, &WallExtents::default());
        assert_eq!(layers.lower.len(), 1);
        assert_eq!(layers.lower[0].cells, vec![Some(0.0), Some(0.4)]);
        assert!(layers.links.is_empty());
    }

    /// ADR-8076: a lower floor that an agent can step onto from a neighbouring top surface gets a
    /// link to it, and erosion counts that link as floor, not wall.
    #[test]
    fn a_lower_floor_beside_a_top_floor_is_linked_and_not_eroded_there() {
        // Three columns: open ground, ground under a deck at 3 m, open ground.
        let field = HeightField {
            origin: Vec3::ZERO,
            cell_size: 1.0,
            width: 3,
            height: 1,
            columns: vec![
                vec![flat_span(0.0)],
                vec![flat_span(0.0), flat_span(3.0)],
                vec![flat_span(0.0)],
            ],
        };
        let agent = NavAgentParams {
            radius: 0.0,
            ..NavAgentParams::default()
        };
        let layers = field.walkable_layers(&agent, 4, &WallExtents::default());
        assert_eq!(layers.top.cells, vec![Some(0.0), Some(3.0), Some(0.0)]);
        assert_eq!(layers.lower.len(), 1);
        assert_eq!(layers.lower[0].cells, vec![None, Some(0.0), None]);
        assert_eq!(
            layers.links,
            vec![
                LayerLink {
                    a_layer: 0,
                    a_cell: 0,
                    b_layer: 1,
                    b_cell: 1,
                },
                LayerLink {
                    a_layer: 1,
                    a_cell: 1,
                    b_layer: 0,
                    b_cell: 2,
                },
            ]
        );
    }

    #[test]
    fn equal_top_spans_keep_the_flatter_normal_whatever_the_order() {
        // H3 (audit sprint-audit-g-nav-determinism): two triangles sampling the same height in a
        // cell, one flat and one steep. The merged span's normal must not depend on the triangle
        // order: the flatter face (the higher normal y) wins the equal top.
        let flat = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(2.0, 0.0, 0.0),
            Vec3::new(0.0, 0.0, 2.0),
        ];
        let steep = [
            Vec3::new(0.5, 0.0, 0.5),
            Vec3::new(1.5, 0.0, 0.5),
            Vec3::new(0.5, 2.0, 0.6),
        ];
        let window = CellRect {
            x0: 0,
            z0: 0,
            x1: 1,
            z1: 1,
        };
        let raster = |tris: &[[Vec3; 3]]| HeightField::rasterize_window(tris, Vec3::ZERO, 1.0, 0.2, window);
        let a = raster(&[flat, steep]);
        let b = raster(&[steep, flat]);
        assert_eq!(a.columns.len(), 1);
        let span_a = a.columns[0].last().expect("a span");
        let span_b = b.columns[0].last().expect("a span");
        assert_eq!(
            span_a.normal, span_b.normal,
            "the normal must not follow the triangle order"
        );
        assert_eq!(span_a.max_y, span_b.max_y);
        assert_eq!(span_a.min_y, span_b.min_y);
        // The steep face alone (normal y ~ 0.05) would fail the default slope test; the flat one
        // must win, so the cell stays walkable in both orders.
        assert_eq!(span_a.normal.y, 1.0);
        let tops_a = a.walkable_tops(&NavAgentParams::default());
        let tops_b = b.walkable_tops(&NavAgentParams::default());
        assert_eq!(tops_a, vec![Some(0.0)]);
        assert_eq!(tops_b, vec![Some(0.0)]);
    }

    /// L6 (audit sprint-audit-g-nav-determinism): a 45° ramp's unit normal y is exactly
    /// `FRAC_1_SQRT_2` (`0x3f3504f3`); with the default agent and HUMANOID it sits on the `>=`
    /// comparison and must stay walkable on every target.
    #[test]
    fn a_45_degree_ramp_stays_walkable() {
        let ramp = [
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(1.0, 1.0, 0.0),
            Vec3::new(0.0, 0.0, 1.0),
        ];
        let window = CellRect {
            x0: 0,
            z0: 0,
            x1: 1,
            z1: 1,
        };
        let hf = HeightField::rasterize_window(&[ramp], Vec3::ZERO, 1.0, 0.2, window);
        let span = hf.columns[0].last().expect("a span");
        // The flipped unit normal of (1,-1,0) is (0, 1, -1)/√2.
        assert_eq!(span.normal.y.to_bits(), std::f32::consts::FRAC_1_SQRT_2.to_bits());
        for agent in [NavAgentParams::default(), NavAgentParams::HUMANOID] {
            assert_eq!(agent.min_normal_y().to_bits(), 0x3f35_04f3);
            assert_eq!(hf.walkable_tops(&agent), vec![Some(0.5)]);
        }
    }

    #[test]
    fn flat_floor_is_walkable_everywhere() {
        let triangles = flat_floor();
        let params = NavMeshBuildParams {
            cell_size: 0.5,
            cell_height: 0.2,
            border_size: 0.5,
            ..Default::default()
        };
        let hf = HeightField::rasterize(&triangles, &params, Vec3::splat(-5.0), Vec3::splat(5.0));
        let grid = hf.walkable_grid(&params.agent);
        // The whole interior should be walkable at y = 0.
        let walkable = grid.cells.iter().filter(|c| c.is_some()).count();
        let total = grid.cells.len();
        assert!(walkable > total / 2, "most cells walkable: {walkable}/{total}");
        for cell in grid.cells.iter().flatten() {
            assert!((*cell - 0.0).abs() < 1e-3, "floor at y=0, got {cell}");
        }
    }

    #[test]
    fn vertical_wall_is_not_walkable() {
        // A thin vertical wall (degenerate in XZ) must not produce a walkable surface.
        let triangles = vec![[
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, 2.0, -1.0),
            Vec3::new(0.0, 2.0, 1.0),
        ]];
        let params = NavMeshBuildParams::default();
        let hf = HeightField::rasterize(
            &triangles,
            &params,
            Vec3::new(-2.0, 0.0, -2.0),
            Vec3::new(2.0, 2.0, 2.0),
        );
        let grid = hf.walkable_grid(&params.agent);
        assert!(
            grid.cells.iter().all(|c| c.is_none()),
            "a vertical wall yields no walkable cells"
        );
    }

    #[test]
    fn ramp_height_tracks_its_slope() {
        // A 45-degree ramp: going +X, y rises from 0 to 4 over x in [0, 4], 4 m wide. (It was one
        // 1 m triangle, which the agent radius erodes to a single walkable column: too narrow to
        // see the rise, finding 15 of sprint-deep-zai-nav-ai.)
        let triangles = vec![
            [
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(4.0, 4.0, 0.0),
                Vec3::new(4.0, 4.0, 4.0),
            ],
            [
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(4.0, 4.0, 4.0),
                Vec3::new(0.0, 0.0, 4.0),
            ],
        ];
        let params = NavMeshBuildParams {
            cell_size: 0.25,
            cell_height: 0.1,
            border_size: 0.25,
            ..Default::default()
        };
        let hf = HeightField::rasterize(
            &triangles,
            &params,
            Vec3::new(0.0, 0.0, 0.0),
            Vec3::new(4.0, 4.0, 4.0),
        );
        let grid = hf.walkable_grid(&params.agent);
        // A 45-degree slope is at the walkable limit; some cells are walkable.
        assert!(grid.cells.iter().any(|c| c.is_some()), "ramp has walkable cells");
        // The stored height follows the slope: it rises with x (y = x), within a cell of error.
        // The rise is measured between the first and the last walkable column (finding 15 of
        // sprint-deep-zai-nav-ai: the grid's own first and last columns sit in the border, outside
        // the ramp, so a check on them never ran).
        let mut lo: Option<(usize, f32)> = None;
        let mut hi: Option<(usize, f32)> = None;
        for z in 0..grid.height {
            for x in 0..grid.width {
                if let Some(y) = grid.cells[z * grid.width + x] {
                    let cx = grid.origin.x + (x as f32 + 0.5) * grid.cell_size;
                    assert!(
                        (y - cx).abs() <= grid.cell_size + params.cell_height,
                        "height {y} at x={cx} does not track y = x"
                    );
                    if lo.is_none_or(|(lx, _)| x < lx) {
                        lo = Some((x, y));
                    }
                    if hi.is_none_or(|(hx, _)| x > hx) {
                        hi = Some((x, y));
                    }
                }
            }
        }
        let ((lo_x, lo), (hi_x, hi)) = lo.zip(hi).expect("the ramp has walkable columns");
        assert!(
            hi_x >= lo_x + 2,
            "the ramp spans at least three walkable columns: {lo_x}..={hi_x}"
        );
        assert!(
            hi - lo > 2.0,
            "height rises along the ramp: {lo} (column {lo_x}) -> {hi} (column {hi_x})"
        );
    }

    #[test]
    fn a_larger_radius_erodes_cells_beside_a_wall() {
        // A 5x5 walkable floor with a wall column at x = 4 (non-walkable). cell_size 0.5.
        let width = 5;
        let height = 5;
        let mut cells = vec![Some(0.0f32); width * height];
        for z in 0..height {
            cells[z * width + 4] = None;
        }
        // radius 0 leaves the wall-adjacent column (x = 3) walkable.
        let mut c0 = cells.clone();
        erode_walkable(&mut c0, width, height, 0.5, 0.0);
        assert!(c0[3].is_some(), "radius 0 keeps cells beside the wall");

        // A radius under one cell (0.3 m < 0.5 m cell) also keeps it: boundary distance 1 ≥ 0.6.
        let mut c_small = cells.clone();
        erode_walkable(&mut c_small, width, height, 0.5, 0.3);
        assert!(c_small[3].is_some(), "radius < cell size keeps the adjacent cell");

        // A radius larger than one cell drops the wall-adjacent column.
        let mut c_large = cells.clone();
        erode_walkable(&mut c_large, width, height, 0.5, 0.6);
        assert!(
            c_large[3].is_none(),
            "radius 0.6 (1.2 cells) erodes the adjacent cell"
        );
        assert!(
            c_large[2 * width + 2].is_some(),
            "cells further from the wall and border survive"
        );
    }

    #[test]
    fn erosion_respects_the_grid_border() {
        // A 3x3 all-walkable floor: every edge cell is within one cell of the border.
        let width = 3;
        let height = 3;
        let cells = vec![Some(0.0f32); width * height];
        let mut eroded = cells.clone();
        erode_walkable(&mut eroded, width, height, 0.5, 0.3);
        // radius 0.3 (< 0.5 cell) keeps everything including edge cells.
        assert!(eroded.iter().all(Option::is_some));
        let mut eroded = cells.clone();
        erode_walkable(&mut eroded, width, height, 0.5, 0.6);
        // radius 0.6 erodes the edge ring; only the centre cell remains.
        assert!(eroded[width + 1].is_some());
        assert_eq!(eroded.iter().filter(|c| c.is_some()).count(), 1);
    }

    #[test]
    fn too_steep_floor_is_rejected() {
        let params = NavMeshBuildParams {
            cell_size: 0.5,
            cell_height: 0.2,
            border_size: 0.5,
            agent: NavAgentParams {
                max_slope: 0.0, // only perfectly flat
                ..Default::default()
            },
        };
        // Replace with a near-vertical "floor" normal: a steep slope.
        let steep: Vec<[Vec3; 3]> = vec![[
            Vec3::new(-5.0, 0.0, -5.0),
            Vec3::new(5.0, 5.0, -5.0),
            Vec3::new(5.0, 5.0, 5.0),
        ]];
        let hf = HeightField::rasterize(&steep, &params, Vec3::splat(-5.0), Vec3::new(5.0, 5.0, 5.0));
        let grid = hf.walkable_grid(&params.agent);
        assert!(
            grid.cells.iter().all(|c| c.is_none()),
            "steep slope is not walkable"
        );
    }

    #[test]
    fn rasterize_measures_an_f32_overflowing_span_in_f64() {
        let params = NavMeshBuildParams {
            cell_size: 1e33,
            ..Default::default()
        };
        let hf = HeightField::rasterize(
            &[],
            &params,
            Vec3::new(-3.0e38, 0.0, 0.0),
            Vec3::new(3.0e38, 1.0, 1.0),
        );
        assert_eq!(hf.width, 600_002);
    }

    fn flat_quad(y: f32) -> Vec<[Vec3; 3]> {
        vec![
            [
                Vec3::new(0.0, y, 0.0),
                Vec3::new(2.0, y, 0.0),
                Vec3::new(2.0, y, 2.0),
            ],
            [
                Vec3::new(0.0, y, 0.0),
                Vec3::new(2.0, y, 2.0),
                Vec3::new(0.0, y, 2.0),
            ],
        ]
    }

    fn column_with_spans(field: &HeightField) -> Option<&Vec<Span>> {
        field.columns.iter().find(|spans| !spans.is_empty())
    }

    /// `cell_height` is the merge gap, not a vertical voxel. A floor at 0.13 m keeps that height
    /// at two cell heights, and two floors 0.08 m apart merge only when half of `cell_height`
    /// covers the gap.
    #[test]
    fn cell_height_merges_stacked_spans_and_keeps_source_heights() {
        let bounds_min = Vec3::ZERO;
        let bounds_max = Vec3::new(2.0, 1.0, 2.0);
        for cell_height in [0.2, 0.05] {
            let params = NavMeshBuildParams {
                cell_size: 1.0,
                cell_height,
                border_size: 0.0,
                ..Default::default()
            };
            let field = HeightField::rasterize(&flat_quad(0.13), &params, bounds_min, bounds_max);
            let spans = column_with_spans(&field).expect("the floor rasterizes");
            assert_eq!(spans.len(), 1, "one floor, cell_height {cell_height}");
            assert!(
                (spans[0].max_y - 0.13).abs() < 1e-4,
                "height stays 0.13 at cell_height {cell_height}, got {}",
                spans[0].max_y
            );
        }
        let mut stacked = flat_quad(0.0);
        stacked.extend(flat_quad(0.08));
        let merged = HeightField::rasterize(
            &stacked,
            &NavMeshBuildParams {
                cell_size: 1.0,
                cell_height: 0.2,
                border_size: 0.0,
                ..Default::default()
            },
            bounds_min,
            bounds_max,
        );
        let spans = column_with_spans(&merged).expect("merged column");
        assert_eq!(spans.len(), 1, "0.08 m gap merges at cell_height 0.2: {spans:?}");
        assert!((spans[0].max_y - 0.08).abs() < 1e-4, "{}", spans[0].max_y);
        let split = HeightField::rasterize(
            &stacked,
            &NavMeshBuildParams {
                cell_size: 1.0,
                cell_height: 0.05,
                border_size: 0.0,
                ..Default::default()
            },
            bounds_min,
            bounds_max,
        );
        let spans = column_with_spans(&split).expect("split column");
        assert_eq!(
            spans.len(),
            2,
            "0.08 m gap stays split at cell_height 0.05: {spans:?}"
        );
        assert!((spans[0].max_y - 0.0).abs() < 1e-4);
        assert!((spans[1].max_y - 0.08).abs() < 1e-4);
    }

    /// The rasteriser before nav cell v3 (ADR-7558), kept here as the reference: a span only where
    /// a cell centre lies inside the triangle's XZ projection, merged exactly as it was.
    fn centre_sampled(
        triangles: &[[Vec3; 3]],
        params: &NavMeshBuildParams,
        lo: Vec3,
        hi: Vec3,
    ) -> HeightField {
        let mut field = HeightField::rasterize(&[], params, lo, hi);
        let cs = field.cell_size;
        for tri in triangles {
            let [a, b, c] = *tri;
            let mut normal = (b - a).cross(c - a);
            let len = normal.length();
            if len < 1e-12 || normal.y.abs() < 1e-6 {
                continue;
            }
            normal /= len;
            if normal.y < 0.0 {
                normal = -normal;
            }
            for z in 0..field.height {
                for x in 0..field.width {
                    let px = field.origin.x + (x as f32 + 0.5) * cs;
                    let pz = field.origin.z + (z as f32 + 0.5) * cs;
                    if !point_in_triangle_xz(Vec3::new(px, 0.0, pz), a, b, c) {
                        continue;
                    }
                    let y = plane_height(px, pz, a, normal);
                    field.columns[z * field.width + x].push(Span {
                        partial: false,
                        min_y: y,
                        max_y: y,
                        normal,
                        area: 0,
                    });
                }
            }
        }
        for spans in &mut field.columns {
            spans.sort_by(|a, b| a.min_y.total_cmp(&b.min_y));
            let mut merged: Vec<Span> = Vec::new();
            for span in spans.iter().copied() {
                match merged.last_mut() {
                    Some(prev) if span.min_y - prev.max_y <= params.cell_height * 0.5 => {
                        if span.max_y > prev.max_y {
                            prev.max_y = span.max_y;
                            prev.normal = span.normal;
                            prev.area = span.area;
                        } else if span.max_y == prev.max_y {
                            if span.normal.y > prev.normal.y {
                                prev.normal = span.normal;
                            }
                            prev.area = prev.area.max(span.area);
                        }
                    }
                    _ => merged.push(span),
                }
            }
            *spans = merged;
        }
        field
    }

    fn span_bits(field: &HeightField) -> Vec<Vec<[u32; 5]>> {
        field
            .columns
            .iter()
            .map(|c| {
                c.iter()
                    .map(|s| {
                        [
                            s.min_y.to_bits(),
                            s.max_y.to_bits(),
                            s.normal.x.to_bits(),
                            s.normal.y.to_bits(),
                            s.normal.z.to_bits(),
                        ]
                    })
                    .collect()
            })
            .collect()
    }

    #[test]
    fn a_grid_aligned_flat_floor_rasterises_bit_identically_to_centre_sampling() {
        // The default grid (0.25 m cells, 1 m border) and a 10 m floor: its edges lie on grid lines,
        // so conservative coverage adds no cell, and every span is the centre sample it was.
        let params = NavMeshBuildParams::default();
        let (lo, hi) = (Vec3::new(-5.0, 0.0, -5.0), Vec3::new(5.0, 0.0, 5.0));
        let new = HeightField::rasterize(&flat_floor(), &params, lo, hi);
        let old = centre_sampled(&flat_floor(), &params, lo, hi);
        assert_eq!(span_bits(&new), span_bits(&old));
        let agent = NavAgentParams::default();
        let (new_grid, old_grid) = (new.walkable_grid(&agent), old.walkable_grid(&agent));
        let bits = |g: &WalkableGrid| g.cells.iter().map(|c| c.map(f32::to_bits)).collect::<Vec<_>>();
        assert_eq!(bits(&new_grid), bits(&old_grid));
        assert!(new_grid.cells.iter().filter(|c| c.is_some()).count() > 1000);
    }

    #[test]
    fn a_walkable_slope_keeps_its_centre_sampled_heights() {
        // A 30-degree slope of eight triangles (coplanar neighbours overlap every diagonal cell):
        // each cell a centre sample filled keeps its top height and normal bit for bit (a span's
        // bottom may reach lower: the overlap's range counts); conservative coverage only adds
        // cells along the outer edges, which are off the 0.25 m grid here, and those partial cells
        // never survive the erosion of an agent with a radius.
        let rise = 30f32.to_radians().tan();
        let mut tris = Vec::new();
        for i in 0..4 {
            let (x0, x1) = (i as f32 * 1.03, (i + 1) as f32 * 1.03);
            let (y0, y1) = (x0 * rise, x1 * rise);
            tris.push([
                Vec3::new(x0, y0, 0.0),
                Vec3::new(x1, y1, 0.0),
                Vec3::new(x1, y1, 3.1),
            ]);
            tris.push([
                Vec3::new(x0, y0, 0.0),
                Vec3::new(x1, y1, 3.1),
                Vec3::new(x0, y0, 3.1),
            ]);
        }
        let params = NavMeshBuildParams::default();
        let (lo, hi) = (Vec3::new(0.0, 0.0, 0.0), Vec3::new(4.12, 4.12 * rise, 3.1));
        let new = HeightField::rasterize(&tris, &params, lo, hi);
        let old = centre_sampled(&tris, &params, lo, hi);
        let tops = |f: &HeightField| -> Vec<Vec<[u32; 4]>> {
            span_bits(f)
                .into_iter()
                .map(|c| {
                    c.into_iter()
                        .map(|[_, top, nx, ny, nz]| [top, nx, ny, nz])
                        .collect()
                })
                .collect()
        };
        let (new_tops, old_tops) = (tops(&new), tops(&old));
        let mut added = 0;
        for (i, (n, o)) in new_tops.iter().zip(&old_tops).enumerate() {
            if o.is_empty() {
                added += usize::from(!n.is_empty());
                assert!(
                    new.columns[i].iter().all(|s| s.partial),
                    "column {i}: an added cell is partial"
                );
            } else {
                assert_eq!(n, o, "column {i}");
            }
        }
        assert!(added > 0, "the off-grid outer edges gain cells");
        let agent = NavAgentParams::default();
        let bits = |g: &WalkableGrid| g.cells.iter().map(|c| c.map(f32::to_bits)).collect::<Vec<_>>();
        assert_eq!(
            bits(&new.walkable_grid(&agent)),
            bits(&old.walkable_grid(&agent)),
            "the eroded surface of a 0.35 m agent is unchanged"
        );
    }

    #[test]
    fn a_thin_strip_between_two_centres_is_seen_and_one_on_a_grid_line_does_not_spill() {
        let params = NavMeshBuildParams {
            cell_size: 1.0,
            cell_height: 0.2,
            border_size: 0.0,
            ..Default::default()
        };
        // Cells [0,1] and [1,2] in x; centres at 0.5 and 1.5. A strip at x in [0.6, 0.9] covers
        // neither centre.
        let strip = |x0: f32, x1: f32| {
            vec![
                [
                    Vec3::new(x0, 1.0, 0.0),
                    Vec3::new(x1, 1.0, 0.0),
                    Vec3::new(x1, 1.0, 2.0),
                ],
                [
                    Vec3::new(x0, 1.0, 0.0),
                    Vec3::new(x1, 1.0, 2.0),
                    Vec3::new(x0, 1.0, 2.0),
                ],
            ]
        };
        let (lo, hi) = (Vec3::new(0.0, 0.0, 0.0), Vec3::new(2.0, 1.0, 2.0));
        let tops = |field: &HeightField| {
            field
                .columns
                .iter()
                .map(|c| c.last().map(|s| s.max_y))
                .collect::<Vec<_>>()
        };
        let thin = strip(0.6, 0.9);
        assert!(
            tops(&centre_sampled(&thin, &params, lo, hi))
                .iter()
                .all(Option::is_none)
        );
        let field = HeightField::rasterize(&thin, &params, lo, hi);
        let w = field.width;
        for z in 0..2 {
            assert_eq!(tops(&field)[z * w], Some(1.0), "row {z}: the strip's cell");
            assert_eq!(tops(&field)[z * w + 1], None, "row {z}: the next cell");
        }
        // A strip ending exactly on the grid line x = 1 does not reach the cell beyond it.
        let field = HeightField::rasterize(&strip(0.2, 1.0), &params, lo, hi);
        for z in 0..2 {
            assert_eq!(tops(&field)[z * w + 1], None, "row {z}");
        }
    }

    #[test]
    fn a_steep_face_overlapping_a_sampled_floor_cell_takes_the_top() {
        // A floor sampled at the centre (y = 0) and a steep (unwalkable) face that rises 1.5 m
        // within the cell without covering its centre: the face's top replaces the floor's.
        let params = NavMeshBuildParams {
            cell_size: 1.0,
            cell_height: 0.2,
            border_size: 0.0,
            ..Default::default()
        };
        let mut tris = flat_floor_unit();
        tris.push([
            Vec3::new(0.7, 0.0, 0.0),
            Vec3::new(0.8, 1.5, 0.0),
            Vec3::new(0.8, 1.5, 1.0),
        ]);
        tris.push([
            Vec3::new(0.7, 0.0, 0.0),
            Vec3::new(0.8, 1.5, 1.0),
            Vec3::new(0.7, 0.0, 1.0),
        ]);
        let (lo, hi) = (Vec3::new(0.0, 0.0, 0.0), Vec3::new(1.0, 1.5, 1.0));
        let field = HeightField::rasterize(&tris, &params, lo, hi);
        let top = field.columns[0].last().copied().expect("a span");
        assert!((top.max_y - 1.5).abs() < 1e-5, "{top:?}");
        assert!(top.normal.y < NavAgentParams::default().min_normal_y(), "steep");
        assert_eq!(
            field.walkable_tops(&NavAgentParams {
                radius: 0.0,
                ..Default::default()
            })[0],
            None
        );
        // The same floor without the face stays walkable at 0.
        let floor = HeightField::rasterize(&flat_floor_unit(), &params, lo, hi);
        assert_eq!(
            floor.walkable_tops(&NavAgentParams {
                radius: 0.0,
                ..Default::default()
            })[0],
            Some(0.0)
        );
    }

    fn flat_floor_unit() -> Vec<[Vec3; 3]> {
        vec![
            [
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 1.0),
            ],
            [
                Vec3::new(0.0, 0.0, 0.0),
                Vec3::new(1.0, 0.0, 1.0),
                Vec3::new(0.0, 0.0, 1.0),
            ],
        ]
    }

    #[test]
    fn conservative_spans_do_not_depend_on_the_triangle_order() {
        let mut tris = flat_floor();
        tris.extend([
            [
                Vec3::new(0.13, 2.0, -5.0),
                Vec3::new(0.23, 2.0, -5.0),
                Vec3::new(0.23, 2.0, 5.0),
            ],
            [
                Vec3::new(0.13, 2.0, -5.0),
                Vec3::new(0.23, 2.0, 5.0),
                Vec3::new(0.13, 2.0, 5.0),
            ],
            [
                Vec3::new(-2.0, 0.0, -1.0),
                Vec3::new(-1.9, 0.12, -1.0),
                Vec3::new(-1.9, 0.12, 1.0),
            ],
        ]);
        let params = NavMeshBuildParams::default();
        let (lo, hi) = (Vec3::new(-5.0, 0.0, -5.0), Vec3::new(5.0, 2.0, 5.0));
        let forward = HeightField::rasterize(&tris, &params, lo, hi);
        tris.reverse();
        let backward = HeightField::rasterize(&tris, &params, lo, hi);
        assert_eq!(span_bits(&forward), span_bits(&backward));
    }
}
