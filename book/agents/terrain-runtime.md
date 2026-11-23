# Terrain runtime: named holes and height patches

Gameplay changes the ground while the game runs: a tunnel mouth is blasted open, a roof
collapses, a dozer cuts a ramp. Two pieces cover it (ADR-1595):

- **Named holes**: a hole with an id that opens and closes at runtime. Inside an open hole the
  terrain is not drawn (color, depth, velocity, decal receiver, every sun cascade, so it casts
  no shadow), not collided (the physics heightfield's cells there are removed), not picked and
  reports no height. The cut is the same at every LOD and through the virtual texture: the
  coverage window test runs per pixel before any page lookup.
- **Height patches**: a rectangle of new height samples, queued and written at a bounded cost
  per frame, in a fixed order, into the drawn terrain, the CPU map and the messages physics and
  navigation read.

Methods: `terrain.set_hole`, `terrain.save_holes`, `terrain.patch_heights`, `terrain.get`. The older
`terrain.set_holes` and `terrain.load_holes` (see [tunnels](tunnels.md)) replace the whole
mask, named holes included; `terrain.set_hole` changes one named hole and leaves the rest alone.

## terrain.set_hole

```json
{ "id": "tunnel_3", "open": false,
  "segment": { "from": [-142.8, -286.0], "to": [-170.0, -256.0], "width_m": 20.0 } }
```

- `id` (required): 1 to 64 ASCII letters, digits, `_`, `-` or `.`.
- `shape` (`{rect|polygon|ellipse|raster: {...}}`, as in `terrain.set_holes`) or `segment`
  (`{ from, to, width_m }`): adds the hole, or replaces the one with this id. Open unless
  `open: false`.
- `open` alone: opens or closes an existing hole.
- `remove: true` (with only `id`): removes it.
- A hole a mesh layer fills (`TerrainMeshLayers`) keeps its footprint: a new `shape` or `segment`
  answers `hole_filled_by_layer`. Opening, closing and removing still succeed, and the mouth's
  open flag follows.

A closed hole cuts nothing but keeps its slot: a terrain holds at most 64 footprints, open or
closed, so opening a hole never fails mid-match. Named holes are kept sorted by id, so the GPU
windows, and so the frame, do not depend on the order holes were added or opened.

With a linked terrain draw (`TerrainSharedDraw::holes_handle()` inserted as the `TerrainHoles`
resource) the edit applies on the next frame: the reply says `applied_to: "draw"` and
`pending: true`. `wait_until` on `terrain.get`'s `holes.pending` being false only works when a
`Terrain` entity exists: without one, `terrain.get` returns `holes: null` and `probes: []` and
drops the draw's hole state, so there is no `pending` field to wait on. Without a draw
(a headless app) the first `Terrain`'s own mask changes at once (`applied_to: "component"`):
picks, `terrain.raycast` and colliders built from it see it immediately.

The reply has `before` and `after` (`{ id, open, shape }` or null).

Errors:

| Code | When |
| --- | --- |
| `hole_bad_id` | the id is empty, too long or has another character |
| `hole_unknown_id` | `open` or `remove` names no hole |
| `hole_too_many` | a new hole would pass 64 footprints |
| `hole_filled_by_layer` | a mesh layer fills this hole, so its footprint cannot change |
| `F0002` | a bad shape, or a bad combination of parameters |
| `F0003` | no linked draw and no `Terrain` in the world |

## terrain.save_holes

Holes are runtime state until something writes them. `terrain.save_holes` writes the mask in effect
(a linked draw's next mask, pending edits included, else the first terrain's) to a file under the
project root, atomically, and replaces what was there:

```json
{ "path": "maps/arena.level.ron" }
```

- `path` is a map or level file (`maps/arena.level.ron` writes `maps/arena.holes.ron`, the file
  `terrain.load_holes` and the bake read for that map) or a name ending in `.holes.ron`. It must stay
  under the project root: no `..`, no symlink leading out.
- Closed named holes are kept, with their state.
- Reply: `{ path, bytes, shapes, named }`; `shapes` counts the footprints that cut, `named` every
  named hole.
- Errors: `F0002` for a bad path, `F0003` when there is no terrain, `holes_write` when the file
  cannot be written.

Check it: `terrain.load_holes { path }` in a fresh session, then `terrain.get` with `probes` on a
point inside the hole.

## terrain.patch_heights

```json
{ "col": 512, "row": 640, "width": 32, "height": 16, "fill": 41.5 }
```

- `col`, `row`: the first sample (column 0 is the map's -X edge, row 0 its -Z edge). Patch
  rectangles are in the render map's **sample grid**, not in meters: sample `(col, row)` sits at
  `TerrainMap::sample_to_world(col, row)`, which includes the map's grid origin
  (`terrain.get` `map.grid_origin`, see [Grid origin](#grid-origin)). A host with a world-space
  box gets its samples from `PatchTarget::samples_within(min, max)`.
- `width`, `height`: the rectangle in samples (at most 2048 × 2048 per patch).
- exactly one of `heights` (meters, row-major, X fastest), `samples` (16-bit values, the
  simulation's own numbers), `fill` (one height for every sample) or `heights_mm` (absolute
  integer millimeters, the DO27 contract's `HeightEdit`; see below).
- `budget`: samples written per frame from now on (default 65 536). Alone, it only sets the
  budget.

Heights are clamped to the map's range and rounded to its 16-bit step, so the drawn ground,
the CPU map, a save and the simulation agree bit for bit. The reply has the patch id and the
queue (`queued_patches`, `queued_samples`, `budget`, `completed`, `last_frame`, ...). Wait until
`terrain.get`'s `patches.completed` reaches the id: patches finish in id order.

Each frame `TerrainPlugin`'s `apply_terrain_height_patches` takes whole rows from the oldest
patch until the budget is used (a patch larger than the budget spreads over several frames; a
single row wider than the budget goes alone). The order depends only on the queue and the
budget. Every band is:

1. written into the first `Terrain`'s map and min/max pyramid (and noted on its
   `TerrainEdits` when the editor tracks one);
2. handed to the linked draw (insert `TerrainSharedDraw::height_patches_handle()` as the
   `TerrainHeightPatches` resource), which on its next prepare updates its own map, pyramid,
   GPU height texture with its height and slope mips, re-selects its LOD nodes and invalidates
   its cached terrain shadows and virtual-texture pages. Only the samples whose heights change
   are redrawn: a band that writes the heights the map already holds redraws nothing and
   leaves the frame bit for bit as it was. The map's altitude range (what layer altitude bands
   test against) follows the edit, so after the virtual texture's feedback has rotated (about
   20 frames) the frame equals a cold load of the edited map, apart from the renderer's
   temporal GI history (ADR-2450);
3. announced as a `TerrainHeightsChanged` message (`rect`, `world_min`, `world_max` over the old
   and new heights). Physics updates its heightfield over the band with
   `CollisionHeightfield::patch` (holes cut as before) and swaps the collider shape; a navmesh
   rebuilds the tiles under the box. Split the terrain collider into chunks
   (`CollisionHeightfield::chunk`, one heightfield collider per `CollisionChunk` placed at its
   `center`) and rebuild only `chunks_touching(cells, &message.rect)`: swapping a collider's
   shape rebuilds that whole collider.

An idle queue costs nothing beyond one check per frame.

### The DO27 contract: `heights_mm` and `TerrainHeightEdits` (ADR-7585)

`heights_mm` sends what DO27's simulation sends (spec 25 § 7.4): absolute heights in integer
millimeters, row-major, row 0 at the map's -Z edge. They go through
`regia_terrain::TerrainHeightEdits` (the Rust handle a host uses, a thin adapter over the same
queue): each value becomes the map's 16-bit sample in `f64` (the same on every machine), at most
128 × 128 samples per edit and 16 edits between two applied frames (`queue.edits_this_frame`
counts them). A value outside the map's height range is refused, not clamped, so the drawn
ground never silently differs from the simulation's.

```json
{ "col": 300, "row": 412, "width": 3, "height": 1, "heights_mm": [12040, 11020, 12040] }
```

What an applied band invalidates, and nothing more (counters on `TerrainDrawStats`):

- the sun cascades whose box the band's world box lands in (`cascade_edit_invalidations`); each
  cascade tests the box against its selected nodes, each grown by its own reach (cell, morph
  and the height level its vertices read, ADR-7638), so an edit just outside a wide cascade
  that moves one of its coarse border vertices redraws it, and a one-sample edit far from the
  camera re-keys no cascade, so the renderer redraws none;
- the grass patch selection, only when the box meets the grass range around its eye
  (`grass_edit_reselects`); the blades themselves read the height texture every frame;
- the virtual-texture pages that meet the band's own rectangle, grown by the texture's height
  reach (`virtual_texture_edits`: `rects`, `pages`, `merges`);
- the scatter instances standing on the changed ground: `resit_runtime_scatter`
  (`ScatterRuntimePlugin`) hands each `TerrainHeightsChanged` to
  `ScatterField::heights_changed`, which visits only the tiles under the box and moves only the
  instances whose ground moved (`RuntimeScatter::last_resit`: `tiles_touched`, `tiles_rebuilt`,
  `instances_resat`, `tiles_resident`). The rest keep their matrices bit for bit. Tiles placed
  later stand on the edited ground, painted instances included. Placements copy only their
  tile's window of the heights, so an edit while tiles stream copies no height map
  (`heights_copied_bytes` is the map's size on the first edit only, while the host still shares
  the map the field was built from, and 0 after it; a host that calls
  `ScatterField::prepare_height_edits()` at load, before `RuntimeScatter::new`, takes that one
  copy there instead).

The engine's probe volumes still rebuild their whole heightfield pyramid on any edit (a
`regia_render` step; ADR-7585 lists the plan).

A host keeps its terrain collider in step with a system like this (chunks of 64 cells; one
static heightfield collider entity per chunk, spawned from `collider.chunk(64, [x, z])` at its
`center`):

```rust
# use regia::prelude::*;
# use regia_core::math::Vec3;
# use regia_physics::{Collider, ColliderShape};
# use regia_terrain::{CollisionHeightfield, Terrain, TerrainHeightsChanged, TerrainHoles};
# #[derive(Resource)]
# struct GroundCollider {
#     field: CollisionHeightfield,
#     entities: std::collections::BTreeMap<[u32; 2], Entity>,
# }
fn follow_terrain_patches(
    mut changed: MessageReader<TerrainHeightsChanged>,
    terrains: Query<&Terrain>,
    holes: Res<TerrainHoles>,
    mut ground: ResMut<GroundCollider>, // the host's CollisionHeightfield + chunk entities
    mut colliders: Query<&mut Collider>,
) {
    let mut dirty = std::collections::BTreeSet::new();
    for msg in changed.read() {
        let Some(terrain) = msg.entity.and_then(|e| terrains.get(e).ok()) else { continue };
        let mask = holes.applied_mask();
        ground.field.patch(&terrain.map, mask.as_deref(), &msg.rect);
        dirty.extend(ground.field.chunks_touching(64, &msg.rect));
    }
    for index in dirty {
        let (Some(chunk), Some(&entity)) = (ground.field.chunk(64, index), ground.entities.get(&index))
        else { continue };
        if let Ok(mut collider) = colliders.get_mut(entity) {
            collider.shape = ColliderShape::HeightField {
                nx: chunk.nx,
                nz: chunk.nz,
                heights: chunk.heights,
                scale: Vec3::from(chunk.scale),
            };
        }
    }
}
```

Errors:

| Code | When |
| --- | --- |
| `patch_empty` | `width` or `height` is 0 |
| `patch_out_of_bounds` | the rectangle leaves the map |
| `patch_bad_length` | `heights` or `samples` does not hold `width × height` values |
| `patch_non_finite` | a height is not finite |
| `patch_too_large` | more than 4 194 304 samples |
| `patch_queue_full` | 16 777 216 samples are already waiting |
| `patch_no_target` | the queue knows no map yet (no draw linked, no `Terrain` spawned) |
| `edit_too_large` | a `heights_mm` edit is wider or taller than 128 samples |
| `edit_frame_full` | 16 `heights_mm` edits were pushed since the last applied frame |
| `edit_out_of_range` | a `heights_mm` value is outside the map's height range |
| `F0003` | the app has no `TerrainHeightPatches` resource (no `TerrainPlugin`) |

When a hole opens or closes, re-cut the collider under it: `TerrainMap::sample_rect` of the
hole's `HoleShape::world_bounds()`, then `CollisionHeightfield::patch` with the new mask over that
rectangle and rebuild `chunks_touching` it (the draw's mask: `TerrainHoles::applied_mask()`; its
`state().generation` changes when the draw applied a new mask).

## terrain.get

```json
{ "at": [[0, 0], [12.5, -40]] }
```

The first terrain's `map` (`width`, `height` in samples, `world_width`, `grid_origin` (`[x, z]`
meters, the map's center; `[0, 0]` unless the host moved the grid), `sample_spacing`,
`height_scale`, `height_offset`, `min_height`, `max_height`), its `holes` (`count` of footprints
that cut, `named: [{ id, open }]`, `linked`, `generation`, `pending`), the `patches` queue and,
for up to 64 `at` points, `{ at, height, hole, inside }` (`height` is null inside a hole or off
the map). With no `Terrain` entity the reply is `terrains: 0`, `map: null`, `holes: null` and
`probes: []`, even when a linked hole draw or a height-patch queue exists (that is why this case
is not `F0003`; `terrain.set_hole` can still report `pending: true`). `F0003` when the world has
no `Terrain`, no linked draw and no patch queue.

