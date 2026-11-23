// SPDX-License-Identifier: MPL-2.0
//! Binned SAH BVH over axis-aligned boxes (Wald 2007, "On fast construction of SAH-based
//! bounding volume hierarchies"; PBRT 4th ed. ch. 7), flattened for the GPU.
//!
//! Layout: siblings are adjacent. An interior node stores the index of its left child in `a`
//! (the right child is `a + 1`) and `count == 0`; a leaf stores its first primitive in `a` and
//! the primitive count. The builder reorders primitives so every leaf covers a contiguous run;
//! [`Bvh::order`] maps that run back to the caller's indices.
//!
//! Large inputs build on several threads (ADR-862): the top of the tree forks into subtrees
//! that build concurrently (the subtree-parallel construction Wald 2007 describes), with the
//! node order fixed so the result does not depend on the thread count.

use bytemuck::{Pod, Zeroable};

/// Bins per axis for the SAH sweep.
const BINS: usize = 16;
/// Largest leaf the builder will make when no split is cheaper.
const MAX_LEAF: usize = 4;
/// Relative cost of one traversal step against one primitive test.
const TRAVERSAL_COST: f32 = 1.0;

/// An axis-aligned box.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Aabb {
    /// Minimum corner.
    pub min: [f32; 3],
    /// Maximum corner.
    pub max: [f32; 3],
}

impl Aabb {
    /// The empty box (grows from nothing).
    pub const EMPTY: Self = Self {
        min: [f32::INFINITY; 3],
        max: [f32::NEG_INFINITY; 3],
    };

    /// The box around `points`.
    #[must_use]
    pub fn from_points(points: &[[f32; 3]]) -> Self {
        let mut b = Self::EMPTY;
        for p in points {
            b.grow_point(*p);
        }
        b
    }

    /// Grows to include `p`.
    pub fn grow_point(&mut self, p: [f32; 3]) {
        for (k, v) in p.into_iter().enumerate() {
            self.min[k] = self.min[k].min(v);
            self.max[k] = self.max[k].max(v);
        }
    }

    /// Grows to include `other`.
    pub fn grow(&mut self, other: &Self) {
        for k in 0..3 {
            self.min[k] = self.min[k].min(other.min[k]);
            self.max[k] = self.max[k].max(other.max[k]);
        }
    }

    /// True when nothing was added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        (0..3).any(|k| self.min[k] > self.max[k])
    }

    /// Centre point.
    #[must_use]
    pub fn centroid(&self) -> [f32; 3] {
        [
            0.5 * (self.min[0] + self.max[0]),
            0.5 * (self.min[1] + self.max[1]),
            0.5 * (self.min[2] + self.max[2]),
        ]
    }

    /// Surface area (0 for an empty box).
    #[must_use]
    pub fn area(&self) -> f32 {
        if self.is_empty() {
            return 0.0;
        }
        let d = [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ];
        2.0 * (d[0] * d[1] + d[1] * d[2] + d[2] * d[0])
    }
}

/// One flattened node (32 bytes, matches `Node` in `pt.wgsl`).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Pod, Zeroable)]
pub struct BvhNode {
    /// Box minimum.
    pub min: [f32; 3],
    /// Interior: left child index. Leaf: first primitive.
    pub a: u32,
    /// Box maximum.
    pub max: [f32; 3],
    /// 0 for an interior node, else the primitive count.
    pub count: u32,
}

impl BvhNode {
    /// True for a leaf.
    #[must_use]
    pub fn is_leaf(&self) -> bool {
        self.count > 0
    }
}

/// A built hierarchy.
#[derive(Clone, Debug, Default)]
pub struct Bvh {
    /// Nodes; the root is node 0.
    pub nodes: Vec<BvhNode>,
    /// `order[i]` is the caller's index of the `i`-th primitive in leaf order.
    pub order: Vec<u32>,
}

impl Bvh {
    /// Builds a hierarchy over `bounds` (one box per primitive) on every core the machine
    /// has. An empty input yields one empty leaf.
    #[must_use]
    pub fn build(bounds: &[Aabb]) -> Self {
        let threads = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        Self::build_with_threads(bounds, threads)
    }

