# Voxel far proxies

A tree's cluster hierarchy (`book/agents/visbuffer.md`) stops getting cheaper where the simplifier
cannot merge its pieces: leaves, needles and twigs are separate parts, so the coarsest cut of a tree
still has thousands of triangles at any distance. A voxel far proxy is a coarse voxel copy of the mesh,
cooked next to its meshlets, that takes over beyond the last cluster level (ADR-2445). It plugs into the
same hierarchy as extra coarse levels, so the switch follows the same one-pixel error rule as every
other level step. There is no impostor swap, no cross-fade and no pop. Shadows switch the same way.

## Cooking

Add a `voxel_far` block to the glTF's `.meta` settings (it is off by default):

```ron
(meshlets: true, voxel_far: Some((resolution: 32)))
```

`resolution` is the number of voxels along the mesh's longest axis at the finest level. It is rounded up
to a power of two, from 1 to 256, and defaults to 32 (`voxel_far: Some(())`). For each mesh primitive,
`regia cook` writes the sub-asset `<gltf>#voxels/<node>/primitive_<i>` (type
`regia_render::VoxelFarProxy`, `.fvox` format version 2, `docs/formats/voxel-far.md`). It is built over
the same decoded mesh as the primitive's meshlets, so both share one object space. A masked primitive
whose alpha test cuts it away everywhere (a base color factor under the cutoff, an invisible helper) gets
no proxy, and the rest of the file still cooks (ADR-5571). An entity drawing it with a `VoxelFarSource`
falls back to plain meshlets with `voxel_far_not_cooked`.

How the cook builds it:

- **Which voxels are filled.** A voxel is filled when a triangle touches its box. Each filled voxel
  stores the averaged surface inside it: mean normal, vertex color, opacity, and one texture coordinate
  where the material's textures are sampled.
- **Sub-voxel boxes** (v2, ADR-2655). Each triangle is clipped to every voxel it touches, and each voxel
  keeps the box its surface pieces fill, rounded outward to fifteenths of the voxel. A level draws those
  boxes instead of whole cubes: a flat leaf draws as a flat box, a twig as a stick.
- **Coarser levels.** Each coarser level merges 2×2×2 voxels into one, down to a single voxel. A merged
  voxel's box is the box of its children's.
- **Recooking.** The cook is cached by the mesh bytes, the resolution and the cooker version. A changed
  resolution recooks only the proxies.

## Using it

A host extends the primitive's meshlets with the proxy and draws the result like any clustered mesh:

<!-- ignore-reason: needs a GPU asset server and the ids of a cooked mesh and its voxel proxy -->
```rust,ignore
use regia_render::visbuffer::{MeshletMesh, VoxelFarProxy, VoxelFarSettings, VisInstanceSet};

let meshlets = MeshletMesh::load(&mut server, meshlet_id)?;
let proxy = VoxelFarProxy::load(&mut server, voxel_id)?;
let far = proxy.extend_meshlets(&meshlets, &vertices, &VoxelFarSettings::default())?;
renderer.insert_meshlets(mesh_key, Arc::new(far.meshlets));
renderer.set_instance_set(set_key, &VisInstanceSet {
    mesh_key,
    vertices: &far.vertices, // the mesh's vertices, then the voxel faces'
    indices: &indices,       // the mesh's own indices, unchanged
    ..
});
```

- **Faces.** Each voxel level becomes clusters of its outside cube faces. Each voxel's corners carry its
  normal, color and texture coordinate.
- **Order of levels.** The first voxel level sits above the hierarchy's coarsest triangle clusters, and
  each coarser voxel level above the one before.
- **When a level draws.** A level's error is the farthest any drawn point can lie from the surface. A box
  that bounds the surface touches it on every face, so a full box is at most 1.5 voxel edges from it and a
  flat or thin one less (`voxel_extent_error`); the level's error is the largest over its voxels. A whole
  cube (a v1 proxy) uses the cube's diagonal, 1.73 edges.
