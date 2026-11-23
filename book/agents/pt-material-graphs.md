# Material graphs and subsurface in the reference tracer

The reference path tracer (`regia_pt`) shades a material graph with the same generated `material_surface` function
the forward pass runs, and traces a material's subsurface scattering with Burley's normalized diffusion
(ADR-2725). It is a validation tool, not a runtime feature: `regia_pt` is never part of a game build.

## Graphs

A frame traced with `regia_pt::extract::from_frame` (or `from_extracted`, which `seeing.reference_trace` uses) brings
every graph material along: `PtScene::surface_graphs` holds, per material, the surface text the forward pass splices,
the parameter block the frame uploads and the graph textures. The tracer composes the scene's graphs into its kernels
(one kernel set per set of graphs, cached on the `PathTracer`) and, at a hit on a graph material, takes the graph's
base colour, metallic, roughness, normal, emission and unlit flag. Masked graph materials take the graph's alpha in the
alpha test. Lighting is the tracer's own.

To freeze an animated graph the same way on both sides, pass the frame's material clock:

<!-- ignore-reason: a fragment; `renderer` and `frame` are the host's FrameRenderer and FrameScene -->
```rust,ignore
renderer.set_material_clock(2.5, 2.5);
let scene = regia_pt::extract::from_frame(&frame, &ExtractOptions {
    material_clock: renderer.material_clock(),
    origin_offset: renderer.origin_offset(), // a frame with a floating origin
    ..ExtractOptions::default()
});
```

### What the tracer cannot evaluate

A closest hit has no pixel quad and the tracer's textures have no mip chain. Every graph texture sample is the bilinear
top level (an explicit level is ignored), as for standard materials. These are **refused by name**:

| Construct | Why |
|---|---|
| `dpdx`, `dpdy`, `fwidth` and their `Fine`/`Coarse` forms | screen-space derivatives |
| `textureSampleBias`, `textureSampleGrad`, `textureSampleCompare*`, `textureGather*`, `textureLoad`, `textureDimensions`, `textureNumLevels` | sampling the tracer does not have |
| `discard` | a compute kernel cannot discard |
| `frame` (other than the parallax gate `frame.pcss.z`, read as on), `material` (other than `material.graph[i]`) | no raster uniforms in the tracer |
| `material_vertex_offset` | a vertex stage: the tracer traces the undisplaced mesh |
| `second_uv` | the tracer's meshes carry one UV set |
| `compose` | the graph did not compose on its own (the message says why) |

A refused graph's material traces with its standard factors. The refusal is never silent: it is listed in the
image's metadata.

### From an agent

`seeing.reference_trace` returns the trace's `metadata`; a scene with graph materials adds `surface_graphs`:

```json
"surface_graphs": {
  "graphs": 2,
  "materials": 2,
  "refusals": [
    { "material": 1, "construct": "fwidth", "reason": "screen-space derivatives: a compute closest hit has no pixel quad" }
  ]
}
```

A scene without graph materials has no `surface_graphs` key. `seeing.compare` then compares the traced EXR with the
frame as for any other scene; a graph material that was refused is the one place the two are not the same material,
so check `refusals` before reading a § 2.2 number.

## Subsurface

`PtScene::subsurface` (`PtSubsurface { material, d, tint }`) scatters a material's diffuse light under the surface:
`d` is Burley's profile shape per channel in meters, `tint` the share that scatters (the rest stays where it landed,
as in the frame's screen-space pass). `ExtractOptions::subsurface = true` maps each `LabMaterial::subsurface` to a
profile with the same mean-square reach as the frame's sum of Gaussians (`regia_pt::extract::burley_d`). It is off by
default: the screen-space comparisons (`tests/subsurface_parity.rs`) trace the light that lands and apply the profile
themselves.

From an agent, `seeing.reference_trace { "scene": "current", "subsurface": true }` traces the frame's materials with
their subsurface on (`invalid_params` for another scene or a value that is not a boolean). `scene_info` counts
`surface_graphs` and `subsurface_materials`. The frame's material clock is taken from the renderer, so a
time-reading graph is traced at the frame's time.

The profile reaches along the surface by probe rays through the instance. Where the surface is thinner or more curved
than the profile's reach a probe can leave the instance; that light is lost (the image is a little dark there).

## Limits

- Probe and lightmap bakes use the base kernels: graph materials bake with their factors.
- A graph texture held only on the GPU (a dynamic texture) samples white, with a warning in the log.
- Graph emission is seen by rays that hit it; no light is sampled toward it.