    /// [`Self::build`] on at most `threads` threads (ADR-862). The result does not depend on
    /// `threads`: below [`PARALLEL_MIN_PRIMS`] primitives the build is the single-threaded
    /// one; above it the top of the tree is split into subtrees of at most `TASK_PRIMS`
    /// primitives by the same SAH decisions, the subtrees build concurrently, and the nodes
    /// are laid out in one fixed order.
    #[must_use]
    pub fn build_with_threads(bounds: &[Aabb], threads: usize) -> Self {
        let _span = tracing::info_span!("pt_bvh_build", prims = bounds.len()).entered();
        let mut order: Vec<u32> = (0..bounds.len() as u32).collect();
        if bounds.is_empty() {
            return Self {
                nodes: vec![BvhNode::zeroed()],
                order,
            };
        }
        let centroids: Vec<[f32; 3]> = bounds.iter().map(Aabb::centroid).collect();
        if bounds.len() < PARALLEL_MIN_PRIMS {
            let nodes = build_sequential(bounds, &centroids, &mut order, 0, 0);
            return Self { nodes, order };
        }
        // Fork while there are threads to fork to: 2^forks subtrees run at once.
        let forks = threads.max(1).next_power_of_two().trailing_zeros() as usize;
        let top = build_top(bounds, &centroids, &mut order, 0, forks, 0);
        Self {
            nodes: flatten(top),
            order,
        }
    }

    /// Surface-area-heuristic cost of the tree (Wald 2007): every interior node costs one
    /// traversal step and every leaf its primitive count, each weighted by the probability
    /// that a ray through the root box enters it (area ratio). Lower is a better tree.
    #[must_use]
    pub fn sah_cost(&self) -> f32 {
        let area = |n: &BvhNode| {
            Aabb {
                min: n.min,
                max: n.max,
            }
            .area()
        };
        let root = self.nodes.first().map_or(0.0, area);
        if root <= 0.0 {
            return 0.0;
        }
        let mut cost = 0.0f64;
        for n in &self.nodes {
            let weight = f64::from(area(n) / root);
            cost += weight
                * if n.is_leaf() {
                    f64::from(n.count)
                } else {
                    f64::from(TRAVERSAL_COST)
                };
        }
        cost as f32
    }

    /// Bounds of the root.
    #[must_use]
    pub fn bounds(&self) -> Aabb {
        self.nodes.first().map_or(Aabb::EMPTY, |n| Aabb {
            min: n.min,
            max: n.max,
        })
    }

    /// Offsets every child and primitive index (to concatenate hierarchies into one array).
    pub fn rebase(&mut self, node_offset: u32, prim_offset: u32) {
        for node in &mut self.nodes {
            if node.is_leaf() {
                node.a += prim_offset;
            } else {
                node.a += node_offset;
            }
        }
    }
}

/// Below this many primitives [`Bvh::build`] runs on one thread.
pub const PARALLEL_MIN_PRIMS: usize = 16_384;
/// Ranges of at most this many primitives build as one single-threaded subtree.
const TASK_PRIMS: usize = 4_096;
/// Deepest the forking top of a parallel build recurses before it builds the rest of a range
/// on one thread (bounds the recursion on very unbalanced splits).
const MAX_TOP_DEPTH: usize = 48;
/// Deepest leaf any build makes, in levels below the root: `pt.wgsl` walks a hierarchy with a
/// stack of this many entries (`BVH_STACK`) and a deeper tree would drop subtrees there. A node
/// whose depth plus `ceil(log2(primitives))` reaches it stops following the SAH and splits at
/// the centroid median of its longest axis, which keeps every leaf below at most this deep.
pub const MAX_DEPTH: usize = 48;

/// `ceil(log2(n))` for `n >= 1`.
fn ceil_log2(n: usize) -> usize {
    n.next_power_of_two().trailing_zeros() as usize
}