`anisotropy_path` is how the terrain draw filters its layer, triplanar and macro textures
anisotropically on this GPU (ADR-3957): `"sampler"` (the hardware's anisotropic filter),
`"footprint"` (the shader takes up to 16 trilinear taps, the anisotropy level, along each pixel's long axis; chosen on
Mesa's Intel Vulkan driver, which applies no sampler anisotropy to the terrain's array textures),
or null before any terrain draw was created. `REGIA_TERRAIN_ANISO_FOOTPRINT=1` forces the footprint
path on any GPU, `=0` keeps the sampler path. To check that a GPU needs it, run `cargo test -p
regia_terrain --test terrain_aniso the_sampler_path_matches_the_driver -- --nocapture` on it:
it prints the mip level the driver picks for an 8:1 footprint with a 16x sampler (near 0 = the
hardware filters; near 3 = it does not) and fails when such a driver is left on the sampler path.

`anisotropy_lod_offset` is how many mips softer than each footprint's own level the sampler path
asks the hardware to filter (ADR-5459), null before any terrain draw was created. With an
anisotropic sampler some GPUs blend a footprint's two mips toward the sharper one, so without the
offset their far ground kept more detail than the look was tuned on and shimmered under TAAU. The
value is measured on the device when the first terrain draw of a process is created (ADR-5797: the
material sampler's calibration probe over a 2D array, `book/agents/material-sampler.md`): `0.25`
on Mesa's AMD Vulkan driver (RADV: the Steam Deck, Radeon cards on Linux) and on a Mali-G715
(Pixel 9 Pro), `0` on NVIDIA, Apple and lavapipe. A device the probe cannot run on (no compute
shaders) takes the listed value for its driver (`0.25` for RADV and Mali, `0` otherwise).
`REGIA_TERRAIN_ANISO_LOD_OFFSET=<mips>` (0 to 2) sets it on any GPU, `=0` turns it off. To
check a GPU, run `cargo test -p regia_terrain --test terrain_aniso -- --nocapture sampler_lod_offset`
on it: `the_device_sampler_lod_offset_keeps_the_sampler_path_at_trilinear` prints the level the 16x
sampler filters footprints between two mips at with the device's own offset, and
`the_sampler_lod_offset_matches_the_driver` does the same with the listed value; each fails when
the sampler path filters sharper than the exact trilinear level.

The offset costs nothing per texture tap: the terrain shader reads it once per pixel and adds it
where it sizes each projection's gradients (ADR-5943), so GPUs with offset `0` draw the same
pixels at the same speed as before the offset existed. To see the offset work through the terrain's
own shader on any GPU, run `cargo test -p regia_terrain --test terrain_aniso
the_sampler_lod_offset_softens_the_sampler_path -- --nocapture`: it prints the far ground's detail at
offsets 0, 0.25, 0.5 and 1 and fails when a larger offset does not filter softer, or when the
isotropic frame or the footprint path changes with it.

The terrain's colour pipelines compile the footprint loop only on a terrain whose `anisotropy_path` is
`"footprint"`, and the cloud layer's reads only in frames that bind a cloud map (ADR-7886): code the
frame cannot take still cost the pass. Both draw the same pixels. The warm builds the colour
pipelines with and without the cloud reads, so a cloud layer switched on or off mid-play compiles
nothing; switching the footprint path after the first frame (`REGIA_TERRAIN_ANISO_FOOTPRINT` is read
when the terrain draw is created) rebuilds the terrain's pipelines once. `cargo test -p regia_terrain
--test shared_dormant_variants` checks the variants' sources.

## Grid origin

A `TerrainMap` is vertex-centered: sample `i` of `n` sits at
`i / (n - 1) * world_width - world_width / 2`, plus the map's **grid origin** (ADR-8179). The
origin is `[0, 0]` by default, which is the mapping every earlier release drew, bit for bit. A
host whose simulation grid is **cell-spaced** (sample `i` of `n` at `(i - n / 2) * W / n`,
sample `n / 2` on the world origin, as DO27's simulation is) matches it with:

- `world_width' = W * (n - 1) / n`, so the spacing is `W / n`, and
- `TerrainMap::with_grid_origin([-0.5 * W / n; 2])`, so sample `n / 2` lands on 0.

Worked example, DO27's 1280 m map at 4096 samples: `world_width' = 1280 * 4095 / 4096 =
1279.6875` m, spacing 0.3125 m, grid origin `[-0.15625, -0.15625]`. The map then spans
`-640 ..= 639.6875` m on X and Z. Without the origin every drawn and collided sample sits
0.156 m toward +X and +Z of the simulation's.

Everything that maps world to grid follows the origin: `sample_bilinear` (bit for bit the
stored sample at each simulation sample position), `CollisionHeightfield` (place the collider
at its new `center`, `[origin_x, 0, origin_z]`; `CollisionChunk::center` already includes it),
holes cut at sample positions, `TerrainHeightPatches` (`PatchTarget::grid_origin`,
`footprint`, `samples_within`), the CDLOD selection and its hole culling, `regia_pick`'s
terrain ray, and the GPU draw. The origin must be finite and the same on X and Z
(otherwise `with_grid_origin` returns `TerrainMapError::BadGridOrigin`): a cell-spaced grid's
half-sample shift is the same on both axes, so the origin stays equal on X and Z. On a square
map the draw folds that origin into `half_width` and the depth lane holds the same numbers; a
rectangular map ([World depth](#world-depth)) keeps the equal origin and stores the Z extent in
the depth lane.

The origin is not saved with the map (no frozen format changes): the host sets it after loading,
before it builds the collider or links a draw.

**Coarse grids.** A simulation that computes a crater on a coarser cell-spaced grid over the same
world square (DO27: 256 samples, 5 m) maps it onto the render grid with
`regia_terrain::resample_cell_grid_mm(coarse_mm, coarse_side, render_side, &rect)`, in integer
millimeters: a render sample that coincides with a coarse sample copies it (render sample `16 *
i` is coarse sample `i` on DO27's grids), the others are the bilinear of the coarse grid at the
render sample's position (integer weights, rounded to the nearest millimeter, halves up), and the
grid clamps past its last sample. Divide by 1000 for the `heights` of `terrain.patch_heights`.

## Checking a tunnel mouth

1. `terrain.set_hole { id: "mouth", shape: {...}, open: false }`, then
   `seeing.screenshot`: the ground is closed.
2. `terrain.set_hole { id: "mouth", open: true }`, `wait_until` `holes.pending` is false, then
   `seeing.screenshot` and `terrain.raycast` straight down into the mouth: `hit: null`.
3. `terrain.get { at: [[x, z]] }` inside the mouth: `hole: true`, `height: null`.

## Format

Named holes are stored in `<map>.holes.ron` format version 2 (`docs/formats/terrain.md` § 5):

```ron
(
    version: 2,
    shapes: [],
    named: [
        (id: "tunnel_3", open: false, shape: rect(center: (40.0, 2.0), half_extents: (10.0, 12.5), rotation_radians: 0.7)),
    ],
)
```

Version 1 files load unchanged (no named holes); `regia upgrade-project --write` re-saves them
at version 2.

## World depth

A `TerrainMap` is square unless the host calls `TerrainMap::with_world_depth(metres)` (ADR-8369).
The call sets the world extent along Z apart from `world_width`. The sample grid stays
`width × height`: a 4100 × 1540 heightfield is that many samples, not padded out to a square.
With the same spacing on both axes, `world_depth = world_width * (height - 1) / (width - 1)`.
Any positive finite depth is accepted; `sample_spacing_z` is then `world_depth / (height - 1)`
and can differ from `sample_spacing`.

Sample `i` of `n` on Z sits at `i / (n - 1) * world_depth - world_depth / 2`, plus the grid
origin. The collider scale is `[world_width, 1, world_depth]`. `height_at` is `None` outside
that rectangle. Runtime patch footprints use the Z extent. The CDLOD quadtree stays a square of
the longer side, centred on the grid origin; nodes wholly past the shorter side are not drawn,
and vertices of a node that straddles that side sit on the map edge.

`terrain.get` reports `map.world_depth` and `map.sample_spacing_z`. There is no new method.
A default map reports `world_depth == world_width` and `sample_spacing_z == sample_spacing`.

The depth is not saved with the map (no frozen format change). The host sets it after loading,
before it builds the collider or a draw. The v0.6 format batch should add an optional
`world_depth_m` next to `world_width_m`.

Not routed, so they still treat the map as square or read only `world_width`: the GI heightfield
(it returns nothing when the sample counts differ), grass placement, scatter, forest gaps, the
water-body plane, `SimGround`, stream tiles, and the material macro tiling. The terrain and
water height samples do follow the depth. GPU normals on a map whose two spacings differ still
step by the X spacing; the CPU normal is per axis. A square map is unchanged, bit for bit.

### Height patches with edit layers

When the target terrain has edit layers, each applied patch band is absorbed into the base
before the composite is recomputed (ADR-8202). The active layer does not capture runtime
patches. Existing Add layers retain their contribution when hidden and shown again; Set
layers can cover the base change. The linked draw receives the resulting composite heights,
and subsequent layer operations and saves retain the patch. Use `terrain.stroke` with
`edit_layer` for a named-layer authoring stroke; `terrain.patch_heights` keeps its existing
parameters and refusal codes.

## Sample layout

A `TerrainMap` is **vertex-centered** by default: sample `i` of `n` sits at
`i / (n - 1) * W - W / 2`, so the first and last samples lie on the map's edges. A heightfield
whose samples are the centers of `n` equal cells (a cell-registered raster, where each pixel is
an area, or a cell-spaced simulation grid) is **cell-centered**: sample `i` sits at
`(i + 0.5) * W / n - W / 2`, spacing `W / n` (ADR-10079).

- Host API: `TerrainMap::with_sample_layout(SampleLayout::CellCentered)` reads the map's width
  (and depth) as the cells' extent. The map then stores the span between its outer samples,
  `W * (n - 1) / n`, as `world_width`, so the collider, the draw, holes and patches cover the
  samples' span; a query in the outer half cell clamps to the edge sample. `cell_extent()`
  returns the cells' extent and `sample_layout()` the layout. Setting the layout a map already
  has changes nothing. `with_world_depth` after it takes the cells' extent too.
- Level file (scene format v14): `LevelSettings::terrain` has `sample_layout: VertexCentered`
  (the default) or `CellCentered`. `world_width_m` is then the cells' extent. With no width, a
  cell-centered terrain takes the cooked index's `size_m` times `n / (n - 1)`, else one metre a
  cell. The level loader and the mesh-layer cook both place the samples by the layout. A
  terrain built from a cooked count mapping refuses `CellCentered` with `level_terrain_mapping`
  (the mapping places the samples).
- A v13 level migrates to v14 with `sample_layout: VertexCentered`: its samples sit where they
  did. `regia upgrade-project --write` re-saves it.

A simulation grid with sample `n / 2` at the world origin (`(i - n / 2) * W / n`) is a
cell-centered map moved by a grid origin of `-0.5 * W / n` on X and Z
(`TerrainMap::with_grid_origin`); that stores the same numbers as the ADR-8179 recipe.

Agent surface (no new method):

- `terrain.get` reports `map.sample_layout` (`VertexCentered` or `CellCentered`) and
  `map.cell_extent` (`[x, z]`, metres). A cell-centered map's `map.world_width` is the span
  between its outer samples.
- `terrain.set_level_terrain` accepts `sample_layout` in its `terrain` object, and
  `terrain.level_terrain` reports it.
- `editor.terrain_create` takes `sample_layout` (`VertexCentered` default, or `CellCentered`:
  the terrain is `size * cell_size` wide). A bad value is `terrain_create_bad_params`. The New
  terrain dialog has a "Samples at cell centers" box, and World Settings shows the field as
  Sample Layout.
