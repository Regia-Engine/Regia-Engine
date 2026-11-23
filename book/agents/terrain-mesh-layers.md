# Mesh-layer seams

A cooked `<map>.meshlayers` file is the one triangle store (spec 121). This page is the run-time seam
on top of it (ADR-8291, spec 26 § 2a): the zipper, the ground collar, and `terrain.seam_report`.
The formats themselves are on [terrain-mesh-layers-formats](terrain-mesh-layers-formats.md).
The agent methods over the cooked geometry, `terrain.surface_at`, `terrain.mesh_layers`,
`terrain.bake_report` and `terrain.blockers` (spec 121 § 12, ADR-8386, ADR-8627), are at the end of
this page. There is one `terrain.mesh_layers`: the level's chunks and the cooked geometry answer
together.

The raster does not draw the band. That draw is a later lane. Physics, the path tracer and the bake
read the collar. The collar replaces the temporary band W-10 stored on the canonical triangles.

## What the cook's zipper writes

For a Passages source the zipper reads DO27's pinned mesher output and W-10's ring pin. It does not
move the pinned mesher's vertices. It joins the authored open boundary to the lattice ring by arc
length (Turk and Levoy, SIGGRAPH 1994), adds a vertical skirt of `skirt_m` (default 0.25 m) facing
out of the hole, and moves throat triangles (inside the footprint and less than `throat_m`, default
2 m, under the LOD-0 ground) from the body into the band.

Refusals, as `SeamCookError::code()`:

| Code | When |
| --- | --- |
| `seam_loop_missing` | no boundary loop stays within `max_stitch_gap_m` (default 1 m) of the ring |
| `seam_strip_folds` | the strip's triangles do not share a facing after one flip |
| `seam_above_ground` | an authored loop vertex outside the cut sits more than 1 cm above the LOD-0 ground |

A closed mouth keeps the mesh and draws no band and no throat.

Collision-sourced layers can pin their open-boundary vertices (`TunnelMeshOptions { pin_boundary: true }`,
default false). The default mesh is unchanged. `TunnelDetail` did not grow a field.

## terrain.seam_report

```json
{ "hole": "north_mouth", "pass": "main", "eye": [12.0, -4.0], "texel_m": 0.5 }
```

Read-only. Measures every open mouth, or one `hole`.

- `hole` (optional): a filled mouth's id. Omit it to walk every mouth.
- `pass` (optional): `main` (the camera's `select_nodes`) or `cascade0`, `cascade1`, `cascade2`,
  `cascade3` (`select_nodes_min_level`, the same stop the shadow cascades use). Default `main`.
- `eye` (optional): `[x, z]` in metres. Omitted, the lowest `Camera`'s translation is used, otherwise
  the origin.
- `texel_m` (optional): cascade texel in metres. Default is half of that cascade's historical cell,
  so cascade k stops at level k. Ignored for `main`.

The reply is `{ pass, eye, mouths, max_gap_m, max_lip_m, worst_at, ring_vertices }`. `mouths` is
`{ hole, open }` for each mouth walked. Gap is drawn terrain above the ring; lip is the ring above
the drawn terrain. Samples sit on the unmorphed lattice edge. The render budget is 1e-4 m.
A closed mouth adds no ring vertices and no gap.

No `Terrain` and no `hole` answers an empty report (`max_gap_m` 0, `mouths` []). That is also the
answer when mesh layers are absent and `hole` is omitted.

Errors:

| Code | When |
| --- | --- |
| `unknown_hole` | `hole` is not a named hole and no mesh layer fills it, or there is no terrain |
| `hole_not_filled` | the named hole exists and no mouth fills it |
| `F0002` | `pass`, `eye` or `texel_m` is the wrong shape |

A height edit under a mouth (`terrain.patch_heights`, a crater) rebuilds the band's LOD-0 heights
and the collar inside the dirty rectangle and bumps `TerrainMeshLayers::generation`. It does not
move a ring vertex's XZ. Changing the footprint of a filled hole is `hole_filled_by_layer` on
`terrain.set_hole` (see [terrain runtime](terrain-runtime.md)).

## Level-0 pitch

CDLOD level 0 nests in the lattice. When the caller asks for the full-detail root and that root's
cell count is larger than the sample intervals `n - 1`, the quadtree span is `W * cells / (n - 1)`
and the level-0 pitch is `W / (n - 1)`, the lattice pitch. A 4096-sample map overshoots the map by
one sample. Every other root, including a coarser root on the same map, keeps `world_width` with no
extra arithmetic, so a map that already nests is bit-identical.

## Layer chunks: cook, streaming and LOD (ADR-8398)

`regia_asset_pipeline::terrain_mesh_layers::cook_mesh_layers` writes the cooked `<map>.meshlayers`
next to the map. Its canonical section is `regia_world_geom::cook::cook`'s, byte for byte (one
writer, one `content_hash`). The cook then cuts each layer's canonical triangles on the CDLOD node
grid at the chunk level: the coarsest level whose node is at most `chunk_m` (default 64 m). A
triangle goes to the chunk that holds its XZ centroid, so two neighbors share their border
vertices bit for bit. LOD `i` (up to 4 LODs, at most `root_level + 1`) is simplified with meshopt
to an absolute error of at most the terrain's level-`i` cell (`node_world_size(i) / grid_size`).
Chunk borders are locked, and so is every vertex within the layer's `max_stitch_gap_m` of a mouth
ring. Each LOD is a `CookedMesh` v3 sub-asset of the map, `meshlayers/<layer>/<x>_<z>/lod<i>`, in
the cooked cache. The layer table (`TerrainMeshLayersCooked`, version 2) is the file's chunk
section, stored after W-07's presentation section (ADR-8576). An unchanged input writes nothing.

