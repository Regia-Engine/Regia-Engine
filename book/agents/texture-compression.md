# Android texture compression

How Regia Engine cooks textures for phones and what a device does with a format it cannot sample
(ADR-2063, `docs/specs/12-assets.md` § 7.1).

## What the cook writes

| Platform | Family | Formats by usage |
|---|---|---|
| Desktop (`linux`, `windows`, `macos`) | BC | BC7 (colour and linear data; at `quality: low`, opaque sRGB is BC1 and sRGB with alpha is BC3; linear data stays BC7), BC5 (normals), BC4 (masks) |
| Android, `format = "astc"` (default) | ASTC | 4×4 at `quality: high`; 6×6 or 8×8 at `low` (normals stay 4×4); sRGB for colour and UI |
| Android, `format = "etc2"` | ETC2 / EAC | ETC2 RGB8 (RGBA8 with alpha) for colour and linear data, EAC RG11 for normals, EAC R11 for masks |

Every chain runs down to 1×1. Choose the Android family and the ASTC block ladder for the whole
project in `Regia.toml`:

```toml
[cook.textures.android]
format = "astc"              # "astc" (default) or "etc2"
astc_profile = "per_texture" # "per_texture" (default: each image's .meta quality), "high" (4x4) or "low"
```

A change to the table recooks only the Android textures. An unknown key or value, or a table for
another platform, fails `regia cook` before anything is cooked; the error names the key. A host
that cooks terrain arrays itself applies the policy with `with_texture_policy(policy, ||
cook_terrain_material(..))`; the arrays keep the layouts the terrain shader reads.

## Comparing the formats: `regia cook --size-report`

`regia cook --size-report` measures every PNG, JPEG, TGA and BMP image under `assets/` (with its
`.meta` settings, writing nothing) and every image of a `.gltf` or `.glb` file three ways: as the
desktop cook writes it (BC), as ASTC at the project's profile and as ETC2 / EAC. A glTF's image is
listed as `assets/model.glb#texture/0`, the cook's sub-asset path, with the usage of the material
slot it fills. For each texture it prints the format, the bytes of the full mip chain and the PSNR of
the decoded first level, then the totals, with ASTC and ETC2 as a percentage of BC and the bytes the
Android cook writes under the policy. Images it does not measure are listed with the reason (a 16-bit
heightfield, `compress: false`, smaller than one block, does not decode). Terrain arrays a host cooks
itself and images inside other model formats are not listed.

`regia cook --size-report --json` prints the same report as JSON (schema 1):

```json
{
  "schema": 1,
  "android_policy": { "android_format": "etc2", "astc_profile": "per_texture" },
  "textures": [
    { "path": "assets/rock_albedo.png", "usage": "color_srgb", "width": 64, "height": 64,
      "rgba8_bytes": 21844,
      "bc":   { "format": "BC7_SRGB",         "bytes": 5488, "psnr_db": 48.1 },
      "astc": { "format": "ASTC_4x4",         "bytes": 5488, "psnr_db": 50.3 },
      "etc2": { "format": "ETC2_R8G8B8_SRGB", "bytes": 2744, "psnr_db": 41.0 } }
  ],
  "skipped": [ { "path": "assets/hud.png", "reason": "compress: false" } ],
  "totals": { "rgba8_bytes": 21844, "bc_bytes": 5488, "astc_bytes": 5488, "etc2_bytes": 2744,
              "android_bytes": 2744 }
}
```

(The numbers above are an example layout, not a measurement.) An agent can run the command and
compare `totals.android_bytes` with `totals.bc_bytes`, or look for textures whose `psnr_db` falls
below a threshold before switching a project to ETC2 or to the `low` profile.

## At run time: `render.get_capabilities` `texture_paths`

```json
{ "texture_paths": { "bc": "native", "astc": "cpu_decode", "etc2": "cpu_decode" } }
```

`render.get_capabilities` reports, for each family, how a cooked texture of it loads on this device:

- `native`: the device samples the format; the blocks are uploaded as cooked.
- `cpu_decode`: it cannot, and a decoder exists. The cooked chain is decoded to RGBA8 on the CPU
  (BC1, BC3, BC4 unorm, BC5 unorm, BC7, ASTC LDR, unsigned ETC2 / EAC), every level, so the shader
  samples the cook's own mips. Material maps and terrain arrays both take this path. This costs at
  least 4 times the GPU memory (about 9 times for ASTC 6×6, 16 times for ASTC 8×8, 36 times for
  ASTC 12×12; BC and ETC2 4×4 stay in 4 to 8 times) and slower loads. Each such texture is counted
  in `render.problems` under `textures_cooked_for_other_platform` (and
  `assets.texture_memory.wrong_platform`).
- `unbound`: no decoder exists (BC2, signed BC4, signed BC5, BC6H, HDR ASTC, signed EAC); the slot
  stays unbound.

A phone with ASTC reports `astc: native`. An ETC2-only phone reports `astc: cpu_decode`: cook that
project with `format = "etc2"` to upload natively. A desktop GPU reports `bc: native` and decodes an
Android pak on the CPU.

## Checks an agent can run

1. `regia cook --size-report --json` and read `totals`.
2. Set `[cook.textures.android]`, run `regia cook --platform android`, then `regia cook --platform
   android --stats --json`: the texture families in the cooked cache.
3. On the device: `render.get_capabilities` `texture_paths`, then `render.problems` for
   `textures_cooked_for_other_platform` (none on a device that samples what was cooked).

No new error codes: cook errors are text, and the agent methods above keep their own codes.
