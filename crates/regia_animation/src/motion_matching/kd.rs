// SPDX-License-Identifier: MPL-2.0
//! An exact k-d tree over fixed-length feature rows, for the motion-matching search.
//!
//! The tree splits on the dimension with the largest spread at the median point (Bentley 1975;
//! Friedman, Bentley and Finkel 1977), keeps up to `LEAF_SIZE` rows per leaf, and answers the
//! single nearest neighbor by squared Euclidean distance with the classic branch-and-bound: visit
//! the near side first, then the far side only when the splitting plane is closer than the best
//! row found so far. The search is exact, and it breaks ties the way the brute-force scan does
//! (the lowest row index wins), so both always return the same row.

/// Rows per leaf by default. Small leaves prune more; this size keeps the node count low for the
/// 20-40-dimension rows motion matching uses.
pub const LEAF_SIZE: usize = 8;

/// The leaf sizes [`KdTree::build_with_leaf_size`] accepts.
pub const LEAF_SIZE_RANGE: std::ops::RangeInclusive<usize> = 4..=32;

/// Relative slack on the pruning bound of a biased search (`2^-20`).
const PRUNE_SLACK: f32 = 1.0 / 1_048_576.0;

/// One tree node.
#[derive(Clone, Copy, Debug, PartialEq)]
enum KdNode {
    /// Rows `order[start..end]`.
    Leaf { start: u32, end: u32 },
    /// Rows with `row[dim] <= value` go left, rows with `row[dim] >= value` go right; `value`
    /// is row `pivot`'s coordinate.
    Split {
        dim: u32,
        value: f32,
        pivot: u32,
        left: u32,
        right: u32,
    },
}

/// The tree's arrays for the cooked database file (rows are not stored: the loader recomputes
/// them). Nodes are `(kind, a, value, b, left, right)`: a leaf is `(0, start, 0, end, 0, 0)`, a
/// split `(1, dim, value, pivot, left, right)`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(super) struct KdParts {
    dims: u32,
    leaf_size: u32,
    depth: u32,
    order: Vec<u32>,
    nodes: Vec<(u8, u32, f32, u32, u32, u32)>,
}

/// Node and depth counts of a built [`KdTree`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KdStats {
    /// Every node, splits and leaves.
    pub nodes: usize,
    /// Leaf nodes.
    pub leaves: usize,
    /// Longest root-to-leaf path, in nodes (1 for a tree that is a single leaf).
    pub depth: usize,
}

/// The nearest row a search found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Nearest {
    /// Row index in the order the rows were given.
    pub row: usize,
    /// Squared Euclidean distance from the query.
    pub distance: f32,
}

/// An exact nearest-neighbor index over `rows × dims` floats (row-major).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KdTree {
    dims: usize,
    rows: Vec<f32>,
    order: Vec<u32>,
    nodes: Vec<KdNode>,
    depth: usize,
    leaf_size: usize,
}

/// Work counters of one search (for the `motion_matching.search` span and the agent read).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchWork {
    /// Tree nodes visited (0 for a scan).
    pub nodes_visited: usize,
    /// Rows whose distance was computed (fully or until the early out).
    pub rows_scanned: usize,
}

impl KdTree {
    /// Build the tree over `rows`, `dims` floats per row. Returns `None` when `dims` is zero,
    /// `rows.len()` is not a multiple of `dims`, or there are more rows than `u32` indexes.
    pub fn build(rows: &[f32], dims: usize) -> Option<Self> {
        Self::build_with_leaf_size(rows, dims, LEAF_SIZE)
    }

    /// [`KdTree::build`] with `leaf_size` rows per leaf (clamped to [`LEAF_SIZE_RANGE`]).
    pub fn build_with_leaf_size(rows: &[f32], dims: usize, leaf_size: usize) -> Option<Self> {
        if dims == 0 || !rows.len().is_multiple_of(dims) {
            return None;
        }
        let count = rows.len() / dims;
        let count_u32 = u32::try_from(count).ok()?;
        let mut tree = KdTree {
            dims,
            rows: rows.to_vec(),
            order: (0..count_u32).collect(),
            nodes: Vec::new(),
            depth: 0,
            leaf_size: leaf_size.clamp(*LEAF_SIZE_RANGE.start(), *LEAF_SIZE_RANGE.end()),
        };
        if count > 0 {
            let mut order = std::mem::take(&mut tree.order);
            tree.depth = tree.build_node(&mut order, 0, 1);
            tree.order = order;
        }
        Some(tree)
    }

