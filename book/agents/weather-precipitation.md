# Rain, snow, wet surfaces and the wind near the ground

The [weather](weather.md) falls, wets and blows (spec 99, ADR-1847): rain and snow drawn around the
camera, surfaces that get wet and collect snow, and a wind near the ground that foliage, grass, rain
and snow read.

All three effects are **off unless the project turns them on**, in `config/weather.ron`
([format](../../docs/formats/weather-ron.md)). A host that keeps a `Weather` for its clouds renders the
same frames, including a map that sits in `overcast`. `regia new` writes the file with all three on,
and an agent can turn each on for a run with the `render.set_*` methods below. Turned on, they do
nothing until a `Weather` has precipitation or wind.

## What each preset does

| Preset | `precipitation` | Temperature | What falls | What you see |
|---|---|---|---|---|
| `overcast` | 0.2 | 8 °C | 2 mm/h rain | drizzle: surfaces wet, no puddles |
| `rain` | 0.35 | 9 °C | 6 mm/h rain | moderate rain, full puddles in about 85 minutes |
| `storm` | 1.0 | 10 °C | 50 mm/h rain | heavy rain, 3 km visibility, puddles in minutes |
| `snow` | 0.5 | -3 °C | 1.2 cm/h of snow | steady snow, the ground covered in about 2.5 hours |
| `blizzard` | 1.0 | -8 °C | 5 cm/h of snow | 400 m visibility, the ground covered in about 35 minutes |

Rates are `max_rate · precipitation²`. The rain preset's 6 mm/h fills puddles in about 85 minutes
from dry (near a third full after half an hour). `precipitation_kind` is `auto` by default: all snow at or
below 0 °C, all rain at or above 2.5 °C, a smooth band between. `rain`, `snow` and `sleet` force it.
During a transition the rates of the two ends blend, so a change of kind never jumps.

`render.get_weather` answers the rates in `rates` (`visibility_m` is null when nothing falls) and the
state of the config file in `config`.

## The scene wind

`SceneWind` is the wind 10 m above the ground. In `weather` mode the weather writes it every frame:
the wind at cloud height through a height profile (about 0.49 of it under a 1500 m layer), plus
deterministic gusts on the weather clock, of strength `gustiness`. The gusts stay within
`1 ± gustiness` of the mean and never reverse it, and a paused weather holds it.

| Reader | What it does with it |
|---|---|
| The renderer | the foliage wind, every frame; `off` clears it |
| Grass | the terrain wind (`TerrainWind`): heading and strength (speed / 5 m/s) of the mean, `terrain.wind` shows `weather_steered: true`; see [Terrain wind](terrain-wind.md) |
| Rain and snow | the slant and the drift: the drops carry `velocity_m_per_s` (gusts included); `off` stills them |
| The occlusion view | its tilt, from the mean fall direction |
| Surface weather | faster drying, from the mean speed |
| VFX emitters with `wind_response` | the particle wind (ADR-2642); without a `SceneWind` the air is calm |
| The sea (`SeaWeatherPlugin`) | the swell and ocean wind, from the mean |
| The ambience bed | the wind layer's level |

