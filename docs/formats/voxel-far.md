# FVOX: voxel far proxy v4

Library: `regia_render::visbuffer::voxel_far` (`VoxelFarProxy::{to_bytes, from_bytes, from_cooked_file, load,
surface, extend_meshlets, mesh_sdf}`). Cook: `regia_asset_pipeline::voxel_far_cook`. Decisions: ADR-2445, ADR-2655
(v2, sub-voxel boxes), ADR-3108 (v3, sampled box errors; alpha-tested cook), ADR-3227 (v4, per-level coverage
estimate; texture transform and `vertex_ao` in the coverage test). Agent
page: `book/agents/voxel-far.md`.

The cooked output of the glTF importer's `voxel_far` setting: one sub-asset per mesh primitive at
`voxels/<node>/primitive_<i>` (the `mesh/` prefix of the primitive's own path swapped for `voxels/`), type path
`regia_render::VoxelFarProxy`, wrapped in the normal cooked header with the mesh as its dependency. It is a
sparse-brick voxel copy of the primitive (in its object space) with a mip chain, which the runtime stacks on the
primitive's cluster DAG as coarser levels past the last cluster LOD.

The payload is little endian:

| Bytes | Content |
|---|---|
| 4 | magic `FVOX` |
| 4 | `format_version` (u32, 4; 1 to 3 are still read, see below) |
| 12 | grid origin (f32 x, y, z: the minimum corner of the cubic grid) |
| 4 | grid extent (f32, the cube's edge length, > 0) |
| 16 | bounding sphere of the source mesh (f32 centre x, y, z, radius) |
| 4 | source triangle count (u32) |
| 4 | level count (u32, at most 32) |
| 4 | brick count (u32, all levels) |
| 4 | voxel count (u32, all levels) |
| 20 per level | `resolution` (u32, 1 to 256, each level half the previous one, at least 1), `voxel_size` (f32), `error` (f32: the farthest a drawn point lies from the surface; never below the level's box bound, see below), `brick_offset`, `brick_count` (u32; levels own contiguous brick ranges in order) |
| 16 per brick | `coord` (u32: brick x in bits 0-9, y in 10-19, z in 20-29, in units of 4 voxels), `voxel_offset` (u32; bricks own contiguous voxel ranges in order), `mask_lo`, `mask_hi` (u32: the 64-bit occupancy mask, bit `x + 4y + 16z` of the brick-local voxel; never 0) |
| 16 per voxel | `normal` (u32: octahedral mean normal, x in bits 0-15, y in 16-31, unorm16), `color` (u32: linear rgb8 in bytes 0-2, opacity in byte 3: the share of view rays the voxel's surface stops, `c + (1 - c)(1 - exp(-0.5 L))` with `L` the surface area over one face's area and `c` the normal consistency, so 1 for a closed or planar surface), `uv` (2 × f32: the texture coordinate the material is sampled at) |
| 4 per voxel (v2 and later) | `extent` (u32, in the same order as the voxels): the sub-voxel box the voxel's surface fills, per axis `k` the low bound in bits `8k..8k+4` and the high bound in bits `8k+4..8k+8`, in fifteenths of the voxel edge from its low corner (rounded outward; low ≤ high); bit 24 marks a voxel the cook could not bound (its box must then be whole, `0x01f0f0f0`, error the cube diagonal); bits 25-31 are 0. `0x00f0f0f0` is the whole voxel |
| 4 per level (v3 and later) | `sampled_error` (f32, in level order): how far the level's drawn boxes lie from the surface as the cook measured it (below), `0 ≤ sampled_error ≤ error`; equal to `error` when the cook measured nothing tighter |
| 4 (v4 only) | `coverage_count` (u32): 0 (nothing measured) or the level count |
| 4 per counted level (v4 only) | `coverage` (f32, in level order): the cook's estimate of the level's screen coverage at its switch distance as a share of the alpha-tested source's (below), `0 ≤ coverage ≤ 64` (`MAX_VOXEL_COVERAGE`) |

Voxels follow their brick's mask in bit order. A level draws each voxel as its box. The cook stores the box of the
clipped surface, which touches the surface on every face, so any point of a box with sizes `d` is within
`min_k sqrt(a_k² + Σ_{j≠k} d_j²)` of the surface (`a_k = d_k / 2`, or `d_k` when the box is one step or less on
`k`), plus 1e-4 voxel: `voxel_extent_error`. A level's `error` is the largest bound of its voxels (the cube diagonal
for a voxel the cook could not bound): 1.5 voxel edges for full boxes against the cube diagonal's 1.73, less for thin
or flat ones. At runtime a level draws boxes, whole cubes, or a mix (a voxel's whole cube where
`voxel_cube_error` is within the level's chosen error), whichever is the smallest error still cheaper than the
mesh's coarsest triangle cut; the format stores only the boxes and the box error. `from_bytes` rejects a bad magic, another version, a truncated
payload, more than 32 levels, a level resolution out of range or not half the previous one, a non-positive extent,
voxel size or a negative error, bricks or voxels not contiguous or not owned by a level, an empty brick, a brick
or voxel outside its level's grid, a reversed box or set reserved bits, a level error below its boxes' bound, and
bytes past the declared arrays.

**Sampled box error (v3, ADR-3108).** The analytic bound above holds for any surface the box could bound; the cook
also measures the surface it has. On each of the six faces of every drawn box it samples a grid of at most 1/8 voxel
spacing, takes each sample's distance to the voxel's surface pieces (the triangles it holds, or the covered
sub-triangles of an alpha-tested one) and adds half the grid cell's diagonal: the distance to a set is 1-Lipschitz,
so no point of the face is farther. A voxel's value is the smaller of that and its analytic bound; the level's is the
largest. A flat box that spans the voxel (analytic 1.12 edges) measures its thickness plus the margin. At runtime a
level drawn as boxes uses `sampled_error` (`VoxelFarProxy::level_box_error`); `error` stays the analytic bound (the
mixed and cube shapes and the validation above use it).

**Alpha-tested geometry (ADR-3108, no layout change).** For a primitive whose material is `alphaMode: MASK`, the cook
reads the material and its base color texture beside the mesh (through the glTF's `model` sub-asset), decodes the
texture and measures coverage with the masked raster's test (bilinear alpha × base color alpha × vertex alpha against
`alphaCutoff`) at stratified samples on sub-triangles of at most half a voxel. Only covered geometry makes voxels; a
voxel's opacity becomes `c f + (1 - c)(1 - exp(-0.5 L))` with `f` its covered over its geometric surface area and `L`
counting covered area only, and its `uv` is a covered sample. A fully covered triangle cooks as an opaque one.
Since ADR-3227 (cooker version 4) a voxel whose alpha test cut part of its surface away instead takes the opacity
fitted to its drawn box: `Σ A_k g(P_k / A_k) / Σ A_k` over the three axes, `A_k` the box's face across axis `k`, `P_k`
the covered area projected along `k`, `g(x) = c min(1, x) + (1 - c)(1 - exp(-x))`, so the dropout's expected drawn
area equals what the covered surface shows.

**Coverage test inputs (ADR-3227, no layout change).** The cooked material carries neither the base color
texture's `KHR_texture_transform` nor whether `COLOR_0` alpha is occlusion, so the glTF cook passes both from the
import (`VoxelFarSource`): the test reads the texture at the transformed coordinate (`uv' = T · R · S · uv`; the
stored `uv` stays the mesh's, so near clusters and voxel faces sample alike), and with the importer's `vertex_ao`
setting it leaves the vertex alpha out (occlusion stays a shading term). A transform naming another UV set reads
UV 0 with a warning. Both enter the cache key only when they change the test.

**Coverage estimate (v4, ADR-3227).** For each level the cook rasterizes three orthographic views along the grid
axes at 4 pixels per level-0 voxel (at most 512 across): the source's triangles, a pixel covered where the hit
passes the material's alpha test (every hit for an opaque mesh), and the level's voxels that the default opacity
dropout draws (`VoxelFarProxy::keeps`), each as its sub-voxel box, alpha-tested at its `uv` as the masked raster
tests its faces. `coverage` is the level's covered pixels over the source's, summed over the three views. Past the
switch distance a pixel is larger than the level's error, so a converged pixel shows the share of it the covered
surface fills; the sum is the covered area whatever the pixel size, which is why one fine raster stands for every
level's switch distance. 1 draws as much of the view as the near clusters; below 1 the far canopy thins, above 1
it swells. The value is clamped to 64. `assets.voxel_far` reports it per level and per asset.

`.meta` settings (RON, in the glTF importer settings):

```ron
(meshlets: true, voxel_far: Some((resolution: 32)))
```

`voxel_far` is `None` (the default: no proxy cooked) or a block with `resolution` (voxels along the longest axis at
level 0, rounded up to a power of two, 1 to 256; default 32). A primitive with no finite triangle fails the cook
with `voxel far cook error`. The cache key covers the mesh bytes, the effective resolution, the format version and
the cooker version (`VOXEL_FAR_COOK_VERSION`), so a changed setting recooks only the proxies. For a masked material
the key also hashes the material's and its base color texture's cooked bytes, so an edited mask recooks.

Migration: v1, v2 and v3 → v4 (`voxel_far_migration_sources()` is `[1, 2, 3]`). A v3 payload has no coverage
block; `from_bytes` reads it with `coverage` empty and the migrator rewrites it at v4 with `coverage_count` 0 (it
draws the same; a recook measures the coverage, `VOXEL_FAR_COOK_VERSION` 4). Before v4: A v2 payload has no sampled errors;
`from_bytes` reads it with `sampled_errors` empty (the box error is `error`), so it draws as before, and the
migrator rewrites it at v3 with each `sampled_error` equal to `error`. Before v3: A v1 payload has no box array; `from_bytes` reads it
with every box whole and keeps its cube-diagonal errors, so it draws as before, and `migrate_cooked_voxel_far_file`
rewrites a cooked v1 file at v2 with the cooked header kept (`regia upgrade-project --write`). A recook gives the tight
boxes and measured errors: the cache key hashes the format and cooker versions (`VOXEL_FAR_COOK_VERSION` 3). Other versions are rejected
(`voxel_far_old_version`). In memory, `VoxelFarProxy::extents` is empty when every box is whole (how a v1 proxy and an
all-whole v2 one load). It has no serde shape, so `cargo xtask check-formats` fingerprints a
description of the layout (`voxel_far_schema_fingerprint()`, registered as `voxel-far`). `regia upgrade-project`
reports and migrates the version of every cooked voxel proxy in the dev cache (`.fckd` files whose type path is
`regia_render::VoxelFarProxy`, read by `cooked_voxel_far_file_version`).