The level-terrain load reads `<map>.meshlayers` and puts `TerrainMeshLayers` on the terrain
entity. A map without the file gets no component and draws as before. A file that does not load
is logged with its code and gives no layers. The load does not fail.

`TerrainLayerStream` (a resource; `TerrainPlugin` drives it each `Update`) wants a chunk while
its rectangle is within `visible_radius` of a `StreamViews` source. A chunk draws LOD `i` while
its rectangle distance is in `[range(i - 1), range(i))`, with `range(l) = range_factor *
node_world_size(l)`: the terrain's own ranges. The coarsest LOD of every chunk stays resident for
the life of the layers. Finer LODs fit in `budget_bytes`, nearest chunk first, ties in
`(layer, z, x)` order. A finer LOD that is not loaded yet draws the coarsest one. One entity per
drawn chunk: `MeshRenderer`, `MapLayer(map_layer)`, `TerrainLayerChunk { layer, node, lod }`,
and `NotShadowCaster` when the layer's `cast_shadows` is false. The chunks draw with the standard
opaque forward set, which the frame warm builds, so a chunk that streams in compiles no pipeline.

### Volume layers (ADR-10071)

A `Volume { sdf, iso, cell_m }` layer cooks through
`cook_mesh_layers_with_volumes(request, &[VolumeSource])`: the host gives the `SdfField` for each
`sdf` name (no volume file format exists yet). The field is meshed by dual contouring and clipped
under the LOD-0 ground; the clipped mesh is the layer's canonical (query) triangles, and each clip
loop the layer fills is that hole's footprint, so the cut, collar and zipper are the same as for
a `Passages` layer. `terrain.mesh_layers`, `terrain.surface_at` and `terrain.bake_report` read a
volume layer like any other; no method is added. Codes: `cook_not_built` (a `Volume` layer with
no source), `meshlayers_volume_mesh`, `meshlayers_volume_grid`, `meshlayers_volume_mouths`. Guide:
[Volumetric terrain](../user-guide/volume-mesh.md#volumes-as-terrain-mesh-layers-adr-10071).

## `regia cook` and levels (ADR-8451)

`regia cook` (any `cook_project` call) cooks the mesh layers of every level that has them, with no
flag. A level has them when its `terrain` block names a heightfield (`layers`) and its map has a
`<map stem>.meshlayers.ron` next to it. The sim ground is the cooked heightfield with the block's
mapping: `width_m` is `world_width_m` (else the cooked index's `size_m`, else one metre a sample),
the height of one count is `height_scale_m / 65535`, and count 0 is `height_offset_m`. The run-time
terrain load reads the same values (`regia_terrain::load_level_ground`). When the cooked terrain
index carries a count mapping (ADR-8471), the mapping decides instead, as it does for the run-time
terrain (ADR-8842): `width_m` is `(n - 1) * cell_m`, one count is `meters_per_count`, count 0 is
`-zero_counts * meters_per_count`, and the block's scale and offset do not apply. A block
`world_width_m` that contradicts the mapping fails with `level_terrain_mapping`, and a grid whose
center is not the origin with `meshlayers_ground_offset`. The pass writes
`<map stem>.meshlayers` next to the map, the chunk LODs into the platform's cooked cache, and one
manifest row per chunk LOD, `assets/<map stem>.meshlayers.ron#meshlayers/<layer>/<x>_<z>/lod<i>`,
so the paks carry them. An unchanged level writes nothing. Two levels that name one map with the
same ground cook it once.

A layer's `material` resolves like an asset-registry lookup: an asset id written as text, an
`assets/` path, or a path under `assets/`. It must be a cooked `CookedStandardMaterial`, and the
presentation stores its id (`material_id`). There is no fallback: a missing material fails the
cook. An empty `material` is the default rock. At run time the chunks draw the material with its
texture slots, through the asset server.

Cook errors (in the `regia cook` report, keyed by the level path, the code in the message):

| Code | Meaning |
| --- | --- |
| `meshlayers_material_missing` | a layer's `material` names no cooked asset of the project |
| `meshlayers_material_type` | a layer's `material` names an asset that is not a `CookedStandardMaterial` |
| `meshlayers_no_heightfield` | the map has a `.meshlayers.ron`, but the level's terrain block names no `layers` file |
| `meshlayers_ground_not_square` | the cooked heightfield is not square (the sim ground lattice is) |
| `meshlayers_ground_offset` | the cooked count mapping (ADR-8471) centers the ground away from the origin; the sim ground lattice is centered on it, so set `sample0_x_m` and `sample0_z_m` to `-(n - 1) * cell_m / 2` (ADR-8842) |
| `meshlayers_level_conflict` | two levels name the same map with a different map id or height mapping; the map is not cooked |
| `meshlayers_volume_mesh` | a `Volume` layer's field did not mesh (a non-finite sample or gradient, a grid the mesher refuses; ADR-10071) |
| `meshlayers_volume_grid` | a `Volume` layer's `cell_m`, `iso` or meshed box cannot form a grid |
| `meshlayers_volume_mouths` | a `Volume` layer's clip loops on the ground do not match its `fills` (count, an open chain, or a loop not star-shaped from its mean point) |
| `meshlayers_level_read` | the level or its `LevelSettings` do not read |
| `level_terrain_*` | the level's terrain block or its files ([Level terrain](level-terrain.md)) |

Run-time log code: `meshlayers_material_load` (a resolved material did not load; the layer draws
the default rock and the error is logged).

Chunk normals are shared across chunk borders: the cook sums area-weighted face normals over the
whole layer before it cuts it into chunks, so a vertex on a border has the same normal (and the same
box-projected UV) in every chunk and LOD that holds it. There is no lighting seam at a border, also
where neighbors draw different LODs. The chunk importer is 1.1.0, and the presentation records it,
so chunks cooked by 1.0.0 cook again. LOD cross-fade is not built.

## terrain.mesh_layers

Parameters: `map`, `section` (both optional). `{}` →

```text
{ loaded, map, layers: [{ id, chunks, lods, triangles, map_layer, cast_shadows, collider, material, material_id }],
  chunks, grid: { chunk_level, chunk_m, root_level, span_m, range_factor } | null, mouths, sections,
  portals, blockers, resident_bytes, content_hash, generation, geom_generation, stream,
  layer_reports, mouth_reports, section_rows, portal_rows, blocker_rows }
```

Read-only. One method reads two sources (ADR-8576 item 6, ADR-8627):

- **The level's `regia_terrain::TerrainMeshLayers` component** (a level load puts it on the
  terrain entity): `layers`, `chunks` (a count), `grid`, `mouths` (the filled holes' ids),
  `generation` (bumped by a mouth or height edit), and `stream`.
- **The agent's `regia_agent::terrain_mesh_layers::TerrainMeshLayers` resource** (a host inserts it
  after a cook, for the other three methods): `map`, `geom_generation` (the blocker generation
  `terrain.surface_at` and `terrain.blockers` answer as `generation`), and the rows
  `layer_reports` (the cook's per-layer `{ id, triangles, welded_vertices, trimmed_triangles,
  degenerate_triangles, render_triangles, sections, plug_triangles, skylight_segments_opened }`),
  `mouth_reports` (`{ id, layer, cut_triangles, collar_triangles }`), `section_rows` (`{ id,
  source, covered, blocker, portals, triangles }`), `portal_rows` (`{ id, sides, mouth, nav_mm,
  blocker }`) and `blocker_rows` (`{ id, section, active }`). Without the resource `map` and
  `geom_generation` are `null` and the rows are `[]`; without a cook report on it,
  `layer_reports` and `mouth_reports` are `[]`.

`sections`, `portals` and `blockers` are counts, and they, `resident_bytes` and `content_hash` (64
lowercase hex characters) describe the resource's geometry when the resource is present, otherwise
the component's. `section` keeps only that section's plugs in `blocker_rows`; every other key
stays complete.

`material` is the layer's path as written; `material_id` is the cooked material it resolved to
(an asset id as text), or `null` for the default rock (ADR-8451).

`stream` is `null` without a `TerrainLayerStream`, otherwise `{ resident: [lod0, lod1, lod2,
lod3], pending, bytes, budget_bytes, coarsest_bytes, resident_bytes, coarsest_resident_bytes, drawn,
wanted, loads, evictions, failed, visible_radius, fade_frames, fading }`. `resident` counts meshes per LOD. `pending`
counts finer LODs inside the budget that are not loaded yet. `bytes` includes the coarsest set
(`coarsest_bytes`).

`fade_frames` is the LOD cross-fade length (`TerrainLayerStream::fade_frames`, default 8, the switch
frame included; 0 or 1 switch at once) and `fading` the chunks in a cross-fade after the last frame
(ADR-8842). During a fade a chunk has two `TerrainLayerChunk` entities, the incoming LOD with
`MeshLodFade(c)` and the outgoing one with `MeshLodFade(-c)`, so every pixel is drawn by one of
them; `bytes` does not count an outgoing mesh the stream already evicted (its entity keeps it until
the fade ends).

`bytes` and `budget_bytes` are cooked bytes: each chunk LOD's record in the presentation section,
the unit the budget counts in before a mesh loads. The memory the resident meshes hold is
`resident_bytes` (`coarsest_resident_bytes` for the coarsest set): their 96-byte render vertices and
`u32` indices. That is the layer geometry's RAM, and the size the renderer uploads per drawn mesh.
On Switchbacks it is about 3.5 times `bytes`. Read it for a RAM, VRAM or Deck layer budget
(ADR-8873).

A world with neither source answers `loaded: false` with zero counts, `null` hash and empty lists,
never `no_world_geom`: a map without `<map>.meshlayers` is the ordinary case. `map` and `section`
are then not checked (there is nothing to check them against).

| Code | Meaning |
| --- | --- |
| `layers_loading` | `TerrainMeshLayersLoading` is present (this wins over `loaded: false`) |
| `unknown_section` | `section` is not a section id of the cooked geometry |
| `F0003` | `map` does not name the resource's map |
| `F0002` | a parameter other than `map` and `section`, or one of the wrong type |

Load codes (logged by the level load, refused by name by the fuzz target `terrain.mesh_layers_cooked`):

| Code | Meaning |
| --- | --- |
| `meshlayers_presentation_newer` | the chunk section (ADR-8576) was written by a newer engine: cook again |
| `meshlayers_presentation_stale` | the chunk section was written by an older engine (version 1 has no `material_id`): cook again (a version-2 section, from v0.6.0, loads with `collider` false, ADR-8872) |
| `meshlayers_presentation_corrupt` | the chunk section does not decode, or a chunk is outside the grid, unsorted or has 0 or more than 4 LODs |

Cook codes (`MeshLayersCookError::code`): W-06's cook codes, plus `meshlayers_cook_io` (a file
could not be written) and `meshlayers_cook_mesh` (a chunk mesh did not pack).

## Joins: picks, the reference trace, shadows and colliders (ADR-8537)

A terrain with `TerrainMeshLayers` is picked as it is drawn. `world.pick` and `terrain.raycast` cut a filled hole on
its lattice triangles (`HolesMask::contains_drawn`, the CPU mirror of the drawn cut), so ground the lattice keeps is
hit even inside the hole's coverage shape. An open mouth's band and throat at LOD 0 are terrain surface: a ray that
meets them reports `surface: "terrain"`, layer 0 and the terrain entity. A ray into the cut hits the band or the layer's
chunk entities (on the layer's `MapLayer`), never the cut ground. Without the component the pick is unchanged.
No new methods or error codes: `world.pick` and `terrain.raycast` keep their parameters and replies.

`seeing.reference_trace` with scene `current` adds each open mouth's band and collar to the traced export beside the
reference heightfield (which drops every cell touching a hole), as instances with mesh keys
`REFERENCE_PIECE_TAG | mouth << 1 | collar`; the reply's `instances` and `meshes` counts include them. Resident chunk
entities are traced at the LOD they draw.

Chunk entities of a layer with `cast_shadows: false` carry `NotShadowCaster` and stay out of every shadow view; the
other layers' chunks cast.

`TerrainMeshLayers::collider_pieces()` gives the canonical triangles (never the render LODs or the band) per layer and
chunk, for hosts that do not lockstep. A lockstep sim uses `regia_world_geom`'s queries instead. To collide with them
on a tiled terrain, put `regia_physics::TerrainLayerColliders` on the terrain entity (next to its heightfield
`Collider` and `TerrainTileCollision`), with each piece moved into the heightfield's frame (minus the field's center:
the body plus the collider offset). A piece is a `TriMesh` on the terrain's static body while a built tile is under
its chunk, is built in the tile's fixed step within `max_tiles_per_step`, and a hit on it reports the terrain entity.
Without `TerrainTileCollision` (an untiled body) the pieces of each layer (`TerrainColliderPiece::layer`) are one
`TriMesh` on the terrain's body, built in the fixed step after the component, the collider or the body changed, all at
once, and not counted in the tile counters (ADR-8872). The cooked file carries each layer's `collider` flag (chunk
section version 3, ADR-8872): `TerrainMeshLayers::cooked_collider_pieces()` gives the pieces of the flagged layers, and
`terrain.mesh_layers` rows report it as `collider`. The level loader does not insert the component (the heightfield
collider is the host's); the host builds it from `cooked_collider_pieces`. The GI bake join is not built yet (ADR-8537).

With scene `current`, `seeing.reference_trace` also traces every layer chunk the frame drew at LOD 0 (ADR-8872): the
chunk's export instances point at its cooked LOD 0 (keys `REFERENCE_CHUNK_TAG | layer << 32 | z << 16 | x`), read as
the stream reads it (the asset server, else the cooked directory). The range is the drawn set: chunks outside the
stream's visible radius are in neither the raster frame nor the trace. The status reply's `scene_info` then has
`mesh_layer_chunks: { lod0, swapped, failed }` (`lod0` counts every drawn chunk traced at LOD 0, `swapped` the ones the
frame drew coarser); a LOD 0 that does not load keeps its drawn LOD, counts in `failed` and logs the warning code
`meshlayers_reference_lod0`. Without layer chunks the key is absent.

```rust,wrap
# use regia::prelude::*;
# let layers: &regia_terrain::TerrainMeshLayers = todo!();
# let (origin, dir) = ([0.0_f32, 50.0, 0.0], [0.0_f32, -1.0, 0.0]);
# let c = [0.0_f32, 0.0];
# let mut commands: Commands = todo!();
# let terrain: Entity = todo!();
let hit = layers.raycast_bands(origin, dir, 500.0); // Option<BandHit>: hole, distance, point, normal
let pieces = layers.cooked_collider_pieces();      // Vec<LayerColliderPiece> of the `collider: true` layers
// Field center `c` (world XZ of the heightfield's center; 0, 0 for a world-centred map on a body at the origin).
let colliders = regia_physics::TerrainLayerColliders {
    pieces: pieces
        .iter()
        .map(|p| regia_physics::TerrainColliderPiece {
            layer: p.layer,
            rect: p.rect.map(|r| [r[0] - c[0], r[1] - c[1], r[2]]),
            triangles: p.triangles.iter().map(|t| t.map(|v| [v[0] - c[0], v[1], v[2] - c[1]])).collect(),
        })
        .collect(),
};
commands.entity(terrain).insert(colliders);
```

## The drawn cut (lattice windows)

A mouth a cooked mesh layer fills is cut on the ground lattice (ADR-8470): `TerrainPlugin`'s
`sync_mesh_layer_lattice` hands the open mouths' cooked cut to the draw whenever `TerrainMeshLayers`
changes, and every terrain pass discards a pixel whose pre-morph grid position lies on a cut
lattice triangle. Other holes keep their coverage windows. Cliff relief is off within two lattice
cells of the cut.

To check the cut from a test or a tool, `TerrainMeshLayers::cuts(map, x, z)` answers what the GPU
draws at a pre-morph grid point, and `TerrainMeshLayers::lattice_hole_cut(map)` gives the windows
(`None` when the cooked lattice is not the map's sample grid: the mouths then draw from their
coverage windows). There is no new agent method in this step: `terrain.holes` still lists the named
holes, and the band and the per-pass `terrain.seam_report` come with the band draw.

### `terrain.seam_report` on the drawn passes

When the host inserts the draw's `TerrainSharedDraw::draw_passes_handle()` as a resource (next to
`holes_handle()`), the draw publishes every pass it ran for the host camera each frame: `main` and
each active `cascade0`..`cascade3`, with the selection it drew and the eye it morphed around.
`terrain.seam_report` without `eye` and `texel_m` then measures a drawn pass on exactly that
selection and eye: `source` is `draw`. A pass the draw did not run, an explicit `eye` or `texel_m`,
or no linked handle selects again as before (`source` is `selected`). `passes` lists the passes
drawn last frame (empty without the handle). No new error codes.

## Cooked-geometry methods

`terrain.surface_at`, `terrain.bake_report` and `terrain.blockers` read the cooked
`TerrainMeshLayers` resource the host inserts after a cook (`geom`, `state`, `report`, `bake`,
`map`, `sim_owned`). While `TerrainMeshLayersLoading` is present, each method, `terrain.mesh_layers`
too, answers `layers_loading`.

The caller adds its own waist reach before `terrain.surface_at`. A body usually asks at origin plus
0.9 m. The kernel uses `near_y` as given and does not add or subtract that reach.

## terrain.surface_at

Parameters: `points` (required), `tolerance_m`, `map`, `section`.

`points` is an array of `[x, z, near_y]` in metres, at most 64. `tolerance_m` is metres added to `near_y` when the floor under the point is chosen (default 0). `map` is the loaded map id. `section` is a cooked section id. On this method `section` is checked and does not filter `points`.

The reply is `{ generation, results }`. Each result has `at` (the `f32` query `[x, z, near_y]`), `section` (the support floor's section, or null outdoors), `under`, `over` and `support`. `support` is `{ floor, ceiling }`. `under` and `over` come from `surface_at` at the same height and `tolerance_m`. A hit is `{ y, normal, kind, material, section, tri }`. `kind` is `ground`, `floor`, `roof`, `wall` or `blocker`. `tri` is `{ layer, triangle }` or null. `ceiling`, `under` and `over` are null when that surface is absent.

An empty `points` array returns `results: []`.

## terrain.bake_report

Parameter: `map`.

The reply is the cook's `TerrainBakeReport` (`regia_world_geom::checks`, the `bake` field of the
resource, `CookOutput::bake_report` of the cook) plus the loaded geometry and the cook report's
counts: `{ map, generation, content_hash, resident_bytes, passed, waived, failure, ground_mode,
memory_bytes, report_content_hash, welded_vertices, trimmed_triangles, ground, checks, facets }`.

`checks` lists every spec 121 § 10 check the cook ran, passed ones included, in the cook's order,
each `{ code, passed, waived, location, measured, triangles, triangle_hash }` plus `fits` and
`tiles` on a `ground_over_budget` row that has them: the rows `regia terrain check --json` prints.
`passed` is true when no unwaived failure remains; `failure` is then `null`, otherwise the first
unwaived failure's code. `waived` counts the failures an authored waiver covers. `ground_mode` and
`memory_bytes` are the authored ground mode and its spec 121 § 11 sim-ground bytes.
`report_content_hash` is the hash the cook checked; compare it with `content_hash` to see whether
the loaded geometry is the one the report describes.

A check whose sweep input is missing fails at `location` `input_missing`; it is never an empty
pass. The § 10 codes are `render_off_collision`, `nav_off_floor`, `mouth_cut_mismatch`,
`junction_open`, `lane_too_narrow`, `hull_blocked`, `drawn_solid_off_wall`, `hidden_face_in_wall`,
`not_watertight`, `above_ground_outside_cut`, `ground_inside_span`, `seam_gap`, `portal_blocked`,
`zone_mismatch`, `ground_vs_render`, `ground_pitch_seam`, `ground_over_budget`,
`column_over_max_floors`, `header_mismatch`, `lattice_mismatch`, `drift_triangles`,
`drift_manifest`, `drift_mouth`, `drift_portal`, `spawn_not_flat` and `determinism_table`, plus
`ramp_facet_slope_over_limit` (ADR-8359).

`welded_vertices` and `trimmed_triangles` are the sums of the cook report's per-layer counts (the
weld step and the trim step). `ground` is `{ mode, sim_bytes, budget_bytes, tiles_per_side }`, and
`mode` is `full` or `nested:<max_level>`. Without a cook report on the resource the three are
`null`.

`facets` lists floor triangles whose grade is at least 1.5. Grade is `sqrt(nx² + nz²) / abs(ny)`
from the triangle's cross product. A triangle with `abs(ny)` under 1e-6 is omitted so the reply
stays finite. Each facet is `{ x, y, z, slope, section }` at the centroid. The cook refuses a floor
facet steeper than 1.50 (`ramp_facet_slope_over_limit`) unless a waiver in `<map>.meshlayers.ron`
names its triangles, so a cook that loaded shows its K1 facets as waived rows.

No `TerrainBakeReport` on the resource is `no_bake_report`.

## terrain.blockers

Parameters: `set`, `active`, `section`, `map`.

A call with neither `set` nor `active` lists plugs as `{ id, section, active }` plus `generation` and `applied: false`. `section` filters that list. Sending both `set` and `active` flips that plug and returns `applied: true` only when the generation changes. A flip to the current state leaves the generation and returns `applied: false`. Sending one of `set` or `active` without the other is invalid params (`F0002`).

The method needs `edit_scene`, including a read, because the flip is the same method. `sim_owned` on the resource makes `{ set, active }` answer `blocker_sim_owned` and change nothing. The list is still returned by a call that does not flip.

## Errors of the cooked-geometry methods

| Code | When |
|---|---|
| `no_world_geom` | No `TerrainMeshLayers` resource (`terrain.surface_at`, `terrain.bake_report`, `terrain.blockers`; `terrain.mesh_layers` answers `loaded: false`). |
| `layers_loading` | `TerrainMeshLayersLoading` is present. This wins over `no_world_geom`. |
| `bad_point` | `points` is missing, or a point is not three numbers. A token that is not a non-finite spelling is `bad_point`. |
| `non_finite` | A coordinate or `tolerance_m` is NaN or infinite. JSON cannot carry those numbers, so send a string token: `NaN`, `Inf`, `Infinity`, or the same with a sign. |
| `too_many_points` | More than 64 points. More than 65536 points is `F0002` from the protocol array cap, before this code. |
| `out_of_map` | An XZ lies outside the cooked ground lattice. |
| `unknown_section` | `section` is not a cooked section id. |
| `no_bake_report` | The geometry is loaded and the resource has no `TerrainBakeReport` (`bake`). |
| `blocker_unknown` | `set` is not a cooked plug. |
| `blocker_sim_owned` | The sim owns flips, and the call sent `set` and `active`. |
| `F0003` | `map` does not name the loaded map. |

`terrain.segment` and `terrain.ground_edit` are not on this surface.

## Acceptance (Stage B, plan row W-29)

The native acceptance suite reruns W-05's Stage A proofs against the native query (`stage_b`) on cooked fixtures,
plus the bars that decide DO27's Stage B bump (spec 121 § 13.2, § 13.3, § 14 rule 3; ADR-8538). It has no
agent-protocol method of its own: it is a test suite over `WorldGeom` and `GeomState`, and a host reaches the
same queries through `terrain.surface_at`, `terrain.blockers` and `terrain.bake_report`.

Rerun on any host:

```text
cargo test -p regia_world_geom --features stage_b,fixtures --test native_acceptance --test host_step_echo -- --nocapture
cargo test -p regia_world_geom --test stage_a_bits                      # Stage A bits without Stage B
cargo test -p regia_world_geom --features stage_b --test stage_a_bits   # ... and with it: the same table
```

The fixtures are DO27's layouts cooked through `regia_world_geom::cook` on the 4096-sample lattice, every mouth
filled, with the content's authored waivers for its two steep ramp facets (`checks::waive_ramp_facets`). A fixture
whose cook fails another check is cooked with the fewest mouths unfilled that pass, and every test names the mouths it
could not drive. The drive also skips a mouth whose ramp top has no floor, and prints it.

| Test | Bar |
| --- | --- |
| `filled_fixtures_pass_the_cook_checks` | Switchbacks, The Cinder Steps and Atacama Rail cook clean with every mouth filled |
| `cooked_sections_hold_their_own_triangles` | every triangle tagged with a section lies in that section's (or its plug's) ranges, and every ranged triangle carries it, after the seal drops lips |
| `native_sweep_bars_switchbacks_and_cinder` | every hull radius (2.5 to 4.5 m) at every 0.5 m station: 0 blocked probes, wall push 0.000 m (lane tolerance 0.02 m), lid at least 10 m where a lid exists (measured on the cooked millimetre triangles; the query's `f32` figure is printed beside it, ADR-8898); the pilot disc is never blocked across a floor |
| `skylight_climbs_are_open_to_the_sky` | K8: at the middle of every open segment of every skylight climb, no lid and nothing solid within 60 m over the body (the whole climb is cut from the ground, ADR-8898) |
| `native_walks_reach_every_point` | the whole-system walk reaches 688/688 points both ways, the gallery walk 69/69 |
| `native_mouth_hop_every_hull_class_le_5cm_per_tick` | W-11's meter at every mouth of every fixture, pilot (6 and 20 m/s) and hover hull (6, 20, 50 m/s), each with its wall push: at most 0.05 m a tick |
| `cinder_south_junction_hop_le_5cm_per_tick` | K7: the same meter across the Cinder Steps' south junction, every ordered passage pair |
| `rubble_drop_le_step_bound` | K3: the whole-system walk drops at most one pilot step (0.55 m) in a tick; DO27's 2.0 m allowance is gone |
| `two_peer_tunnel_drive_checksums_equal_x86_aarch64` | two peers, two hover hulls, 3600 ticks in and out of every mouth along the tube, a blocker flip, a crater, a segment shot every 20 ticks: equal per-tick hashes (they fold `GeomState::state_hash`), pinned per target in `goldens/tunnel_drive_two_peer.detgold` |
| `floor_step_echo_is_one_tick` | K4 guard: the hover step reads the floor at the pre-move XZ and the ground pass at the post-move XZ, so a step of h shows +h, then about -h for exactly one tick, then the spring climbs back |

Pitch seams (E1) are `pitch_seam_hop_at_full_speed_le_5cm_per_tick` in `tests/lattice_ground.rs` (W-10), the 0.5 EV
bar is `drive_through_every_mouth_steps_le_half_ev` in `tests/zones_from_bake.rs` (W-24), and Stage A's own bits are
`stage_a_bits_are_the_same_with_and_without_stage_b` (W-05).

**Cross-architecture legs.** The drive golden is recorded on x86_64-linux. On another target the test compares
its per-tick hashes with the reference trace and fails on the first diverging tick. To record a target's line after a
passing run, rerun with `REGIA_BLESS=1` on that target. The aarch64 legs are the Mac (`aarch64-macos`), qemu-user
(`cargo xtask det-golden run --legs host,qemu`) and the Android device.

**DO27's Stage B switch (one sim-version bump, outside an alpha week).** DO27 replaces
`ir_content::surface::column` and `TunnelsState::{support, place_floor, confine, underground}` with thin calls on
`WorldGeom::{floors_at, support, place_floor, push_disc, covered_at}`. The waist rule, the hover rules and the on-foot
grid stay DO27's. It adds `QUERY_BITS_VERSION` and `MATH_BITS_VERSION` to its handshake beside `content_hash`, folds
`GeomState::state_hash` into its checksum, and re-records in the same bump: the tunnel-fight pin
(`switchbacks_tunnel_fight_lockstep_checksum_is_pinned`), the predicted-hull drive
(`a_net_client_predicting_its_hull_through_the_tunnels_never_shows_a_vertical_jump`),
`determinism_fuzz_short_fixtures_agree_every_tick`, the 3600-tick replay hash and the lockstep checkpoints.

## The band draw (ADR-8569)

The terrain draw draws each open mouth's band, the zipper strip, the skirt and the throat, in every
pass it draws: the camera's colour (every ground-look, virtual-texture, local-light and cloud-map
variant), depth, velocity and decal-receiver passes, and each sun cascade. Each pass places the
band on its own selection and morph eye: a ring vertex goes on the drawn triangle under it, a skirt
bottom follows the ring vertex above it, and authored and throat vertices stay where the cook put
them. The band is drawn by the terrain's own pipelines, so it adds no pipeline and no warm entry,
and it takes the terrain's ground material. A closed mouth draws no band. Texture views (probe
captures, mirrors, screens, stereo eyes) and the precipitation occlusion tile draw it too
(ADR-8824, below).

`sync_mesh_layer_lattice` hands the bands to the draw with the cut, so an opened mouth shows both on
the next prepare. A host that draws the terrain itself calls
`TerrainSharedDraw::set_mouth_bands(MouthBands::from_layers(&layers).map(Arc::new))`.

`terrain.seam_report` adds `band_vertices` to its reply: the band vertices the measured pass drew
(`source` `draw`). It is 0 when the pass drew no band, and when the pass was selected rather than
read from the draw. `DrawnPass::band` holds the placed vertices for tools and tests.
No new error codes.

### Every pass, with a count (ADR-8824)

Each pass that draws terrain draws the band:

| Pass | Band set |
| --- | --- |
| colour (every variant), depth prepass, velocity, decal receiver | the camera's (set 0) |
| sun cascade `i`, full draw and page redraw under moved meshes | cascade `i`'s (set `1 + i`) |
| precipitation occlusion tile | the camera's (the tile draws the camera's nodes) |
| a texture view's passes and cascades | the view's own sets, placed on the view's selection |

A texture view places its band on its own selection and cascades, like the main camera. The
views take turns on one set of band slots, so a view placed after another one places again.
The precipitation tile now changes its key when a hole or band changes, so the tile redraws.

`terrain.seam_report` returns `band_draws`, or null when no draw linked its counters
(`TerrainSharedDraw::draw_passes_handle` links them). The value is `{ main, views }`. Each camera
has `{ color, depth, velocity, receiver, cascades, cascade_regions, precip_tile }`: the band
draws in each pass since the draw was made. `cascades` and `cascade_regions` have four entries,
the full cascade draws and the page redraws. `views` adds up every texture view. A count that
does not grow means that pass drew the terrain without the band. In Rust, the same counts are
`TerrainDrawStats::band_draws` and `view_band_draws` (`BandDrawCounts`). No new error codes.

The ray-traced sun (`set_engine_sun_traced`) and the GI trace read the heightfield and the hole
coverage, not the drawn triangles, so the band does not cast or bounce there.