- **Shapes.** Boxes that do not touch their neighbours draw all six faces, so a level of boxes can cost more
  triangles than the same level of cubes. A level therefore tries its shapes from the smallest error up and
  takes the first one cheaper than the coarsest triangle cut (`VoxelFarLevelUse::shape`): all boxes; mixed
  (a voxel draws its whole cube where the cube is within 1.5, then 1.6, voxel edges of its surface, else its
  box); all cubes. The box still bounds a whole cube's error: a cube whose box spans it on one axis is at
  most 1.5 edges from its surface (`voxel_cube_error`), not the diagonal. So even all-cube levels usually
  switch nearer than in v1 at the same silhouette error (the counted margin on the test canopy is in the
  cost section). The visibility buffer's LOD cut
  draws it once that error is within `lod_threshold_px` (`render.set_visbuffer`). All levels share one
  bounding sphere, so a whole instance switches at once: never half voxels and half triangles, never
  both.
- **Shadows.** Shadow passes pick levels by their texel size. A far cascade therefore casts voxels.
- **Skipped levels.** A level is used only while it has fewer triangles than the coarsest triangle cut
  (`VoxelFarSettings::max_cost_ratio`). `far.levels` lists the levels used. An empty list means no level
  was cheaper, and the meshlets come back unchanged.
- **Opacity.** A voxel's opacity is the share of view rays its surface stops: 1 for a closed or flat
  surface, lower for scattered leaves (a leaf-area attenuation). With `VoxelFarSettings::opacity_dropout`
  (on by default) a voxel is kept only above a fixed per-voxel threshold (`voxel_dither`), so a canopy
  level lets light and view rays through like its leaves instead of drawing as a solid shell. The kept
  set is the same on every machine and frame.
- **Error scale.** `VoxelFarSettings::error_scale` below 1 moves the switch closer, at a visible error.
- **Forward path.** The voxel vertices are appended to the mesh's vertices, and the mesh keeps its own
  indices. A mesh that falls back to the forward path never draws them.
- **Lighting and effects.** The faces are ordinary opaque triangles: depth, the material, SSAO, fog and
  the GI inputs see them like any surface. `VoxelFarProxy::mesh_sdf(level, resolution)` gives a level as
  a signed-distance volume for probe rays.

The switch distance for a level is `error * pixels_per_unit / threshold_px + radius`, measured from the
bounding sphere's centre. `pixels_per_unit` is the viewport height / (2 tan(fov_y / 2)).

Not covered yet:

- Masked leaf cards are covered since ADR-3108 (see "Masked leaf cards" below). Since ADR-3227 the cook's coverage test
  applies the base color's `KHR_texture_transform` and leaves a `vertex_ao` alpha out ("Coverage test inputs" below).
  Still not covered: the renderer applies no UV transform yet (the cooked material does not carry it), so on such a
  material near clusters and voxel faces both sample the untransformed coordinate; the voxel opacity already follows
  the transformed coverage.

## Mesh entities

A placed mesh (an ECS entity with a `MeshRenderer`) gets the voxel levels through the meshlet bridge
(`regia_framework::meshlet_bridge`, `book/agents/visbuffer.md`). Give the entity a `VoxelFarSource` beside
its `MeshletSource`:

<!-- ignore-reason: fragment of a game system: `commands`, `server` and `tree` come from the caller, and the paths need a cooked project -->
```rust,ignore
use regia_framework::meshlet_bridge::{MeshletSource, VoxelFarSource};

let path = "assets/trees/oak.gltf#mesh/Oak/primitive_0";
commands.entity(tree).insert((
    MeshletSource::for_mesh_path(&server, path).expect("meshlets cooked"),
    VoxelFarSource::for_mesh_path(&server, path).expect("voxel_far cooked"),
));
```

- **Same mesh, same key.** The voxel vertices travel inside the inserted meshlets
  (`MeshletMesh::appended_vertices`, from `VoxelFarMesh::bridged`). The entity keeps its mesh, its mesh key and
  its forward vertex buffer. The bridge checks the payload against the mesh's own vertex count
  (`MeshletMesh::base_vertex_count`), so the extended DAG is not a mismatch.
- **Shared meshes.** Entities with the same mesh share one insert. The first entity's proxy and settings
  win, as with meshlets.
- **Fallback, never a panic.** A missing, damaged or old `.fvox`, or one that does not extend the DAG, logs one
  warning, and the mesh takes its plain meshlets. `render.voxel_far_stats` lists it under `fallbacks` with
  its code (`voxel_far_not_cooked`, `voxel_far_unreadable`, `voxel_far_old_version`, `voxel_far_invalid`).
- **Changes.** A recook and hot reload of the `.fvox`, new `VoxelFarSource::settings`, or removing the
  component inserts the meshlets again.

`render.voxel_far_stats {}` reports what the levels add:

