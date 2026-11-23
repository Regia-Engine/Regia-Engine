// SPDX-License-Identifier: MPL-2.0
//! A stackless triangle BVH for shadow rays on adapters without ray queries (ADR-515).
//!
//! Built top-down by splitting each node's triangles at the median centroid on the longest axis
//! of the centroid bounds (the "equal counts" split of Pharr, Jakob, Humphreys, *Physically
//! Based Rendering* 4th ed., § 7.3), and flattened depth-first. Every node stores a skip
//! ("escape") index: the node to visit when the ray misses this node's box or finishes its
//! subtree (Smits, "Efficiency Issues for Ray Tracing", JGT 1998). Traversal is then a single
//! forward loop with no stack, which suits WGSL: ADR-026 forbids the function-local array a
//! stack would need. Shadow rays are any-hit: an opaque triangle stops the ray, and a masked
//! triangle stops it only when [`any_hit`] accepts (ADR-4557). A rejected candidate does not
//! end the walk.
//!
//! The CPU tracer here and `bvh_walk.wgsl` run the same tests in the same order: the CPU
//! result is the reference the GPU passes are compared against.

use bytemuck::{Pod, Zeroable};

/// Most triangles one leaf holds.
pub const MAX_LEAF_TRIANGLES: usize = 4;

/// Most triangles one tree holds: a leaf packs its first triangle in the upper 28 bits.
pub const MAX_BVH_TRIANGLES: usize = 1 << 28;

/// Directions closer to zero than this per axis are nudged to it before the slab test, so the
/// reciprocal is finite on the CPU and the GPU alike.
pub const MIN_DIRECTION_COMPONENT: f32 = 1e-9;

/// Determinants smaller than this are a ray parallel to the triangle (no hit).
const PARALLEL_EPSILON: f32 = 1e-12;

/// One node of the flattened tree (32 bytes, the WGSL `BvhNode` layout).
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct BvhNode {
    /// Box minimum corner (slightly padded, so the box is conservative).
    pub min: [f32; 3],
    /// Node visited after this one's subtree: on a box miss, after a leaf, or past the last
    /// node of the subtree. Equal to the node count when traversal ends.
    pub escape: u32,
    /// Box maximum corner.
    pub max: [f32; 3],
    /// `0` for an interior node (its first child is the next node); for a leaf,
    /// `(first_triangle << 4) | count` with `count` in `1..=MAX_LEAF_TRIANGLES`.
    pub prims: u32,
}

impl BvhNode {
    /// Triangles of a leaf as `(first, count)`; `None` for an interior node.
    #[must_use]
    pub fn leaf(&self) -> Option<(u32, u32)> {
        let count = self.prims & 0xF;
        (count != 0).then_some((self.prims >> 4, count))
    }
}

/// One triangle in leaf order (48 bytes: three `vec4<f32>`).
///
/// [`Self::a`]'s `w` is the any-hit coverage fraction (ADR-4557). `0` (what [`ShadowBvh::build`]
/// stores) and any value outside `(0, 1)` mean opaque: the walk commits the hit. A value in
/// `(0, 1)` is the fraction of rays the triangle stops.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct BvhTriangle {
    /// First corner. `w` is the any-hit coverage fraction (`0` = opaque).
    pub a: [f32; 4],
    /// Second corner.
    pub b: [f32; 4],
    /// Third corner.
    pub c: [f32; 4],
}

impl BvhTriangle {
    fn new(t: &[[f32; 3]; 3]) -> Self {
        let p = |v: [f32; 3]| [v[0], v[1], v[2], 0.0];
        Self {
            a: p(t[0]),
            b: p(t[1]),
            c: p(t[2]),
        }
    }

    fn corners(&self) -> [[f32; 3]; 3] {
        let p = |v: [f32; 4]| [v[0], v[1], v[2]];
        [p(self.a), p(self.b), p(self.c)]
    }
}

