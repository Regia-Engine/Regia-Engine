// SPDX-License-Identifier: MPL-2.0
//! The bake's spatial index over the source cells of one proxy (ADR-2713): a small box tree over
//! the cells' bounds whose leaves are the cells' own triangle trees, a two-level tree. A parent
//! node bakes against every leaf cell under it, so a ray used to visit each of those cells' trees in
//! turn; through the index it visits only the cells its segment can reach, and the nearest hit so
//! far shortens the search in the cells after it. A proxy triangle with only a few cells near it
//! still searches them in turn (`FEW_CELLS`).
//!
//! Method (original code): a top-down median split on the longest axis of the cell box centers,
//! as bounding volume hierarchies are commonly built (Kay & Kajiya, "Ray Tracing Complex Scenes",
//! SIGGRAPH 1986, for the slab test the traversal uses), with the second level reusing the
//! `regia_core::mesh_query::TriBvh` each cell already has.

use regia_core::math::{Aabb, Ray, Vec3};
use regia_core::mesh_query::QueryCounts;

use super::BakeSource;

/// At most this many cells in an index leaf.
const LEAF_CELLS: usize = 2;
/// A proxy triangle with at most this many cells near it searches them in turn: below it the
/// index's own box tests cost more than they save (measured, ADR-2713).
pub(super) const FEW_CELLS: usize = 3;
/// Index boxes grow by this much, meters: the cells' trees test in `f64`, the index in `f32`, and
/// a box a hair too small would drop a hit the old search found.
const BOX_SLACK: f32 = 1e-3;

/// Work the bake's source searches did: what the cook's cost follows (ADR-2713). The counts are
/// deterministic for the same input; the tests compare them, never time.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BakeCounts {
    /// Ray casts, the steps through cutout holes included.
    pub rays: u64,
    /// Closest-point searches (a texel no ray found surface for).
    pub closest: u64,
    /// Index boxes tested.
    pub index_nodes: u64,
    /// Cell trees entered.
    pub cells_entered: u64,
    /// Node boxes and triangles tested inside the cells' trees.
    pub tree: QueryCounts,
    /// Steps a ray took on through a cutout hole.
    pub cutout_steps: u64,
}

impl BakeCounts {
    /// Every box and triangle test per query (ray or closest-point search): the cost per query.
    pub fn tests_per_query(&self) -> f64 {
        let queries = (self.rays + self.closest).max(1) as f64;
        (self.index_nodes + self.tree.nodes + self.tree.triangles) as f64 / queries
    }

    /// Triangles tested per query.
    pub fn triangles_per_query(&self) -> f64 {
        self.tree.triangles as f64 / (self.rays + self.closest).max(1) as f64
    }

    /// The sum of two counts.
    #[must_use]
    pub fn plus(self, other: Self) -> Self {
        Self {
            rays: self.rays + other.rays,
            closest: self.closest + other.closest,
            index_nodes: self.index_nodes + other.index_nodes,
            cells_entered: self.cells_entered + other.cells_entered,
            tree: self.tree.plus(other.tree),
            cutout_steps: self.cutout_steps + other.cutout_steps,
        }
    }
}

/// A hit: distance, source position in the bake's source list, triangle, barycentric.
pub(super) type IndexHit = (f32, usize, u32, Vec3);

/// `a` is nearer than `b`: by distance, then by the earlier source (the order the search used to
/// visit them in, so ties resolve as before).
fn nearer(a: &IndexHit, b: &IndexHit) -> bool {
    a.0 < b.0 || (a.0 == b.0 && a.1 < b.1)
}

/// One index node: a leaf holds `count > 0` sources at `order[first..first + count]`; an internal
/// node has its children at `first` and `first + 1`.
#[derive(Debug, Clone, Copy)]
struct Node {
    bounds: Aabb,
    first: u32,
    count: u32,
}

/// How the bake finds source surface.
pub(super) enum Lookup {
    /// Through the two-level index.
    Indexed(SourceIndex),
    /// Every source near the proxy triangle in turn, each with the full reach, however many: the
    /// search before the index, kept for the tests that measure the difference.
    #[cfg(test)]
    Linear,
}

