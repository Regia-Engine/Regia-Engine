# Cooked photoreal terrain

A game whose package step builds its own terrain ground and photoreal material can cook them straight into the
map. The player then draws them as they were built: no material rebuild, no extraction to disk, no second height
upload, and no copy of the KTX2 chains (ADR-8024, spec 26 § 7, `docs/formats/terrain.md` § 14).

## What gets cooked

| Sub-asset of the map id | Holds |
|---|---|
| `terrain/index` | The terrain index, version 2: what follows, plus the usual heightfield entry |
| `terrain/heightfield` | The R16 tiled heightfield: your heights quantized with the index's scale and offset |
| `terrain/heights` | The exact f32 heights: residuals over the R16 grid when they restore every bit, raw otherwise |
| `terrain/holes` | The holes as `.holes.ron` text (optional) |
| `terrain/material/<kind>/<i>` | One KTX2 texture per material blob (optional): `layer_albedo`, `layer_normal`, `layer_height`, `layer_orm`, `macro_albedo`, `macro_normal`, plus `terrain/material/macro_coarse_albedo` and `..._normal` |

The heights are the ground, exactly: mirror, sink and keep fractional heights before you call the adapter. The
R16 grid is still written for every tool that reads R16.

## Package time: the adapter

<!-- ignore-reason: a host-side outline: `my_material`, the heights, layer names, holes and cache dir are the host's own values -->
```rust,ignore
use regia_asset_pipeline::{
    PhotorealMaterialInput, PhotorealTerrainInput, TextureCookQuality, cook_terrain_material,
    write_photoreal_terrain,
};

let blobs = cook_terrain_material(&my_material, "linux", TextureCookQuality::High)?;
let cook = write_photoreal_terrain(
    &PhotorealTerrainInput {
        map_id,                       // the map's stable id; the adapter owns its terrain/* ids
        width, height,
        heights: &final_heights,      // metres, row 0 = -Z edge
        world_size_m: 1280.0,
        world_origin: None,
        height_mapping: None,         // or Some((scale, offset)); None spans the heights
        layers: layer_names,
        holes: Some(&holes),
        material: Some(PhotorealMaterialInput {
            cooked: blobs.view(),
            params: my_material.params,
            macro_tiles: 4,
        }),
    },
    "assets/maps/ridge.level.ron",    // the map's manifest path: chunk rules follow the map
    &cooked_cache_dir,
)?;
// cook.manifest: one entry per file, paths "assets/maps/ridge.level.ron#terrain/...".
```

Do not also cook a `.terrain.layers.ron` for the same map id: both write `terrain/index`. Pack the files
uncompressed when you want the material served as views of the mapped pak (`PakWriteOptions { compress: false, .. }`
or a chunk rule that stores them).

## Play time

<!-- ignore-reason: needs the host's wgpu `device` and its terrain draw settings; wgpu is not a dependency of the book's snippet crate -->
```rust,ignore
let source = PakTileSource::new(map_id, Some(&content_dir), cache_dir);
// Exact heights (the index's scale and offset win over the arguments) and holes, in one upload.
let (mut draw, index) = TerrainSharedDraw::from_cooked(device, &source, &layers, world, scale, offset, config, root)?;
if let Some(material) = source.load_cooked_material(&index)? {
    draw.set_layer_mip_skip(skip);    // optional memory tier, before the upload
    draw.set_material_cooked(&material.blobs, material.params, material.macro_tiles, &[])?;
}
```

`set_material_cooked` takes `CookedMaterialKtx2<B>` for any blob type: `Vec<u8>`, `PakPayload` or `&[u8]`.
Reading the material allocates per blob, never per byte, when the pak stores the blobs uncompressed.

## Agent: `assets.terrain_tiles`

The cooked-index report (see [Terrain tiles](terrain-tiles.md)) shows what a host cooked:

```json
{
  "version": 2,
  "final_heights": { "width": 4096, "height": 4096, "encoding": "residual",
                     "height_scale": 96.0, "height_offset": -12.0, "bytes": 67108912 },
  "holes": { "bytes": 412 },
  "material": { "macro_tiles": 4, "layer_albedo": 6, "layer_normal": 6, "layer_height": 6, "layer_orm": 6,
                "macro_albedo_tiles": 16, "macro_normal_tiles": 16,
                "macro_coarse_albedo": true, "macro_coarse_normal": true, "bytes": 301989888 }
}
```

All three are null for a standard `.terrain.layers.ron` cook. Pass the index path from the manifest
(`assets/maps/ridge.level.ron#terrain/index`) or its id. Errors are the method's own (`terrain_tiles_*`); an index whose
heights or material entry does not fit (heights of another size than the R16 grid, more than eight layer blobs,
more macro tiles than the grid holds) is `terrain_tiles_bad_index`.

## Versions

- `terrain-index` v2. A v1 index (v0.5.0, the standard cook) still loads, with no heights, holes or material. A v0.5
  build refuses a v2 index with its version error. `regia upgrade-project` rewrites a v1 index `.fckd` in a dev
  cache to v2 (the other cooked files of the map are untouched).
- `terrain-final-heights` v1 (magic `RTFH`).
- Holes keep the `terrain-holes` format and its migrator.