/// Why [`ShadowBvh::build`] refused its input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BvhError {
    /// More than [`MAX_BVH_TRIANGLES`] triangles.
    TooManyTriangles(usize),
    /// The triangle at this index has a NaN or infinite coordinate.
    NonFinite(usize),
}

impl std::fmt::Display for BvhError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooManyTriangles(n) => write!(f, "{n} triangles (at most {MAX_BVH_TRIANGLES})"),
            Self::NonFinite(i) => write!(f, "triangle {i} has a non-finite coordinate"),
        }
    }
}

impl std::error::Error for BvhError {}

/// A flattened, stackless BVH over world-space triangles.
#[derive(Debug, Clone, PartialEq)]
pub struct ShadowBvh {
    nodes: Vec<BvhNode>,
    triangles: Vec<BvhTriangle>,
    triangle_count: usize,
}

impl ShadowBvh {
    /// Builds the tree over `triangles` (world space). Deterministic: the same input gives the
    /// same nodes. No triangles gives one interior node with no children (traversal ends right
    /// after it) and one degenerate triangle, so the GPU buffers are never zero-sized.
    /// Every triangle is opaque ([`Self::build_with_coverage`] stores a mask).
    pub fn build(triangles: &[[[f32; 3]; 3]]) -> Result<Self, BvhError> {
        Self::build_with_coverage(triangles, &[])
    }

    /// [`Self::build`] with an any-hit coverage fraction per input triangle (ADR-4557).
    ///
    /// `coverage[i]` in `(0, 1)` is the fraction of rays triangle `i` stops. A missing entry,
    /// `0`, or a value outside `(0, 1)` is opaque, which is also what an unused `w` meant
    /// before this channel existed. The fraction is stored in the first corner's `w`.
    pub fn build_with_coverage(triangles: &[[[f32; 3]; 3]], coverage: &[f32]) -> Result<Self, BvhError> {
        if triangles.len() > MAX_BVH_TRIANGLES {
            return Err(BvhError::TooManyTriangles(triangles.len()));
        }
        if let Some(i) = triangles
            .iter()
            .position(|t| t.iter().flatten().any(|c| !c.is_finite()))
        {
            return Err(BvhError::NonFinite(i));
        }
        if triangles.is_empty() {
            return Ok(Self {
                nodes: vec![BvhNode {
                    min: [1.0; 3],
                    escape: 1,
                    max: [-1.0; 3],
                    prims: 0,
                }],
                triangles: vec![BvhTriangle::zeroed()],
                triangle_count: 0,
            });
        }
        let centroids: Vec<[f32; 3]> = triangles
            .iter()
            .map(|t| std::array::from_fn(|k| (t[0][k] + t[1][k] + t[2][k]) / 3.0))
            .collect();
        let mut order: Vec<u32> = (0..triangles.len() as u32).collect();
        let mut out = Self {
            nodes: Vec::with_capacity(2 * triangles.len().div_ceil(MAX_LEAF_TRIANGLES)),
            triangles: Vec::with_capacity(triangles.len()),
            triangle_count: triangles.len(),
        };
        out.emit(&mut order, triangles, &centroids, coverage);
        Ok(out)
    }

    fn emit(
        &mut self,
        items: &mut [u32],
        triangles: &[[[f32; 3]; 3]],
        centroids: &[[f32; 3]],
        coverage: &[f32],
    ) {
        let index = self.nodes.len();
        let (min, max) = padded_bounds(items.iter().flat_map(|&i| triangles[i as usize]));
        self.nodes.push(BvhNode {
            min,
            escape: 0,
            max,
            prims: 0,
        });
        if items.len() <= MAX_LEAF_TRIANGLES {
            let first = self.triangles.len() as u32;
            self.triangles.extend(items.iter().map(|&i| {
                let mut tri = BvhTriangle::new(&triangles[i as usize]);
                if let Some(&fraction) = coverage.get(i as usize)
                    && fraction > 0.0
                    && fraction < 1.0
                {
                    tri.a[3] = fraction;
                }
                tri
            }));
            self.nodes[index].prims = (first << 4) | items.len() as u32;
        } else {
            let (cmin, cmax) = bounds(items.iter().map(|&i| centroids[i as usize]));
            let extent: [f32; 3] = std::array::from_fn(|k| cmax[k] - cmin[k]);
            let axis = if extent[0] >= extent[1] && extent[0] >= extent[2] {
                0
            } else if extent[1] >= extent[2] {
                1
            } else {
                2
            };
            let mid = items.len() / 2;
            items.select_nth_unstable_by(mid, |&a, &b| {
                centroids[a as usize][axis]
                    .total_cmp(&centroids[b as usize][axis])
                    .then(a.cmp(&b))
            });
            let (left, right) = items.split_at_mut(mid);
            self.emit(left, triangles, centroids, coverage);
            self.emit(right, triangles, centroids, coverage);
        }
        self.nodes[index].escape = self.nodes.len() as u32;
    }