/// The two-level index over a bake's sources.
pub(super) struct SourceIndex {
    nodes: Vec<Node>,
    order: Vec<u32>,
}

impl SourceIndex {
    /// The index over `sources` (positions in that slice); `None` with no source.
    pub(super) fn build(sources: &[&BakeSource]) -> Option<Self> {
        if sources.is_empty() {
            return None;
        }
        let grow = |b: Aabb| Aabb {
            min: b.min - Vec3::splat(BOX_SLACK),
            max: b.max + Vec3::splat(BOX_SLACK),
        };
        let boxes: Vec<Aabb> = sources.iter().map(|s| grow(s.bounds)).collect();
        let mut order: Vec<u32> = (0..sources.len() as u32).collect();
        let mut nodes = vec![Node {
            bounds: boxes[0],
            first: 0,
            count: 0,
        }];
        // (node, start, end) over `order`.
        let mut work = vec![(0usize, 0usize, order.len())];
        while let Some((node, start, end)) = work.pop() {
            let mut bounds = boxes[order[start] as usize];
            for &i in &order[start + 1..end] {
                bounds = bounds.union(&boxes[i as usize]);
            }
            nodes[node].bounds = bounds;
            let count = end - start;
            if count <= LEAF_CELLS {
                nodes[node].first = start as u32;
                nodes[node].count = count as u32;
                continue;
            }
            let extent = bounds.size();
            let axis = if extent.x >= extent.y && extent.x >= extent.z {
                0
            } else if extent.y >= extent.z {
                1
            } else {
                2
            };
            // Sort by center along the axis, ties by source position: deterministic.
            order[start..end].sort_by(|&a, &b| {
                boxes[a as usize].center()[axis]
                    .total_cmp(&boxes[b as usize].center()[axis])
                    .then(a.cmp(&b))
            });
            let mid = start + count / 2;
            let first = nodes.len();
            nodes[node].first = first as u32;
            nodes.push(Node {
                bounds,
                first: 0,
                count: 0,
            });
            nodes.push(Node {
                bounds,
                first: 0,
                count: 0,
            });
            work.push((first, start, mid));
            work.push((first + 1, mid, end));
        }
        Some(Self { nodes, order })
    }

    /// Index boxes (a measure of the index's size).
    #[cfg(test)]
    pub(super) fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// The nearest hit along `ray` within `reach` over `sources`.
    pub(super) fn ray(
        &self,
        sources: &[&BakeSource],
        ray: &Ray,
        reach: f32,
        counts: &mut BakeCounts,
    ) -> Option<IndexHit> {
        let mut best: Option<IndexHit> = None;
        let mut stack: Vec<(u32, f32)> = Vec::with_capacity(32);
        counts.index_nodes += 1;
        if let Some(t) = ray.intersect_aabb(&self.nodes[0].bounds)
            && t <= reach
        {
            stack.push((0, t));
        }
        while let Some((ni, entry)) = stack.pop() {
            let limit = best.map_or(reach, |b| b.0);
            if entry > limit {
                continue;
            }
            let node = self.nodes[ni as usize];
            if node.count > 0 {
                for &s in &self.order[node.first as usize..(node.first + node.count) as usize] {
                    let s = s as usize;
                    counts.cells_entered += 1;
                    let limit = best.map_or(reach, |b| b.0);
                    if let Some(hit) = sources[s].tree.ray_cast_counted(ray, limit, &mut counts.tree) {
                        let hit = (hit.distance, s, hit.triangle, hit.barycentric);
                        if best.is_none_or(|b| nearer(&hit, &b)) {
                            best = Some(hit);
                        }
                    }
                }
                continue;
            }
            let limit = best.map_or(reach, |b| b.0);
            let mut children = [None, None];
            for (k, child) in [node.first, node.first + 1].into_iter().enumerate() {
                counts.index_nodes += 1;
                children[k] = ray
                    .intersect_aabb(&self.nodes[child as usize].bounds)
                    .filter(|t| *t <= limit)
                    .map(|t| (child, t));
            }
            // The nearer child is searched first (pushed last).
            match children {
                [Some(a), Some(b)] if a.1 <= b.1 => stack.extend([b, a]),
                [Some(a), Some(b)] => stack.extend([a, b]),
                [Some(a), None] | [None, Some(a)] => stack.push(a),
                [None, None] => {}
            }
        }
        best
    }

