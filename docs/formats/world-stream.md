# World stream formats (`.wstream`, `.wcell`)

Cooked outputs of a streamed level (`docs/specs/78-world-streaming.md` § 6, ADR-1804, ADR-1888). Both sit
behind the standard cooked header (`docs/specs/12-assets.md` § 4: `FCKD`, then a postcard `CookedHeader`,
then the payload) and live in the cooked cache and paks like any cooked asset. Payloads are postcard, in
the field order below.

Asset ids derive from the level id, so a recook gives the same ids:

| Output | Asset id | Header type path |
|---|---|---|
| World index | `AssetId::sub_asset(level, "wstream")` | `regia_world::format::WorldIndex` |
| Always-loaded set | `AssetId::sub_asset(level, "always")` | `regia_world::format::AlwaysLoaded` |
| Cell blob | `AssetId::sub_asset(level, "cells/<x>_<z>")` | `regia_world::format::CellBlob` |
| HLOD proxy | `AssetId::sub_asset(level, "hlod/<node>")` | cooked mesh |
| HLOD voxel proxy (ADR-3253) | `AssetId::sub_asset(level, "hlod/<node>/voxels")` | `regia_render::VoxelFarProxy` (`voxel-far`) |
| HLOD voxel table (ADR-3253) | `AssetId::sub_asset(level, "hlod_voxels")` | `regia_world::hlod_voxels::HlodVoxels` |

The manifest rows are `<level path>#wstream`, `#always`, `#cells/<x>_<z>` and `#hlod/<node>`.

## World index (`format_version` 2)

| Field | Type | Meaning |
|---|---|---|
| `format_version` | `u32` | 2 (1 before ADR-2483; still read). A newer version is refused with `stream_format_newer`. |
| `level` | guid | The level. |
| `content_hash` | 32 bytes | BLAKE3 over every `blob_hash` in coordinate order. |
| `settings` | `LevelStreaming` | The validated settings (version 2 adds `hlod_atlas_px` and `hlod_atlas_max_px` at the end). |
| `cells` | `[CellRecord]` | One per non-empty cell, sorted by `(x, z)`. |
| `bounds` | 2 cells | Lowest and highest cell, inclusive per axis. |
| `hlod` | option | `{ config, proxies, node_proxies, node_anchors, node_colors }`: the tree is rebuilt from `config` and `proxies` (`HlodTree::build` is deterministic); `node_proxies` holds one mesh id per tree node, `None` for a node with nothing to draw; `node_anchors` holds each proxy's origin-space cell, which its vertices are relative to; `node_colors` holds each proxy's flat linear RGBA base color (the area-weighted mean of the `Primitive` colors it merges), `None` when no source named one (ADR-2022); `node_atlases` (version 2) holds each proxy's baked atlas, `None` for a flat proxy: `{ material, textures: [base, normal, orm], size: [w, h], texels_per_m, asked_texels_per_m, capped, bake_error_m, miss_ratio }` (ADR-2483). |
| `replicated` | `[(guid, cell)]` | Entities with replication, sorted by guid. |
| `nav` | `StreamNavLayout?` | The grid the cells' nav tiles were cooked on (`LevelStreaming::nav`): cook settings, world origin, cells along X and Z. `None` when the cells carry none. |

`CellRecord`: `coord`, `blob`, `blob_hash` (BLAKE3 of the blob file), `entity_count`, `own_bytes`,
`dep_bytes` and `vram_bytes` (each referenced asset's size divided by the cells referencing it; `vram_bytes`
also counts the vertex and index bytes of each distinct built-in primitive shape the cell draws, divided by the
cells drawing it, ADR-4993),
`min_y`, `max_y`, `priority`, `deps` (sorted), `nav_tiles`, `has_terrain_collision`.

## Cell blob (`format_version` 1)

