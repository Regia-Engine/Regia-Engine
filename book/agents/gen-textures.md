# Procedural textures and heightfields (`gen.texture`, `gen.heightfield`, `regia_gen`)

The second half of `regia_gen` (ADR-7280, spec 23 § 6) makes textures and terrain from code:

- seeded **noise** (value, gradient, cellular) summed over octaves as fBm or ridged, tileable on request;
- **masks**: threshold, levels, blend, blur and invert over a single-channel float image;
- **mesh-derived masks**: an edge (convexity) and a cavity (concavity) mask from a mesh's dihedral angles,
  baked into the mesh's UV space;
- **wear**: an edge-wear and grime recipe that combines the two;
- **heightfields**: noise fill, terrace, flatten-to-pad, clamp, normalize, thermal erosion, slope and
  normal maps, and export to the engine's terrain heightmap form and to a mesh grid.

Agents reach them through two methods, `gen.texture` and `gen.heightfield`, which take a JSON recipe
and report what came out. Every result is the same to the bit on every platform: noise uses integer
hashing and exact pixel coordinates, and the code uses only exact IEEE operations (`+ - * /`, `sqrt`)
plus the engine's platform-independent `cos` and `powf`. A test can pin a recipe by its `hash`. For
meshes, see [Procedural meshes](gen-mesh.md).

## Images

The working image is `FloatImage`: one 32-bit float per pixel, row-major, top row first, values usually
in `[0, 1]`. It converts to the engine's image forms:

| Output | Rust | Recipe `format` | Bytes |
|---|---|---|---|
| Gray RGBA8 (`regia_asset::CookedImage`) | `image.to_rgba8()` | `rgba8` (default) | R = G = B = the value, A = 255, rounded to 8 bits. Linear values: a mask, not an sRGB color. |
| R16 (`regia_asset::Heightfield` samples) | `image.to_r16()` | `r16` | Rounded to 16 bits, little endian. |
| R32F | `image.r32f_bytes()` | `r32f` | Each value's bits, little endian. |
| Packed RGBA8 | `pack_rgba8([&r, &g, &b, &a])` | (Rust only) | Four masks in one texture. |

Values outside `[0, 1]` are clamped when quantized.

## Noise

| Field | Default | Meaning |
|---|---|---|
| `kind` | (required) | `value` (hashed lattice values, quintic fade), `gradient` (Perlin-style: hashed unit gradients, quintic fade), `cellular` (Worley F1: distance to the nearest of one jittered point per cell, capped at 1), `cellular_edge` (F2 − F1: 0 on cell borders, good for cracks and tiles). |
| `seed` | 0 | Any 64-bit integer. |
| `cells` | 4 | Lattice cells across the image in the first octave. |
| `octaves` | 1 | 1 to 16. |
| `fractal` | `fbm` | `fbm`: the amplitude-weighted mean of the octaves. `ridged`: each octave becomes `(1 − abs(2v − 1))²` first, so mid values form sharp ridges. |
| `lacunarity` | 2 | Integer cell multiplier between octaves, 1 to 8. |
| `gain` | 0.5 | Amplitude multiplier between octaves, `(0, 1]`. |
| `tile` | false | Wrap the lattice at the image size: the image repeats without a seam. |

Coordinates are pixels: the first octave has `cells` cells across each axis whatever the image size,
so a non-square image stretches the cells. The finest octave may have at most 2^20 cells across.

## Mask operations

| Operation | Rust | Recipe op | Notes |
|---|---|---|---|
| Threshold | `threshold(&img, at, softness)` | `{ "threshold": { "at": 0.5, "softness": 0.1 } }` | 1 at or above `at`, else 0; a positive `softness` is a smoothstep over `[at − softness, at + softness]`. |
| Levels | `levels(&img, &Levels { .. })` | `{ "levels": { "in_low", "in_high", "gamma", "out_low", "out_high" } }` | `[in_low, in_high]` → `[0, 1]` (clamped), raised to `gamma` (above 1 darkens the mid-tones), then onto `[out_low, out_high]`. Every field has a default (0, 1, 1, 0, 1). |
| Invert | `invert(&img)` | `{ "invert": {} }` | `1 − v`. |
| Blur | `blur(&img, radius, passes, wrap)` | `{ "blur": { "radius": 2, "passes": 3, "wrap": true } }` | Separable box blur; three passes approximate a Gaussian. `wrap` for tileable images, otherwise edge pixels repeat. |
| Blend | `blend(&a, &b, mode, opacity)` | `{ "blend": { "mode": "multiply", "opacity": 1, "layer": { "source": {...}, "ops": [...] } } }` | `a + (mode(a, b) − a) · opacity`; modes `mix`, `multiply`, `add`, `subtract` (both clamped), `max`, `min`, `screen`, `overlay`. |

