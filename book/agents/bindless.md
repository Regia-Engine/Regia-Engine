# Bindless state and fallback reasons

The renderer binds a standard material's constants and textures one of three ways, and which one a
device takes depends on its limits, its platform and the `REGIA_BINDLESS_LEVEL` it was created
with. `render.bindless_report` says which path is active and, for every faster path that is off,
why. It is read-only and joins what `render.get_capabilities` (the device) and
`render.get_frame_stats` (the counters) report separately, so one call answers "is bindless on
here, and if not, what would turn it on".

The paths, slowest first:

| `path` | `level.effective` | What a standard material binds |
|---|---|---|
| `per_material` | 0 | Its own uniform buffer and its own textures, in its own bind group. |
| `material_arena` | 1 | One record of the shared material constants storage buffer, in its own bind group. |
| `texture_table` | 3 | The shared constants group at the material's dynamic offset; textures come from the shared table. |

Levels 2 and 4 are reserved (ADR-356), so `effective` is never 2 or 4.

## Read

`render.bindless_report` (no parameters):

```json
{
  "path": "material_arena",
  "level": { "requested": 1, "effective": 1 },
  "tier": "texture_arrays",
  "adapter_tier": "texture_arrays",
  "material_arena": true,
  "texture_table": { "path": "per_material", "capacity": null, "fallback": "not_requested" },
  "shared_samplers": { "set": ["repeat", "clamp", "point_clamp"], "honored": false },
  "reasons": [
    { "feature": "texture_table", "code": "not_requested",
      "message": "the adapter could carry the texture table, but the bindless level is below 3",
      "hint": "set REGIA_BINDLESS_LEVEL=3 before the device is created" }
  ],
  "live": { "opaque_material_batches": 2, "opaque_draws": 4, "material_arena_live": 2,
            "texture_table_live": null, "texture_table_overflows": null }
}
```

- `path` and `level.effective` are what this device's renderer does. They come from the device, not
  from the environment.
- `level.requested` is the environment parsed now by `requested_level` (1 when unset or not a number, capped at 4). It is not the level the device was created at. The next device takes the higher of that environment value and a level a profile asked for (`set_device_level_request`); a profile whose `visibility_path.single_pass_resolve` is on asks for level 3 before the window's device is created. A different `requested` and `effective` with no `reasons` entry means the environment changed after startup, or a profile asked for a higher level than the environment.
- `tier` is the device's capability tier (`none`, `material_ssbo`, `texture_arrays`) and
  `adapter_tier` is what the adapter could reach if the renderer asked for its binding-array
  features. A device at `material_ssbo` with an adapter at `texture_arrays` is off the table only
  because the level was not raised.
- `texture_table` repeats `render.get_capabilities` `bindless.texture_table`.
- `shared_samplers` is `{ set, honored }`. `set` names the table's samplers in shader order: `repeat`
  (linear, repeating, material anisotropy: the sampler of every earlier release), `clamp` (linear,
  clamped to the edge, material anisotropy) and `point_clamp`. A material picks one per glTF slot
  (`LabMaterial::samplers`, base colour, normal, ORM, emissive; default `repeat`). `honored` is true on the
  table path. Every other path binds one sampler per material and reads `repeat`, so a material that
  chose another draws differently there.
- `reasons` has one entry per feature that is off, in this order: the material arena, then the
  texture table. Each has `feature` (`material_arena` or `texture_table`), a stable `code`, a
  `message` (an English fragment; do not match on it) and a `hint` (the change that gets the
  feature, or `null` when only a different device or platform helps). On the table path the array
  is empty only when `live.texture_table_overflows` is 0 or null. A live table with overflows
  still pushes `texture_table_full`.
- `live` is `null` without a `RuntimeRenderer`. Otherwise it carries the last main-view frame's
  `opaque_draws` and `opaque_material_batches` (1 on the table path: that is the point of it),
  `material_arena_live` (materials holding an arena slot; `null` off the arena),
  `texture_table_live` and `texture_table_overflows` (`null` off the table).

## Reason codes

Codes are lowercase ASCII, digits and `_`, and stable.

| Feature | Code | Meaning | Fix |
|---|---|---|---|
| `material_arena` | `mobile_unmeasured` | Android keeps per-material uniform buffers until the storage path is measured on phones. | None yet. |
| `material_arena` | `storage_limits` | Fewer than 12 storage buffers per shader stage, or a storage binding under 1 MiB. | A device with higher limits. |
| `texture_table` | `not_requested` | The adapter could carry the table, but the level is below 3. | `REGIA_BINDLESS_LEVEL=3` before the device is created. |
| `texture_table` | `uniform_materials` | Material constants use per-material uniform buffers (tier `none`, or Android). | See the arena reasons. |
| `texture_table` | `backend` | GL or WebGPU: no binding arrays through wgpu. | Run on Vulkan, DX12 or Metal. |
| `texture_table` | `no_binding_arrays` | The device has no texture binding arrays. | A device with them. |
| `texture_table` | `no_non_uniform_indexing` | The device cannot index them with a per-pixel value. | A device with it. |
| `texture_table` | `array_elements` | Fewer than 4096 binding-array elements per shader stage. | A device with higher limits. |
| `texture_table` | `bind_groups` | Too few bind groups for the table's group 4. | A device with higher limits. |
| `texture_table` | `texture_table_full` | Table is on, but some texture slots sample their neutral view because it is full. Also a `render.problems` entry. | Share or atlas textures, or raise `REGIA_TEXTURE_TABLE_VIEWS`. |

`uniform_materials` follows from an arena reason whenever both are listed: fix the arena reason
first.

## Errors

Without a `RenderDevice` (a headless app without `regia_render::RenderPlugin`) the call fails
with the protocol's not-found error, whose hint says to add the plugin. There are no other error
codes. Without a `RuntimeRenderer` the call succeeds and `live` is `null`.

## Checking a machine

```sh
REGIA_BINDLESS_LEVEL=3 cargo run --release -p lookdev
regia agent call render.bindless_report '{}' --project .
```

Expect `path: "texture_table"` and `live.opaque_material_batches: 1` on a desktop Vulkan or DX12
device. `reasons` is empty there only when the table is not overflowing; `texture_table_full` is
present when `live.texture_table_overflows` is above 0. On a device that cannot take the table,
the `reasons` entry names the first requirement it misses. `render.get_frame_stats` has the per-pass counters and `render.get_capabilities` the raw
limits; see [Renderer excellence](renderer-excellence.md).
