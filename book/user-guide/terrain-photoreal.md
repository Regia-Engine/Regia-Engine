# Photoreal terrain

The photoreal material path for `regia_terrain` (DO27 alpha lane): height-blended layers,
triplanar projection, micro-normal detail and whole-map macro variation, over the CDLOD
heightfield the simulation agrees with. Without a material, the terrain renders the plain
M13 look.

There is one terrain texture cook, `regia_asset_pipeline::cook_tiled_texture`. The 8K
masks and macros are cooked by it and streamed by `regia_terrain::TileCache`.
`cook_terrain_material` cooks layer textures (albedo, normal, height, and ORM when a layer
sets `use_orm`) and macro tiles through the same function, as one whole-image tile each;
`TerrainGpu::set_material_cooked` uploads those KTX2/BCn blobs. `TerrainGpu::set_material`
uploads the same material as in-memory RGBA8 (the lab path); the two render the same frame
to within 0.8/255 (`cooked_bcn_material_renders_like_the_rgba8_material`).

## Layer stack (format v5)

A `.terrain.layers.ron` layer names `albedo_texture`, `normal_texture`,
`height_texture` and `orm_texture` (glTF ORM: R occlusion, G roughness, B metallic), plus
`detail_normal_strength` and `height_blend`. v4 adds an optional per-layer `weather`
block and the stack's `puddles` (how rain and snow change the ground, see
[Rain, snow and wet surfaces](../agents/weather-precipitation.md#surface-weather)). v5 adds
the block's `porosity_from_orm`: the ORM map's blue channel as the layer's porosity.
Loaders migrate v1 → v2 → v3 → v4 → v5; `orm_texture` defaults to `None`, which keeps the
scalar roughness, and the weather fields default to today's response.
`regia upgrade-project --write` re-saves older files at v5.

`LayerStyle::bake` generates tileable CC0-style sets for labs. `use_orm` stays off on
those bakes so the scalar roughness is unchanged until a caller opts in.

```text
cargo run -p terrain_lab -- --photoreal --camera near --sun noon \
    --screenshot docs/evidence/terrain-photoreal/near_noon.png
```

The near camera sits 1.05 m above the valley floor and looks at a point 1 m ahead on the
surface, so the frame is a surface read. Mid is 50 m up, looking across the grass to the
foot of the eroded ring. Far is about 2 km from the map center with a 0.55 rad lens and a
sun-tinted aerial haze (~45% at 2 km; `FogSettings::outdoor_haze` is tuned for a 200 m
scene and fogs a 2 km view solid). The 256² sim and the 4096² visual share the 1280 m
extent (~0.31 m/texel). The lab sim is a driveable valley inside a ring of ridged
mountains (~170 m), droplet-eroded on the slopes; the visual is its Catmull-Rom upsample
plus rock detail on steep cells.

## What the material path does

- **Height-blended transitions** — each covering layer gets the key
  `w + hb·(h − ½)` (`w` its normalized band weight, `h` its height texel); only layers within a
  depth of the highest key (`mix(0.5, 0.12, max hb)`) share the pixel, so the tallest texels win
  the transition locally (snow settles in the cracks of the rock). A layer whose slope/altitude
  band is zero stays at zero however tall its texture is, and `height_blend = 0` on every layer
  keeps the plain normalized weights.
  Slope bands that start at 0 keep full weight on flat ground.
- **Triplanar projection** — weights are `|N|` (Geiss, GPU Gems 3). A flat layer with the
  flag on keeps the top-plane sample. Steep faces mix the side planes in. Per-layer
  `triplanar` or automatic above `triplanar_auto_slope`.
- **Detail normals** — a tiling micro-normal per layer, accumulated as a world-space slope
  vector, projected onto the surface and faded from `detail_fade_start` to
  `detail_fade_end`. A 0.05 slope tilts the normal ~3°, not 45°.
- **Shading normal** — the photoreal path shades with the heightfield normal at the
  pixel's footprint, so distant CDLOD levels keep their relief.
- **Sky light** — the ambient is scaled by the hemispherical sky visibility
  `(1 + N·up)/2` (floor 0.35), so shaded slopes keep shape at low sun.
- **Exposure** — `TerrainFrame::exposure_compensation_ev` (default 0 = the EV100 14.5
  reference). `TimeOfDay::metered_exposure_compensation_ev` is what an incident meter
  dials in: 0 at noon, 1.81 EV for the lab morning preset, 2.38 EV for the evening preset (capped at 3 EV).
- **Macro variation** — the lab's in-memory bake is a small RGBA8 tile set. The 8K albedo,
  normal, AO and the two RGBA masks are the tiled KTX2 cook. A macro grid is up to 16 × 16
  tiles (`MAX_MACRO_TILES_PER_SIDE`). The `macro_resident` argument of `set_material` and
  `set_material_cooked` names the tiles to upload, in any order; each tile left out shades
  from the coarse whole-map fallback, and every other tile keeps its own data (a 256-bit
  residency mask, ADR-3367).
- **Anti-tiling** — a triangle grid of `anti_tiling_cell` meters (Heitz & Neyret 2018,
  Deliot & Heitz 2019): every lattice vertex hashes a UV offset and rotation, and a
  fragment blends its triangle's three taps with sharpened barycentric weights. The blend
  is continuous across every edge; normals are rotated back per tap.
- **ORM** — when `use_orm` is set, R is indirect-light occlusion (scaled by the occlusion strength; it darkens bounce light and SSAO, not the albedo) and G replaces roughness. B (glTF metallic) is not read for shading: terrain is never metallic. A layer whose weather block sets `porosity_from_orm` (v5) reads B as its porosity on wet frames instead (0 sheds a film, 1 soaks); without that flag, B stays unread whatever the map holds.
- **Road** — a same-depth decal (`TerrainGpu::set_road`). The fragment keeps the terrain
  depth, so the ribbon does not z-fight. `RoadRibbon` is the mesh form; its lift is
  1 cm ..= 0.25 m.

Debug view 7 flags the visual-vs-sim delta. Driveable cells (slope under 0.6 rad) are
green within ±0.25 m and red outside it. Cliffs are blue within ±2 m and yellow outside
it. A 1 m cliff stays blue.

## The sim ground

Both grids cover 1280 m. `SimGround::height` is the bilinear of the raw u16 sim samples.
The character and the vehicle stand on that, not on the visual surface. The lab's visual
is the Catmull-Rom upsample of the sim plus a non-periodic ±0.12 m swell (stays green),
rock detail on steep cells, a +0.6 m driveable bump and a +3.2 m cliff. In the mid
evidence view, red marks gentle slope feet where the bicubic surface departs from the
bilinear sim by more than 0.25 m — exactly what the view is for. Coarse LOD far from the
camera compares the morphed mesh to the sim, so the horizon band can flag red even where
the two heightfields agree.

## Budgets (stated targets, two maps)

`terrain_map_budget` is the cooked size: 4096² R16 height, 8K albedo/normal/AO, two 8K
masks, six layers at 4K (albedo, normal, ORM, height). The two-map package is about
2.23 GiB, under the 6 GiB ceiling. Tile-cache allowances:

| GPU | Tile cache | Terrain-pass target |
| --- | --- | --- |
| RTX 5090 | 8 GiB | 2.0 ms at 1440p |
| RX 7900 XTX | 6 GiB | 2.5 ms at 1440p (spec 26 § 6 floor) |
| Steam Deck (Medium) | 1 GiB | 4.0 ms at 800p |

The frame times are targets, not measurements from this machine. The Deck cache does not
hold the full footprint; it streams. The 5090 and 7900 XTX budgets hold the cooked set.

## Metrics and evidence checks

`terrain_lab --metrics path.md` measures the nine evidence shots (mean luma, near-black
fractions, lower-half σ/μ, bright outliers, worst one-pixel step, relief contrast, far-shot
sun response, sim-delta flag counts), then the autocorrelation peak, blend sharpness and
the 30-frame LOD-pop diff. The LOD gate is
`lod_pop_stays_under_the_bound_across_30_frames`: the same-camera selection diff stays
under 4/255, and zeroing the morph band pops by more.

`cargo test -p terrain_lab` runs three checks on the evidence cameras themselves:

| Test | Round 3 | Now (1280×720) |
| --- | --- | --- |
| `mid_shot_is_continuous_material_at_every_sun` (luma<20 < 2%, σ/μ < 0.3, > 2× median < 0.5%) | 77–91% under luma 20; white dot lattice | 0% dark; σ/μ 0.10–0.21; ≤ 0.22% bright |
| `near_shots_read_the_material_at_low_sun_without_a_seam` (luma<8 < 0.5%, mean > 35, step < 3) | 51% / 63% under luma 8 at morning / evening; noon seam | 0%; mean 67–71; step ≤ 1.0 |
| `far_shots_show_relief_and_follow_the_sun` (relief > 1, every sun pair Δ > 8) | one fogged slab, relief 0.2, suns alike | relief 5.2–7.3; Δ 22–32 |

Root causes fixed for those frames: the additive height blend let the out-of-band snow
layer print blobs over the grass; the procedural albedo was stored as linear bytes in an
sRGB array (decoded twice); the micro normal tilted 45° for any texel; the stochastic
grid cut a seam on every cell edge; the lab map was a 34 m hill seen through a 200 m fog
from 2 km. The cooked path had its own bug: BCn mips below 4×4 encoded as zero blocks
(black distant terrain), now edge-padded.

## Normal maps must be zero-mean (ADR-1428)

Anti-tiling gives every lattice cell its own texture rotation and rotates the micro normal back with it.
A tiling normal map therefore has to average out to flat: a map whose red and green bytes average 147
instead of 128 carries a constant tilt (here about 12°), and the lattice points that tilt a different
way in every cell. Under a low sun the cells light and shade differently, and a nearly uniform snow layer
shows a hexagonal grid of dark and light blotches at 50 to 200 m. The cause was found in the showcase's
Poly Haven `snow_02` and `snow_03` normal maps (mean offsets +19 and +29 byte levels).

The engine now removes the mean X and Y tilt of every layer normal map at upload when the anti-tiling
lattice is on (`anti_tiling_cell > 0`) and the mean is at least 2 byte levels off (`Rgba8Image::recentred_normal`).
Maps that are already zero-mean, the procedural sets, and every material with anti-tiling off are
uploaded byte for byte. If you author a layer on purpose with a net tilt, give it a separate slope or
rotation instead; the lattice cannot keep a direction. For a texture you are about to ship, check it
with `Rgba8Image::normal_mean_shift`, which returns the mean offsets in byte levels (0 means flat).

Terrain debug views 90, 91 and 92 (`SHOWCASE_TERRAIN_DEBUG` in the showcase host, `set_debug_view` in
code) switch the anti-tiling lattice off, the height blend off, and both, with the rest of the shading
unchanged. They are for telling which of the two puts a pattern on screen.
