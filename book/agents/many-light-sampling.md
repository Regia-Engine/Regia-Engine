# Stochastic many-light sampling (agents)

With hundreds of local lights a pixel's froxel list can hold up to 256 lights, and the forward pass
shades every one. Stochastic many-light sampling (ADR-2470) shades **one** light per pixel per frame
instead, picked so that the expected value of the result equals the sum over every light:

1. **Candidates.** Each pixel draws a few lights (16 by default) uniformly from its cluster list and
   keeps one by resampled importance sampling against a *target*: the light's unshadowed shaded
   luminance at the pixel (plus a small irradiance term so every light that reaches the surface
   stays possible).
2. **Temporal reuse.** The pixel reads last frame's pick where its surface was (along the motion
   vector) and combines it with this frame's, unless the surface moved more than a tenth of its view
   depth or its normal turned (the same rules the GI denoiser uses, ADR-864). Revealed pixels start
   over.
3. **Spatial reuse.** It also combines the picks of up to 8 neighbors within 16 pixels, skipping
   neighbors across depth or normal edges.
4. **Shading.** The kept light is shaded and weighted; shadows are tested for that light only.

This follows Bitterli et al., "Spatiotemporal reservoir resampling for real-time ray tracing with
dynamic direct lighting" (SIGGRAPH 2020), with its unbiased normalization. It is meant for Desktop
Ultra scenes with very many lights. It is off in every preset: `mode: "production"` turns it on for
the frame (below), `mode: "debug"` draws the evaluation view.

## Production (the frame's own lights)

`render.set_many_light_sampling { mode: "production" }` turns on `RenderProfile::many_light_sampling`
(ADR-2652) and removes the evaluation pass. From the next frame the built-in step
`many_light_sampling` (after the sun trace in `render.graph`) rebuilds the sampler's inputs from the
prepass depth and runs the reservoir passes over the frame's **own** light buffer and cluster lists;
the opaque forward pass (and the GPU-driven batches) then shade the pixel's chosen point or spot light,
weighted, in place of looping over the list's points and spots. Fitted emitters, the sun, GI, probes
and image light are unchanged. Blended, OIT and glass surfaces, lightmap receivers, thin-film,
subsurface and material-graph pipelines, and the visibility-buffer resolve keep the full loop (they are
not the depth surface the reservoirs were built for). The light is shadowed by its shadow-atlas tile as
the loop would. The image is noisier per frame and converges over a few frames under TAA/TSR; a camera
cut, a resize or a changed light list (a light appearing, leaving or changing kind, colour, intensity
or range) restarts the history.

It samples single-sampled linear-HDR main views with at least one point or spot light; otherwise the
frame keeps the loop and `render.many_light_stats` says why (`production.skipped`): `off` (the profile
does not ask), `multisampled`, `not_linear_hdr`, `no_lights`, `no_binding` (a device without room for
the reservoir binding, and Android) or `failed` (`production.last_error`). Texture-view frames keep the
loop and leave the main view's history alone. `mode: "off"` turns the profile field off again;
`mode: "debug"` turns it off and registers the evaluation pass. The sampler settings in the request
apply to both. `render.set_profile` with a preset replaces the field (every preset has it off).

## Evaluation pass

`render.set_many_light_sampling { mode: "debug" }` registers the pass `restir_di_debug` at the
frame's `after_opaque` slot (see [Frame graph and host passes](render-graph.md)). It rebuilds a
G-buffer from the scene depth (normals from neighboring depths, one flat gray material, camera
motion), takes a **snapshot** of the world's `PointLight` and `SpotLight` entities, and draws the
sampler's estimate over the view. With `view: "reference"` it draws the brute-force sum over each
pixel's cluster list instead, so the two can be compared with screenshots. It replaces what the
frame drew there: it is a look at the sampler, not lighting. Set `debug` again after moving lights;
`mode: "off"` removes the pass. Multisampled frames skip it (the pass row shows the error).

## Methods

