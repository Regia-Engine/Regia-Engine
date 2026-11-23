# Frame generation (FSR 3)

Frame generation presents one interpolated frame between every two rendered frames, so the
display shows about twice the rendered frame rate. It is a WGSL port of the frame interpolation and
optical flow shaders of the AMD FidelityFX SDK v1.1.4 (MIT, the same tag as the FSR 3.1 upscaler)
with Regia Engine's own pacing on its wgpu surfaces (ADR-514, ADR-2067). No vendor binary is
involved.

It needs the cargo feature `frame-gen` (`regia/frame-gen` or `regia_render/frame-gen`), a window and a
desktop GPU (Vulkan, DX12 or Metal with compute), and it stays off until `enabled` is set. Each
rendered frame reaches the screen half a frame later than it would without it; below about 55
rendered frames per second the artifacts stay on screen longer and the pacer turns it off.

## What runs

1. The scene renders into frame generation's own image instead of the swap chain.
2. **Prepare inputs**: dilated depth and motion and the reconstructed previous depth, from the scene
   depth, the velocity target and the camera. It runs behind any upscaler, or none.
3. **Optical flow**: a luma pyramid of the presented image, a scene-change test on section
   histograms, then per level a block search, a middle-vector filter and a 2x upscale.
4. **Interpolator**: game and optical flow vector fields at the middle of the two frames, their
   inpainting pyramid, the disocclusion mask, the interpolated colour, inpainting.
5. **Present**: the generated image with the UI overlay drawn over it, then half an interval later the
   rendered image with the overlay. The overlay is never interpolated. A worker thread presents at the
   pacer's times, so the next frame starts rendering while the rendered image waits for its turn.

A camera cut, a resize (the surface is reconfigured or the scene images change size), a pause of
over half a second, a missing input, or the first tick after frame generation turns on sets the
pacer's reset. That tick presents the rendered frame only.

A scene change the histograms detect does not set that reset. Five or more of the nine luma
sections differing by more than 0.5 sets a change bit. The low four bits of that history stay set
for four rendered frames, and each of those frames forces the interpolator's frame counter back to
0, so the generated image is a copy of the current frame. The pacer still schedules both presents,
so each of those four rendered frames is shown twice.

Any error presents the rendered frame plainly, reports it in `pacing.present`, and keeps frame
generation off until it is turned off and on again.

## `render.frame_gen`

No params.

```json
{ "method": "render.frame_gen" }
```

Returns:

- `settings`: `enabled`, `hud` (`redraw`, `ui_texture` or `hud_less`), `min_base_fps`,
  `debug_tear_lines`, `debug_view`.
- `compiled`: the build has the `frame-gen` feature.
- `can_enable`: `true` once `blockers` is empty.
- `active`: generated frames are being presented now.
- `blockers`, most fundamental first:

  | Code | Meaning |
  |---|---|
  | `not_compiled` | The build lacks the cargo feature `frame-gen`. |
  | `interpolator_not_ported` | The interpolation passes are missing (no current build). |
  | `headless` | The app has no window surfaces. |
  | `needs_desktop_gpu` | The device lacks compute with storage textures or storage buffers (the passes bind 4 storage textures and 5 storage buffers per stage), or runs on GL. |

- `pacing`: `null` without the feature or without a window. Otherwise:
  - `base_fps`, `average_interval_ms`: the moving average of the rendered frame interval (eight
    frames; a hitch counts as at most twice the average).
  - `added_latency_ms`: how much later each rendered frame reaches the screen (half an interval: it
    waits behind the generated one). The interpolation's GPU time comes on top.
  - `last`: `{ kind: "generate", interval_ms }` or `{ kind: "real_only", reason }`, where
    `reason` is `disabled`, `warmup`, `reset` or `base_rate_too_low`. After the rate drops under the
    floor the reason stays `base_rate_too_low` for a hold-off of 120 rendered frames, doubling on each
    drop up to 3840: on a vsync swap chain two presents per tick halve the rendered frame rate, which
    can cross the floor by itself.
  - `present`: what the present path did since start:

    | Field | Meaning |
    |---|---|
    | `generated_presents` | Generated images presented. |
    | `real_presents` | Rendered images presented through this path. |
    | `resets` | Frames that started a new history. |
    | `acquire_failures` | Swap-chain images it could not get (that tick shows less). |
    | `fallback` | Why an enabled frame presented plainly (table below), or `null`. |
    | `last_error` | The error behind `fallback: "failed"`, or `null`. |
    | `gaps` | The spacing of the presents since frame generation started: `count`, `mean_ms`, `stddev_ms`, `max_ms` (gaps over 250 ms do not count). Even pacing has a small `stddev_ms` against `mean_ms`. |

  - `stats`: `real_frames`, `generated`, and one `skipped_*` counter per reason.
