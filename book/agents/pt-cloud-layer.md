# Path-traced cloud layer

The reference path tracer can put a cloud layer over a scene, so the real-time cloud model has something to be
checked against. It is a tool for validation and look work, not a runtime feature: `regia_pt` is never part of a game
build. There are two forms:

- **The cloud field** (`PtScene::clouds`, ADR-2127): the raster sky's own density, broken clouds and all, traced as a
  participating medium. Use it to compare the raster clouds with a reference.
- **The slab** (`PtCloudLayer`, ADR-1834): a uniform layer with a known answer, for checking the ground-light model.

## The cloud field

`PtScene::clouds = Some(PtCloudField { .. })`. `None` (the default) traces no cloud, and a scene without a field traces
the same image as before.

The density is one definition: `regia::cloud_density` (`crates/regia_render/shaders/cloud_density.wgsl`) holds the
arithmetic, and `clouds.wgsl` and the tracer's `pt.wgsl` both import it. They read the same weather map, noise volumes
and curl field, the raster through its sampler, the tracer from a buffer with the same filtering. Free flights use delta
tracking against a constant majorant and shadow rays use ratio tracking, so every scattering order is unbiased: one
path vertex (`PtSettings::max_bounces = 1`) is single scattering, and 128 to 256 vertices is effectively all of them.

| `PtCloudField` field | Meaning |
|---|---|
| `settings` | The layer: `regia_render::CloudSettings`, as the raster sky reads it. |
| `planet_radius_m` | The planet the layer's shells curve with. |
| `albedo`, `phase` | `PtCloudField::from_settings` gives water droplets (albedo 1, `PtCloudPhase::DROPLET`, asymmetry 0.85). `CLOUD_MARCH_ALBEDO` with `PtCloudPhase::RASTER_DUAL_LOBE` are the march's own values. |
| `curl` | The detail erosion swirls with the curl field, as the raster's cone light does (Desktop High and up). |
| `density` | `PtCloudDensity::Noise`, or `Uniform(d)` for a shell with a closed-form answer. |

The cloud is a medium like the air: the tracer's medium layers still apply, and the air between the eye and a cloud
counts as it does for a surface. Cirrus (`CloudLook::cirrus`, a sheet the raster composites) is not part of the field.

### From an agent

```json
seeing.reference_trace { "scene": "current", "clouds": "droplet", "max_bounces": 256, "spp": 1024 }
```