fn interior(bb: &Aabb, left: usize) -> BvhNode {
    BvhNode {
        min: bb.min,
        a: left as u32,
        max: bb.max,
        count: 0,
    }
}

fn leaf(bb: &Aabb, first: usize, count: usize) -> BvhNode {
    BvhNode {
        min: bb.min,
        a: first as u32,
        max: bb.max,
        count: count as u32,
    }
}

/// The bounds of `prims` and where to split them: `Some(mid)` puts `prims[..mid]` left and
/// `prims[mid..]` right (reordering `prims`), `None` makes a leaf. `depth` is the node's level
/// below the root (see [`MAX_DEPTH`]).
fn split_range(
    bounds: &[Aabb],
    centroids: &[[f32; 3]],
    prims: &mut [u32],
    depth: usize,
) -> (Aabb, Option<usize>) {
    let mut bb = Aabb::EMPTY;
    let mut cb = Aabb::EMPTY;
    for &i in prims.iter() {
        bb.grow(&bounds[i as usize]);
        cb.grow_point(centroids[i as usize]);
    }
    let n = prims.len();
    if n > 1 && depth + ceil_log2(n) >= MAX_DEPTH {
        // Out of SAH depth: halve by the centroid median of the longest centroid axis (ties by
        // primitive index, so the split is deterministic), which never deepens past MAX_DEPTH.
        let extent = |a: usize| cb.max[a] - cb.min[a];
        let axis = (0..3)
            .max_by(|&a, &b| extent(a).total_cmp(&extent(b)).then(b.cmp(&a)))
            .unwrap_or(0);
        let mid = n / 2;
        prims.select_nth_unstable_by(mid, |&x, &y| {
            centroids[x as usize][axis]
                .total_cmp(&centroids[y as usize][axis])
                .then(x.cmp(&y))
        });
        return (bb, Some(mid));
    }
    let split = if n <= 1 {
        None
    } else {
        choose_split(bounds, centroids, prims, &bb, &cb)
    };
    let mid = match split {
        Some((axis, pos)) => {
            let mut left = 0usize;
            for k in 0..prims.len() {
                if centroids[prims[k] as usize][axis] < pos {
                    prims.swap(k, left);
                    left += 1;
                }
            }
            if left == 0 || left == n { None } else { Some(left) }
        }
        None if n > MAX_LEAF => Some(n / 2),
        None => None,
    };
    let mid = match (mid, split) {
        // A split that put everything on one side: fall back to a median by index.
        (None, Some(_)) if n > MAX_LEAF => Some(n / 2),
        (m, _) => m,
    };
    (bb, mid)
}

/// The single-threaded build of `prims` (the caller's primitives `base..base + len` in leaf
/// order): nodes with the root at 0, leaf `a` absolute (`base` added).
fn build_sequential(
    bounds: &[Aabb],
    centroids: &[[f32; 3]],
    prims: &mut [u32],
    base: usize,
    depth: usize,
) -> Vec<BvhNode> {
    let mut nodes = vec![BvhNode::zeroed()];
    // (node index, start, end, depth) still to build.
    let mut stack = vec![(0usize, 0usize, prims.len(), depth)];
    while let Some((node, start, end, level)) = stack.pop() {
        let (bb, mid) = split_range(bounds, centroids, &mut prims[start..end], level);
        match mid {
            Some(mid) => {
                let left = nodes.len();
                nodes.push(BvhNode::zeroed());
                nodes.push(BvhNode::zeroed());
                nodes[node] = interior(&bb, left);
                stack.push((left + 1, start + mid, end, level + 1));
                stack.push((left, start, start + mid, level + 1));
            }
            None => nodes[node] = leaf(&bb, base + start, end - start),
        }
    }
    nodes
}

/// The top of a parallel build: interior nodes down to ranges of `TASK_PRIMS` primitives,
/// which are whole single-threaded subtrees.
enum Top {
    Node { bb: Aabb, children: Box<[Top; 2]> },
    Subtree(Vec<BvhNode>),
}

