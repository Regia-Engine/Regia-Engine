# .texpack.ron — Channel-pack recipe v1

Code: `regia_asset_pipeline::texture_pack` (`TexturePackRecipe`, ADR-3090). Example:
[`texpack.example.ron`](texpack.example.ron). Spec: `docs/specs/12-assets.md` § 7.1.

## Overview

A `.texpack.ron` builds one texture from up to four source images. Each output channel (R, G, B, A)
is one channel of a named source, optionally inverted, or a constant. The usual use is a packed ORM
map (occlusion, roughness, metalness) from three grayscale maps, or a roughness map from a gloss map.

The packed image cooks exactly like a PNG: block-compressed with a full mip chain (BC7 on desktop,
ASTC or ETC2 / EAC on Android), under the recipe's own `.meta` `settings` (`usage`, `max_size`,
`compress`, `quality`, `stream`, `alpha_cutoff`, ...). The cooked asset is a
`regia_asset::CookedTexture` with the recipe's id, so a material names the recipe where it would
name an image.

Version **1**. A file of another version is rejected before the body is parsed
(`texpack_version`). `regia upgrade-project` lists the format; there is no older version to
migrate. The shape is hashed by `cargo xtask check-formats`.

## Fields

| Field | Type | Default | Meaning |
|---|---|---|---|
| `format_version` | `u32` | required | `1`. |
| `sources` | map name → path | empty | Up to four sources, paths relative to the recipe (`/` or `\`). PNG, JPEG, TGA, BMP, or another `.texpack.ron` (its packed result). A path may use `..` but must stay inside the project. |
| `r`, `g`, `b` | channel | `Constant(0.0)` | The red, green and blue output. |
| `a` | channel | `Constant(1.0)` | The alpha output. |
| `size_mismatch` | `resize_to_largest` \| `resize_to_smallest` \| `error` | `resize_to_largest` | Sources of different sizes are resized to the largest (or smallest) width and height among them, or fail the recipe. |
| `size` | `Option<(u32, u32)>` | `None` | The output size; every source is resized to it. Required when every channel is a constant. Each edge 1 to 16384. |
| `usage` | `Option<color \| linear \| normal \| mask \| ui>` | `linear` | How the packed texture is sampled; the recipe's `.meta` `usage` overrides it. |

A channel is one of:

- `Source(source: "<name>", channel: "<r|g|b|a|luma>", invert: <bool>)`: the named source's channel
  (`luma` is Rec. 709 luma, 0.2126 R + 0.7152 G + 0.0722 B, over the stored values); `invert: true`
  stores `1 - value`. `invert` defaults to `false`.
- `Constant(<0..=1>)`: the same value in every texel (stored as `round(value * 255)`).

Resizing is separable: an area average of the covered source texels when shrinking, a linear
interpolation between the two nearest texel centres when growing.

## Cook and dependencies

Every source file (and through a nested recipe, its sources) is an input of the cook key, local and
shared (ADR-530, ADR-2612): editing a source recooks the recipe, and an unchanged recipe skips
without packing. The live cooker recooks a recipe when one of its sources is saved. A source image
also cooks on its own, as any image under `assets/` does.

## Error codes

| Code | When |
|---|---|
| `texpack_parse` | Not UTF-8, RON syntax, an unknown field or a wrong type. |
| `texpack_version` | `format_version` is not 1. |
| `texpack_bad_sources` | More than four sources, or an empty source name or path. |
| `texpack_unknown_source` | A channel names a source the recipe does not list. |
| `texpack_bad_channel` | A channel reads a channel name other than `r`, `g`, `b`, `a`, `luma`. |
| `texpack_bad_constant` | A constant is not a finite number in 0..=1. |
| `texpack_bad_path` | A source path is absolute or leaves the project. |
| `texpack_missing_source` | A source file does not exist or cannot be read. |
| `texpack_decode` | A source is not an image the cook decodes. |
| `texpack_size_mismatch` | Sources differ in size under `size_mismatch: error`. |
| `texpack_bad_size` | `size` has a zero edge or one above 16384, the output would exceed 16384, or nothing gives a size. |
| `texpack_cycle` | A recipe reaches itself through its sources, or recipes nest deeper than eight. |

A failing recipe is a cook error naming the file and the code; its previous cooked output stays.
The agent's `assets.texpack_dry_run` reports the same codes without cooking.

## Example

```ron
(
    format_version: 1,
    sources: {
        "ao": "rock_ao.png",
        "rough": "rock_rough.png",
        "metal": "rock_metal.png",
    },
    r: Source(source: "ao", channel: "r"),
    g: Source(source: "rough", channel: "r"),
    b: Source(source: "metal", channel: "r"),
    a: Constant(1.0),
    size_mismatch: resize_to_largest,
    size: None,
    usage: Some(linear),
)
```

A roughness map from a gloss map: `g: Source(source: "gloss", channel: "r", invert: true)`.

## Migration notes

v1 is the first version. A future version adds its migrator to the loader and to
`regia upgrade-project` (`specs/12` § 4a).
