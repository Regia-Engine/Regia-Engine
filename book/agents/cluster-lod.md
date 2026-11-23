# Vertex-cluster far LOD

A large model is often split into many pieces, one per UV tile or material, and arrives
with no light level of detail. Hiding pieces at distance leaves floating plates, and
simplifying each piece on its own opens cracks along the seams, because each piece snaps
its border to its own grid. `render.cluster_lod` keeps every piece and clusters the
vertices of all of them on **one shared grid**: the full silhouette at a few percent of
the triangles, with closed seams (ADR-8464).

The method is uniform-grid vertex clustering. Every vertex snaps to the mean of the
vertices in its grid cell, taken over every piece, and triangles that collapse or repeat
after the snap are dropped. Vertices are also sorted by the main direction of their
normal, so the two sides of a thin plate keep their own faces. The grid gets coarser
until the triangle budget fits.

## `render.cluster_lod`

```json
{ "method": "render.cluster_lod",
  "params": { "entities": [4294967301, 4294967302], "target_triangles": 2000, "dry_run": true } }
```

| Param | Type | Meaning |
|---|---|---|
| `entities` | array, required | Ids of the model's pieces (entities with a `MeshRenderer`), 1 to 256, each once. |
| `target_triangles` | integer | Triangle budget for all pieces together, 1 to 10000000 (default 2000). |
| `screen_size` | number | Projected size below which the new level draws, in (0, 1] (default 0.05). It must be below each piece's current coarsest switch size. |
| `dry_run` | boolean | Report the level without changing any entity (default false). |

Pieces under different transforms are clustered in the first piece's space, so a seam that
two pieces share in the world stays shared. Without `dry_run` the result is appended to each
piece's `MeshLodChain` as its coarsest level: the piece's mesh gains the far vertices, its
finest level keeps its triangles, and the level before the new one switches at
`screen_size`. A piece without a chain gets one (its own triangles, then the far level).
Call again with a smaller `screen_size` to add a coarser level.

Result:

```json
render.cluster_lod { "entities": [4294967301, 4294967302], "target_triangles": 2000, "dry_run": true }
→ { "dry_run": true, "source_triangles": 59904, "triangles": 1890, "cell_size": 0.31,
  "max_error": 0.27, "passes": 3, "screen_size": 0.05,
  "pieces": [{ "entity": 4294967301, "source_triangles": 2304, "triangles": 71,
               "vertices_added": 52, "levels": 2 }] }
```

- `max_error` is the largest distance any vertex moved. Every point of a kept triangle moved
  at most that far, so the silhouette of the far level lies within `max_error` of the
  source's (features thinner than a cell may vanish). Pick the switch distance so that
  `max_error` projects under a pixel or two.
- `cell_size` is the edge of the grid cell; `passes` is how many grid sizes were tried.
- A model already at or under the budget (or under 64 triangles) comes back unchanged with
  `max_error` 0 and `passes` 0.

Far levels are for rigid meshes: skinned and morphing pieces are refused.

### Errors

| Code | When |
|---|---|
| `F0002` | `entities` missing, empty, over 256, repeated or not ids; `target_triangles`, `screen_size` or `dry_run` out of range or of the wrong type. |
| `F0003` | An entity does not exist. |
| `cluster_lod_no_mesh` | An entity has no `MeshRenderer`, or its mesh keeps no CPU vertices (a counted or empty mesh). |
| `cluster_lod_deforming` | A piece is skinned or has morph targets. |
| `cluster_lod_bad_mesh` | A piece's mesh has an index out of range or a non-finite position. |
| `cluster_lod_screen_size` | `screen_size` is not below a piece's current coarsest switch size. |

## From Rust

<!-- ignore-reason: fragment of a game's code: `pieces` are the model's CpuMesh pieces the game loaded, inside a function that returns a Result -->
```rust,ignore
use regia_render::{VertexClusterSettings, cluster_cpu_meshes};

// One call for all pieces of the model: one shared grid, closed seams.
let (far_meshes, lod) = cluster_cpu_meshes(&pieces, &VertexClusterSettings::with_target(2_000))?;
println!("{} -> {} triangles, error {:.3} m", lod.source_triangles, lod.triangles(), lod.max_error);
```

`cluster_pieces` takes raw positions, normals and indices and returns, per piece, the
representative positions and normals, the source vertex each output vertex copies its
other attributes from, and the remap from source vertices. `cluster_lab_mesh` clusters one
`LabVertex` mesh and never fails (bad input comes back unchanged). `VertexClusterSettings`
holds the schedule: `min_triangles` (64), `max_passes` (16), `shrink` (0.75), `min_cells`
(4) and `split_by_facing` (true).

## Verify

1. `render.cluster_lod { entities, dry_run: true }` reports `triangles` at or under the budget
   and one row per piece with `triangles` above 0 (no piece dropped).
2. Apply it: each row's `levels` is one more than before, and a second call with the same
   `screen_size` is refused with `cluster_lod_screen_size`. `render.mesh_lod { radius, thresholds }`
   gives the distance at which a switch size draws.
3. Move the camera away until the far level draws and take a `screenshot`: the outline
   matches the near view within `max_error` and there are no cracks between pieces.
