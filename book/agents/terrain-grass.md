# Terrain grass (dense ground cover)

A map's grass comes from its `.grass.ron` (format v1, `docs/formats/grass-ron.md`, ADR-1400):
classes of grass, each with density rules (the terrain layers it grows on, slope and altitude
bands, a density map, patchiness noise, exclusion paths: the scatter vocabulary), a blade shape
(blades per clump, height, width, lean, bend), stem and green-shoot colours, a range, shadows
and sway. The host bakes a mask from the map once (`GrassMask::build`), binds a `GrassLayer` to
the terrain draw, and the GPU grows clumps of real tapered blades around the camera every frame:
no alpha-tested cards. Two renders of the same frame are byte-identical.

Presets: `GrassClass::winter_dead()` (matted straw-coloured stems 25–55 cm, about one blade in
eight a short green shoot, still air) and `GrassClass::summer()` (upright green blades with dry
tips, a light sway).

## Check what is on screen

`terrain.grass` returns the request in effect and the last frame's report:

```json
{ "request": { "enabled": true, "quality": null, "density_scale": 1.0, "range_scale": 1.0,
               "shadows": true, "sway_scale": 1.0, "debug_view": 0 },
  "report": { "installed": true, "drawing": true, "quality": "high", "quality_auto": true,
              "classes": ["dead_grass"], "patches": 412, "patches_per_level": [180, 96, 70, 48, 18],
              "camera_patches": 260, "shadow_patches": 41, "candidates": 412000,
              "max_instances": 393216, "range_m": 250.0, "shadow_m": 25.0, "shadow_cascades": 1,
              "mask_side": 2048, "mask_generation": 1, "time_s": 0.0, "frames": 120,
              "shadow_sway_cascades": 0, "dynamic_boxes": 0,
              "surface_weather": false, "weather_frames": 0, "subsurface": false } }
```

`shadow_sway_cascades` and `dynamic_boxes` are the grass on the dynamic-caster shadow class
(`book/agents/foliage-wind.md`): once the clock runs, the cascades whose grass shadow sways and
the boxes it reported. `quality: null` in the request means the tier follows the render profile (`quality_auto` in the
report). `candidates` is what the GPU walks; the drawn clumps are the ones the density law and
the mask keep. `installed: false` means no frame has been prepared with the layer yet.

### Wet and snowy blades

On a wet, snowy or rainy frame (`render.set_surface_weather`, `render.set_weather`; see
[Rain, snow, wet surfaces](weather-precipitation.md)) the colour pass draws the blades with a
surface-weather variant (spec 99 § 6.4, grass stage 3, ADR-3249):

- wetness darkens and glosses a blade by the response of the ground layer under it: the albedo
  takes the ground's factor `1 - 0.5 φ w` (φ the layer's porosity, `w` the wetness under open
  sky), and the film lowers the blade's roughness as it does the ground's. A sealed layer
  (`weather: Some((porosity: Some(0.0)))`) glosses its blades without darkening them;
- snow whitens the upper third of each blade by `0.7 σ` (σ the snow cover times the layer's
  `snow` amount), on ground flat enough to hold snow. A layer with `snow: 0.0` keeps green blades;
- the rain's extinction reaches the blades as it reaches the ground;
- under a roof (the occlusion view) blades stay dry, like the ground under them.

`report.surface_weather` is true on the frames drawn with the variant, and `weather_frames` counts
them. A dry frame keeps the dry pipeline and its bytes, and compiles nothing. The variant is built on
the first wet frame, or before it by the weather warm (`effects.surface` in `config/weather.ron`, or
`set_warm_surface_weather`). The blades still sway in the wind: only the colour changes.

## Tiers

| Tier | Profile | Range | Full density to | Shadows | Notes |
|---|---|---|---|---|---|
| `ultra` | Desktop Ultra, offline renders | 600 m (each class caps it; the presets 300 m) | 40 m | 40 m, 2 cascades | 16 blades, 4 segments near |
| `high` | Desktop High | 250 m | 25 m | 25 m, 1 cascade | |
| `medium` | — | 150 m | 16 m | none | |
| `low` | Desktop Low, Steam Deck | 70 m | 10 m | none | 5 blades, 3 segments |
| `minimal` | Mobile Low and High | 30 m | 6 m | none | the cheap fallback: two single-segment blades per clump |
| `off` | — | — | — | — | nothing drawn |

Past the full-density distance the clump density falls as `(full / d)²` (the clumps per screen
area stay constant), times a linear fade to 0 over the last 15 % of the range and the blades widen to keep the cover; past the range the terrain's own
grass layer carries the look. Clumps shrink into the ground as they thin out, so nothing pops,
and patch edges never show.

## Toggle and compare