    /// The same tree over new rows: every split value is re-read from its pivot row. Exact
    /// (the partition stays valid) when each dimension of the new rows is a non-decreasing
    /// function of the old one, for example the old rows rescaled per dimension by a factor
    /// `>= 0`. `None` when `rows` has a different shape.
    pub fn with_rows(&self, rows: Vec<f32>) -> Option<Self> {
        if rows.len() != self.rows.len() {
            return None;
        }
        let mut tree = self.clone();
        for node in &mut tree.nodes {
            if let KdNode::Split {
                dim, value, pivot, ..
            } = node
            {
                *value = rows[*pivot as usize * tree.dims + *dim as usize];
            }
        }
        tree.rows = rows;
        Some(tree)
    }

    /// The arrays of the tree (for the cooked file).
    pub(super) fn to_parts(&self) -> KdParts {
        KdParts {
            dims: self.dims as u32,
            leaf_size: self.leaf_size as u32,
            depth: self.depth as u32,
            order: self.order.clone(),
            nodes: self
                .nodes
                .iter()
                .map(|n| match *n {
                    KdNode::Leaf { start, end } => (0, start, 0.0, end, 0, 0),
                    KdNode::Split {
                        dim,
                        value,
                        pivot,
                        left,
                        right,
                    } => (1, dim, value, pivot, left, right),
                })
                .collect(),
        }
    }

    /// Rebuild a tree from its arrays over `rows`. `None` when anything is out of range, a child
    /// does not follow its parent, or a split value is not its pivot row's coordinate.
    pub(super) fn from_parts(parts: KdParts, rows: Vec<f32>, dims: usize) -> Option<Self> {
        if dims == 0 || parts.dims as usize != dims || !rows.len().is_multiple_of(dims) {
            return None;
        }
        let count = rows.len() / dims;
        if parts.order.len() != count || parts.order.iter().any(|&r| r as usize >= count) {
            return None;
        }
        let total = parts.nodes.len();
        let mut nodes = Vec::with_capacity(total);
        for (index, &(kind, a, value, b, left, right)) in parts.nodes.iter().enumerate() {
            let node = match kind {
                0 if a <= b && b as usize <= count => KdNode::Leaf { start: a, end: b },
                1 if (a as usize) < dims
                    && (b as usize) < count
                    && (left as usize) > index
                    && (right as usize) > index
                    && (left as usize) < total
                    && (right as usize) < total
                    && rows[b as usize * dims + a as usize].to_bits() == value.to_bits() =>
                {
                    KdNode::Split {
                        dim: a,
                        value,
                        pivot: b,
                        left,
                        right,
                    }
                }
                _ => return None,
            };
            nodes.push(node);
        }
        if count > 0 && nodes.is_empty() {
            return None;
        }
        Some(KdTree {
            dims,
            rows,
            order: parts.order,
            nodes,
            depth: parts.depth as usize,
            leaf_size: (parts.leaf_size as usize).clamp(*LEAF_SIZE_RANGE.start(), *LEAF_SIZE_RANGE.end()),
        })
    }

    /// Rows per leaf the tree was built with.
    pub fn leaf_size(&self) -> usize {
        self.leaf_size
    }

    /// Floats per row.
    pub fn dims(&self) -> usize {
        self.dims
    }

    /// Number of rows.
    pub fn len(&self) -> usize {
        self.order.len()
    }

    /// True when the tree holds no rows.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Node and depth counts.
    pub fn stats(&self) -> KdStats {
        KdStats {
            nodes: self.nodes.len(),
            leaves: self
                .nodes
                .iter()
                .filter(|n| matches!(n, KdNode::Leaf { .. }))
                .count(),
            depth: self.depth,
        }
    }

