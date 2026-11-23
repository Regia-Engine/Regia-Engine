# Product viewer package (`viewer-package`, `regia.viewer_package/1`)

One product for the web player and the Android viewer (ADR-2540), as a folder of static files.
Written by `regia viewer pack` (`regia_viewer_pack::cook`), read by `regia_viewer_pack::Package`
(`from_files` for fetched bytes, `read_dir` for a folder). Registry name `viewer-package`, version 1.

## Files

| File | Kind | Content |
|---|---|---|
| `manifest.json` | (manifest) | canonical JSON (keys sorted at every level, no whitespace, LF) |
| `mesh.rvm` | `mesh` | every mesh, binary, below |
| `recipe.json` | `recipe` | the piece recipe (`jewelry.piece_recipe/1`), canonical JSON |
| `options.json` | `options` | `regia.viewer_options/1`, below |
| `materials.json` | `materials` | `regia.viewer_materials/1`: `library_version`, `entries[]` (material library entries the options pick, unchanged) |
| `textures/<name>.<png\|jpg\|ktx2>` | `texture` | optional; names are `a-z0-9._-` |

## `manifest.json`

| Field | Meaning |
|---|---|
| `schema` | `"regia.viewer_package/1"` |
| `format_version` | `1` |
| `package_id` | `<house>/<design slug>` |
| `house`, `design_key`, `product_keys`, `recipe_version` | from the recipe |
| `recipe_hash` | `blake3:<hex>` of `recipe.json` |
| `kits[]` | `{kit_id, kit_version, house}` of every kit the cook read |
| `engine_version` | the cook's Regia version |
| `meshes[]` | `{name, material_slot, vertices, triangles}` per mesh in `mesh.rvm` |
| `files[]` | `{path, kind, bytes, blake3}` of every file except the manifest; `mesh.rvm`, `recipe.json`, `options.json`, `materials.json` are required |
| `package_hash` | `blake3:<hex>` over the lines `path TAB bytes TAB blake3 LF` of `files[]` sorted by path |

A reader refuses a package unless: `schema` and `format_version` match; every path is safe
(non-empty `/`-separated components of `a-z0-9._-`, never `.` or `..`, at most 200 bytes) and
listed once; every file is present with the listed size and BLAKE3; `package_hash` matches; caps
hold (manifest 1 MiB, a file 64 MiB, the package 256 MiB, 512 files).

## `mesh.rvm` (mesh format version 1)

Little-endian. Each mesh is one material slot.

```text
"RVM1"  u32 format_version (1)  u32 mesh_count  u32 reserved (0)
per mesh:
  u16 name_len, name (UTF-8)    u16 slot_len, slot (UTF-8)    pad to 4
  u32 flags (1 = has uvs, 2 = u32 indices; other bits refused)
  u32 vertex_count  u32 index_count (a multiple of 3)
  f32x3 bounds_min  f32x3 bounds_max  [f32x2 uv_min  f32x2 uv_max]
  vertex_count x u16x3 positions: bounds_min + (bounds_max - bounds_min) * q / 65535      pad to 4
  vertex_count x i16x2 normals: octahedral (Cigolle et al. 2014), snorm16
  [vertex_count x u16x2 uvs, quantized to the uv bounds]
  index_count x u16 (u32 with flag 2; used when vertex_count > 65536)                       pad to 4
```

Caps: 4096 meshes, 4194304 vertices and 12582912 indices per mesh. A reader checks every count
against the bytes left before allocating, refuses indices out of range, non-finite bounds,
unknown flags and trailing bytes.

Mesh names: `part:<part_ref>` (a kit part), `gem:<slot_id>@<length>x<width>` (a gem cut at one
size, flat-shaded: one normal per facet).

Precision: positions within half a quantization step (1/131070 of the mesh's extent); normals within
0.05 degrees.

## `options.json` (`regia.viewer_options/1`)

| Field | Meaning |
|---|---|
| `groups[]` | `{group, visual, drives[], values[]}` per option group: `drives` are the recipe's `to` objects (`material_zone`, `value` + `map`, `part_swap` + `map` of label to `{part_ref, mesh}`); `values` are `{label, material_id?}`, from the option record when given, else the drive maps. `no_visual` groups have `visual: false` |
| `parts[]` | the default layout: `{instance_id, part_ref, mesh, at}` |
| `gem_slots[]` | `{slot_id, cut, material_slot, count, pattern, at, sizes[{size_mm, mesh, values}]}` |
| `material_zones` | from the recipe |
| `option_graph` | the option record with every `cost`, `cost_cents`, `markup`, `markup_pct`, `dealer_price` removed, or null |
| `option_graph_hash` | `blake3:<hex>` of the option record file as read, or null |
| `cost_fields_removed` | how many fields were removed |

## Versions and migration

A package is cooked data. There is no migrator: when `format_version` or the mesh format version
changes, cook the package again with `regia viewer pack`. A reader refuses any other version with
`pack_version`, and `regia upgrade-project` lists such files (probe: `mesh.rvm` by its magic,
`manifest.json` by its `schema`).

## Size budget

The cook's size report flags a package over 3,000,000 bytes (`BUDGET_BYTES`, before HTTP
compression). Measured 2026-11-04 on the stand-ins (no textures): marquise bezel end 38,551 bytes (12,687 gzip),
round prong end 66,091 (20,383 gzip), dangle ring 100,226 (60,146 gzip). A 512 x 512 noisy detail map is
515,292 bytes as PNG and 50,316 bytes as JPEG quality 85, so a product with a few web-size textures stays
well under the budget.
