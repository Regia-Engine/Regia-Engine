# Volumetric terrain: dual contouring chunks

<!-- index: Dual contouring -->
<!-- index: Signed-distance field -->
<!-- index: Volume mesh -->
<!-- index: Voxel terrain -->
<!-- index: Mesh layer -->

`regia_terrain::volume_mesh` meshes signed-distance fields — caves, overhangs, tunnels and
other volumes a heightfield cannot express — by dual contouring (Ju, Losasso, Schaefer &
Warren, SIGGRAPH 2002). It complements the CDLOD heightfield path: use heightfields for
ordinary ground, and volume chunks where the surface folds over itself.
The heightfield chapter is [Terrain, tunnels and scatter](terrain-vfx.md).

## What is built, and what spec 122 is not

This page is the mesher and the volume source on a terrain mesh layer (ADR-10071). You supply
a signed-distance field. The cook meshes it and clips it to the ground. That is in the tree.

`docs/specs/122-voxel-core.md` is a different feature: an integer voxel core,
`regia_voxel_core`, with a fixed-point field, an operation log that replays bit for bit,
integer queries, a `.voxfield` file, and a plug-in on `regia_world_geom`. That crate is not in
this tree. There is no runtime voxel edit, no operation-log replay, and no `.voxfield` load.
The catalog status for voxel terrain stays partial until that crate exists and this page
describes it. Do not treat the sections below as that core.

What you can do today:

- Mesh a field you implement as `SdfField` (a cave, a carve, a heightfield adapted by
  `TerrainMapField`).
- Weld chunks and step LOD by 2× without skirts.
- Cook a volume as one terrain mesh layer. The layer names an `sdf` string. The host passes
  the field to `cook_mesh_layers_with_volumes`. There is no volume file format.

What you cannot do in this version:

- Dig or fill a stored voxel field at run time, on the simulation, with a replayable log.
- Read a voxel volume from the world-geometry queries (`support`, `segment`, `push_disc`). Those
  calls exist for the heightfield and for mesh layers. They do not read a voxel core, because
  that core is not in the tree.
- Load a `.voxfield`. The name `.fvox` is the cooked far proxy (`docs/formats/voxel-far.md`). It is not the voxel-core file.
- Author a volume by dropping a file in the project. A `Volume` layer with no host-supplied field is refused with `cook_not_built`.

A crater or a berm on the heightfield is a terrain edit, not a voxel volume. An overhang that
stands above the ground is not a volume layer either: the cook clips the mesh to the ground.
Author that overhang as a `Gltf` or `Collision` mesh layer.

## Defining a field

Implement `SdfField` (`sample` is required; `gradient` defaults to a central finite
difference, override it with an analytic gradient for crisp sharp features or fields far
from unit scale). The sign convention is the usual SDF one: negative inside the solid,
positive outside. Closures `Fn([f32; 3]) -> f32` implement the trait automatically, and
`TerrainMapField` adapts a heightfield `TerrainMap` (solid below the surface), so a cave
field combined with the heightfield by `min`/`max` in your own `SdfField` meshes through
the same path.

```rust
use regia_terrain::volume_mesh::{
    mesh_chunk, MeshParams, NeighborLod, NeighborLods, SdfField,
};

struct Cave {
    center: [f32; 3],
    radius: f32,
}

impl SdfField for Cave {
    fn sample(&self, p: [f32; 3]) -> f32 {
        let d = [p[0] - self.center[0], p[1] - self.center[1], p[2] - self.center[2]];
        // Solid everywhere except a spherical hollow: the negation of a sphere SDF.
        self.radius - (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
    }
}
```

## Meshing a chunk

```rust,wrap
# use regia_terrain::volume_mesh::{mesh_chunk, MeshParams, NeighborLod, NeighborLods, SdfField};
# struct Cave { center: [f32; 3], radius: f32 }
# impl SdfField for Cave {
#     fn sample(&self, p: [f32; 3]) -> f32 {
#         let d = [p[0] - self.center[0], p[1] - self.center[1], p[2] - self.center[2]];
#         self.radius - (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
#     }
# }
let field = Cave { center: [8.0, 4.0, 8.0], radius: 3.0 };
let mut neighbors = NeighborLods::same();
neighbors.pos_x = NeighborLod::Coarser; // the +x chunk runs at half resolution
let mesh = mesh_chunk(
    &field,
    [0.0, 0.0, 0.0], // world position of the minimum grid corner
    0.5,             // cell size (meters)
    [32, 16, 32],    // cells per axis
    neighbors,
    &MeshParams::default(),
)?;
// mesh.positions / mesh.normals / mesh.indices: an indexed, outward-wound triangle mesh.
```

Each cell the surface passes through contributes one vertex. The quadratic error
function of the cell's Hermite data (crossing points and normals on the cell edges) is
minimized with a pseudo-inverse. When that minimizer falls outside the cell,
`mesh_chunk` replaces it with the mass point of the crossings. It does not clamp the
minimizer onto the cell boundary: that would glue mirror-image cells together. With
the default `MeshParams`, two Newton steps then project the vertex onto the surface
and clamp those steps into the cell, so the stored vertex is the projected point, not
the raw minimizer and not the mass point, unless the projection stays put. Sharp
edges and corners of the field survive because the vertex starts from the
least-squares intersection of the tangent planes. `solve_qef` is that pseudo-inverse
alone: it clamps the minimizer into the box you pass and does not project onto a
field.

