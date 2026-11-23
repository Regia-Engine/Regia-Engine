# Frame AOVs: normal and roughness

The forward frame has no G-buffer. `seeing.frame_aov` reads its shading normal and roughness anyway (ADR-2951). It
renders the extracted frame twice more through the forward shader's own debug outputs and returns one of them as a
PNG, with statistics. Use it to check a surface's normal map, roughness or metallic as the lights saw them, and to
score the frame's normal and roughness against the reference trace's AOVs (`seeing.reference_trace`, the tracer's
`normal` AOV and `PtImage::roughness`).

```json
seeing.frame_aov { "width": 640, "height": 360, "aov": "normal", "rect": [300, 160, 340, 200] }
→ { "width": 640, "height": 360, "aov": "normal", "covered": 1600,
    "normal_mean": [0.29, 0.96, 0.0], "roughness": { "mean": 0.8, "min": 0.796, "max": 0.804 },
    "metallic_mean": 0.0, "rect": [300, 160, 340, 200], "mime": "image/png", "png_base64": "…" }
```

## Parameters

- `width`, `height`: the capture size, 16 to 4096 (default 640 × 360).
- `aov`: `"normal"` (default) or `"roughness"`. This picks which AOV the PNG shows. The statistics cover both.
- `rect`: `[x0, y0, x1, y1)` in capture pixels. Statistics are taken over the covered pixels inside it, or over the
  whole frame without it.

## What the result holds

- **The PNG.** The normal is shown as `n * 0.5 + 0.5` and the roughness as grey. Pixels the forward pass did not
  shade (sky, background) are transparent.
- **`covered`**: the number of shaded pixels in the rect.
- **`normal_mean`**: their mean world normal, normalized.
- **`roughness`**: the `{ mean, min, max }` of their perceptual roughness.
- **`metallic_mean`**: their mean metallic.

## How it works, and what it leaves out

The capture sets `FrameQuality::shadow_debug` for two frames on the runtime renderer:

- **`5`** writes the shading normal the frame's lights used. That includes the normal map, the detail normal and
  any material input the forward pass applies.
- **`7`** writes the perceptual roughness and metallic the frame shaded with.

Both outputs skip the post stack. The debug branch writes them with its own 2.2-power encode into the 8-bit
offscreen target, and the capture decodes the same power. Normal error is about 0.5°. Roughness error is about 1/255
near 0 and 1/115 near 1. Translucent layers (particles and the projected decal
pass) sit out the two frames, because they would composite over the debug output as if it were a colour. On the
lite forward tier (Mobile profiles on tile GPUs, ADR-1874) the two frames force the full forward shader: the lite
shader has no debug outputs, and its normal and roughness inputs are the ones every tier shades with. The
renderer's tier choice is restored after the capture. The capture is for tests and agents, never for game frames. The renderer's frame index advances by two, so compare
later frames with a renderer at the same frame number. The temporal history (TAA, temporal GI and denoisers) also
sees the two debug frames, so a live game profile with temporal effects can show a few frames of ghosting after a
call. Capture on a paused scene, or let the history settle for a few frames before reading beauty frames. From Rust, call `regia_render::capture_frame_aov`, which
returns a `FrameAov`.

**Decals.** The projected decal is `regia_vfx`'s layer, composited after the forward pass. It relights the lit
colour from its own normal and adds a roughness-shaped lobe, but the forward pass never shades with that normal or
roughness. So on a decal's footprint this AOV holds the receiver's values, not the decal's. Material-mask decals
(`MaskDecals`) change only base colour and emission, so the AOV is unchanged by them as well. The spec 27 suite's
`decal_aovs` row records the projected-decal case as a fail, with numbers ([Spec 27 calibration](spec27-calibration.md)).
The reference trace applies decals to its material inputs, so its AOVs do hold them ([Decals](decals.md)).

**Terrain.** Native terrain draws with its own shader, which does not read `shadow_debug`. Its pixels come back
uncovered. For the terrain's shading normal, use `TerrainSharedDraw::render_normal_view` (terrain debug view 16).
The spec 27 `terrain_normal` row does this.

## Errors

| Code | Meaning | Fix |
|---|---|---|
| `frame_aov_no_device` | The app has no render device. | Run it in an app with `regia_render::RenderPlugin`. |
| `frame_aov_no_frame` | The app has no runtime renderer, or no extracted frame to draw. | Step one frame (`play.step`) and call again. |
| `frame_aov_render_failed` | One of the two frames failed on the GPU. | Check the log for the GPU error. |

Bad parameters (an unknown `aov`, a size outside 16 to 4096, a `rect` outside the capture) are `INVALID_PARAMS`.
