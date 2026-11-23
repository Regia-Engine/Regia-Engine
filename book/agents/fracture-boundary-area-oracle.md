# Fracture boundary area regression recipe

This CPU regression checks the visible meshes of existing level 2 pre-fractured assets. It
does not start runtime fracture. Run:

```sh
cargo test -p regia_asset_pipeline --test fracture_boundary_area
```

Use the build host and resource wrapper assigned to your lane. No image reference or GPU is
needed. The existing [pre-fractured destruction guide](fracture.md) documents
`editor.fracture_preview` and the Fracture panel. Their existing host-extension rows in
[COVERAGE](../COVERAGE.md#5-host-extension-methods--agent-guide), and the existing
`.fracture.ron` v2 row, remain the coverage for those surfaces. This recipe adds no agent method,
CLI command, Cargo feature, or content format.

The original fixtures are a closed box with half extents (1, 1, 1), and a closed stepped prism
with XY outline (0,0), (3,0), (3,1), (1,1), (1,2), (0,2), extruded from z=-1 to z=1.
The box's six source planes each have area 4. The prism's two end planes each have area 4;
its six wall planes have areas 6, 2, 4, 2, 2, 4 in outline order. Areas multiply by scale squared.

The fixed matrix uses scales 0.125 and 32, seeds 3, 11, and 29, and seven sites. Each shape
uses three triangulations: one diagonal/fan, the other diagonal/fan, and center subdivisions.
Two-site box cases also cut along x+y=0 (containing source edges) and x+y+z=1 (through source
vertices). Each explicit cut runs at both scales and with all three triangulations.
`min_chunk_volume_fraction=0` disables deliberate chunk dropping; every case requires the
requested number of chunks.

Six additional stepped-prism cases use explicit sites (0.5,0.5,0) and (1.5,0.5,0), multiplied
by scale, at both scales and with all three triangulations. Their bisector x=1 contains both
the exterior step wall (y=1..2) and the interior cut (y=0..1). Finite face bounds must keep
those patches separate. Each variant is a separate test so one failure cannot hide another.
The existing private `fracture_clip::tests::a_plane_on_a_face_keeps_the_cube` proves that
the keeping half-space preserves a box on its outer face. These cases extend that contract
to two positive-volume cells, with exterior and interior patches on the same plane.

The oracle reads only render positions, indices, and normals. It promotes stored positions to
`f64`, computes triangle area from the cross product of two edges, and assigns each exterior
triangle to its source face by geometric plane membership and finite face bounds. It compares
each source plane's sum with its analytic area and outward area vector separately. It never
uses production clipping helpers, welded-mesh volume, joint areas, or `interior_start` to
decide what is exterior. Shared cut faces contribute no exterior area.

Remaining triangles form per-chunk geometric planes, established by the largest triangle
before accumulating small triangles. A cut's mate must belong to another chunk, lie on the
same plane in both directions, and have the same area centroid. Each cut has exactly one mate.
Areas must match; geometric area vectors and area-weighted render normals must oppose.
Different triangulations on opposite sides need not have matching indices or triangles.
This is an area and first-moment oracle, not a proof of pointwise polygon coverage.

Plane-distance tolerance is `2e-6 * max(scale, abs(plane_offset))`; area tolerance is
`2e-5 * expected_area + 1e-10 * scale^2`. Mate centroids allow eight plane-distance tolerances.
Exactly zero-area triangles contribute nothing; positive areas are accumulated. These bounds
account for stored `f32` positions while keeping the reference arithmetic in `f64`.

Existing tests already prove volume and watertightness (`fracture.rs`), source UV interpolation
and shared cut patterns (`fracture_render.rs`), and vertex-cut closedness (the private
`fracture_clip` tests). This recipe extends them with per-source-plane render-area conservation
and complete geometric cut pairing across retriangulation, seeds, and scale.

For an omission negative control, temporarily remove one large geometrically classified
boundary triangle from an output render index list before measurement. Run the test and require
its per-plane area assertion to fail. Restore the source, repeat by duplicating the triangle,
then restore again and require a passing test. Keep both failure logs; commit neither mutation.
If a legitimate fixture reveals a production counterexample, report the exact shape, sites or
seed, scale, triangulation, plane, expected and observed areas. The fracture/clip/render owner
must fix production geometry; cook/cache recovery belongs to the separate cook audit lane.

The coplanar step-wall cases currently reveal a production counterexample: the wall at x=scale,
y=scale..2*scale has area `2*scale^2`, but output render triangles sum to `6*scale^2` for all
six variants. At scale 0.125 this is 0.09375 instead of 0.03125; at scale 32 it is 6144 instead
of 2048. The clip/cap owner must resolve how a wholly coplanar source wall is retained on
the two sides of a cut. These assertions remain strict and active; the recipe is not fully green.