## Stitching and LOD

Chunks stitch without skirts. A border vertex is a pure function of its cell's world box,
so neighboring chunks compute bit-identical copies, and each surface face is emitted by
exactly one chunk (the one whose half-open domain contains the midpoint of the primal
edge the face is dual to). Weld chunk meshes by exact vertex position to join them; the
union is the manifold mesh a single big chunk would have produced.

A neighbor at half resolution (`NeighborLod::Coarser` here, `NeighborLod::Finer` from the
other side) is stitched the same way on a graded grid: faces crossing the shared plane
join fine vertices to the coarse grid's dual vertices (triangles where one coarse cell
stands in for two fine ones), so no T-junction ever appears. Rules: LOD may step by at
most 2x across a face (`NeighborLods::from_neighbor_levels` converts absolute levels and
rejects bigger jumps), every `cells` entry must be even when a step exists, and
neighbors must share tangential origins so the coarse lattice is a sublattice of the
fine one.

Coarse cells that touch a finer neighbor take their Hermite data from every half-size
segment of their boundary (edge halves and the segments through each face center), so a
feature only the fine side samples still gets a coarse vertex. The rest of a coarse chunk
meshes exactly as it would without the finer neighbor, so it still welds to its
same-LOD neighbors.

Where LOD steps meet along a chunk edge or at a corner (every corner of a finer region),
describe the edge and corner neighbors too, or the diagonal chunk cannot know about the
finer chunk and the seam cracks along that edge. The simplest way is to build every
chunk's `NeighborLods` from the absolute LOD levels of its 3x3x3 neighborhood:

```rust,wrap
# use regia_terrain::volume_mesh::*;
# let own_lod = 0u32;
# let levels = Default::default();
// levels[dx + 1][dy + 1][dz + 1]: absolute LOD of the chunk at that offset, None = no chunk.
let lods = NeighborLods::from_region_levels(own_lod, levels)?;
```

`NeighborLods::set_region([dx, dy, dz], lod)` sets one face, edge or corner entry;
unset edge and corner entries are derived from the touching faces.

Exact welding needs chunk origins and cell sizes whose sums are exact in floating point
(for example powers of two, or integer chunk coordinates times a dyadic chunk size). With
values such as an origin of `0.1 + 8 * 0.3`, neighboring chunks compute border vertices
that differ in the last bits and do not weld by exact position.

Two consequences to budget for: a chunk mesh can contain vertices up to one
neighbor-sized cell outside its nominal box (halo vertices of the border faces it owns),
and a lone chunk is open where the surface exits it.

## Errors and determinism

Invalid input is an error, never a panic: non-positive cell size, zero or over-limit cell
counts, non-finite origin or parameters, misaligned LOD steps, and the first non-finite
field sample or gradient all return `MeshError`. An all-air or all-solid chunk returns an
empty mesh. Output is deterministic: fixed iteration order and exact arithmetic in `f64`
mean repeated calls are bit-identical on the same platform.

## Volumes as terrain mesh layers (ADR-10071)

A cave authored as a signed-distance volume becomes a terrain mesh layer through the same seam,
streaming and query contract as an authored mesh. The `<map>.meshlayers.ron` layer names the
volume:

```text
(id: "cave", source: Volume(sdf: "cave", iso: 0.0, cell_m: 0.5), fills: ["cave_mouth"])
```

There is no volume file format yet, so the host passes the field to the cook:
`regia_asset_pipeline::terrain_mesh_layers::cook_mesh_layers_with_volumes(&request, &volumes)`,
where each `VolumeSource { sdf, field, bounds, fingerprint }` gives the `SdfField` for one `sdf`
name, the world box to mesh, and bytes that identify the field for the cook's input hash. A
`Volume` layer without a source is refused with `cook_not_built`.

The cook meshes the field (minus `iso`) with `mesh_chunk` on one grid of `cell_m` cells, then
keeps only what lies under the LOD-0 ground: a triangle that crosses the ground is split where
its height over the ground changes sign, and the new vertices sit on the ground. A cave that
breaks the surface therefore loses its dome everywhere, and the cut edges form closed loops on
the ground. Each filled hole is one loop (`fills` take the loops in `(z, x)` order), and the
loop's outline is the hole's footprint: the ground opens inside it and the cook's zipper joins
the loop to the ring. A loop the layer does not fill stays under whole ground, which closes the
cave. The clipped mesh is the layer's query mesh (`surface_at` answers its floor inside the
mouth), and its chunks and LODs are cut and locked like any layer's.

Use a power-of-two `cell_m` so the meshing blocks weld exactly. An overhang that stands above
the ground is not a volume layer (the clip removes it): author it as a `Gltf` or `Collision`
source. Errors: `meshlayers_volume_mesh` (the mesher refused the field),
`meshlayers_volume_grid` (empty box, bad `cell_m` or `iso`), `meshlayers_volume_mouths` (the
loops do not match `fills`, do not close inside the box, or a loop is not star-shaped from its
mean point).
