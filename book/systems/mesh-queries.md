# Mesh queries

`regia_core::mesh_query` answers spatial questions about triangle meshes and point sets on the
CPU: where does a ray hit, what is the closest surface point, which triangles are in this box,
do two meshes touch, is a point inside, how far is it from the surface. It has no engine state:
you build a structure from a mesh once and query it as often as you like, from a game system, a
tool or the asset pipeline (ADR-1461).

| Structure | Built from | Answers |
|---|---|---|
| `TriMesh` | positions and index triples (or a flat index buffer) | the validated mesh every other structure uses |
| `TriBvh` | a `TriMesh` | `ray_cast`, `closest_point`, `triangles_in_aabb`, `triangles_in_sphere`, `intersects` and `intersecting_pairs` with another tree (optionally under an `Affine3A`) |
| `WindingNumberTree` | a `TriMesh` or a `TriBvh` | `winding_number`, `is_inside` |
| `SignedDistanceGrid` | a `WindingNumberTree` (or a `TriMesh`) and `SdfParams` | `sample`, `gradient`, `sample_with_gradient`, grid values |
| `PointHash` | a slice of points and a cell size | `within_radius`, `nearest` |

`winding_number_exact` and the `triangle` module (closest point, ray hit, triangle/box and
triangle/triangle overlap, solid angle on one triangle) are there for one-off questions and as
references for tests.

## Example

```rust
use regia_core::glam::Vec3;
use regia_core::math::Ray;
use regia_core::mesh_query::{
    MeshQueryError, SdfParams, SignedDistanceGrid, TriMesh, WindingNumberTree,
};

/// Builds the queries for one mesh and asks it a few questions.
fn mesh_questions(positions: Vec<Vec3>, indices: &[u32]) -> Result<(), MeshQueryError> {
    let mesh = TriMesh::from_indices(positions, indices)?;
    let tree = WindingNumberTree::build(mesh)?;
    let bvh = tree.bvh();
    let down = Ray::new(Vec3::new(0.0, 5.0, 0.0), Vec3::NEG_Y);
    if let Some(hit) = bvh.ray_cast(&down, 100.0) {
        println!("hit triangle {} at {} m", hit.triangle, hit.distance);
    }
    let nearest = bvh.closest_point(Vec3::ZERO, f32::INFINITY);
    let inside = tree.is_inside(Vec3::ZERO);
    let sdf = SignedDistanceGrid::build(&tree, &SdfParams::new(0.05))?;
    println!("{nearest:?} inside={inside} distance={:?}", sdf.sample(Vec3::ZERO));
    Ok(())
}
```

## Meshes and errors

A `TriMesh` checks every index against the vertex count and rejects NaN or infinite positions.
Triangles are counter-clockwise when seen from outside; `TriMesh::flip` reverses them all.
Degenerate (zero-area) triangles are kept: closest-point and overlap tests treat them as segments
or points, rays never hit them, and they add nothing to a winding number.

Every failure is a `MeshQueryError`:

| Variant | When |
|---|---|
| `EmptyMesh` | a tree, winding tree or distance grid is built from a mesh without triangles |
| `IndexCount` | a flat index list's length is not a multiple of three |
| `IndexOutOfRange` | a triangle names a vertex the mesh does not have |
| `NonFinite` | a position or point has a NaN or infinite coordinate |
| `TooLarge` | more elements than a `u32` index can name |
| `InvalidCellSize` | a grid cell size is zero, negative or not finite |
| `GridTooLarge` | a distance grid would exceed `SdfParams::max_points` (2²⁵ by default) |
| `InvalidBeta` | the winding-number `beta` is not finite and positive |

Queries do not fail: a ray with a zero direction, a NaN point or a negative radius simply finds
nothing. The one exception is `winding_number`, which returns NaN for a non-finite point
(`is_inside` is then `false`); filter such points before averaging winding numbers.

## The tree

`TriBvh` is a binary tree of axis-aligned boxes, split at the midpoint of the longest axis of the
triangle centroids, with at most four triangles per leaf. Ray and closest-point searches visit the
nearer child first and skip boxes farther than the best answer so far, so a query near the surface
touches a handful of leaves. The one slow case is a point deep inside a closed, round mesh: from
the center of a sphere every triangle is about equally far, and the search must look at most of
them. All per-triangle math runs in `f64`; results are `f32`.

`intersects` and `intersecting_pairs` walk two trees together. Pass `Some(&other_to_self)` when
the second mesh is placed by a transform; touching (a shared vertex or edge) counts as an
intersection.