```json
{ "bridge": true, "entities": 120, "meshes": 3, "meshes_without_levels": 0, "levels": 15,
  "clusters": 291, "triangles": 11796, "vertices": 17355, "gpu_bytes": 1245000,
  "fallbacks": [{ "asset": "…", "code": "voxel_far_not_cooked", "message": "…" }],
  "last_frame": { "frames": 812, "instances": 4100, "candidate_clusters": 96000, "resident_meshes": 40,
                  "geometry_bytes": 52000000 },
  "arena": { "meshes": 40, "pending": 0, "clusters": 9100, "used_bytes": 54000000 } }
```

(Illustrative values.) `gpu_bytes` counts, once per mesh, the appended vertices (64 B each), the cluster
vertex lists and triangles (4 B per entry) and the cluster records. Without the bridge or a renderer the
counts are zero and `last_frame` and `arena` are null.

## Inspecting a proxy

`assets.voxel_far { asset, viewport_height?, fov_y?, threshold_px? }` reads a cooked proxy through the
running asset server. `asset` is the sub-asset's path (`trees/oak.gltf#voxels/Oak/primitive_0`) or its
id. The viewport defaults to 1440 pixels high with `fov_y` 1.0 rad and a 1-pixel threshold.

The shape of an answer (illustrative values, close to the test canopy below):

<!-- not-a-request -->
```json
{ "asset": "assets/trees/oak.gltf#voxels/Oak/primitive_0", "id": "…", "format_version": 2, "boxes": true,
  "source_triangles": 6000, "extent": 1.9, "bounds": [0.0, 0.0, 0.0, 1.0], "bytes": 270640,
  "bricks": 475, "voxels": 16429,
  "levels": [{ "level": 1, "resolution": 16, "voxel_size": 0.11, "error": 0.19, "bricks": 64,
               "voxels": 1400, "surface_triangles": 2928, "surface_vertices": 2900,
               "gpu_bytes": 200000, "switch_distance_m": 250.0 }, …] }
```

The fields:

- `bytes`: the cooked blob.
- `gpu_bytes`: what a level adds to the visibility buffer's geometry arena (vertices, cluster lists,
  triangles).
- `switch_distance_m`: where the level takes over for the given viewport.
- `error` against `cube_error`: the level's error from its boxes and what a whole cube would cost (the voxel
  diagonal). `boxes` is false for a v1 proxy or one whose boxes are all whole.

A level whose `surface_triangles` is not below the coarsest triangle cut is skipped at runtime. Tell
that cut from `render.get_visbuffer`'s cluster counts.

Errors:

| Code | Meaning |
|---|---|
| `voxel_far_bad_param` | `asset` is missing, or `viewport_height` (1 to 16384), `fov_y` (0.01 to 3.1) or `threshold_px` (0.01 to 1024) is out of range |
| `voxel_far_not_cooked` (F0016) | No pak or cooked cache holds the proxy: add the `voxel_far` block and run `regia cook` |
| `voxel_far_unreadable` (F0013) | The cooked file is not a voxel proxy (bad wrapper, another asset type, bad magic) |
| `voxel_far_old_version` (F0013) | Cooked with a format version this build does not read (v1 to v3 load): `regia cook` rebuilds it |
| `voxel_far_invalid` (F0013) | Truncated, or a level, brick or sample out of range |

An unknown asset path or id, or an app without an `AssetServer`, answers F0016.

## Cost

Per frame, a voxel level is clusters like any other, so the existing cull counters
(`render.get_visbuffer`, `FrameRenderer::visbuffer_counters`) count them. Every voxel cluster is a
cull candidate for every instance, at any distance, so an instance's cull work grows with the levels
it carries. Memory is per asset, not per instance:

- The `.fvox` blob takes 20 bytes per filled voxel (16 for the sample, 4 for its box) plus 16 per brick.
- At runtime, each used level adds its face vertices (at most 8 per voxel) and cluster records to the
  arena once per mesh, shared by every instance.

Measured on the tests' 3000-leaf canopy (6000 triangles the simplifier cannot reduce, 32-voxel cook, format v2):