    /// The flattened nodes (upload as the WGSL `nodes` buffer).
    #[must_use]
    pub fn nodes(&self) -> &[BvhNode] {
        &self.nodes
    }

    /// The triangles in leaf order (upload as the WGSL `tris` buffer).
    #[must_use]
    pub fn triangles(&self) -> &[BvhTriangle] {
        &self.triangles
    }

    /// Triangles the tree was built from (0 for an empty tree).
    #[must_use]
    pub fn triangle_count(&self) -> usize {
        self.triangle_count
    }

    /// Whether the ray `origin + t * dir`, `t` in `(t_min, t_max)`, hits a triangle the any-hit
    /// accepts. `dir` need not be normalized; `t` is in units of its length. An opaque triangle
    /// accepts every hit. A masked triangle accepts when [`any_hit`] says so. A ray that hits
    /// nothing, or only rejected candidates, is a miss.
    #[must_use]
    pub fn occluded(&self, origin: [f32; 3], dir: [f32; 3], t_min: f32, t_max: f32) -> bool {
        let inv = inverse_direction(dir);
        let count = self.nodes.len();
        let mut i = 0usize;
        while i < count {
            let node = &self.nodes[i];
            let next = node.escape as usize;
            if !ray_hits_box(origin, inv, node.min, node.max, t_min, t_max) {
                if next <= i {
                    break;
                }
                i = next;
                continue;
            }
            match node.leaf() {
                Some((first, n)) => {
                    for t in first..first + n {
                        let Some(tri) = self.triangles.get(t as usize) else {
                            return false;
                        };
                        if ray_hits_triangle(origin, dir, &tri.corners(), t_min, t_max)
                            && any_hit(origin, t, 0, triangle_coverage_byte(tri.a[3]))
                        {
                            return true;
                        }
                    }
                    if next <= i {
                        break;
                    }
                    i = next;
                }
                None => i += 1,
            }
        }
        false
    }
}

/// PCG output permutation (O'Neill 2014). The same function as `masked_hash` in
/// `sun_trace_ray_query.wgsl` and `bvh_walk.wgsl`.
#[must_use]
fn masked_hash(v: u32) -> u32 {
    let state = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let word = ((state >> ((state >> 28) + 4)) ^ state).wrapping_mul(277_803_737);
    (word >> 22) ^ word
}

/// The 8-bit coverage stored in a triangle's `w` ([`BvhTriangle::a`]).
///
/// `0` and any value outside `(0, 1)` are opaque (`255`): that channel used to be unused, so
/// existing trees stay solid. A fraction in `(0, 1)` becomes `floor(fraction * 255 + 0.5)`, the
/// same byte [`crate::hwrt::casters::coverage_custom_data`] packs for `0.5` (128). The walk has no
/// mask texture, so this byte is the whole test.
#[must_use]
pub fn triangle_coverage_byte(coverage_w: f32) -> u32 {
    if coverage_w > 0.0 && coverage_w < 1.0 {
        (coverage_w * 255.0 + 0.5).floor() as u32
    } else {
        255
    }
}