fn build_top(
    bounds: &[Aabb],
    centroids: &[[f32; 3]],
    prims: &mut [u32],
    base: usize,
    forks: usize,
    depth: usize,
) -> Top {
    if prims.len() <= TASK_PRIMS || depth >= MAX_TOP_DEPTH {
        return Top::Subtree(build_sequential(bounds, centroids, prims, base, depth));
    }
    let n = prims.len();
    let (bb, mid) = split_range(bounds, centroids, prims, depth);
    // A range this large always splits (median fallback); a leaf is only defensive.
    let Some(mid) = mid else {
        return Top::Subtree(vec![leaf(&bb, base, n)]);
    };
    let (lo, hi) = prims.split_at_mut(mid);
    let (left, right) = if forks > 0 {
        std::thread::scope(|s| {
            let left = s.spawn(move || build_top(bounds, centroids, lo, base, forks - 1, depth + 1));
            let right = build_top(bounds, centroids, hi, base + mid, forks - 1, depth + 1);
            let left = left
                .join()
                .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
            (left, right)
        })
    } else {
        (
            build_top(bounds, centroids, lo, base, 0, depth + 1),
            build_top(bounds, centroids, hi, base + mid, 0, depth + 1),
        )
    };
    Top::Node {
        bb,
        children: Box::new([left, right]),
    }
}

/// Lays a parallel build out as one node array (siblings adjacent, depth first, left first).
fn flatten(top: Top) -> Vec<BvhNode> {
    let mut nodes = vec![BvhNode::zeroed()];
    let mut stack = vec![(0usize, top)];
    while let Some((slot, item)) = stack.pop() {
        match item {
            Top::Node { bb, children } => {
                let left = nodes.len();
                nodes.push(BvhNode::zeroed());
                nodes.push(BvhNode::zeroed());
                nodes[slot] = interior(&bb, left);
                let [l, r] = *children;
                stack.push((left + 1, r));
                stack.push((left, l));
            }
            Top::Subtree(sub) => {
                // The subtree's root takes `slot`; its node j ≥ 1 lands at `offset + j − 1`.
                let offset = nodes.len();
                for (j, mut node) in sub.into_iter().enumerate() {
                    if !node.is_leaf() {
                        node.a = (offset + node.a as usize - 1) as u32;
                    }
                    if j == 0 {
                        nodes[slot] = node;
                    } else {
                        nodes.push(node);
                    }
                }
            }
        }
    }
    nodes
}