Not yet following it (spec 99 § 3.3 lists them as follow-ups): cloth (`cloth.set_wind` /
`ClothSet::set_global_wind` stays the host's), the fluid and smoke simulation (`regia_fluid`), and the
far scatter impostors, which do not sway (near scatter meshes sway with the renderer's foliage wind).

- `render.scene_wind` → `{ present, mode, velocity_m_per_s, mean_m_per_s, speed_m_per_s, gust,
  surface_factor, renderer_foliage_wind }`.
- `render.set_scene_wind { mode: "weather" | "manual" | "off", wind_m_per_s? }` → `{ before, after }`.
  It creates the resource when absent. `manual` needs `wind_m_per_s`, shortened to 40 m/s.

While a `SceneWind` exists the extraction sets the renderer's foliage wind every frame, so a call to
`FrameRenderer::set_foliage_wind` is overwritten: a host sets `SceneWind` instead. `render.set_foliage_wind`
writes the resource when it exists (`SceneWind::set_explicitly`: `manual` with the value, `off` for null).
`renderer_foliage_wind` is the renderer's foliage wind, which the extraction sets from `SceneWind` every frame. Materials read it. Grass follows `TerrainWind` instead (`terrain.wind` answers `weather_steered`), so it takes the wind once (ADR-2043).

The terrain wind follows the gust-free mean only, rounded (whole degrees, 1/32 strength steps), and is
written only when that changes: gusts do not redraw the grass's cached shadows, and a `terrain.set_wind`
holds until the mean wind changes a step.

## Drawn rain and snow

Stateless GPU particles in a box around the eye: no simulation buffer, no CPU work per drop, and a
full field on the first frame after a cut.

| Tier | Rain | Snow | Splashes | Box half width / height (m) | Occlusion view |
|---|---|---|---|---|---|
| MobileLow | 2,000 | 1,500 | 0 | 12 / 10 | off |
| MobileHigh | 6,000 | 4,000 | 256 | 12 / 10 | 256² over 32 m |
| Deck | 8,000 | 5,000 | 256 | 14 / 10 | 256² over 48 m |
| Low | 12,000 | 8,000 | 512 | 16 / 12 | 512² over 64 m |
| Medium | 24,000 | 16,000 | 1,024 | 18 / 12 | 512² over 64 m |
| High | 48,000 | 30,000 | 2,048 | 18 / 12 | 512² over 64 m |
| Ultra | 96,000 | 60,000 | 4,096 | 22 / 14 | 1024² over 96 m |

These are the counts at the heaviest rate; the drawn count follows the drops above 1 mm per unit
volume (about 9 % at 2 mm/h, 24 % at 6 mm/h).

- `render.precipitation` → `{ enabled, quality_tier, kind: { rain, snow }, rates, drawn: { rain, snow,
  splashes }, wanted: { rain, snow, splashes }, budget: { rain, snow, splashes, half_extent_m,
  height_m, resolution }, resolution, resolution_override, density_scale, max_drops, splashes,
  streak_exposure_s, occlusion: null | { size_texels,
  extent_m, origin, tilt_deg, redraws, last_redraw_frame }, fog_extinction_per_m }`. `wanted` is what
  the weather asks for now, `drawn` what the last frame's step reported (zero with the effect off or
  without air; `drawn.splashes` is zero while there is no occlusion view, since splashes draw only on the
  view's top surfaces).
- `render.set_precipitation { enabled?, density_scale?, max_drops?, splashes?, occlusion?,
  streak_exposure_s?, resolution? }` → `{ before, after }`. `resolution` is `"full"`, `"half"`,
  `"quarter"`, or null for the tier's; anything else answers `F0002` naming `resolution`.

### Resolution

Every tier draws the drops at full resolution in the frame's own pass (`budget.resolution` is
`full`). `resolution: "half"` or `"quarter"` draws them into a target of half or a quarter of the
frame's width and height in a pass of their own, which tests the scene depth in its fragment stage,
and composites that target over the frame (one bilinear sample per frame pixel). A streak thinner
than the target's pixel widens to one pixel and pays in coverage, so a smaller target shades fewer
fragments but widens and softens thin rain. `render.precipitation`'s `resolution` is the choice in
effect, `resolution_override` the run-time one (null: the tier's). The choice is a run-time setting,
not part of `config/weather.ron`; a tier's default moves only by ADR, after a device measurement
(ADR-3249 holds the lavapipe streak-survival numbers and the Pixel 6 / Fairphone 5 command).

While the effect and `visibility.couple_fog` are on, what falls thickens the air: a constant
extinction, applied wherever the frame fog is, with the fog's color. `RenderSettings::fog` is never
changed and the term exists whether the host's fog is on or not. `fog_extinction_per_m` shows it.

## Surface weather

`SurfaceWeather` is the state of exposed surfaces. In `simulated` mode the weather integrates it over
game time; in `manual` it holds what you set (a wet street with no weather clock).

- 5 mm/h wets a surface in 2.4 minutes; it dries in 30 minutes at a drying factor of 1. The factor
  rises with sun and wind and falls under cloud.
- Puddles fill only once a surface is wet, and only above 2.5 mm/h: drizzle wets and does not pool.
  Full puddles cover at most about 60 % of flat ground. Lab materials pool only with
  `WeatherResponse::puddles`; terrain may unless its layer stack says `puddles: false`, and its ruts
  and dips fill first.
- Snow accumulates at or below 0 °C (less up to 2 °C) and is kept as water equivalent, so the melt
  conserves it. Meltwater wets. 3 cm of depth hides the ground.
- `Weather::set_now` does not wet the ground at once: it gets wet at the rates above.

The response runs after the surface stage, on built-in and graph materials: porous surfaces darken
and metal keeps its albedo but gets the film, ripples stir the puddles, and snow settles on what
faces up. `WeatherResponse::NONE` keeps a material out of it.

A `.material.ron` (v5) sets the response in data, so no code is needed:

```ron
weather: (porosity: Some(0.6), wet: 1.0, snow: 0.8, puddles: true),
```

Every field is optional: `porosity: None` derives it from roughness and metallic, `wet` and `snow`
scale the effect (`0..1`, 0 keeps it out), `puddles` lets flat parts pool. No block is the default
response. A v4 file loads with the default and `regia upgrade-project` rewrites only its version;
cooked materials from earlier builds keep loading (cooked v6 reads v5 and v4). See
`docs/formats/material-v5.example.ron`.

A terrain's `.terrain.layers.ron` (v5) does the same per layer, and holds the puddle switch for the
whole stack:

```ron
LayerDesc(name: "sand", /* ... */ weather: Some((porosity: Some(0.95), snow: 1.0))),
LayerDesc(name: "road", /* ... */ weather: Some((porosity: Some(0.1), snow: 0.0))),
// ... after `layers: [...]`:
puddles: true,
```

A layer whose ORM map carries porosity in its blue channel (0 sheds a film, 1 soaks) says so with
`porosity_from_orm: true` (v5). An ORM map's blue channel is glTF metallic by default, so it is never
read as porosity without the flag; a named `porosity` wins over it:

```ron
LayerDesc(name: "cobbles", /* ... */ orm_texture: Some("cobbles_orm.png"),
    weather: Some((porosity_from_orm: true))),
```

`porosity: None` derives it from the layer's roughness, `snow` scales the cover (`0..1`; 0: a warm
road stays bare), and no block is the default response. The layers' blocks blend by their slope,
altitude and painted weights, so a sand-to-rock transition mixes the two. `puddles: false` keeps the
whole terrain to a wet film (a desert, scree). A v3 file loads with the defaults and
`regia upgrade-project` rewrites only its version. Nothing changes on a dry frame.

The Mobile tiers' lite forward path (phones) shades the same rule. It compiles its weather variant
the first time a frame is wet, snowy or has an extinction, and a dry frame draws with the pipelines
it always had. The High tier reads the occlusion view; the Low tier has none, so covered surfaces
there get wet too.

- `render.surface_weather` → `{ present, mode, wetness, puddles, snow_we_mm, snow_depth_m, snow_cover,
  ripple, rain_mm_per_h, snow_we_mm_per_h, drying_factor, frame: { wetness, puddles, snow_cover, ripple },
  terrain_variant_on, terrain_layers: [{ entity, puddles, overrides, layers: [{ name, porosity,
  porosity_from, snow }] }] }`. `terrain_layers` lists each terrain's response: `porosity_from` is
  `"layer"` where the layer names one, `"orm"` where it reads each texel's porosity from its ORM
  map's blue channel (v5 `porosity_from_orm`; `porosity` is then `null`, as there is no single
  number), and `"roughness"` where it comes from the layer's scalar roughness (a layer with an ORM
  texture shades with the texture's roughness instead; a layer that sets `porosity_from_orm` without
  an ORM texture also lands here). `overrides` is true when a layer's block differs from the
  default.