| Method | Permission | What it does |
|---|---|---|
| `render.set_many_light_sampling { mode, view?, initial_candidates?, temporal?, spatial_taps?, spatial_radius_px?, trace_steps? }` | edit scene | Turns the evaluation pass on (`debug`) or off (`off`) and sets the sampler. |
| `render.many_light_stats { reference?, lights?, seeds?, frames? }` | read | Mode, view, lights fed, settings, the pass row and counters; with `reference: true` a CPU check against brute force. |

### `render.set_many_light_sampling`

| Param | Type | Default | Meaning |
|---|---|---|---|
| `mode` | string | required | `"off"`, `"debug"` or `"production"`. |
| `view` | string | `"estimate"` | `"estimate"` or `"reference"`. |
| `initial_candidates` | integer | 16 | Candidates per pixel per frame, 1-64. |
| `temporal` | boolean | true | Reuse last frame's pick. |
| `spatial_taps` | integer | 4 | Neighbor picks combined, 0-8. |
| `spatial_radius_px` | integer | 16 | Neighborhood radius in pixels, 1-64. |
| `trace_steps` | integer | 0 | Screen-space shadow steps toward the kept light, 0-64 (0 = unshadowed). |

Returns `{ mode, view, pass_changed, profile_changed, lights, settings, note }` (`profile_changed`:
the request turned `many_light_sampling` on or off). A settings change restarts the history. Errors
(`data.reason`): `no_renderer`, `no_render_settings` (`production` without a `RenderSettings`
resource), `unknown_field`, `invalid_value`.

### `render.many_light_stats`

Returns `{ mode, view, lights_fed, settings, pass, production, counters, reference }`:

- `production`: the frame's sampler (`null` without a runtime renderer): `profile` (the field is
  on), `active` (the last main-view frame sampled), `skipped` (why not, `null` when active), `frames`
  sampled, `resets` of the history, `resource_builds` and `bind_group_builds`, `lights` and
  `width`/`height` of the last sampled frame, `last_error`.

- `pass`: the frame-graph row (`registered`, `runs`, `failures`, `last_frame`, `last_error`,
  `last_cpu_ms`), `null` without a runtime renderer.
- `counters`: `frames_recorded`, `frames_skipped`, `light_uploads`, `cluster_builds`,
  `resource_builds`, `last_lights`, `last_width`, `last_height`, `last_cluster_max`,
  `last_cluster_overflowed` (clusters whose lights did not all fit; their extra lights are never
  picked, the same loss the forward pass has).
- `reference` (with `reference: true`, `null` otherwise): the CPU reference on a synthetic
  48 × 27 scene with `lights` lights (1-2000, default 1000) over `seeds` independent seeds (2-64,
  default 32) of `frames` frames (1-8, default 6): `truth_mean` (brute force), `estimate_mean`,
  `relative_bias`, `bias_sigma` (the bias in standard errors of the seed mean; `unbiased` is
  `bias_sigma < 4`), `rmse_uniform` (one uniform light per pixel), `rmse_candidates` (resampling
  without reuse), `rmse_restir` (the full sampler at its last frame), `variance_reduction`
  (`(rmse_uniform / rmse_restir)²`) and `convergence` (relative RMSE per frame, mean over seeds).
  The run takes a moment: it is a heavy call.

## Example

```json
{"method": "render.many_light_stats", "params": {"reference": true, "seeds": 32}}
{"method": "render.set_many_light_sampling", "params": {"mode": "debug", "view": "reference"}}
{"method": "seeing.screenshot", "params": {}}
{"method": "render.set_many_light_sampling", "params": {"mode": "debug", "view": "estimate"}}
{"method": "seeing.screenshot", "params": {}}
{"method": "render.set_many_light_sampling", "params": {"mode": "off"}}
```

Production, then a check that the frames sample (`production.active` is `true` once a frame ran):

```json
{"method": "render.set_many_light_sampling", "params": {"mode": "production"}}
{"method": "render.many_light_stats", "params": {}}
{"method": "seeing.screenshot", "params": {}}
{"method": "render.set_many_light_sampling", "params": {"mode": "off"}}
```

## Errors

`data.reason`: `no_renderer` (F0003: the app has no runtime renderer), `no_render_settings` (F0003:
`production` without a `RenderSettings` resource), `unknown_field`, `invalid_value` (F0002: a value
out of range, a bad `mode` or `view`).
