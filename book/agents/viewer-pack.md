# Product viewer packages

A product package is everything the web player or the Android viewer loads for one piece, as a
folder of static files: no server, no stream (ADR-2540). The format is `regia.viewer_package/1`
(`docs/formats/viewer-package.md`).

```text
<package>/
  manifest.json     ids, every file with its size and BLAKE3, package_hash
  mesh.rvm          every mesh, quantized (parts, part-swap alternatives, gem cuts per size)
  recipe.json       the piece recipe (view_range, drives, gem slots)
  options.json      option groups and values, layout, gem slots, the option record without cost
  materials.json    the material entries the options can pick
  textures/...      optional: small PNG or JPEG (KTX2 passed through)
```

## Cook one: `regia viewer pack`

```text
regia viewer pack <kit> <recipe> -o <dir> [--kit DIR]... [--option-graph FILE] [--materials FILE]
                  [--max-texture-px 512] [--json] [--strict-budget]
```

- `<kit>`: a part kit folder (`kit.json` of `jewelry.part_kit/1` plus its GLB). `--kit` adds more
  kits (a design kit beside the shared one). A kit of another house than the recipe's is refused.
- `<recipe>`: a `jewelry.piece_recipe/1` file. Its `view_range` must pass the drag view's checks.
- `--option-graph`: the product's option record; copied into `options.json` with every `cost`,
  `markup` and dealer-price field removed (interfaces.md § 12).
- `--materials`: a `jewelry.material_library/1`; the entries whose `catalogue_facets` match an
  option value (`Material=14K Rose Gold`) are kept.

What the cook writes:

- **every part the piece can show**: the recipe's parts and every `part_swap` alternative, so an
  option switch never loads anything;
- **every gem cut at every size** the value drives offer (`Gem Size` setting `mq_l` and `mq_w`
  together cooks the linked pairs only). A cut comes from the kit's `gem_cuts[].facets`; when the
  kit lacks it, a stand-in is generated from published proportions and the cook warns;
- **cheap hidden parts**: posts, threadless pins and end bases take the kit's last LOD;
- **textures** scaled to `--max-texture-px` and re-encoded: color maps as JPEG quality 85,
  normals and masks as PNG; KTX2 passes through;
- **the size report**, per file with gzip level-9 sizes (what a static host with compression
  sends), against the budget of 3 MB (`--strict-budget` exits 1 over it).

Output is byte-identical for identical inputs, so a host can cache by `package_hash`.

```text
$ regia viewer pack kits/shared/standin-parts recipes/house-b/standin-round-prong-end.json -o pkg
package house-b/standin-round-prong -> pkg
  manifest.json                manifest        2.1 KiB  gzip      0.8 KiB
  mesh.rvm                     mesh           61.3 KiB  gzip     36.0 KiB
  ...
  total 71.6 KiB (gzip 40.2 KiB), 6 meshes, 7664 triangles; budget 2929.7 KiB: ok
```

(Numbers are indicative; the lane report has the measured ones.)

## Check one: `regia viewer verify` and `viewer.pack_verify`

`regia viewer verify <dir> [--json]` and the agent method `viewer.pack_verify {dir}` (a
project-relative folder) read the manifest, check every listed file's size and BLAKE3 and the
`package_hash`, decode `mesh.rvm` (every count is checked against the bytes left before anything is
allocated; every index against the vertex count) and parse the JSON files.

```json
{ "method": "viewer.pack_verify", "params": { "dir": "packages/standin-round-prong-end" } }
```

```json
{ "ok": true, "package_id": "house-b/standin-round-prong", "package_hash": "blake3:…",
  "format_version": 1,
  "meshes": [{ "name": "gem:s1@2.25x2.25", "material_slot": "gem:s1", "vertices": 1061, "triangles": 802 }],
  "groups": [{ "group": "Material", "visual": true, "values": 4 }],
  "view_range": { "tilt_max_deg": 30, "spin": "none" },
  "report": { "total_bytes": 73320, "budget_bytes": 3000000, "within_budget": true } }
```

A damaged package is a result, not a call error: `{ "ok": false, "reason": "pack_hash_mismatch",
"message": "mesh.rvm: …" }`. A `dir` outside the project answers `F0002` with
`path_outside_project`.

## Stand-ins: `regia viewer standin`

`regia viewer standin -o <dir> [--pack]` writes generic fixtures to cook and test against until
real kits exist: a kit `kits/shared/standin-parts` (post, threadless pin, end base, ball, two prong
heads, two marquise bezel cups, a seam ring, a chain link; round brilliant and marquise cuts from
published proportions), three recipes (a marquise bezel end, a round prong end, a dangle ring),
a stand-in material library (rounded public F0 values and refractive indices, marked `estimate`)
and synthetic option records (placeholder houses `house-a` and `house-b`; no supplier data).
`--pack` also cooks every recipe into `<dir>/packages/<name>/` and writes the static demo page `<dir>/demo/index.html`, which drives the web player's `RegiaViewer` API (put the `regia_web` wasm build in `<dir>/demo/pkg/`, serve `<dir>`, open `demo/index.html?product=../packages/<name>`).

## The player side

<!-- ignore-reason: a fragment of a host; the bytes and input come from the platform, and the book's snippet crate does not build the viewer crates -->
```rust,ignore
// web: fetch manifest.json, then each listed file; Android: read the folder
let pkg = regia_viewer_pack::Package::from_files(&manifest_bytes, |path| files.get(path).map(Vec::as_slice))?;
let gem = pkg.mesh("gem:s1@3x1.5");                    // quantized on disk, f32 here
let view = regia_viewer_view::DragView::from_recipe(&pkg.recipe)?;
```

`options.json` lists, per option group, its `drives` (a material zone, a value with its map, or a
part swap with the mesh of each label) and its `values` (`label`, plus `material_id` for material
groups); `parts` is the default layout (instance, mesh, `at`), `gem_slots` the cuts per size.
Which combinations are sold is the configurator's job (`regia_configurator`, the shared
`resolve()`); the package carries the option record it needs.

## Errors

| Reason | When |
|---|---|
| `pack_io` | a file could not be read or written |
| `pack_bad_manifest` | the manifest is not a `regia.viewer_package/1` document, or lists a file twice or misses a required one |
| `pack_version` | a `format_version` this build does not read (re-run `regia viewer pack`) |
| `pack_unsafe_path` | a file path leaves the package or uses characters outside `a-z0-9._-/` |
| `pack_missing_file` | a listed file is not there |
| `pack_too_large` | a file (64 MiB), the manifest (1 MiB) or the package (256 MiB, 512 files) is over its cap |
| `pack_hash_mismatch` | a file's size or BLAKE3, or the `package_hash`, does not match |
| `pack_bad_mesh` | `mesh.rvm` is damaged or a mesh is malformed |
| `pack_bad_json` | `recipe.json`, `options.json`, `materials.json` or an input is not JSON |
| `pack_bad_kit` | the part kit is malformed (kit.json, a GLB, a node, a gem cut that does not close) |
| `pack_bad_recipe` | the recipe is malformed, names a part or cut no kit has, belongs to another house, or has a view_range the drag view refuses |