## Edge, cavity and wear masks from a mesh

`curvature_masks(&mesh, width, height, &CurvatureOptions)` needs a mesh with one UV per vertex (a
non-overlapping layout gives a usable texture; overlapping UVs keep the larger value). For every edge
shared by two triangles it measures the bend between the faces (`1 − n₁·n₂`, ramped from `min_angle`,
default 10°, to `full_angle`, default 60°) and whether the edge is convex (an outer corner) or concave
(an inner corner). Each covered texel takes its point on the surface and the strongest nearby edge,
fading to 0 at `width` mesh units (default 0.05) from it. Uncovered texels next to a UV island are
filled from their neighbours for `dilate` rings (default 2) so filtering at seams does not pull in
black. On a cube with each face in its own UV tile, the edge mask marks the 12 edges and leaves the
face centres at exactly 0; a cube has no concave edge, so its cavity mask is empty.

`wear_masks(&mesh, width, height, &WearOptions)` turns them into material masks:

- `wear`: 0 wherever the edge mask is 0; else the edge mask broken up by tileable gradient fBm
  (`breakup_cells`, `breakup_strength`) and stepped at `1 − amount`, so a higher `amount` wears more of
  the edge band;
- `grime`: the blurred cavity mask times a tileable value-noise dirt pattern, scaled by `grime`;
- `WearMasks::packed()` gives one RGBA8 texture: R wear, G grime, B edge, A cavity.

## Heightfields

`Heightfield { width, height, spacing, heights }` holds heights in metres on a grid centred on the
origin: sample `(col, row)` sits at `x = col · spacing − (width − 1) · spacing / 2` and the same for `z`
with `row`, row 0 at −Z, as in `regia_asset::mesh_from_heightfield`.

| Operation | Rust | Recipe op | Notes |
|---|---|---|---|
| Noise fill | `add_noise(&NoiseParams, amplitude, base)` | `{ "noise": { "noise": {...}, "amplitude": 40, "base": 0 } }` | Adds `base + amplitude · noise`. |
| Terrace | `terrace(steps, strength)` | `{ "terrace": { "steps": 6, "strength": 1 } }` | `steps` (at least 2) flat levels spread evenly from the minimum to the maximum; `strength < 1` blends toward them. |
| Flatten pad | `flatten_pad(&Pad)` | `{ "flatten": { "center": [x, z], "radius": 10, "falloff": 6, "height": 12 } }` | Inside `radius` the height is exactly `height`; a smoothstep band of `falloff` metres blends back. |
| Clamp | `clamp(min, max)` | `{ "clamp": { "min": 0, "max": 50 } }` | |
| Normalize | `normalize(min, max)` | `{ "normalize": { "min": 0, "max": 50 } }` | The old minimum maps to `min` and the maximum to `max`. |
| Thermal erosion | `thermal_erode(&Erosion)` | `{ "erode": { "iterations": 20, "talus": 0.7, "rate": 0.25 } }` | Material slides to lower neighbours (8 of them) wherever the drop is steeper than `talus` (rise over run, the tangent of the angle of repose); `rate` in `(0, 0.25]` (default 0.25) is the share of the largest excess moved per pass (all samples update at once, and a larger share overshoots into a two-sample ripple). Conserves the total height; the result does not depend on visiting order. |
| Slope map | `slope_map()` | `slope` in the answer | The gradient's length per sample, rise over run. |
| Normal map | `normals()`, `normal_map()` | (Rust only) | Tangent space in the renderer's OpenGL convention: R = +X, G = −Z (image up), B = +Y. |
| Terrain export | `to_terrain()` | `terrain` in the answer | `TerrainExport { heightfield, world_width, height_scale, height_offset }`: the 16-bit heightmap and the mapping `regia_terrain::Terrain::new` takes; every sample rebuilds within `height_scale / 65535`. The terrain form is square: a non-square grid is `gen_invalid_input` in Rust and `terrain: null` in the answer. |
| Mesh grid | `to_mesh()` | `mesh` in the answer | One vertex per sample, two counter-clockwise triangles per cell, `mesh_from_heightfield`'s order and UVs. |

## `gen.texture`

```json
{ "jsonrpc": "2.0", "id": 1, "method": "gen.texture", "params": {
  "recipe": {
    "width": 256, "height": 256,
    "source": { "noise": { "kind": "gradient", "seed": 7, "cells": 4, "octaves": 5, "tile": true } },
    "ops": [
      { "levels": { "in_low": 0.3, "in_high": 0.7 } },
      { "blend": { "mode": "multiply", "layer": { "source": { "noise": { "kind": "cellular_edge", "seed": 2, "cells": 8, "tile": true } } } } }
    ],
    "format": "r16"
  }
} }
```