- `render.set_surface_weather { mode?, wetness?, puddles?, snow_depth_m?, snow_cover? }` → `{ before,
  after }`. It creates the resource when absent. Supplied numeric values must remain finite in `f32`
  before clamping: wetness, puddles and cover to 0..1, depth at zero. Snow water/depth conversions
  must also stay finite, including the inverse multiply used by the next simulation step; otherwise
  the call returns `invalid_params` and leaves the surface unchanged. A value given without `mode`
  keeps the mode.

## The occlusion view

A depth map of what stands between the sky and a point, looking along the mean fall direction. It
hides drops under roofs, places splashes on the top surface the rain hits, and keeps covered
surfaces dry. It lives in a tile of the local-light shadow atlas, which grows by the tile's size, so
no light loses room. It is drawn when a surface is not dry or precipitation falls, on a tier that has
one. It redraws when its snapped origin moves (an eighth of its side), its tilt bucket changes, or a
caster over its square changes bounds or pose, and holds otherwise: `redraws` counts.

- `render.surface_exposure { points: [[x, y, z], ...] }` (at most 256) → `{ ready, map_frame, points:
  [{ p, exposure, cover_y }] }`. `exposure` is 1 under open sky and 0 under cover, and `cover_y` is the
  height of the first occluder above the point (null under open sky). The first call schedules a
  one-shot copy of the tile and answers `ready: false`; a call after the next frame answers.