    /// The row nearest to `query` (squared Euclidean distance), skipping rows for which
    /// `excluded(row)` is true. Ties go to the lowest row index. Returns `None` when the query
    /// has the wrong length or every row is excluded.
    pub fn nearest(&self, query: &[f32], excluded: impl Fn(usize) -> bool) -> Option<Nearest> {
        if query.len() != self.dims || self.nodes.is_empty() {
            return None;
        }
        let mut best: Option<Nearest> = None;
        self.search_node(0, query, &excluded, &mut best);
        best
    }

    /// The row with the lowest `distance + bias(row)`, skipping rows for which `excluded(row)`
    /// is true. Ties on the biased cost go to the lowest row index. `min_bias` must be a lower
    /// bound of every `bias(row)` (use 0 when no bias is negative): the far side of a split is
    /// pruned only when even the most favorable bias cannot beat the best cost found, with a
    /// `1 + 2^-20` slack so rounding never changes the winner. The returned `distance` is the
    /// biased cost.
    pub fn nearest_biased(
        &self,
        query: &[f32],
        excluded: impl Fn(usize) -> bool,
        bias: impl Fn(usize) -> f32,
        min_bias: f32,
    ) -> Option<Nearest> {
        if query.len() != self.dims || self.nodes.is_empty() {
            return None;
        }
        self.nearest_biased_counted(query, excluded, bias, min_bias).0
    }

    /// [`KdTree::nearest_biased`] that also reports the work it did.
    pub fn nearest_biased_counted(
        &self,
        query: &[f32],
        excluded: impl Fn(usize) -> bool,
        bias: impl Fn(usize) -> f32,
        min_bias: f32,
    ) -> (Option<Nearest>, SearchWork) {
        let mut work = SearchWork::default();
        if query.len() != self.dims || self.nodes.is_empty() {
            return (None, work);
        }
        let mut best = Vec::with_capacity(1);
        let search = KSearch {
            query,
            excluded,
            bias,
            min_bias: min_bias.min(0.0),
            k: 1,
        };
        self.search_node_k(0, &search, &mut best, &mut work);
        (best.first().copied(), work)
    }

    /// The `k` rows with the lowest `distance + bias(row)`, cheapest first, ties by row index,
    /// skipping excluded rows. Equal to sorting every eligible row by `(biased cost, row)` and
    /// keeping the first `k`. `min_bias` as in [`KdTree::nearest_biased`].
    pub fn nearest_k_biased(
        &self,
        query: &[f32],
        excluded: impl Fn(usize) -> bool,
        bias: impl Fn(usize) -> f32,
        min_bias: f32,
        k: usize,
    ) -> (Vec<Nearest>, SearchWork) {
        let mut work = SearchWork::default();
        if query.len() != self.dims || self.nodes.is_empty() || k == 0 {
            return (Vec::new(), work);
        }
        let mut best = Vec::with_capacity(k);
        let search = KSearch {
            query,
            excluded,
            bias,
            min_bias: min_bias.min(0.0),
            k,
        };
        self.search_node_k(0, &search, &mut best, &mut work);
        (best, work)
    }

    /// Branch-and-bound for the `k` best rows; `best` stays sorted by `(distance, row)` and holds
    /// at most `k` entries. The far side is skipped only when its plane bound, lowered by the
    /// most negative bias, exceeds the current `k`-th best with a `1 + 2^-20` slack.
    fn search_node_k<E: Fn(usize) -> bool, B: Fn(usize) -> f32>(
        &self,
        node: u32,
        search: &KSearch<'_, E, B>,
        best: &mut Vec<Nearest>,
        work: &mut SearchWork,
    ) {
        work.nodes_visited += 1;
        match self.nodes[node as usize] {
            KdNode::Leaf { start, end } => {
                for &row in &self.order[start as usize..end as usize] {
                    let row = row as usize;
                    if (search.excluded)(row) {
                        continue;
                    }
                    work.rows_scanned += 1;
                    let distance = squared_distance(search.query, self.row(row)) + (search.bias)(row);
                    insert_k(best, search.k, Nearest { row, distance });
                }
            }
            KdNode::Split {
                dim,
                value,
                left,
                right,
                ..
            } => {
                let diff = search.query[dim as usize] - value;
                let (near, far) = if diff < 0.0 { (left, right) } else { (right, left) };
                self.search_node_k(near, search, best, work);
                let bound = diff * diff + search.min_bias;
                let visit = best.len() < search.k
                    || best
                        .last()
                        .is_none_or(|b| bound <= b.distance + b.distance.abs() * PRUNE_SLACK);
                if visit {
                    self.search_node_k(far, search, best, work);
                }
            }
        }
    }