`terrain.set_grass` takes any of `enabled`, `quality` (a tier or `"auto"`), `density_scale`
(0..1), `range_scale` (0.1..4), `shadows`, `sway_scale` (0..4; 0 is still air), `debug_view`
(0 shaded, 1 geometry LOD: near red, mid green, far blue; 2 patch level; 3 fade; 4 the foliage
transmission through each blade toward the sun, raw linear RGB, see below) and
`clear: { at: [x, z], radius, factor }` (scales the density in a disc; factor 0, the default,
clears it: a burnt patch, a crater). `clear.at` must be two finite numbers that fit in `f32`;
otherwise the call is `F0002` and nothing in that call is applied. It returns `{ before, after }` and applies on the next frame.

1. `seeing.screenshot` (target `"offscreen"`) → `on.png`
2. `terrain.set_grass { "enabled": false }`, `play.step`, `seeing.screenshot` → `off.png`
3. `seeing.compare_images` on the two; set `enabled` back to `true`.

`{ "enabled": false }` and the `off` tier render the frame byte-identical to a terrain without
grass. A/B the shadows the same way with `{ "shadows": false }`, the fallback with
`{ "quality": "minimal" }`.

## Light through the blades: the foliage profile

With the render profile's subsurface switch on (`render.set_subsurface { "enabled": true }`, off in
every preset; ADR-3170) the light the sun sends through a blade is the subsurface foliage profile's
(`book/agents/subsurface.md`) instead of the grass's own back-lit term: the profile's two Gaussians
integrated over a plane as deep as the blade along the sun ray (0.3 mm over `|n · l|`, at most ten
times that), times the class's translucency and its colour. Green gets through furthest; light
grazing a blade gets through less than light crossing it. The report's `subsurface` says whether
the last frame used it. Off, the grass renders byte-identical to a build without it.
`debug_view: 4` shows the transmission itself, raw (not lit, not fogged).

## Errors

| Code | When |
|---|---|
| `F0003` | No `TerrainGrassHandle` resource, or it is not linked to a grass layer (insert `GrassLayer::handle()` or `TerrainSharedDraw::grass_handle()`). |
| `F0002` | An unknown parameter, a tier name that does not exist, a value of the wrong type, `debug_view` above 4, a `clear` without `at: [x, z]` or with a radius outside (0, 10000] m or a factor outside 0..1. |

## Host contract

```rust,wrap
# use regia::prelude::*;
# use regia_render::RenderDevice;
# use regia_terrain::{
#     DensityMaps, GrassLayer, GrassMask, GrassMaskInputs, GrassSet, HolesMask, LayerWeightMap,
#     TerrainLayers, TerrainMap, TerrainSharedDraw,
# };
# let app: &mut App = todo!();
# let device: &RenderDevice = todo!();
# let draw: &mut TerrainSharedDraw = todo!();
# let (text, map, layers): (String, TerrainMap, TerrainLayers) = todo!();
# let (weights, maps, holes): (LayerWeightMap, DensityMaps, HolesMask) = todo!();
# let sim_seconds: f32 = 0.0;
let set = GrassSet::from_ron(&text, Some(&layers))?;
let mask = GrassMask::build(&set, &GrassMaskInputs { layers: Some(&layers), painted: Some(&weights),
    density_maps: Some(&maps), holes: Some(&holes), texel_m: 2.0, ..GrassMaskInputs::new(&map) })?;
let layer = GrassLayer::new(&device.device, &device.queue, set, mask)?;
let handle = layer.handle();
draw.set_grass(Some(layer));          // before boxing the terrain draw
app.insert_resource(handle.clone());  // terrain.grass / terrain.set_grass
// every frame (fixed step): the sway clock
handle.set_time(sim_seconds);
```

The tier follows the frame's render profile unless the host or the agent picks one
(`GrassRequest::quality`). `draw.prewarm(format)` builds the pipelines at load time. Grass
grows around the main camera only (a render-to-texture camera sees none). The mask is 2048²
texels at most per class on a 4 km map (2 m texels); `TerrainGrassHandle::scale_density` edits
it at run time (the layer re-uploads it).

## What it costs

`cargo run --release -p regia_terrain --example grass_cost` renders a 4 km tile
(4097² samples) with the winter set at full density, grass off and per tier, and prints the
shared forward, prepass, shadow and frame GPU times (`GRASS_COST_SIZE`, `GRASS_COST_PROFILE`,
`GRASS_COST_TIERS`, `GRASS_COST_EYE`). The measured numbers are in the lane report and
`docs/decisions.md` ADR-1400.

## Limits

- Blades sway in the colour, depth and velocity passes. While the sway clock is still, the sun
  cascades get the rest pose, so a retained cascade stays valid. Once the host moves the clock
  (`set_time`), the cascades the dynamic-caster class names draw the swayed blade every frame
  (`vs_grass_shadow_sway`) and report dynamic boxes (spec 88 § 2.4).
- Nothing is drawn into the decal receiver: a scorch decal over grass darkens the blades.
- Clumps root on the base heightfield, sunk a little with distance (up to 0.2 m) where the
  terrain's distant LOD can sit off it.
- Grass does not bend under characters or vehicles yet; `scale_density` is the tool for
  trampled or burnt ground.