- `hud_modes`: the accepted `hud` values.
- `hud_effective`: what this build does with `hud`: `redraw` in every mode today (below).
- `optical_flow`: `block_size` (8), `search_radius` (8) and `max_levels` (7) of the estimator.

`fallback` codes:

| Code | Meaning |
|---|---|
| `blocked` | A code in `blockers` applies (a device without compute, for example). |
| `multiple_windows` | More than one window is open: frame generation runs on one. |
| `multiple_views` | The window shows more than one view, or one that does not fill it. |
| `splash` | The boot splash is up. |
| `no_scene` | There is no scene to interpolate. |
| `failed` | An error stopped frame generation; turn it off and on again to retry. |

## `render.set_frame_gen`

Params, all optional:

| Param | Type | Meaning |
|---|---|---|
| `enabled` | boolean | Present an interpolated frame between every two rendered frames. |
| `hud` | string | `redraw` (the default: the UI overlay is drawn again over every presented frame). `ui_texture` and `hud_less` are accepted and behave as `redraw`: the overlay never enters the interpolation, so there is nothing for them to keep out. |
| `min_base_fps` | number, 10 to 240 | Below this rendered frame rate, only rendered frames are presented (default 55). |
| `debug_tear_lines` | boolean | Moving bars at the edges of generated frames, to see that they are presented and whether they tear. |
| `debug_view` | boolean | Present only generated frames, with the interpolator's surfaces in four panes: game vectors top left, optical flow vectors top right, disocclusion mask bottom left (red = visible in the previous frame, green = in the current), interpolated colour bottom right with inpainted texels in magenta. |

```json
{ "method": "render.set_frame_gen", "params": { "min_base_fps": 50 } }
{ "method": "render.set_frame_gen", "params": { "enabled": true } }
```

Returns `{ before, after }` (the settings). Every parameter is checked before anything changes.

Errors (`data.error`):

| Code | JSON-RPC | Regia Engine | When |
|---|---|---|---|
| `frame_gen_unavailable` | `UNSUPPORTED` | `F0013` | `enabled: true` while `blockers` is not empty; `data.blockers` lists them. Nothing changes, including the other params of the call. |
| `unknown_hud_mode` | `INVALID_PARAMS` | `F0002` | `hud` is not one of the modes. |
| `out_of_range` | `INVALID_PARAMS` | `F0002` | `min_base_fps` is outside 10 to 240. |

A param of the wrong JSON type is `INVALID_PARAMS` (`F0002`) without a `data.error`.

## Check it worked

1. `render.frame_gen` on a build with `frame-gen`: `can_enable` is `true` and `pacing.base_fps` is
   close to the frame rate the performance overlay shows. `pacing.last.reason` is `disabled`.
2. `render.set_frame_gen { enabled: true }`, wait a second, then `render.frame_gen`: `active` is
   `true`, `pacing.last.kind` is `generate`, and `pacing.present.generated_presents` and
   `real_presents` grow together. If `active` stays `false`, read `pacing.last.reason` and
   `pacing.present.fallback`.
3. `render.set_frame_gen { debug_tear_lines: true }`: bars at the left and right edges flicker through
   colors; a capture of a real frame has none, because screenshots show the rendered frame.
4. `render.set_frame_gen { debug_view: true }` and take a screenshot: it shows the four panes.
5. Move the camera with `min_base_fps` above your frame rate: `pacing.last.reason` becomes
   `base_rate_too_low` and `generated_presents` stops growing.

## Present modes

A vsync swap chain shows every present on its own refresh, so two presents per tick would cap the
rendered frame rate at half the refresh. While frame generation runs, a vsync window uses Mailbox when
the surface offers it (it does not tear either) and goes back to the host's mode afterward;
`render.swapchain` reports the mode in effect. A surface with no Mailbox stays on vsync: a 60 Hz display
then renders at 30, under the default floor, and `pacing.last.reason` says `base_rate_too_low`.

## Conventions

These are fixed by the CPU references and their tests (`regia_render::frame_gen`):

- Motion vectors are in UV units of the render target and point from the current position to the
  previous one (`previous uv − current uv`). The frame at time `t` (0 = previous, 1 = current)
  sees a surface at `current + vector · (1 − t)`; one generated frame uses `t = 0.5`.
- Depth is standard (0 near, 1 far) with a finite far plane.
- The optical flow field has one vector per 8 × 8 block, in full-resolution pixels, pointing to
  the previous position like the game motion vectors. The GPU field equals
  `optical_flow::estimate_flow` vector for vector.
- On a camera cut or history reset, the generated frame is the rendered frame (nothing to
  interpolate from), and the pacer presents the rendered frame only.