    /// Row `row` of the indexed data.
    fn row(&self, row: usize) -> &[f32] {
        &self.rows[row * self.dims..(row + 1) * self.dims]
    }

    /// Build the subtree for `order` and return its depth. The node is pushed first, so the
    /// root is node 0 and children always follow their parent.
    fn build_node(&mut self, order: &mut [u32], offset: usize, depth: usize) -> usize {
        let index = self.nodes.len();
        let leaf = KdNode::Leaf {
            start: offset as u32,
            end: (offset + order.len()) as u32,
        };
        self.nodes.push(leaf);
        if order.len() <= self.leaf_size {
            return depth;
        }
        // Split on the dimension with the largest spread (lowest dimension on ties).
        let mut split_dim = 0usize;
        let mut split_spread = 0.0f32;
        for dim in 0..self.dims {
            let mut lo = f32::INFINITY;
            let mut hi = f32::NEG_INFINITY;
            for &row in order.iter() {
                let v = self.rows[row as usize * self.dims + dim];
                lo = lo.min(v);
                hi = hi.max(v);
            }
            let spread = hi - lo;
            if spread > split_spread {
                split_spread = spread;
                split_dim = dim;
            }
        }
        if split_spread <= 0.0 || !split_spread.is_finite() {
            // Every row is the same point (or the data is not finite): keep one leaf.
            return depth;
        }
        let dims = self.dims;
        let rows = &self.rows;
        let coordinate = |row: u32| rows[row as usize * dims + split_dim];
        let mid = order.len() / 2;
        // A total order (value, then row index) makes the split deterministic.
        order.select_nth_unstable_by(mid, |&a, &b| {
            coordinate(a).total_cmp(&coordinate(b)).then(a.cmp(&b))
        });
        let value = coordinate(order[mid]);
        let pivot = order[mid];
        let (left_rows, right_rows) = order.split_at_mut(mid);
        let left = self.nodes.len() as u32;
        let left_depth = self.build_node(left_rows, offset, depth + 1);
        let right = self.nodes.len() as u32;
        let right_depth = self.build_node(right_rows, offset + mid, depth + 1);
        self.nodes[index] = KdNode::Split {
            dim: split_dim as u32,
            value,
            pivot,
            left,
            right,
        };
        left_depth.max(right_depth)
    }

    fn search_node(
        &self,
        node: u32,
        query: &[f32],
        excluded: &impl Fn(usize) -> bool,
        best: &mut Option<Nearest>,
    ) {
        match self.nodes[node as usize] {
            KdNode::Leaf { start, end } => {
                for &row in &self.order[start as usize..end as usize] {
                    let row = row as usize;
                    if excluded(row) {
                        continue;
                    }
                    let distance = squared_distance(query, self.row(row));
                    if is_better(distance, row, *best) {
                        *best = Some(Nearest { row, distance });
                    }
                }
            }
            KdNode::Split {
                dim,
                value,
                left,
                right,
                ..
            } => {
                let diff = query[dim as usize] - value;
                let (near, far) = if diff < 0.0 { (left, right) } else { (right, left) };
                self.search_node(near, query, excluded, best);
                // Every row on the far side is at least `diff²` away. Equal bounds are still
                // visited: a row there may tie the best distance with a lower index.
                let bound = diff * diff;
                if best.is_none_or(|b| bound <= b.distance) {
                    self.search_node(far, query, excluded, best);
                }
            }
        }
    }
}