| Field | Type | Meaning |
|---|---|---|
| `format_version` | `u32` | 1. A newer version fails the cell with `cell_format_newer`. |
| `coord` | cell | The cell. |
| `scene` | `CookedScene` | The entities. Hierarchies are sorted by root guid; each is contiguous, root first, then depth first by guid, so one spawn batch is `CookedScene::hierarchy_ranges` long. |
| `anchor` | `[i64; 3]` | The cell's position in the origin space (`LargePosition::cell`). |
| `nav` | option of bytes | Cooked nav tiles, opaque to the runtime. |

Root transforms are stored relative to the center of `anchor`, so a cell 50 km out keeps millimeters in
its file. The runtime converts each root with `FloatingOrigin::to_local` when it spawns the cell.

A blob is read in this order: `blob_hash` against the file; header and type; `format_version`; the
payload. A hash or header failure is `cell_corrupt`, a payload failure after a good hash `cell_decode`.

## HLOD voxel table (`.wvoxels`, `format_version` 1, ADR-3253)

Written beside the world index only when the cook placed terrain scatter under some HLOD node (spec 78 § 11.6); a level
without scatter has none and cooks the same bytes as before. Postcard in the standard cooked header; the header's
dependencies are the voxel proxies it names.

| Field | Type | Meaning |
|---|---|---|
| `format_version` | `u32` | 1. A newer version is refused (`Newer`); the runtime then opens the level without voxel proxies. |
| `level` | guid | The level; must match the cooked header's asset id. |
| `resolution` | `u32` | Voxels along the longest axis of a node's scatter at its finest level, as cooked. |
| `nodes` | `[Option<HlodVoxelNode>]` | One per tree node, in tree order (at most 4 194 304, never more than the tree has); `None` for a node with no scatter under it. |

`HlodVoxelNode`: `proxy` (asset id of the cooked `VoxelFarProxy`), `anchor` (`[i64; 3]` origin-space cell its
coordinates are relative to), `instances` (`u32`), `voxels` (`u32`, occupied voxels at the finest level), `voxel_size_m`
(`f32`, finite and positive, else `Corrupt`), `bytes` (`u64`, the cooked proxy file).

## Determinism

The same sources give byte-identical files: inputs are sorted by guid and nothing records a time. A blob
that did not change is not rewritten, and the index is written last.

## Example

A level of two entities 300 m apart with 128 m cells and no HLOD cooks to two blobs and one index:

```text
cells: [
  (coord: (0, 0), entity_count: 1, own_bytes: 214, dep_bytes: 0, min_y: 0.0, max_y: 0.0, priority: 0, ...),
  (coord: (2, 0), entity_count: 1, own_bytes: 214, dep_bytes: 0, min_y: 0.0, max_y: 0.0, priority: 0, ...),
]
bounds: [(0, 0), (2, 0)]
```

## Versioning

`LevelStreaming` carries its own `version`. A shape change adds fields with serde defaults or bumps the
version with a migrator in `regia upgrade-project`. A layout change of `.wstream` or `.wcell` bumps
`format_version`; the loader keeps reading every older layout, and `upgrade-project` rewrites cooked files
in the dev cache as it does for the cooked mesh (`docs/specs/12-assets.md` § 4a). Both are registered with
`cargo xtask check-formats` as `world-index` and `cell-blob`.

**Version 2 (ADR-2483).** `HlodIndex` gains `node_atlases` and `LevelStreaming` gains `hlod_atlas_px` and
`hlod_atlas_max_px` (both at the end of their structs). A version 1 index reads through a frozen version 1
layout and migrates with every `node_atlases` entry `None` and the atlas off: an old cook loads and its
proxies draw flat. `regia upgrade-project` rewrites cooked version 1 indexes as version 2, header kept. A
baked proxy's mesh carries its atlas UVs in UV0 (`CookedMesh` v3 unchanged), and its atlas is four
ordinary cooked assets, sub-assets of the proxy mesh id: `atlas/base`, `atlas/normal`, `atlas/orm`
(cooked textures) and `atlas/material` (a cooked standard material), with manifest rows
`#hlod/<node>/atlas/<name>`.
