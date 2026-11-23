# Alpha coverage and channel packing

Two texture cook features for foliage and packed material maps (ADR-3090), with two read-only agent
methods to check them.

## Alpha-tested mips keep their coverage

An alpha-tested texture (leaf cards, fences, hair cards) draws a texel where its alpha reaches the
material's cutoff. Each mip box-filters the level above, so opaque leaves average with the gaps
between them and fall under the cutoff: every smaller mip passes fewer texels, and a far canopy
thins out until it disappears.

Set the cutoff in the texture's `.meta` `settings`:

```ron
(alpha_cutoff: Some(0.5))
```

Use the material's `alpha_cutoff` (0.5 for a glTF `MASK` material without one). The cook then scales
each mip's alpha so the fraction of texels at or above the cutoff matches mip 0's (Castano,
"Computing Alpha Mipmaps", 2010): within 2 % on every level of 8×8 texels or more, and as close as
one texel allows on the smallest ones. The scaling runs before compression, so BC7, BC3, ASTC and
ETC2 / EAC all get the corrected chain. The setting reads alpha, or red for a texture cooked as
`usage: Some(mask)`; a normal map ignores it.

It works on standalone images (PNG, JPEG, TGA, BMP), on images a glTF references (the image's own
`.meta`), and on `.texpack.ron` outputs. Without the setting nothing changes: every existing texture
cooks to the same bytes. The setting is part of the cook key, so changing it recooks the texture
(and the glTF that uses it), and changing nothing does not.

HLOD proxy atlases with holes opt in on their own: their base colour cooks with the proxy's cutout
cutoff (0.5).

An uncompressed texture (`compress: Some(false)`) keeps a single level; the renderer builds its mips
at load, without this correction.

## Channel-pack recipes

A `.texpack.ron` builds one texture from up to four source images, each output channel a source's
channel (optionally inverted) or a constant. See [the format page](../../docs/formats/texpack-ron.md)
for every field and error code. The packed texture cooks like a PNG with the recipe's `.meta`
settings, and every source is a cook input: editing one recooks the recipe, live.

## `assets.texture_coverage`

```json
{ "method": "assets.texture_coverage", "params": { "asset": "assets/trees/oak_leaves.png" } }
```

Decodes every mip of a cooked texture with the runtime's CPU decoders and reports the fraction of
texels at or above the cutoff. Params: `asset` (path or id, required), `cutoff` (in (0, 1]; default
the `.meta`'s `alpha_cutoff`, else 0.5), `channel` (`a` or `r`; default alpha, red for a mask).
The `.meta` read is capped: a sidecar larger than 1 MiB counts as setting no cutoff (the call is
read-scoped, so a huge planted sidecar is never read whole), and `cutoff_source` then reports
`param` or `default`.

For shared external glTF images, pass the image path (for example `assets/trees/oak_leaves.png`),
its `#gltf_texture/<settings hash>` variant path, or the current texture id from a material slot or
the model's `.meta` sub-asset table. The old `<gltf>#texture/<n>` manifest path remains valid
for private images (embedded images or external sources outside the project). Recook before resolving migrated ids; see [cooked packs](cooked-packs.md).

The answer: `levels: [{ level, width, height, coverage, mean }]`, `mip0_coverage`, `max_drop` (the
largest difference from mip 0 among levels of 64 texels or more), `preserved` (`max_drop` within
2 %), `cutoff`, `cutoff_source` (`param`, `meta` or `default`), `usage`, `format`, `channel`.

A texture with `preserved: false` and a falling coverage is a canopy that will thin out at distance:
add `alpha_cutoff` to its `.meta` and recook.

Error codes:

| Code | When |
|---|---|
| `F0016` | No asset server, no manifest entry for the asset, or no cooked file. |
| `texture_coverage_bad_param` | `cutoff` is not a number in (0, 1], or `channel` is not `a` or `r`. |
| `texture_coverage_not_texture` | The asset is not a cooked texture or image, or it is a normal map. |
| `texture_coverage_undecodable` | The format has no CPU decoder (BC6H, HDR ASTC) or a level does not decode. |

## `assets.texpack_dry_run`

```json
{ "method": "assets.texpack_dry_run", "params": { "path": "assets/rocks/rock_orm.texpack.ron" } }
```

Checks a recipe without cooking: the parse, each channel (`ao.r`, `1 - gloss.r`, `0.25`), every
source with its size (image headers only, nested recipes followed) and whether it is resized, the
output size, the usage and every input file. A recipe that would fail the cook answers
`{ ok: false, code, message }` with its `texpack_*` code (the list is on the format page:
`texpack_parse`, `texpack_version`, `texpack_bad_sources`, `texpack_unknown_source`,
`texpack_bad_channel`, `texpack_bad_constant`, `texpack_bad_path`, `texpack_missing_source`,
`texpack_decode`, `texpack_size_mismatch`, `texpack_bad_size`, `texpack_cycle`).

Error codes of the call itself:

| Code | When |
|---|---|
| `texpack_dry_run_bad_param` | `path` is missing or does not end in `.texpack.ron`. |
| `F0002` (`path_outside_project`) | `path` leaves the project. |