Errors: `F0002` for invalid parameters (the message names the field), `F0003` without a
`RuntimeRenderer`. `render.surface_exposure` answers `F0002` with `reason: "occlusion_off"` while the
frame has no view.

A bad config shows in `render.get_weather`: `config.error.code` is `weather_config_invalid` (does not
parse, an unknown field, a preset name clash, a value out of range) or `weather_config_version` (newer
than this build).

## Budgets and platforms

The budgets in spec 99 § 12 are targets until the real-card run records them in `docs/perf-log.md`:

| Where | What | Budget |
|---|---|---|
| CPU | the weather drivers and the integrator | 0.02 ms a frame |
| CPU | the precipitation prepare (one uniform, no per-drop work, no per-frame allocation) | 0.02 ms a frame |
| 5080, 1440p, Desktop High | `storm` rain (48k streaks, 2k splashes) | 0.6 ms |
| 5080, 1440p, Desktop High | `blizzard` snow (30k flakes) | 0.5 ms |
| 5080, 1440p, Desktop High | an occlusion view redraw, on the frames it redraws | 0.3 ms |
| 5080, 1440p, Desktop High | the surface response on, over lab and terrain | 0.15 ms |
| 5080, 1440p, Desktop High | the surface response present but dry | 0.02 ms |
| Pixel 6, MobileHigh | rain at 6k streaks | 1.0 ms |
| Pixel 6, MobileHigh | the surface response | 0.3 ms |

The tile costs 1 MB of atlas at 512² and 4 MB at 1024². Lavapipe measures correctness, counts and redraws, not time.

- Linux, Windows and Android aarch64 compile the same code. The draw uses no compute and no storage buffers, so it
  runs on every tier, and the shaders use no derivatives in the vertex stage and fixed loop bounds (FXC-safe).
- Android surface loss: the draw holds pipelines and a uniform, and nothing is recreated beyond what `VfxGpu` recreates.
- A tier whose shadow atlas plus the tile passes the device's texture limit has no occlusion view: drops fall through
  roofs and covered surfaces get wet there, and `render.precipitation`'s `occlusion` is null.
- A frame that goes wet for the first time compiles the terrain's weather variant once, unless the project has
  `effects.surface` on, which adds it to the pipeline warm. Going dry and wet again compiles nothing. The same
  setting has the Mobile tiers' lite forward path build its weather pipelines with its first dry frame.

## Checks you can run

```json
{ "method": "render.set_weather", "params": { "preset": "rain" } }
{ "method": "render.set_surface_weather", "params": { "mode": "simulated" } }
```

- Set `rain`, step 300 frames at 1/60 s, read `render.surface_weather`: `wetness` rose by
  `R · 5 s / (wet_mm · 3600)`, about 0.042 at 6.1 mm/h (`rain` is precipitation 0.35, and the
  rate is `50 mm/h × precipitation²` over the default `wet_mm` of 0.2). Take a screenshot.
- Set `snow` with `paused: false`, step, and watch `snow_cover` rise. A screenshot shows white on flat
  ground and not on walls.
- Put a roof over a point and compare `render.surface_exposure` under and beside it.
- Give the road layer `weather: Some((snow: 0.0))`, set `snow`, read `render.surface_weather`
  (`terrain_layers[0].layers[i].snow` is 0) and take a screenshot: the road stays dark while the
  verge whitens.
- Set `render.set_scene_wind { mode: "off" }` and read `render.scene_wind`: `renderer_foliage_wind` is
  null.
- Over grass, set `rain` and read `terrain.grass`: `report.surface_weather` is true and a screenshot
  shows darker, glossier blades; set `snow` and the blade tips whiten.
- Set `render.set_precipitation { resolution: "quarter" }` in `storm` and screenshot next to `"full"`:
  the streaks are wider and softer; `render.precipitation` answers `resolution: "quarter"`.

## Not in this page's scope

Lightning, lens drops, streaks running down walls, snow deformation, hail, regional weather, and rain
on water bodies are out of scope (spec 99 § 16). The path tracer and the baked GI stay dry, and local
lights do not light rain and snow.
