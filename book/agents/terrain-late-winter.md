# Terrain late-winter look

`TerrainGroundLook` (ADR-1408, `regia_terrain::ground_look`) adds to the photoreal terrain what a
tiling layer set cannot carry on its own. It is off by default (`TerrainGroundLook::OFF`).

- **Variation** (`variation`, `variation_scale_m`). Without a baked macro map, nothing varies
  between the texture repeat (1–3 m) and the layer bands, so a field reads as one carpet even when
  the stochastic lattice hides the repeat. Three octaves of world noise move the brightness and a
  dry/damp tint.
- **Remix** (`remix`, `remix_scale`). A second tap of each layer at a different scale and
  rotation, mixed in by a 5 m noise mask near the camera (inside the detail fade). Neighbouring
  repeats then differ in their mid-scale structure. It costs one more albedo tap per layer.
- **Thin patchy snow** (`snow`, `snow_line_m`, `snow_aspect`, `snow_patch_m`, `melt_edge`). The
  snow keeps to slopes facing away from the sun and to ground above the snow line, and steep faces
  shed it. Noise breaks it into patches. Thin snow lets the ground show at a patch's edge, the
  edge is slushy and greyer, and a darker wet fringe of melt water surrounds it. Old spring snow
  has an albedo of about 0.7 (Warren 1982).
- **Wet mud** (`mud_wetness`). Dark, soil-coloured flat ground in noise patches gets darker and
  glossier through the terrain's wetness path (Lekner & Dorf 1988).
- **Hoarfrost** (`hoarfrost`). A pale, faintly blue film appears where the geometric normal's Y
  goes from 0.6 to 0.95 and the sun does not reach. Steep faces stay bare. Shadowed ground still
  roughens toward roughness 0.95, including faces the film does not cover.

Presets: `off`, `varied` (variation and remix only), `late_winter` (everything).

Ground contact under scatter rocks comes from the scatter contact map (`scatter/contact.rs`,
`contact_ao` per scatter class). A host that places its own scatter entities builds that map
from a footprint list with `ContactRaster::from_footprints` and installs it with
`TerrainSharedDraw::set_contact_map`.

## Host

```rust,wrap
# use regia::prelude::*;
# use regia_render::RenderDevice;
# use regia_terrain::{CdlodConfig, TerrainLayers, TerrainMap, TerrainSharedDraw};
# let world: &mut World = todo!();
# let device: RenderDevice = todo!();
# let (map, layers): (TerrainMap, TerrainLayers) = todo!();
let mut draw = TerrainSharedDraw::new(&device, map, &layers, CdlodConfig::default(), 0)?;
draw.set_ground_look(regia_terrain::TerrainGroundLook::LATE_WINTER);
world.insert_resource(regia_terrain::TerrainGroundLookControl(draw.ground_look_handle()));
```

The handle is shared: the draw applies a new value at its next prepare, after it has gone into
the renderer.

### Cost when off

While every term is off, the shared terrain pass is compiled without the look's code (ADR-1452:
`regia_terrain::gpu::without_ground_look` removes the lines `terrain.wgsl` marks), so a terrain
that never turns it on pays nothing for it. The first switch on builds the shader with the look
and its colour pipeline (plus the hole-cut one when a hole is cut) on the next frame, and
rebuilds the virtual texture when it is on; set the look before the prewarm to pay that at load
time. Switching off, and on again later, compiles nothing. `TerrainGpu::shared_pipelines_ground_look`
says which variant is drawing.

## Agent

- `terrain.get_ground_look` returns `{ look: { on, variation, variation_scale_m, remix,
  remix_scale, snow, snow_line_m, snow_aspect, melt_edge, snow_patch_m, mud_wetness, hoarfrost },
  generation, presets, layer_albedo, fill_canyon_scope, fill_canyon_scopes }`.
- `terrain.set_ground_look { preset?, <any field>?, fill_canyon_scope? }` applies a preset and/or
  fields over the look in effect (clamped) and returns `{ before, after, generation,
  fill_canyon_scope }`.
- Errors: `F0003` when the host inserted no `TerrainGroundLookControl`; `F0002` for an unknown
  preset, an unknown `fill_canyon_scope` or a non-number.

### Planet-shine fill scope

`fill_canyon_scope` (ADR-8412) is the draw's `FillCanyonScope`, which a host sets with
`TerrainSharedDraw::set_fill_canyon_scope`. It is stored in the same handle, so it is reported and
set here. It decides where the sky body's fill on the ground takes the canyon-scale occlusion
(`min(macro AO, SSAO)`) instead of the full indirect stack:

| Value | Where |
|---|---|
| `off` (default) | nowhere: the full stack everywhere |
| `map_wide` | every fill-lit pixel (the old `set_fill_canyon_visibility(true)`); trench and canyon floors lift too |
| `near_occluders` | the sun shade of the frame's mesh occluders (hull boxes), weighted by `regia_terrain::near_occluder_shade`; the full stack elsewhere |

Check it on a frame: `terrain.set_ground_look { "fill_canyon_scope": "near_occluders" }`. Then
compare a screenshot with an `off` one taken at the same frame index. The ground beside a hull,
on its shadow side, gets brighter. A trench floor away from the hulls keeps its bytes
(`crates/regia_terrain/tests/fill_canyon_scope.rs`).

The legacy (no material) terrain path ignores the look.
