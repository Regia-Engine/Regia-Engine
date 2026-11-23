# Streamed forests: scatter on the ECS frame

A map's scatter set can hold whole forests (the showcase map places about 190,000 trees over 8 km). The
engine draws them through the streamed scatter field (`regia_terrain::ScatterField`), not one entity per
tree: instances are placed in 64 m tiles around the camera (deterministic per cell, so a tile placed again is
identical), whole tiles and sub-blocks are culled on the CPU, every instance picks its level of detail with
hysteresis and a dithered cross-fade, fades out at its class's `fade_start..fade_end`, and the renderer draws
one instanced batch per mesh, LOD and material, culled per instance on the GPU (ADR-1415).

## What a host does

```rust,wrap
# use regia::prelude::*;
# use regia_terrain::{ScatterLibrary, ScatterSet, ScatterSource};
# let app: &mut App = todo!();
# let (set, source, library): (ScatterSet, ScatterSource, ScatterLibrary) = todo!();
# let key_base: u64 = 0;
use regia_terrain::{RuntimeScatter, ScatterBuildOptions, ScatterField, ScatterRuntimePlugin};

app.add_plugins(ScatterRuntimePlugin);
let field = ScatterField::new_with(&set, source, &library, key_base,
    ScatterBuildOptions { shared_materials: true })?;
app.insert_resource(RuntimeScatter::new(field));
```

`ScatterRuntimePlugin` adds `extract_runtime_scatter` to `Render`, after the frame's own meshes are extracted
and LOD-selected: it streams the field around the primary view's eye (one `ScatterField::update` per frame,
the cross-fade clock), appends its draws to `ExtractedFrame::meshes` (`ScatterField::extract_meshes`, sharing
the field's `Arc` meshes and materials so the renderer's caches see the same keys every frame), applies the
agent's `ScatterControl` and records the frame into it. Use `shared_materials: true` for textured trees: each
texture is then resident once per mesh and part instead of once per class and tint variant. Seat the field on
the drawn terrain (`field.set_rendered_surface(Some(draw.surface()))`) so far trees on a coarse terrain level
neither float nor sink.

For the forest look and repeatable renders, set the renderer's switches (ADR-1415; the order-fixed cull is the default since ADR-4777):

| Switch | What it does |
|---|---|
| `FrameRenderer::set_ordered_gpu_cull` | On by default (ADR-4777): the GPU cull keeps each instanced batch's instances in submission order (a culled instance keeps its slot and draws nothing), so two runs draw the same instances in the same order and temporal AA gives the same frames. `set_ordered_gpu_cull(false)` opts into the unordered atomicAdd pack for speed, which two renders of one sequence do not survive (ADR-1402). |
| `FrameRenderer::set_taa_hashed_alpha(true)` | Masked materials (twig cards) take the hashed alpha test on temporal-AA frames too (before: TSR frames only). Where the twig texture is minified, a pixel survives with probability alpha and TAA averages that into coverage, instead of a hard cutoff that shimmers and thins out far canopies. The threshold is a function of the pixel and the frame index (no clock). |
| `FrameRenderer::set_alpha_tested_shadows(true)` | Masked casters on the GPU-driven instanced shadow path are alpha tested against their base-colour texture (hard cutoff, stable from frame to frame), so twig cards cast leaf shadows, not rectangles. |

## Tree assets

A tree is one `ScatterMeshAsset` with material parts:

- `lods` and `material`: the main part (bark). LOD 0 must hold triangles; a coarser LOD may be empty.
- `parts`: more materials drawn with every instance at the same LOD, fade and transform (`ScatterMeshPart`, one
  entry per LOD, empty where the part draws nothing). A Tincal tree has twig cards at LOD 0 to 2 and a billboard
  part at LOD 3 only, so bark and twigs end where the billboard begins (cross-faded).
- `shadow_lod`: while a finer LOD is drawn, the shadow comes from this LOD as a shadow-only draw (glTF extras
  `shadow_lod`). Far trees cast from what they draw, billboards included.

The showcase builds these from glTF files: `_LOD<n>` nodes are levels (a billboard node that names its level is
kept), `extras.lod_screen_size` sets the switch sizes and `extras.shadow_lod` the shadow LOD.

## Check what is on screen

`terrain.scatter` reports the field the host recorded (`book/agents/terrain-scatter.md`): `resident`
instances in streamed tiles, `emitted` instances handed to the frame, `per_lod` (LOD 3 is the billboard for
Tincal trees), `fading`, and per-class `resident` counts. `terrain.set_scatter` toggles the field or a class
(`{"class": "woodland_trees", "class_enabled": false}`); the next frame shows the change.

```json
{"method": "terrain.scatter", "params": {}}
```

A forest check:

1. `terrain.scatter`: `report.installed` is `true`, `report.per_lod[3]` is non-zero when trees are far out,
   `report.emitted` is far below `report.resident` (the CPU tile cull).
2. `seeing.screenshot` with the camera low in a wood, then `terrain.set_scatter {"class": "woodland_trees",
   "class_enabled": false}` and a second screenshot: `seeing.compare_images` shows the woodland only.
3. Two `render.out_start` jobs of the same camera path give the same `sequence_hash` with the switches above on.

## Errors

The field has no agent errors of its own: `terrain.set_scatter` answers an unknown class with `not_found`
(listing the classes) once the field has been recorded, as before. Building a field fails with `ScatterError::Asset` when the set names a mesh or
density map the host did not supply, or when a part's LOD count differs from its mesh's (the showcase reports
it as `scatter_malformed`).