/// Best SAH split of `prims` as (axis, centroid threshold), or `None` when a leaf is cheaper
/// (and allowed) or the centroids coincide.
fn choose_split(
    bounds: &[Aabb],
    centroids: &[[f32; 3]],
    prims: &[u32],
    bb: &Aabb,
    cb: &Aabb,
) -> Option<(usize, f32)> {
    let n = prims.len();
    let parent_area = bb.area().max(1e-30);
    let leaf_cost = n as f32;
    let mut best: Option<(f32, usize, f32)> = None;
    for (axis, (&lo, &hi)) in cb.min.iter().zip(&cb.max).enumerate() {
        let extent = hi - lo;
        if extent <= 0.0 || !extent.is_finite() {
            continue;
        }
        let scale = BINS as f32 / extent;
        let mut bin_box = [Aabb::EMPTY; BINS];
        let mut bin_count = [0usize; BINS];
        for &i in prims {
            let b = (((centroids[i as usize][axis] - lo) * scale) as usize).min(BINS - 1);
            bin_box[b].grow(&bounds[i as usize]);
            bin_count[b] += 1;
        }
        // Sweep from the right to get suffix areas/counts.
        let mut right_area = [0.0f32; BINS];
        let mut right_count = [0usize; BINS];
        let mut acc = Aabb::EMPTY;
        let mut cnt = 0usize;
        for b in (1..BINS).rev() {
            acc.grow(&bin_box[b]);
            cnt += bin_count[b];
            right_area[b] = acc.area();
            right_count[b] = cnt;
        }
        let mut acc = Aabb::EMPTY;
        let mut cnt = 0usize;
        for b in 0..BINS - 1 {
            acc.grow(&bin_box[b]);
            cnt += bin_count[b];
            let rc = right_count[b + 1];
            if cnt == 0 || rc == 0 {
                continue;
            }
            let cost =
                TRAVERSAL_COST + (acc.area() * cnt as f32 + right_area[b + 1] * rc as f32) / parent_area;
            if best.is_none_or(|(c, _, _)| cost < c) {
                best = Some((cost, axis, lo + (b + 1) as f32 / scale));
            }
        }
    }
    let (cost, axis, pos) = best?;
    if cost >= leaf_cost && n <= MAX_LEAF {
        return None;
    }
    Some((axis, pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unit_box(x: f32, y: f32, z: f32) -> Aabb {
        Aabb {
            min: [x, y, z],
            max: [x + 1.0, y + 1.0, z + 1.0],
        }
    }

    fn check(bvh: &Bvh, bounds: &[Aabb]) {
        // Every primitive appears exactly once in leaf order and is inside its leaf's box.
        let mut seen = vec![0u32; bounds.len()];
        let mut stack = vec![0usize];
        while let Some(i) = stack.pop() {
            let node = bvh.nodes[i];
            if node.is_leaf() {
                for k in node.a..node.a + node.count {
                    let prim = bvh.order[k as usize] as usize;
                    seen[prim] += 1;
                    for axis in 0..3 {
                        assert!(bounds[prim].min[axis] >= node.min[axis] - 1e-5);
                        assert!(bounds[prim].max[axis] <= node.max[axis] + 1e-5);
                    }
                }
            } else {
                for child in [node.a as usize, node.a as usize + 1] {
                    let c = bvh.nodes[child];
                    for axis in 0..3 {
                        assert!(c.min[axis] >= node.min[axis] - 1e-5);
                        assert!(c.max[axis] <= node.max[axis] + 1e-5);
                    }
                    stack.push(child);
                }
            }
        }
        assert!(
            seen.iter().all(|&s| s == 1),
            "every primitive in exactly one leaf"
        );
    }

    #[test]
    fn builds_a_valid_tree_over_a_grid() {
        let mut bounds = Vec::new();
        for x in 0..20 {
            for z in 0..15 {
                bounds.push(unit_box(x as f32 * 2.0, (x * z % 7) as f32, z as f32 * 2.0));
            }
        }
        let bvh = Bvh::build(&bounds);
        check(&bvh, &bounds);
        assert!(bvh.nodes.len() > 1);
        let leaves = bvh.nodes.iter().filter(|n| n.is_leaf()).count();
        assert!(leaves >= bounds.len() / MAX_LEAF);
    }

    #[test]
    fn coincident_centroids_still_terminate() {
        let bounds = vec![unit_box(0.0, 0.0, 0.0); 37];
        let bvh = Bvh::build(&bounds);
        check(&bvh, &bounds);
        assert!(
            bvh.nodes
                .iter()
                .all(|n| !n.is_leaf() || n.count as usize <= MAX_LEAF)
        );
    }

    /// Levels below the root of the deepest leaf.
    fn max_depth(bvh: &Bvh) -> usize {
        let mut deepest = 0;
        let mut stack = vec![(0usize, 0usize)];
        while let Some((i, depth)) = stack.pop() {
            let node = bvh.nodes[i];
            if node.is_leaf() {
                deepest = deepest.max(depth);
            } else {
                stack.push((node.a as usize, depth + 1));
                stack.push((node.a as usize + 1, depth + 1));
            }
        }
        deepest
    }

    /// Three arms of small boxes along the axes, each box 16.5 times farther out than the next:
    /// every bin boundary of the binned SAH leaves only the outermost box of an arm on its far
    /// side, so the build peels one box per level (about 90 levels here).
    fn nested(per_arm: usize) -> Vec<Aabb> {
        let mut out = Vec::new();
        for axis in 0..3 {
            for i in 0..per_arm {
                let at = 16.5f32.powi(-(i as i32));
                let mut min = [0.0f32; 3];
                let mut max = [0.0f32; 3];
                min[axis] = at;
                max[axis] = at * 1.001;
                for other in (0..3).filter(|&a| a != axis) {
                    max[other] = at * 0.001;
                }
                out.push(Aabb { min, max });
            }
        }
        out
    }

    /// Audit v034 WGSL H1: `pt.wgsl` walks a hierarchy with a fixed stack of `MAX_DEPTH` (48)
    /// entries and drops what does not fit, so no input may build a deeper tree. (The audit's
    /// boxes growing by 2 per step peel several per level and stay shallow; the arms below do
    /// not.)
    #[test]
    fn no_input_builds_deeper_than_the_shader_stack() {
        let bounds = nested(30);
        let bvh = Bvh::build_with_threads(&bounds, 1);
        check(&bvh, &bounds);
        eprintln!("arms: depth {}", max_depth(&bvh));
        assert!(max_depth(&bvh) <= 48, "depth {}", max_depth(&bvh));

        // Above the parallel threshold: a long nested chain over a field of small boxes.
        let mut bounds = scattered(PARALLEL_MIN_PRIMS + 4_000);
        bounds.extend(nested(30));
        let one = Bvh::build_with_threads(&bounds, 1);
        check(&one, &bounds);
        assert!(max_depth(&one) <= 48, "depth {}", max_depth(&one));
        let many = Bvh::build_with_threads(&bounds, 8);
        assert_eq!(many.order, one.order, "the depth cap does not depend on threads");
        assert_eq!(many.nodes.len(), one.nodes.len());
        // The cap is the shader's stack.
        let shader = include_str!("../shaders/pt.wgsl");
        assert!(
            shader.contains(&format!("const BVH_STACK: u32 = {MAX_DEPTH}u;")),
            "pt.wgsl's BVH_STACK is not MAX_DEPTH ({MAX_DEPTH})"
        );
    }

    #[test]
    fn empty_and_single_inputs() {
        let empty = Bvh::build(&[]);
        assert_eq!(empty.nodes.len(), 1);
        let one = Bvh::build(&[unit_box(1.0, 2.0, 3.0)]);
        assert_eq!(one.nodes.len(), 1);
        assert_eq!(one.nodes[0].count, 1);
        assert_eq!(one.bounds().min, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn rebase_shifts_children_and_prims() {
        let bounds: Vec<Aabb> = (0..10).map(|i| unit_box(i as f32 * 3.0, 0.0, 0.0)).collect();
        let mut bvh = Bvh::build(&bounds);
        let before = bvh.nodes.clone();
        bvh.rebase(100, 1000);
        for (a, b) in before.iter().zip(&bvh.nodes) {
            if a.is_leaf() {
                assert_eq!(b.a, a.a + 1000);
            } else {
                assert_eq!(b.a, a.a + 100);
            }
        }
    }

    /// `n` boxes from a fixed linear congruential sequence: clusters of small boxes plus a few
    /// long ones, the mix a scattered map gives.
    fn scattered(n: usize) -> Vec<Aabb> {
        let mut state = 0x2545_f491_u32;
        let mut next = || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32
        };
        (0..n)
            .map(|i| {
                let cluster = (i % 7) as f32 * 40.0;
                let p = [cluster + next() * 30.0, next() * 10.0, next() * 200.0];
                let size = if i.is_multiple_of(97) { 25.0 } else { 0.2 + next() };
                Aabb {
                    min: p,
                    max: [p[0] + size * next(), p[1] + size * next(), p[2] + size * next()],
                }
            })
            .collect()
    }

    /// The leaves of `bvh` as (first primitive, count), sorted: its topology without the node
    /// order.
    fn leaves(bvh: &Bvh) -> Vec<(u32, u32)> {
        let mut out: Vec<(u32, u32)> = bvh
            .nodes
            .iter()
            .filter(|n| n.is_leaf())
            .map(|n| (n.a, n.count))
            .collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn parallel_builds_do_not_depend_on_the_thread_count() {
        let bounds = scattered(3 * PARALLEL_MIN_PRIMS);
        let one = Bvh::build_with_threads(&bounds, 1);
        check(&one, &bounds);
        for threads in [2, 3, 8, 64] {
            let many = Bvh::build_with_threads(&bounds, threads);
            assert_eq!(many.nodes, one.nodes, "{threads} threads");
            assert_eq!(many.order, one.order, "{threads} threads");
        }
    }

    #[test]
    fn a_parallel_build_is_the_single_threaded_tree_laid_out_again() {
        let bounds = scattered(2 * PARALLEL_MIN_PRIMS + 123);
        let parallel = Bvh::build_with_threads(&bounds, 8);
        check(&parallel, &bounds);
        let centroids: Vec<[f32; 3]> = bounds.iter().map(Aabb::centroid).collect();
        let mut order: Vec<u32> = (0..bounds.len() as u32).collect();
        let sequential = Bvh {
            nodes: build_sequential(&bounds, &centroids, &mut order, 0, 0),
            order,
        };
        // Same SAH decisions: the same primitive order, leaves and node count, so the same
        // cost up to the order of the sum.
        assert_eq!(parallel.order, sequential.order);
        assert_eq!(leaves(&parallel), leaves(&sequential));
        assert_eq!(parallel.nodes.len(), sequential.nodes.len());
        assert_eq!(parallel.nodes[0], sequential.nodes[0], "same root box");
        let (a, b) = (parallel.sah_cost(), sequential.sah_cost());
        assert!((a - b).abs() <= 1e-4 * b, "{a} vs {b}");
    }

    #[test]
    fn small_inputs_keep_the_single_threaded_layout() {
        let bounds = scattered(PARALLEL_MIN_PRIMS - 1);
        let built = Bvh::build_with_threads(&bounds, 16);
        let centroids: Vec<[f32; 3]> = bounds.iter().map(Aabb::centroid).collect();
        let mut order: Vec<u32> = (0..bounds.len() as u32).collect();
        assert_eq!(
            built.nodes,
            build_sequential(&bounds, &centroids, &mut order, 0, 0)
        );
        assert_eq!(built.order, order);
    }

    #[test]
    fn sah_cost_prefers_the_tree_that_separates_clusters() {
        let mut bounds = Vec::new();
        for i in 0..64 {
            bounds.push(unit_box(i as f32 * 0.01, 0.0, 0.0));
            bounds.push(unit_box(500.0 + i as f32 * 0.01, 0.0, 0.0));
        }
        let sah = Bvh::build(&bounds);
        // A worse tree: median splits by index of an interleaved order mix the clusters.
        let mut nodes = vec![BvhNode::zeroed()];
        let mut stack = vec![(0usize, 0usize, bounds.len())];
        while let Some((node, start, end)) = stack.pop() {
            let mut bb = Aabb::EMPTY;
            for b in &bounds[start..end] {
                bb.grow(b);
            }
            if end - start <= MAX_LEAF {
                nodes[node] = leaf(&bb, start, end - start);
                continue;
            }
            let mid = start + (end - start) / 2;
            let left = nodes.len();
            nodes.push(BvhNode::zeroed());
            nodes.push(BvhNode::zeroed());
            nodes[node] = interior(&bb, left);
            stack.push((left + 1, mid, end));
            stack.push((left, start, mid));
        }
        let naive = Bvh {
            nodes,
            order: (0..bounds.len() as u32).collect(),
        };
        assert!(
            sah.sah_cost() < 0.5 * naive.sah_cost(),
            "{} vs {}",
            sah.sah_cost(),
            naive.sah_cost()
        );
        assert_eq!(Bvh::build(&[]).sah_cost(), 0.0);
    }

    /// Levels below the root of the deepest node (the root alone is depth 0).
    fn tree_depth(bvh: &Bvh) -> usize {
        let mut deepest = 0;
        let mut stack = vec![(0usize, 0usize)];
        while let Some((i, depth)) = stack.pop() {
            deepest = deepest.max(depth);
            let node = bvh.nodes[i];
            if !node.is_leaf() {
                stack.push((node.a as usize, depth + 1));
                stack.push((node.a as usize + 1, depth + 1));
            }
        }
        deepest
    }

    /// `BVH_STACK` of `pt.wgsl`: the traversal pushes at most one far child per level, so a
    /// tree no deeper than this never overflows the stack (audit-v035-wgsl 1).
    fn shader_stack() -> usize {
        let source = include_str!("../shaders/pt.wgsl");
        let line = source
            .lines()
            .find(|l| l.starts_with("const BVH_STACK: u32 = "))
            .expect("pt.wgsl declares BVH_STACK");
        line.trim_start_matches("const BVH_STACK: u32 = ")
            .trim_end_matches("u;")
            .parse()
            .expect("BVH_STACK is a number")
    }

    /// A grid core of `core` unit boxes plus a chain of `per_axis` outliers on each axis, at
    /// alternating sides and doubling distances: each SAH split peels the farthest outlier
    /// off, one level at a time (the audit's "dense core plus outliers" mesh).
    fn outlier_chains(core: usize, per_axis: i32) -> Vec<Aabb> {
        let side = (core as f64).cbrt().round() as usize + 1;
        let mut bounds: Vec<Aabb> = (0..core)
            .map(|i| {
                unit_box(
                    (i % side) as f32,
                    ((i / side) % side) as f32,
                    (i / (side * side)) as f32,
                )
            })
            .collect();
        for axis in 0..3 {
            for i in 1..=per_axis {
                let mut p = [0.5f32; 3];
                p[axis] = 2f32.powi(i) * if i % 2 == 1 { 1.0 } else { -1.0 };
                bounds.push(unit_box(p[0], p[1], p[2]));
            }
        }
        bounds
    }

    /// A tree deeper than the shader's traversal stack would make the path tracer skip the
    /// far subtrees below level `BVH_STACK` (missing geometry, light leaks): the builder caps
    /// the depth, in the single-threaded and the parallel build alike (audit-v035-wgsl 1).
    #[test]
    fn outlier_chains_stay_within_the_shader_traversal_stack() {
        let stack = shader_stack();
        for (core, per_axis) in [(64, 56), (PARALLEL_MIN_PRIMS + 616, 50)] {
            let bounds = outlier_chains(core, per_axis);
            for threads in [1, 4] {
                let bvh = Bvh::build_with_threads(&bounds, threads);
                check(&bvh, &bounds);
                let depth = tree_depth(&bvh);
                assert!(
                    depth <= stack,
                    "{} prims, {threads} threads: depth {depth} > shader stack {stack}",
                    bounds.len()
                );
            }
        }
    }

    /// The builder's cap is the shader's stack.
    #[test]
    fn the_depth_cap_is_the_shader_traversal_stack() {
        assert_eq!(MAX_DEPTH, shader_stack());
    }

    /// The cap does not touch ordinary trees: a scattered map far from the cap builds the same
    /// nodes as before (its depth is well under the stack).
    #[test]
    fn ordinary_trees_are_far_from_the_depth_cap() {
        let bounds = scattered(PARALLEL_MIN_PRIMS + 5);
        let bvh = Bvh::build_with_threads(&bounds, 4);
        assert!(
            tree_depth(&bvh) + 8 < shader_stack(),
            "depth {}",
            tree_depth(&bvh)
        );
    }

    #[test]
    fn sah_separates_two_clusters() {
        let mut bounds = Vec::new();
        for i in 0..8 {
            bounds.push(unit_box(i as f32 * 0.1, 0.0, 0.0));
            bounds.push(unit_box(100.0 + i as f32 * 0.1, 0.0, 0.0));
        }
        let bvh = Bvh::build(&bounds);
        let root = bvh.nodes[0];
        assert!(!root.is_leaf());
        let left = bvh.nodes[root.a as usize];
        let right = bvh.nodes[root.a as usize + 1];
        // The first split is the gap between the clusters.
        assert!(left.max[0] < 50.0 || right.max[0] < 50.0);
        assert!(left.min[0] > 50.0 || right.min[0] > 50.0);
    }
}
