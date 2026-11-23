# Resource inspector: list and dump GPU targets

When a frame looks wrong, the useful question is what an intermediate target holds: the HDR
color before tonemapping, the depth, the velocity, the ambient-occlusion result, a shadow atlas,
the terrain virtual-texture atlas, the cloud history. Two read-only calls answer it.

## List

```json
seeing.list_resources { "label": "bloom", "category": "render_targets" }
→ { "debug_readback": true, "count": 12, "total_bytes": 3407872,
    "resources": [ { "label": "bloom down 0", "kind": "texture_2d", "format": "Rgba16Float",
                     "width": 640, "height": 360, "layers": 1, "mips": 1, "samples": 1,
                     "bytes": 1843200, "category": "render_targets",
                     "copyable": true, "dump_format": "exr" } ] }
```

Both filters are optional. `label` is a case-insensitive substring; `category` is one of the
[`seeing.memory_report`](seeing.md) categories. Fields:

| Field | Meaning |
|---|---|
| `kind` | `texture_2d`, `texture_2d_array`, `texture_3d` or `texture_1d` |
| `layers` | Array layers (2D) or depth slices (3D) |
| `bytes` | All mips, layers and samples |
| `category` | The memory category of the label (same rules as `seeing.memory_report`) |
| `copyable` | The GPU can copy it out now and the inspector holds its handle |
| `dump_format` | The file `seeing.dump_resource` writes: `exr`, `png`, or `null` (no writer) |

The list shows each target as it was last created under its label. A target the renderer
released later (a resize replaces `frame hdr`) stays listed until the label is created again.
It lists textures only; buffers (for example the GI screen lookup, which is a storage buffer)
are not in it.

Registered today: `frame hdr`, `frame depth`, `shadow atlas`, `velocity`, `ssao`, `ssao raw`,
the TAAU `taau display` and `taau history` targets, `bloom down N` / `bloom up N`, `exposed`,
`tonemap`, `fxaa`, `terrain vt atlas N` and the outdoor passes' render targets (cloud and fog
history).

## Dump

```json
seeing.dump_resource { "label": "frame hdr", "mip": 0, "layer": 0, "format": "exr" }
→ { "path": ".../.regia/agent/resources/frame_hdr_m0_l0.exr", "label": "frame hdr",
    "format": "exr", "width": 1280, "height": 720, "mip": 0, "layer": 0, "bytes": 7372928 }
```

- `mip` and `layer` default to 0. `layer` is the array layer or the 3D slice.
- `format` is optional and must suit the texture: `exr` for float formats, `png` for 8-bit
  ones. Anything else is `format_unsupported`.
- `out_dir` is confined like `render.out_start`'s `out` (no `..`; inside the project or the
  system temp directory). The default is `<agent dir>/resources`, else the system temp directory.
- The file is written through a temp file and a rename, so a reader never sees half a file.
- The call waits for the GPU (it is a tool call, not a per-frame cost).

EXR files are uncompressed scanline files. `Rgba16Float`, `Rg16Float` and `R16Float` are
written as `HALF` channels from the raw bits, so the file holds exactly what the texture holds.
`R32Float`, `Rg32Float`, `Rgba32Float` and `Depth32Float` are `FLOAT`; `Rg11b10Ufloat` is
widened to three `FLOAT` channels. Channels are `R G B A`, or `Y` for one channel. 8-bit formats
(`Rgba8Unorm[Srgb]`, `Bgra8Unorm[Srgb]`, `Rg8Unorm`, `R8Unorm`) are written as PNG; BGRA is
swapped to RGBA. sRGB-encoded texels stay as stored.

### Error codes

Codes are lowercase and stable (`data.code`).

| Code | JSON-RPC class | When |
|---|---|---|
| `resource_not_found` | not found | No target has that label. List first. |
| `resource_not_copyable` | unsupported | The GPU cannot copy it (`Depth24Plus`, multisampled without a resolve), or it was created while debug readback was off. |
| `format_unsupported` | unsupported | No writer for the format (for example `Rgb10a2Unorm`), or the requested `format` does not suit it. |
| `subresource_out_of_range` | invalid params | `mip` or `layer` is outside the target. |
| `readback_failed` | internal | The copy or the map failed. |

Bad parameters (a missing `label`, a negative `mip`) are `F0002`; no render device yet is `F0011`.

## Debug readback

Targets need `COPY_SRC` to be copied. The agent plugin turns on **debug readback** before the
renderer creates any target (`regia_render::set_debug_readback`), which adds `COPY_SRC` to the
targets that lack it and keeps their handles for dumping. Without the agent plugin the switch is
off: shipping usage flags are unchanged, and the registry keeps only metadata, so `seeing.list_resources`
still works but every target reports `copyable: false`. A host that is not using the agent can
call `set_debug_readback(true)` itself, before the first frame.

## Rust API

`regia_render::list_resources()` and `regia_render::dump_resource(&device, label, mip, layer,
format)` are the same two calls without the protocol; `regia_render::resource_inspector::track`
registers a target of your own.