    /// The closest source point to `p` within `reach` over `sources`.
    pub(super) fn closest(
        &self,
        sources: &[&BakeSource],
        p: Vec3,
        reach: f32,
        counts: &mut BakeCounts,
    ) -> Option<IndexHit> {
        let distance = |b: &Aabb| (b.min - p).max(p - b.max).max(Vec3::ZERO).length();
        let mut best: Option<IndexHit> = None;
        let mut stack: Vec<(u32, f32)> = Vec::with_capacity(32);
        counts.index_nodes += 1;
        let d = distance(&self.nodes[0].bounds);
        if d <= reach {
            stack.push((0, d));
        }
        while let Some((ni, d)) = stack.pop() {
            if d > best.map_or(reach, |b| b.0) {
                continue;
            }
            let node = self.nodes[ni as usize];
            if node.count > 0 {
                for &s in &self.order[node.first as usize..(node.first + node.count) as usize] {
                    let s = s as usize;
                    counts.cells_entered += 1;
                    let limit = best.map_or(reach, |b| b.0);
                    if let Some(c) = sources[s].tree.closest_point_counted(p, limit, &mut counts.tree) {
                        let hit = (c.distance, s, c.triangle, c.barycentric);
                        if best.is_none_or(|b| nearer(&hit, &b)) {
                            best = Some(hit);
                        }
                    }
                }
                continue;
            }
            let limit = best.map_or(reach, |b| b.0);
            let mut children = [None, None];
            for (k, child) in [node.first, node.first + 1].into_iter().enumerate() {
                counts.index_nodes += 1;
                let d = distance(&self.nodes[child as usize].bounds);
                children[k] = (d <= limit).then_some((child, d));
            }
            match children {
                [Some(a), Some(b)] if a.1 <= b.1 => stack.extend([b, a]),
                [Some(a), Some(b)] => stack.extend([a, b]),
                [Some(a), None] | [None, Some(a)] => stack.push(a),
                [None, None] => {}
            }
        }
        best
    }
}

/// The nearest hit along `ray` within `reach` over the sources at `near` (positions in `sources`),
/// each searched with the full reach: the search for a few cells, and the one before the index.
pub(super) fn linear_ray(
    sources: &[&BakeSource],
    near: &[usize],
    ray: &Ray,
    reach: f32,
    counts: &mut BakeCounts,
) -> Option<IndexHit> {
    let mut best: Option<IndexHit> = None;
    for &s in near {
        counts.cells_entered += 1;
        if let Some(hit) = sources[s].tree.ray_cast_counted(ray, reach, &mut counts.tree) {
            let hit = (hit.distance, s, hit.triangle, hit.barycentric);
            if best.is_none_or(|b| nearer(&hit, &b)) {
                best = Some(hit);
            }
        }
    }
    best
}

/// The closest point within `reach` over the sources at `near`, each searched with the full reach.
pub(super) fn linear_closest(
    sources: &[&BakeSource],
    near: &[usize],
    p: Vec3,
    reach: f32,
    counts: &mut BakeCounts,
) -> Option<IndexHit> {
    let mut best: Option<IndexHit> = None;
    for &s in near {
        counts.cells_entered += 1;
        if let Some(c) = sources[s].tree.closest_point_counted(p, reach, &mut counts.tree) {
            let hit = (c.distance, s, c.triangle, c.barycentric);
            if best.is_none_or(|b| nearer(&hit, &b)) {
                best = Some(hit);
            }
        }
    }
    best
}
