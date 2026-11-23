# Model map and node report

## `editor.model_report`

Read-only, CPU only. Params: `path` (project-relative glTF/GLB, required), `class` (`prop` default,
`character`, `vehicle`, `building`, `hero`), `limits` (object overriding the class limits), `root` (entity
id of a hierarchy from `editor.spawn_gltf_hierarchy`), `text` (also return the log rendering).

```json
{ "method": "editor.model_report",
  "params": { "path": "models/tank.glb", "class": "vehicle",
              "limits": { "max_texture_dimension": 2048, "require_normal_maps": true },
              "text": true } }
```

Returns `{ path, report, clean, text? }`. `report` has `nodes[]` (`gltf_index`, `name`, `parent`,
`children`, `depth`, `triangles`, `vertices`, `materials`, `skin`, `joints`, `is_socket`,
`is_collision`, `is_joint`, `in_scene`), `roots`, `skins[]`, `materials[]` (also `glass`), `lods[]` (`level`, `triangles`, `mesh_nodes`; empty without
`_LOD<n>` node names), `maps[]` (`material`, `slot`, `bound`,
`size`, `used`), `totals`, `flags[]` (`kind`, `subject`, `value`, `limit`, `message`),
`duplicate_names[]` and `orphans[]` (`gltf_index`, `name`, `reason`). `clean` is true with no flag,
duplicate or orphan.

Limit names: `max_triangles`, `max_node_triangles`, `max_nodes`, `max_materials`,
`max_materials_per_node`, `max_joints_per_skin`, `max_texture_dimension`, `max_texture_bytes`,
`require_normal_maps`, `lod1_permille`, `lod2_permille`, `lod3_permille`, `lod_ratio_tolerance_percent`,
`max_lod2_triangles`, `max_lod3_triangles`, `max_glass_materials`. An unknown name is an `invalid_params` error. 0 disables a check.

Nodes named `collision_*` are physics proxies (`regia_asset::is_collision_node_name`): they have
`is_collision: true`, stay out of `totals.triangles`, `totals.vertices`, `lods[]`, the material tables and
the per-node triangle budget, and are counted in `totals.collision_nodes`, `collision_triangles` and
`collision_vertices` instead.

How to check a result: "normal map not bound" is a `maps[]` row with `slot: "normal"`, `bound:
false`, `used: true`; a node over budget is a `flags[]` entry with `kind: "node_triangles"` and the
node name as `subject`. With `root`, a missing or extra entity is an `orphans[]` entry; a
`gltf_index` of null means the entity is not in the file.

For the `hero` class, `flags[]` also carries `lod_triangles` (subject `LOD2` or `LOD3` over its hard
ceiling), `lod_ratio` (a level above its expected share of LOD0) and `glass_materials` (subject `model`).
See [Model map and node report](../user-guide/model-report.md) for the numbers.

## `editor.model_extensions`

Read-only, CPU only. Params: `path` (project-relative glTF/GLB, required), `include_json` (also return
the raw JSON of unknown extension blocks, default false).

```json
{ "method": "editor.model_extensions", "params": { "path": "models/car.glb" } }
```

Returns `{ path, extensions }`; `extensions` has `used`, `required`, `materials[]` (`index`,
`clearcoat`, `sheen`, `specular`, `anisotropy`, `emissive_strength`, `textures` by slot with `image`,
`tex_coord` and `transform`), `unknown[]` (`scope`, `index`, `sub_index`, `extension`, `json` only with
`include_json`), `dropped_unknown` and `unknown_names`. A file with `unknown_names` empty uses only
extensions Regia Engine reads. See [glTF extensions](../user-guide/gltf-extensions.md) for which of them
render.
