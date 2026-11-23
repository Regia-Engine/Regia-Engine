# Texture table growth and residency feedback (bindless step 5)

On the texture table path (`REGIA_BINDLESS_LEVEL=3`, ADR-863) every standard material samples
its textures through one per-device table. Step 5 (ADR-1110) adds three things to it:

- a **growth policy**: how much of the declared table each rebuild binds;
- **stable indices under streaming**: a residency swap keeps the texture's table index;
- **GPU residency feedback** (opt-in): the GPU reports the texel need it actually sampled, per
  texture, beside the CPU's estimate.

`render.texture_residency` reports all three. Nothing here changes pixels: every check compares
against the per-material path byte for byte.

## Growth policy

The table's size is fixed when the device is created: the shaders are compiled with it
(`binding_array<texture_2d<f32>, N>`), so growing it would recompile every table pipeline. What
grows is the part each rebuilt table group binds.

- On devices with `PARTIALLY_BOUND_BINDING_ARRAY` (Vulkan and DX12; requested automatically at
  level 3 when the adapter has it), a rebuild binds a power-of-two prefix: at least 256 views,
  growing to the next power of two when a new texture's index passes it. It shrinks only when
  the scene falls to a quarter of it, and then only to twice what the scene needs, so a scene
  that loads and unloads around a boundary does not change length on every rebuild.
- Every other device (Metal has no partially bound arrays) binds the whole table, as before.
- Indices are handed out lowest first, and freeing the top index lowers the high-water mark, so
  the live textures stay in a compact prefix.
- The declared size grows only across launches: when the table overflows, the `texture_table_full`
  problem (`render.problems`) names the `REGIA_TEXTURE_TABLE_VIEWS` value that would hold every
  texture.
- The policy never adds a rebuild: the length changes only on a rebuild that a changed texture
  forced anyway. A rebuild's CPU cost follows the bound length, so `views_written` (descriptor
  writes over all rebuilds) is the deterministic cost measure; `last_rebuild_us` on
  `render.get_frame_stats` is the measured one.

## Residency swaps keep their index

When streaming swaps a material's mip set (a larger set after it is filled, or a smaller one
under memory pressure), the new texture takes the old texture's table index, so the material's
record keeps naming it and is not rewritten. A slot moves to another index only when the old
index was shared with another material or the new view is already in the table (`moved_swaps`).

## Residency feedback

Set before launch (read once per process, because it changes the shaders and the table layout):

| `REGIA_RESIDENCY_FEEDBACK` | Effect |
|---|---|
| unset, `off` | No feedback: no buffer, no binding, no shader change (the default). |
| `observe` | The GPU's need is read back and reported; streaming ignores it. |
| `drive` | Also fed to streaming as one more draw's need (the larger of GPU and CPU wins). |

Any other value is logged once and treated as `off`. A device gets feedback only on the texture
table path with at least 13 storage buffers per shader stage; elsewhere `feedback` is `null`.

How it works: in each 4 × 4 pixel cell one pixel a frame (the cell position rotates over 16
frames) stores the texel need of its material's four streamed maps (base color, normal, ORM,
emissive) into one word per table index, keeping the largest. The need is texels along the
texture's edge for one texel per pixel, from the pixel's own UV gradients (after culling,
occlusion, parallax and the upscaler's mip bias), so it does not depend on what is resident.
Each frame the renderer copies the previous frame's words to one of three mappable buffers and
reads the newest one that has mapped, without waiting. A word whose table index was given to
another texture after the frame that wrote it is dropped (`stale_dropped`).

## Read

`render.texture_residency` (optional `limit`, 1-64 materials, default 16):

```json
{
  "table": {
    "capacity": 4032, "partially_bound": true, "bound": 512, "high_water": 300,
    "live": 296, "overflows": 0, "rebuilds": 3, "bound_changes": 1, "views_written": 1280,
    "in_place_swaps": 9, "moved_swaps": 0
  },
  "feedback": {
    "mode": "observe", "copies": 40, "skipped_copies": 0, "readouts": 38, "superseded": 0,
    "map_failures": 0, "stale_dropped": 0, "last_latency": 2, "textures_reported": 2,
    "materials_reported": 1, "materials_driven": 0,
    "materials": [
      { "material_key": "0x0000000000000001", "gpu_need_px": 21.1, "cpu_need_px": 18.6,
        "resident_px": 32, "mismatch_levels": 0.18 }
    ]
  }
}
```

- `table` is `null` off the texture table path; `feedback` is `null` while feedback is off.
- `bound` is never below `high_water` and never above `capacity`.
- `skipped_copies` counts frames whose copy found the ring full (the GPU or the map is behind);
  `superseded` counts copies dropped because a newer one was ready.
- `last_latency` is how many frames old the last readout was (usually 2 or 3).
- `materials` lists the last readout's streamed materials, the largest `mismatch_levels`
  (`|log2(max(gpu, 1) / max(cpu, 1))|`, mip levels the two estimates disagree by) first. A CPU
  need of 0 is treated as 1 texel. `material_key` is a hex
  string so every bit survives JSON. `cpu_need_px` is 0 when no draw measured the material this
  frame (for example, when only the GPU saw it).

Errors: without a `RuntimeRenderer` the call fails with the protocol's not-found error and a hint
to add `regia_render::RenderPlugin`; a `limit` outside 1-64 fails with invalid params.

## Typical use

1. Run the scene at `REGIA_BINDLESS_LEVEL=3 REGIA_RESIDENCY_FEEDBACK=observe`.
2. `render.texture_residency {"limit": 8}`: materials with `mismatch_levels` of 1 or more are where
   the CPU estimate is a mip level or more off (tiled detail UVs, parallax, upscaled output).
3. If the GPU need is larger, `drive` makes streaming use it; compare the shots.
4. On a partially bound device, `table.bound` shows how much of the table a rebuild walks; on a
   large scene it should track `high_water`, not `capacity`.

Rust hosts read the same through `FrameRenderer::texture_table_stats` and
`FrameRenderer::residency_feedback_stats`.