`clouds` is `off` (default), `droplet` (the physical reference) or `raster` (the march's phase and albedo).
`scene_info.clouds` says what went in (`null` for none, including a frame with no volumetric cloud or an airless planet).
It needs `scene: "current"`. Score the result with `seeing.compare`. A wide trace is slow: a cloudy path has hundreds of
vertices and each of them marches its shadow rays; start small.

### The numbers

```text
cargo run --release -p regia_pt --example cloud_compare -- \
    [--layers default_045,fair_cumulus,cumulus,stratocumulus] [--width 64] [--height 48] [--spp 32] \
    [--bounces 512] [--raster-frames 16] [--seed 3] [--no-ground] [--no-views] [--terms]
```

It needs a GPU adapter (lavapipe is enough; the full grid takes a while there) and prints one JSON object per line. The
raster side is the real pass, `regia_render::render_cloud_view` (`fs_cloud_march`) and `render_cloud_sun_map`
(`fs_cloud_shadow_map`), with no history, no upsample and no air, so the numbers are the cloud alone. Both sides get the
same sun (as it arrives at the middle of the layer) and sky.

`"kind": "view"` lines: a camera 3 m up, looking 40° above the horizon. Each of `physical` (droplets),
`raster_parameters` (the march's phase and albedo), `fine_march_transmittance` (the finest march) and
`single_scattering` (no octaves against one vertex) holds:

| Field | Meaning |
|---|---|
| `raster_radiance`, `pt_radiance` | Mean luminance of the in-scattered light, cd/m². |
| `radiance_log2` | `log2(raster / traced)`: stops, negative is "the raster is darker". |
| `radiance_block_rms_log2`, `radiance_block_correlation` | The same over 8-pixel blocks with cloud in them. |
| `raster_transmittance`, `pt_transmittance` | Mean view transmittance (`sky · a + rgb`). |
| `transmittance_block_rms`, `transmittance_block_correlation` | The same block by block: how alike the two densities are. |
| `tint_red_over_blue` | Red over blue of the raster's light, over that of the traced light. 1 is the same tint. |
| `cloudy_blocks` | Blocks with a traced transmittance under 0.9. |

`"kind": "ground"` lines: the ground's light relative to clear, over a 12 km footprint.

| Field | Meaning |
|---|---|
| `raster_sun`, `pt_sun_direct`, `sun_direct_block_*` | The raster's cloud transmittance map against the traced direct beam: both are the same Beer–Lambert term. |
| `pt_sun_all` | The traced sun with every scattering order: what really reaches the ground. |
| `raster_sky`, `pt_sky_all` | The raster's sky-light multiplier (`1 − 0.6 · overcast`, ADR-321) against the traced sky. |
| `raster_total`, `pt_total`, `total_log2` | Both together by their clear shares; stops, negative is "the raster's ground is darker". |
| `raster_screen_shade`, `screen_log2` | The ground of a default frame (`shadow_map` off): the screen-space shadow's whole-pixel multiply. |

`--terms` prints `"kind": "terms"` lines instead: the view's in-scattered light by source, cd/m², which says which term
of the march differs. `raster_ambient` (the ambient row alone), `raster_sun_single` (the sun's first octave, diffusion
off), `raster_sun_full` (the sun with the march's multiple scattering); `pt_sky_raster_phase`, `pt_sky_droplet`,
`pt_sun_single_raster_phase` (one path vertex), `pt_sun_all_raster_phase`, `pt_sun_all_droplet`.

`regia_pt::cloud_compare` is the library surface: `CloudCase`, `standard_cases`, `CompareLight`, `compare_scene`,
`compare_view`, `compare_ground`, `light_terms`, `CloudLightTerms`, `raster_view`, `traced_view`, `measure_view`, `RasterMarch`, `CloudTraceSize`,
`CloudViewMeasure`, `GroundLightMeasure`, `physical_field`, `raster_parameters_field`. What the numbers were on the
shipped settings, and which parts of the march they point at, is ADR-2127. In short: the densities agree (view
transmittance correlation 0.9999), and the light terms differ. The march's multiple-scattering stand-in falls short
on thin, broken cloud (the sun's in-scatter is about 2.9 stops under the trace on the default layer, about 0.4 on
thick cumulus). Single scattering is about 0.8 stops under the trace. The ambient row is about 2 stops over the
traced sky light and hides part of that gap in the totals.

## The slab

ADR-1834: the shadow map's sun term (ADR-320) and the overcast sky light (ADR-321) on a uniform layer.

### The layer

`regia_pt::cloud::PtCloudLayer` is a horizontal slab of medium between a base and a top height: constant extinction,
single-scattering albedo 1 (water does not absorb visible light), Henyey–Greenstein asymmetry 0.85. It becomes one of
the tracer's medium layers (`add_to(&mut PtScene)`, at most 8 layers in a scene), so the sun, the sky, the ground and
the slab exchange light through any number of scattering events.

- `PtCloudLayer::overcast(base_m, top_m, optical_depth)`: a slab of a given vertical optical depth.
- `PtCloudLayer::from_settings(&CloudSettings, fill)`: the fully covered column the renderer's settings describe
  (`density × thickness × mean height profile × fill`). The runtime assumes `fill = 0.05` (`COLUMN_FILL`).
- A slab is horizontally uniform: it is the covered column. A broken sky is the coverage-weighted mix of the clear and
  the covered result (`coverage_mix`).

### The command

```text
cargo run --release -p regia_pt --example cloud_validation -- \
    [--tau 0.5,2,5,10] [--sun-y 0.3,0.6,0.9] [--spp 64] [--fill 0.05]
```

It needs a GPU adapter (lavapipe is enough) and prints one JSON object per line. Per optical depth and sun height
(`sun_y` is the sine of the sun's elevation):

| Field | Meaning |
|---|---|
| `pt_ratio`, `pt_sun_ratio`, `pt_sky_ratio` | Traced ground light under the slab ÷ clear, with sun and sky, the sun alone, the sky alone. |
| `reference_ratio` | The same from a one-dimensional Monte Carlo transport reference (exact for any depth). |
| `renderer_ratio`, `renderer_sun_ratio`, `renderer_sky_ratio` | What the real-time model gives (the shadow-map march; `1 − 0.6 × opacity` for the sky). |
| `renderer_log2_gap` | `log2(renderer ÷ reference)`: negative is "the renderer lights the ground less", in stops. |
| `beam_transmittance`, `two_stream_diffuse` | The unscattered beam (Beer–Lambert) and the two-stream diffuse limit. |

`--fill F` adds a `settings` line with the optical depth the default `CloudSettings` give at that fill.

### What to expect

The traced ratios agree with the reference to about 0.05 up to optical depth 10; beyond that a small trace is too noisy
(use `reference_ratio`). The real-time model is darker than both: Beer–Lambert removes the light a cloud scatters
forward. See ADR-1834 for the table.

### Library surface

`regia_pt::cloud`: `PtCloudLayer`, `CloudLight`, `measure_ground_ratio`, `validation_scene`,
`slab_transport_reference`, `reference_ground_ratio`, `renderer_sun_ratio`, `renderer_sky_ratio`,
`renderer_ground_ratio`, `renderer_log2_gap`, `two_stream_diffuse_transmittance`, `suggested_max_bounces`.
`PtSettings::max_bounces` may now go up to `MAX_PATH_VERTICES` (1024; it was clamped to 64): a thick slab needs
hundreds of scattering vertices.

## Error codes

The examples exit with status 1 and a message on stderr (`cloud_compare: <reason>`, `cloud_validation: <reason>`) for a
bad argument or a missing adapter. `seeing.reference_trace` answers an invalid-params error for an unknown `clouds` name
or `clouds` on a named scene; no new error code.