/// Any-hit alpha test for one candidate (ADR-4557).
///
/// Opaque coverage (`coverage_byte >= 255`) accepts the hit and the caller skips the test.
/// A masked candidate is accepted when a hash of the ray origin, the triangle and the instance
/// falls under `coverage_byte`. `instance_index` is `0` on the software walk: its triangles are
/// already in world space. A ray-query candidate passes the instance index from the top-level
/// structure. No mask texture is read. Returning `false` does not end the ray.
///
/// The rule is the candidate confirmation in the Vulkan ray-query specification: a non-opaque
/// intersection is confirmed or ignored, and ignoring it leaves the ray a miss when nothing
/// else confirms.
#[must_use]
pub fn any_hit(origin: [f32; 3], primitive_index: u32, instance_index: u32, coverage_byte: u32) -> bool {
    if coverage_byte >= 255 {
        return true;
    }
    let seed = masked_hash(
        origin[0].to_bits() ^ masked_hash(origin[1].to_bits() ^ masked_hash(origin[2].to_bits())),
    );
    let roll = masked_hash(seed ^ primitive_index.wrapping_mul(2_654_435_761) ^ instance_index) & 0xFF;
    roll < coverage_byte
}

/// [`ShadowBvh::occluded`] without the tree: every triangle, in input order. The test oracle.
///
/// Opaque only: it does not apply [`any_hit`]. Trees built with [`ShadowBvh::build`] agree with
/// it. A masked tree can disagree, on purpose.
#[must_use]
pub fn occluded_brute_force(
    triangles: &[[[f32; 3]; 3]],
    origin: [f32; 3],
    dir: [f32; 3],
    t_min: f32,
    t_max: f32,
) -> bool {
    triangles
        .iter()
        .any(|t| ray_hits_triangle(origin, dir, t, t_min, t_max))
}

fn bounds(points: impl Iterator<Item = [f32; 3]>) -> ([f32; 3], [f32; 3]) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in points {
        min = std::array::from_fn(|k| min[k].min(p[k]));
        max = std::array::from_fn(|k| max[k].max(p[k]));
    }
    (min, max)
}

/// Bounds grown by a small relative margin, so a ray grazing a triangle edge never misses the
/// box that holds the triangle.
fn padded_bounds(points: impl Iterator<Item = [f32; 3]>) -> ([f32; 3], [f32; 3]) {
    let (min, max) = bounds(points);
    let extent = (0..3).map(|k| max[k] - min[k]).fold(0.0f32, f32::max);
    let magnitude = min.iter().chain(&max).map(|c| c.abs()).fold(0.0f32, f32::max);
    let pad = (extent + magnitude) * 1e-5 + 1e-6;
    (min.map(|c| c - pad), max.map(|c| c + pad))
}

/// The reciprocal of `dir` with near-zero components nudged to [`MIN_DIRECTION_COMPONENT`].
#[must_use]
pub fn inverse_direction(dir: [f32; 3]) -> [f32; 3] {
    dir.map(|d| {
        let d = if d.abs() < MIN_DIRECTION_COMPONENT {
            MIN_DIRECTION_COMPONENT
        } else {
            d
        };
        1.0 / d
    })
}

/// Slab test (Kay and Kajiya, SIGGRAPH 1986) of the ray against a box, within `[t_min, t_max]`.
#[must_use]
pub fn ray_hits_box(
    origin: [f32; 3],
    inv: [f32; 3],
    min: [f32; 3],
    max: [f32; 3],
    t_min: f32,
    t_max: f32,
) -> bool {
    let mut near = f32::NEG_INFINITY;
    let mut far = f32::INFINITY;
    for (((lo, hi), o), r) in min.iter().zip(&max).zip(&origin).zip(&inv) {
        let t0 = (lo - o) * r;
        let t1 = (hi - o) * r;
        near = near.max(t0.min(t1));
        far = far.min(t0.max(t1));
    }
    near <= far && far >= t_min && near <= t_max
}