The answer (values illustrative): `{ "width": 256, "height": 256, "format": "r16", "bytes": 131072,
"hash": "…64 hex digits…", "min": 0.0, "max": 0.93, "mean": 0.41 }`.

A source is one of `noise { … }`, `constant { value }`, `edge { mesh, curvature? }`,
`cavity { mesh, curvature? }`, `wear { mesh, wear? }` or `grime { mesh, wear? }`, where `mesh` is a
[`gen.mesh` recipe](gen-mesh.md) that gives the mesh UVs (for example with a `uv_box` or `uv_planar`
op), `curvature` is `{ width, min_angle, full_angle, dilate }` and `wear` is `{ seed, amount,
breakup_cells, breakup_strength, grime, grime_blur, curvature }`.

Limits (`TextureLimits::default()`): side 1024 (512 with a mesh source), 32 operations including nested
layers, layer nesting 4, blur radius 32 and 4 passes (the wear `grime_blur` too), the `gen.mesh` limits
for mesh recipes, and an estimated cost of at most 2^25 work units (`texture_work`; one unit is one
noise octave at one pixel, about 40 ns, so a call stays near a second and a half). The estimate is
checked before anything is built: 1024² of 6-octave gradient noise plus a radius-32 blur fits; four
full-size 16-octave cellular layers do not. The estimate counts the texture work only: a mesh
source's recipe build is bounded separately by the `gen.mesh` limits above, so a call with a mesh
source may cost texture budget plus mesh budget. UV winding does not matter when baking a mesh
source: CCW and CW UV islands (mirrored unwraps) bake the same coverage.

## `gen.heightfield`

```json
{ "jsonrpc": "2.0", "id": 1, "method": "gen.heightfield", "params": {
  "recipe": {
    "width": 129, "height": 129, "spacing": 2.0,
    "ops": [
      { "noise": { "noise": { "kind": "gradient", "seed": 1, "octaves": 5 }, "amplitude": 40 } },
      { "erode": { "iterations": 20, "talus": 0.7 } },
      { "flatten": { "center": [0, 0], "radius": 12, "falloff": 8, "height": 18 } }
    ]
  }
} }
```

The answer: `{ width, height, spacing, min, max, mean, hash, slope: { max, mean }, terrain: { world_width,
height_scale, height_offset, hash } | null, mesh: { vertices, triangles, hash } }`. `hash` covers the
float heights, `terrain.hash` the 16-bit samples, `mesh.hash` the grid mesh (as `gen.mesh` hashes).

Limits (`HeightfieldLimits::default()`): side 1025, 32 operations, and an estimated cost of at most
2^26 work units (`heightfield_work`: an erode costs samples × iterations, a noise fill samples ×
octaves × 2, three times that for cellular kinds), for example 513 × 513 samples × 255 erosion passes,
or 1025 × 1025 samples with 8 octaves of noise and 40 erosion passes.

Both methods are read-only, need no world, renderer or project, and are rate-limited as heavy calls.

## Errors

| `data.code` | When |
|---|---|
| `F0002` | `recipe` is missing or not an object. |
| `gen_invalid_recipe` | The recipe does not parse: an unknown source, operation or field, a wrong type. The message names the field. |
| `gen_invalid_input` | A value is out of range: a zero image side, octaves or cells of 0, a gain outside `(0, 1]`, a non-finite number, `in_high ≤ in_low`, a mesh without UVs, terrace steps under 2, an erosion rate outside `(0, 0.25]`, a negative talus. |
| `gen_too_large` | A limit above was passed. |
| `gen_bevel_too_wide`, `gen_cap_not_closed` | A mesh source's `gen.mesh` recipe failed (see [Procedural meshes](gen-mesh.md)). |

## Checking a result

1. Pin `hash` in a test: the same recipe gives the same bits on Linux, Windows and Android.
2. Use `min`, `max` and `mean` to check a mask's coverage before baking it into a material (a wear
   mask with `max` 0 means the mesh has no UVs over its edges, or the edges are softer than
   `min_angle`).
3. For terrain, check `slope.max` against what the character controller can climb, and the
   `terrain` mapping before handing the 16-bit samples to `regia_terrain`.

The acceptance tests are `crates/regia_gen/tests/textures.rs` and `crates/regia_gen/tests/heightfields.rs`
(tileable edges, the fBm range, the cube's 12 edges, the terrace step count, the pad height, erosion's
conservation and downhill movement, byte pins).
