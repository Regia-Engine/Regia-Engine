# Terrain virtual texture

The terrain virtual texture caches the photoreal terrain surface in pages of one atlas, baked by
the terrain shader's own surface function. Pixels whose pages are resident read them instead of
shading the surface. It is opt-in: it trades a little texture sharpness for terrain shading cost.

## Methods

- `terrain.vt` reports whether it is on, its settings, and last frame's counters:
  - `resident_pages`.
  - `stats.baked` and `stats.rebaked`.
  - `stats.missing` and `stats.oldest_miss_frames` (how far behind the bakes are).
  - `stats.hit_rate`.
  - `stats.bake_ms` and `stats.reduce_ms` (GPU).
  - `depth_test`: `early`, `late` (DX12, Metal: needs `late_depth_fallback`) or `unsupported`
    ([terrain virtual texture tiers](terrain-vt-tiers.md)). `supported` is `true` on `early`,
    and on `late` only while the settings carry `late_depth_fallback`.
- `terrain.set_vt { enabled, settings }` switches it on, off, or to new settings on the next frame.
  `settings` is any subset of:
  - `cache`: `lighting`, `relief` or `surface`.
  - `lod0_texel_m`, `page_texels`, `atlas_slots`, `oversample`, `near_m`, `min_normal_y`.
    `near_m` is metres. The whole-surface cache never serves nearer than 24 m (the near-field
    detail has faded, and the atlas stores no frost). Lighting and relief use `near_m` as
    given, so a value under 24 serves those nearer pixels from the atlas when the slope
    passes `min_normal_y`.
  - `bakes_per_frame`, `rebakes_per_frame`, `stale_threshold`, `stochastic_lod`.
  - `late_depth_fallback`: also run on devices without forced early depth tests.

  Wait until `terrain.vt` shows `pending: false`. A refused request shows in `error`.

## What `cache` picks

| `cache` | Cached | Per pixel | Frames vs uncached (DO27 plates, SSIM) |
|---|---|---|---|
| `lighting` | Cliff-relief normal, heightfield sun occlusion, cavity, horizon slopes, bounce | Layers, mid relief, strata, rock, talus, macro | 0.998–1.0 |
| `relief` | The above plus the strata/talus albedo factor and the fractured-rock normal | Layers, mid relief, macro | 0.995–1.0 |
| `surface` | The whole surface | Lighting only | 0.977–0.997, slightly softer texture detail |

## Behavior

- **A page miss never shows.** A pixel whose pages are not resident shades the surface in full.
  The counters only tell you the cost.
- **Pages need a few frames to arrive.** Pages are requested by the frame that needs them and
  baked two frames later. A camera cut therefore costs full shading for a few frames.
- **Changing what the surface depends on drops every page.** That covers the sun direction, the
  material and layers, the heights, the terrain budget, and the relief settings.
- **Moving cameras re-bake pages.** The re-bake budget is `rebakes_per_frame`. When the camera
  has moved far enough to change a page's distance-dependent terms, it is re-baked.
- **Stochastic LOD needs TAA.** `stochastic_lod` reads one of the two LODs per pixel, and the
  temporal filter averages them. Turn it off for frames without TAA.

## Checking it

1. Take `seeing.screenshot` with the texture off, then on.
2. Debug view 96 shows how far the bake's rebuilt geometry is from the rasterized one:
   - green: within 5 cm;
   - red: height off;
   - blue: pyramid level off.
3. `TerrainVtHandle::read_page` (Rust) reads a page back for tests.

## Example calls

Example calls (`regia agent call <method> '<params json>'`; entity ids come from `scene.query_entities`):

```text
terrain.vt {}
terrain.set_vt {"enabled": true, "settings": {"cache": "lighting"}}
```
