# Forward decals: lit decal normals and roughness

Projected decals normally composite after the forward pass. They relight the finished colour from
their own normal and add a highlight shaped by their roughness, so the lights never see the decal's
normal or roughness. Forward decals (ADR-3255) change the surface itself. After the material is
sampled and before any light, the forward shader blends the decals of the pixel's screen tile into
the surface: albedo (modulate or replace), normal, roughness and metallic. The pixel is then lit
once, with the same BRDF as everything else. A normal-mapped decal catches a grazing light the way
its bumps predict, a rough decal on a glossy floor removes the highlight under it, and the frame's
normal and roughness AOVs (`seeing.frame_aov`) read the decal's values.

## Turning it on

The path is off by default in every preset.

- **Profile.** `RenderProfile::forward_decals = true`. `RenderSettings::preset()` still reports the
  preset.
- **Environment.** `REGIA_FORWARD_DECALS=1` before the first frame.
- **Host.** `regia_render::set_process_forward_decals(true)` before the first renderer.

The switch is latched for the process on its first read, so the frame's binding layout and every
forward shader agree. With it off, frames are unchanged. With it on and no decals, the forward
shader skips the gather on one uniform test. A decal-free frame is byte-identical either way (test
`a_decal_free_frame_is_byte_identical_with_the_switch_on_and_off`).

## Which decals go forward

`regia_vfx::route_decals(decals, surfaces, switch)` gives every decal a path:

| Path | When | What draws it |
|---|---|---|
| `post` | switch off; `shading: "post"`; emissive-only; terrain-only mask | the projected decal pass, as before |
| `forward` | the decal writes albedo, normal, roughness or metallic, on mesh receivers only | the forward pass |
| `split` | as `forward`, and the mask has terrain or the decal has emission | the forward pass on meshes; the projected pass for terrain (all channels) and for emission on meshes |

The forward pass gives a rigid mesh receiver layer 1 and a skinned mesh layer 2. A world decal
(mask 3) therefore never paints a character. Explicit receiver-stamp layers are not seen by the
forward pass. The terrain shader has no forward gather yet, so terrain keeps the projected pass.

A game that uses the VFX runtime (`VfxRuntimePlugin`) needs nothing more: each frame the runtime
routes its pool's decals and hands the forward copies and the forward atlas to the frame through its
translucent layer (`TranslucentLayer::forward_decal_feed`). The runtime learns from the frame whether
the device has the forward binding, so the first frame after start-up, Android and a switch-off
process all keep every decal on the projected pass.

A host that owns its own `VfxGpu` hands the forward list to
`FrameRenderer::set_forward_decals(&route.forward)` and the atlas once to
`FrameRenderer::set_forward_decal_atlas(&regia_vfx::forward_atlas())`. The post lists go to
`VfxGpu::write_decals_with(&route.post, &route.post_surfaces)`.

## Prefabs

`.decal.ron` v2 adds two fields: `metallic` (0..1, written with the `metallic` channel) and `shading`
(`auto` or `post`). A v1 file loads as v2 with the same meaning; `regia upgrade-project` rewrites it
in place and keeps its comments.

```ron
(
    version: 2,
    name: "brass_plate",
    half_size: (0.4, 0.4, 0.2),
    albedo: "#b08d57",
    roughness: 0.3,
    metallic: 1.0,
    channels: "albedo+roughness+metallic",
    blend: "replace",
    shading: "auto",
)
```

The atlas has three pages per layer: albedo with coverage, a normal page cooked from the layer's
luma, and roughness/metallic multipliers.

## What the agent sees

`vfx.decal_budget` reports the path of every live decal and, with a `VfxView`, the forward pass's
tile counts for that view:

```json
{}
→ { "capacity": 256, "live": 2, "culled": 0, "tiles_touched": 6,
    "shading": {
      "forward_switch": true,
      "paths": ["split", "post"],
      "post": 1, "forward": 0, "split": 1,
      "forward_max": 64,
      "forward_tiles": { "packed": 1, "dropped": 0, "culled": 0, "tile_px": 32,
                         "tiles_x": 60, "tiles_y": 34, "tiles_touched": 12,
                         "max_per_tile": 1, "tile_entries": 12 } } }
```

`forward_tiles` is `null` without a view or without forward decals. `FrameRenderer::forward_decal_stats()`
returns the same counts for the last frame the renderer drew, or `None` when the device has no
forward-decal binding.

## Limits and cost

- At most 64 forward decals per frame (`forward_max`). Past that, the lowest priority are dropped
  and counted in `dropped`.
- Tiles are 32 px. They double on very large views, so the grid fits 8192 tiles.
- Per pixel on desktop: one 8-byte mask read. Each decal in the tile costs 64 bytes for the box
  test. Each decal that covers the pixel costs 80 bytes more, plus up to three page samples of four
  reads each.
- **Mobile.** Android keeps the projected pass on every tier (Mobile Low and High). The lite
  forward shader has no gather.
- Another view of the same frame, such as a probe face or a render-to-texture view, tests every
  decal instead of reading the tiles.

## Checking it

| Test | What it proves |
|---|---|
| `regia_render --test forward_decals` | A normal decal under a grazing sun brightens or darkens the wall as its normal predicts, and the AOV reads it. A rough decal shades byte for byte like a wall of that roughness. A decal-free frame is byte-identical with the switch on and off. |
| `regia_render --lib forward_decals` | Tile masks, priority order, culling, overflow, the near-plane case, the CPU blend oracle and the latch. |
| `regia_vfx --lib decal_forward` | The routing table above. |
| `regia_pt --test spec27_calibration spec27_decals` | Run with `REGIA_FORWARD_DECALS=1`, this scores the spec 27 decal rows on the forward path. Run without it, it scores the projected pass and splits that pass's brightening by channel. |