## Inside and outside: winding numbers

The generalized winding number is 1 inside a closed mesh and 0 outside. Unlike a ray-parity test
it degrades gracefully: a mesh with a hole gives values a little under 1 inside, a few flipped
triangles disturb only the points close to them, and a triangle soup still gets a sensible answer.
`is_inside` means "winding number above 0.5".

`WindingNumberTree` stores a small far-field expansion on every tree node, so far-away parts of the
mesh are summed in one step. `beta` trades accuracy for speed: the expansion is used once a node is
farther than `beta` times its own size. The default is 2. Measured largest errors against the exact
sum, over points in and around the mesh:

| Mesh | beta 2 | beta 4 | beta 8 |
|---|---|---|---|
| sphere, 2,208 triangles | 0.041 | 0.0089 | 0.00046 |
| torus, 2,304 triangles | 0.020 | 0.0018 | 0.00028 |
| random soup, 1,500 triangles | 0.0024 | 0.00018 | 0.000015 |

The error falls about as `1 / beta³`. For inside/outside tests the default is plenty; raise `beta`
(`set_beta`, no rebuild) when you use the value itself.

## Signed distance grids

`SignedDistanceGrid::build` samples the signed distance to the mesh on a regular grid: negative
inside, positive outside, grid points at `origin() + (i, j, k) * cell_size()`. The grid covers the
mesh's box plus `2 * exact_band` cells and `margin` on every side.

1. Every grid point within `exact_band` cells of a triangle's box gets the exact distance.
2. With `sweep` on (the default), fast sweeping carries the closest triangle outward so every point
   gets the distance to a nearby triangle. Outside the band this is an upper bound that is usually
   exact; with `sweep` off, points outside the band keep a large placeholder distance.
3. The winding number decides each point's sign, so open meshes work. Blocks of 4³ points that lie
   outside every triangle's band (its box grown by `exact_band` cells) are signed with one evaluation;
   blocks that touch a band are signed per point.

`sample` interpolates trilinearly between grid points and `gradient` returns that interpolant's
derivative (about unit length near the surface, pointing outward). Both return `None` outside
`bounds()`. In the tests, every grid point and every sample of a tessellated sphere is within one
cell of the analytic distance.

## Point hash

`PointHash` buckets points in cubic cells. `within_radius(center, r)` returns the indices of all
points within `r` (any radius; a sphere far larger than the cells visits occupied cells instead),
in ascending order. `nearest(center, max_distance)` searches growing shells of cells and falls back
to a scan when the points are too spread out for shells to pay off. Pick a cell size near your
typical query radius.

## Cost

One thread, release build, a 100,000-triangle sphere:

| Operation | Time |
|---|---|
| `TriBvh::build` | 14 ms |
| `closest_point`, points near the surface | 2.3 µs each |
| `closest_point`, points 1 to 2 radii outside | 23 µs each |
| `closest_point`, points deep inside | 380 µs each |
| `ray_cast` | 0.7 µs each |
| `WindingNumberTree::from_bvh` | 9 ms |
| `winding_number` | 2.4 µs each |
| `winding_number_exact` | 2.3 ms each |
| `SignedDistanceGrid`, 45³ points | 0.7 s |
| `SignedDistanceGrid`, 85³ points | 2.9 s |

Distance grids are meant to be built offline or at load, not per frame. The benchmark is the
ignored test `bench_100k_triangles` in `crates/regia_core/tests/mesh_query.rs`:

```sh
cargo test --release -p regia_core --test mesh_query -- --ignored --nocapture bench_100k
```

## Agents

There is no agent-protocol method for this module: it holds no engine state, so there is nothing
for an agent to inspect or change. Features built on it (a distance-field bake, a query method on
the scene) carry their own methods and pages.

## Sources

The tree, winding-number, distance-grid and point-hash code is ported from geometry3Sharp by Ryan
Schmidt (Boost Software License 1.0), with the changes listed in `docs/provenance.md`. The methods
come from A. Jacobson, L. Kavan and O. Sorkine-Hornung, "Robust Inside-Outside Segmentation using
Generalized Winding Numbers" (SIGGRAPH 2013); G. Barill, N. Dickson, R. Schmidt, D. Levin and A.
Jacobson, "Fast Winding Numbers for Soups and Clouds" (SIGGRAPH 2018); H. Zhao, "A fast sweeping
method for Eikonal equations" (2005); and, for the triangle tests, C. Ericson, *Real-Time Collision
Detection*, Möller and Trumbore (1997), Akenine-Möller (2001) and van Oosterom and Strackee (1983).