## Lower-detail leaf shadows at distance

Full leaf cards in the far cascades are most of a forest's shadow cost. A class can cast less from a drawn LOD
on (off by default, ADR-3146):

```json
{"method": "terrain.set_scatter", "params": {"class": "woodland_trees",
  "class_caster": {"from_lod": 1, "leaf_keep_one_in": 4}}}
```

- `from_lod`: the hint applies to instances drawn at this LOD or coarser. A far instance is a far caster, so near
  shadows keep their detail and the far cascades lose triangles. Missing means 0.
- `max_lod`, `min_lod`: the clamp on the LOD those instances cast from, as in `shadow_lod` (`book/agents/terrain-scatter.md`).
  `{"from_lod": 1, "min_lod": 3}` casts far trees from the billboard of a Tincal chain.
- `leaf_keep_one_in`: of those instances' leaf cards, keep one in this many (1 to 16). Each kept card grows to the area
  of its group, so the crown's shadow keeps its coverage, and moves toward the crown's centre, so the crown's
  outline does not swell. The kept card is the one whose orientation best keeps the crown's shadow area along 13 fixed
  directions, so no sun angle loses its shadow. A part keeps at least 64 cards: a sparse crown thins less than asked,
  and one with fewer than 128 cards (a billboard, a sapling, one welded mesh) casts whole. Measured: within 6 % of
  the whole crown's shadow coverage for crowns of 150 to 2500 cards at one in four, three sun elevations. The thinned
  meshes are built when a class first asks, once per LOD.
- `"class_caster": null` clears it. The next frame is byte-identical to one that never set it. The field-wide
  `shadow_lod` still applies first; the class hint narrows what it gave.

A shadow moved this way is a shadow-only draw; the drawn mesh does not change. `terrain.scatter` reports
`report.caster_triangles` (triangles emitted into shadow maps last frame, each draw counted once) and each class's
`caster`.

Rust: `ScatterField::set_class_caster_hint`, `ScatterCasterHint`, `ScatterControl::set_class_caster`.
`caster_coverage` measures what a caster covers from a sun direction, `reduce_cards` is the thinning itself.

Measured on the test fixture (a tree of 1200, 600 and 300 cards at LOD 0 to 2): `leaf_keep_one_in: 4` from LOD 1
brings far-cascade triangles down by at least 3x, with near casters unchanged and each tree's coverage within 8 % of
the whole tree's. The field-wide hint alone (`shadow_lod: {max_lod: 3, min_lod: 3}`) also thins the far cascades, and
takes the near shadows with it.

## Seasons on scatter classes

`SeasonalFoliage` (`book/agents/season.md`) drives mesh entities. A scatter class has no per-entity material, so it
carries its own season (off by default, ADR-3146):

```json
{"method": "terrain.set_scatter", "params": {"class": "woodland_trees",
  "class_season": {"kind": "broadleaf"}}}
```

- `{ "kind": "broadleaf" | "evergreen" | "grass", "year_fraction"?: 0.5 }`: the look of that kind at that time of year,
  by default the scene's (`scene.season`). Applied once: send it again as the year moves.
- `{ "density": 0.4, "tint"?: [2.2, 0.95, 0.38] }`: the values themselves.
- `null` clears it. The next frame is byte-identical to one that never set it.

Which instances keep their leaves is `regia_framework::season::leaf_present`, the same fixed rank per instance against
the density: the same trees in every run, and a tree in leaf at a density is in leaf at every higher one. Leaf parts
are a mesh's alpha-masked materials; a mesh with none is all leaves. They take the tint; the bark does not.

- A leafless instance leaves its leaf parts out of the frame and the shadows and keeps the rest. A far LOD made only
  of a masked billboard draws nothing while leafless.
- A season change rewrites no instance. The mesh path decides in the loop that builds the frame's draws, and the
  impostor far field compares the rank each instance holds on the GPU with its prototype's density in the cull shader.
- The first tint on a class copies its leaf materials once, which uploads their textures once and takes one more copy
  of their bytes. Each later tint rewrites their parameters under the same key. While a frame in flight holds the copies
  a tint waits for the next `ScatterControl::apply`.
- Impostors: **set the season before `bake_impostors`**. A class with a season at bake time gets a bare card (its LOD 0
  without the leaf parts), about one more card of atlas per prototype, and a leafless far tree draws it. A season set
  later still thins the far field, but a leafless instance then draws no card.

`terrain.scatter` reports each class's `season` and `report.leafless`, the emitted instances without their leaves.

Rust: `ScatterField::set_class_season`, `ScatterSeason`, `ScatterControl::set_class_season`, `regia_terrain::leaf_present`.

## Impostor cards of trees

`ScatterField::bake_impostors` captures every material part of a mesh's LOD 0 and cuts the holes of its masked ones, so
a tree's card holds its crown. (Before ADR-3146 a card held part 0 only and no holes.)

Across the hand-over band a moving view takes the lattice-free threshold of the hashed alpha test on both the mesh and
the impostor, and a held view keeps interleaved gradient noise, so a held sequence renders what it rendered before
(ADR-1489, ADR-3146).

## Wind

Branch sway from the Tincal `_WIND` / `_WIND_PIVOT` attributes (`tincal.wind/1`) is not drawn yet: the trees
render static and correct. See the lane report for the planned vertex-stage design.
