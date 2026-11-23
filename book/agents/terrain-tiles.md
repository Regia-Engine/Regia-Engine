# Terrain tiles (`assets.terrain_tiles`)

`regia cook` splits a terrain's texture slots into tiles (ADR-5294, `docs/formats/terrain.md` § 7).
Desktop tiles are BCn. Android tiles are ASTC 4×4. The heightfield stays 16-bit samples on both.
`assets.terrain_tiles` reports how many tiles a cooked terrain has, and which encoding the texture
slots used.

The Android cooker uses ASTC 4×4 for every tiled slot. Slots must be powers of two, at least 256
a side (`regia cook` tiles at 256 on both platforms); a smaller slot fails the cook with
`BadTileSize`. `terrain_astc_alignment` (a tile edge that is not a multiple of four, refused
before encoding) is an API-level guard for callers of `cook_tiled_texture` that pass their own
tile size; `regia cook` never raises it. An encoder or container failure is
`terrain_astc_encode`. These are cook failures, separate from the inspection call's errors
below. The desktop cook keeps its BCn output.

## Call

```json
{ "asset": "assets/textures/terrain.layers.ron" }
```

`asset` is required. It is the terrain layer path, or the cooked index id from `assets.list`.
The call is read-only.

## Result

```json
{
  "asset": "assets/textures/terrain.layers.ron",
  "id": "<cooked index id>",
  "version": 3,
  "encoding": "astc",
  "slots": [
    {
      "name": "macro_pack0",
      "tile_count": 1,
      "width": 256,
      "height": 256,
      "tile_size": 256,
      "format": "bc7_linear",
      "encoding": "astc"
    }
  ],
  "heightfield": { "tiles_x": 2, "tiles_y": 2, "tile_count": 4, "encoding": null },
  "final_heights": null,
  "holes": null,
  "material": null,
  "source_grid": null
}
```

- `version` is the terrain index version: 3 since ADR-8871 (a v1 or v2 index from an older cook loads as 3).
  The encoding is the KTX2 `vkFormat` of each slot's tail, not a field of the index.
- `source_grid` is null unless the index carries a count mapping (a schema-2 `heightfield.json`, a raster
  descriptor or a source tile set). Then `spacing_m` is `[x, z]` in meters and `missing_samples` the samples
  without data. A version-3 index can hold a Z spacing other than X and a valid-data mask (ADR-8871); the
  cook still refuses both (`terrain_mapping_unrepresentable`, `terrain_raster_missing_data`), and a level
  whose cooked index holds either fails to load with `level_terrain_mapping`, so today a loaded map always
  reports equal spacing and 0.
- `final_heights`, `holes` and `material` describe what a host's package step cooked into the map
  ([Cooked photoreal terrain](terrain-cooked-photoreal.md)); all three are null for a standard cook.
- `encoding` is `bcn` or `astc` when every texture slot agrees, `mixed` when they do not, and null
  when the map has no texture slots.
- `format` is the slot's desktop block choice: `bc7_srgb`, `bc5`, `bc4` or `bc7_linear`. On Android
  that choice still names the source (albedo, normal, height, mask). The file itself is ASTC 4×4,
  sRGB for albedo and linear for the rest.
- `heightfield` is null when the source had none. Its `encoding` is always null: the samples are
  not block-compressed.

## Errors

| Code | When |
|---|---|
| `terrain_tiles_bad_param` | `asset` is missing or empty. |
| `F0016` | The app has no asset server, or the path or id is not in the manifest. |
| `terrain_tiles_missing` | The cooked index or a slot tail is not in the cache. Cook the project for this platform. |
| `terrain_tiles_not_terrain` | The asset is cooked as something other than a terrain index. |
| `terrain_tiles_bad_index` | The index does not decode, or its version or slot sizes are not ones this build reads. |
| `terrain_tiles_unreadable` | A slot tail is not a KTX2 texture, so the encoding cannot be read. |