/// Möller and Trumbore, "Fast, Minimum Storage Ray/Triangle Intersection", JGT 1997.
/// Double-sided: a shadow ray is blocked by either face.
#[must_use]
pub fn ray_hits_triangle(
    origin: [f32; 3],
    dir: [f32; 3],
    tri: &[[f32; 3]; 3],
    t_min: f32,
    t_max: f32,
) -> bool {
    let e1 = sub(tri[1], tri[0]);
    let e2 = sub(tri[2], tri[0]);
    let p = cross(dir, e2);
    let det = dot(e1, p);
    if det.abs() < PARALLEL_EPSILON {
        return false;
    }
    let inv_det = 1.0 / det;
    let s = sub(origin, tri[0]);
    let u = dot(s, p) * inv_det;
    if !(0.0..=1.0).contains(&u) {
        return false;
    }
    let q = cross(s, e1);
    let v = dot(dir, q) * inv_det;
    if v < 0.0 || u + v > 1.0 {
        return false;
    }
    let t = dot(e2, q) * inv_det;
    t > t_min && t < t_max
}

fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn cross(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic generator (a 64-bit LCG, Knuth's MMIX constants).
    struct Lcg(u64);

    impl Lcg {
        fn next_f32(&mut self) -> f32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
        }

        fn range(&mut self, lo: f32, hi: f32) -> f32 {
            lo + (hi - lo) * self.next_f32()
        }

        fn point(&mut self, lo: f32, hi: f32) -> [f32; 3] {
            [self.range(lo, hi), self.range(lo, hi), self.range(lo, hi)]
        }
    }

    fn random_triangles(rng: &mut Lcg, n: usize) -> Vec<[[f32; 3]; 3]> {
        (0..n)
            .map(|_| {
                let c = rng.point(-10.0, 10.0);
                let mut corner = || {
                    let d = rng.point(-0.8, 0.8);
                    [c[0] + d[0], c[1] + d[1], c[2] + d[2]]
                };
                [corner(), corner(), corner()]
            })
            .collect()
    }

    #[test]
    fn every_triangle_lands_in_exactly_one_leaf() {
        let mut rng = Lcg(7);
        let tris = random_triangles(&mut rng, 333);
        let bvh = ShadowBvh::build(&tris).expect("build");
        assert_eq!(bvh.triangle_count(), 333);
        assert_eq!(bvh.triangles().len(), 333);
        let mut seen = vec![0u32; 333];
        for node in bvh.nodes() {
            if let Some((first, n)) = node.leaf() {
                assert!((1..=MAX_LEAF_TRIANGLES as u32).contains(&n));
                for t in first..first + n {
                    seen[t as usize] += 1;
                }
            }
        }
        assert!(seen.iter().all(|&s| s == 1), "{seen:?}");
        // Every input triangle is present (as a multiset of corner sets).
        let key = |t: &[[f32; 3]; 3]| t.iter().flatten().map(|c| c.to_bits()).collect::<Vec<u32>>();
        let mut want: Vec<_> = tris.iter().map(key).collect();
        let mut got: Vec<_> = bvh.triangles().iter().map(|t| key(&t.corners())).collect();
        want.sort_unstable();
        got.sort_unstable();
        assert_eq!(want, got);
    }

    #[test]
    fn escape_indices_move_forward_and_nest() {
        let mut rng = Lcg(11);
        let bvh = ShadowBvh::build(&random_triangles(&mut rng, 200)).expect("build");
        let nodes = bvh.nodes();
        let n = nodes.len() as u32;
        assert_eq!(nodes[0].escape, n, "the root's subtree is the whole tree");
        for (i, node) in nodes.iter().enumerate() {
            let i = i as u32;
            assert!(
                node.escape > i && node.escape <= n,
                "node {i} escape {}",
                node.escape
            );
            if node.leaf().is_some() {
                assert_eq!(node.escape, i + 1, "a leaf has no subtree");
            } else {
                // Interior: two children, the first right after it, the second at the first's
                // escape, and the second's subtree ends where this one does.
                let left = &nodes[(i + 1) as usize];
                let right = left.escape;
                assert!(right < node.escape, "node {i} has one child");
                assert_eq!(nodes[right as usize].escape, node.escape);
                // Children's boxes sit inside the parent's.
                for child in [left, &nodes[right as usize]] {
                    let inside = (0..3).all(|k| child.min[k] >= node.min[k] && child.max[k] <= node.max[k]);
                    assert!(inside, "node {i}: a child box leaves its parent");
                }
            }
        }
    }

    #[test]
    fn the_tree_answers_like_brute_force() {
        let mut rng = Lcg(42);
        let tris = random_triangles(&mut rng, 400);
        let bvh = ShadowBvh::build(&tris).expect("build");
        let mut hits = 0;
        for _ in 0..3000 {
            let origin = rng.point(-12.0, 12.0);
            let dir = rng.point(-1.0, 1.0);
            let t_max = rng.range(0.5, 30.0);
            let want = occluded_brute_force(&tris, origin, dir, 1e-4, t_max);
            assert_eq!(
                bvh.occluded(origin, dir, 1e-4, t_max),
                want,
                "{origin:?} {dir:?} {t_max}"
            );
            hits += usize::from(want);
        }
        // The sample is meaningful: both answers occur often.
        assert!(hits > 50 && hits < 2950, "{hits} hits of 3000");
    }

    #[test]
    fn axis_aligned_rays_take_the_nudged_reciprocal() {
        // A unit quad at y = 2; rays straight up have zero x and z direction components.
        let quad = [
            [[-1.0, 2.0, -1.0], [1.0, 2.0, -1.0], [1.0, 2.0, 1.0]],
            [[-1.0, 2.0, -1.0], [1.0, 2.0, 1.0], [-1.0, 2.0, 1.0]],
        ];
        let bvh = ShadowBvh::build(&quad).expect("build");
        assert!(bvh.occluded([0.3, 0.0, -0.2], [0.0, 1.0, 0.0], 1e-3, 100.0));
        assert!(!bvh.occluded([1.5, 0.0, 0.0], [0.0, 1.0, 0.0], 1e-3, 100.0));
        // Beyond t_max, and pointing away.
        assert!(!bvh.occluded([0.0, 0.0, 0.0], [0.0, 1.0, 0.0], 1e-3, 1.5));
        assert!(!bvh.occluded([0.0, 0.0, 0.0], [0.0, -1.0, 0.0], 1e-3, 100.0));
        assert!(inverse_direction([0.0, 1.0, -0.0]).iter().all(|c| c.is_finite()));
    }

    #[test]
    fn empty_and_bad_inputs() {
        let empty = ShadowBvh::build(&[]).expect("empty");
        assert_eq!(empty.triangle_count(), 0);
        assert_eq!(empty.nodes().len(), 1);
        assert_eq!(empty.triangles().len(), 1);
        assert!(!empty.occluded([0.0; 3], [0.0, 1.0, 0.0], 0.0, 1e9));
        let nan = [[[0.0, f32::NAN, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]]];
        assert_eq!(ShadowBvh::build(&nan), Err(BvhError::NonFinite(0)));
        assert!(
            !BvhError::TooManyTriangles(MAX_BVH_TRIANGLES + 1)
                .to_string()
                .is_empty()
        );
    }

    #[test]
    fn the_build_is_deterministic_and_matches_the_gpu_layout() {
        let mut rng = Lcg(3);
        let tris = random_triangles(&mut rng, 97);
        assert_eq!(ShadowBvh::build(&tris), ShadowBvh::build(&tris));
        assert_eq!(std::mem::size_of::<BvhNode>(), 32);
        assert_eq!(std::mem::size_of::<BvhTriangle>(), 48);
    }

    /// A 2×2 card at y = 2. Two triangles, the diagonal included by both.
    fn card() -> [[[f32; 3]; 3]; 2] {
        [
            [[-1.0, 2.0, -1.0], [1.0, 2.0, -1.0], [1.0, 2.0, 1.0]],
            [[-1.0, 2.0, -1.0], [1.0, 2.0, 1.0], [-1.0, 2.0, 1.0]],
        ]
    }

    /// Pixel centers of a 64² view of x, z in [-2, 2], the ground at y = 0. The card covers
    /// the central 32² texels (1024). Matches the sun-trace test's inverse view-projection.
    fn ground_texels() -> Vec<[f32; 3]> {
        let n = 64u32;
        let mut out = Vec::with_capacity((n * n) as usize);
        for j in 0..n {
            for i in 0..n {
                let ndc_x = (i as f32 + 0.5) / n as f32 * 2.0 - 1.0;
                let ndc_y = 1.0 - (j as f32 + 0.5) / n as f32 * 2.0;
                out.push([ndc_x * 2.0, 0.0, ndc_y * 2.0]);
            }
        }
        out
    }

    #[test]
    fn half_coverage_packs_like_the_ray_query_proxy() {
        assert_eq!(
            triangle_coverage_byte(0.5),
            super::super::casters::coverage_custom_data(0.5)
        );
        assert_eq!(triangle_coverage_byte(0.9999), 255);
        assert_eq!(triangle_coverage_byte(0.0), 255);
        assert_eq!(triangle_coverage_byte(1.0), 255);
        assert_eq!(triangle_coverage_byte(-0.25), 255);
        assert_eq!(triangle_coverage_byte(f32::NAN), 255);
        assert!(any_hit([0.0; 3], 0, 0, 255));
        assert!(!any_hit([0.2, 0.0, -0.4], 1, 0, 0));
    }

    /// Absolute tolerance on the lit fraction of a coverage-0.5 card over 1024 texels.
    /// A fair coin has standard deviation 16/1024 ≈ 0.0156; 0.08 is about five of those, so a
    /// solid card or a missing card cannot pass. The holes are the hash: no mask texture is bound.
    const LIT_FRACTION_EPSILON: f32 = 0.08;

    #[test]
    fn a_masked_card_opens_about_half_its_shadow_and_a_miss_stays_open() {
        let tris = card();
        let opaque = ShadowBvh::build(&tris).expect("opaque");
        let masked = ShadowBvh::build_with_coverage(&tris, &[0.5, 0.5]).expect("masked");
        assert!(masked.triangles().iter().all(|t| t.a[3] == 0.5));
        let points = ground_texels();
        let dir = [0.0, 1.0, 0.0];
        let mut opaque_hits = 0usize;
        let mut masked_hits = 0usize;
        let mut outside = 0usize;
        for p in &points {
            let inside = p[0].abs() < 1.0 && p[2].abs() < 1.0;
            let solid = opaque.occluded(*p, dir, 0.0, 100.0);
            let dappled = masked.occluded(*p, dir, 0.0, 100.0);
            if inside {
                assert!(solid, "opaque footprint {p:?}");
                opaque_hits += 1;
                masked_hits += usize::from(dappled);
            } else {
                assert!(!solid && !dappled, "a miss stays a miss {p:?}");
                outside += 1;
            }
        }
        assert_eq!(opaque_hits, 1024, "pinned covered texel count");
        assert_eq!(outside, 64 * 64 - 1024);
        let open = 1.0 - masked_hits as f32 / opaque_hits as f32;
        assert!(
            (open - 0.5).abs() < LIT_FRACTION_EPSILON,
            "lit fraction {open} over {opaque_hits} texels, {masked_hits} shadowed"
        );
        // A rejected candidate must not hide an opaque triangle on the same ray.
        let mut stacked = tris.to_vec();
        stacked.extend(tris);
        let both = ShadowBvh::build_with_coverage(&stacked, &[0.5, 0.5, 0.0, 0.0]).expect("stacked");
        let saved = points
            .iter()
            .filter(|p| p[0].abs() < 1.0 && p[2].abs() < 1.0)
            .filter(|p| both.occluded(**p, dir, 0.0, 100.0))
            .count();
        assert_eq!(
            saved, 1024,
            "an opaque triangle behind a rejected any-hit still occludes"
        );
    }
}