| What | Value |
|---|---|
| Cooked blob | 336 356 B (16 429 voxels in 475 bricks, six levels; 20 B per voxel with its box) |
| Levels used | 5 (16³ down to 1); the first draws `Mixed` (cubes within 1.5 edges, boxes elsewhere) with 4278 triangles, the next 1748 |
| Added to the arena | 8240 vertices, 137 clusters: 586 088 B with the cluster lists, 13 152 B of cluster records |
| Takes over at 1440p, 1 rad, 1 px | 221 m with the boxes, 255 m as whole cubes (v1): 34 m (13.3 %) nearer |
| Same, lavapipe test viewport (128 px, 2 px threshold) | 11.09 m against 12.62 m (12.1 % nearer) |
| Placed mesh (bridged DAG) against the instance set | the same clusters (115 before the switch, 100 past it), ≥ 99.7 % of pixels equal |
| Shadow jobs at a 64-texel shadow threshold | 865 → 79 shadow clusters |

Counted cost on lavapipe (`voxel_far_cost_table`, 64 canopies, 128 px viewport, 2 px threshold). Near-only puts
them at 0.3 to 0.9 times the switch distance, far-only at 1.2 to 4 times, mixed at 0.3 to 4 times. Work items and
clusters are the GPU cull counters, shadow clusters are summed over the shadow jobs, cut triangles are the CPU LOD
cut before frustum and occlusion culling, and arena bytes are the visibility buffer's resident geometry:

| View | DAG | Work items | Clusters | Shadow clusters | Cut triangles | Arena bytes |
|---|---|---|---|---|---|---|
| near-only | plain | 192 | 7264 | 56 982 | 384 000 | 858 048 |
| near-only | voxel | 194 | 7264 | 46 998 | 384 000 | 1 480 632 |
| far-only | plain | 192 | 7560 | 41 802 | 384 000 | 858 048 |
| far-only | voxel | 80 | 2961 | 31 818 | 146 810 | 1 480 632 |
| mixed | plain | 192 | 7461 | 45 422 | 384 000 | 858 048 |
| mixed | voxel | 115 | 4341 | 35 438 | 220 514 | 1 480 632 |

- **Near-only.** Views draw the same clusters and cut as the plain DAG. Their shadow jobs still take voxels,
  because those jobs choose levels by texel size.
- **Far-only.** Views draw 61 % fewer clusters and cut 62 % fewer triangles.
- **Cull work.** The per-instance level range (ADR-3915) skips levels an instance cannot select. Near-only
  work items are 192 → 194 (two canopies close to the switch remain in the conservative range); far-only
  work items are 192 → 80, mixed 192 → 115. Previously every view paid 384 work items.
- **Every view** pays 0.6 MB of arena memory, once per mesh.

Rerun with `cargo test -p regia_render --test visbuffer_voxel_far voxel_far_cost_table -- --nocapture --test-threads=1`.

Frame times on a real GPU are not measured yet. The `voxel_far_cost` test prints them:
`VOXEL_FAR_N=4000 cargo test -p regia_render --release --test visbuffer_voxel_far voxel_far_cost -- --nocapture`.

## Masked leaf cards

Leaf cards are quads whose base color alpha cuts the leaves out (`alphaMode: MASK`). Since ADR-3108 the cook voxelizes
them with that alpha test instead of the quads' whole area:

