# Convex decomposition

A dynamic body cannot use a `TriMesh` collider. To give a concave prop (a chair, an L-shaped crate, a
vehicle hull) a collider that moves, split its mesh into a few convex hulls and use them as one
`ColliderShape::Compound`. The code is `regia_physics::convex_decomposition`; the agent method
`physics.convex_decompose` runs the same call on an entity's trimesh collider and reports the hulls, so you
can pick the parameters before the game code bakes them in.

```rust,wrap
# let positions: Vec<regia_core::math::Vec3> = Vec::new();
# let triangles: Vec<[u32; 3]> = Vec::new();
use regia_core::mesh_query::TriMesh;
use regia_physics::convex_decomposition::{DecompositionParams, compound_shape, decompose};

let mesh = TriMesh::new(positions, triangles)?;
let hulls = decompose(&mesh, &DecompositionParams { max_hulls: 8, ..Default::default() })?;
let shape = compound_shape(&hulls); // None for no hulls, one ConvexHull, or a Compound
```

The work is single-threaded and allocates (a 64-cell grid of a mesh of a few thousand triangles takes
tens of milliseconds in a release build); run it at import or load time, not per frame.

## How it works

1. **Voxelize.** A grid with `resolution` cells along the mesh's longest axis. Cells a triangle touches are
   surface cells; a flood fill from the grid's border marks the outside; the rest is interior. Triangle
   winding does not matter, and holes smaller than a cell do not let the flood fill in.
2. **Cut.** The part with the largest concavity (its hull's volume minus its solid volume) is cut by the
   axis-aligned plane, on a cell boundary, that minimizes the summed concavity of the two sides, plus two small
   biases: toward an even split of the part's cells and toward cuts across its longer extents, so a ring is cut
   into arcs instead of having a cap trimmed or being sliced flat (16 positions per axis, then every position
   around the best). Cutting stops when every part's concavity is at most
   `max_concavity` of the mesh's volume, a part has had `max_depth` cuts, or there are four times
   `max_hulls` parts (at most 64).
3. **Hull.** Each part's hull is built from the mesh's triangles clipped to the part's box, plus the box
   corners inside the solid, so hulls follow the mesh and not the voxel staircase.
4. **Merge.** The pair of hulls whose merged hull adds the least volume is merged until at most `max_hulls`
   remain, and further while a merge adds at most `max_concavity` of the volume.
5. **Cap.** Each hull keeps at most `max_hull_vertices` vertices, farthest points first, so a capped hull
   lies inside the uncapped one.

The same mesh and parameters always give the same hulls, largest first. A closed box gives one hull; an
L-shaped prism gives two whose volumes add up to the L's (within 0.3 %); two separate spheres give one hull
each; a torus gives eight arcs whose hulls hold about 1.08 times the tube's volume and never reach across the hole
(its single hull is 1.5 times the volume). An open
mesh (missing faces) is treated as a thin shell. With `max_hulls: 1` the result is the hull of every vertex.

## Parameters

| Field | Range | Default | Effect |
|---|---|---|---|
| `max_hulls` | 1 to 64 | 16 | Most hulls returned. |
| `resolution` | 8 to 256 | 64 | Cells along the longest axis. Higher finds smaller notches; memory and time grow with the cube. |
| `max_concavity` | 0 to 1 | 0.01 | Concavity a part may keep (and a merge may add), as a fraction of the mesh volume. Lower gives more, tighter hulls. |
| `max_depth` | 0 to 16 | 10 | Most cuts to any one part. 0 means no cuts: one hull. |
| `max_hull_vertices` | 0, or 4 to 4096 | 64 | Most vertices per hull; 0 for no cap. |

## Method

`physics.convex_decompose { entity, max_hulls?, resolution?, max_concavity?, max_depth?, max_hull_vertices?,
points? }` reads the entity's `Collider`, whose shape must be `ColliderShape::TriMesh`, and returns:

- `hull_count`, `total_volume` (the hulls' summed volume; compare it with the mesh's volume to see how
  tightly the hulls fit),
- `hulls`: per hull `vertices` and `triangles` (counts), `volume`, `centroid` `[x, y, z]`, and with
  `points: true` the vertex positions `points`, largest hull first, in the collider's local space,
- `params`: the values used, defaults filled in.

The call is read-only: it does not change the collider. It is a heavy call (rate class `heavy`).

```json
{ "method": "physics.convex_decompose", "params": { "entity": 4294967296, "max_hulls": 8 } }
{ "result": { "entity": 4294967296, "hull_count": 2, "total_volume": 3.01,
  "hulls": [ { "vertices": 8, "triangles": 12, "volume": 2.0, "centroid": [0.5, 1.0, 0.5] },
             { "vertices": 8, "triangles": 12, "volume": 1.01, "centroid": [1.5, 0.5, 0.5] } ],
  "params": { "max_hulls": 8, "resolution": 64, "max_concavity": 0.01, "max_depth": 10, "max_hull_vertices": 64 } } }
```

## Errors

A missing entity, or an entity without a `Collider`, answers `F0003`. Everything else answers `F0002` with a
lowercase code in the message, in brackets:

| Code | Meaning |
|---|---|
| `not_trimesh` | The collider's shape is not a `TriMesh`. |
| `bad_mesh` | The trimesh names a vertex it does not have, or has a non-finite position. |
| `empty_mesh` | The trimesh has no triangles. |
| `zero_area` | Every triangle has zero area. |
| `flat_mesh` | The vertices are coplanar: the mesh encloses no volume. |
| `bad_max_hulls` | `max_hulls` is not an integer from 1 to 64. |
| `bad_resolution` | `resolution` is not an integer from 8 to 256. |
| `bad_max_concavity` | `max_concavity` is not a number from 0 to 1. |
| `bad_max_depth` | `max_depth` is not an integer from 0 to 16. |
| `bad_max_hull_vertices` | `max_hull_vertices` is not 0 or an integer from 4 to 4096. |
| `bad_points` | `points` is not a boolean. |

The same codes come from `DecompositionError::code()` in Rust (`empty_mesh`, `zero_area`, `flat_mesh` and the
`bad_*` parameter codes).

## Checking a decomposition

- `total_volume` close to the mesh's volume means the hulls fit; much larger means a notch was bridged.
  Lower `max_concavity` or raise `max_hulls`.
- A notch narrower than two cells cannot be found: raise `resolution`.
- After building the compound collider, drop the body and look at it with `physics.raycast` or a screenshot.

## At import

The glTF importer cooks the hulls for you. Set, in the asset's `.meta` importer settings:

```ron
(collision: ConvexDecomposition, collision_hulls: (max_hulls: 8, resolution: 64))
```

Each mesh primitive then gets a sibling sub-asset `collision/<node>/primitive_<i>` (type
`regia_physics::HullSet`, format in `docs/formats/hull-set.md`) holding its convex hulls, built with the
same algorithm and parameters as `physics.convex_decompose` (`collision_hulls` takes the same fields and
defaults). `regia_physics::hull_set::HullSet::to_collider_shape` turns one into a collider shape. The cook
is cached per mesh bytes and settings; a primitive with no volume logs a warning and gets no hull set; a bad
setting fails the import naming the field. `collision: None` (the default) cooks nothing.