/// The fixed inputs of one k-best search.
struct KSearch<'q, E, B> {
    query: &'q [f32],
    excluded: E,
    bias: B,
    min_bias: f32,
    k: usize,
}

/// Insert `candidate` into the sorted list `best` (at most `k` entries, by `(distance, row)`).
fn insert_k(best: &mut Vec<Nearest>, k: usize, candidate: Nearest) {
    let before = |b: &Nearest| {
        b.distance < candidate.distance || (b.distance == candidate.distance && b.row < candidate.row)
    };
    let at = best.partition_point(before);
    if at >= k {
        return;
    }
    if best.len() == k {
        best.pop();
    }
    best.insert(at, candidate);
}

/// True when `(distance, row)` beats `best`: a smaller distance, or the same distance at a lower
/// row index.
fn is_better(distance: f32, row: usize, best: Option<Nearest>) -> bool {
    match best {
        None => true,
        Some(b) => distance < b.distance || (distance == b.distance && row < b.row),
    }
}

/// Squared Euclidean distance in the canonical eight-lane order ([`super::lanes::lane_distance`]).
/// Every search method uses this one function, so equal rows give bit-identical distances.
pub fn squared_distance(a: &[f32], b: &[f32]) -> f32 {
    super::lanes::lane_distance(a, b)
}

/// The row nearest to `query` by a linear scan over `rows` (`dims` floats per row), skipping
/// excluded rows; ties go to the lowest row index. The reference [`KdTree::nearest`] must match.
pub fn nearest_brute_force(
    rows: &[f32],
    dims: usize,
    query: &[f32],
    excluded: impl Fn(usize) -> bool,
) -> Option<Nearest> {
    if dims == 0 || query.len() != dims {
        return None;
    }
    let mut best: Option<Nearest> = None;
    for (row, values) in rows.chunks_exact(dims).enumerate() {
        if excluded(row) {
            continue;
        }
        // Rows come in increasing index order, so a later row only wins with a strictly
        // smaller distance: one whose partial sum already exceeds the best can stop early.
        let limit = best.map_or(f32::INFINITY, |b| b.distance);
        let Some(distance) = super::lanes::lane_distance_bounded(query, values, limit) else {
            continue;
        };
        if is_better(distance, row, best) {
            best = Some(Nearest { row, distance });
        }
    }
    best
}

/// The row with the lowest `distance + bias(row)` by a linear scan; ties go to the lowest row
/// index. The reference [`KdTree::nearest_biased`] must match.
pub fn nearest_brute_force_biased(
    rows: &[f32],
    dims: usize,
    query: &[f32],
    excluded: impl Fn(usize) -> bool,
    bias: impl Fn(usize) -> f32,
) -> Option<Nearest> {
    if dims == 0 || query.len() != dims {
        return None;
    }
    nearest_brute_force_biased_counted(rows, dims, query, excluded, bias).0
}

/// [`nearest_brute_force_biased`] that also reports the rows it scanned. The scan stops a row
/// early once its partial distance exceeds the best biased cost minus the row's own bias, with
/// a `1 + 2^-20` slack so rounding can never drop the winner.
pub fn nearest_brute_force_biased_counted(
    rows: &[f32],
    dims: usize,
    query: &[f32],
    excluded: impl Fn(usize) -> bool,
    bias: impl Fn(usize) -> f32,
) -> (Option<Nearest>, SearchWork) {
    let mut work = SearchWork::default();
    if dims == 0 || query.len() != dims {
        return (None, work);
    }
    let mut best: Option<Nearest> = None;
    for (row, values) in rows.chunks_exact(dims).enumerate() {
        if excluded(row) {
            continue;
        }
        work.rows_scanned += 1;
        let own = bias(row);
        let limit = best.map_or(f32::INFINITY, |b| {
            (b.distance - own) + (b.distance.abs() + own.abs()) * PRUNE_SLACK
        });
        let Some(distance) = super::lanes::lane_distance_bounded(query, values, limit) else {
            continue;
        };
        let distance = distance + own;
        if is_better(distance, row, best) {
            best = Some(Nearest { row, distance });
        }
    }
    (best, work)
}