- **Which geometry makes voxels.** The cook finds the primitive's material and its base color texture beside the mesh
  (the glTF's `model` sub-asset), decodes the texture and tests coverage the way the masked raster does: texel alpha
  (bilinear, level 0) × base color alpha × vertex alpha against `alphaCutoff`. Each triangle is split into pieces of at
  most half a voxel and sampled evenly; only covered samples make voxels. A fully cut-away card makes none, a fully
  covered one cooks as an opaque triangle.
- **The fill rule.** A voxel's opacity is `c f + (1 - c)(1 - exp(-0.5 L))`: `c` how planar its surface is, `f` the share
  of its surface the alpha test keeps, `L` the kept leaf area over a face's area. The opacity dropout at runtime keeps
  that share of voxels, so a crown stays as see-through as its cards: it neither fills into a blob nor thins away at
  the switch. Since ADR-3227 a voxel whose alpha test cut part of its surface away fits its opacity to the box the
  runtime draws instead: the dropout's expected drawn area in the three axis views equals the covered surface's
  projection there (a see-through canopy's 8³ level went from 1.69 to 1.02 of the near coverage).
- **The texture coordinate** each voxel samples is a covered point, so the masked raster's test on the voxel faces
  passes where the voxel stands for leaves.
- **Sampled box errors** (`.fvox` v3). The cook also measures how far each level's boxes lie from the surface it holds
  (distance samples over every box face plus the sampling margin) and the level draws its boxes at that error
  (`box_error` in the report) when it is below the analytic bound (`error`). Flat boxes, a leaf card's piece in a voxel,
  measure their thickness instead of the analytic 1.12 voxel edges, so the switch comes nearer.

`assets.voxel_far` reports per level `box_error`, `mean_opacity` (mean voxel opacity) and `kept_voxels` (the voxels
the default dropout draws). A crown cooked from masked cards shows a mean opacity well below 1 on its first levels; an
opaque mesh stays near 1.

### Coverage at the switch

`crown_coverage_at_the_switch_distance_matches_the_near_level` (`regia_render/tests/visbuffer_dense_bench.rs`) cooks a
fixed see-through crown (300 bent leaf cards, a six-leaf mask that keeps about 40 %) and compares, for each voxel
level, the crown's covered share at that level's switch distance (orthographic views along the three axes at the pixel
size where the level's error is 1 px, 4 × 4 samples per pixel) against the full-detail crown alpha-tested at every
sample. It measures inside the crown's silhouette shrunk by the level's error: the silhouette ring is the error the
level is allowed, and on a crown a few pixels across it is most of the crown. The bound is 25 % either way, for the
voxel faces alpha-tested at their texture coordinate (the masked raster) and untested. Measured: 98.5 % and 104.2 % of
the near interior on the two measurable levels, both ways. The area cook (before ADR-3108) fails it: untested it fills
the interior (103 % to 104 %, and 140 % to 190 % of the whole view), alpha-tested it thins it (57 % and 34 %).

### Coverage test inputs

The cooked material carries neither the base color texture's `KHR_texture_transform` nor whether `COLOR_0` alpha is
occlusion, so the glTF cook passes both from the import (ADR-3227):

- **Texture transform.** The test reads the texture at the transformed coordinate (offset, rotation, scale as glTF
  defines them). A voxel's stored texture coordinate stays the mesh's, so the near clusters and the voxel faces sample
  the same way. A transform that names another UV set reads UV 0 and the cook logs a warning.
- **`vertex_ao`.** With the importer's `vertex_ao` setting the vertex alpha is occlusion: the coverage test leaves it
  out, so a crown with baked occlusion keeps all its leaves at a distance (the occlusion stays a shading term).
  Without the setting the vertex alpha is opacity and stays in the test, as glTF defines it.

Both enter the cache key only when they change the test.

### Coverage at the switch, per asset (`.fvox` v4)

The cook measures each level's screen coverage at its switch distance and stores it in the proxy (ADR-3227).
`assets.voxel_far` reports it:

- per level, `coverage_at_switch`: the level's covered share of the view over the alpha-tested source's. 1 draws as much
  of the view as the near clusters; below 1 the far canopy thins out, above 1 it swells into a blob.
- per asset, `coverage_at_switch: { first_level, min, max }`: the first level (where the clusters hand over) and the
  range over every level. `null` for a proxy cooked before v4 (`regia cook` measures it).

```json
{"jsonrpc":"2.0","id":1,"method":"assets.voxel_far","params":{"asset":"assets/trees/oak.gltf#voxels/Crown/primitive_0"}}
{"jsonrpc":"2.0","id":1,"result":{ "asset": "assets/trees/oak.gltf#voxels/Crown/primitive_0", "format_version": 4,
  "coverage_at_switch": { "first_level": 0.97, "min": 0.91, "max": 1.12 },
  "levels": [{ "level": 0, "coverage_at_switch": 0.97, "mean_opacity": 0.61, "kept_voxels": 5210, … }, …] }}
```

How it is measured: three orthographic views along the grid axes at 4 pixels per level-0 voxel (at most 512 across).
The source counts a pixel where a triangle's hit passes the material's alpha test; a level counts the pixels its
voxels cover as the runtime draws them: the voxels the default opacity dropout keeps, each as its box, alpha-tested at
its texture coordinate. Past the switch distance a pixel is larger than the level's error, so a converged pixel shows
the share of it the covered surface fills, and the sum over the view is the covered area at any pixel size.

What to do with it: a first level well below 1 means the far canopy thins at the switch (check the material's cutoff
and the cook resolution); well above 1 means it swells (a coarse `resolution`, or an opaque material on cards). The
coarsest levels (4³ voxels and below) hold a crown in a handful of voxels and drift furthest.

Tests: `a_masked_canopy_keeps_its_coverage_at_the_switch_distance` (`regia_asset_pipeline`, the cook's estimate on a
see-through and a dense leaf-card canopy within 20 % on the 32³, 16³ and 8³ levels) and
`alpha_tested_voxel_levels_keep_the_cluster_coverage` (`regia_render/tests/visbuffer_masked_coverage.rs`, the levels as
the DAG cut draws them against the cluster path on the same CPU raster).

### Synthetic forest

`forest_cost_table_counts` scatters 100 000 of those trees (a 160-triangle trunk, the 7200-triangle crown with its
voxel levels) on a jittered 7 m grid and counts the LOD cut per quality tier, against the plain cluster DAG. Rerun:

```sh
cargo test --release -p regia_render --test visbuffer_dense_bench forest_cost_table_counts -- --nocapture
```

Counted on the build server (11-05; counts, so any machine gives the same numbers). The tree: trunk 160 triangles in 3 clusters,
crown 7200 triangles in 258 clusters, crown voxel levels used at DAG levels 2 to 6 (5358, 1008, 158, 12 and 12
triangles; the first draws `Mixed`, the rest `Boxes`). The camera stands at the forest's edge looking down the rows;
"render px" is the tier's output at its render scale, the size the cut projects errors with; clusters and triangles
are the LOD cut of every tree in the view frustum, before occlusion culling; arena bytes are the two meshes' resident
cluster geometry (64 B per vertex, the cluster lists and records), once for all 100 000 trees:

| Tier | Render px | Trees in view | DAG | Clusters | Triangles | Voxel clusters | Voxel triangles | Arena bytes |
|---|---|---|---|---|---|---|---|---|
| mobile_low | 896x504 | 74 642 | plain | 9 703 460 | 409 038 160 | 0 | 0 | 663 036 |
| mobile_low | 896x504 | 74 642 | voxel | 377 984 | 19 667 186 | 183 372 | 8 673 826 | 1 103 468 |
| mobile_high | 1632x918 | 74 642 | plain | 9 703 463 | 409 038 400 | 0 | 0 | 663 036 |
| mobile_high | 1632x918 | 74 642 | voxel | 880 617 | 47 653 418 | 416 263 | 25 368 418 | 1 103 468 |
| deck | 896x560 | 71 781 | plain | 9 331 530 | 393 359 880 | 0 | 0 | 663 036 |
| deck | 896x560 | 71 781 | voxel | 405 768 | 21 254 090 | 195 312 | 9 706 610 | 1 103 468 |
| desktop_low | 1440x810 | 74 642 | plain | 9 703 463 | 409 038 400 | 0 | 0 | 663 036 |
| desktop_low | 1440x810 | 74 642 | voxel | 724 221 | 38 988 444 | 345 007 | 20 267 444 | 1 103 468 |
| desktop_high | 1920x1080 | 74 642 | plain | 9 703 465 | 409 038 560 | 0 | 0 | 663 036 |
| desktop_high | 1920x1080 | 74 642 | voxel | 1 138 828 | 61 724 302 | 527 025 | 33 266 942 | 1 103 468 |
| desktop_ultra | 3840x2160 | 74 642 | plain | 9 703 476 | 409 039 440 | 0 | 0 | 663 036 |
| desktop_ultra | 3840x2160 | 74 642 | voxel | 3 615 243 | 191 690 878 | 1 440 336 | 97 800 838 | 1 103 468 |

The leaf cards cannot be simplified (their borders are locked), so without the voxel levels every tree in view keeps
its root cut at any distance (about 5480 triangles per tree with the trunk): 409 million triangles on every tier. With them the cut is 6.6
times smaller at Desktop High (62 million) and 21 times smaller at Mobile Low, for 0.44 MB more arena memory in total.
Half of what remains is voxel geometry (far trees), the rest the near trees' leaves. These are cut counts: frustum and
occlusion culling on the GPU draw fewer, which the card run reports.

The card run renders the same forest per tier and prints frame and GPU times, cull work items, clusters drawn, masked
clusters and arena bytes (masked faces are alpha-tested on the visibility path with ADR-3009's frame hookup):

```sh
REGIA_VIS_BENCH_FRAMES=16 cargo test --release -p regia_render --test visbuffer_dense_bench \
    forest_voxel_far_bench -- --ignored --nocapture
# one tier, fewer trees: REGIA_FOREST_TIERS=desktop_high REGIA_FOREST_INSTANCES=20000
```
